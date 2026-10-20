pub mod build;
pub mod lintian;

use crate::{convert_codemod_script_failed, WorkerFailure};
use breezyshim::tree::{PyTree, Tree, WorkingTree};
use breezyshim::workingtree::PyWorkingTree;
use janitor::api::worker::DebianBuildConfig;
use silver_platter::debian::codemod::{
    script_runner as debian_script_runner, CommandResult as DebianCommandResult,
    Error as DebianCodemodError,
};
use silver_platter::CommitPending;
use std::collections::HashMap;
use std::fs::File;

use std::path::Path;

pub const MAX_BUILD_ITERATIONS: usize = 50;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DebUpdateChangelog {
    #[default]
    Auto,
    Update,
    Leave,
}

impl std::str::FromStr for DebUpdateChangelog {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(DebUpdateChangelog::Auto),
            "update" => Ok(DebUpdateChangelog::Update),
            "leave" => Ok(DebUpdateChangelog::Leave),
            _ => Err(format!("Invalid value for deb-update-changelog: {}", s)),
        }
    }
}

impl DebUpdateChangelog {
    /// Read `DEB_UPDATE_CHANGELOG`, defaulting to `Auto` when it is
    /// unset or invalid.
    fn from_env(env: &HashMap<String, String>) -> Self {
        match env
            .get("DEB_UPDATE_CHANGELOG")
            .map_or("auto", |x| x.as_str())
            .parse()
        {
            Ok(x) => x,
            Err(e) => {
                log::warn!(
                    "Invalid value for DEB_UPDATE_CHANGELOG: {}, defaulting to auto.",
                    e
                );
                DebUpdateChangelog::Auto
            }
        }
    }
}

impl std::fmt::Display for DebUpdateChangelog {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DebUpdateChangelog::Auto => write!(f, "auto"),
            DebUpdateChangelog::Update => write!(f, "update"),
            DebUpdateChangelog::Leave => write!(f, "leave"),
        }
    }
}

