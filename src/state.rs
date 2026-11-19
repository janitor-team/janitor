use crate::config::Config;
use breezyshim::RevisionId;
use janitor_types::{Campaign, ChangeSetId, CodebaseName, RunId};
use log::warn;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, Postgres};
use sqlx::PgPool;
use sqlx::Pool;

/// Create a connection pool to the database
///
/// # Arguments
/// * `config` - The configuration to use for the database connection
///
/// # Returns
/// A connection pool to the database
pub async fn create_pool(config: &Config) -> Result<Pool<Postgres>, sqlx::Error> {
    let pool_options = PgPoolOptions::new().max_connections(5);
    let pool = if let Some(ref database_url) = config.database_location {
        pool_options.connect(database_url).await?
    } else {
        let options = PgConnectOptions::new();
        pool_options.connect_with(options).await?
    };

    Ok(pool)
}

/// A tag a run created, as stored in the `result_tag` composite type.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "result_tag")]
pub struct ResultTag {
    pub actual_name: String,
    pub revision: RevisionId,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Run {
    pub id: RunId,
    pub command: String,
    pub description: Option<String>,
    pub result_code: String,
    pub main_branch_revision: Option<RevisionId>,
    pub revision: Option<RevisionId>,
    pub context: Option<String>,
    pub result: Option<serde_json::Value>,
    pub campaign: Campaign,
    pub instigated_context: Option<String>,
    pub vcs_type: Option<crate::vcs::VcsType>,
    pub branch_url: Option<String>,
    pub logfilenames: Option<Vec<String>>,
    // Column in the schema is `worker` (name of the worker); keep the
    // Rust field as `worker_name` for clarity but rename at the sqlx
    // layer so queries can select `run.worker` directly without an
    // `AS worker_name` alias everywhere.
    #[sqlx(rename = "worker")]
    pub worker_name: Option<String>,
    pub result_branches: Option<Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>>,
    pub result_tags: Option<Vec<ResultTag>>,
    pub target_branch_url: Option<String>,
    pub change_set: ChangeSetId,
    pub failure_details: Option<serde_json::Value>,
    pub failure_transient: Option<bool>,
    pub failure_stage: Option<String>,
    pub codebase: CodebaseName,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub finish_time: chrono::DateTime<chrono::Utc>,
    pub value: Option<i32>,
}

impl PartialEq for Run {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Run {
    pub fn duration(&self) -> chrono::Duration {
        self.finish_time - self.start_time
    }

