use crate::Mode;
use breezyshim::branch::Branch;
use breezyshim::transport::Transport;
use breezyshim::RevisionId;
use sqlx::{FromRow, PgPool, Row};
use url::Url;

/// Persist the result of a publish attempt: insert a `publish` row,
/// upsert the `merge_proposal` row when a proposal was created, mark
/// `new_result_branch` rows absorbed for push-mode wins, and bump the
/// change_set state to `publishing` or `done` as appropriate.
pub async fn store_publish(
    conn: &PgPool,
    change_set: &str,
    codebase: &str,
    branch_name: Option<&str>,
    target_branch_url: Option<&Url>,
    target_branch_web_url: Option<&str>,
    main_branch_revision: Option<&RevisionId>,
    revision: Option<&RevisionId>,
    role: &str,
    mode: Mode,
    result_code: &str,
    description: &str,
    merge_proposal_url: Option<&Url>,
    publish_id: Option<&str>,
    requester: Option<&str>,
    run_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    let mut tx = conn.begin().await?;

    if result_code == "success" {
        if let Some(merge_proposal_url) = merge_proposal_url {
            assert_eq!(mode, Mode::Propose);
            sqlx::query(
                "INSERT INTO merge_proposal (url, status, revision, last_scanned,  target_branch_url, codebase) VALUES ($1, 'open', $2, NOW(), $3, $4) ON CONFLICT (url) DO UPDATE SET revision = EXCLUDED.revision, last_scanned = EXCLUDED.last_scanned, target_branch_url = EXCLUDED.target_branch_url, codebase = EXCLUDED.codebase")
            .bind(merge_proposal_url.to_string())
            .bind(revision.map(|r| r.to_string()))
            .bind(target_branch_url.map(|u| u.to_string()))
            .bind(codebase)
            .execute(&mut *tx)
            .await?;
        } else {
            assert!(revision.is_some());
            assert!([Mode::Push, Mode::PushDerived].contains(&mode));
            assert!(run_id.is_some());
            if mode == Mode::Push {
                sqlx::query(
                    "UPDATE new_result_branch SET absorbed = true WHERE run_id = $1 AND role = $2",
                )
                .bind(run_id)
                .bind(role)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    // `mode` is a `publish_mode` enum on the column; sqlx binds it
    // as TEXT, so cast at the placeholder. Without `$5::publish_mode`
    // postgres rejects the INSERT with "column mode is of type
    // publish_mode but expression is of type text".
    sqlx::query(
        "INSERT INTO publish (branch_name, main_branch_revision, revision, role, mode, result_code, description, merge_proposal_url, id, requester, change_set, run_id, target_branch_url, target_branch_web_url, codebase) values ($1, $2, $3, $4, $5::publish_mode, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) ")
    .bind(branch_name)
    .bind(main_branch_revision.map(|r| r.to_string()))
    .bind(revision.map(|r| r.to_string()))
    .bind(role)
    .bind(mode.to_string())
    .bind(result_code)
    .bind(description)
    .bind(merge_proposal_url.map(|u| u.to_string()))
    .bind(publish_id)
    .bind(requester)
    .bind(change_set)
    .bind(run_id)
    .bind(target_branch_url.map(|u| u.to_string()))
    .bind(target_branch_web_url.map(|u| u.to_string()))
    .bind(codebase)
    .execute(&mut *tx)
    .await?;
    if result_code == "success" {
        sqlx::query("UPDATE change_set SET state = 'publishing' WHERE state = 'ready' AND id = $1")
            .bind(change_set)
            .execute(&mut *tx)
            .await?;

        // Check if there's nothing left to publish for this change_set
        let remaining_unpublished = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM new_result_branch 
             INNER JOIN run ON new_result_branch.run_id = run.id 
             WHERE run.change_set = $1 AND NOT COALESCE(new_result_branch.absorbed, false)",
        )
        .bind(change_set)
        .fetch_one(&mut *tx)
        .await?;

        if remaining_unpublished == 0 {
            // Mark change_set as done since nothing is left to publish
            sqlx::query("UPDATE change_set SET state = 'done' WHERE id = $1 AND state != 'done'")
                .bind(change_set)
                .execute(&mut *tx)
                .await?;

            log::info!(
                "Marked change_set {} as done - no unpublished branches remaining",
                change_set
            );
        }
    }

    tx.commit().await
}

pub(crate) async fn already_published(
    conn: &PgPool,
    target_branch_url: &Url,
    branch_name: &str,
    revision: &RevisionId,
    modes: &[Mode],
) -> Result<bool, sqlx::Error> {
    let modes = modes.iter().map(|m| m.to_string()).collect::<Vec<_>>();
    let row = sqlx::query(
        "SELECT * FROM publish WHERE mode = ANY($1::publish_mode[]) AND revision = $2 AND target_branch_url = $3 AND branch_name = $4").bind(modes).bind(revision.to_string()).bind(target_branch_url.to_string()).bind(branch_name).fetch_optional(conn).await?;
    Ok(row.is_some())
}

pub(crate) async fn get_open_merge_proposal(
    conn: &PgPool,
    codebase: &str,
    branch_name: &str,
) -> Result<Option<(RevisionId, Url)>, sqlx::Error> {
    let row: Option<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(
        &*r###"
SELECT
    merge_proposal.revision,
    merge_proposal.url
FROM
    merge_proposal
INNER JOIN publish ON merge_proposal.url = publish.merge_proposal_url
WHERE
    merge_proposal.status = 'open' AND
    merge_proposal.codebase = $1 AND
    publish.branch_name = $2
ORDER BY timestamp DESC
"###,
    ))
    .bind(codebase)
    .bind(branch_name)
    .fetch_optional(conn)
    .await?;

    match row {
        Some((revision, url)) => {
            match Url::parse(&url) {
                Ok(parsed_url) => Ok(Some((
                    RevisionId::from(revision.as_bytes().to_vec()),
                    parsed_url,
                ))),
                Err(e) => {
                    log::error!("Failed to parse URL '{}': {}", url, e);
                    Ok(None) // Return None instead of crashing on invalid URL
                }
            }
        }
        None => Ok(None),
    }
}

pub(crate) async fn check_last_published(
    conn: &PgPool,
    campaign: &str,
    codebase: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, sqlx::Error> {
    let row: Option<(Option<chrono::DateTime<chrono::Utc>>,)> =
        sqlx::query_as(sqlx::AssertSqlSafe(
            &*r###"
SELECT timestamp from publish left join run on run.revision = publish.revision
WHERE run.suite = $1 and run.codebase = $2 AND publish.result_code = 'success'
order by timestamp desc limit 1
"###,
        ))
        .bind(campaign)
        .bind(codebase)
        .fetch_optional(conn)
        .await?;
    Ok(row.and_then(|(timestamp,)| timestamp))
}

/// Look up the codebase and rate-limit bucket for a merge proposal whose
/// source revision matches a known [`new_result_branch`] row, by joining
/// through `candidate` and `named_publish_policy`. Returns `(None, None)`
/// when zero or multiple candidates match; we don't want to guess
/// ambiguously.
pub async fn guess_proposal_info_from_revision(
    conn: &PgPool,
    revision: &RevisionId,
) -> Result<(Option<String>, Option<String>), sqlx::Error> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        r#"SELECT DISTINCT run.codebase,
       named_publish_policy.rate_limit_bucket AS rate_limit_bucket
FROM run
LEFT JOIN new_result_branch rb ON rb.run_id = run.id
INNER JOIN candidate
        ON run.codebase = candidate.codebase AND run.suite = candidate.suite
INNER JOIN named_publish_policy
        ON named_publish_policy.name = candidate.publish_policy
WHERE rb.revision = $1 AND run.codebase IS NOT NULL"#,
    )
    .bind(revision.to_string())
    .fetch_all(conn)
    .await?;
    if rows.len() == 1 {
        let (codebase, bucket) = rows.into_iter().next().unwrap();
        Ok((Some(codebase), bucket))
    } else {
        Ok((None, None))
    }
}

