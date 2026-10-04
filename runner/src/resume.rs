use crate::database::RunnerDatabase;
use breezyshim::branch::{Branch, GenericBranch};
use breezyshim::error::Error as BrzError;
use breezyshim::forge::get_forge;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::error::Error;
use std::fmt;

/// Information about a run that can be resumed from.
///
/// The resume lookup is keyed on `(suite, revision)` -- the revision
/// of the resume branch's tip -- not on a per-run branch name. The
/// `branch_name` field is kept for backwards-compatible
/// serialization, populated from `new_result_branch.remote_name` for
/// the `main` role rather than from a (nonexistent) `run.branch_name`
/// column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeInfo {
    /// ID of the run to resume from.
    pub run_id: String,
    /// Campaign (schema column: `suite`) that produced the run.
    pub campaign: String,
    /// Codebase the run targeted.
    pub codebase: String,
    /// Remote branch name from the `main` role of the resumed run's
    /// result branches, if any. May be empty for legacy runs.
    pub branch_name: String,
    /// Result code of the completed run.
    pub result_code: String,
    /// Revision ID if available.
    pub revision: Option<String>,
    /// The completed run's `result` JSON blob (Python calls this
    /// `codemod_result`). Included in the assign response's `resume`
    /// field so the worker can rehydrate the previous run's output.
    pub result: Option<serde_json::Value>,
    /// Per-role result branches for the completed run.
    /// Each tuple is `(role, remote_name, base_revision, revision)`.
    pub result_branches: Vec<(String, Option<String>, Option<String>, Option<String>)>,
}

/// Errors that can occur during resume operations.
#[derive(Debug)]
pub enum ResumeError {
    /// Database operation failed.
    DatabaseError(sqlx::Error),
    /// No resume information found.
    NoResumeFound,
    /// Invalid resume branch.
    InvalidResumeBranch(String),
    /// VCS operation failed.
    VcsError(String),
}

impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResumeError::DatabaseError(e) => write!(f, "Database error: {}", e),
            ResumeError::NoResumeFound => write!(f, "No suitable resume found"),
            ResumeError::InvalidResumeBranch(msg) => write!(f, "Invalid resume branch: {}", msg),
            ResumeError::VcsError(msg) => write!(f, "VCS error: {}", msg),
        }
    }
}

impl Error for ResumeError {}

impl From<sqlx::Error> for ResumeError {
    fn from(error: sqlx::Error) -> Self {
        ResumeError::DatabaseError(error)
    }
}

/// Service for managing resume logic for interrupted runs.
pub struct ResumeService {
    database: RunnerDatabase,
}

impl ResumeService {
    /// Create a new resume service.
    pub fn new(database: RunnerDatabase) -> Self {
        Self { database }
    }

    /// Check if a resume result exists for the given campaign and
    /// resume-branch revision. The lookup keys on `(suite, revision)`,
    /// where `revision` is the tip revision of the resume branch as
    /// returned by `branch.last_revision()`. Skips runs whose
    /// `publish_status` is `rejected`.
    pub async fn check_resume_result(
        &self,
        campaign: &str,
        resume_revision: &str,
    ) -> Result<Option<ResumeInfo>, ResumeError> {
        // `publish_status` is a Postgres ENUM; cast to text so sqlx
        // decodes it as Option<String>.
        #[derive(sqlx::FromRow)]
        struct ResumeRow {
            id: String,
            campaign: String,
            codebase: String,
            result_code: String,
            revision: Option<String>,
            publish_status: Option<String>,
            result: Option<serde_json::Value>,
            main_remote_name: Option<String>,
        }
        let row: Option<ResumeRow> = sqlx::query_as(
            r#"
            SELECT
                r.id,
                r.suite AS campaign,
                r.codebase,
                r.result_code,
                r.revision,
                r.publish_status::text AS publish_status,
                r.result AS result,
                (
                    SELECT nrb.remote_name
                    FROM new_result_branch nrb
                    WHERE nrb.run_id = r.id AND nrb.role = 'main'
                    LIMIT 1
                ) AS main_remote_name
            FROM run r
            WHERE r.suite = $1
              AND r.revision = $2
              AND r.result_code = 'success'
              AND r.finish_time IS NOT NULL
            ORDER BY r.finish_time DESC
            LIMIT 1
            "#,
        )
        .bind(campaign)
        .bind(resume_revision)
        .fetch_optional(self.database.pool())
        .await?;

        let row = match row {
            Some(row) => row,
            None => return Ok(None),
        };

        if row.publish_status.as_deref() == Some("rejected") {
            log::info!("Unsetting resume branch, since last run was rejected.");
            return Ok(None);
        }

        // Pull every result branch for the run, matching Python's
        // `ResumeInfo.resume_result_branches` shape:
        // `[(role, remote_name, base_revision, revision), ...]`.
        #[derive(sqlx::FromRow)]
        struct BranchRow {
            role: String,
            remote_name: Option<String>,
            base_revision: Option<String>,
            revision: Option<String>,
        }
        let result_branches: Vec<(String, Option<String>, Option<String>, Option<String>)> =
            sqlx::query_as::<_, BranchRow>(
                r#"
                SELECT role, remote_name, base_revision, revision
                FROM new_result_branch
                WHERE run_id = $1
                ORDER BY role
                "#,
            )
            .bind(&row.id)
            .fetch_all(self.database.pool())
            .await?
            .into_iter()
            .map(|b| (b.role, b.remote_name, b.base_revision, b.revision))
            .collect();

        Ok(Some(ResumeInfo {
            run_id: row.id,
            campaign: row.campaign,
            codebase: row.codebase,
            branch_name: row.main_remote_name.unwrap_or_default(),
            result_code: row.result_code,
            revision: row.revision,
            result: row.result,
            result_branches,
        }))
    }