pub fn debian_make_changes(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &Path,
    argv: &[&str],
    env: HashMap<String, String>,
    log_directory: &Path,
    resume_metadata: Option<&serde_json::Value>,
    committer: Option<&str>,
    update_changelog: DebUpdateChangelog,
) -> std::result::Result<DebianCommandResult, WorkerFailure> {
    if argv.is_empty() {
        return Err(WorkerFailure::new(
            "no-changes",
            "No change build",
            ["codemod"],
        ));
    }

    log::info!("Running {:?}", argv);

    let mut dist_command = vec![
        "janitor-dist".to_string(),
        format!("--log-directory={}", log_directory.display()),
    ];
    let mut dist_env = HashMap::new();

    if let Some(chroot) = env.get("CHROOT") {
        dist_env.insert("SCHROOT".to_string(), chroot.to_string());
    }

    let debian_path = subpath.join("debian");

    if local_tree.has_filename(&debian_path) {
        dist_command.push(format!(
            "--packaging={}",
            local_tree.abspath(&debian_path).unwrap().display()
        ));
    }

    // Prevent 404s because files have gone away:
    dist_command.push("--apt-update".to_string());
    dist_command.push("--apt-dist-upgrade".to_string());

    let dist_env = dist_env
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join(" ");
    let dist_command =
        shlex::try_join(dist_command.iter().map(|s| s.as_str()).collect::<Vec<_>>()).unwrap();
    let dist_command = if !dist_env.is_empty() {
        format!("{} {}", dist_env, dist_command)
    } else {
        format!("{}", dist_command)
    };

    let mut extra_env = HashMap::new();
    extra_env.insert("DIST".to_string(), dist_command);

    // Suppress ANSI colour escapes in codemod output; without this the
    // codemod.log winds up full of escape sequences that render as
    // gibberish in the run-detail page. TERM=dumb is a fallback for
    // tools that ignore NO_COLOR.
    extra_env.insert("NO_COLOR".to_string(), "1".to_string());
    extra_env.insert("TERM".to_string(), "dumb".to_string());

    for (k, v) in env {
        extra_env.insert(k, v);
    }

    let codemod_log_path = log_directory.join("codemod.log");

    let f = File::create(codemod_log_path).unwrap();

    match debian_script_runner(
        local_tree,
        argv,
        subpath,
        CommitPending::Auto,
        resume_metadata,
        committer,
        Some(extra_env),
        f.into(),
        match update_changelog {
            DebUpdateChangelog::Auto => None,
            DebUpdateChangelog::Update => Some(true),
            DebUpdateChangelog::Leave => Some(false),
        },
    ) {
        Ok(r) => Ok(r),
        Err(DebianCodemodError::ScriptMadeNoChanges) => {
            Err(
                WorkerFailure::new("nothing-to-do", "No changes made", ["codemod"])
                    .with_transient(false),
            )
        }
        Err(DebianCodemodError::MissingChangelog(p)) => Err(WorkerFailure::new(
            "missing-changelog",
            format!("No changelog present: {}", p.display()),
            ["codemod"],
        )
        .with_transient(false)),
        Err(DebianCodemodError::ExitCode(i)) => Err(convert_codemod_script_failed(
            i,
            shlex::try_join(argv.to_vec()).unwrap().as_str(),
        )),
        Err(DebianCodemodError::ScriptNotFound) => Err(WorkerFailure::new(
            "codemod-not-found",
            format!(
                "Codemod script {} not found",
                shlex::try_join(argv.to_vec()).unwrap()
            ),
            ["codemod"],
        )),
        Err(DebianCodemodError::Detailed(f)) => Err(crate::detailed_codemod_failure(
            f.result_code,
            f.description,
            f.stage,
            f.details,
        )),
        Err(DebianCodemodError::Io(e)) => Err(WorkerFailure::new(
            "io-error",
            format!("IO error: {}", e),
            ["codemod"],
        )),
        Err(DebianCodemodError::Json(e)) => Err(WorkerFailure::new(
            "result-file-format",
            format!("JSON error: {}", e),
            ["codemod"],
        )),
        Err(DebianCodemodError::Utf8(e)) => Err(WorkerFailure::new(
            "utf8-error",
            format!("UTF8 error: {}", e),
            ["codemod"],
        )),
        Err(DebianCodemodError::ChangelogParse(e)) => Err(WorkerFailure::new(
            "changelog-parse-error",
            format!("Changelog parse error: {}", e),
            ["codemod"],
        )),
        Err(DebianCodemodError::Other(e)) => Err(WorkerFailure::new(
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
    config: &serde_json::Value,
    env: &std::collections::HashMap<String, String>,
) -> Result<serde_json::Value, WorkerFailure> {
    let config: DebianBuildConfig = crate::parse_build_config(config, "build")?;
    let committer = env.get("COMMITTER");
    let update_changelog = DebUpdateChangelog::from_env(env);
    crate::debian::build::build(
        local_tree,
        subpath,
        output_directory,
        committer.as_ref().map(|x| x.as_str()),
        update_changelog,
        &config,
    )
    .map_err(|e| WorkerFailure {
        code: e.code,
        description: e.description,
        details: e.details,
        stage: vec!["build".to_string()]
            .into_iter()
            .chain(e.stage)
            .collect(),
        transient: None,
    })
    .map(|x| serde_json::to_value(x).unwrap())
}

pub struct DebianTarget {
    env: HashMap<String, String>,
    committer: Option<String>,
    update_changelog: DebUpdateChangelog,
}

impl DebianTarget {
    pub fn new(env: HashMap<String, String>) -> Self {
        let committer = env.get("COMMITTER").cloned();
        let update_changelog = DebUpdateChangelog::from_env(&env);
        Self {
            env,
            update_changelog,
            committer,
        }
    }
}

impl crate::Target for DebianTarget {
    fn name(&self) -> String {
        "debian".to_string()
    }

    fn build(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        output_directory: &std::path::Path,
        config: &crate::BuildConfig,
    ) -> Result<serde_json::Value, WorkerFailure> {
        build_from_config(local_tree, subpath, output_directory, config, &self.env)
    }

    fn validate(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        config: &crate::ValidateConfig,
    ) -> Result<(), WorkerFailure> {
        validate_from_config(local_tree, subpath, config)
    }

    fn make_changes(
        &self,
        local_tree: &breezyshim::workingtree::GenericWorkingTree,
        subpath: &std::path::Path,
        argv: &[&str],
        log_directory: &std::path::Path,
        resume_metadata: Option<&serde_json::Value>,
    ) -> Result<Box<dyn silver_platter::CodemodResult>, WorkerFailure> {
        debian_make_changes(
            local_tree,
            subpath,
            argv,
            self.env.clone(),
            log_directory,
            resume_metadata,
            self.committer.as_deref(),
            self.update_changelog,
        )
        .map(|x| Box::new(x) as Box<dyn silver_platter::CodemodResult>)
    }
}

/// Describe a failure to open the configured apt repository, separating a
/// missing dependency from a problem with the configured value.
fn apt_repository_failure(apt_repository: &str, e: breezyshim::error::Error) -> WorkerFailure {
    let code = match &e {
        breezyshim::error::Error::DependencyNotPresent(..) => "missing-dependency",
        _ => "apt-repository-setup-failure",
    };
    WorkerFailure::new(
        code.to_string(),
        format!("Failed to set up apt repository: {}", e),
        ["validate"],
    )
    .with_details(serde_json::json!({"apt_repository": apt_repository}))
}

fn up_to_date_check_failure(e: impl std::fmt::Display) -> WorkerFailure {
    WorkerFailure::new(
        "vcs-up-to-date-check-failed",
        format!("Failed to check whether the tree is up to date: {}", e),
        ["validate"],
    )
}

/// Check an up-to-date outcome, returning the warning to log for an outcome
/// the run carries on past.
fn validate_up_to_date_status<E: std::fmt::Display>(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &std::path::Path,
    status: Result<breezyshim::debian::vcs_up_to_date::UpToDateStatus, E>,
) -> Result<Option<String>, WorkerFailure> {
    use breezyshim::debian::vcs_up_to_date::UpToDateStatus;
    match status.map_err(up_to_date_check_failure)? {
        UpToDateStatus::UpToDate => Ok(None), // codespell:ignore
        UpToDateStatus::MissingChangelog => {
            if !local_tree.has_filename(&subpath.join("debian")) {
                Err(WorkerFailure::new(
                    "not-debian-package",
                    "Not a Debian package",
                    ["validate"],
                ))
            } else {
                Err(WorkerFailure::new(
                    "missing-changelog",
                    "Missing changelog",
                    ["validate"],
                ))
            }
        }
        UpToDateStatus::PackageMissingInArchive { package } => Ok(Some(format!(
            "Package {} is not present in archive",
            package
        ))),
        UpToDateStatus::TreeVersionNotInArchive { tree_version, .. } => Ok(Some(format!(
            "Last tree version {} not present in the archive",
            tree_version
        ))),
        UpToDateStatus::NewArchiveVersion {
            archive_version,
            tree_version,
        } => Err(WorkerFailure::new(
            "new-archive-version",
            format!(
                "New archive version {} (last tree version {})",
                archive_version, tree_version
            ),
            ["validate"],
        )),
    }
}

fn validate_apt_repository(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &std::path::Path,
    apt: &impl breezyshim::debian::apt::Apt,
) -> Result<(), WorkerFailure> {
    let status = breezyshim::debian::vcs_up_to_date::check_up_to_date(local_tree, subpath, apt);
    if let Some(warning) = validate_up_to_date_status(local_tree, subpath, status)? {
        log::warn!("{}", warning);
    }
    Ok(())
}

fn validate_from_config(
    local_tree: &breezyshim::workingtree::GenericWorkingTree,
    subpath: &std::path::Path,
    config: &serde_json::Value,
) -> Result<(), WorkerFailure> {
    let config: DebianBuildConfig = crate::parse_build_config(config, "validate")?;
    if let Some(apt_repository) = config.apt_repository.as_ref() {
        let apt = breezyshim::debian::apt::RemoteApt::from_string(
            apt_repository,
            config
                .apt_repository_key
                .as_deref()
                .map(std::path::Path::new),
        )
        .map_err(|e| apt_repository_failure(apt_repository, e))?;
        validate_apt_repository(local_tree, subpath, &apt)?;
    }
    Ok(())
}

fn tree_set_changelog_version(
    tree: &breezyshim::workingtree::GenericWorkingTree,
    build_version: &debversion::Version,
    subpath: &Path,
) -> Result<(), debian_analyzer::editor::EditorError> {
    use debian_analyzer::editor::{Editor, MutableTreeEdit};
    let editor = tree.edit_file::<debian_changelog::ChangeLog>(
        &subpath.join("debian/changelog"),
        true,
        true,
    )?;
    if let Some(mut entry) = editor.iter().next() {
        let version: debversion::Version =
            format!("{}~", entry.version().unwrap()).parse().unwrap();
        if version > *build_version {
            return Ok(());
        }
        entry.set_version(build_version);
        editor.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Target;

    use super::*;
    use breezyshim::controldir::{create_standalone_workingtree, ControlDirFormat};
    use breezyshim::tree::MutableTree;

    #[test]
    fn test_create() {
        let target = DebianTarget::new(maplit::hashmap! {
            "COMMITTER".to_string() => "Joe Example <joe@example.com>".to_string(),
            "DEB_UPDATE_CHANGELOG".to_string() => "auto".to_string(),
        });
        assert_eq!(
            target.committer,
            Some("Joe Example <joe@example.com>".to_string())
        );
        assert_eq!(target.update_changelog, DebUpdateChangelog::Auto);
    }

    #[test]
    fn test_validate_missing_changelog() {
        let td = tempfile::tempdir().unwrap();
        let target = DebianTarget::new(maplit::hashmap! {
            "COMMITTER".to_string() => "Joe Example <joe@example.com>".to_string(),
            "DEB_UPDATE_CHANGELOG".to_string() => "auto".to_string(),
        });

        let tree =
            create_standalone_workingtree(&td.path().join("main"), &ControlDirFormat::default())
                .unwrap();

        tree.mkdir(std::path::Path::new("debian")).unwrap();
        tree.put_file_bytes_non_atomic(
            std::path::Path::new("debian/changelog"),
            br#"foo (0.1) unstable; urgency=low

  * Initial release.

 -- Joe Example <joe@example.com>  Mon, 01 Jan 2001 00:00:00 +0000
"#,
        )
        .unwrap();
        tree.add(&[
            std::path::Path::new("debian"),
            std::path::Path::new("debian/changelog"),
        ])
        .unwrap();
        let output_directory = td.path().join("output");
        std::fs::create_dir(&output_directory).unwrap();
        let result = target
            .make_changes(
                &tree,
                std::path::Path::new(""),
                &["sh", "-c", "touch foo; echo Do a thing"],
                output_directory.as_ref(),
                None,
            )
            .unwrap();
        assert_eq!(result.value(), None);
        assert_eq!(result.description(), Some("Do a thing\n".to_string()));
        assert_eq!(result.context(), serde_json::Value::Null);
        assert_eq!(result.tags(), Vec::new());
    }

    fn validate_target() -> DebianTarget {
        DebianTarget::new(maplit::hashmap! {
            "COMMITTER".to_string() => "Joe Example <joe@example.com>".to_string(),
            "DEB_UPDATE_CHANGELOG".to_string() => "auto".to_string(),
        })
    }

    fn empty_tree(td: &tempfile::TempDir) -> breezyshim::workingtree::GenericWorkingTree {
        create_standalone_workingtree(&td.path().join("main"), &ControlDirFormat::default())
            .unwrap()
    }

    #[test]
    fn test_validate_malformed_config_is_reported() {
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        let failure = validate_target()
            .validate(
                &tree,
                std::path::Path::new(""),
                &serde_json::json!("not a config"),
            )
            .unwrap_err();

        assert_eq!(failure.code, "build-config-parse-failure");
        assert_eq!(failure.stage, vec!["validate".to_string()]);
        assert_eq!(failure.transient, Some(false));
        assert!(
            failure.description.starts_with("Failed to parse config: "),
            "unexpected description: {}",
            failure.description
        );
    }

    #[test]
    fn test_validate_bad_apt_repository_is_reported() {
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        // No mirror is contacted either way. A value without spaces is
        // rejected by the parse, or the brz-debian import fails first.
        let failure = validate_target()
            .validate(
                &tree,
                std::path::Path::new(""),
                &serde_json::json!({
                    "lintian": {},
                    "base-apt-repository": "nonsense",
                }),
            )
            .unwrap_err();

        assert_eq!(failure.stage, vec!["validate".to_string()]);
        assert!(
            failure
                .description
                .starts_with("Failed to set up apt repository"),
            "unexpected description: {}",
            failure.description
        );
        assert_eq!(
            failure.details,
            Some(serde_json::json!({"apt_repository": "nonsense"}))
        );
        assert_eq!(failure.transient, None);
    }

    const APT_LINE: &str = "deb http://deb.example.com/debian sid main";

    #[test]
    fn test_apt_repository_failure_codes() {
        let bad_value = apt_repository_failure(
            APT_LINE,
            breezyshim::error::Error::UnknownFormat("nonsense".to_string()),
        );
        assert_eq!(bad_value.code, "apt-repository-setup-failure");
        assert_eq!(bad_value.stage, vec!["validate".to_string()]);
        assert_eq!(bad_value.transient, None);
        assert_eq!(
            bad_value.details,
            Some(serde_json::json!({"apt_repository": APT_LINE}))
        );
        assert!(
            !bad_value.description.contains("deb.example.com"),
            "configured value leaked into the description: {}",
            bad_value.description
        );

        let missing = apt_repository_failure(
            APT_LINE,
            breezyshim::error::Error::DependencyNotPresent(
                "breezy.plugins.debian".to_string(),
                "Install the brz-debian plugin".to_string(),
            ),
        );
        assert_eq!(missing.code, "missing-dependency");
        assert_eq!(missing.stage, vec!["validate".to_string()]);
        assert_eq!(missing.transient, None);
        assert!(
            missing.description.contains("breezy.plugins.debian"),
            "unexpected description: {}",
            missing.description
        );
    }

    #[test]
    fn test_up_to_date_check_failure() {
        let failure = up_to_date_check_failure("connection reset by peer");
        assert_eq!(failure.code, "vcs-up-to-date-check-failed");
        assert_eq!(failure.stage, vec!["validate".to_string()]);
        assert_eq!(failure.transient, None);
        assert_eq!(failure.details, None);
        assert_eq!(
            failure.description,
            "Failed to check whether the tree is up to date: connection reset by peer"
        );
    }

    #[test]
    fn test_up_to_date_status_check_error_is_reported() {
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        let failure = validate_up_to_date_status(
            &tree,
            std::path::Path::new(""),
            Err::<breezyshim::debian::vcs_up_to_date::UpToDateStatus, _>(
                "connection reset by peer",
            ),
        )
        .unwrap_err();

        assert_eq!(failure.code, "vcs-up-to-date-check-failed");
        assert_eq!(failure.stage, vec!["validate".to_string()]);
        assert_eq!(failure.transient, None);
        assert_eq!(failure.details, None);
        assert_eq!(
            failure.description,
            "Failed to check whether the tree is up to date: connection reset by peer"
        );
    }

    #[test]
    fn test_up_to_date_status_without_debian_dir_is_not_debian_package() {
        use breezyshim::debian::vcs_up_to_date::UpToDateStatus;
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        let failure = validate_up_to_date_status(
            &tree,
            std::path::Path::new(""),
            Ok::<_, &str>(UpToDateStatus::MissingChangelog),
        )
        .unwrap_err();

        assert_eq!(failure.code, "not-debian-package");
        assert_eq!(failure.description, "Not a Debian package");
        assert_eq!(failure.stage, vec!["validate".to_string()]);
        assert_eq!(failure.transient, None);
    }

    #[test]
    fn test_up_to_date_status_with_debian_dir_is_missing_changelog() {
        use breezyshim::debian::vcs_up_to_date::UpToDateStatus;
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);
        tree.mkdir(std::path::Path::new("debian")).unwrap();

        let failure = validate_up_to_date_status(
            &tree,
            std::path::Path::new(""),
            Ok::<_, &str>(UpToDateStatus::MissingChangelog),
        )
        .unwrap_err();

        assert_eq!(failure.code, "missing-changelog");
        assert_eq!(failure.description, "Missing changelog");
        assert_eq!(failure.stage, vec!["validate".to_string()]);
        assert_eq!(failure.transient, None);
    }

    #[test]
    fn test_up_to_date_status_new_archive_version() {
        use breezyshim::debian::vcs_up_to_date::UpToDateStatus;
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        let failure = validate_up_to_date_status(
            &tree,
            std::path::Path::new(""),
            Ok::<_, &str>(UpToDateStatus::NewArchiveVersion {
                archive_version: "1.1-1".parse().unwrap(),
                tree_version: "1.0-1".parse().unwrap(),
            }),
        )
        .unwrap_err();

        assert_eq!(failure.code, "new-archive-version");
        assert_eq!(
            failure.description,
            "New archive version 1.1-1 (last tree version 1.0-1)"
        );
        assert_eq!(failure.stage, vec!["validate".to_string()]);
        assert_eq!(failure.transient, None);
    }

    #[test]
    fn test_up_to_date_status_up_to_date_is_ok() {
        use breezyshim::debian::vcs_up_to_date::UpToDateStatus;
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        assert_eq!(
            validate_up_to_date_status(
                &tree,
                std::path::Path::new(""),
                Ok::<_, &str>(UpToDateStatus::UpToDate) // codespell:ignore
            ),
            Ok(None)
        );
    }

    #[test]
    fn test_up_to_date_status_package_missing_in_archive_is_ok() {
        use breezyshim::debian::vcs_up_to_date::UpToDateStatus;
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        assert_eq!(
            validate_up_to_date_status(
                &tree,
                std::path::Path::new(""),
                Ok::<_, &str>(UpToDateStatus::PackageMissingInArchive {
                    package: "blah".to_string(),
                })
            ),
            Ok(Some("Package blah is not present in archive".to_string()))
        );
    }

    #[test]
    fn test_up_to_date_status_tree_version_not_in_archive_is_ok() {
        use breezyshim::debian::vcs_up_to_date::UpToDateStatus;
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        assert_eq!(
            validate_up_to_date_status(
                &tree,
                std::path::Path::new(""),
                Ok::<_, &str>(UpToDateStatus::TreeVersionNotInArchive {
                    tree_version: "1.0-1".parse().unwrap(),
                    archive_versions: vec!["1.1-1".parse().unwrap()],
                })
            ),
            Ok(Some(
                "Last tree version 1.0-1 not present in the archive".to_string()
            ))
        );
    }

    struct StubApt(pyo3::Py<pyo3::PyAny>);

    impl breezyshim::debian::apt::Apt for StubApt {
        fn as_pyobject(&self) -> &pyo3::Py<pyo3::PyAny> {
            &self.0
        }
    }

    #[test]
    fn test_validate_apt_repository_reports_the_check() {
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);
        let apt = StubApt(pyo3::Python::attach(|py| py.None()));

        // The stub reaches neither the plugin nor an archive, so the check
        // fails on the import without brz-debian and on the tree with it.
        let failure = validate_apt_repository(&tree, std::path::Path::new(""), &apt).unwrap_err();

        assert_eq!(failure.stage, vec!["validate".to_string()]);
    }

    #[test]
    fn test_build_null_config_is_build_config_parse_failure() {
        let td = tempfile::tempdir().unwrap();
        let tree = empty_tree(&td);

        let failure = build_from_config(
            &tree,
            std::path::Path::new(""),
            &td.path().join("out"),
            &serde_json::Value::Null,
            &std::collections::HashMap::new(),
        )
        .unwrap_err();

        assert_eq!(failure.code, "build-config-parse-failure");
        assert_eq!(failure.stage, vec!["build".to_string()]);
        assert_eq!(failure.transient, Some(false));
    }
}
