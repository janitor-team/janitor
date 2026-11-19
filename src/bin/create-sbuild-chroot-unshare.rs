//! Create sbuild chroots in `unshare` mode using mmdebstrap.
//!
//! Port of the former `create-sbuild-chroot-unshare.py` script.

use clap::Parser;
use janitor::config::config::campaign::Build;
use janitor::config::{Config, Distribution};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Create sbuild chroots (unshare mode) for janitor distributions"
)]
struct Args {
    /// Currently unused — kept for CLI compatibility with the
    /// previous Python script.
    #[arg(long)]
    remove_old: bool,

    /// Extra package(s) to include in the chroot. May be repeated.
    #[arg(long)]
    include: Vec<String>,

    /// Base directory for chroots. Defaults to ~/.cache/sbuild.
    #[arg(long)]
    base_directory: Option<PathBuf>,

    /// User to create a home directory for inside the chroot.
    #[arg(long)]
    user: Option<String>,

    /// Path to the janitor configuration file.
    #[arg(long, default_value = "janitor.conf")]
    config: PathBuf,

    /// Distributions to build chroots for. If empty, every
    /// distribution in the config is built.
    distribution: Vec<String>,
}

fn default_base_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".cache/sbuild")
}

/// Resolve a user's home directory by shelling out to `getent passwd`.
/// Mirrors `pwd.getpwnam(user).pw_dir` from the Python original.
fn user_home(user: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let out = Command::new("getent").args(["passwd", user]).output()?;
    if !out.status.success() {
        return Err(format!("user {user} not found in passwd").into());
    }
    let line = String::from_utf8(out.stdout)?;
    // passwd format: name:passwd:uid:gid:gecos:home:shell
    let home = line
        .trim_end_matches('\n')
        .split(':')
        .nth(5)
        .ok_or_else(|| format!("malformed passwd entry for {user}"))?
        .to_string();
    Ok(PathBuf::from(home))
}

fn sbuild_arch() -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("dpkg-architecture")
        .arg("-qDEB_BUILD_ARCH")
        .output()?;
    if !out.status.success() {
        return Err("dpkg-architecture failed".into());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Build the argv for mmdebstrap without actually invoking it.
/// Extracted so it can be unit-tested.
fn mmdebstrap_argv(
    distro: &Distribution,
    sbuild_path: &Path,
    arch: &str,
    include: &[String],
    setup_hooks: &[String],
) -> Result<Vec<String>, String> {
    let archive_mirror = distro
        .archive_mirror_uri
        .as_deref()
        .ok_or("distribution missing archive_mirror_uri")?;
    let name = distro.name.as_deref().ok_or("distribution missing name")?;
    let sbuild_str = sbuild_path
        .to_str()
        .ok_or("sbuild_path is not valid UTF-8")?;

    let mut argv: Vec<String> = vec![
        "--variant=buildd".into(),
        name.to_string(),
        sbuild_str.to_string(),
        archive_mirror.to_string(),
        "--mode=unshare".into(),
        format!("--arch={arch}"),
        format!("--components={}", distro.component.join(",")),
    ];

    if !include.is_empty() {
        argv.push(format!("--include={}", include.join(",")));
    }
    for extra_name in &distro.extra {
        argv.push(format!(
            "--extra-repository=deb {} {} {}",
            archive_mirror,
            extra_name,
            distro.component.join(" ")
        ));
    }
    for hook in setup_hooks {
        argv.push(format!("--setup-hook={hook}"));
    }
    Ok(argv)
}

fn create_chroot(
    distro: &Distribution,
    sbuild_path: &Path,
    suites: &[String],
    arch: &str,
    include: &[String],
    setup_hooks: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let argv = mmdebstrap_argv(distro, sbuild_path, arch, include, setup_hooks)?;
    let mut cmd = Command::new("mmdebstrap");
    cmd.args(&argv);

    println!("{}", shlex_join(&cmd));
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!("mmdebstrap exited with status {status}").into());
    }

    create_suite_symlinks(sbuild_path, suites, arch)
}