/// For a merge proposal whose source branch lives on the janitor's own
/// hosting and follows the `<campaign>/...` naming convention, look up
/// the rate-limit bucket that applies to this codebase + campaign by
/// joining through `named_publish_policy`.
pub async fn guess_rate_limit_bucket(
    conn: &PgPool,
    codebase: &str,
    source_branch_name: &str,
) -> Result<Option<String>, sqlx::Error> {
    // Assume source_branch_name is `<campaign>/...`.
    let campaign = source_branch_name
        .split('/')
        .next()
        .unwrap_or(source_branch_name);
    sqlx::query_scalar::<_, Option<String>>(
        r#"SELECT named_publish_policy.rate_limit_bucket FROM candidate
INNER JOIN named_publish_policy ON named_publish_policy.name = candidate.publish_policy
WHERE candidate.suite = $1 AND candidate.codebase = $2"#,
    )
    .bind(campaign)
    .bind(codebase)
    .fetch_optional(conn)
    .await
    .map(|opt| opt.flatten())
}

/// Result of preparing the candidate-lookup query for
/// [`guess_codebase_from_branch_url`]. Split out so the SQL string,
/// the bound `text[]` parameter, and the parsed branch name can be
/// unit-tested without needing a live PostgreSQL.
pub(crate) struct GuessCodebaseQuery {
    /// The trimmed URL we're searching for, used both as the first
    /// candidate and as the comparison key when checking the result.
    pub url_trimmed: String,
    /// The text-array bound to `$1`. Contains the trimmed URL and,
    /// when distinct, the trimmed repository URL with the branch
    /// segment stripped off.
    pub candidates: Vec<String>,
    /// The branch name extracted from the URL's `branch=` segment
    /// parameter, if any.
    pub branch: Option<String>,
}

