use crate::rate_limiter::RateLimiter;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post, put};
use axum::Router;
use breezyshim::error::Error as BrzError;
use breezyshim::forge::Forge;
use breezyshim::RevisionId;
use janitor::vcs::{VcsManager, VcsType};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

async fn get_merge_proposals_by_campaign() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn get_merge_proposals_by_codebase() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn post_merge_proposal() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

async fn absorbed() {
    unimplemented!()
}

async fn get_policy() {
    unimplemented!()
}

async fn get_policies() {
    unimplemented!()
}

async fn put_policy() {
    unimplemented!()
}

async fn put_policies() {
    unimplemented!()
}

async fn update_merge_proposal() {
    unimplemented!()
}

async fn delete_policy(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let result = match sqlx::query("DELETE FROM named_publish_policy WHERE name = $1")
        .bind(&name)
        .execute(&state.conn)
        .await
    {
        Ok(result) => result,
        Err(e)
            if e.as_database_error()
                .map(|e| e.is_foreign_key_violation())
                .unwrap_or(false) =>
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "reason": "Policy in use",
                    "name": name,
                })),
            );
        }
        Err(e) => {
            log::warn!("Error deleting policy {}: {}", name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json("Error deleting policy".into()),
            );
        }
    };

    if result.rows_affected() == 0 {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "reason": "No such policy",
                "name": name,
            })),
        )
    } else {
        (StatusCode::NO_CONTENT, Json(serde_json::Value::Null))
    }
}

async fn consider(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    async fn run(state: Arc<AppState>, id: String) {
        let (run, rate_limit_bucket, command, unpublished_branches) =
            match crate::state::iter_publish_ready(&state.conn, Some(&id))
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                Some((run, rate_limit_bucket, command, unpublished_branches)) => {
                    (run, rate_limit_bucket, command, unpublished_branches)
                }
                None => return,
            };
        crate::consider_publish_run(
            &state.conn,
            state.redis.clone(),
            state.config,
            &state.publish_worker,
            &state.vcs_managers,
            &state.bucket_rate_limiter,
            &run,
            &rate_limit_bucket,
            unpublished_branches.as_slice(),
            &command,
            state.push_limit,
            state.require_binary_diff,
        )
        .await
        .unwrap();
    }

    tokio::spawn(run(state.clone(), id));
    (StatusCode::ACCEPTED, "Consider started")
}

#[derive(serde::Serialize, serde::Deserialize, sqlx::FromRow)]
/// Details about a publish operation.
pub struct PublishDetails {
    codebase: String,
    target_branch_url: String,
    branch: String,
    main_branch_revision: RevisionId,
    revision: RevisionId,
    mode: String,
    merge_proposal_url: String,
    result_code: String,
    description: String,
}

