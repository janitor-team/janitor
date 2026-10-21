use breezyshim::branch::Branch;
use breezyshim::repository::Repository;
use breezyshim::tree::{Tree, WorkingTree};
use breezyshim::workingtree::PyWorkingTree;
use janitor::api::worker::{DebianBuildConfig, WorkerFailure};
use ognibuild::debian::build::{BuildOnceError, BuildOnceResult};
use ognibuild::debian::context::Phase;
use ognibuild::debian::fix_build::IterateBuildError;
use ognibuild::session::Session;

pub(crate) fn build(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &std::path::Path,
    output_directory: &std::path::Path,
    committer: Option<&str>,
    update_changelog: crate::debian::DebUpdateChangelog,
    config: &DebianBuildConfig,
) -> Result<DebianBuildResult, WorkerFailure> {
    if !local_tree.has_filename(&subpath.join("debian/changelog")) {
        return Err(WorkerFailure::new(
            "missing-changelog",
            "Missing changelog",
            ["pre-check"],
        ));
    }

    #[cfg(target_os = "linux")]
    let session: Box<dyn Session> = if let Some(chroot) = config.chroot.as_ref() {
        Box::new(
            ognibuild::session::schroot::SchrootSession::new(chroot, Some("janitor-worker"))
                .map_err(|e| match e {
                    ognibuild::session::Error::SetupFailure(_n, e) => WorkerFailure::new(
                        "session-setup-failure",
                        format!("Error setting up schroot session: {}", e),
                        Vec::<String>::new(),
                    ),
                    _e => unreachable!(),
                })?,
        ) as Box<dyn Session>
    } else {
        Box::new(ognibuild::session::plain::PlainSession::new()) as Box<dyn Session>
    };

    #[cfg(not(target_os = "linux"))]
    let session: Box<dyn Session> = if config.chroot.is_some() {
        return Err(WorkerFailure::new(
            "unsupported-schroot",
            "Schroot is not supported on this platform",
            ["pre-check"],
        ));
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

        let result: Result<BuildOnceResult, IterateBuildError> = if let Some(suffix) =
            config.build_suffix.as_ref()
        {
            let packaging_context = ognibuild::debian::context::DebianPackagingContext::new(
                Clone::clone(local_tree),
                subpath,
                committer.map(breezyshim::config::parse_username),
                update_changelog == crate::debian::DebUpdateChangelog::Update,
                Some(Box::new(breezyshim::commit::NullCommitReporter::new())),
            );
            let fixers = ognibuild::debian::fixers::default_fixers(&packaging_context, &apt)
                .map_err(|e| WorkerFailure::new("fixers-unavailable", e.to_string(), ["build"]))?;

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

        let build_result = match result {
            Ok(result) => result,
            Err(IterateBuildError::ResetTree(e)) => {
                return Err(WorkerFailure::new(
                    "reset-tree",
                    format!("Error resetting tree: {}", e),
                    ["build"],
                ));
            }
            Err(IterateBuildError::Persistent(phase, e)) => {
                let mut stage = vec!["build".to_string()];
                match phase {
                    Phase::Build => stage.push("build".to_string()),
                    Phase::BuildEnv => stage.push("build-env".to_string()),
                    Phase::AutoPkgTest(name) => {
                        stage.push("autopkgtest".to_string());
                        stage.push(name);
                    }
                    Phase::CreateSession => stage.push("create-session".to_string()),
                    Phase::Unpack => stage.push("unpack".to_string()),
                    Phase::AptGetUpdate => stage.push("apt-get-update".to_string()),
                }
                return Err(WorkerFailure::new(
                    e.kind().to_string(),
                    e.to_string(),
                    stage,
                ));
            }
            Err(IterateBuildError::MissingPhase) => {
                return Err(WorkerFailure::new(
                    "missing-phase",
                    "Missing phase",
                    ["build"],
                ));
            }
            Err(IterateBuildError::FixerLimitReached(n)) => {
                return Err(WorkerFailure::new(
                    "fixer-limit-reached",
                    format!("Fixer limit reached: {}", n),
                    ["build"],
                ));
            }
            Err(IterateBuildError::Other(o)) => {
                return Err(WorkerFailure::new(
                    "other",
                    format!("Other error: {}", o),
                    ["build"],
                ));
            }
            Err(IterateBuildError::Unidentified {
                retcode: _,
                lines,
                secondary: _,
                phase: _,
            }) => {
                return Err(WorkerFailure::new(
                    "unidentified",
                    format!("Unidentified error: {}", lines.join("\n")),
                    ["build"],
                ));
            }
        };
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
        .map_err(|e| {
            WorkerFailure::new(
                "lintian",
                format!("Error running lintian: {}", e),
                ["lintian"],
            )
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