/// For each `suite`, create `<dir>/<suite>-<arch>-sbuild<ext>` →
/// `<basename>` symlinks alongside `sbuild_path`. `<ext>` matches
/// Python's `os.path.splitext` (last extension only — so
/// `unstable-amd64-sbuild.tar.xz` yields `.xz`).
fn create_suite_symlinks(
    sbuild_path: &Path,
    suites: &[String],
    arch: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let ext = sbuild_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let dirname = sbuild_path.parent().unwrap_or_else(|| Path::new("."));
    let basename = sbuild_path
        .file_name()
        .ok_or("sbuild_path has no filename")?;
    for suite in suites {
        let link_name = dirname.join(format!("{suite}-{arch}-sbuild{ext}"));
        std::os::unix::fs::symlink(basename, link_name)?;
    }
    Ok(())
}


/// Render a `Command` back to a shell-quoted string for logging.
fn shlex_join(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    shlex::try_join(parts.iter().map(String::as_str)).unwrap_or_else(|_| parts.join(" "))
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let config: Config = janitor::config::read_file(&args.config)
        .map_err(|e| format!("config path {}: {}", args.config.display(), e))?;

    let distributions: Vec<String> = if args.distribution.is_empty() {
        config
            .distribution
            .iter()
            .filter_map(|d| d.name.clone())
            .collect()
    } else {
        args.distribution.clone()
    };

    let base_directory = args.base_directory.unwrap_or_else(default_base_directory);
    let arch = sbuild_arch()?;

    for distribution in &distributions {
        let distro = config
            .get_distribution(distribution)
            .ok_or_else(|| format!("no such distribution: {distribution}"))?;

        let suites: Vec<String> = config
            .campaign
            .iter()
            .filter_map(|c| match c.build.as_ref()? {
                Build::DebianBuild(b)
                    if b.base_distribution.as_deref() == distro.name.as_deref() =>
                {
                    // A campaign without its own build_distribution builds into
                    // a suite named after the campaign.
                    b.build_distribution
                        .clone()
                        .filter(|d| !d.is_empty())
                        .or_else(|| c.name.clone())
                }
                _ => None,
            })
            .collect();

        let chroot_name = distro
            .chroot
            .as_deref()
            .ok_or_else(|| format!("distribution {distribution} has no chroot"))?;
        let sbuild_path = base_directory.join(format!("{chroot_name}.tar.xz"));

        let mut setup_hooks = Vec::new();
        if let Some(user) = args.user.as_deref() {
            let home = user_home(user)?;
            setup_hooks.push(format!(
                "install -d --owner={user} {}",
                home.display()
            ));
        }

        create_chroot(
            distro,
            &sbuild_path,
            &suites,
            &arch,
            &args.include,
            &setup_hooks,
        )?;
    }
    Ok(())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    if let Err(e) = run(args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_distro() -> Distribution {
        let mut d = Distribution::new();
        d.name = Some("unstable".into());
        d.archive_mirror_uri = Some("http://deb.debian.org/debian".into());
        d.chroot = Some("unstable-amd64-sbuild".into());
        d.component = vec!["main".into()];
        d
    }

    #[test]
    fn create_suite_symlinks_targets_basename_with_python_splitext_ext() {
        let dir = tempfile::tempdir().unwrap();
        let sbuild = dir.path().join("unstable-amd64-sbuild.tar.xz");
        std::fs::write(&sbuild, b"").unwrap();

        create_suite_symlinks(
            &sbuild,
            &["lintian-fixes".to_string(), "fresh-releases".to_string()],
            "amd64",
        )
        .unwrap();

        // Python's os.path.splitext drops only `.xz`, so symlinks
        // end in `.xz` (not `.tar.xz`); preserve that quirk.
        let lintian = dir.path().join("lintian-fixes-amd64-sbuild.xz");
        let fresh = dir.path().join("fresh-releases-amd64-sbuild.xz");
        assert!(lintian.is_symlink(), "expected symlink at {:?}", lintian);
        assert!(fresh.is_symlink(), "expected symlink at {:?}", fresh);
        assert_eq!(
            std::fs::read_link(&lintian).unwrap(),
            std::path::PathBuf::from("unstable-amd64-sbuild.tar.xz")
        );
    }

    #[test]
    fn create_suite_symlinks_on_sbuild_without_extension() {
        // sbuild_path without an extension → link names match bare
        // suite-arch-sbuild form.
        let dir = tempfile::tempdir().unwrap();
        let sbuild = dir.path().join("unstable-amd64-sbuild");
        std::fs::write(&sbuild, b"").unwrap();

        create_suite_symlinks(&sbuild, &["lintian-fixes".into()], "amd64").unwrap();
        assert!(dir
            .path()
            .join("lintian-fixes-amd64-sbuild")
            .is_symlink());
    }

    #[test]
    fn create_suite_symlinks_empty_suites_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let sbuild = dir.path().join("unstable-amd64-sbuild.tar.xz");
        std::fs::write(&sbuild, b"").unwrap();
        create_suite_symlinks(&sbuild, &[], "amd64").unwrap();
        // Only the tarball itself is left in the directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(
            "unstable-amd64-sbuild.tar.xz"
        )]);
    }

    #[test]
    fn mmdebstrap_argv_has_positional_header_variant_name_path_mirror_mode() {
        // mmdebstrap's positional args: VARIANT SUITE TARGET MIRROR;
        // --mode=unshare comes before --arch so the exit code matches
        // what the Python script sent.
        let distro = minimal_distro();
        let argv = mmdebstrap_argv(
            &distro,
            Path::new("/srv/unstable-amd64-sbuild.tar.xz"),
            "amd64",
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(argv[0], "--variant=buildd");
        assert_eq!(argv[1], "unstable");
        assert_eq!(argv[2], "/srv/unstable-amd64-sbuild.tar.xz");
        assert_eq!(argv[3], "http://deb.debian.org/debian");
        assert_eq!(argv[4], "--mode=unshare");
        assert_eq!(argv[5], "--arch=amd64");
        assert_eq!(argv[6], "--components=main");
    }

    #[test]
    fn mmdebstrap_argv_skips_include_when_empty() {
        let distro = minimal_distro();
        let argv = mmdebstrap_argv(
            &distro,
            Path::new("/srv/x.tar.xz"),
            "amd64",
            &[],
            &[],
        )
        .unwrap();
        assert!(
            !argv.iter().any(|a| a.starts_with("--include=")),
            "should not emit --include= for an empty list"
        );
    }

    #[test]
    fn mmdebstrap_argv_emits_include_and_setup_hooks() {
        let distro = minimal_distro();
        let argv = mmdebstrap_argv(
            &distro,
            Path::new("/srv/x.tar.xz"),
            "amd64",
            &["python3".into(), "eatmydata".into()],
            &["install -d /home/worker".into()],
        )
        .unwrap();
        assert!(argv.iter().any(|a| a == "--include=python3,eatmydata"));
        assert!(argv
            .iter()
            .any(|a| a == "--setup-hook=install -d /home/worker"));
    }

    #[test]
    fn mmdebstrap_argv_emits_extra_repository_with_space_joined_components() {
        let mut distro = minimal_distro();
        distro.component = vec!["main".into(), "contrib".into()];
        distro.extra = vec!["experimental".into()];
        let argv =
            mmdebstrap_argv(&distro, Path::new("/srv/x.tar.xz"), "amd64", &[], &[]).unwrap();
        assert!(argv.iter().any(|a| a
            == "--extra-repository=deb http://deb.debian.org/debian experimental main contrib"));
    }

    #[test]
    fn mmdebstrap_argv_rejects_distro_missing_required_fields() {
        let mut d = Distribution::new();
        d.component = vec!["main".into()];
        assert!(mmdebstrap_argv(&d, Path::new("/srv/x.tar.xz"), "amd64", &[], &[]).is_err());
    }
}
