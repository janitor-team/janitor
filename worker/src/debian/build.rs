use breezyshim::branch::Branch;
use breezyshim::repository::Repository;
use breezyshim::tree::{Tree, WorkingTree};
use breezyshim::workingtree::PyWorkingTree;
use janitor::api::worker::{DebianBuildConfig, WorkerFailure};
use ognibuild::debian::build::{BuildOnceError, BuildOnceResult};
use ognibuild::debian::context::Phase;
use ognibuild::debian::fix_build::IterateBuildError;
use ognibuild::session::Session;

#[derive(Debug)]
pub struct BuildFailure {
    pub code: String,
    pub description: String,
    pub details: Option<serde_json::Value>,
    pub stage: Vec<String>,
    pub transient: Option<bool>,
}

impl From<WorkerFailure> for BuildFailure {
    fn from(f: WorkerFailure) -> Self {
        BuildFailure {
            code: f.code,
            description: f.description,
            details: f.details,
            stage: f.stage,
            transient: f.transient,
        }
    }
}

impl std::fmt::Display for BuildFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.description)
    }
}

impl std::error::Error for BuildFailure {}

fn phase_stage(phase: Phase) -> Vec<String> {
    match phase {
        Phase::Build => vec!["build".to_string()],
        Phase::BuildEnv => vec!["build-env".to_string()],
        Phase::AutoPkgTest(name) => vec!["autopkgtest".to_string(), name],
        Phase::CreateSession => vec!["create-session".to_string()],
        Phase::Unpack => vec!["unpack".to_string()],
        Phase::AptGetUpdate => vec!["apt-get-update".to_string()],
    }
}

/// Stages are relative to the "build" stage that `build_from_config` adds.
fn build_failure(e: IterateBuildError) -> BuildFailure {
    match e {
        IterateBuildError::ResetTree(e) => BuildFailure {
            code: "reset-tree".to_string(),
            description: format!("Error resetting tree: {}", e),
            stage: vec![],
            details: None,
            transient: None,
        },
        IterateBuildError::Persistent(phase, e) => BuildFailure {
            code: e.kind().to_string(),
            description: e.to_string(),
            stage: phase_stage(phase),
            details: None,
            transient: None,
        },
        IterateBuildError::MissingPhase => BuildFailure {
            code: "missing-phase".to_string(),
            description: "Missing phase".to_string(),
            stage: vec![],
            details: None,
            transient: None,
        },
        IterateBuildError::FixerLimitReached(n) => BuildFailure {
            code: "fixer-limit-reached".to_string(),
            description: format!("Fixer limit reached: {}", n),
            stage: vec![],
            details: None,
            transient: None,
        },
        IterateBuildError::Other(o) => BuildFailure {
            code: "other".to_string(),
            description: format!("Other error: {}", o),
            stage: vec![],
            details: None,
            transient: None,
        },
        IterateBuildError::Unidentified {
            retcode: _,
            lines,
            secondary: _,
            phase,
        } => BuildFailure {
            code: "unidentified".to_string(),
            description: format!("Unidentified error: {}", lines.join("\n")),
            stage: phase.map(phase_stage).unwrap_or_default(),
            details: None,
            transient: None,
        },
    }
}

