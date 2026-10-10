//! Creation of chroots for sbuild's schroot mode, with sbuild-createchroot.

use super::{extra_repository, Chroot, Error, TARBALL_EXTENSION};
use std::path::{Component, Path, PathBuf};

/// Directory in which schroot keeps the chroot definitions.
pub const DEFAULT_CONFIG_DIR: &str = "/etc/schroot/chroot.d";

/// Directory in which schroot keeps its open sessions.
pub const DEFAULT_SESSION_DIR: &str = "/var/lib/schroot/session";

/// Position of the target in the command built by `createchroot_command`.
pub const TARGET_INDEX: usize = 2;

/// Prefix of the temporary directory a chroot is built in for a tarball.
const SCRATCH_PREFIX: &str = ".sbuild-chroot.";

/// The chroot mode that sbuild-createchroot sets the chroot up for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
pub enum ChrootMode {
    #[default]
    Schroot,
    Sudo,
    Unshare,
}

impl ChrootMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChrootMode::Schroot => "schroot",
            ChrootMode::Sudo => "sudo",
            ChrootMode::Unshare => "unshare",
        }
    }
}

/// Options that apply to every chroot that is created.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub base_directory: PathBuf,
    pub arch: String,
    pub include: Vec<String>,
    pub eatmydata: bool,
    pub make_tarball: bool,
    pub chroot_mode: ChrootMode,
}

/// The work to do for one chroot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Name under which sbuild-createchroot registers the chroot.
    pub name: String,
    pub directory: PathBuf,
    pub tarball: PathBuf,
    pub make_tarball: bool,
    pub aliases: Vec<String>,
    pub command: Vec<String>,
}

/// Name that sbuild-createchroot gives the chroot of a suite.
pub fn schroot_name(suite: &str, arch: &str) -> String {
    format!("{}-{}-sbuild", suite, arch)
}

/// Build the sbuild-createchroot command line that creates a chroot.
pub fn createchroot_command(
    chroot: &Chroot,
    target: &Path,
    tarball: Option<&Path>,
    aliases: &[String],
    options: &Options,
) -> Vec<String> {
    let mut cmd = vec![
        "sbuild-createchroot".to_string(),
        chroot.suite.clone(),
        target.to_string_lossy().into_owned(),
        chroot.mirror.clone(),
        // The name of the chroot has the architecture in it
        format!("--arch={}", options.arch),
    ];
    if !chroot.components.is_empty() {
        cmd.push(format!("--components={}", chroot.components.join(",")));
    }
    let mut include = options.include.clone();
    if options.eatmydata {
        cmd.push("--command-prefix=eatmydata".to_string());
        if !include.iter().any(|p| p == "eatmydata") {
            include.push("eatmydata".to_string());
        }
    }
    if !include.is_empty() {
        cmd.push(format!("--include={}", include.join(",")));
    }
    for alias in aliases {
        cmd.push(format!("--alias={}", alias));
    }
    if let Some(tarball) = tarball {
        cmd.push(format!("--make-sbuild-tarball={}", tarball.display()));
    }
    cmd.push(format!("--chroot-mode={}", options.chroot_mode.as_str()));
    for name in &chroot.extra {
        cmd.push(format!(
            "--extra-repository={}",
            extra_repository(chroot, name)
        ));
    }
    cmd
}

/// Work out the name, paths, aliases and command for a chroot.
pub fn plan(chroot: &Chroot, options: &Options) -> Result<Job, Error> {
    if chroot.chroot.contains('/') || ["", ".", ".."].contains(&chroot.chroot.as_str()) {
        return Err(Error::InvalidChrootName(chroot.chroot.clone()));
    }
    if chroot.components.is_empty() && !chroot.extra.is_empty() {
        return Err(Error::ExtraWithoutComponents(chroot.suite.clone()));
    }
    let name = schroot_name(&chroot.suite, &options.arch);
    let mut aliases: Vec<String> = Vec::new();
    let campaigns = chroot
        .build_distributions
        .iter()
        .map(|d| schroot_name(d, &options.arch));
    for alias in chroot.aliases.iter().cloned().chain(campaigns) {
        // An alias equal to the name of the chroot is left out
        if alias != name && !aliases.contains(&alias) {
            aliases.push(alias);
        }
    }
    let incompatible = |what: &str| {
        Err(Error::IncompatibleChrootMode(format!(
            "sbuild chroot mode {} {} (chroot for {})",
            options.chroot_mode.as_str(),
            what,
            chroot.suite
        )))
    };
    if options.chroot_mode != ChrootMode::Schroot && options.eatmydata {
        return incompatible("can not use eatmydata; pass --no-eatmydata");
    }
    if options.chroot_mode == ChrootMode::Unshare {
        if !options.make_tarball {
            return incompatible("needs --make-sbuild-tarball");
        }
        if !aliases.is_empty() {
            return incompatible("does not support aliases");
        }
    }
    let directory = options.base_directory.join(&chroot.chroot);
    let tarball = options
        .base_directory
        .join(format!("{}{}", chroot.chroot, TARBALL_EXTENSION));
    let scratch = options
        .base_directory
        .join(format!("{}XXXXXX", SCRATCH_PREFIX));
    let command = if options.make_tarball {
        createchroot_command(chroot, &scratch, Some(&tarball), &aliases, options)
    } else {
        createchroot_command(chroot, &directory, None, &aliases, options)
    };
    Ok(Job {
        name,
        directory,
        tarball,
        make_tarball: options.make_tarball,
        aliases,
        command,
    })
}

