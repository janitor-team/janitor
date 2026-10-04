//! Creation of chroot tarballs for sbuild's unshare mode.

use janitor::config::Config;
use std::path::{Path, PathBuf};

/// Extension of the tarballs that are created.
const TARBALL_EXTENSION: &str = ".tar.xz";

#[derive(Debug)]
pub enum Error {
    NoSuchDistribution(String),
    MissingMirror(String),
    MissingChroot(String),
    UnusableChrootName(String),
    UnusableBuildDistribution(String),
    ExtraWithoutComponents(String),
    NameCollision(String),
    UnknownUser(String),
    UnusableHome(String, PathBuf),
    TarballBlocked(PathBuf),
    NoCacheDirectory,
    LinkBlocked(PathBuf),
    BuildFailed(String),
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::NoSuchDistribution(name) => write!(f, "no such distribution: {}", name),
            Error::MissingMirror(name) => {
                write!(f, "distribution {} has no archive_mirror_uri", name)
            }
            Error::MissingChroot(name) => write!(f, "distribution {} has no chroot", name),
            Error::UnusableChrootName(name) => write!(
                f,
                "chroot name {} can not be found by sbuild in unshare mode",
                name
            ),
            Error::UnusableBuildDistribution(name) => write!(
                f,
                "no chroot name that sbuild can find exists for build distribution {}",
                name
            ),
            Error::ExtraWithoutComponents(name) => {
                write!(f, "extra suites for {} need at least one component", name)
            }
            Error::NameCollision(name) => {
                write!(f, "{} would be created for more than one chroot", name)
            }
            Error::UnknownUser(name) => write!(f, "unable to resolve user {}", name),
            Error::UnusableHome(name, home) => write!(
                f,
                "home directory of user {} is not an absolute path: {:?}",
                name, home
            ),
            Error::TarballBlocked(path) => write!(
                f,
                "{} is a directory; not replacing it with a tarball",
                path.display()
            ),
            Error::NoCacheDirectory => write!(
                f,
                "unable to determine the cache directory; pass --base-directory"
            ),
            Error::LinkBlocked(path) => write!(
                f,
                "{} exists and is not a symbolic link; not replacing it",
                path.display()
            ),
            Error::BuildFailed(e) => write!(f, "{}", e),
            Error::Io(path, e) => write!(f, "{}: {}", path.display(), e),
        }
    }
}

impl std::error::Error for Error {}

/// A chroot to create, and the build distributions that use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chroot {
    pub suite: String,
    pub mirror: String,
    pub chroot: String,
    pub components: Vec<String>,
    pub extra: Vec<String>,
    pub build_distributions: Vec<String>,
}

impl Chroot {
    pub fn new(
        suite: &str,
        mirror: &str,
        chroot: &str,
        components: &[String],
        extra: &[String],
        build_distributions: &[String],
    ) -> Self {
        let mut unique: Vec<String> = Vec::new();
        for name in build_distributions {
            if !unique.contains(name) {
                unique.push(name.clone());
            }
        }
        Chroot {
            suite: suite.to_string(),
            mirror: mirror.to_string(),
            chroot: chroot.to_string(),
            components: components.to_vec(),
            extra: extra.to_vec(),
            build_distributions: unique,
        }
    }
}

/// Options that apply to every chroot that is created.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub base_directory: PathBuf,
    pub arch: String,
    pub include: Vec<String>,
    pub customize_hooks: Vec<String>,
}

/// The work to do for one chroot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub tarball: PathBuf,
    pub command: Vec<String>,
    pub links: Vec<String>,
}

impl Job {
    /// Name that the links point at.
    pub fn link_target(&self) -> &str {
        self.tarball.file_name().unwrap().to_str().unwrap()
    }

    /// Whether the tarball has to be built.
    pub fn needs_build(&self, force: bool) -> bool {
        force
            || !std::fs::metadata(&self.tarball)
                .map(|m| m.is_file() && m.len() > 0)
                .unwrap_or(false)
    }
}

