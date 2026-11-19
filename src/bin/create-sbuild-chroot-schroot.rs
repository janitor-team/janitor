//! Create sbuild chroots using `sbuild-createchroot`.
//!
//! Port of the former `create-sbuild-chroot-schroot.py` script.

use clap::{Parser, ValueEnum};
use configparser::ini::Ini;
use janitor::config::config::campaign::Build;
use janitor::config::{Config, Distribution};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ChrootMode {
    Schroot,
    Sudo,
    Unshare,
}

impl ChrootMode {
    fn as_str(self) -> &'static str {
        match self {
            ChrootMode::Schroot => "schroot",
            ChrootMode::Sudo => "sudo",
            ChrootMode::Unshare => "unshare",
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Create sbuild chroots for janitor distributions"
)]
struct Args {
    /// Remove existing schroots for these distributions before creating new ones.
    #[arg(long)]
    remove_old: bool,

    /// Extra package(s) to include in the chroot. May be repeated.
    #[arg(long)]
    include: Vec<String>,

    /// Base directory for chroots.
    #[arg(long)]
    base_directory: Option<PathBuf>,

    /// Currently unused — kept for CLI compatibility with the
    /// previous Python script.
    #[arg(long)]
    user: Option<String>,

    /// Create an sbuild tarball instead of a directory chroot.
    #[arg(long)]
    make_sbuild_tarball: bool,

    /// Path to the janitor configuration file.
    #[arg(long, default_value = "janitor.conf")]
    config: PathBuf,

    /// sbuild chroot mode.
    #[arg(long, value_enum, default_value_t = ChrootMode::Schroot)]
    chroot_mode: ChrootMode,

    /// Command(s) to run inside the freshly-built schroot. May be repeated.
    #[arg(long)]
    run_command: Vec<String>,

    /// Distributions to build chroots for. If empty, every
    /// distribution in the config is built.
    distribution: Vec<String>,
}

