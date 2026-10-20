use crate::{convert_codemod_script_failed, WorkerFailure};
use janitor::api::worker::GenericBuildConfig;
use ognibuild::analyze::AnalyzedError;
use ognibuild::installer::Error as InstallerError;
use silver_platter::codemod::{
    script_runner as generic_script_runner, CommandResult as GenericCommandResult,
    Error as GenericCodemodError,
};
use silver_platter::CommitPending;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

pub fn generic_make_changes(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &Path,
    argv: &[&str],
    env: &HashMap<String, String>,
    log_directory: &Path,
    resume_metadata: Option<&serde_json::Value>,
) -> std::result::Result<GenericCommandResult, WorkerFailure> {
    if argv.is_empty() {
        return Err(WorkerFailure::new(
            "no-changes",
            "No change build",
            ["codemod"],
        ));
    }

    log::info!("Running {:?}", argv);

    let codemod_log_path = log_directory.join("codemod.log");

    let f = File::create(codemod_log_path).unwrap();

    let committer = env.get("COMMITTER").map(|s| s.to_string());

    match generic_script_runner(
        local_tree,
        argv,
        subpath,
        CommitPending::Auto,
        resume_metadata,
        committer.as_deref(),
        Some(env.clone()),
        f.into(),
    ) {
        Ok(r) => Ok(r),
        Err(GenericCodemodError::ScriptMadeNoChanges) => {
            Err(
                WorkerFailure::new("nothing-to-do", "No changes made", ["codemod"])
                    .with_transient(false),
            )
        }
        Err(GenericCodemodError::ExitCode(i)) => Err(convert_codemod_script_failed(
            i,
            shlex::try_join(argv.to_vec()).unwrap().as_str(),
        )),
        Err(GenericCodemodError::ScriptNotFound) => Err(WorkerFailure::new(
            "codemod-not-found",
            format!(
                "Codemod script {} not found",
                shlex::try_join(argv.to_vec()).unwrap()
            ),
            ["codemod"],
        )),
        Err(GenericCodemodError::Detailed(f)) => Err(crate::detailed_codemod_failure(
            f.result_code,
            f.description,
            f.stage,
            f.details,
        )),
        Err(GenericCodemodError::Io(e)) => Err(WorkerFailure::new(
            "io-error",
            format!("IO error: {}", e),
            ["codemod"],
        )),
        Err(GenericCodemodError::Json(e)) => Err(WorkerFailure::new(
            "result-file-format",
            format!("JSON error: {}", e),
            ["codemod"],
        )),
        Err(GenericCodemodError::Utf8(e)) => Err(WorkerFailure::new(
            "utf8-error",
            format!("UTF8 error: {}", e),
            ["codemod"],
        )),
        Err(GenericCodemodError::Other(e)) => Err(WorkerFailure::new(
            "unknown-error",
            format!("Unknown error: {}", e),
            ["codemod"],
        )),
    }
}

pub fn build_from_config(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &std::path::Path,
    output_directory: &std::path::Path,
    config: &GenericBuildConfig,
    _env: &HashMap<String, String>,
) -> Result<serde_json::Value, WorkerFailure> {
    build(
        local_tree,
        subpath,
        output_directory,
        config.chroot.as_deref(),
        config.dep_server_url.as_ref(),
    )
}

