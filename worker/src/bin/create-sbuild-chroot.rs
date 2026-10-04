use clap::{CommandFactory, Parser};
use janitor_worker::sbuild_chroot::{self, schroot, Chroot, Options};
use std::path::PathBuf;

/// How sbuild is going to use the chroots.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Tarballs made with mmdebstrap, for sbuild's unshare mode
    Unshare,
    /// Chroots made with sbuild-createchroot and registered with schroot (needs root)
    Schroot,
}

/// Create chroots for sbuild: tarballs for its unshare mode, or schroot chroots.
///
/// The chroots are described either by the distributions and campaigns in a
/// configuration file, or, when --suite is given, by the command line alone.
#[derive(Parser, Debug)]
struct Args {
    /// Kind of chroot to create
    #[clap(long, value_enum, default_value = "unshare")]
    mode: Mode,

    /// Path to configuration
    #[clap(long, default_value = "janitor.conf", conflicts_with = "suite")]
    config: PathBuf,

    /// Distributions from the configuration to create chroots for (default: all)
    #[clap(conflicts_with = "suite")]
    distribution: Vec<String>,

    /// Suite to create a chroot for, without reading a configuration
    #[clap(long, requires_all = ["mirror", "chroot"])]
    suite: Option<String>,

    /// Archive mirror URI (with --suite)
    #[clap(long, requires = "suite")]
    mirror: Option<String>,

    /// Name of the chroot (with --suite)
    #[clap(long, requires = "suite")]
    chroot: Option<String>,

    /// Archive component to enable (with --suite)
    #[clap(long, requires = "suite")]
    component: Vec<String>,

    /// Extra suite to enable in the chroot (with --suite)
    #[clap(long, requires = "suite")]
    extra: Vec<String>,

    /// Build distribution that uses the chroot (with --suite)
    #[clap(long, requires = "suite")]
    build_distribution: Vec<String>,

    /// Extra name for the chroot (with --suite and --mode schroot)
    #[clap(long, requires = "suite")]
    alias: Vec<String>,

    /// Base directory for chroots [default with --mode unshare: $XDG_CACHE_HOME/sbuild or
    /// ~/.cache/sbuild; required with --mode schroot]
    #[clap(long)]
    base_directory: Option<PathBuf>,

    /// Architecture of the chroots [default: the build architecture]
    #[clap(long)]
    arch: Option<String>,

    /// Include specified package
    #[clap(long)]
    include: Vec<String>,

    /// User to create home directory for
    #[clap(long)]
    user: Option<String>,

    /// Recreate chroots that already exist (with --mode unshare)
    #[clap(long)]
    force: bool,

    /// Do not install eatmydata and run commands under it (with --mode schroot)
    #[clap(long)]
    no_eatmydata: bool,

    /// Create a tarball in the base directory rather than a directory (with --mode schroot)
    #[clap(long)]
    make_sbuild_tarball: bool,

    /// sbuild chroot mode to set the chroot up for (with --mode schroot) [default: schroot]
    #[clap(long, value_enum)]
    sbuild_chroot_mode: Option<schroot::ChrootMode>,

    /// Remove the existing chroot and its schroot definition first (with --mode schroot)
    #[clap(long)]
    remove_old: bool,

    /// Command to run in the chroot after creating it (with --mode schroot)
    #[clap(long)]
    run_command: Vec<String>,

    /// Print what would be done, without changing anything
    #[clap(long)]
    dry_run: bool,

    #[command(flatten)]
    logging: janitor::logging::LoggingArgs,
}

fn chroots(args: &Args) -> Result<Vec<Chroot>, String> {
    if let Some(suite) = args.suite.as_deref() {
        return Ok(vec![Chroot::new(
            suite,
            args.mirror.as_deref().unwrap(),
            args.chroot.as_deref().unwrap(),
            &args.component,
            &args.extra,
            &args.build_distribution,
        )
        .with_aliases(&args.alias)]);
    }
    let config = janitor::config::read_file(&args.config)
        .map_err(|e| format!("unable to read {}: {}", args.config.display(), e))?;
    sbuild_chroot::chroots_from_config(&config, &args.distribution).map_err(|e| e.to_string())
}

