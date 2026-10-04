use crate::config::Config;
use breezyshim::RevisionId;
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Run {
    pub id: String,
    pub command: String,
    pub description: Option<String>,
    pub result_code: String,
    pub main_branch_revision: Option<RevisionId>,
    pub revision: Option<RevisionId>,
    pub context: Option<String>,
    pub result: Option<serde_json::Value>,
    pub suite: String,
    pub instigated_context: Option<String>,
    pub vcs_type: String,
    pub branch_url: String,
    pub logfilenames: Option<Vec<String>>,
    // Column in the schema is `worker` (name of the worker); keep the
    // Rust field as `worker_name` for clarity but rename at the sqlx
    // layer so queries can select `run.worker` directly without an
    // `AS worker_name` alias everywhere.
    #[sqlx(rename = "worker")]
    pub worker_name: Option<String>,
    pub result_branches: Option<Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>>,
    pub result_tags: Option<Vec<(String, String)>>,
    pub target_branch_url: Option<String>,
    pub change_set: String,
    pub failure_details: Option<serde_json::Value>,
    pub failure_transient: Option<bool>,
    pub failure_stage: Option<String>,
    pub codebase: String,
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

    /// Look up a result branch by role, returning
    /// `(remote_name, base_revision, revision)`.
    ///
    /// This matches Python's `get_result_branch`, which returns
    /// `entry[1:]` of the `(role, remote_name, base_revision, revision)`
    /// tuple. The last two fields must not be swapped: callers
    /// destructure the result positionally.
    pub fn get_result_branch(
        &self,
        role: &str,
    ) -> Option<(String, Option<RevisionId>, Option<RevisionId>)> {
        self.result_branches.as_ref().and_then(|branches| {
            branches
                .iter()
                .find(|(r, _, _, _)| r == role)
                .map(|(_, n, base_rev, rev)| (n.clone(), base_rev.clone(), rev.clone()))
        })
    }
}

/// Return whether `codebase` shares its VCS URL with another codebase
/// (a "cotenant"). None when we can't tell (URL not found in the
/// codebase table).
pub async fn has_cotenants(
    conn: &PgPool,
    codebase: &str,
    url: &url::Url,
) -> Result<Option<bool>, sqlx::Error> {
    #[derive(Debug, Clone, sqlx::FromRow)]
    struct Codebase {
        pub name: String,
    }
    let url = breezyshim::urlutils::split_segment_parameters(url)
        .0
        .to_string();

    let rows: Vec<Codebase> =
        sqlx::query_as("SELECT name FROM codebase where branch_url = $1 or url = $1")
            .bind(url.trim_end_matches('/'))
            .fetch_all(conn)
            .await?;

    Ok(match rows.len() {
        0 => {
            // Uhm, we actually don't really know
            warn!(
                "Unable to figure out if {} has cotenants on {}",
                codebase, url
            );
            None
        }
        1 => Some(rows[0].name != codebase),
        _ => Some(true),
    })
}

/// All distinct suites that currently have publishable runs for
/// `codebase`, per the `publish_ready` view.
pub async fn iter_publishable_suites(
    conn: &PgPool,
    codebase: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT suite FROM publish_ready WHERE codebase = $1")
            .bind(codebase)
            .fetch_all(conn)
            .await?;

    Ok(rows.into_iter().map(|row| row.0).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_run() -> Run {
        Run {
            id: "run-123".to_string(),
            command: "lintian-brush".to_string(),
            description: Some("Fixed lintian issues".to_string()),
            result_code: "success".to_string(),
            main_branch_revision: Some(RevisionId::from(b"main-rev".to_vec())),
            revision: Some(RevisionId::from(b"new-rev".to_vec())),
            context: Some("context-data".to_string()),
            result: Some(serde_json::json!({"applied": 3})),
            suite: "lintian-fixes".to_string(),
            instigated_context: None,
            vcs_type: "git".to_string(),
            branch_url: "https://salsa.debian.org/foo/bar".to_string(),
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
            result_tags: Some(vec![("v1.0".to_string(), "tag-ref".to_string())]),
            target_branch_url: Some("https://salsa.debian.org/foo/bar".to_string()),
            change_set: "cs-1".to_string(),
            failure_details: None,
            failure_transient: None,
            failure_stage: None,
            codebase: "mycodebase".to_string(),
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
    fn test_run_get_result_branch_found() {
        let run = make_run();
        let (name, base_rev, rev) = run.get_result_branch("main").unwrap();
        assert_eq!(name, "refs/heads/lintian-fixes");
        assert_eq!(base_rev, Some(RevisionId::from(b"base-rev".to_vec())));
        assert_eq!(rev, Some(RevisionId::from(b"tip-rev".to_vec())));
    }

    #[test]
    fn test_run_get_result_branch_not_found() {
        let run = make_run();
        assert!(run.get_result_branch("nonexistent").is_none());
    }

    #[test]
    fn test_run_get_result_branch_no_branches() {
        let mut run = make_run();
        run.result_branches = None;
        assert!(run.get_result_branch("main").is_none());
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

        // Different id = not equal
        run2.id = "run-456".to_string();
        assert_ne!(run1, run2);
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
}