pub(crate) fn build(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &std::path::Path,
    output_directory: &std::path::Path,
    committer: Option<&str>,
    update_changelog: crate::debian::DebUpdateChangelog,
    config: &DebianBuildConfig,
) -> Result<DebianBuildResult, BuildFailure> {
    if !local_tree.has_filename(&subpath.join("debian/changelog")) {
        return Err(BuildFailure {
            code: "missing-changelog".to_string(),
            description: "Missing changelog".to_string(),
            stage: vec!["pre-check".to_string()],
            details: None,
            transient: None,
        });
    }

    #[cfg(target_os = "linux")]
    let session: Box<dyn Session> = if let Some(chroot) = config.chroot.as_ref() {
        Box::new(
            ognibuild::session::schroot::SchrootSession::new(chroot, Some("janitor-worker"))
                .map_err(|e| crate::session_failure(e, &[]))?,
        ) as Box<dyn Session>
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new()) as Box<dyn Session>
    };

    #[cfg(not(target_os = "linux"))]
    let session: Box<dyn Session> = if config.chroot.is_some() {
        return Err(BuildFailure {
            code: "unsupported-schroot".to_string(),
            description: "Schroot is not supported on this platform".to_string(),
            stage: vec!["pre-check".to_string()],
            details: None,
            transient: None,
        });
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new()) as Box<dyn Session>
    };

    let source_date_epoch = local_tree
        .branch()
        .repository()
        .get_revision(&local_tree.branch().last_revision())
        .unwrap()
        .datetime();

    let source_date_epoch = source_date_epoch.to_utc();

    let apt = ognibuild::debian::apt::AptManager::new(session.as_ref(), None);
    if let Some(command) = config.build_command.as_ref() {
        if let Some(last_build_version) = config.last_build_version.as_ref() {
            // Update the changelog entry with the previous build version;
            // This allows us to upload incremented versions for subsequent
            // runs.
            crate::debian::tree_set_changelog_version(local_tree, last_build_version, subpath)
                .unwrap();
        }

        let result: Result<BuildOnceResult, IterateBuildError> =
            if let Some(suffix) = config.build_suffix.as_ref() {
                let packaging_context = ognibuild::debian::context::DebianPackagingContext::new(
                    Clone::clone(local_tree),
                    subpath,
                    committer.map(breezyshim::config::parse_username),
                    update_changelog == crate::debian::DebUpdateChangelog::Update,
                    Some(Box::new(breezyshim::commit::NullCommitReporter::new())),
                );
                let fixers = ognibuild::debian::fixers::default_fixers(&packaging_context, &apt)
                    .map_err(|e| BuildFailure {
                        code: "fixers-unavailable".to_string(),
                        description: e.to_string(),
                        details: None,
                        transient: None,
                        stage: vec![],
                    })?;

                ognibuild::debian::fix_build::build_incrementally(
                    local_tree,
                    Some(&format!("~{}", suffix)),
                    config.build_distribution.as_deref(),
                    output_directory,
                    command,
                    fixers
                        .iter()
                        .map(|f| f.as_ref())
                        .collect::<Vec<_>>()
                        .as_slice(),
                    Some("Build for debian-janitor apt repository."),
                    Some(crate::debian::MAX_BUILD_ITERATIONS),
                    subpath,
                    Some(source_date_epoch),
                    config.apt_repository.as_deref(),
                    config.apt_repository_key.as_deref(),
                    config
                        .extra_repositories
                        .as_ref()
                        .map(|m| m.iter().map(|r| r.as_str()).collect::<Vec<_>>()),
                    update_changelog == crate::debian::DebUpdateChangelog::Leave,
                )
            } else {
                ognibuild::debian::build::build_once(
                    local_tree,
                    config.build_distribution.as_deref(),
                    output_directory,
                    command,
                    subpath,
                    Some(source_date_epoch),
                    config.apt_repository.as_deref(),
                    config.apt_repository_key.as_deref(),
                    config
                        .extra_repositories
                        .as_ref()
                        .map(|m| m.iter().map(|r| r.as_str()).collect::<Vec<_>>())
                        .as_ref(),
                )
                .map_err(|e| match e {
                    BuildOnceError::Detailed {
                        stage: _,
                        phase,
                        retcode: _,
                        command: _,
                        error,
                        description: _,
                    } => IterateBuildError::Persistent(phase.unwrap(), error),
                    BuildOnceError::Unidentified {
                        stage: _,
                        phase,
                        retcode,
                        command: _,
                        description: _,
                    } => IterateBuildError::Unidentified {
                        retcode,
                        lines: Vec::new(),
                        secondary: None,
                        phase,
                    },
                })
            };

        let build_result = result.map_err(build_failure)?;
        log::info!("Built {:?}.", build_result.changes_names);

        let lintian_result = crate::debian::lintian::run_lintian(
            output_directory,
            build_result
                .changes_names
                .iter()
                .map(|s| s.as_path())
                .collect(),
            config.lintian.profile.as_deref(),
            config
                .lintian
                .suppress_tags
                .as_ref()
                .map(|tags| tags.iter().map(|tag| tag.as_str()).collect()),
        )
        .map_err(|e| BuildFailure {
            code: "lintian".to_string(),
            description: format!("Error running lintian: {}", e),
            stage: vec!["lintian".to_string()],
            details: None,
            transient: None,
        })?;
        Ok(DebianBuildResult {
            lintian: lintian_result,
        })
    } else {
        Ok(DebianBuildResult {
            lintian: crate::debian::lintian::LintianResult::default(),
        })
    }
}

#[derive(serde::Serialize)]
pub struct DebianBuildResult {
    lintian: crate::debian::lintian::LintianResult,
}

#[cfg(test)]
mod tests {
    use super::*;
    use buildlog_consultant::problems::debian::UnsatisfiedAptDependencies;

    fn failure(e: IterateBuildError) -> (String, String, Vec<String>) {
        let f = build_failure(e);
        (f.code, f.description, f.stage)
    }

    #[test]
    fn test_build_failure() {
        let missing = || {
            Box::new(UnsatisfiedAptDependencies {
                relations: "no-such-package".to_string(),
            })
        };
        assert_eq!(
            vec![
                failure(IterateBuildError::Persistent(Phase::Build, missing())),
                failure(IterateBuildError::Persistent(
                    Phase::AutoPkgTest("smoke".to_string()),
                    missing()
                )),
                failure(IterateBuildError::MissingPhase),
                failure(IterateBuildError::FixerLimitReached(10)),
                failure(IterateBuildError::Unidentified {
                    retcode: 2,
                    lines: vec!["make: *** [all] Error 1".to_string(), "end".to_string()],
                    secondary: None,
                    phase: Some(Phase::Build),
                }),
                failure(IterateBuildError::Unidentified {
                    retcode: 1,
                    lines: vec![],
                    secondary: None,
                    phase: None,
                }),
            ],
            vec![
                (
                    "unsatisfied-apt-dependencies".to_string(),
                    "unsatisfied apt dependencies: no-such-package".to_string(),
                    vec!["build".to_string()]
                ),
                (
                    "unsatisfied-apt-dependencies".to_string(),
                    "unsatisfied apt dependencies: no-such-package".to_string(),
                    vec!["autopkgtest".to_string(), "smoke".to_string()]
                ),
                (
                    "missing-phase".to_string(),
                    "Missing phase".to_string(),
                    vec![]
                ),
                (
                    "fixer-limit-reached".to_string(),
                    "Fixer limit reached: 10".to_string(),
                    vec![]
                ),
                (
                    "unidentified".to_string(),
                    "Unidentified error: make: *** [all] Error 1\nend".to_string(),
                    vec!["build".to_string()]
                ),
                (
                    "unidentified".to_string(),
                    "Unidentified error: ".to_string(),
                    vec![]
                ),
            ]
        );
    }
}
