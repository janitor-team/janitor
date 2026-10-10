//! Runs janitor-create-sbuild-chroot with a stand-in `mmdebstrap` first on PATH.

use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);
const TARBALL: &str = "unstable-amd64-sbuild.tar.xz";

/// Kills the stand-in when a test fails before it is gone.
struct KillOnDrop(String);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if Path::new("/proc").join(&self.0).exists() {
            let _ = Command::new("kill").args(["-KILL", &self.0]).status();
        }
    }
}

/// Write a shell script called `mmdebstrap`; its third argument is the tarball.
fn write_stand_in(directory: &Path, body: &str) {
    let path = directory.join("mmdebstrap");
    std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn create_sbuild_chroot(stand_in: &Path, cache: &Path) -> Command {
    let mut path = vec![stand_in.to_path_buf()];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_janitor-create-sbuild-chroot"));
    command
        .args(["--suite=unstable", "--mirror=http://deb.debian.org/debian"])
        .args(["--chroot=unstable-amd64-sbuild", "--arch=amd64"])
        .arg("--base-directory")
        .arg(cache)
        .env("PATH", std::env::join_paths(path).unwrap());
    command
}

fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_exit(child: &mut Child) -> ExitStatus {
    wait_until("the tool to exit", || child.try_wait().unwrap())
}

fn entries(directory: &Path) -> Vec<PathBuf> {
    let mut entries = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

/// Start the tool with a stand-in that records its process ID in `pid_file`.
fn start(td: &Path, cache: &Path, body: &str) -> (Child, KillOnDrop) {
    let pid_file = td.join("pid");
    write_stand_in(td, body);
    let tool = create_sbuild_chroot(td, cache)
        .env("PID_FILE", &pid_file)
        .env("TERM_FILE", td.join("term"))
        .spawn()
        .unwrap();
    let pid = wait_until("the stand-in mmdebstrap to start", || {
        std::fs::read_to_string(&pid_file).ok()
    });
    (tool, KillOnDrop(pid.trim().to_string()))
}

fn signal(name: &str, tool: &Child) {
    let killed = Command::new("kill")
        .args([name, &tool.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
}

const RECORD_PID: &str =
    ": > \"$3\"\necho $$ > \"$PID_FILE.new\"\nmv \"$PID_FILE.new\" \"$PID_FILE\"";

fn check_signal_removes_temporary_directory(name: &str) {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("sbuild");
    // Take a moment to stop, so that a tool that does not wait is caught
    let body = format!(
        "trap 'sleep 0.3; exit 1' TERM\n{}\nwhile :; do sleep 0.02; done",
        RECORD_PID
    );
    let (mut tool, stand_in) = start(td.path(), &cache, &body);
    let running = Path::new("/proc").join(&stand_in.0);
    assert!(running.exists());

    let scratch = entries(&cache);
    assert_eq!(scratch.len(), 1, "{:?}", scratch);
    assert!(scratch[0].join(TARBALL).exists());

    signal(name, &tool);
    let status = wait_for_exit(&mut tool);

    assert_eq!(entries(&cache), Vec::<PathBuf>::new());
    assert!(!running.exists(), "mmdebstrap is still running");
    assert_eq!(status.code(), Some(1));
}

/// Run the tool with --dry-run and return what it printed.
fn dry_run(extra: &[&str], base_directory: &Path) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_janitor-create-sbuild-chroot"));
    command
        .args(["--suite=unstable", "--mirror=http://deb.debian.org/debian"])
        .args(["--chroot=unstable-amd64-sbuild", "--dry-run"])
        .arg("--base-directory")
        .arg(base_directory)
        .args(extra);
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn test_dry_run_unshare_prints_the_command_and_changes_nothing() {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("sbuild");
    std::fs::create_dir_all(&cache).unwrap();
    let stdout = dry_run(
        &[
            "--arch=arm64",
            "--include=ccache",
            "--build-distribution=lintian-fixes",
        ],
        &cache,
    );
    assert_eq!(
        stdout,
        format!(
            "mmdebstrap '--variant=buildd' unstable {0}/unstable-amd64-sbuild.tar.xz \
http://deb.debian.org/debian '--mode=unshare' '--arch=arm64' '--include=ccache'\n\
ln -sf unstable-amd64-sbuild.tar.xz {0}/lintian-fixes-arm64-sbuild.tar.xz\n",
            cache.display()
        )
    );
    assert_eq!(entries(&cache), Vec::<PathBuf>::new());
}

#[test]
fn test_dry_run_unshare_keeps_an_existing_tarball_unless_forced() {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("sbuild");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(TARBALL), b"old").unwrap();
    assert_eq!(
        dry_run(&["--arch=amd64"], &cache),
        format!("# keeping existing {}/{}\n", cache.display(), TARBALL)
    );
    assert!(dry_run(&["--arch=amd64", "--force"], &cache).starts_with("mmdebstrap "));
}

#[test]
fn test_dry_run_unshare_adds_the_home_directory_hook() {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("sbuild");
    std::fs::create_dir_all(&cache).unwrap();
    let stdout = dry_run(&["--arch=amd64", "--user=root"], &cache);
    assert!(
        stdout.contains("--customize-hook=install -d --owner=0 --group=0 \"$1\"/root"),
        "{}",
        stdout
    );
}

#[test]
fn test_dry_run_schroot_resolves_the_base_directory() {
    let td = tempfile::tempdir().unwrap();
    let real = td.path().join("chroots");
    std::fs::create_dir_all(&real).unwrap();
    let link = td.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let stdout = dry_run(
        &[
            "--mode=schroot",
            "--arch=amd64",
            "--component=main",
            "--include=ccache",
        ],
        &link,
    );
    assert_eq!(
        stdout,
        format!(
            "sbuild-createchroot unstable {}/unstable-amd64-sbuild \
http://deb.debian.org/debian '--arch=amd64' '--components=main' \
'--command-prefix=eatmydata' '--include=ccache,eatmydata' '--chroot-mode=schroot'\n",
            real.canonicalize().unwrap().display()
        )
    );
}

#[test]
#[serial]
fn test_force_replaces_an_existing_tarball() {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("sbuild");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join(TARBALL), b"old").unwrap();
    write_stand_in(td.path(), "echo complete > \"$3\"");

    let mut keep = create_sbuild_chroot(td.path(), &cache);
    let mut tool = keep.spawn().unwrap();
    assert!(wait_for_exit(&mut tool).success());
    assert_eq!(std::fs::read(cache.join(TARBALL)).unwrap(), b"old".to_vec());

    let mut forced = create_sbuild_chroot(td.path(), &cache);
    forced.arg("--force");
    let mut tool = forced.spawn().unwrap();
    assert!(wait_for_exit(&mut tool).success());
    assert_eq!(
        std::fs::read(cache.join(TARBALL)).unwrap(),
        b"complete\n".to_vec()
    );
}

#[test]
#[serial]
fn test_sigterm_removes_temporary_directory() {
    check_signal_removes_temporary_directory("-TERM");
}

#[test]
#[serial]
fn test_sigint_removes_temporary_directory() {
    check_signal_removes_temporary_directory("-INT");
}

#[test]
#[serial]
fn test_second_signal_kills_a_command_that_does_not_stop() {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("sbuild");
    // Note the first SIGTERM and carry on
    let body = format!(
        "trap ': > \"$TERM_FILE\"' TERM\n{}\nwhile :; do sleep 0.02; done",
        RECORD_PID
    );
    let (mut tool, stand_in) = start(td.path(), &cache, &body);
    let running = Path::new("/proc").join(&stand_in.0);

    signal("-TERM", &tool);
    wait_until("the stand-in mmdebstrap to get SIGTERM", || {
        td.path().join("term").exists().then_some(())
    });
    assert!(running.exists());
    assert_eq!(tool.try_wait().unwrap(), None);

    signal("-TERM", &tool);
    let status = wait_for_exit(&mut tool);

    assert_eq!(entries(&cache), Vec::<PathBuf>::new());
    assert!(!running.exists(), "mmdebstrap is still running");
    assert_eq!(status.code(), Some(1));
}

#[test]
#[serial]
fn test_leftover_temporary_directory_does_not_stop_a_run() {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("sbuild");
    let leftover = cache.join(".tmpAAAAAA");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join(TARBALL), b"partial").unwrap();
    write_stand_in(td.path(), "echo complete > \"$3\"");

    let mut tool = create_sbuild_chroot(td.path(), &cache).spawn().unwrap();
    let status = wait_for_exit(&mut tool);

    assert!(status.success(), "{}", status);
    assert_eq!(
        std::fs::read(cache.join(TARBALL)).unwrap(),
        b"complete\n".to_vec()
    );
    assert_eq!(entries(&cache), vec![leftover, cache.join(TARBALL)]);
}
