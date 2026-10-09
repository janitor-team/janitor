use breezyshim::error::Error as BrzError;
use clap::Parser;
use ognibuild::analyze::AnalyzedError;
use ognibuild::buildsystem::Error;
use ognibuild::dist::dist;
use ognibuild::logs::{DirectoryLogManager, LogManager, LogMode, NoLogManager};
use ognibuild::session::Session;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    #[clap(short, long)]
    /// Write logs to files in specified directory rather than standard out
    log_directory: Option<PathBuf>,

    #[cfg(target_os = "linux")]
    #[clap(short, long, env = "SCHROOT")]
    /// Schroot to use
    schroot: Option<String>,

    #[clap(short, long, default_value = "..")]
    /// Directory to write to
    target_dir: PathBuf,

    #[clap(short, long)]
    /// Path to packaging tree
    packaging: Option<PathBuf>,

    #[clap(short, long, default_value = ".")]
    /// Path to tree to create dist tarball for
    directory: PathBuf,

    #[clap(long)]
    /// Fail if declared dependencies can not be installed
    require_declared: bool,

    #[clap(long)]
    apt_update: bool,

    #[clap(long)]
    apt_dist_upgrade: bool,

    #[command(flatten)]
    logging: janitor::logging::LoggingArgs,
}

fn report_failure(kind: &str, description: &str) -> ! {
    log::error!("{}: {}", kind, description);
    if let Ok(path) = std::env::var("DIST_RESULT") {
        let result = serde_json::json!({
            "result_code": kind,
            "description": description,
        });
        std::fs::write(path, serde_json::to_string(&result).unwrap()).unwrap();
    }
    std::process::exit(1);
}

fn report_session_failure(e: ognibuild::session::Error) -> ! {
    let (code, description, _details) = janitor_worker::session_error_parts(e);
    report_failure(code, &description);
}