fn build(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &Path,
    output_directory: &Path,
    schroot: Option<&str>,
    dep_server_url: Option<&url::Url>,
) -> Result<serde_json::Value, WorkerFailure> {
    use ognibuild::session::Session;
    #[cfg(target_os = "linux")]
    let mut session: Box<dyn Session> = if let Some(schroot) = schroot {
        log::info!("Using schroot {:?}", schroot);
        Box::new(
            ognibuild::session::schroot::SchrootSession::new(schroot, Some("janitor-worker"))
                .map_err(|e| match e {
                    ognibuild::session::Error::SetupFailure(_n, e) => WorkerFailure::new(
                        "session-setup-failure",
                        format!("Failed to setup session: {}", e),
                        ["build"],
                    ),
                    _e => unreachable!(),
                })?,
        ) as Box<dyn Session>
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new()) as Box<dyn Session>
    };

    #[cfg(not(target_os = "linux"))]
    let mut session: Box<dyn Session> = if schroot.is_some() {
        return Err(WorkerFailure::new(
            "chroot-not-supported",
            "Chroot is not supported",
            ["build"],
        ));
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new())
    };

    let scope = ognibuild::installer::InstallationScope::Global;
    let project = session
        .project_from_vcs(local_tree, None, None)
        .map_err(|e| {
            WorkerFailure::new(
                "session-setup-failure",
                format!("Failed to setup session: {}", e),
                ["build"],
            )
        })?;
    session.chdir(project.internal_path()).unwrap();
    let installer = ognibuild::installer::auto_installer(session.as_ref(), scope, dep_server_url);
    let fixers = [Box::new(ognibuild::fixers::InstallFixer::new(
        installer.as_ref(),
        scope,
    ))
        as Box<dyn ognibuild::fix_build::BuildFixer<InstallerError>>];
    let bss = ognibuild::buildsystem::detect_buildsystems(project.external_path());
    if bss.is_empty() {
        return Err(WorkerFailure::new(
            "no-build-system-detected",
            "No build system detected",
            ["build"],
        ));
    }

    let mut log_manager = ognibuild::logs::DirectoryLogManager::new(
        output_directory.join(ognibuild::debian::build::BUILD_LOG_FILENAME),
        ognibuild::logs::LogMode::Redirect,
    );

    use ognibuild::buildsystem::Error as BsError;

    match ognibuild::actions::build::run_build(
        session.as_ref(),
        bss.iter()
            .map(|b| b.as_ref())
            .collect::<Vec<_>>()
            .as_slice(),
        installer.as_ref(),
        fixers
            .iter()
            .map(|x| x.as_ref())
            .collect::<Vec<_>>()
            .as_slice(),
        &mut log_manager,
    ) {
        Ok(_) => {}
        Err(BsError::Error(AnalyzedError::MissingCommandError { command })) => {
            return Err(WorkerFailure::new(
                "missing-command",
                format!("Missing command: {}", command),
                ["build"],
            ));
        }
        Err(BsError::Error(AnalyzedError::Unidentified {
            retcode,
            lines,
            secondary,
        })) => {
            return Err(WorkerFailure::new(
                "unidentified-error",
                if let Some(secondary) = secondary {
                    format!("Unidentified error: {}", secondary.lines().join("\n"))
                } else {
                    format!("Unidentified error: {}", lines.join("\n"))
                },
                ["build"],
            )
            .with_details(serde_json::json!({
                "retcode": retcode,
            })));
        }
        Err(BsError::Error(AnalyzedError::Detailed { retcode, error })) => {
            return Err(
                WorkerFailure::new(error.kind().to_string(), error.to_string(), ["build"])
                    .with_details(serde_json::json!({
                        "retcode": retcode,
                    })),
            );
        }
        Err(BsError::NoBuildSystemDetected) => {
            return Err(WorkerFailure::new(
                "no-build-system-detected",
                "No build system detected",
                ["build"],
            ));
        }
        Err(BsError::DependencyInstallError(err)) => {
            return Err(WorkerFailure::new(
                "dependency-install-error",
                format!("Dependency install error: {}", err),
                ["build"],
            ));
        }
        Err(BsError::Unimplemented) => {
            return Err(WorkerFailure::new(
                "build-action-unimplemented",
                format!(
                    "The build action is not implemented for the detected buildsystem(s): {:?}",
                    bss.iter().map(|bs| bs.name()).collect::<Vec<_>>()
                ),
                ["build"],
            )
            .with_details(serde_json::json!({
                "detected_buildsystems": bss.iter().map(|bs| bs.name()).collect::<Vec<_>>(),
                "project_path": project.external_path(),
                "suggestion": "This build system may need additional implementation in ognibuild"
            })));
        }
        Err(BsError::Error(AnalyzedError::IoError(e))) | Err(BsError::IoError(e)) => {
            return Err(WorkerFailure::new(
                "io-error",
                format!("IO error: {}", e),
                ["build"],
            ));
        }
        Err(BsError::Other(e)) => {
            return Err(WorkerFailure::new(
                "unknown-error",
                format!("Unknown error: {}", e),
                ["build"],
            ));
        }
    }

    let mut log_manager = ognibuild::logs::DirectoryLogManager::new(
        output_directory.join("test.log"),
        ognibuild::logs::LogMode::Redirect,
    );

    match ognibuild::actions::test::run_test(
        session.as_ref(),
        bss.iter()
            .map(|b| b.as_ref())
            .collect::<Vec<_>>()
            .as_slice(),
        installer.as_ref(),
        fixers
            .iter()
            .map(|x| x.as_ref())
            .collect::<Vec<_>>()
            .as_slice(),
        &mut log_manager,
    ) {
        Ok(_) => {}
        Err(BsError::Error(AnalyzedError::MissingCommandError { command })) => {
            return Err(WorkerFailure::new(
                "missing-command",
                format!("Missing command: {}", command),
                ["test"],
            ));
        }
        Err(BsError::Error(AnalyzedError::Unidentified {
            retcode,
            lines,
            secondary,
        })) => {
            return Err(WorkerFailure::new(
                "unidentified-error",
                if let Some(secondary) = secondary {
                    format!("Unidentified error: {}", secondary.lines().join("\n"))
                } else {
                    format!("Unidentified error: {}", lines.join("\n"))
                },
                ["test"],
            )
            .with_details(serde_json::json!({
                "retcode": retcode,
            })));
        }
        Err(BsError::Error(AnalyzedError::Detailed { retcode, error })) => {
            return Err(
                WorkerFailure::new(error.kind().to_string(), error.to_string(), ["test"])
                    .with_details(serde_json::json!({
                        "retcode": retcode,
                    })),
            );
        }
        Err(BsError::NoBuildSystemDetected) => {
            return Err(WorkerFailure::new(
                "no-build-system-detected",
                "No build system detected",
                ["test"],
            ));
        }
        Err(BsError::DependencyInstallError(err)) => {
            return Err(WorkerFailure::new(
                "dependency-install-error",
                format!("Dependency install error: {}", err),
                ["test"],
            ));
        }
        Err(BsError::Unimplemented) => {
            log::warn!(
                "Test action not implemented for buildsystem(s): {:?}, but build succeeded",
                bss.iter().map(|bs| bs.name()).collect::<Vec<_>>()
            );
            // For tests, we don't fail the entire build if tests aren't implemented
            // This is different from build actions which are essential
        }
        Err(BsError::Error(AnalyzedError::IoError(e))) | Err(BsError::IoError(e)) => {
            return Err(WorkerFailure::new(
                "io-error",
                format!("IO error: {}", e),
                ["test"],
            ));
        }
        Err(BsError::Other(e)) => {
            return Err(WorkerFailure::new(
                "unknown-error",
                format!("Unknown error: {}", e),
                ["test"],
            ));
        }
    }

    Ok(serde_json::json!({}))
}