/// Check that the options fit the mode.
fn check_mode(args: &Args) -> Result<(), String> {
    let (mode, given): (&str, &[(&str, bool)]) = match args.mode {
        Mode::Unshare => (
            "schroot",
            &[
                ("--alias", !args.alias.is_empty()),
                ("--no-eatmydata", args.no_eatmydata),
                ("--make-sbuild-tarball", args.make_sbuild_tarball),
                ("--sbuild-chroot-mode", args.sbuild_chroot_mode.is_some()),
                ("--remove-old", args.remove_old),
                ("--run-command", !args.run_command.is_empty()),
            ],
        ),
        Mode::Schroot => ("unshare", &[("--force", args.force)]),
    };
    if let Some((option, _)) = given.iter().find(|(_, given)| *given) {
        return Err(format!("{} can only be used with --mode {}", option, mode));
    }
    if args.mode == Mode::Schroot {
        match &args.base_directory {
            None => return Err("--base-directory is required with --mode schroot".to_string()),
            Some(path) if !path.is_absolute() => {
                return Err(
                    "--base-directory must be an absolute path with --mode schroot".to_string(),
                )
            }
            Some(_) => {}
        }
        // Only that mode registers the chroot with schroot
        if args.sbuild_chroot_mode.unwrap_or_default() != schroot::ChrootMode::Schroot {
            for (option, given) in [
                ("--remove-old", args.remove_old),
                ("--run-command", !args.run_command.is_empty()),
            ] {
                if given {
                    return Err(format!("{} needs --sbuild-chroot-mode schroot", option));
                }
            }
        }
    }
    Ok(())
}