fn sbuild_schroot_name(suite: &str, arch: &str) -> String {
    format!("{suite}-{arch}-sbuild")
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

/// Build the argv for `sbuild-createchroot` without actually invoking
/// it. Extracted so it can be unit-tested.
#[allow(clippy::too_many_arguments)]
fn sbuild_createchroot_argv(
    distro: &Distribution,
    sbuild_path: &Path,
    suites: &[String],
    arch: &str,
    include: &[String],
    eatmydata: bool,
    make_sbuild_tarball: Option<&Path>,
    extra_aliases: &[String],
    chroot_mode: ChrootMode,
) -> Result<Vec<String>, String> {
    let archive_mirror = distro
        .archive_mirror_uri
        .as_deref()
        .ok_or("distribution missing archive_mirror_uri")?;
    let name = distro.name.as_deref().ok_or("distribution missing name")?;

    let mut argv: Vec<String> = vec![
        name.to_string(),
        sbuild_path.display().to_string(),
        archive_mirror.to_string(),
        format!("--components={}", distro.component.join(",")),
    ];

    let mut effective_include: Vec<&str> = include.iter().map(String::as_str).collect();
    if eatmydata {
        argv.push("--command-prefix=eatmydata".into());
        if !effective_include.contains(&"eatmydata") {
            effective_include.push("eatmydata");
        }
    }
    if !effective_include.is_empty() {
        argv.push(format!("--include={}", effective_include.join(",")));
    }

    let mut aliases: Vec<String> = extra_aliases.to_vec();
    for suite in suites {
        aliases.push(sbuild_schroot_name(suite, arch));
    }
    for alias in &aliases {
        argv.push(format!("--alias={alias}"));
    }
    if let Some(tarball) = make_sbuild_tarball {
        argv.push(format!("--make-sbuild-tarball={}", tarball.display()));
    }
    argv.push(format!("--chroot-mode={}", chroot_mode.as_str()));

    for extra_name in &distro.extra {
        argv.push(format!(
            "--extra-repository=deb {} {} {}",
            archive_mirror,
            extra_name,
            distro.component.join(" ")
        ));
    }

    Ok(argv)
}

fn create_chroot(
    distro: &Distribution,
    sbuild_path: &Path,
    suites: &[String],
    arch: &str,
    include: &[String],
    eatmydata: bool,
    make_sbuild_tarball: Option<&Path>,
    extra_aliases: &[String],
    chroot_mode: ChrootMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let argv = sbuild_createchroot_argv(
        distro,
        sbuild_path,
        suites,
        arch,
        include,
        eatmydata,
        make_sbuild_tarball,
        extra_aliases,
        chroot_mode,
    )?;

    let status = Command::new("sbuild-createchroot").args(&argv).status()?;
    if !status.success() {
        return Err(format!("sbuild-createchroot exited with status {status}").into());
    }
    Ok(())
}

/// Walk /etc/schroot/chroot.d and remove any chroot config that
/// references `distro.chroot`. Mirrors the Python script's
/// --remove-old behavior, including its quirks: it asserts the path
/// hasn't moved before deleting, and removes the on-disk chroot tree.
fn remove_old(
    distro: &Distribution,
    arch: &str,
    sbuild_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let chroot = distro
        .chroot
        .as_deref()
        .ok_or("distribution has no chroot")?;
    let distro_name = distro.name.as_deref().ok_or("distribution has no name")?;

    let chroot_d = Path::new("/etc/schroot/chroot.d");
    if !chroot_d.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(chroot_d)? {
        let entry = entry?;
        let mut ini = Ini::new();
        if ini.load(entry.path()).is_err() {
            continue;
        }
        if !ini.sections().iter().any(|s| s == chroot) {
            continue;
        }
        let alias_section = sbuild_schroot_name(distro_name, arch);
        let old_sbuild_path = ini
            .get(&alias_section, "directory")
            .ok_or_else(|| format!("no directory= in section {alias_section}"))?;
        let old_sbuild_path = PathBuf::from(old_sbuild_path);
        if old_sbuild_path != sbuild_path {
            return Err(format!(
                "sbuild path has changed: {} != {}",
                old_sbuild_path.display(),
                sbuild_path.display()
            )
            .into());
        }
        if old_sbuild_path.is_dir() {
            std::fs::remove_dir_all(&old_sbuild_path)?;
        }
        std::fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn run_in_schroot(alias: &str, command: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("sbuild-shell {alias}"))
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(command.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("command {command} failed to run: {status}").into());
    }
    Ok(())
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

    let arch = sbuild_arch()?;

    for distribution in &distributions {
        let distro = config
            .get_distribution(distribution)
            .ok_or_else(|| format!("no such distribution: {distribution}"))?;

        let chroot_name = distro
            .chroot
            .as_deref()
            .ok_or_else(|| format!("distribution {distribution} has no chroot"))?;

        let base_directory = match args.base_directory.as_ref() {
            Some(d) => d.clone(),
            None => {
                eprintln!("error: --base-directory is required");
                std::process::exit(2);
            }
        };

        let sbuild_path = if args.make_sbuild_tarball {
            tempfile::tempdir()?.keep()
        } else {
            base_directory.join(chroot_name)
        };

        if args.remove_old {
            remove_old(distro, &arch, &sbuild_path)?;
        }

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

        let make_sbuild_tarball: Option<PathBuf> = if args.make_sbuild_tarball {
            Some(base_directory.join(format!("{chroot_name}.tar.xz")))
        } else {
            None
        };

        create_chroot(
            distro,
            &sbuild_path,
            &suites,
            &arch,
            &args.include,
            true,
            make_sbuild_tarball.as_deref(),
            &distro.chroot_alias,
            args.chroot_mode,
        )?;

        if !args.run_command.is_empty() {
            let alias = sbuild_schroot_name(
                distro.name.as_deref().expect("distribution name"),
                &arch,
            );
            for cmd in &args.run_command {
                run_in_schroot(&alias, cmd)?;
            }
        }
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
    fn sbuild_schroot_name_builds_suite_arch_sbuild() {
        assert_eq!(
            sbuild_schroot_name("lintian-fixes", "amd64"),
            "lintian-fixes-amd64-sbuild"
        );
    }

    #[test]
    fn argv_has_positional_header_name_path_mirror_components() {
        // sbuild-createchroot's positional args: DISTRO DIRECTORY MIRROR.
        // Everything after is options.
        let distro = minimal_distro();
        let argv = sbuild_createchroot_argv(
            &distro,
            Path::new("/srv/chroots/unstable-amd64-sbuild"),
            &[],
            "amd64",
            &[],
            false,
            None,
            &[],
            ChrootMode::Schroot,
        )
        .unwrap();
        assert_eq!(argv[0], "unstable");
        assert_eq!(argv[1], "/srv/chroots/unstable-amd64-sbuild");
        assert_eq!(argv[2], "http://deb.debian.org/debian");
        assert_eq!(argv[3], "--components=main");
        // chroot-mode is always emitted.
        assert!(argv.iter().any(|a| a == "--chroot-mode=schroot"));
    }

    #[test]
    fn argv_appends_eatmydata_to_include_list_and_command_prefix() {
        // The Python script force-adds eatmydata to --include when
        // eatmydata=True, plus --command-prefix=eatmydata. When the
        // caller already passed eatmydata explicitly, it must not be
        // duplicated.
        let distro = minimal_distro();
        let argv = sbuild_createchroot_argv(
            &distro,
            Path::new("/srv/x"),
            &[],
            "amd64",
            &["python3".into()],
            true,
            None,
            &[],
            ChrootMode::Schroot,
        )
        .unwrap();
        assert!(argv.iter().any(|a| a == "--command-prefix=eatmydata"));
        assert_eq!(
            argv.iter().find(|a| a.starts_with("--include=")),
            Some(&"--include=python3,eatmydata".to_string())
        );

        let argv = sbuild_createchroot_argv(
            &distro,
            Path::new("/srv/x"),
            &[],
            "amd64",
            &["eatmydata".into(), "python3".into()],
            true,
            None,
            &[],
            ChrootMode::Schroot,
        )
        .unwrap();
        assert_eq!(
            argv.iter().find(|a| a.starts_with("--include=")),
            Some(&"--include=eatmydata,python3".to_string()),
            "eatmydata must not be duplicated when caller passed it already"
        );
    }

    #[test]
    fn argv_emits_alias_for_each_suite_plus_chroot_aliases_from_distro() {
        let mut distro = minimal_distro();
        distro.chroot_alias = vec!["unstable".into()];
        let argv = sbuild_createchroot_argv(
            &distro,
            Path::new("/srv/x"),
            &["lintian-fixes".into(), "fresh-releases".into()],
            "amd64",
            &[],
            false,
            None,
            &distro.chroot_alias,
            ChrootMode::Schroot,
        )
        .unwrap();
        let aliases: Vec<&String> = argv
            .iter()
            .filter(|a| a.starts_with("--alias="))
            .collect();
        assert_eq!(
            aliases,
            vec![
                &"--alias=unstable".to_string(),
                &"--alias=lintian-fixes-amd64-sbuild".to_string(),
                &"--alias=fresh-releases-amd64-sbuild".to_string(),
            ]
        );
    }

    #[test]
    fn argv_emits_extra_repository_for_each_distro_extra() {
        // distro.extra is a list of suite names; each expands to
        // `deb MIRROR NAME COMPONENTS…`. Components are space-joined
        // inside the repo line (distinct from --components= which is
        // comma-joined).
        let mut distro = minimal_distro();
        distro.component = vec!["main".into(), "contrib".into()];
        distro.extra = vec!["experimental".into(), "backports".into()];
        let argv = sbuild_createchroot_argv(
            &distro,
            Path::new("/srv/x"),
            &[],
            "amd64",
            &[],
            false,
            None,
            &[],
            ChrootMode::Schroot,
        )
        .unwrap();
        let repos: Vec<&String> = argv
            .iter()
            .filter(|a| a.starts_with("--extra-repository="))
            .collect();
        assert_eq!(
            repos,
            vec![
                &"--extra-repository=deb http://deb.debian.org/debian experimental main contrib"
                    .to_string(),
                &"--extra-repository=deb http://deb.debian.org/debian backports main contrib"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn argv_emits_make_sbuild_tarball_only_when_requested() {
        let distro = minimal_distro();
        let without = sbuild_createchroot_argv(
            &distro,
            Path::new("/srv/x"),
            &[],
            "amd64",
            &[],
            false,
            None,
            &[],
            ChrootMode::Schroot,
        )
        .unwrap();
        assert!(
            !without
                .iter()
                .any(|a| a.starts_with("--make-sbuild-tarball=")),
            "should not emit --make-sbuild-tarball= when None"
        );

        let with = sbuild_createchroot_argv(
            &distro,
            Path::new("/srv/x"),
            &[],
            "amd64",
            &[],
            false,
            Some(Path::new("/srv/x.tar.xz")),
            &[],
            ChrootMode::Schroot,
        )
        .unwrap();
        assert!(with
            .iter()
            .any(|a| a == "--make-sbuild-tarball=/srv/x.tar.xz"));
    }

    #[test]
    fn argv_rejects_distro_missing_required_fields() {
        let mut d = Distribution::new();
        // No name, no archive_mirror_uri.
        d.component = vec!["main".into()];
        assert!(sbuild_createchroot_argv(
            &d,
            Path::new("/srv/x"),
            &[],
            "amd64",
            &[],
            false,
            None,
            &[],
            ChrootMode::Schroot,
        )
        .is_err());
    }

    #[test]
    fn chroot_mode_as_str_matches_sbuild_values() {
        // These strings go straight into --chroot-mode= and must
        // match what sbuild-createchroot expects.
        assert_eq!(ChrootMode::Schroot.as_str(), "schroot");
        assert_eq!(ChrootMode::Sudo.as_str(), "sudo");
        assert_eq!(ChrootMode::Unshare.as_str(), "unshare");
    }
}