/// Find the chroots for the named distributions (all if none are named).
pub fn chroots_from_config(config: &Config, names: &[String]) -> Result<Vec<Chroot>, Error> {
    let names: Vec<String> = if names.is_empty() {
        config
            .distribution
            .iter()
            .map(|d| d.name().to_string())
            .collect()
    } else {
        names.to_vec()
    };

    let mut chroots = Vec::new();
    for name in names {
        let distribution = config
            .get_distribution(&name)
            .ok_or_else(|| Error::NoSuchDistribution(name.clone()))?;
        if distribution.archive_mirror_uri().is_empty() {
            return Err(Error::MissingMirror(name));
        }
        if distribution.chroot().is_empty() {
            return Err(Error::MissingChroot(name));
        }
        let build_distributions: Vec<String> = config
            .campaign
            .iter()
            .filter(|c| c.has_debian_build())
            .map(|c| c.debian_build())
            .filter(|b| b.base_distribution() == name && !b.build_distribution().is_empty())
            .map(|b| b.build_distribution().to_string())
            .collect();
        chroots.push(Chroot::new(
            &name,
            distribution.archive_mirror_uri(),
            distribution.chroot(),
            &distribution.component,
            &distribution.extra,
            &build_distributions,
        ));
    }
    Ok(chroots)
}

/// Check whether sbuild's unshare mode would consider a file name a chroot.
pub fn is_sbuild_chroot_filename(filename: &str) -> bool {
    // Same as sbuild's ^[^-]+-[^-]+(-[^-]+)?(-sbuild)?\.t.+$
    filename.match_indices(".t").any(|(i, _)| {
        let rest = &filename[i + 2..];
        if rest.is_empty() || rest.contains('\n') {
            return false;
        }
        let parts: Vec<&str> = filename[..i].split('-').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return false;
        }
        match parts.len() {
            2 | 3 => true,
            4 => parts[3] == "sbuild",
            _ => false,
        }
    })
}

/// Name under which sbuild registers a chroot file.
fn sbuild_chroot_name(filename: &str) -> Option<&str> {
    if !is_sbuild_chroot_filename(filename) {
        return None;
    }
    // sbuild cuts the name at the first ".t" that is followed by anything
    filename
        .match_indices(".t")
        .find(|(i, _)| i + 2 < filename.len())
        .map(|(i, _)| &filename[..i])
}

/// Check whether sbuild registers a chroot tarball under the given name.
fn is_usable_chroot_name(name: &str) -> bool {
    sbuild_chroot_name(&format!("{}{}", name, TARBALL_EXTENSION)) == Some(name)
}

/// Pick the chroot name under which sbuild will find a distribution.
pub fn chroot_name_for_distribution(distribution: &str, arch: &str) -> Option<String> {
    // The order in which sbuild looks for a chroot
    [
        format!("{}-{}-sbuild", distribution, arch),
        format!("{}-sbuild", distribution),
        format!("{}-{}", distribution, arch),
        distribution.to_string(),
    ]
    .into_iter()
    .find(|name| is_usable_chroot_name(name))
}

/// Position of the target in the command built by `mmdebstrap_command`.
const TARGET_INDEX: usize = 3;

/// Build the mmdebstrap command line that creates a chroot tarball.
pub fn mmdebstrap_command(chroot: &Chroot, tarball: &Path, options: &Options) -> Vec<String> {
    let mut cmd = vec![
        "mmdebstrap".to_string(),
        "--variant=buildd".to_string(),
        chroot.suite.clone(),
        tarball.to_string_lossy().into_owned(),
        chroot.mirror.clone(),
        "--mode=unshare".to_string(),
        format!("--arch={}", options.arch),
    ];
    if !chroot.components.is_empty() {
        cmd.push(format!("--components={}", chroot.components.join(",")));
    }
    if !options.include.is_empty() {
        cmd.push(format!("--include={}", options.include.join(",")));
    }
    for name in &chroot.extra {
        let mut entry = format!("deb {} {}", chroot.mirror, name);
        for component in &chroot.components {
            entry.push(' ');
            entry.push_str(component);
        }
        cmd.push(format!("--extra-repository={}", entry));
    }
    for hook in &options.customize_hooks {
        cmd.push(format!("--customize-hook={}", hook));
    }
    cmd
}