fn main() -> Result<(), i32> {
    let args = Args::parse();

    args.logging.init();

    let package = std::env::var("PACKAGE").ok();
    let version = std::env::var("VERSION").ok();

    let subdir = package.unwrap_or_else(|| "package".to_owned());

    #[cfg(target_os = "linux")]
    let mut session: Box<dyn Session> = if let Some(schroot) = args.schroot {
        match ognibuild::session::schroot::SchrootSession::new(&schroot, None) {
            Ok(session) => Box::new(session),
            Err(e) => report_session_failure(e),
        }
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new())
    };

    #[cfg(not(target_os = "linux"))]
    let mut session: Box<dyn Session> = Box::new(ognibuild::session::plain::PlainSession::new());

    #[cfg(feature = "debian")]
    if args.apt_update {
        match ognibuild::debian::apt::run_apt(session.as_ref(), vec!["update"], vec![]) {
            Ok(()) => {}
            Err(ognibuild::debian::apt::Error::Session(e)) => report_session_failure(e),
            Err(e) => panic!("{:?}", e),
        }
    }
    #[cfg(feature = "debian")]
    if args.apt_dist_upgrade {
        match ognibuild::debian::apt::run_apt(session.as_ref(), vec!["dist-upgrade"], vec![]) {
            Ok(()) => {}
            Err(ognibuild::debian::apt::Error::Session(e)) => report_session_failure(e),
            Err(e) => panic!("{:?}", e),
        }
    }

    let project = match breezyshim::workingtree::open(&args.directory) {
        Ok(tree) => session
            .project_from_vcs(&tree, Some(true), Some(&subdir))
            .unwrap_or_else(|e| report_session_failure(e)),
        Err(BrzError::NotBranchError(..)) => session
            .project_from_directory(&args.directory, Some(&subdir))
            .unwrap_or_else(|e| report_session_failure(e)),
        Err(e) => {
            report_failure("vcs-error", &format!("Error opening working tree: {}", e));
        }
    };

    #[cfg(feature = "debian")]
    let (packaging_tree, packaging_debian_path) = if let Some(packaging) = args.packaging {
        let (packaging_tree, packaging_debian_path) =
            breezyshim::workingtree::open_containing(&packaging).unwrap();

        match ognibuild::debian::satisfy_build_deps(
            session.as_ref(),
            &packaging_tree,
            &packaging_debian_path,
        ) {
            Ok(_) => (Some(packaging_tree), Some(packaging_debian_path)),
            Err(ognibuild::debian::apt::Error::Detailed { error, .. }) => {
                let error = error.unwrap();
                log::warn!(
                    "Ignoring error installing declared build dependencies ({}): {:?}",
                    error.kind(),
                    error
                );
                if args.require_declared {
                    report_failure(error.kind().as_ref(), &error.to_string());
                }
                (None, None)
            }
            Err(ognibuild::debian::apt::Error::Unidentified {
                lines,
                secondary,
                args: argv,
                ..
            }) => {
                if let Some(secondary) = secondary {
                    log::warn!(
                        "Ignoring error installing declared build dependencies ({:?}): {}",
                        argv,
                        secondary.line()
                    );
                    if args.require_declared {
                        report_failure("command-failed", &secondary.line());
                    }
                } else if lines.len() == 1 {
                    log::warn!(
                        "Ignoring error installing declared build dependencies ({:?}): {}",
                        argv,
                        lines[0]
                    );
                } else {
                    log::warn!(
                        "Ignoring error installing declared build dependencies ({:?}): {:?}",
                        argv,
                        lines
                    );
                }
                if args.require_declared {
                    report_failure("command-failed", &lines.join("\n"));
                }
                (None, None)
            }
            Err(ognibuild::debian::apt::Error::Session(e)) => report_session_failure(e),
            Err(ognibuild::debian::apt::Error::FileSearch(e)) => {
                report_failure("file-search-error", &format!("{:?}", e));
            }
        }
    } else {
        (None, None)
    };

    let target_dir = args
        .directory
        .join(&args.target_dir)
        .canonicalize()
        .unwrap();

    let mut log_manager: Box<dyn LogManager> = if let Some(log_directory) = args.log_directory {
        Box::new(DirectoryLogManager::new(
            log_directory.join("dist.log"),
            LogMode::Redirect,
        ))
    } else {
        Box::new(NoLogManager)
    };

    match dist(
        session.as_mut(),
        project.external_path(),
        project.internal_path(),
        &target_dir,
        log_manager.as_mut(),
        version.as_deref(),
        !args.logging.debug,
    ) {
        Ok(t) => {
            log::info!("Dist tarball {} created successfully", t.to_str().unwrap());
        }
        Err(Error::Unimplemented) => {
            return Err(2);
        }
        Err(Error::NoBuildSystemDetected) => {
            log::warn!("No build system detected, falling back to simple export.");
            return Err(2);
        }
        Err(Error::Error(AnalyzedError::Unidentified {
            retcode,
            lines,
            secondary,
        })) => {
            if let Some(secondary) = secondary {
                report_failure("command-failed", &secondary.line());
            } else if lines.len() == 1 {
                report_failure("command-failed", &lines[0]);
            } else {
                report_failure("command-failed", &lines.join("\n"));
            }
        }
        Err(Error::Error(AnalyzedError::Detailed { error, .. })) => {
            report_failure(error.kind().as_ref(), &error.to_string());
        }
        Err(Error::Error(AnalyzedError::IoError(e))) => {
            report_failure("io-error", &e.to_string());
        }
        Err(Error::Error(AnalyzedError::MissingCommandError { command })) => {
            report_failure("missing-command", &format!("Missing command: {}", command));
        }
        Err(Error::Other(e)) => {
            report_failure("other", &format!("Unknown error: {}", e));
        }
        Err(Error::DependencyInstallError(e)) => {
            report_failure("dependency-install-error", "Failed to install dependencies");
        }
        Err(Error::IoError(e)) => {
            report_failure("io-error", &e.to_string());
        }
    }

    Ok(())
}
