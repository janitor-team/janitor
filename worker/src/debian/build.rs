use breezyshim::branch::Branch;
use breezyshim::repository::Repository;
use breezyshim::tree::{Tree, WorkingTree};
use breezyshim::workingtree::PyWorkingTree;
use janitor::api::worker::DebianBuildConfig;
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
}

impl std::fmt::Display for BuildFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.description)
    }
}

impl std::error::Error for BuildFailure {}

fn append_chroot_arg(command: &str, chroot: Option<&str>) -> String {
    match chroot {
        Some(chroot) => format!("{} --chroot={}", command, shlex::try_quote(chroot).unwrap()),
        None => command.to_string(),
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
        });
    }

    #[cfg(target_os = "linux")]
    let session: Box<dyn Session> = if let Some(chroot) = config.chroot.as_ref() {
        Box::new(
            ognibuild::session::schroot::SchrootSession::new(chroot, Some("janitor-worker"))
                .map_err(|e| match e {
                    ognibuild::session::Error::SetupFailure(_n, e) => BuildFailure {
                        code: "session-setup-failure".to_string(),
                        description: format!("Error setting up schroot session: {}", e),
                        details: None,
                        stage: vec![],
                    },
                    _e => unreachable!(),
                })?,
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
        // config.chroot only picks the session above, so fold it into the command too.
        let command = append_chroot_arg(command, config.chroot.as_deref());
        let command = command.as_str();

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
                        stage: vec!["build".to_string()],
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

        let build_result = match result {
            Ok(result) => result,
            Err(IterateBuildError::ResetTree(e)) => {
                return Err(BuildFailure {
                    code: "reset-tree".to_string(),
                    description: format!("Error resetting tree: {}", e),
                    stage: vec!["build".to_string()],
                    details: None,
                });
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
                return Err(BuildFailure {
                    code: e.kind().to_string(),
                    description: e.to_string(),
                    stage,
                    details: None,
                });
            }
            Err(IterateBuildError::MissingPhase) => {
                return Err(BuildFailure {
                    code: "missing-phase".to_string(),
                    description: "Missing phase".to_string(),
                    stage: vec!["build".to_string()],
                    details: None,
                });
            }
            Err(IterateBuildError::FixerLimitReached(n)) => {
                return Err(BuildFailure {
                    code: "fixer-limit-reached".to_string(),
                    description: format!("Fixer limit reached: {}", n),
                    stage: vec!["build".to_string()],
                    details: None,
                });
            }
            Err(IterateBuildError::Other(o)) => {
                return Err(BuildFailure {
                    code: "other".to_string(),
                    description: format!("Other error: {}", o),
                    stage: vec!["build".to_string()],
                    details: None,
                });
            }
            Err(IterateBuildError::Unidentified {
                retcode: _,
                lines,
                secondary: _,
                phase: _,
            }) => {
                return Err(BuildFailure {
                    code: "unidentified".to_string(),
                    description: format!("Unidentified error: {}", lines.join("\n")),
                    stage: vec!["build".to_string()],
                    details: None,
                });
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
        .map_err(|e| BuildFailure {
            code: "lintian".to_string(),
            description: format!("Error running lintian: {}", e),
            stage: vec!["lintian".to_string()],
            details: None,
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

    #[test]
    fn test_append_chroot_arg_some() {
        assert_eq!(
            append_chroot_arg("sbuild", Some("bookworm-amd64-sbuild")),
            "sbuild --chroot=bookworm-amd64-sbuild"
        );
    }

    #[test]
    fn test_append_chroot_arg_none() {
        assert_eq!(append_chroot_arg("sbuild", None), "sbuild");
    }

    #[test]
    fn test_append_chroot_arg_quotes_special_characters() {
        // The result is shell-split again by breezy builddeb --builder=, so quoting must survive.
        assert_eq!(
            append_chroot_arg("sbuild", Some("unstable amd64-sbuild")),
            "sbuild --chroot='unstable amd64-sbuild'"
        );
        assert_eq!(
            append_chroot_arg("sbuild", Some("unstable'amd64-sbuild")),
            "sbuild --chroot=\"unstable'amd64-sbuild\""
        );
    }
}