pub(crate) const GUESS_CODEBASE_QUERY_SQL: &str = "
SELECT
  name, branch_url
FROM
  codebase
WHERE
  TRIM(trailing '/' from branch_url) = ANY($1::text[])
ORDER BY length(branch_url) DESC
";

/// Strip trailing `/` characters from `s` in place, avoiding the extra
/// allocation that `s.trim_end_matches('/').to_string()` would incur.
fn trim_trailing_slash(mut s: String) -> String {
    let end = s.trim_end_matches('/').len();
    s.truncate(end);
    s
}

pub(crate) fn build_guess_codebase_query(url: &url::Url) -> Option<GuessCodebaseQuery> {
    let url_trimmed = trim_trailing_slash(url.to_string());
    let parsed = match url_trimmed.parse::<Url>() {
        Ok(url) => url,
        Err(e) => {
            log::error!("Failed to parse URL: {}", e);
            return None;
        }
    };
    let (repo_url, params) = breezyshim::urlutils::split_segment_parameters(&parsed);
    let branch = params
        .get("branch")
        .map(|b| breezyshim::urlutils::unescape_utf8(b));
    let repo_url_trimmed = trim_trailing_slash(repo_url.to_string());
    // Also consider scheme variants: forges (salsa, github) commonly
    // expose the same repo as both https:// and git+ssh:// (or plain
    // ssh://). The codebase table is typically keyed on the https form,
    // while Debian merge-proposal target_branch_urls arrive with
    // git+ssh. Enumerate the obvious alternates so the lookup hits on
    // either. Seen in prod logs as "No codebase known for ... git+ssh:
    // //git@salsa.debian.org/...,branch=master" for rows whose codebase
    // row uses https:// (BUGS.md #7).
    let mut candidates = Vec::with_capacity(6);
    push_scheme_variants(&mut candidates, &url_trimmed);
    if repo_url_trimmed != url_trimmed {
        push_scheme_variants(&mut candidates, &repo_url_trimmed);
    }
    Some(GuessCodebaseQuery {
        url_trimmed,
        candidates,
        branch,
    })
}

/// Append the URL itself plus any scheme-swapped variants that refer
/// to the same forge path, skipping duplicates. Handles the common
/// salsa/github https<->ssh<->git+ssh aliasing.
fn push_scheme_variants(candidates: &mut Vec<String>, url_str: &str) {
    let push = |candidates: &mut Vec<String>, s: String| {
        if !candidates.contains(&s) {
            candidates.push(s);
        }
    };
    push(candidates, url_str.to_string());

    let parsed = match url::Url::parse(url_str) {
        Ok(u) => u,
        Err(_) => return,
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return,
    };
    // Normalise away the `git@` user that ssh URLs carry.
    let path = parsed.path().trim_start_matches('/');
    let qf = match (parsed.query(), parsed.fragment()) {
        (Some(q), Some(f)) => format!("?{}#{}", q, f),
        (Some(q), None) => format!("?{}", q),
        (None, Some(f)) => format!("#{}", f),
        (None, None) => String::new(),
    };
    match parsed.scheme() {
        "git+ssh" | "ssh" => {
            push(candidates, format!("https://{}/{}{}", host, path, qf));
        }
        "https" | "http" => {
            push(candidates, format!("git+ssh://git@{}/{}{}", host, path, qf));
            push(candidates, format!("ssh://git@{}/{}{}", host, path, qf));
        }
        _ => {}
    }
}