fn run_schroot(args: &Args, chroots: &[Chroot]) -> Result<(), String> {
    if args.user.is_some() {
        log::warn!("--user has no effect with --mode schroot");
    }
    let base_directory = args
        .base_directory
        .clone()
        .ok_or("--base-directory is required with --mode schroot")?;
    // sbuild-createchroot records the resolved path
    let base_directory = match base_directory.canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => base_directory.components().collect(),
        Err(e) => return Err(format!("{}: {}", base_directory.display(), e)),
    };
    let options = schroot::Options {
        base_directory,
        arch: match args.arch.clone() {
            Some(arch) => arch,
            None => janitor_worker::get_build_arch().map_err(|e| e.to_string())?,
        },
        include: args.include.clone(),
        eatmydata: !args.no_eatmydata,
        make_tarball: args.make_sbuild_tarball,
        chroot_mode: args.sbuild_chroot_mode.unwrap_or_default(),
    };
    let config_dir = std::path::Path::new(schroot::DEFAULT_CONFIG_DIR);
    let session_dir = std::path::Path::new(schroot::DEFAULT_SESSION_DIR);

    // Validate everything before anything is removed or created
    let jobs = chroots
        .iter()
        .map(|chroot| schroot::plan(chroot, &options))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    schroot::check_collisions(&jobs).map_err(|e| e.to_string())?;
    let mut removals = Vec::new();
    for job in &jobs {
        removals.push(if args.remove_old {
            let mounts = schroot::mount_points().map_err(|e| e.to_string())?;
            schroot::plan_remove_old(job, config_dir, session_dir, &mounts)
                .map_err(|e| e.to_string())?
        } else {
            vec![]
        });
    }

    for (job, removals) in jobs.iter().zip(&removals) {
        if args.dry_run {
            let quote = |path: &std::path::Path| {
                sbuild_chroot::format_command(&[path.to_string_lossy().into_owned()])
            };
            for removal in removals {
                let rm = if removal.is_directory {
                    "rm -rf"
                } else {
                    "rm -f"
                };
                println!("{} -- {}", rm, quote(&removal.path));
                println!("rm -- {}", quote(&removal.entry));
            }
            if job.make_tarball {
                println!("# the chroot is built in a new directory next to the tarball");
            }
            println!("{}", sbuild_chroot::format_command(&job.command));
            for command in &args.run_command {
                println!(
                    "printf %s {} | {}",
                    sbuild_chroot::format_command(std::slice::from_ref(command)),
                    sbuild_chroot::format_command(&schroot::shell_command(job))
                );
            }
        } else {
            schroot::remove_old(removals, session_dir, &schroot::mount_points)
                .map_err(|e| e.to_string())?;
            schroot::create(
                job,
                &args.run_command,
                &mut sbuild_chroot::run_command_with_input,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    let chroots = chroots(args)?;
    if args.mode == Mode::Schroot {
        return run_schroot(args, &chroots);
    }

    let mut options = Options {
        base_directory: match args.base_directory.clone() {
            Some(path) => path,
            None => sbuild_chroot::default_base_directory().map_err(|e| e.to_string())?,
        },
        arch: match args.arch.clone() {
            Some(arch) => arch,
            None => janitor_worker::get_build_arch().map_err(|e| e.to_string())?,
        },
        include: args.include.clone(),
        customize_hooks: vec![],
    };
    if let Some(user) = args.user.as_deref() {
        let entry = sbuild_chroot::lookup_user(user).map_err(|e| e.to_string())?;
        options
            .customize_hooks
            .push(sbuild_chroot::home_directory_hook(
                entry.uid.as_raw(),
                entry.gid.as_raw(),
                &entry.dir,
            ));
    }

    // Validate everything before the first lengthy build
    let jobs = chroots
        .iter()
        .map(|chroot| sbuild_chroot::plan(chroot, &options))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    sbuild_chroot::check_collisions(&jobs).map_err(|e| e.to_string())?;

    for job in &jobs {
        if args.dry_run {
            if job.needs_build(args.force) {
                println!("{}", sbuild_chroot::format_command(&job.command));
            } else {
                println!("# keeping existing {}", job.tarball.display());
            }
            let directory = job.tarball.parent().unwrap();
            for name in &job.links {
                println!(
                    "ln -sf {} {}",
                    job.link_target(),
                    directory.join(name).display()
                );
            }
        } else {
            sbuild_chroot::create(job, args.force, &mut sbuild_chroot::run_command)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn main() {
    let args = Args::parse();
    if let Err(e) = check_mode(&args) {
        Args::command()
            .bin_name(env!("CARGO_BIN_NAME"))
            .error(clap::error::ErrorKind::ArgumentConflict, e)
            .exit();
    }

    args.logging.init();

    if let Err(e) = run(&args) {
        log::error!("{}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_mode() {
        let args = Args::try_parse_from(["prog", "--config", "j.conf", "unstable"]).unwrap();
        assert_eq!(args.config, PathBuf::from("j.conf"));
        assert_eq!(args.distribution, vec!["unstable"]);
        assert!(args.suite.is_none());
    }

    #[test]
    fn test_suite_mode() {
        let args = Args::try_parse_from([
            "prog",
            "--suite=unstable",
            "--mirror=http://deb.debian.org/debian",
            "--chroot=unstable-amd64-sbuild",
            "--component=main",
            "--build-distribution=lintian-fixes",
            "--build-distribution=fresh-releases",
        ])
        .unwrap();
        assert_eq!(
            chroots(&args).unwrap(),
            vec![Chroot::new(
                "unstable",
                "http://deb.debian.org/debian",
                "unstable-amd64-sbuild",
                &["main".to_string()],
                &[],
                &["lintian-fixes".to_string(), "fresh-releases".to_string()],
            )]
        );
    }

    #[test]
    fn test_conflicting_modes() {
        let suite = ["--suite=sid", "--mirror=http://m", "--chroot=sid-amd64"];
        for extra in [&["--config", "j.conf"][..], &["unstable"][..]] {
            let argv = ["prog"].iter().chain(&suite).chain(extra);
            assert!(Args::try_parse_from(argv).is_err(), "{:?}", extra);
        }
    }

    #[test]
    fn test_default_mode() {
        let args = Args::try_parse_from(["prog", "unstable"]).unwrap();
        assert_eq!(args.mode, Mode::Unshare);
        check_mode(&args).unwrap();
        let args = Args::try_parse_from(["prog", "--force", "--mode=unshare"]).unwrap();
        check_mode(&args).unwrap();
        assert!(Args::try_parse_from(["prog", "--mode=sudo"]).is_err());
    }

    #[test]
    fn test_schroot_mode() {
        let args = Args::try_parse_from([
            "prog",
            "--mode=schroot",
            "--base-directory=/srv/chroots",
            "--remove-old",
            "--no-eatmydata",
            "--make-sbuild-tarball",
            "--sbuild-chroot-mode=schroot",
            "--run-command=apt -y install foo",
            "--run-command=true",
            "--include=ccache",
            "unstable",
        ])
        .unwrap();
        check_mode(&args).unwrap();
        assert_eq!(args.mode, Mode::Schroot);
        assert_eq!(args.sbuild_chroot_mode, Some(schroot::ChrootMode::Schroot));
        assert_eq!(args.run_command, vec!["apt -y install foo", "true"]);
        assert!(args.remove_old && args.no_eatmydata && args.make_sbuild_tarball);
        assert_eq!(args.distribution, vec!["unstable"]);
        assert!(Args::try_parse_from(["prog", "--sbuild-chroot-mode=chroot"]).is_err());
    }

    #[test]
    fn test_schroot_mode_aliases() {
        let args = Args::try_parse_from([
            "prog",
            "--mode=schroot",
            "--base-directory=/srv/chroots",
            "--suite=unstable",
            "--mirror=http://m",
            "--chroot=unstable-amd64-sbuild",
            "--alias=sid",
            "--alias=UNRELEASED",
        ])
        .unwrap();
        check_mode(&args).unwrap();
        assert_eq!(
            chroots(&args).unwrap()[0].aliases,
            vec!["sid", "UNRELEASED"]
        );
        // Aliases come from the configuration otherwise
        assert!(Args::try_parse_from(["prog", "--mode=schroot", "--alias=sid"]).is_err());
    }

    #[test]
    fn test_schroot_mode_needs_base_directory() {
        let args = Args::try_parse_from(["prog", "--mode=schroot", "unstable"]).unwrap();
        let err = check_mode(&args).unwrap_err();
        assert!(err.contains("--base-directory is required"), "{}", err);

        let argv = ["prog", "--mode=schroot", "--base-directory=chroots"];
        let err = check_mode(&Args::try_parse_from(argv).unwrap()).unwrap_err();
        assert!(err.contains("must be an absolute path"), "{}", err);
    }

    #[test]
    fn test_schroot_mode_other_sbuild_chroot_modes() {
        let parse = |extra: &[&str]| {
            let base = ["prog", "--mode=schroot", "--base-directory=/c"];
            Args::try_parse_from(base.iter().chain(extra)).unwrap()
        };
        for mode in ["--sbuild-chroot-mode=sudo", "--sbuild-chroot-mode=unshare"] {
            check_mode(&parse(&[mode])).unwrap();
            for option in ["--remove-old", "--run-command=true"] {
                let err = check_mode(&parse(&[mode, option])).unwrap_err();
                assert!(
                    err.contains("needs --sbuild-chroot-mode schroot"),
                    "{}",
                    err
                );
            }
        }
    }

    #[test]
    fn test_options_of_the_other_mode() {
        for option in [
            "--no-eatmydata",
            "--make-sbuild-tarball",
            "--sbuild-chroot-mode=schroot",
            "--remove-old",
            "--run-command=true",
        ] {
            let args = Args::try_parse_from(["prog", option]).unwrap();
            let err = check_mode(&args).unwrap_err();
            assert!(err.contains("--mode schroot"), "{}: {}", option, err);
        }
        let suite = ["--suite=sid", "--mirror=http://m", "--chroot=sid-amd64"];
        let argv = ["prog", "--alias=unstable"].iter().chain(&suite);
        assert!(check_mode(&Args::try_parse_from(argv).unwrap()).is_err());

        let argv = ["prog", "--mode=schroot", "--base-directory=/c", "--force"];
        let err = check_mode(&Args::try_parse_from(argv).unwrap()).unwrap_err();
        assert!(err.contains("--mode unshare"), "{}", err);
    }

    #[test]
    fn test_incomplete_suite_mode() {
        for argv in [
            &["prog", "--suite=sid", "--mirror=http://m"][..],
            &["prog", "--suite=sid", "--chroot=sid-amd64"][..],
            &["prog", "--mirror=http://m", "--chroot=sid-amd64"][..],
            &["prog", "--component=main"][..],
            &["prog", "--build-distribution=lintian-fixes"][..],
        ] {
            assert!(Args::try_parse_from(argv).is_err(), "{:?}", argv);
        }
    }
}