pub struct GenericTarget {
    env: HashMap<String, String>,
}

impl GenericTarget {
    pub fn new(env: HashMap<String, String>) -> Self {
        Self { env }
    }
}

impl crate::Target for GenericTarget {
    fn name(&self) -> String {
        "generic".to_string()
    }

    fn build(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        output_directory: &std::path::Path,
        config: &crate::BuildConfig,
    ) -> Result<serde_json::Value, WorkerFailure> {
        let config: GenericBuildConfig = crate::parse_build_config(config, "build")?;
        build_from_config(local_tree, subpath, output_directory, &config, &self.env)
    }

    fn validate(
        &self,
        _local_tree: &breezyshim::workingtree::GenericWorkingTree,
        _subpath: &std::path::Path,
        _config: &crate::ValidateConfig,
    ) -> Result<(), WorkerFailure> {
        Ok(())
    }

    fn make_changes(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        argv: &[&str],
        log_directory: &std::path::Path,
        resume_metadata: Option<&serde_json::Value>,
    ) -> Result<Box<dyn silver_platter::CodemodResult>, WorkerFailure> {
        generic_make_changes(
            local_tree,
            subpath,
            argv,
            &self.env,
            log_directory,
            resume_metadata,
        )
        .map(|x| Box::new(x) as Box<dyn silver_platter::CodemodResult>)
    }
}