/// Work out the tarball, command and links for a chroot.
pub fn plan(chroot: &Chroot, options: &Options) -> Result<Job, Error> {
    let filename = format!("{}{}", chroot.chroot, TARBALL_EXTENSION);
    if chroot.chroot.contains('/') || !is_usable_chroot_name(&chroot.chroot) {
        return Err(Error::UnusableChrootName(chroot.chroot.clone()));
    }
    if chroot.components.is_empty() && !chroot.extra.is_empty() {
        return Err(Error::ExtraWithoutComponents(chroot.suite.clone()));
    }
    let tarball = options.base_directory.join(&filename);
    let mut links = Vec::new();
    for distribution in &chroot.build_distributions {
        let name = chroot_name_for_distribution(distribution, &options.arch)
            .filter(|_| !distribution.contains('/'))
            .ok_or_else(|| Error::UnusableBuildDistribution(distribution.clone()))?;
        let link = format!("{}{}", name, TARBALL_EXTENSION);
        if link != filename && !links.contains(&link) {
            links.push(link);
        }
    }
    Ok(Job {
        command: mmdebstrap_command(chroot, &tarball, options),
        tarball,
        links,
    })
}

/// Check that no file would be created for more than one job.
pub fn check_collisions(jobs: &[Job]) -> Result<(), Error> {
    let mut seen = std::collections::HashSet::new();
    for job in jobs {
        let names = std::iter::once(job.link_target()).chain(job.links.iter().map(|s| s.as_str()));
        for name in names {
            if !seen.insert(name) {
                return Err(Error::NameCollision(name.to_string()));
            }
        }
    }
    Ok(())
}

/// Format a command line for display.
pub fn format_command(command: &[String]) -> String {
    shlex::try_join(command.iter().map(|s| s.as_str())).unwrap_or_else(|_| command.join(" "))
}

/// Run a command line built by `mmdebstrap_command`.
pub fn run_command(command: &[String]) -> Result<(), Error> {
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .status()
        .map_err(|e| Error::BuildFailed(format!("unable to run {}: {}", command[0], e)))?;
    if !status.success() {
        return Err(Error::BuildFailed(format!(
            "{} failed: {}",
            command[0], status
        )));
    }
    Ok(())
}

/// Create a scratch directory that sbuild does not take for a chroot.
fn scratch_directory(directory: &Path) -> Result<tempfile::TempDir, Error> {
    use std::os::unix::fs::PermissionsExt;
    let io_error = |e| Error::Io(directory.to_path_buf(), e);
    let scratch = tempfile::Builder::new()
        .prefix(".tmp")
        .tempdir_in(directory)
        .map_err(io_error)?;
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o755))
        .map_err(io_error)?;
    Ok(scratch)
}

fn check_replaceable(path: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(Error::LinkBlocked(path.to_path_buf())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(path.to_path_buf(), e)),
    }
}

fn replace_link(directory: &Path, name: &str, target: &str) -> Result<(), Error> {
    let path = directory.join(name);
    check_replaceable(&path)?;
    // Create the link elsewhere and rename it, so that it is replaced atomically
    let scratch = scratch_directory(directory)?;
    let temporary = scratch.path().join(name);
    std::os::unix::fs::symlink(target, &temporary).map_err(|e| Error::Io(temporary.clone(), e))?;
    std::fs::rename(&temporary, &path).map_err(|e| Error::Io(path, e))
}