pub(crate) async fn guess_codebase_from_branch_url(
    conn: &PgPool,
    url: &url::Url,
    mut possible_transports: Option<&mut Vec<Transport>>,
) -> Result<Option<String>, sqlx::Error> {
    let GuessCodebaseQuery {
        url_trimmed,
        candidates,
        branch,
    } = match build_guess_codebase_query(url) {
        Some(q) => q,
        None => return Ok(None),
    };
    let result = sqlx::query_as::<_, (String, String)>(GUESS_CODEBASE_QUERY_SQL)
        .bind(&candidates)
        .fetch_optional(conn)
        .await?;

    let result = match result {
        Some(r) => r,
        None => return Ok(None),
    };

    if url_trimmed == result.1.trim_end_matches('/') {
        return Ok(Some(result.0));
    }

    let branch_url = match result.1.parse() {
        Ok(url) => url,
        Err(e) => {
            log::error!("Failed to parse branch URL '{}': {}", result.1, e);
            return Ok(None);
        }
    };

    // spawn_blocking needs a 'static closure, so the caller's list is moved in
    // and handed back rather than borrowed.
    let mut transports = possible_transports
        .as_deref_mut()
        .map(std::mem::take)
        .unwrap_or_default();
    let joined = tokio::task::spawn_blocking(move || {
        let result =
            silver_platter::vcs::open_branch(&branch_url, Some(&mut transports), None, None);
        (result, transports)
    })
    .await;
    let joined = match joined {
        Ok((result, transports)) => {
            if let Some(out) = possible_transports.as_deref_mut() {
                *out = transports;
            }
            Ok(result)
        }
        Err(e) => Err(e),
    };
    let source_branch = match joined {
        Ok(branch_result) => match branch_result {
            Ok(branch) => {
                if let Some(out) = possible_transports.as_deref_mut() {
                    crate::remember_transport(out, branch.user_transport());
                }
                branch
            }
            Err(e) => {
                // This is the SECOND-pass verification: the SQL
                // candidate list already matched (line 367 above
                // takes the trivial-match shortcut), and we only
                // get here when the candidate's stored URL has a
                // different scheme/host shape than the one we're
                // checking. Failure to *open* that URL is a
                // remote-side problem (salsa returns 429 for
                // anonymous HTTPS clones; transient TCP errors;
                // codebase moved/deleted) - none of which are
                // bugs in our code. The caller already treats
                // None as "couldn't guess, fall through" and
                // continues, so logging at error level just
                // spams the journal during normal salsa
                // rate-limit windows. Downgrade to info.
                log::info!("Failed to open VCS branch (treating as unguessable): {}", e);
                return Ok(None);
            }
        },
        Err(e) => {
            log::error!("Task join error: {}", e);
            return Ok(None);
        }
    };
    if source_branch
        .get_user_url()
        .to_string()
        .trim_end_matches("/")
        != url.to_string().trim_end_matches("/")
        && source_branch.name() != branch
    {
        log::info!(
            "Did not resolve branch URL to codebase: {} ({}) != {} ({})",
            source_branch.get_user_url(),
            source_branch.name().unwrap_or("".to_string()),
            url,
            branch.unwrap_or("".to_string()),
        );
        return Ok(None);
    }
    Ok(Some(result.0))
}

#[derive(Debug, Clone, sqlx::FromRow)]
/// Information about a run that resulted in a merge proposal.
///
/// Returned by [`get_merge_proposal_run`] for `check_existing_mp` to
/// consume.
pub struct MergeProposalRun {
    /// Run id.
    pub id: String,
    /// Campaign name (`run.suite` in the schema).
    pub campaign: String,
    /// URL of the codebase branch the run targeted.
    pub branch_url: String,
    /// Command that produced the run.
    pub command: String,
    /// Configured value of the candidate that scheduled this run.
    /// Matches `run.value` which is `int` (INT4) in the schema.
    pub value: i32,
    /// Role of the result branch that matched the merge proposal
    /// revision (e.g. `main`).
    pub role: String,
    /// Remote branch name recorded for the matching result branch.
    pub remote_branch_name: String,
    /// Revision id of the matching result branch.
    pub revision: RevisionId,
    /// Codebase the run belonged to.
    pub codebase: String,
    /// Change set id.
    pub change_set: String,
}

