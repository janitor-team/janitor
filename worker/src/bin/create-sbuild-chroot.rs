use clap::Parser;
use janitor_worker::sbuild_chroot::{self, Chroot, Options};
use std::path::PathBuf;

/// Create chroot tarballs for sbuild's unshare mode.
///
/// The chroots are described either by the distributions and campaigns in a
/// configuration file, or, when --suite is given, by the command line alone.
#[derive(Parser, Debug)]
struct Args {
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

    /// Base directory for chroots [default: $XDG_CACHE_HOME/sbuild or ~/.cache/sbuild]
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

    /// Recreate chroots that already exist
    #[clap(long)]
    force: bool,

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
        )]);
    }
    let config = janitor::config::read_file(&args.config)
        .map_err(|e| format!("unable to read {}: {}", args.config.display(), e))?;
    sbuild_chroot::chroots_from_config(&config, &args.distribution).map_err(|e| e.to_string())
}

fn run(args: &Args) -> Result<(), String> {
    let chroots = chroots(args)?;

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