/// Build the tarball if needed and (re)create the links; returns whether it was built.
pub fn create(
    job: &Job,
    force: bool,
    builder: &mut dyn FnMut(&[String]) -> Result<(), Error>,
) -> Result<bool, Error> {
    let directory = job.tarball.parent().unwrap();
    std::fs::create_dir_all(directory).map_err(|e| Error::Io(directory.to_path_buf(), e))?;

    // Fail before a lengthy build if its result can not be put in place
    for name in &job.links {
        check_replaceable(&directory.join(name))?;
    }
    if std::fs::symlink_metadata(&job.tarball)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Err(Error::TarballBlocked(job.tarball.clone()));
    }

    let build = job.needs_build(force);
    if build {
        // Build elsewhere, so that a failure leaves an existing tarball alone
        let scratch = scratch_directory(directory)?;
        let target = scratch.path().join(job.link_target());
        let mut command = job.command.clone();
        command[TARGET_INDEX] = target.to_string_lossy().into_owned();
        log::info!("Running {}", format_command(&command));
        builder(&command)?;
        std::fs::rename(&target, &job.tarball).map_err(|e| Error::Io(job.tarball.clone(), e))?;
    } else {
        log::info!("Keeping existing {}", job.tarball.display());
    }

    for name in &job.links {
        replace_link(directory, name, job.link_target())?;
        log::info!("Linked {} to {}", name, job.link_target());
    }
    Ok(build)
}