/// Look up the most recent run whose `new_result_branch.revision`
/// matches the source revision recorded for the given merge proposal.
pub async fn get_merge_proposal_run(
    conn: &PgPool,
    mp_url: &url::Url,
) -> Result<Option<MergeProposalRun>, sqlx::Error> {
    sqlx::query_as::<_, MergeProposalRun>(
        r#"
SELECT
    run.id AS id,
    run.suite AS campaign,
    run.branch_url AS branch_url,
    run.command AS command,
    run.value AS value,
    rb.role AS role,
    rb.remote_name AS remote_branch_name,
    rb.revision AS revision,
    run.codebase AS codebase,
    run.change_set AS change_set
FROM new_result_branch rb
RIGHT JOIN run ON rb.run_id = run.id
WHERE rb.revision IN (
    SELECT revision from merge_proposal WHERE merge_proposal.url = $1)
ORDER BY run.finish_time DESC
LIMIT 1
"#,
    )
    .bind(mp_url.to_string())
    .fetch_optional(conn)
    .await
}

/// Fetch the most recent effective run (`success` or
/// `nothing-new-to-do`) for the given codebase + campaign from the
/// `last_effective_runs` view.
pub async fn get_last_effective_run(
    conn: &PgPool,
    codebase: &str,
    campaign: &str,
) -> Result<Option<janitor::state::Run>, sqlx::Error> {
    sqlx::query_as(
        r#"
SELECT
    id, command,
    -- Schema columns are TIMESTAMP (no TZ); the `Run` struct
    -- decodes them as `DateTime<Utc>` (TIMESTAMPTZ). Cast both to
    -- TIMESTAMPTZ via `AT TIME ZONE 'UTC'` so sqlx FromRow doesn't
    -- error with "mismatched types ... is not compatible with SQL
    -- type TIMESTAMP". Same pattern used elsewhere
    -- (`get_run_details`, schedule.rs, etc.).
    (start_time AT TIME ZONE 'UTC') AS start_time,
    (finish_time AT TIME ZONE 'UTC') AS finish_time,
    description,
    result_code,
    value, main_branch_revision, revision, context, result, suite,
    instigated_context, vcs_type::text AS vcs_type, branch_url, logfilenames,
    worker,
    array(SELECT row(role, remote_name, base_revision,
     revision) FROM new_result_branch WHERE run_id = id) AS result_branches,
    result_tags, target_branch_url, change_set AS change_set,
    failure_transient, failure_stage, failure_details, codebase
FROM
    last_effective_runs
WHERE codebase = $1 AND suite = $2
LIMIT 1
"#,
    )
    .bind(codebase)
    .bind(campaign)
    .fetch_optional(conn)
    .await
}

/// Fetch a single run by id, reconstructing `result_branches` from
/// `new_result_branch` the same way `py/janitor/publish.py::get_run`
/// does. Used by the publish-status Redis listener to resolve the
/// `run_id` carried in an approval message.
pub async fn get_run(
    conn: &PgPool,
    run_id: &str,
) -> Result<Option<janitor::state::Run>, sqlx::Error> {
    sqlx::query_as(
        r#"
SELECT
    id, command,
    -- Schema columns are TIMESTAMP (no TZ); the `Run` struct
    -- decodes them as `DateTime<Utc>` (TIMESTAMPTZ). Cast both to
    -- TIMESTAMPTZ via `AT TIME ZONE 'UTC'` so sqlx FromRow doesn't
    -- error with "mismatched types ... is not compatible with SQL
    -- type TIMESTAMP". Same pattern used elsewhere
    -- (`get_run_details`, schedule.rs, etc.).
    (start_time AT TIME ZONE 'UTC') AS start_time,
    (finish_time AT TIME ZONE 'UTC') AS finish_time,
    description,
    result_code,
    value, main_branch_revision, revision, context, result, suite,
    instigated_context, vcs_type::text AS vcs_type, branch_url, logfilenames,
    worker,
    array(SELECT row(role, remote_name, base_revision,
     revision) FROM new_result_branch WHERE run_id = id) AS result_branches,
    result_tags, target_branch_url, change_set AS change_set,
    failure_transient, failure_stage, failure_details, codebase
FROM
    run
WHERE id = $1
"#,
    )
    .bind(run_id)
    .fetch_optional(conn)
    .await
}

