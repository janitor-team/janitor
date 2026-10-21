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

fn session_failure(e: ognibuild::session::Error) -> WorkerFailure {
    let (code, msg, details) = crate::session_error_parts(e);
    WorkerFailure {
        code: code.to_string(),
        description: format!("Failed to setup session: {}", msg),
        details,
        stage: vec!["build".to_string()],
        transient: None,
    }
}

pub fn generic_make_changes(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &Path,
    argv: &[&str],
    env: &HashMap<String, String>,
    log_directory: &Path,
    resume_metadata: Option<&serde_json::Value>,
) -> std::result::Result<GenericCommandResult, WorkerFailure> {
    if argv.is_empty() {
        return Err(WorkerFailure {
            code: "no-changes".to_string(),
            description: "No change build".to_string(),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        });
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
        Err(GenericCodemodError::ScriptMadeNoChanges) => Err(WorkerFailure {
            code: "nothing-to-do".to_string(),
            description: "No changes made".to_string(),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: Some(false),
        }),
        Err(GenericCodemodError::ExitCode(i)) => Err(convert_codemod_script_failed(
            i,
            shlex::try_join(argv.to_vec()).unwrap().as_str(),
        )),
        Err(GenericCodemodError::ScriptNotFound) => Err(WorkerFailure {
            code: "codemod-not-found".to_string(),
            description: format!(
                "Codemod script {} not found",
                shlex::try_join(argv.to_vec()).unwrap()
            ),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        }),
        Err(GenericCodemodError::Detailed(f)) => {
            let mut stage = vec!["codemod".to_string()];
            if let Some(extra_stage) = f.stage {
                stage.extend(extra_stage);
            }
            Err(WorkerFailure {
                code: f.result_code,
                description: f
                    .description
                    .unwrap_or_else(|| "Codemod failed".to_string()),
                details: f.details,
                stage,
                transient: None,
            })
        }
        Err(GenericCodemodError::Io(e)) => Err(WorkerFailure {
            code: "io-error".to_string(),
            description: format!("IO error: {}", e),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        }),
        Err(GenericCodemodError::Json(e)) => Err(WorkerFailure {
            code: "result-file-format".to_string(),
            description: format!("JSON error: {}", e),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        }),
        Err(GenericCodemodError::Utf8(e)) => Err(WorkerFailure {
            code: "utf8-error".to_string(),
            description: format!("UTF8 error: {}", e),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        }),
        Err(GenericCodemodError::Other(e)) => Err(WorkerFailure {
            code: "unknown-error".to_string(),
            description: format!("Unknown error: {}", e),
            details: None,
            stage: vec!["codemod".to_string()],
            transient: None,
        }),
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
                .map_err(session_failure)?,
        ) as Box<dyn Session>
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new()) as Box<dyn Session>
    };

    #[cfg(not(target_os = "linux"))]
    let mut session: Box<dyn Session> = if schroot.is_some() {
        return Err(WorkerFailure {
            code: "chroot-not-supported".to_string(),
            description: "Chroot is not supported".to_string(),
            details: None,
            stage: vec!["build".to_string()],
            transient: None,
        });
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new())
    };

    let scope = ognibuild::installer::InstallationScope::Global;
    let project = session
        .project_from_vcs(local_tree, None, None)
        .map_err(session_failure)?;
    session
        .chdir(project.internal_path())
        .map_err(session_failure)?;
    let installer = ognibuild::installer::auto_installer(session.as_ref(), scope, dep_server_url);
    let fixers = [Box::new(ognibuild::fixers::InstallFixer::new(
        installer.as_ref(),
        scope,
    ))
        as Box<dyn ognibuild::fix_build::BuildFixer<InstallerError>>];
    let bss = ognibuild::buildsystem::detect_buildsystems(project.external_path());
    if bss.is_empty() {
        return Err(WorkerFailure {
            code: "no-build-system-detected".to_string(),
            description: "No build system detected".to_string(),
            details: None,
            stage: vec!["build".to_string()],
            transient: None,
        });
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
            return Err(WorkerFailure {
                code: "missing-command".to_string(),
                description: format!("Missing command: {}", command),
                details: None,
                stage: vec!["build".to_string()],
                transient: None,
            });
        }
        Err(BsError::Error(AnalyzedError::Unidentified {
            retcode,
            lines,
            secondary,
        })) => {
            return Err(WorkerFailure {
                code: "unidentified-error".to_string(),
                description: if let Some(secondary) = secondary {
                    format!("Unidentified error: {}", secondary.lines().join("\n"))
                } else {
                    format!("Unidentified error: {}", lines.join("\n"))
                },
                details: Some(serde_json::json!({
                    "retcode": retcode,
                })),
                stage: vec!["build".to_string()],
                transient: None,
            });
        }
        Err(BsError::Error(AnalyzedError::Detailed { retcode, error })) => {
            return Err(WorkerFailure {
                code: error.kind().to_string(),
                description: error.to_string(),
                details: Some(serde_json::json!({
                    "retcode": retcode,
                })),
                stage: vec!["build".to_string()],
                transient: None,
            });
        }
        Err(BsError::NoBuildSystemDetected) => {
            return Err(WorkerFailure {
                code: "no-build-system-detected".to_string(),
                description: "No build system detected".to_string(),
                details: None,
                stage: vec!["build".to_string()],
                transient: None,
            });
        }
        Err(BsError::DependencyInstallError(err)) => {
            return Err(WorkerFailure {
                code: "dependency-install-error".to_string(),
                description: format!("Dependency install error: {}", err),
                details: None,
                stage: vec!["build".to_string()],
                transient: None,
            });
        }
        Err(BsError::Unimplemented) => {
            return Err(WorkerFailure {
                code: "build-action-unimplemented".to_string(),
                description: format!(
                    "The build action is not implemented for the detected buildsystem(s): {:?}",
                    bss.iter().map(|bs| bs.name()).collect::<Vec<_>>()
                ),
                details: Some(serde_json::json!({
                    "detected_buildsystems": bss.iter().map(|bs| bs.name()).collect::<Vec<_>>(),
                    "project_path": project.external_path(),
                    "suggestion": "This build system may need additional implementation in ognibuild"
                })),
                stage: vec!["build".to_string()],
                transient: None,
            });
        }
        Err(BsError::Error(AnalyzedError::IoError(e))) | Err(BsError::IoError(e)) => {
            return Err(WorkerFailure {
                code: "io-error".to_string(),
                description: format!("IO error: {}", e),
                details: None,
                stage: vec!["build".to_string()],
                transient: None,
            });
        }
        Err(BsError::Other(e)) => {
            return Err(WorkerFailure {
                code: "unknown-error".to_string(),
                description: format!("Unknown error: {}", e),
                details: None,
                stage: vec!["build".to_string()],
                transient: None,
            });
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
            return Err(WorkerFailure {
                code: "missing-command".to_string(),
                description: format!("Missing command: {}", command),
                details: None,
                stage: vec!["test".to_string()],
                transient: None,
            });
        }
        Err(BsError::Error(AnalyzedError::Unidentified {
            retcode,
            lines,
            secondary,
        })) => {
            return Err(WorkerFailure {
                code: "unidentified-error".to_string(),
                description: if let Some(secondary) = secondary {
                    format!("Unidentified error: {}", secondary.lines().join("\n"))
                } else {
                    format!("Unidentified error: {}", lines.join("\n"))
                },
                details: Some(serde_json::json!({
                    "retcode": retcode,
                })),
                stage: vec!["test".to_string()],
                transient: None,
            });
        }
        Err(BsError::Error(AnalyzedError::Detailed { retcode, error })) => {
            return Err(WorkerFailure {
                code: error.kind().to_string(),
                description: error.to_string(),
                details: Some(serde_json::json!({
                    "retcode": retcode,
                })),
                stage: vec!["test".to_string()],
                transient: None,
            });
        }
        Err(BsError::NoBuildSystemDetected) => {
            return Err(WorkerFailure {
                code: "no-build-system-detected".to_string(),
                description: "No build system detected".to_string(),
                details: None,
                stage: vec!["test".to_string()],
                transient: None,
            });
        }
        Err(BsError::DependencyInstallError(err)) => {
            return Err(WorkerFailure {
                code: "dependency-install-error".to_string(),
                description: format!("Dependency install error: {}", err),
                details: None,
                stage: vec!["test".to_string()],
                transient: None,
            });
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
            return Err(WorkerFailure {
                code: "io-error".to_string(),
                description: format!("IO error: {}", e),
                details: None,
                stage: vec!["test".to_string()],
                transient: None,
            });
        }
        Err(BsError::Other(e)) => {
            return Err(WorkerFailure {
                code: "unknown-error".to_string(),
                description: format!("Unknown error: {}", e),
                details: None,
                stage: vec!["test".to_string()],
                transient: None,
            });
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
        let config: GenericBuildConfig =
            serde_json::from_value(config.clone()).map_err(|e| WorkerFailure {
                code: "build-config-parse-failure".to_string(),
                description: format!("Failed to parse config: {}", e),
                details: None,
                stage: vec!["build".to_string()],
                transient: Some(false),
            })?;
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