    /// Set resume information for a run
    pub async fn set_resume_from(
        &self,
        run_id: &str,
        resume_from_id: &str,
    ) -> Result<(), ResumeError> {
        sqlx::query(
            r#"
            UPDATE run 
            SET resume_from = $1 
            WHERE id = $2
            "#,
        )
        .bind(resume_from_id)
        .bind(run_id)
        .execute(self.database.pool())
        .await?;

        Ok(())
    }

    /// Check if a run can be resumed from another run
    pub async fn can_resume_from(
        &self,
        run_id: &str,
        potential_resume_id: &str,
    ) -> Result<bool, ResumeError> {
        let row = sqlx::query(
            r#"
            SELECT
                r1.suite as current_campaign,
                r1.codebase as current_codebase,
                r2.suite as resume_campaign,
                r2.codebase as resume_codebase,
                r2.result_code as resume_result
            FROM run r1, run r2
            WHERE r1.id = $1 AND r2.id = $2
            "#,
        )
        .bind(run_id)
        .bind(potential_resume_id)
        .fetch_optional(self.database.pool())
        .await?;

        if let Some(row) = row {
            let current_campaign: String = row.get("current_campaign");
            let current_codebase: String = row.get("current_codebase");
            let resume_campaign: String = row.get("resume_campaign");
            let resume_codebase: String = row.get("resume_codebase");
            let resume_result: String = row.get("resume_result");

            // Can only resume if campaigns and codebases match and previous run was successful
            Ok(current_campaign == resume_campaign
                && current_codebase == resume_codebase
                && (resume_result == "success" || resume_result == "nothing-new-to-do"))
        } else {
            Ok(false)
        }
    }

    /// Get all runs that resume from a specific run
    pub async fn get_resume_descendants(&self, run_id: &str) -> Result<Vec<String>, ResumeError> {
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE resume_tree AS (
                SELECT id, resume_from, 1 as level
                FROM run
                WHERE resume_from = $1
                
                UNION ALL
                
                SELECT r.id, r.resume_from, rt.level + 1
                FROM run r
                INNER JOIN resume_tree rt ON r.resume_from = rt.id
                WHERE rt.level < 10  -- Prevent infinite recursion
            )
            SELECT id FROM resume_tree ORDER BY level, id
            "#,
        )
        .bind(run_id)
        .fetch_all(self.database.pool())
        .await?;

        Ok(rows.into_iter().map(|row| row.get("id")).collect())
    }

    /// Get the resume chain for a run (all the runs it transitively resumes from)
    pub async fn get_resume_chain(&self, run_id: &str) -> Result<Vec<String>, ResumeError> {
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE resume_chain AS (
                SELECT id, resume_from, 0 as level
                FROM run
                WHERE id = $1
                
                UNION ALL
                
                SELECT r.id, r.resume_from, rc.level + 1
                FROM run r
                INNER JOIN resume_chain rc ON r.id = rc.resume_from
                WHERE rc.level < 10  -- Prevent infinite recursion
            )
            SELECT id FROM resume_chain WHERE id != $1 ORDER BY level DESC
            "#,
        )
        .bind(run_id)
        .fetch_all(self.database.pool())
        .await?;

        Ok(rows.into_iter().map(|row| row.get("id")).collect())
    }

    /// Validate that resume relationships are consistent
    pub async fn validate_resume_consistency(&self) -> Result<Vec<String>, ResumeError> {
        let rows = sqlx::query(
            r#"
            SELECT r1.id, r1.resume_from
            FROM run r1
            LEFT JOIN run r2 ON r1.resume_from = r2.id
            WHERE r1.resume_from IS NOT NULL 
            AND (r2.id IS NULL OR r2.result_code NOT IN ('success', 'nothing-new-to-do'))
            "#,
        )
        .fetch_all(self.database.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let id: String = row.get("id");
                let resume_from: Option<String> = row.get("resume_from");
                format!("Run {} has invalid resume_from: {:?}", id, resume_from)
            })
            .collect())
    }
}