/// Check that no name or path would be used for more than one job.
pub fn check_collisions(jobs: &[Job]) -> Result<(), Error> {
    let mut names = std::collections::HashSet::new();
    let mut directories = std::collections::HashSet::new();
    for job in jobs {
        for name in std::iter::once(&job.name).chain(job.aliases.iter()) {
            if !names.insert(name.as_str()) {
                return Err(Error::NameCollision(name.clone()));
            }
        }
        if !directories.insert(&job.directory) {
            return Err(Error::NameCollision(job.directory.display().to_string()));
        }
    }
    Ok(())
}

/// Command line that runs a shell in the chroot of a job.
pub fn shell_command(job: &Job) -> Vec<String> {
    vec!["sbuild-shell".to_string(), job.name.clone()]
}

/// Something that runs a command line, optionally with text on its standard input.
pub type Runner<'a> = dyn FnMut(&[String], Option<&str>) -> Result<(), Error> + 'a;

/// Create the chroot and run the commands in it, each on the standard input of a shell.
pub fn create(job: &Job, run_commands: &[String], runner: &mut Runner) -> Result<(), Error> {
    let mut command = job.command.clone();
    let mut scratch = None;
    if job.make_tarball {
        // Built next to the tarball, as a temporary directory may not allow device nodes
        let parent = job.tarball.parent().unwrap_or(Path::new("/"));
        std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.to_path_buf(), e))?;
        let path = tempfile::Builder::new()
            .prefix(SCRATCH_PREFIX)
            .tempdir_in(parent)
            .map_err(|e| Error::Io(parent.to_path_buf(), e))?
            .keep();
        command[TARGET_INDEX] = path.to_string_lossy().into_owned();
        scratch = Some(path);
    }
    log::info!("Running {}", super::format_command(&command));
    let result = runner(&command, None);
    if let Some(path) = scratch {
        // sbuild-createchroot removes the directory; only drop it if left empty
        let _ = std::fs::remove_dir(&path);
        if path.exists() {
            log::warn!("Leaving {} behind", path.display());
        }
    }
    result?;

    let shell = shell_command(job);
    for run_command in run_commands {
        log::info!("Running {:?} in {}", run_command, job.name);
        runner(&shell, Some(run_command))
            .map_err(|e| Error::RunCommandFailed(run_command.clone(), e.to_string()))?;
    }
    Ok(())
}

/// A chroot definition to remove, with the chroot it records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    /// Name of the chroot in schroot.
    pub name: String,
    pub entry: PathBuf,
    pub path: PathBuf,
    pub is_directory: bool,
}

/// Sections of a schroot chroot definition file, with their keys.
fn parse_sections(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((name.trim().to_string(), Vec::new()));
        } else if let (Some((key, value)), Some(section)) =
            (line.split_once('='), sections.last_mut())
        {
            section
                .1
                .push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    sections
}

/// Mount points listed in the text of /proc/self/mounts.
fn parse_mount_points(text: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    text.split(|b| *b == b'\n')
        .filter_map(|line| line.split(|b| *b == b' ').nth(1))
        .map(|raw| {
            // Unusual characters are written as a backslash and three octal digits
            let mut bytes = Vec::with_capacity(raw.len());
            let mut i = 0;
            while i < raw.len() {
                let digits = raw
                    .get(i + 1..i + 4)
                    .filter(|d| raw[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
                match digits {
                    Some(d) => {
                        let value = d.iter().fold(0u32, |v, c| v * 8 + u32::from(c - b'0'));
                        bytes.push(value as u8);
                        i += 4;
                    }
                    None => {
                        bytes.push(raw[i]);
                        i += 1;
                    }
                }
            }
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        })
        .collect()
}

/// Mount points of the running system.
pub fn mount_points() -> Result<Vec<PathBuf>, Error> {
    let path = Path::new("/proc/self/mounts");
    let text = std::fs::read(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    Ok(parse_mount_points(&text))
}

/// Check that schroot has no open session for a chroot.
fn check_no_session(name: &str, session_dir: &Path) -> Result<(), Error> {
    let io_error = |e| Error::Io(session_dir.to_path_buf(), e);
    let entries = match std::fs::read_dir(session_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_error(e)),
    };
    // A session is named after its chroot, followed by a dash and an identifier
    let prefix = format!("{}-", name);
    for entry in entries {
        let path = entry.map_err(io_error)?.path();
        let session = path.file_name().unwrap_or_default().to_string_lossy();
        // A session started under a name of its own records the chroot it came from
        let text = std::fs::read(&path).map_err(|e| Error::Io(path.clone(), e))?;
        let named = parse_sections(&String::from_utf8_lossy(&text))
            .iter()
            .flat_map(|(_, keys)| keys)
            .any(|(key, value)| key == "original-name" && value == name);
        if session.starts_with(&prefix) || named {
            return Err(Error::SessionOpen {
                chroot: name.to_string(),
                session: session.into_owned(),
            });
        }
    }
    Ok(())
}

/// Check that a recorded path is one that can be removed safely.
fn check_removable(path: &Path, is_directory: bool, mounts: &[PathBuf]) -> Result<(), Error> {
    let refuse = |reason| Err(Error::RefusingToRemove(path.to_path_buf(), reason));
    if !path.is_absolute() {
        return refuse("not an absolute path");
    }
    let mut depth = 0;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(_) => depth += 1,
            _ => return refuse("not a plain path"),
        }
    }
    if depth == 0 {
        return refuse("it is the root directory");
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(path.to_path_buf(), e)),
    };
    if !is_directory {
        if metadata.is_dir() {
            return refuse("it is a directory, not a tarball");
        }
        return Ok(());
    }
    // A symbolic link is never followed
    if !metadata.is_dir() {
        return refuse("it is not a directory");
    }
    let real = path
        .canonicalize()
        .map_err(|e| Error::Io(path.to_path_buf(), e))?;
    if mounts.iter().any(|m| m.starts_with(&real)) {
        return refuse("a filesystem is mounted on or in it");
    }
    Ok(())
}