/// Count the number of publish attempts for a specific revision, excluding those with transient result codes.
pub async fn get_publish_attempt_count(
    conn: &PgPool,
    revision: &RevisionId,
    transient_result_codes: &[&str],
) -> Result<usize, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, i64>(
        "select count(*) from publish where revision = $1 and result_code != ALL($2::text[])",
    )
    .bind(revision)
    .bind(transient_result_codes)
    .fetch_one(conn)
    .await? as usize)
}

/// Get the status of previous merge proposals for a codebase and campaign.
pub async fn get_previous_mp_status(
    conn: &PgPool,
    codebase: &str,
    campaign: &str,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        r#"
WITH per_run_mps AS (
    SELECT run.id AS run_id, run.finish_time,
    merge_proposal.url AS mp_url, merge_proposal.status AS mp_status
    FROM run
    LEFT JOIN merge_proposal ON run.revision = merge_proposal.revision
    WHERE run.codebase = $1
    AND run.suite = $2
    AND run.result_code = 'success'
    AND merge_proposal.status NOT IN ('open', 'abandoned')
    GROUP BY run.id, merge_proposal.url
)
SELECT mp_url, mp_status FROM per_run_mps
WHERE run_id = (
    SELECT run_id FROM per_run_mps ORDER BY finish_time DESC LIMIT 1)
"#,
    )
    .bind(codebase)
    .bind(campaign)
    .fetch_all(conn)
    .await
}

#[derive(Debug, Clone, sqlx::FromRow)]
/// Information about a branch that hasn't been published yet.
pub struct UnpublishedBranch {
    /// Role of the branch.
    pub role: String,
    /// Name of the remote branch.
    pub remote_name: Option<String>,
    /// Base revision ID.
    pub base_revision: Option<RevisionId>,
    /// Current revision ID.
    pub revision: Option<RevisionId>,
    /// Mode to use for publishing.
    pub publish_mode: Option<String>,
    /// Maximum frequency in days between publish attempts.
    pub max_frequency_days: Option<i32>,
    /// The name of the branch.
    pub name: Option<String>,
}