/// Outcome of attempting to locate a resume branch on the forge.
///
/// `RateLimited` surfaces `BranchOpenError::RateLimited` from
/// silver-platter so the runner can call `rate_limit_host` and skip
/// the offending host on subsequent assignments. Everything else
/// collapses to `NotFound` -- resume is best-effort and non-rate-limit
/// failures shouldn't block assigning a fresh run.
pub enum ResumeLookup {
    /// Found a resume branch.
    Found(GenericBranch),
    /// Didn't find one, and the failure wasn't rate-limit-related.
    NotFound,
    /// Forge is rate-limiting us; skip this host until `retry_after`.
    /// `retry_after` is the forge's suggested wait in seconds, or
    /// `None` if the `Retry-After` header was missing/unparsable.
    RateLimited {
        /// Host we should put in the runner's `rate-limit-hosts` set.
        host: String,
        /// Seconds until we should try this host again (forge-supplied).
        retry_after: Option<f64>,
    },
}

/// Locate a previously-proposed resume branch by contacting the
/// forge.
///
/// Tries the forge for each of the candidate derived-branch names
/// (`<campaign>`, `<campaign>/main`, `<campaign>/main/<codebase>`)
/// and returns the first one that exists. Rate-limit errors are
/// surfaced via `ResumeLookup::RateLimited` so the caller can feed
/// the host into the runner's rate-limit hash. Every other failure
/// collapses to `NotFound`: resume is best-effort and failures must
/// not block assigning a fresh run.
///
/// This is a blocking function (it calls into PyO3/breezy via
/// silver_platter); call it via `tokio::task::spawn_blocking`.
pub fn open_resume_branch(
    main_branch: &GenericBranch,
    campaign_branch_name: &str,
    codebase: &str,
) -> ResumeLookup {
    use silver_platter::vcs::BranchOpenError;

    // Short-circuit for schemes breezy's plugin registry can't open.
    // `get_forge` otherwise ends up in breezyshim's error conversion
    // (error.rs:444) where it unwraps `UnsupportedProtocol.url` -- an
    // attribute that doesn't exist on the Python exception, so it
    // panics. The caller (`web.rs:2654`) catches the panic and logs,
    // so nothing blows up, but a plain `git://` URL shouldn't be taken
    // all the way into breezy to find that out (BUGS.md #1).
    let branch_url = main_branch.get_user_url();
    if !matches!(
        branch_url.scheme(),
        "http" | "https" | "ssh" | "git+ssh" | "bzr" | "bzr+ssh"
    ) {
        log::warn!(
            "Skipping resume-branch lookup for unsupported scheme {}: {}",
            branch_url.scheme(),
            branch_url
        );
        return ResumeLookup::NotFound;
    }

    let forge = match get_forge(main_branch) {
        Ok(f) => f,
        Err(BrzError::UnsupportedForge(url)) => {
            log::warn!("Unsupported forge: {}", url);
            return ResumeLookup::NotFound;
        }
        Err(BrzError::ForgeLoginRequired) => {
            crate::metrics::FORGE_LOGIN_REQUIRED_COUNT.inc();
            log::error!("No credentials to list proposals on forge");
            return ResumeLookup::NotFound;
        }
        Err(e) => {
            log::warn!("Error opening forge for resume branch: {}", e);
            return ResumeLookup::NotFound;
        }
    };

    let candidates = [
        campaign_branch_name.to_string(),
        format!("{}/main", campaign_branch_name),
        format!("{}/main/{}", campaign_branch_name, codebase),
    ];

    for name in &candidates {
        match silver_platter::publish::find_existing_proposed_classified(
            main_branch,
            &forge,
            name,
            false,
            None,
            Some(&["https", "git", "bzr"]),
        ) {
            Ok((Some(branch), _, _)) => return ResumeLookup::Found(branch),
            Ok((None, _, _)) => continue,
            Err(BranchOpenError::RateLimited {
                url,
                description,
                retry_after,
            }) => {
                log::warn!(
                    "Rate-limited by forge for {} (candidate {}): {}",
                    url,
                    name,
                    description
                );
                let host = url
                    .host_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| url.to_string());
                return ResumeLookup::RateLimited { host, retry_after };
            }
            Err(BranchOpenError::Missing { url, .. }) => {
                log::warn!("Project missing on forge: {}", url);
                return ResumeLookup::NotFound;
            }
            Err(e) => {
                log::warn!("Error finding existing proposed branch {}: {}", name, e);
                return ResumeLookup::NotFound;
            }
        }
    }

    ResumeLookup::NotFound
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_resume_info_serialization() {
        let resume_info = ResumeInfo {
            run_id: "test-run-1".to_string(),
            campaign: "test-campaign".to_string(),
            codebase: "https://example.com/repo".to_string(),
            branch_name: "test-branch".to_string(),
            result_code: "success".to_string(),
            revision: Some("abc123".to_string()),
            result: None,
            result_branches: Vec::new(),
        };

        let json = serde_json::to_string(&resume_info).unwrap();
        let deserialized: ResumeInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(resume_info.run_id, deserialized.run_id);
        assert_eq!(resume_info.campaign, deserialized.campaign);
    }

    #[test]
    fn test_resume_error_display() {
        let error = ResumeError::NoResumeFound;
        assert_eq!(error.to_string(), "No suitable resume found");

        let error = ResumeError::InvalidResumeBranch("test".to_string());
        assert_eq!(error.to_string(), "Invalid resume branch: test");
    }
}