/// Find the definitions of the chroot of a job, checking that they can be removed.
pub fn plan_remove_old(
    job: &Job,
    config_dir: &Path,
    session_dir: &Path,
    mounts: &[PathBuf],
) -> Result<Vec<Removal>, Error> {
    let io_error = |e| Error::Io(config_dir.to_path_buf(), e);
    let mut entries = match std::fs::read_dir(config_dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(io_error)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(io_error(e)),
    };
    entries.sort();

    let mut removals = Vec::new();
    let mut shared = None;
    // Every file is read, also the ones that schroot itself would skip
    for entry in entries {
        if !entry.is_file() {
            continue;
        }
        let text = std::fs::read(&entry).map_err(|e| Error::Io(entry.clone(), e))?;
        let sections = parse_sections(&String::from_utf8_lossy(&text));
        // The section is named as sbuild-createchroot names the chroot
        let Some((_, keys)) = sections.iter().find(|(name, _)| *name == job.name) else {
            let uses_path = |(key, value): &(String, String)| {
                (key == "directory" && Path::new(value) == job.directory)
                    || (key == "file" && Path::new(value) == job.tarball)
            };
            if sections.iter().any(|(_, keys)| keys.iter().any(uses_path)) {
                shared = Some(entry);
            }
            continue;
        };
        if sections.len() != 1 {
            return Err(Error::EntryShared(entry));
        }
        let get = |wanted: &str| {
            keys.iter()
                .find(|(key, _)| key == wanted)
                .map(|(_, value)| PathBuf::from(value))
        };
        let (recorded, expected, is_directory) = match (get("directory"), get("file")) {
            (Some(directory), None) => (directory, &job.directory, true),
            (None, Some(file)) => (file, &job.tarball, false),
            _ => return Err(Error::EntryWithoutPath(entry)),
        };
        // Compared as written, so that nothing else is ever removed
        if recorded.as_os_str() != expected.as_os_str() {
            return Err(Error::ChrootPathChanged {
                entry,
                recorded,
                expected: expected.clone(),
            });
        }
        check_removable(&recorded, is_directory, mounts)?;
        check_no_session(&job.name, session_dir)?;
        removals.push(Removal {
            name: job.name.clone(),
            entry,
            path: recorded,
            is_directory,
        });
    }
    match shared {
        Some(entry) if !removals.is_empty() => Err(Error::PathShared(entry)),
        _ => Ok(removals),
    }
}