    /// Look up a result branch by role. Returns
    /// `(remote_name, base_revision, revision)` to match Python's
    /// `get_result_branch(result_branches, role)` which returns
    /// `entry[1:]` from `(role, remote_name, base_revision, revision)`.
    /// A previous implementation swapped the last two fields,
    /// which silently corrupted any caller that destructured the
    /// returned tuple positionally.
    pub fn get_result_branch(
        &self,
        role: &str,
    ) -> Option<(String, Option<RevisionId>, Option<RevisionId>)> {
        self.result_branches.as_ref().and_then(|branches| {
            branches
                .iter()
                .find(|(r, _, _, _)| r == role)
                .map(|(_, n, br, r)| (n.clone(), br.clone(), r.clone()))
        })
    }
}

/// Return whether `codebase` shares its VCS URL with another codebase
/// (a "cotenant"). None when we can't tell (URL not found in the
/// codebase table).
pub async fn has_cotenants(
    conn: &PgPool,
    codebase: &CodebaseName,
    url: &url::Url,
) -> Result<Option<bool>, sqlx::Error> {
    #[derive(Debug, Clone, sqlx::FromRow)]
    struct Row {
        pub name: String,
    }
    // Join through the codebase table so we always compare canonical URLs
    // from the DB, never the run's raw transport URL (which may differ in
    // scheme, e.g. git+ssh vs https, due to gitconfig insteadOf rewriting).
    let rows: Vec<Row> = sqlx::query_as(
        "WITH target AS (SELECT url, branch_url FROM codebase WHERE name = $1)
         SELECT c.name FROM codebase c, target t
         WHERE c.branch_url = t.branch_url
            OR c.url        = t.url
            OR c.branch_url = t.url
            OR c.url        = t.branch_url",
    )
    .bind(codebase)
    .fetch_all(conn)
    .await?;

    let names: Vec<String> = rows.into_iter().map(|r| r.name).collect();
    let result = classify_cotenants(codebase.as_str(), &names);
    if result.is_none() {
        warn!("Unable to figure out if {} has cotenants", codebase);
    }
    Ok(result)
}

/// All distinct campaigns that currently have publishable runs for
/// `codebase`, per the `publish_ready` view.
pub async fn iter_publishable_campaigns(
    conn: &PgPool,
    codebase: &CodebaseName,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT campaign FROM publish_ready WHERE codebase = $1")
            .bind(codebase)
            .fetch_all(conn)
            .await?;

    Ok(rows.into_iter().map(|row| row.0).collect::<Vec<_>>())
}

/// Get the result branch information for a specific role
///
/// This is a standalone utility function that matches the Python API
///
/// # Arguments
/// * `result_branches` - The result branches array from a Run
/// * `role` - The role to search for
///
/// # Returns
/// Tuple of (name, base_revision, revision) if found
///
/// # Errors
/// Returns an error if the role is not found
pub fn get_result_branch(
    result_branches: &[(String, String, Option<RevisionId>, Option<RevisionId>)],
    role: &str,
) -> Result<(String, Option<RevisionId>, Option<RevisionId>), String> {
    result_branches
        .iter()
        .find(|(r, _, _, _)| r == role)
        .map(|(_, n, br, r)| (n.clone(), br.clone(), r.clone()))
        .ok_or_else(|| format!("Role '{}' not found in result branches", role))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_run() -> Run {
        Run {
            id: RunId::new("run-123").unwrap(),
            command: "lintian-brush".to_string(),
            description: Some("Fixed lintian issues".to_string()),
            result_code: "success".to_string(),
            main_branch_revision: Some(RevisionId::from(b"main-rev".to_vec())),
            revision: Some(RevisionId::from(b"new-rev".to_vec())),
            context: Some("context-data".to_string()),
            result: Some(serde_json::json!({"applied": 3})),
            campaign: Campaign::new("lintian-fixes").unwrap(),
            instigated_context: None,
            vcs_type: Some(crate::vcs::VcsType::Git),
            branch_url: Some("https://salsa.debian.org/foo/bar".to_string()),
            logfilenames: Some(vec!["build.log".to_string(), "worker.log".to_string()]),
            worker_name: Some("worker-1".to_string()),
            result_branches: Some(vec![
                (
                    "main".to_string(),
                    "refs/heads/lintian-fixes".to_string(),
                    Some(RevisionId::from(b"base-rev".to_vec())),
                    Some(RevisionId::from(b"tip-rev".to_vec())),
                ),
                (
                    "debian".to_string(),
                    "refs/heads/debian".to_string(),
                    None,
                    None,
                ),
            ]),
            result_tags: Some(vec![ResultTag {
                actual_name: "v1.0".to_string(),
                revision: RevisionId::from(b"tag-ref".to_vec()),
            }]),
            target_branch_url: Some("https://salsa.debian.org/foo/bar".to_string()),
            change_set: ChangeSetId::new("cs-1").unwrap(),
            failure_details: None,
            failure_transient: None,
            failure_stage: None,
            codebase: CodebaseName::new("mycodebase").unwrap(),
            start_time: chrono::Utc.with_ymd_and_hms(2025, 1, 1, 10, 0, 0).unwrap(),
            finish_time: chrono::Utc.with_ymd_and_hms(2025, 1, 1, 10, 5, 0).unwrap(),
            value: Some(30),
        }
    }

    #[test]
    fn test_run_duration() {
        let run = make_run();
        assert_eq!(run.duration(), chrono::Duration::minutes(5));
    }

    #[test]
    fn test_run_get_result_branch() {
        let run = create_test_run();

        // get_result_branch returns (remote_name, base_revision,
        // revision) — matches Python's `entry[1:]` slice of the
        // `(role, remote_name, base_revision, revision)` tuple.
        let main_result = run.get_result_branch("main");
        assert!(main_result.is_some());
        let (name, base_revision, revision) = main_result.unwrap();
        assert_eq!(name, "master");
        assert_eq!(base_revision, Some(RevisionId::from("base-123".as_bytes())));
        assert_eq!(revision, Some(RevisionId::from("head-456".as_bytes())));

        let feature_result = run.get_result_branch("feature");
        assert!(feature_result.is_some());

        let nonexistent = run.get_result_branch("nonexistent");
        assert!(nonexistent.is_none());
    }

    #[test]
    fn test_run_get_result_branch_not_found() {
        let run = make_run();
        assert!(run.get_result_branch("nonexistent").is_none());
    }

    #[test]
    fn test_standalone_get_result_branch() {
        let result_branches = vec![
            (
                "main".to_string(),
                "master".to_string(),
                Some(RevisionId::from("base".as_bytes())),
                Some(RevisionId::from("head".as_bytes())),
            ),
            (
                "feature".to_string(),
                "feat".to_string(),
                None,
                Some(RevisionId::from("feat-head".as_bytes())),
            ),
        ];

        let main_result = get_result_branch(&result_branches, "main");
        assert!(main_result.is_ok());
        let (name, base_revision, revision) = main_result.unwrap();
        assert_eq!(name, "master");
        assert_eq!(base_revision, Some(RevisionId::from("base".as_bytes())));
        assert_eq!(revision, Some(RevisionId::from("head".as_bytes())));

        let feature_result = get_result_branch(&result_branches, "feature");
        assert!(feature_result.is_ok());
        let (name, base_revision, revision) = feature_result.unwrap();
        assert_eq!(name, "feat");
        assert_eq!(base_revision, None);
        assert_eq!(revision, Some(RevisionId::from("feat-head".as_bytes())));

        let nonexistent = get_result_branch(&result_branches, "nonexistent");
        assert!(nonexistent.is_err());
        assert!(nonexistent.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_create_pool_with_database_url() {
        let mut config = Config::default();
        config.database_location = Some("postgresql://localhost/nonexistent".to_string());

        // This should fail to connect but shouldn't panic
        let result = create_pool(&config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_pool_without_database_url() {
        let config = Config::default();

        // This should try default connection which will likely fail in tests
        let result = create_pool(&config).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_run_get_result_branch_no_revisions() {
        let run = make_run();
        let (name, base_rev, rev) = run.get_result_branch("debian").unwrap();
        assert_eq!(name, "refs/heads/debian");
        assert_eq!(base_rev, None);
        assert_eq!(rev, None);
    }

    #[test]
    fn test_run_equality() {
        let run1 = make_run();
        let mut run2 = make_run();
        // Same id = equal
        assert_eq!(run1, run2);

        let result = run.get_result_branch("main");
        assert!(result.is_none());
    }

    #[test]
    fn test_run_with_empty_result_branches() {
        let mut run = create_test_run();
        run.result_branches = Some(vec![]);

        let result = run.get_result_branch("main");
        assert!(result.is_none());
    }

    #[test]
    fn test_empty_result_branches_standalone() {
        let result_branches = vec![];
        let result = get_result_branch(&result_branches, "main");
        assert!(result.is_err());
    }

    #[test]
    fn test_run_equality_ignores_other_fields() {
        let run1 = make_run();
        let mut run2 = make_run();
        run2.command = "different-command".to_string();
        run2.result_code = "failure".to_string();
        run2.value = Some(999);
        // Still equal because id is the same
        assert_eq!(run1, run2);
    }

    #[test]
    fn test_run_zero_duration() {
        let mut run = make_run();
        run.finish_time = run.start_time;
        assert_eq!(run.duration(), chrono::Duration::zero());
    }

    /// Unit tests for the decision matrix in `has_cotenants`. We
    /// call the real `classify_cotenants` helper the function uses
    /// under the hood so tests and production stay locked in step.
    /// Python semantics:
    ///
    ///   - len > 1           → Some(true)
    ///   - len == 1, matches → Some(false)
    ///   - else              → None
    fn names(ss: &[&str]) -> Vec<String> {
        ss.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn test_cotenants_multiple_rows() {
        assert_eq!(classify_cotenants("a", &names(&["a", "b"])), Some(true));
        assert_eq!(classify_cotenants("a", &names(&["b", "c"])), Some(true));
    }

    #[test]
    fn test_cotenants_single_matching() {
        assert_eq!(classify_cotenants("a", &names(&["a"])), Some(false));
    }

    #[test]
    fn test_cotenants_single_non_matching_is_none() {
        // Regression guard for the bug fixed on this branch: the
        // previous Rust port returned Some(true) here; Python
        // returns None ("unknown").
        assert_eq!(classify_cotenants("a", &names(&["other"])), None);
    }

    #[test]
    fn test_cotenants_empty() {
        assert_eq!(classify_cotenants("a", &names(&[])), None);
    }

}