/// Directory in which sbuild's unshare mode looks for chroots.
pub fn default_base_directory() -> Result<PathBuf, Error> {
    cache_directory(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

fn cache_directory(
    xdg_cache_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, Error> {
    let cache = match (xdg_cache_home, home) {
        (Some(cache), _) if !cache.is_empty() => PathBuf::from(cache),
        (_, Some(home)) if !home.is_empty() => PathBuf::from(home).join(".cache"),
        _ => return Err(Error::NoCacheDirectory),
    };
    Ok(cache.join("sbuild"))
}

/// Look up a user.
pub fn lookup_user(user: &str) -> Result<nix::unistd::User, Error> {
    match nix::unistd::User::from_name(user) {
        Ok(Some(entry)) => {
            check_home(user, &entry.dir)?;
            Ok(entry)
        }
        _ => Err(Error::UnknownUser(user.to_string())),
    }
}

/// Check that a home directory can be created inside a chroot.
fn check_home(user: &str, home: &Path) -> Result<(), Error> {
    if home.is_absolute() {
        Ok(())
    } else {
        Err(Error::UnusableHome(user.to_string(), home.to_path_buf()))
    }
}

/// Hook that creates a home directory inside the chroot.
pub fn home_directory_hook(uid: u32, gid: u32, home: &Path) -> String {
    format!(
        "install -d --owner={} --group={} \"$1\"{}",
        uid,
        gid,
        shlex::try_quote(&home.to_string_lossy()).unwrap()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const CONFIG: &str = r#"
        distribution {
            name: "unstable"
            archive_mirror_uri: "http://deb.debian.org/debian"
            chroot: "unstable-amd64-sbuild"
            component: "main"
            component: "contrib"
            extra: "experimental"
        }
        distribution {
            name: "bookworm"
            archive_mirror_uri: "http://deb.debian.org/debian"
            chroot: "bookworm-amd64-sbuild"
            component: "main"
        }
        campaign {
            name: "lintian-fixes"
            debian_build { build_distribution: "lintian-fixes" base_distribution: "unstable" }
        }
        campaign {
            name: "fresh-releases"
            debian_build { build_distribution: "fresh-releases" base_distribution: "unstable" }
        }
        campaign {
            name: "backports"
            debian_build { build_distribution: "bookworm-backports" base_distribution: "bookworm" }
        }
        campaign { name: "generic" generic_build { chroot: "unstable" } }
    "#;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn unstable() -> Chroot {
        Chroot::new(
            "unstable",
            "http://deb.debian.org/debian",
            "unstable-amd64-sbuild",
            &strings(&["main", "contrib"]),
            &strings(&["experimental"]),
            &strings(&["lintian-fixes", "fresh-releases"]),
        )
    }

    fn options(base_directory: &Path) -> Options {
        Options {
            base_directory: base_directory.to_path_buf(),
            arch: "amd64".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_chroots_from_config_all() {
        let config = janitor::config::read_string(CONFIG).unwrap();
        let chroots = chroots_from_config(&config, &[]).unwrap();
        assert_eq!(chroots.len(), 2);
        assert_eq!(chroots[0], unstable());
        assert_eq!(chroots[1].suite, "bookworm");
        assert_eq!(
            chroots[1].build_distributions,
            strings(&["bookworm-backports"])
        );
    }

    #[test]
    fn test_chroots_from_config_selected() {
        let config = janitor::config::read_string(CONFIG).unwrap();
        let chroots = chroots_from_config(&config, &strings(&["bookworm"])).unwrap();
        assert_eq!(chroots.len(), 1);
        assert_eq!(chroots[0].chroot, "bookworm-amd64-sbuild");
        assert!(chroots[0].extra.is_empty());
    }

    #[test]
    fn test_chroots_from_config_errors() {
        let config = janitor::config::read_string(
            r#"
            distribution { name: "nomirror" chroot: "nomirror-amd64-sbuild" }
            distribution { name: "nochroot" archive_mirror_uri: "http://example.com/debian" }
            "#,
        )
        .unwrap();
        assert!(matches!(
            chroots_from_config(&config, &strings(&["missing"])),
            Err(Error::NoSuchDistribution(n)) if n == "missing"
        ));
        assert!(matches!(
            chroots_from_config(&config, &strings(&["nomirror"])),
            Err(Error::MissingMirror(n)) if n == "nomirror"
        ));
        assert!(matches!(
            chroots_from_config(&config, &strings(&["nochroot"])),
            Err(Error::MissingChroot(n)) if n == "nochroot"
        ));
    }

    #[test]
    fn test_chroot_new_drops_duplicates() {
        let chroot = Chroot::new("s", "m", "c", &[], &[], &strings(&["a", "b", "a"]));
        assert_eq!(chroot.build_distributions, strings(&["a", "b"]));
    }

    #[test]
    fn test_is_sbuild_chroot_filename() {
        for name in [
            "unstable-amd64.tar.xz",
            "unstable-amd64-sbuild.tar.xz",
            "lintian-fixes-amd64-sbuild.tar.xz",
            "a-b-c.tar",
            "a-b.tgz",
            "debian.testing-amd64.tar.zst",
        ] {
            assert!(is_sbuild_chroot_filename(name), "{}", name);
        }
        for name in [
            "unstable.tar.xz",
            "a-b-c-d.tar.xz",
            "a-b-c-d-sbuild.tar.xz",
            "a--b.tar.xz",
            "-a-b.tar.xz",
            "a-b",
            "a-b.t",
            "a-b.zip",
        ] {
            assert!(!is_sbuild_chroot_filename(name), "{}", name);
        }
    }

    #[test]
    fn test_chroot_name_for_distribution() {
        assert_eq!(
            chroot_name_for_distribution("unstable", "amd64").as_deref(),
            Some("unstable-amd64-sbuild")
        );
        assert_eq!(
            chroot_name_for_distribution("lintian-fixes", "amd64").as_deref(),
            Some("lintian-fixes-amd64-sbuild")
        );
        assert_eq!(
            chroot_name_for_distribution("fresh-upstream-releases", "amd64").as_deref(),
            Some("fresh-upstream-releases-sbuild")
        );
        assert_eq!(chroot_name_for_distribution("a-b-c-d", "amd64"), None);
        assert_eq!(
            chroot_name_for_distribution("debian.testing", "amd64"),
            None
        );
    }

    #[test]
    fn test_mmdebstrap_command() {
        let mut options = options(Path::new("/chroots"));
        options.include = strings(&["eatmydata", "ccache"]);
        options.customize_hooks = strings(&["install -d \"$1\"/home/build"]);
        assert_eq!(
            mmdebstrap_command(
                &unstable(),
                Path::new("/chroots/unstable-amd64-sbuild.tar.xz"),
                &options
            ),
            strings(&[
                "mmdebstrap",
                "--variant=buildd",
                "unstable",
                "/chroots/unstable-amd64-sbuild.tar.xz",
                "http://deb.debian.org/debian",
                "--mode=unshare",
                "--arch=amd64",
                "--components=main,contrib",
                "--include=eatmydata,ccache",
                "--extra-repository=deb http://deb.debian.org/debian experimental main contrib",
                "--customize-hook=install -d \"$1\"/home/build",
            ])
        );
    }

    #[test]
    fn test_mmdebstrap_command_minimal() {
        let chroot = Chroot::new("sid", "http://m/debian", "sid-arm64", &[], &[], &[]);
        let mut options = options(Path::new("/c"));
        options.arch = "arm64".to_string();
        assert_eq!(
            mmdebstrap_command(&chroot, Path::new("/c/sid-arm64.tar.xz"), &options),
            strings(&[
                "mmdebstrap",
                "--variant=buildd",
                "sid",
                "/c/sid-arm64.tar.xz",
                "http://m/debian",
                "--mode=unshare",
                "--arch=arm64",
            ])
        );
    }

    #[test]
    fn test_plan() {
        let job = plan(&unstable(), &options(Path::new("/chroots"))).unwrap();
        assert_eq!(
            job.tarball,
            Path::new("/chroots/unstable-amd64-sbuild.tar.xz")
        );
        assert_eq!(job.link_target(), "unstable-amd64-sbuild.tar.xz");
        assert_eq!(
            job.links,
            strings(&[
                "lintian-fixes-amd64-sbuild.tar.xz",
                "fresh-releases-amd64-sbuild.tar.xz"
            ])
        );
        assert_eq!(
            job.command[TARGET_INDEX],
            "/chroots/unstable-amd64-sbuild.tar.xz"
        );
    }

    #[test]
    fn test_plan_skips_link_to_itself() {
        let mut chroot = unstable();
        chroot.build_distributions = strings(&["unstable"]);
        let job = plan(&chroot, &options(Path::new("/chroots"))).unwrap();
        assert!(job.links.is_empty());
    }

    #[test]
    fn test_plan_unusable_names() {
        let mut chroot = unstable();
        chroot.chroot = "unstable".to_string();
        assert!(matches!(
            plan(&chroot, &options(Path::new("/chroots"))),
            Err(Error::UnusableChrootName(n)) if n == "unstable"
        ));

        let mut chroot = unstable();
        chroot.chroot = "debian.testing-amd64".to_string();
        assert!(matches!(
            plan(&chroot, &options(Path::new("/chroots"))),
            Err(Error::UnusableChrootName(n)) if n == "debian.testing-amd64"
        ));

        let mut chroot = unstable();
        chroot.build_distributions = strings(&["a-b-c-d"]);
        let err = plan(&chroot, &options(Path::new("/chroots"))).unwrap_err();
        assert!(matches!(&err, Error::UnusableBuildDistribution(n) if n == "a-b-c-d"));
        assert!(err.to_string().contains("a-b-c-d"));
    }

    #[test]
    fn test_plan_extra_without_components() {
        let mut chroot = unstable();
        chroot.components = vec![];
        assert!(matches!(
            plan(&chroot, &options(Path::new("/chroots"))),
            Err(Error::ExtraWithoutComponents(n)) if n == "unstable"
        ));
    }

    #[test]
    fn test_check_collisions() {
        let options = options(Path::new("/chroots"));
        let first = plan(&unstable(), &options).unwrap();
        let mut chroot = unstable();
        chroot.chroot = "bookworm-amd64-sbuild".to_string();
        chroot.build_distributions = strings(&["backports"]);
        let second = plan(&chroot, &options).unwrap();
        check_collisions(&[first.clone(), second]).unwrap();

        // The same build distribution on two chroots
        chroot.build_distributions = strings(&["lintian-fixes"]);
        let second = plan(&chroot, &options).unwrap();
        assert!(matches!(
            check_collisions(&[first.clone(), second]),
            Err(Error::NameCollision(n)) if n == "lintian-fixes-amd64-sbuild.tar.xz"
        ));

        // A link with the name of the tarball of another chroot
        chroot.build_distributions = strings(&["unstable"]);
        let second = plan(&chroot, &options).unwrap();
        assert!(matches!(
            check_collisions(&[first, second]),
            Err(Error::NameCollision(n)) if n == "unstable-amd64-sbuild.tar.xz"
        ));
    }

    #[test]
    fn test_run_command_failures() {
        let err = run_command(&strings(&["false"])).unwrap_err();
        assert!(matches!(&err, Error::BuildFailed(m) if m.starts_with("false failed")));
        let err = run_command(&strings(&["/nonexistent/mmdebstrap"])).unwrap_err();
        assert!(matches!(&err, Error::BuildFailed(m) if m.starts_with("unable to run")));
    }

    /// Builder that records its calls and writes a marker to the tarball.
    fn run_create(job: &Job, force: bool) -> (Result<bool, Error>, usize) {
        let calls = RefCell::new(0);
        let result = create(job, force, &mut |command| {
            *calls.borrow_mut() += 1;
            std::fs::write(&command[TARGET_INDEX], b"new").unwrap();
            Ok(())
        });
        let calls = *calls.borrow();
        (result, calls)
    }

    #[test]
    fn test_create_builds_and_links() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().join("sbuild");
        let job = plan(&unstable(), &options(&base)).unwrap();
        assert!(job.needs_build(false));

        let (result, calls) = run_create(&job, false);
        assert!(result.unwrap());
        assert_eq!(calls, 1);
        assert_eq!(std::fs::read(&job.tarball).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 3);
        for name in &job.links {
            assert_eq!(
                std::fs::read_link(base.join(name)).unwrap(),
                Path::new("unstable-amd64-sbuild.tar.xz")
            );
            assert_eq!(std::fs::read(base.join(name)).unwrap(), b"new");
        }
    }

    #[test]
    fn test_create_keeps_existing_tarball() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        std::fs::write(&job.tarball, b"old").unwrap();
        assert!(!job.needs_build(false));

        let (result, calls) = run_create(&job, false);
        assert!(!result.unwrap());
        assert_eq!(calls, 0);
        assert_eq!(std::fs::read(&job.tarball).unwrap(), b"old");
        assert!(td
            .path()
            .join("lintian-fixes-amd64-sbuild.tar.xz")
            .is_symlink());
    }

    #[test]
    fn test_create_rebuilds_empty_tarball() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        std::fs::write(&job.tarball, b"").unwrap();

        let (result, calls) = run_create(&job, false);
        assert!(result.unwrap());
        assert_eq!(calls, 1);
    }

    #[test]
    fn test_create_force() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        std::fs::write(&job.tarball, b"old").unwrap();

        let (result, calls) = run_create(&job, true);
        assert!(result.unwrap());
        assert_eq!(calls, 1);
        assert_eq!(std::fs::read(&job.tarball).unwrap(), b"new");
    }

    #[test]
    fn test_create_replaces_symlink() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        let link = td.path().join("lintian-fixes-amd64-sbuild.tar.xz");
        std::os::unix::fs::symlink("stale.tar.xz", &link).unwrap();

        let (result, _) = run_create(&job, false);
        result.unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("unstable-amd64-sbuild.tar.xz")
        );
    }

    #[test]
    fn test_create_does_not_replace_file() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        let link = td.path().join("fresh-releases-amd64-sbuild.tar.xz");
        std::fs::write(&link, b"precious").unwrap();

        let (result, calls) = run_create(&job, false);
        assert!(matches!(result, Err(Error::LinkBlocked(p)) if p == link));
        assert_eq!(calls, 0);
        assert_eq!(std::fs::read(&link).unwrap(), b"precious");
    }

    #[test]
    fn test_create_refuses_directory_at_tarball() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        std::fs::create_dir(&job.tarball).unwrap();

        for force in [false, true] {
            let (result, calls) = run_create(&job, force);
            assert!(matches!(result, Err(Error::TarballBlocked(p)) if p == job.tarball));
            assert_eq!(calls, 0);
        }
        assert!(job.tarball.is_dir());
    }

    #[test]
    fn test_create_replaces_symlink_to_directory_at_tarball() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        let elsewhere = td.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &job.tarball).unwrap();

        let (result, calls) = run_create(&job, false);
        assert!(result.unwrap());
        assert_eq!(calls, 1);
        assert!(!job.tarball.is_symlink());
        assert_eq!(std::fs::read(&job.tarball).unwrap(), b"new");
        assert!(elsewhere.is_dir());
    }

    #[test]
    fn test_create_failure_keeps_existing_tarball() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        std::fs::write(&job.tarball, b"old").unwrap();
        let result = create(&job, true, &mut |command| {
            std::fs::write(&command[TARGET_INDEX], b"partial").unwrap();
            Err(Error::BuildFailed("mmdebstrap failed".to_string()))
        });
        assert!(matches!(result, Err(Error::BuildFailed(_))));
        assert_eq!(std::fs::read(&job.tarball).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(td.path()).unwrap().count(), 1);
    }

    #[test]
    fn test_create_leaves_nothing_behind_on_failure() {
        let td = tempfile::tempdir().unwrap();
        let job = plan(&unstable(), &options(td.path())).unwrap();
        let result = create(&job, false, &mut |command| {
            std::fs::write(&command[TARGET_INDEX], b"partial").unwrap();
            Err(Error::BuildFailed("mmdebstrap failed".to_string()))
        });
        assert!(matches!(result, Err(Error::BuildFailed(_))));
        assert!(!job.tarball.exists());
        assert!(std::fs::read_dir(td.path()).unwrap().next().is_none());
    }

    #[test]
    fn test_cache_directory() {
        assert_eq!(
            cache_directory(Some("/xdg".into()), Some("/home/user".into())).unwrap(),
            Path::new("/xdg/sbuild")
        );
        assert_eq!(
            cache_directory(None, Some("/home/user".into())).unwrap(),
            Path::new("/home/user/.cache/sbuild")
        );
        assert_eq!(
            cache_directory(Some("".into()), Some("/home/user".into())).unwrap(),
            Path::new("/home/user/.cache/sbuild")
        );
        assert!(matches!(
            cache_directory(None, None),
            Err(Error::NoCacheDirectory)
        ));
    }

    #[test]
    fn test_lookup_user() {
        assert_eq!(lookup_user("root").unwrap().dir, Path::new("/root"));
        assert!(matches!(
            lookup_user("no-such-user-for-janitor-tests"),
            Err(Error::UnknownUser(n)) if n == "no-such-user-for-janitor-tests"
        ));
        check_home("build", Path::new("/home/build")).unwrap();
        for home in ["", "home/build"] {
            let err = check_home("build", Path::new(home)).unwrap_err();
            assert!(
                matches!(&err, Error::UnusableHome(n, h) if n == "build" && h == Path::new(home))
            );
            assert!(err.to_string().contains("not an absolute path"));
        }
    }

    #[test]
    fn test_home_directory_hook() {
        assert_eq!(
            home_directory_hook(1000, 1000, Path::new("/home/build")),
            "install -d --owner=1000 --group=1000 \"$1\"/home/build"
        );
    }
}