/// Remove the chroots and definitions found by `plan_remove_old`.
pub fn remove_old(
    removals: &[Removal],
    session_dir: &Path,
    mounts: &dyn Fn() -> Result<Vec<PathBuf>, Error>,
) -> Result<(), Error> {
    for removal in removals {
        // Checked again, as creating the previous chroot takes a long time
        check_removable(&removal.path, removal.is_directory, &mounts()?)?;
        check_no_session(&removal.name, session_dir)?;
        log::info!("Removing {}", removal.path.display());
        let result = match std::fs::symlink_metadata(&removal.path) {
            Ok(metadata) if removal.is_directory && metadata.is_dir() => {
                std::fs::remove_dir_all(&removal.path)
            }
            Ok(metadata) if !removal.is_directory && !metadata.is_dir() => {
                std::fs::remove_file(&removal.path)
            }
            Ok(_) => {
                return Err(Error::RefusingToRemove(
                    removal.path.clone(),
                    "it changed while running",
                ))
            }
            Err(e) => Err(e),
        };
        match result {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(Error::Io(removal.path.clone(), e))
            }
            _ => {}
        }
        log::info!("Removing {}", removal.entry.display());
        std::fs::remove_file(&removal.entry).map_err(|e| Error::Io(removal.entry.clone(), e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::chroots_from_config;
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::ffi::OsStrExt;

    const CONFIG: &str = r#"
        distribution {
            name: "unstable"
            archive_mirror_uri: "http://deb.debian.org/debian"
            chroot: "unstable-amd64-sbuild"
            chroot_alias: "sid"
            chroot_alias: "UNRELEASED"
            component: "main"
            component: "contrib"
            extra: "experimental"
        }
        campaign {
            name: "lintian-fixes"
            debian_build { build_distribution: "lintian-fixes" base_distribution: "unstable" }
        }
        campaign {
            name: "unchanged"
            debian_build { build_distribution: "unstable" base_distribution: "unstable" }
        }
    "#;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn unstable() -> Chroot {
        let config = janitor::config::read_string(CONFIG).unwrap();
        chroots_from_config(&config, &[]).unwrap().remove(0)
    }

    fn options(base_directory: &Path) -> Options {
        Options {
            base_directory: base_directory.to_path_buf(),
            arch: "amd64".to_string(),
            eatmydata: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_plan_from_config() {
        let mut options = options(Path::new("/srv/chroots"));
        options.include = strings(&["ccache"]);
        let job = plan(&unstable(), &options).unwrap();
        assert_eq!(job.name, "unstable-amd64-sbuild");
        assert_eq!(
            job.directory,
            Path::new("/srv/chroots/unstable-amd64-sbuild")
        );
        assert_eq!(
            job.command,
            strings(&[
                "sbuild-createchroot",
                "unstable",
                "/srv/chroots/unstable-amd64-sbuild",
                "http://deb.debian.org/debian",
                "--arch=amd64",
                "--components=main,contrib",
                "--command-prefix=eatmydata",
                "--include=ccache,eatmydata",
                "--alias=sid",
                "--alias=UNRELEASED",
                "--alias=lintian-fixes-amd64-sbuild",
                "--chroot-mode=schroot",
                "--extra-repository=deb http://deb.debian.org/debian experimental main contrib",
            ])
        );
    }

    #[test]
    fn test_plan_without_config() {
        let chroot = Chroot::new(
            "bookworm",
            "http://m/debian",
            "bookworm-build",
            &strings(&["main"]),
            &[],
            &strings(&["backports", "backports"]),
        )
        .with_aliases(&strings(&["stable", "backports-arm64-sbuild"]));
        let mut options = options(Path::new("/c"));
        options.arch = "arm64".to_string();
        options.eatmydata = false;
        let job = plan(&chroot, &options).unwrap();
        assert_eq!(job.name, "bookworm-arm64-sbuild");
        assert_eq!(job.aliases, strings(&["stable", "backports-arm64-sbuild"]));
        assert_eq!(
            job.command,
            strings(&[
                "sbuild-createchroot",
                "bookworm",
                "/c/bookworm-build",
                "http://m/debian",
                "--arch=arm64",
                "--components=main",
                "--alias=stable",
                "--alias=backports-arm64-sbuild",
                "--chroot-mode=schroot",
            ])
        );
    }

    #[test]
    fn test_plan_eatmydata_already_included() {
        let mut options = options(Path::new("/c"));
        options.include = strings(&["eatmydata", "ccache"]);
        let job = plan(&unstable(), &options).unwrap();
        assert!(job
            .command
            .contains(&"--include=eatmydata,ccache".to_string()));

        options.eatmydata = false;
        options.include = vec![];
        let job = plan(&unstable(), &options).unwrap();
        assert!(!job.command.iter().any(|a| a.starts_with("--include")));
        assert!(!job
            .command
            .iter()
            .any(|a| a.starts_with("--command-prefix")));
    }

    #[test]
    fn test_plan_tarball() {
        let mut options = options(Path::new("/srv/chroots"));
        options.make_tarball = true;
        let job = plan(&unstable(), &options).unwrap();
        assert!(job.make_tarball);
        assert_eq!(
            job.tarball,
            Path::new("/srv/chroots/unstable-amd64-sbuild.tar.xz")
        );
        assert!(job.command.contains(
            &"--make-sbuild-tarball=/srv/chroots/unstable-amd64-sbuild.tar.xz".to_string()
        ));
        assert_eq!(
            job.command[TARGET_INDEX],
            "/srv/chroots/.sbuild-chroot.XXXXXX"
        );
    }

    #[test]
    fn test_plan_chroot_modes() {
        let mut options = options(Path::new("/c"));
        options.chroot_mode = ChrootMode::Sudo;
        let err = plan(&unstable(), &options).unwrap_err();
        assert!(err.to_string().contains("--no-eatmydata"), "{}", err);

        options.eatmydata = false;
        let job = plan(&unstable(), &options).unwrap();
        assert!(job.command.contains(&"--chroot-mode=sudo".to_string()));

        options.chroot_mode = ChrootMode::Unshare;
        let err = plan(&unstable(), &options).unwrap_err();
        assert!(err.to_string().contains("--make-sbuild-tarball"), "{}", err);

        options.make_tarball = true;
        let err = plan(&unstable(), &options).unwrap_err();
        assert!(err.to_string().contains("aliases"), "{}", err);

        let chroot = Chroot::new("sid", "http://m", "sid-amd64-sbuild", &[], &[], &[]);
        let job = plan(&chroot, &options).unwrap();
        assert!(job.command.contains(&"--chroot-mode=unshare".to_string()));
    }

    #[test]
    fn test_plan_invalid() {
        for name in ["", ".", "..", "a/b"] {
            let mut chroot = unstable();
            chroot.chroot = name.to_string();
            assert!(matches!(
                plan(&chroot, &options(Path::new("/c"))),
                Err(Error::InvalidChrootName(n)) if n == name
            ));
        }
        let mut chroot = unstable();
        chroot.components = vec![];
        assert!(matches!(
            plan(&chroot, &options(Path::new("/c"))),
            Err(Error::ExtraWithoutComponents(n)) if n == "unstable"
        ));
    }

    #[test]
    fn test_check_collisions() {
        let options = options(Path::new("/c"));
        let first = plan(&unstable(), &options).unwrap();
        let other = |suite: &str, chroot: &str, aliases: &[&str]| {
            let chroot = Chroot::new(suite, "http://m", chroot, &[], &[], &[])
                .with_aliases(&strings(aliases));
            plan(&chroot, &options).unwrap()
        };
        check_collisions(&[first.clone(), other("bookworm", "bookworm", &["stable"])]).unwrap();
        assert!(matches!(
            check_collisions(&[first.clone(), other("bookworm", "bookworm", &["sid"])]),
            Err(Error::NameCollision(n)) if n == "sid"
        ));
        assert!(matches!(
            check_collisions(&[first.clone(), other("bookworm", "unstable-amd64-sbuild", &[])]),
            Err(Error::NameCollision(n)) if n == "/c/unstable-amd64-sbuild"
        ));
    }

    type Calls = RefCell<Vec<(Vec<String>, Option<String>)>>;

    fn record(calls: &Calls, command: &[String], input: Option<&str>) {
        calls
            .borrow_mut()
            .push((command.to_vec(), input.map(|s| s.to_string())));
    }

    #[test]
    fn test_create_runs_commands_on_stdin() {
        let job = plan(&unstable(), &options(Path::new("/c"))).unwrap();
        let calls = Calls::default();
        create(
            &job,
            &strings(&["apt -y install foo", "echo done"]),
            &mut |command, input| {
                record(&calls, command, input);
                Ok(())
            },
        )
        .unwrap();
        let shell = strings(&["sbuild-shell", "unstable-amd64-sbuild"]);
        assert_eq!(
            calls.into_inner(),
            vec![
                (job.command.clone(), None),
                (shell.clone(), Some("apt -y install foo".to_string())),
                (shell, Some("echo done".to_string())),
            ]
        );
    }

    #[test]
    fn test_create_failing_run_command() {
        let job = plan(&unstable(), &options(Path::new("/c"))).unwrap();
        let calls = Calls::default();
        let result = create(&job, &strings(&["false", "true"]), &mut |command, input| {
            record(&calls, command, input);
            match input {
                Some("false") => Err(Error::BuildFailed("sbuild-shell failed: 1".to_string())),
                _ => Ok(()),
            }
        });
        let err = result.unwrap_err();
        assert!(matches!(&err, Error::RunCommandFailed(c, _) if c == "false"));
        assert!(err.to_string().contains("sbuild-shell failed: 1"));
        assert_eq!(calls.borrow().len(), 2);
    }

    #[test]
    fn test_create_failure_skips_run_commands() {
        let job = plan(&unstable(), &options(Path::new("/c"))).unwrap();
        let calls = Calls::default();
        let result = create(&job, &strings(&["true"]), &mut |command, input| {
            record(&calls, command, input);
            Err(Error::BuildFailed("sbuild-createchroot failed".to_string()))
        });
        assert!(matches!(result, Err(Error::BuildFailed(_))));
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn test_create_tarball_builds_next_to_it() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().join("chroots");
        let mut options = options(&base);
        options.make_tarball = true;
        let job = plan(&unstable(), &options).unwrap();
        let targets = RefCell::new(Vec::new());
        create(&job, &[], &mut |command, _| {
            let target = PathBuf::from(&command[TARGET_INDEX]);
            assert!(target.is_dir());
            assert_eq!(target.parent(), Some(base.as_path()));
            assert_ne!(command[TARGET_INDEX], job.command[TARGET_INDEX]);
            assert_eq!(command[TARGET_INDEX + 1..], job.command[TARGET_INDEX + 1..]);
            targets.borrow_mut().push(target);
            Ok(())
        })
        .unwrap();
        let targets = targets.into_inner();
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].exists());
    }

    /// A base directory with a chroot, and a chroot.d directory.
    struct Fixture {
        _td: tempfile::TempDir,
        base: PathBuf,
        config_dir: PathBuf,
        session_dir: PathBuf,
        job: Job,
    }

    impl Fixture {
        fn plan(&self, mounts: &[PathBuf]) -> Result<Vec<Removal>, Error> {
            plan_remove_old(&self.job, &self.config_dir, &self.session_dir, mounts)
        }

        fn remove(&self, removals: &[Removal]) -> Result<(), Error> {
            remove_old(removals, &self.session_dir, &|| Ok(vec![]))
        }
    }

    fn fixture(make_tarball: bool) -> Fixture {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().canonicalize().unwrap();
        let base = root.join("chroots");
        let config_dir = root.join("chroot.d");
        let session_dir = root.join("session");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        let mut options = options(&base);
        options.make_tarball = make_tarball;
        let job = plan(&unstable(), &options).unwrap();
        std::fs::create_dir_all(job.directory.join("etc")).unwrap();
        std::fs::write(job.directory.join("etc/hostname"), b"chroot").unwrap();
        Fixture {
            _td: td,
            base,
            config_dir,
            session_dir,
            job,
        }
    }

    fn write_entry(fixture: &Fixture, name: &str, section: &str, location: &str) -> PathBuf {
        let path = fixture.config_dir.join(name);
        let text = format!(
            "# written by sbuild-createchroot\n[{}]\ndescription=Debian unstable/amd64 autobuilder\n\
             groups=root,sbuild\nprofile=sbuild\n{}\naliases=sid\n",
            section, location
        );
        std::fs::write(&path, text).unwrap();
        path
    }

    fn directory_entry(fixture: &Fixture, directory: &Path) -> PathBuf {
        write_entry(
            fixture,
            "unstable-amd64-sbuild-Ab12Cd",
            "unstable-amd64-sbuild",
            &format!("type=directory\ndirectory={}", directory.display()),
        )
    }

    #[test]
    fn test_remove_old_matching_entry() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        let other = write_entry(&f, "bookworm", "bookworm-amd64-sbuild", "directory=/x");
        let removals = f.plan(&[]).unwrap();
        assert_eq!(
            removals,
            vec![Removal {
                name: "unstable-amd64-sbuild".to_string(),
                entry: entry.clone(),
                path: f.job.directory.clone(),
                is_directory: true,
            }]
        );
        // Planning changes nothing
        assert!(entry.exists() && f.job.directory.join("etc/hostname").exists());

        f.remove(&removals).unwrap();
        assert!(!entry.exists());
        assert!(!f.job.directory.exists());
        assert!(other.exists());
        assert!(f.base.exists());
    }

    #[test]
    fn test_remove_old_directory_already_gone() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        std::fs::remove_dir_all(&f.job.directory).unwrap();
        let removals = f.plan(&[]).unwrap();
        f.remove(&removals).unwrap();
        assert!(!entry.exists());
    }

    #[test]
    fn test_remove_old_different_directory() {
        let f = fixture(false);
        let elsewhere = f.base.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let entry = directory_entry(&f, &elsewhere);
        let err = f.plan(&[]).unwrap_err();
        assert!(matches!(
            &err,
            Error::ChrootPathChanged { recorded, expected, .. }
                if *recorded == elsewhere && *expected == f.job.directory
        ));
        assert!(err.to_string().contains("sbuild path has changed"));
        assert!(entry.exists() && elsewhere.exists() && f.job.directory.exists());
    }

    #[test]
    fn test_remove_old_no_matching_entry() {
        let f = fixture(false);
        // The name of the file does not matter, only that of the section
        let other = write_entry(
            &f,
            "unstable-amd64-sbuild-Ab12Cd",
            "bookworm-amd64-sbuild",
            &format!("directory={}", f.job.directory.display()),
        );
        assert!(f.plan(&[]).unwrap().is_empty());
        assert!(other.exists() && f.job.directory.exists());

        let missing = f.base.join("no-such-chroot.d");
        assert!(plan_remove_old(&f.job, &missing, &f.session_dir, &[])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_remove_old_tarball() {
        let f = fixture(true);
        std::fs::write(&f.job.tarball, b"tarball").unwrap();
        let entry = write_entry(
            &f,
            "unstable-amd64-sbuild-Ab12Cd",
            "unstable-amd64-sbuild",
            &format!("type=file\nfile={}", f.job.tarball.display()),
        );
        let removals = f.plan(&[]).unwrap();
        assert!(!removals[0].is_directory);
        f.remove(&removals).unwrap();
        assert!(!entry.exists() && !f.job.tarball.exists());
        // The directory is not what the entry recorded
        assert!(f.job.directory.exists());
    }

    #[test]
    fn test_remove_old_refuses_dangerous_paths() {
        for directory in ["/", "", "relative/path", "//", "/."] {
            let mut f = fixture(false);
            f.job.directory = PathBuf::from(directory);
            let entry = directory_entry(&f, Path::new(directory));
            let err = f.plan(&[]).unwrap_err();
            assert!(
                matches!(&err, Error::RefusingToRemove(..)),
                "{:?}: {}",
                directory,
                err
            );
            assert!(entry.exists());
        }

        let mut f = fixture(false);
        f.job.directory = f.base.join("a/../unstable-amd64-sbuild");
        directory_entry(&f, &f.job.directory.clone());
        let err = f.plan(&[]).unwrap_err();
        assert!(matches!(&err, Error::RefusingToRemove(_, r) if *r == "not a plain path"));
    }

    #[test]
    fn test_remove_old_refuses_symlink() {
        let f = fixture(false);
        let precious = f.base.join("precious");
        std::fs::create_dir(&precious).unwrap();
        std::fs::write(precious.join("data"), b"data").unwrap();
        std::fs::remove_dir_all(&f.job.directory).unwrap();
        std::os::unix::fs::symlink(&precious, &f.job.directory).unwrap();
        let entry = directory_entry(&f, &f.job.directory);
        let err = f.plan(&[]).unwrap_err();
        assert!(matches!(&err, Error::RefusingToRemove(_, r) if *r == "it is not a directory"));
        assert!(entry.exists() && precious.join("data").exists());
    }

    #[test]
    fn test_remove_old_keeps_targets_of_inner_symlinks() {
        let f = fixture(false);
        let precious = f.base.join("precious");
        std::fs::create_dir(&precious).unwrap();
        std::fs::write(precious.join("data"), b"data").unwrap();
        std::os::unix::fs::symlink(&precious, f.job.directory.join("link")).unwrap();
        directory_entry(&f, &f.job.directory);
        let removals = f.plan(&[]).unwrap();
        f.remove(&removals).unwrap();
        assert!(!f.job.directory.exists());
        assert!(precious.join("data").exists());
    }

    #[test]
    fn test_remove_old_refuses_mounted() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        let mounts = vec![PathBuf::from("/proc"), f.job.directory.join("proc")];
        let err = f.plan(&mounts).unwrap_err();
        assert!(
            matches!(&err, Error::RefusingToRemove(_, r) if *r == "a filesystem is mounted on or in it")
        );
        // The chroot may be a mount point itself
        let mounts = vec![f.job.directory.clone()];
        assert!(matches!(f.plan(&mounts), Err(Error::RefusingToRemove(..))));
        assert!(entry.exists() && f.job.directory.exists());
        // A mount next to the chroot is fine
        let mounts = vec![f.base.clone(), f.base.join("unstable-amd64-sbuild2")];
        assert_eq!(f.plan(&mounts).unwrap().len(), 1);
    }

    #[test]
    fn test_remove_old_refuses_shared_or_incomplete_entry() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        let mut text = std::fs::read_to_string(&entry).unwrap();
        text.push_str("[other]\ndirectory=/srv/other\n");
        std::fs::write(&entry, text).unwrap();
        assert!(matches!(
            f.plan(&[]),
            Err(Error::EntryShared(p)) if p == entry
        ));

        let entry = write_entry(&f, "unstable-amd64-sbuild-Ab12Cd", &f.job.name, "type=x");
        assert!(matches!(
            f.plan(&[]),
            Err(Error::EntryWithoutPath(p)) if p == entry
        ));
        assert!(entry.exists() && f.job.directory.exists());
    }

    #[test]
    fn test_remove_old_refuses_open_session() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        std::fs::create_dir(&f.session_dir).unwrap();
        // A session of another chroot does not matter
        std::fs::write(f.session_dir.join("bookworm-amd64-sbuild-1234"), b"").unwrap();
        let removals = f.plan(&[]).unwrap();

        let session = "unstable-amd64-sbuild-0f6b1c9e";
        std::fs::write(f.session_dir.join(session), b"").unwrap();
        let err = f.plan(&[]).unwrap_err();
        assert!(matches!(&err, Error::SessionOpen { session: s, .. } if s == session));
        // Also when the session was opened after planning
        assert!(matches!(
            f.remove(&removals),
            Err(Error::SessionOpen { .. })
        ));
        assert!(entry.exists() && f.job.directory.join("etc/hostname").exists());

        // A session with a name of its own is found by the chroot it records
        std::fs::remove_file(f.session_dir.join(session)).unwrap();
        f.plan(&[]).unwrap();
        let text = "[mysession]\noriginal-name=unstable-amd64-sbuild\n";
        std::fs::write(f.session_dir.join("mysession"), text).unwrap();
        let err = f.plan(&[]).unwrap_err();
        assert!(matches!(&err, Error::SessionOpen { session: s, .. } if s == "mysession"));
    }

    #[test]
    fn test_remove_old_checks_mounts_again() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        let removals = f.plan(&[]).unwrap();
        let mounted = f.job.directory.join("proc");
        let err = remove_old(&removals, &f.session_dir, &|| Ok(vec![mounted.clone()])).unwrap_err();
        assert!(matches!(&err, Error::RefusingToRemove(..)));
        let unreadable = || {
            Err(Error::Io(
                PathBuf::from("/proc/self/mounts"),
                std::io::ErrorKind::NotFound.into(),
            ))
        };
        assert!(matches!(
            remove_old(&removals, &f.session_dir, &unreadable),
            Err(Error::Io(..))
        ));
        assert!(entry.exists() && f.job.directory.join("etc/hostname").exists());
    }

    #[test]
    fn test_remove_old_changed_while_running() {
        let f = fixture(true);
        std::fs::write(&f.job.tarball, b"tarball").unwrap();
        let entry = write_entry(
            &f,
            "unstable-amd64-sbuild-Ab12Cd",
            "unstable-amd64-sbuild",
            &format!("file={}", f.job.tarball.display()),
        );
        let removals = f.plan(&[]).unwrap();
        std::fs::remove_file(&f.job.tarball).unwrap();
        std::fs::create_dir(&f.job.tarball).unwrap();
        let err = f.remove(&removals).unwrap_err();
        assert!(
            matches!(&err, Error::RefusingToRemove(_, r) if *r == "it is a directory, not a tarball"),
            "{}",
            err
        );
        assert!(entry.exists() && f.job.tarball.is_dir());
    }

    #[test]
    fn test_remove_old_different_tarball() {
        let f = fixture(true);
        let elsewhere = f.base.join("elsewhere.tar.xz");
        std::fs::write(&elsewhere, b"tarball").unwrap();
        let entry = write_entry(
            &f,
            "unstable-amd64-sbuild-Ab12Cd",
            "unstable-amd64-sbuild",
            &format!("type=file\nfile={}", elsewhere.display()),
        );
        assert!(matches!(
            f.plan(&[]),
            Err(Error::ChrootPathChanged { recorded, expected, .. })
                if recorded == elsewhere && expected == f.job.tarball
        ));
        assert!(entry.exists() && elsewhere.exists());
    }

    #[test]
    fn test_remove_old_tarball_that_is_a_directory() {
        let f = fixture(true);
        std::fs::create_dir(&f.job.tarball).unwrap();
        write_entry(
            &f,
            "unstable-amd64-sbuild-Ab12Cd",
            "unstable-amd64-sbuild",
            &format!("file={}", f.job.tarball.display()),
        );
        assert!(matches!(
            f.plan(&[]),
            Err(Error::RefusingToRemove(_, r)) if r == "it is a directory, not a tarball"
        ));
    }

    #[test]
    fn test_remove_old_entry_with_directory_and_file() {
        let f = fixture(false);
        let entry = write_entry(
            &f,
            "unstable-amd64-sbuild-Ab12Cd",
            "unstable-amd64-sbuild",
            &format!(
                "directory={}\nfile={}",
                f.job.directory.display(),
                f.job.tarball.display()
            ),
        );
        assert!(matches!(
            f.plan(&[]),
            Err(Error::EntryWithoutPath(p)) if p == entry
        ));
    }

    #[test]
    fn test_remove_old_entry_with_repeated_and_translated_keys() {
        let f = fixture(false);
        // The first value of a repeated key is the one that is compared
        let entry = write_entry(
            &f,
            "unstable-amd64-sbuild-Ab12Cd",
            "unstable-amd64-sbuild",
            &format!(
                "description[fr]=directory=/srv/autre\ndirectory={}\ndirectory=/srv/other",
                f.job.directory.display()
            ),
        );
        let removals = f.plan(&[]).unwrap();
        assert_eq!(removals[0].path, f.job.directory);
        f.remove(&removals).unwrap();
        assert!(!entry.exists() && !f.job.directory.exists());
    }

    #[test]
    fn test_remove_old_path_shared_with_another_chroot() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        let other = write_entry(
            &f,
            "other",
            "other-amd64-sbuild",
            &format!("directory={}", f.job.directory.display()),
        );
        assert!(matches!(
            f.plan(&[]),
            Err(Error::PathShared(p)) if p == other
        ));
        assert!(entry.exists() && f.job.directory.exists());
    }

    #[test]
    fn test_remove_old_symlinked_parent() {
        let f = fixture(false);
        let link = f.base.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&f.base, &link).unwrap();
        let mut job = f.job.clone();
        job.directory = link.join("unstable-amd64-sbuild");
        directory_entry(&f, &job.directory);
        // The mount is found through the resolved path
        let mounts = vec![f.job.directory.join("proc")];
        assert!(matches!(
            plan_remove_old(&job, &f.config_dir, &f.session_dir, &mounts),
            Err(Error::RefusingToRemove(..))
        ));
    }

    #[test]
    fn test_parse_mount_points() {
        assert_eq!(
            parse_mount_points(
                b"proc /proc proc rw 0 0\n/dev/sda1 /srv/my\\040chroots ext4 rw 0 0\n\
                  /dev/sda2 /srv/back\\134slash ext4 rw 0 0\n/dev/sda3 /srv/\\+12\\377 ext4 rw 0 0\n"
            ),
            vec![
                PathBuf::from("/proc"),
                PathBuf::from("/srv/my chroots"),
                PathBuf::from("/srv/back\\slash"),
                PathBuf::from(std::ffi::OsStr::from_bytes(b"/srv/\\+12\xff")),
            ]
        );
        assert!(mount_points().unwrap().contains(&PathBuf::from("/proc")));
    }

    #[test]
    fn test_parse_sections() {
        let text = "# written by sbuild-createchroot\n\
                    [ unstable-amd64-sbuild ]\n\
                    # directory=/srv/commented-out\n\
                    ; file=/srv/also-commented-out\n\
                    \x20 directory = /srv/chroots/unstable-amd64-sbuild \n\
                    \n\
                    [other]\n";
        assert_eq!(
            parse_sections(text),
            vec![
                (
                    "unstable-amd64-sbuild".to_string(),
                    vec![(
                        "directory".to_string(),
                        "/srv/chroots/unstable-amd64-sbuild".to_string()
                    )]
                ),
                ("other".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn test_remove_old_skips_entries_that_are_not_files() {
        let f = fixture(false);
        let entry = directory_entry(&f, &f.job.directory);
        // schroot reads the regular files in the directory
        std::fs::create_dir(f.config_dir.join("a-directory")).unwrap();
        let removals = f.plan(&[]).unwrap();
        assert_eq!(removals.len(), 1);
        assert_eq!(removals[0].entry, entry);
    }

    #[test]
    fn test_run_command_with_input() {
        use super::super::run_command_with_input;
        run_command_with_input(&strings(&["sh"]), Some("exit 0")).unwrap();
        let err = run_command_with_input(&strings(&["sh"]), Some("exit 3")).unwrap_err();
        assert!(matches!(&err, Error::BuildFailed(m) if m.starts_with("sh failed")));
    }
}
