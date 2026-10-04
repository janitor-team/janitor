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