async fn get_publish_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let publish = sqlx::query_as::<_, PublishDetails>(
        r#"""
SELECT
  codebase,
  branch_name,
  main_branch_revision,
  revision,
  mode,
  merge_proposal_url,
  target_branch_url,
  result_code,
  description
FROM publish
LEFT JOIN codebase
ON codebase.branch_url = publish.target_branch_url
WHERE id = $1
"""#,
    )
    .bind(&id)
    .fetch_optional(&state.conn)
    .await
    .unwrap();

    if let Some(details) = publish {
        (StatusCode::OK, Json(serde_json::to_value(details).unwrap()))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "reason": "No such publish",
                "id": id,
            })),
        )
    }
}

async fn publish() {
    unimplemented!()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ForgeCredentials {
    kind: String,
    name: String,
    url: url::Url,
    user: Option<String>,
    user_url: Option<url::Url>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Credentials {
    ssh_keys: Vec<String>,
    pgp_keys: Vec<String>,
    hosting: Vec<ForgeCredentials>,
}

async fn get_credentials(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut ssh_keys = vec![];

    let ssh_dir = std::env::home_dir().unwrap().join(".ssh");

    for entry in std::fs::read_dir(ssh_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().unwrap() == "pub" {
            let f = std::fs::File::open(path).unwrap();
            use std::io::BufRead;
            let reader = std::io::BufReader::new(f);
            let lines = reader.lines();
            ssh_keys.extend(lines.map(|l| l.unwrap().trim().to_string()));
        }
    }

    let mut pgp_keys = vec![];
    for gpg_entry in state.gpg.keylist(true) {
        pgp_keys.push(String::from_utf8(state.gpg.key_export_minimal(&gpg_entry.fpr)).unwrap());
    }

    let mut hosting = vec![];
    for instance in breezyshim::forge::iter_forge_instances() {
        let current_user = match instance.get_current_user() {
            Ok(user) => user,
            Err(BrzError::ForgeLoginRequired) => continue,
            Err(BrzError::UnsupportedForge(..)) => {
                // WTF? Well, whatever.
                continue;
            }
            Err(BrzError::RedirectRequested { .. }) => {
                // This should never happen; forge implementation is meant to either ignore or handle this redirect.
                continue;
            }
            Err(e) => {
                log::warn!(
                    "Error getting current user for {}: {}",
                    instance.forge_name(),
                    e
                );
                continue;
            }
        };
        let current_user_url = current_user
            .as_ref()
            .map(|current_user| instance.get_user_url(&current_user).unwrap());
        let forge = ForgeCredentials {
            kind: instance.forge_kind(),
            name: instance.forge_name(),
            url: instance.base_url(),
            user: current_user,
            user_url: current_user_url,
        };
        hosting.push(forge);
    }

    (
        StatusCode::OK,
        Json(Credentials {
            ssh_keys,
            pgp_keys,
            hosting,
        }),
    )
}

async fn health() -> &'static str {
    "OK"
}

async fn ready() -> &'static str {
    "OK"
}

async fn check_straggler(
    proposal_info_manager: &crate::proposal_info::ProposalInfoManager,
    url: &url::Url,
) {
    // Find the canonical URL
    match reqwest::get(url.to_string()).await {
        Ok(resp) => {
            if resp.status() == 200 && resp.url() != url {
                proposal_info_manager
                    .update_canonical_url(url, resp.url())
                    .await
                    .unwrap();
            }
            if resp.status() == 404 {
                // TODO(jelmer): Keep it but leave a tumbestone around?
                proposal_info_manager
                    .delete_proposal_info(url)
                    .await
                    .unwrap();
            }
        }
        Err(e) => {
            log::warn!("Got error loading straggler {}: {}", url, e);
        }
    }
}

async fn check_stragglers(
    State(state): State<Arc<AppState>>,
    Query(ndays): Query<usize>,
) -> impl IntoResponse {
    async fn scan(conn: PgPool, redis: Option<redis::aio::ConnectionManager>, urls: Vec<url::Url>) {
        let proposal_info_manager =
            crate::proposal_info::ProposalInfoManager::new(conn.clone(), redis.clone()).await;
        for url in urls {
            check_straggler(&proposal_info_manager, &url).await;
        }
    }

    let proposal_info_manager =
        crate::proposal_info::ProposalInfoManager::new(state.conn.clone(), state.redis.clone())
            .await;

    let urls = proposal_info_manager
        .iter_outdated_proposal_info_urls(chrono::Duration::days(ndays as i64))
        .await
        .unwrap();

    let conn = state.conn.clone();
    let redis = state.redis.clone();

    tokio::spawn(scan(conn, redis, urls.clone()));

    (StatusCode::OK, Json(serde_json::json!(urls)))
}

async fn scan(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    async fn scan(state: Arc<AppState>) {
        crate::check_existing(
            state.conn.clone(),
            state.redis.clone(),
            state.config,
            &state.publish_worker,
            &state.bucket_rate_limiter,
            state.forge_rate_limiter.clone(),
            &state.vcs_managers,
            state.modify_mp_limit,
            state.unexpected_mp_limit,
        )
        .await;
    }

    tokio::spawn(scan(state));
    (StatusCode::ACCEPTED, "Scan started")
}

async fn refresh_status(
    State(state): State<Arc<AppState>>,
    Query(url): Query<url::Url>,
) -> impl IntoResponse {
    log::info!("Request to refresh proposal status for {}", url);

    async fn scan(state: Arc<AppState>, url: url::Url) {
        let mp = breezyshim::forge::get_proposal_by_url(&url).unwrap();
        let status = if mp.is_merged().unwrap() {
            breezyshim::forge::MergeProposalStatus::Merged
        } else if mp.is_closed().unwrap() {
            breezyshim::forge::MergeProposalStatus::Closed
        } else {
            breezyshim::forge::MergeProposalStatus::Open
        };
        match crate::check_existing_mp(
            &state.conn,
            state.redis.clone(),
            state.config,
            &state.publish_worker,
            &mp,
            status,
            &state.vcs_managers,
            &state.bucket_rate_limiter,
            false,
            None,
            None,
        )
        .await
        {
            Ok(_) => {
                log::info!("Refreshed proposal status for {}", url);
            }
            Err(crate::CheckMpError::NoRunForMergeProposal(url)) => {
                log::info!("Unable to find stored metadata for {}, skipping", url);
            }
            Err(crate::CheckMpError::BranchRateLimited { retry_after }) => {
                log::info!("Rate-limited accessing {}, skipping", url);
            }
            Err(crate::CheckMpError::UnexpectedHttpStatus {}) => {
                log::info!("Unexpected HTTP status {} for {}, skipping", status, url);
            }
            Err(crate::CheckMpError::ForgeLoginRequired {}) => {
                log::info!("Forge login required for {}, skipping", url);
            }
        }
    }

    tokio::spawn(scan(state.clone(), url));
    (StatusCode::ACCEPTED, "Refresh of proposal status started")
}

async fn autopublish() {
    unimplemented!()
}

async fn get_rate_limit(
    State(state): State<Arc<AppState>>,
    Path(bucket): Path<String>,
) -> impl IntoResponse {
    let stats = state.bucket_rate_limiter.lock().unwrap().get_stats();

    if let Some(stats) = stats {
        if let Some(current_open) = stats.per_bucket.get(&bucket) {
            let max_open = state
                .bucket_rate_limiter
                .lock()
                .unwrap()
                .get_max_open(&bucket);
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(&BucketRateLimit {
                        open: Some(*current_open),
                        max_open,
                        remaining: max_open.map(|max_open| max_open - *current_open),
                    })
                    .unwrap(),
                ),
            )
        } else {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "reason": "No such rate limit bucket",
                    "bucket": bucket,
                })),
            )
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "reason": "No rate limit stats available",
            })),
        )
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BucketRateLimit {
    open: Option<usize>,
    max_open: Option<usize>,
    remaining: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RateLimitsInfo {
    per_bucket: HashMap<String, BucketRateLimit>,
    per_forge: HashMap<String, chrono::DateTime<chrono::Utc>>,
    push_limit: Option<usize>,
}

async fn get_all_rate_limits(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let stats = state.bucket_rate_limiter.lock().unwrap().get_stats();

    let per_bucket = if let Some(stats) = stats {
        let mut per_bucket = HashMap::new();
        for (bucket, current_open) in stats.per_bucket.iter() {
            let max_open = state
                .bucket_rate_limiter
                .lock()
                .unwrap()
                .get_max_open(bucket);
            per_bucket.insert(
                bucket.clone(),
                BucketRateLimit {
                    open: Some(*current_open),
                    max_open,
                    remaining: max_open.map(|max_open| max_open - *current_open),
                },
            );
        }
        per_bucket
    } else {
        HashMap::new()
    };

    Json(
        serde_json::to_value(&RateLimitsInfo {
            per_bucket,
            per_forge: state
                .forge_rate_limiter
                .read()
                .unwrap()
                .iter()
                .map(|(f, t)| (f.to_string(), *t))
                .collect(),
            push_limit: state.push_limit,
        })
        .unwrap(),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Blocker<D> {
    result: bool,
    details: D,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerSuccessDetails {
    result_code: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerInactiveDetails {
    inactive: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerCommandDetails {
    correct: String,
    actual: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Review {
    reviewer: String,
    reviewed_at: chrono::DateTime<chrono::Utc>,
    comment: String,
    verdict: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerPublishStatusDetails {
    status: String,
    reviews: HashMap<String, Review>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerBackoffDetails {
    attempt_count: usize,
    next_try_time: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerProposeRateLimitDetails {
    open: Option<usize>,
    max_open: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerChangeSetDetails {
    change_set_id: String,
    change_set_state: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerPreviousMpDetails {
    url: String,
    status: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerInfo {
    success: Blocker<BlockerSuccessDetails>,
    inactive: Blocker<BlockerInactiveDetails>,
    command: Blocker<BlockerCommandDetails>,
    publish_status: Blocker<BlockerPublishStatusDetails>,
    backoff: Blocker<BlockerBackoffDetails>,
    propose_rate_limit: Blocker<BlockerProposeRateLimitDetails>,
    change_set: Blocker<BlockerChangeSetDetails>,
    previous_mp: Blocker<Vec<BlockerPreviousMpDetails>>,
}

async fn get_blockers(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    #[derive(sqlx::FromRow)]
    struct RunDetails {
        id: String,
        codebase: String,
        campaign: String,
        finish_time: chrono::DateTime<chrono::Utc>,
        run_command: String,
        publish_status: String,
        rate_limit_bucket: Option<String>,
        revision: Option<breezyshim::RevisionId>,
        policy_command: String,
        result_code: String,
        change_set_state: String,
        change_set: String,
        inactive: bool,
    }

    let run = sqlx::query_as::<_, RunDetails>(
        r#"""
SELECT
  run.id AS id,
  run.codebase AS codebase,
  run.suite AS campaign,
  run.finish_time AS finish_time,
  run.command AS run_command,
  run.publish_status AS publish_status,
  named_publish_policy.rate_limit_bucket AS rate_limit_bucket,
  run.revision AS revision,
  candidate.command AS policy_command,
  run.result_code AS result_code,
  change_set.state AS change_set_state,
  change_set.id AS change_set,
  codebase.inactive AS inactive
FROM run
INNER JOIN codebase ON codebase.name = run.codebase
INNER JOIN candidate ON candidate.codebase = run.codebase AND candidate.suite = run.suite
INNER JOIN named_publish_policy ON candidate.publish_policy = named_publish_policy.name
INNER JOIN change_set ON change_set.id = run.change_set
WHERE run.id = $1
"""#,
    )
    .bind(&id)
    .fetch_optional(&state.conn)
    .await
    .unwrap();

    let run = if let Some(run) = run {
        run
    } else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "reason": "No such publish-ready run",
                "run_id": id,
            })),
        );
    };

    #[derive(sqlx::FromRow)]
    struct ReviewDetails {
        reviewer: String,
        reviewed_at: chrono::DateTime<chrono::Utc>,
        comment: String,
        verdict: String,
    }

    let reviews = sqlx::query_as::<_, ReviewDetails>("SELECT * FROM review WHERE run_id = $1")
        .bind(&id)
        .fetch_all(&state.conn)
        .await
        .unwrap();

    let attempt_count = if let Some(revision) = run.revision {
        crate::state::get_publish_attempt_count(&state.conn, &revision, &["differ-unreachable"])
            .await
            .unwrap()
    } else {
        0
    };

    let last_mps = crate::state::get_previous_mp_status(&state.conn, &run.codebase, &run.campaign)
        .await
        .unwrap();

    let success = Blocker {
        result: run.result_code == "success",
        details: BlockerSuccessDetails {
            result_code: run.result_code,
        },
    };

    let inactive = Blocker {
        result: !run.inactive,
        details: BlockerInactiveDetails {
            inactive: run.inactive,
        },
    };

    let command = Blocker {
        result: run.run_command == run.policy_command,
        details: BlockerCommandDetails {
            correct: run.policy_command,
            actual: run.run_command,
        },
    };

    let publish_status = Blocker {
        result: run.publish_status == "approved",
        details: BlockerPublishStatusDetails {
            status: run.publish_status,
            reviews: reviews
                .into_iter()
                .map(|row| {
                    (
                        row.reviewer.clone(),
                        Review {
                            reviewer: row.reviewer,
                            reviewed_at: row.reviewed_at,
                            comment: row.comment,
                            verdict: row.verdict,
                        },
                    )
                })
                .collect(),
        },
    };

    let next_try_time = crate::calculate_next_try_time(run.finish_time, attempt_count);

    let backoff = Blocker {
        result: chrono::Utc::now() >= next_try_time,
        details: BlockerBackoffDetails {
            attempt_count,
            next_try_time,
        },
    };

    // TODO(jelmer): include forge rate limits?

    let propose_rate_limit = {
        if let Some(bucket) = run.rate_limit_bucket {
            let open = state
                .bucket_rate_limiter
                .lock()
                .unwrap()
                .get_stats()
                .and_then(|stats| stats.per_bucket.get(&bucket).cloned());
            let max_open = state
                .bucket_rate_limiter
                .lock()
                .unwrap()
                .get_max_open(&bucket);
            Blocker {
                result: state
                    .bucket_rate_limiter
                    .lock()
                    .unwrap()
                    .check_allowed(&bucket)
                    .is_allowed(),
                details: BlockerProposeRateLimitDetails { open, max_open },
            }
        } else {
            Blocker {
                result: true,
                details: BlockerProposeRateLimitDetails {
                    open: None,
                    max_open: None,
                },
            }
        }
    };

    let change_set = Blocker {
        result: ["publishing", "ready"].contains(&run.change_set_state.as_str()),
        details: BlockerChangeSetDetails {
            change_set_id: run.change_set,
            change_set_state: run.change_set_state,
        },
    };

    let previous_mp = Blocker {
        result: last_mps
            .iter()
            .all(|last_mp| last_mp.1 != "rejected" && last_mp.1 != "closed"),
        details: last_mps
            .iter()
            .map(|last_mp| BlockerPreviousMpDetails {
                url: last_mp.0.clone(),
                status: last_mp.1.clone(),
            })
            .collect(),
    };

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(&BlockerInfo {
                success,
                previous_mp,
                change_set,
                inactive,
                command,
                publish_status,
                backoff,
                propose_rate_limit,
            })
            .unwrap(),
        ),
    )
}

/// Create the web application router with all routes.
pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/:campaign/merge-proposals",
            get(get_merge_proposals_by_campaign),
        )
        .route(
            "/c/:codebase/merge-proposals",
            get(get_merge_proposals_by_codebase),
        )
        .route("/merge-proposals", get(post_merge_proposal))
        .route("/absorbed", get(absorbed))
        .route("/policy/:name", get(get_policy))
        .route("/policy", get(get_policies))
        .route("/policy/:name", put(put_policy))
        .route("/policy", put(put_policies))
        .route("/merge-proposal", post(update_merge_proposal))
        .route("/policy/:name", delete(delete_policy))
        .route("/merge-proposal", post(update_merge_proposal))
        .route("/consider:id", post(consider))
        .route("/publish/:id", get(get_publish_by_id))
        .route("/:campaign/:codebase/publish", post(publish))
        .route("/credentials", get(get_credentials))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/scan", post(scan))
        .route("/check-stragglers", post(check_stragglers))
        .route("/refresh-status", post(refresh_status))
        .route("/autopublish", post(autopublish))
        .route("/rate-limits/:bucket", get(get_rate_limit))
        .route("/rate-limits", get(get_all_rate_limits))
        .route("/blockers/:id", get(get_blockers))
        .with_state(state)
}