/// Iterate through runs that are ready to be published.
pub async fn iter_publish_ready(
    conn: &PgPool,
    run_id: Option<&str>,
) -> Result<Vec<(janitor::state::Run, String, String, Vec<UnpublishedBranch>)>, sqlx::Error> {
    // Explicit column list with the same casts the other Run-returning
    // queries (`get_last_effective_run`, `get_run`) apply: TIMESTAMPTZ
    // for start/finish_time and `vcs_type::text` for the enum, since
    // `janitor::state::Run` decodes those as `DateTime<Utc>` and
    // `String` respectively. `SELECT *` here used to crash the
    // publish_pending_ready loop with `column "vcs_type": mismatched
    // types; Rust type alloc::string::String (as SQL type TEXT) is
    // not compatible with SQL type vcs_type`.
    // Two type adapters needed to keep `janitor::state::Run::FromRow`
    // happy against `publish_ready`:
    // 1. `failure_details` is dropped from the view (a publishable
    //    run is by definition successful, so it'd always be NULL) -
    //    sub in an explicit NULL::json.
    // 2. `result_branches` is `result_branch[]` (a typed Postgres
    //    composite array) in the view, but `Run` decodes it as
    //    `RECORD[]`. Rebuild the RECORD[] form via
    //    `array(SELECT row(...) FROM new_result_branch ...)` - the
    //    same shape the other Run-returning queries use.
    let mut query = sqlx::QueryBuilder::new(
        "SELECT \
            id, command, \
            (start_time AT TIME ZONE 'UTC') AS start_time, \
            (finish_time AT TIME ZONE 'UTC') AS finish_time, \
            description, result_code, \
            value, main_branch_revision, revision, context, result, suite::text AS suite, \
            instigated_context, vcs_type::text AS vcs_type, branch_url, logfilenames, \
            worker, \
            array(SELECT row(role, remote_name, base_revision, revision) \
                  FROM new_result_branch WHERE run_id = id) AS result_branches, \
            result_tags, target_branch_url, \
            change_set, failure_transient, failure_stage, \
            NULL::json AS failure_details, \
            codebase, \
            policy_command, rate_limit_bucket, publish_status::text AS publish_status, change_set_state::text AS change_set_state, \
            -- Same RECORD[] coercion as result_branches: the view
            -- exposes `result_branch_with_policy[]` (a typed composite
            -- array including a `publish_mode` enum), but the
            -- consumer-side decode is `Vec<(String, Option<String>, ...,
            -- Option<String>, Option<i32>)>` (RECORD[] with mode as
            -- text). Without the projection sqlx silently `Err`s the
            -- try_get and the per-branch loop iterates zero rows,
            -- exactly the symptom that kept `0 published` in the
            -- log even though there were 19 publish-ready entries.
            array( \
                SELECT row( \
                    role, remote_name, base_revision, revision, \
                    mode::text, frequency_days \
                ) FROM unnest(unpublished_branches) \
            ) AS unpublished_branches \
         FROM publish_ready WHERE ",
    );
    if let Some(run_id) = run_id {
        query.push("id = ");
        query.push_bind(run_id);
    } else {
        query.push("True");
    }
    query.push(" AND publish_status = 'approved'");
    query.push(" AND change_set_state IN ('ready', 'publishing')");
    query.push(" AND exists (select from unnest(unpublished_branches) where mode in ('propose', 'attempt-push', 'push-derived', 'push'))");
    query.push(
        " ORDER BY change_set_state = 'publishing' DESC, value DESC NULLS LAST, finish_time DESC",
    );

    let query = query.build();

    let rows = query.fetch_all(conn).await?;

    let mut result = vec![];
    for row in rows {
        // Decode the unpublished_branches array
        // Note: We keep revision IDs as strings in the database, and convert to RevisionId when needed
        let unpublished_branches: Vec<UnpublishedBranch> = if let Ok(branches_data) = row
            .try_get::<Vec<(
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<i32>,
            )>, _>("unpublished_branches")
        {
            branches_data
                .into_iter()
                .map(
                    |(
                        role,
                        remote_name,
                        base_revision_str,
                        revision_str,
                        mode,
                        max_frequency_days,
                    )| {
                        UnpublishedBranch {
                            role,
                            remote_name,
                            base_revision: base_revision_str
                                .map(|s| RevisionId::from(s.as_bytes().to_vec())),
                            revision: revision_str.map(|s| RevisionId::from(s.as_bytes().to_vec())),
                            publish_mode: mode,
                            max_frequency_days,
                            name: None, // name is not part of the result_branch_with_policy type
                        }
                    },
                )
                .collect()
        } else {
            // Fallback to empty if decoding fails
            Vec::new()
        };

        let run = janitor::state::Run::from_row(&row).map_err(|e| {
            sqlx::Error::Decode(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to parse run from row: {}", e),
            )))
        })?;

        result.push((
            run,
            row.get("rate_limit_bucket"),
            row.get("policy_command"),
            unpublished_branches,
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `run.vcs_type` is a Postgres ENUM
    /// (`vcs_type`). Decoding it as `String` without a `::text`
    /// cast errors with `mismatched types; Rust type 'String' (as
    /// SQL type 'TEXT') is not compatible with SQL type vcs_type`,
    /// which made every `check_existing_mp` call that needed a Run
    /// 500 with `Database error checking merge proposal` and stalled
    /// the publisher's MP refresh loop. This is a static check of
    /// the SQL strings - full roundtrip would need a live DB plus
    /// a populated `run` row.
    #[test]
    fn run_select_queries_cast_vcs_type_to_text() {
        // Build the canonical query bodies we use in
        // get_last_effective_run / get_run by re-reading the
        // module source at compile time. Cheap regression: the
        // moment someone "cleans up" the cast we'll catch it here.
        let src = include_str!("state.rs");
        // Both SELECTs must mention the cast. We don't grep for
        // bare `vcs_type` because that would trip on column
        // definitions / variable names.
        let cast_count = src.matches("vcs_type::text AS vcs_type").count();
        assert!(
            cast_count >= 2,
            "expected at least two `vcs_type::text AS vcs_type` casts \
             (one in get_last_effective_run, one in get_run); found {}",
            cast_count
        );
    }

    #[test]
    fn guess_codebase_query_sql_has_no_quoted_identifier_artifact() {
        // Regression: a previous raw-string literal was r#"""...."""# which
        // produced SQL beginning with `""`, parsed by Postgres as an empty
        // quoted identifier and rejected at execution time.
        let trimmed = GUESS_CODEBASE_QUERY_SQL.trim();
        assert!(
            trimmed.starts_with("SELECT"),
            "query must start with SELECT, got: {:?}",
            trimmed
        );
        assert!(
            !GUESS_CODEBASE_QUERY_SQL.contains("\"\""),
            "query must not contain stray empty quoted identifiers: {:?}",
            GUESS_CODEBASE_QUERY_SQL
        );
    }

    #[test]
    fn guess_codebase_query_binds_text_array() {
        // The placeholder is `ANY($1::text[])`, so the prepared query
        // takes exactly one parameter - a text array. Regression for a
        // previous version that bound two separate strings.
        let placeholder_count = GUESS_CODEBASE_QUERY_SQL
            .match_indices('$')
            .filter(|(i, _)| {
                GUESS_CODEBASE_QUERY_SQL[i + 1..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            })
            .count();
        assert_eq!(placeholder_count, 1);
        assert!(GUESS_CODEBASE_QUERY_SQL.contains("$1::text[]"));
    }

    #[test]
    fn build_guess_codebase_query_trims_trailing_slash() {
        let url = "https://example.com/foo/bar/".parse().unwrap();
        let q = build_guess_codebase_query(&url).unwrap();
        assert_eq!(q.url_trimmed, "https://example.com/foo/bar");
        // Trailing-slash trim applies to every scheme variant, so
        // none of the produced candidates should re-introduce one.
        for c in &q.candidates {
            assert!(
                !c.ends_with('/'),
                "candidate must not retain trailing slash: {}",
                c
            );
        }
        assert!(q
            .candidates
            .contains(&"https://example.com/foo/bar".to_string()));
        assert_eq!(q.branch, None);
    }

    #[test]
    fn build_guess_codebase_query_separates_repo_url_and_branch() {
        // breezy uses the `,branch=...` segment parameter syntax to encode
        // a colocated branch name on a single repository URL. The lookup
        // should try both the full URL (with the segment params) and
        // the bare repository URL - each in every scheme variant we
        // recognise (https / git+ssh / ssh) so the codebase row hits
        // regardless of which form the publisher target_branch_url is
        // expressed in.
        let url = "https://example.com/foo/bar,branch=main".parse().unwrap();
        let q = build_guess_codebase_query(&url).unwrap();
        assert_eq!(q.branch.as_deref(), Some("main"));
        assert!(q.candidates.contains(&q.url_trimmed));
        // Bare repo URL (no segment params) is also a candidate.
        assert!(
            q.candidates
                .iter()
                .any(|c| c == "https://example.com/foo/bar"),
            "expected bare repo URL among candidates; got: {:?}",
            q.candidates
        );
        // ...and so are the ssh / git+ssh aliases of both forms.
        assert!(
            q.candidates
                .iter()
                .any(|c| c.starts_with("git+ssh://") && c.contains(",branch=main")),
            "expected git+ssh alias of the full URL; got: {:?}",
            q.candidates
        );
        assert!(
            q.candidates
                .iter()
                .any(|c| c.starts_with("git+ssh://") && !c.contains(",branch=")),
            "expected git+ssh alias of the bare repo URL; got: {:?}",
            q.candidates
        );
    }

    #[test]
    fn build_guess_codebase_query_dedups_when_repo_url_matches() {
        // When the URL has no segment parameters, the bare repo URL
        // equals the full URL - we should not enumerate the same
        // scheme variants twice. With three recognised schemes
        // (https / git+ssh / ssh) we expect exactly three
        // candidates, not six.
        let url = "https://example.com/foo/bar".parse().unwrap();
        let q = build_guess_codebase_query(&url).unwrap();
        let mut sorted = q.candidates.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            q.candidates.len(),
            "candidates must already be unique; got: {:?}",
            q.candidates
        );
        assert_eq!(
            q.candidates.len(),
            3,
            "expected one entry per scheme variant; got: {:?}",
            q.candidates
        );
        assert!(q
            .candidates
            .contains(&"https://example.com/foo/bar".to_string()));
    }
}
