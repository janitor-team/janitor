use crate::api_types::MessageResponse;
use crate::{
    get_builder, metrics::MetricsCollector, ActiveRun, AppState, Backchannel, BuilderResult,
    CampaignConfig, QueueItem,
};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Extension, Json, Router,
};
use chrono::Utc;
use janitor::shared_config::ConfigLoader;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

/// Request for work assignment.
///
/// `worker` is only honoured by the unauthenticated `/active-runs`
/// route on the private (intra-cluster) app. The authenticated
/// public app's `/active-runs` route (workers reach it via the ingress
/// `/runner/(.*)` -> `/$2`) ignores this field entirely and reads
/// the worker name from the credentials extension instead.
#[derive(Debug, Deserialize)]
struct AssignRequest {
    /// Worker name (private/admin path only). The Rust worker sends
    /// this as `node`; older Python workers sent `worker`.
    #[serde(alias = "node")]
    worker: Option<String>,
    /// Worker link.
    worker_link: Option<String>,
    /// Backchannel configuration.
    backchannel: Option<serde_json::Value>,
    /// Specific codebase to work on.
    codebase: Option<String>,
    /// Specific campaign to work on.
    campaign: Option<String>,
}

/// Request for updating run publish status.
#[derive(Debug, Deserialize)]
struct UpdateRunRequest {
    /// New publish status.
    publish_status: String,
}

/// Request for resume information.
#[derive(Debug, Deserialize)]
struct ResumeInfoRequest {
    /// Campaign name (stored as `run.suite` in the schema).
    campaign: String,
    /// Revision id of the resume branch's tip. Used together with
    /// `campaign` to form the `(suite, revision)` resume lookup key.
    resume_revision: String,
}

/// JSON body for `POST /schedule`. Callers may send either a `run_id`
/// (in which case campaign+codebase are derived from the run row) or
/// an explicit (campaign, codebase) pair, plus optional scheduling
/// knobs.
#[derive(Debug, Deserialize)]
struct ScheduleRequest {
    run_id: Option<String>,
    campaign: Option<String>,
    codebase: Option<String>,
    refresh: Option<bool>,
    change_set: Option<String>,
    requester: Option<String>,
    bucket: Option<String>,
    offset: Option<f64>,
    /// Estimated duration in seconds.
    estimated_duration: Option<f64>,
}

/// JSON body for `POST /schedule-control`.
#[derive(Debug, Deserialize)]
struct ScheduleControlRequest {
    run_id: Option<String>,
    codebase: Option<String>,
    main_branch_revision: Option<String>,
    change_set: Option<String>,
    offset: Option<f64>,
    requester: Option<String>,
    refresh: Option<bool>,
    bucket: Option<String>,
    /// Estimated duration in seconds.
    estimated_duration: Option<f64>,
}

/// Response for finishing a run.
#[derive(Debug, Serialize)]
struct FinishResponse {
    /// Run ID.
    id: String,
    /// Uploaded filenames.
    filenames: Vec<String>,
    /// Log filenames.
    logs: Vec<String>,
    /// Artifact names.
    artifacts: Vec<String>,
    /// Result information.
    result: serde_json::Value,
}

/// Extract avoided hosts from configuration: any archive mirror URI
/// that looks restricted/internal has its host added to the block
/// list, so the assignment loop skips codebases hosted there.
fn get_avoided_hosts(config: &janitor::config::Config) -> Vec<String> {
    config
        .distribution
        .iter()
        .filter_map(|d| d.archive_mirror_uri.as_deref())
        .filter(|m| m.contains("restricted") || m.contains("internal"))
        .filter_map(|m| url::Url::parse(m).ok())
        .filter_map(|u| u.host_str().map(str::to_string))
        .collect()
}

/// Create campaign configuration from actual config files and queue item.
fn create_campaign_config(
    queue_item: &QueueItem,
    app_config: &janitor::config::Config,
) -> CampaignConfig {
    // Find the campaign configuration in the loaded config
    let campaign_config = app_config
        .campaign
        .iter()
        .find(|c| c.name() == queue_item.campaign);

    if let Some(config) = campaign_config {
        // Extract build configuration from the campaign. Mirrors
        // Python's runner.py: `extra_build_distribution` is the
        // proto's `repeated string` (used for `--extra-repository`
        // sbuild flags), distinct from the singular `build_distribution`
        // which sets the apt label on the produced package. Earlier
        // the Rust port conflated the two and shipped an empty
        // `extra_build_distribution`, so sbuild only saw `sid`.
        let debian_build = if config.has_debian_build() {
            let db_config = config.debian_build();
            let none_if_empty = |s: &str| -> Option<String> {
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            };
            Some(crate::DebianBuildConfig {
                base_distribution: db_config.base_distribution().to_string(),
                build_distribution: none_if_empty(db_config.build_distribution()),
                build_suffix: none_if_empty(db_config.build_suffix()),
                build_command: none_if_empty(db_config.build_command()),
                chroot: none_if_empty(db_config.chroot()),
                extra_build_distribution: db_config
                    .extra_build_distribution
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            })
        } else {
            None
        };

        let generic_build = if config.has_generic_build() {
            let gb_config = config.generic_build();
            Some(crate::GenericBuildConfig {
                chroot: if gb_config.chroot().is_empty() {
                    None
                } else {
                    Some(gb_config.chroot().to_string())
                },
            })
        } else {
            None
        };

        CampaignConfig {
            generic_build,
            debian_build,
            force_build: config.force_build(),
            default_empty: config.default_empty(),
        }
    } else {
        // Fallback to default configuration if campaign not found
        log::warn!(
            "Campaign '{}' not found in config, using default",
            queue_item.campaign
        );
        CampaignConfig {
            generic_build: Some(crate::GenericBuildConfig { chroot: None }),
            debian_build: None,
            force_build: false,
            default_empty: false,
        }
    }
}

/// Query parameters for `GET /queue/position`
/// (`?codebase=...&campaign=...`).
#[derive(serde::Deserialize)]
struct QueuePositionQuery {
    codebase: String,
    campaign: String,
}

/// `GET /queue/position?codebase=...&campaign=...` -- return the
/// (position, wait_time, cumulative_wait_time) ETA for a specific
/// (codebase, campaign) queue entry.
///
/// `wait_time` divides the cumulative wait by the current number of
/// active runs to yield a per-worker time estimate.
async fn queue_position(
    State(state): State<Arc<AppState>>,
    Query(query): Query<QueuePositionQuery>,
) -> impl IntoResponse {
    let queue = janitor::queue::Queue::new(state.database.pool());
    let eta = match queue.get_position(&query.campaign, &query.codebase).await {
        Ok(eta) => eta,
        Err(e) => {
            log::error!(
                "Failed to get queue position for {}/{}: {}",
                query.codebase,
                query.campaign,
                e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            );
        }
    };

    let active_count = state.active_runs.len().await as i64;
    let cumulative_wait_seconds = eta
        .as_ref()
        .map(|e| e.wait_time.microseconds as f64 / 1_000_000.0);
    let per_run_wait_seconds = match (cumulative_wait_seconds, active_count) {
        (Some(total), n) if n > 0 => Some(total / n as f64),
        (Some(total), _) => Some(total),
        (None, _) => None,
    };

    (
        StatusCode::OK,
        Json(json!({
            "position": eta.as_ref().map(|e| e.position),
            "wait_time": per_run_wait_seconds,
            "cumulative_wait_time": cumulative_wait_seconds,
        })),
    )
}

/// `POST /schedule-control` -- schedule a `control` campaign run for
/// a codebase. Either the body supplies (codebase, main_branch_revision)
/// directly, or a `run_id` from which both are derived.
async fn schedule_control(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ScheduleControlRequest>,
) -> impl IntoResponse {
    let pool = state.database.pool();

    let (codebase, main_branch_revision) = if let Some(run_id) = request.run_id.as_deref() {
        let row: Option<(Option<String>, String)> =
            match sqlx::query_as("SELECT main_branch_revision, codebase FROM run WHERE id = $1")
                .bind(run_id)
                .fetch_optional(pool)
                .await
            {
                Ok(row) => row,
                Err(e) => {
                    log::error!("Failed to find run {}: {}", run_id, e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "Database error"})),
                    );
                }
            };
        match row {
            Some((Some(rev), codebase)) => (codebase, rev),
            Some((None, _)) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"reason": "Run has no main branch revision"})),
                );
            }
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"reason": "Run not found"})),
                );
            }
        }
    } else {
        let codebase = match request.codebase {
            Some(c) => c,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"reason": "missing codebase"})),
                );
            }
        };
        let rev = match request.main_branch_revision {
            Some(r) => r,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"reason": "missing main_branch_revision"})),
                );
            }
        };
        (codebase, rev)
    };

    let estimated_duration = request
        .estimated_duration
        .map(|secs| chrono::Duration::milliseconds((secs * 1000.0) as i64));
    let main_branch_revision = breezyshim::RevisionId::from(main_branch_revision.into_bytes());

    let result = janitor::schedule::do_schedule_control(
        pool,
        &codebase,
        request.change_set.as_deref(),
        Some(&main_branch_revision),
        request.offset,
        request.refresh.unwrap_or(false),
        request.bucket.as_deref(),
        request.requester.as_deref(),
        estimated_duration,
    )
    .await;

    match result {
        Ok((offset, duration, queue_id, bucket)) => (
            StatusCode::OK,
            Json(json!({
                "campaign": "control",
                "offset": offset,
                "bucket": bucket,
                "codebase": codebase,
                "queue_id": queue_id,
                "estimated_duration_seconds": duration.num_seconds() as f64
                    + (duration.subsec_nanos() as f64) / 1e9,
            })),
        ),
        Err(janitor::schedule::Error::CandidateUnavailable { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"reason": "Candidate not available"})),
        ),
        Err(e) => {
            log::error!("do_schedule_control failed for {}: {}", codebase, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Schedule error"})),
            )
        }
    }
}

/// `POST /schedule` -- schedule a regular run for a (campaign,
/// codebase). Either the body provides (campaign, codebase) directly,
/// or a `run_id` from which both are derived. The command is resolved
/// from the candidate row, then from the campaign config, then from
/// the run row, in that order.
async fn schedule(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ScheduleRequest>,
) -> impl IntoResponse {
    let pool = state.database.pool();

    let (campaign, codebase, run_command) = if let Some(run_id) = request.run_id.as_deref() {
        let row: Option<(String, String, Option<String>)> = match sqlx::query_as(
            "SELECT suite AS campaign, codebase, command FROM run WHERE id = $1",
        )
        .bind(run_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row,
            Err(e) => {
                log::error!("Failed to find run {}: {}", run_id, e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "Database error"})),
                );
            }
        };
        match row {
            Some((campaign, codebase, command)) => (campaign, codebase, command),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"reason": "Run not found"})),
                );
            }
        }
    } else {
        let campaign = match request.campaign {
            Some(c) => c,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"reason": "missing campaign"})),
                );
            }
        };
        let codebase = match request.codebase {
            Some(c) => c,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"reason": "missing codebase"})),
                );
            }
        };
        (campaign, codebase, None)
    };

    // Resolve the command in the same order Python does: candidate
    // first, then the campaign config's command, then the run row's
    // command (only set when run_id was supplied).
    let candidate_command: Option<String> = match sqlx::query_scalar(
        "SELECT command FROM candidate WHERE codebase = $1 AND suite = $2",
    )
    .bind(&codebase)
    .bind(&campaign)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            log::error!(
                "Failed to look up candidate command for {}/{}: {}",
                codebase,
                campaign,
                e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            );
        }
    };
    let command = candidate_command
        .or_else(|| {
            state
                .config
                .get_campaign(&campaign)
                .and_then(|c| c.command.clone())
        })
        .or(run_command);
    let command = match command {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"reason": "no command specified"})),
            );
        }
    };

    let estimated_duration = request
        .estimated_duration
        .map(|secs| chrono::Duration::milliseconds((secs * 1000.0) as i64));

    let result = janitor::schedule::do_schedule(
        pool,
        &campaign,
        &codebase,
        request.bucket.as_deref().unwrap_or("default"),
        request.change_set.as_deref(),
        request.offset,
        request.refresh.unwrap_or(false),
        request.requester.as_deref(),
        estimated_duration,
        Some(&command),
    )
    .await;

    match result {
        Ok((offset, duration, queue_id, bucket)) => {
            // Populate queue_position/queue_wait_time so the frontend's
            // "Scheduled new run at position N" success message has real
            // data instead of "position undefined".
            let (queue_position, queue_wait_time) = match state
                .database
                .get_queue_position(&codebase, &campaign)
                .await
            {
                Ok(Some((pos, wait))) => (Some(pos), Some(wait.as_secs_f64())),
                _ => (None, None),
            };
            (
                StatusCode::OK,
                Json(json!({
                    "campaign": campaign,
                    "offset": offset,
                    "bucket": bucket,
                    "codebase": codebase,
                    "queue_id": queue_id,
                    "estimated_duration_seconds": duration.num_seconds() as f64
                        + (duration.subsec_nanos() as f64) / 1e9,
                    "queue_position": queue_position,
                    "queue_wait_time": queue_wait_time,
                })),
            )
        }
        Err(janitor::schedule::Error::CandidateUnavailable { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"reason": "Candidate not available"})),
        ),
        Err(e) => {
            log::error!("do_schedule failed for {}/{}: {}", campaign, codebase, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Schedule error"})),
            )
        }
    }
}

/// `GET /status` -- return the live status of the queue processor as
/// `{processing: [...], avoid_hosts: [...], rate_limit_hosts: {...}}`.
///
/// `processing` is the list of currently-active runs (each in the
/// shape `ActiveRun.to_json()`). We don't track per-run keepalive
/// timestamps, so no keepalive_age / mia / last-keepalive fields are
/// included.
async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let processing: Vec<serde_json::Value> = state
        .active_runs
        .list()
        .await
        .iter()
        .map(|r| r.to_json())
        .collect();

    // Avoid-hosts come from `JANITOR_AVOID_HOSTS` (CSV), or from the
    // runner config at `$RUNNER_CONFIG`; same resolution order as
    // `assign_work_internal`.
    let avoid_hosts: Vec<String> = if let Ok(env) = std::env::var("JANITOR_AVOID_HOSTS") {
        parse_avoid_hosts_csv(&env)
    } else if let Ok(path) = std::env::var("RUNNER_CONFIG") {
        match crate::config::RunnerConfig::from_file(&path) {
            Ok(cfg) => cfg.worker.avoid_hosts,
            Err(e) => {
                log::debug!("status: failed to load runner config {}: {}", path, e);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Rate-limited hosts come from Redis; emit an ISO-8601 timestamp
    // per host so downstream tooling can compare to `now()`.
    let rate_limit_hosts: serde_json::Map<String, serde_json::Value> =
        match state.database.get_rate_limited_hosts().await {
            Ok(map) => map
                .into_iter()
                .map(|(host, until)| (host, serde_json::Value::String(until.to_rfc3339())))
                .collect(),
            Err(e) => {
                log::debug!("status: failed to read rate-limited hosts: {}", e);
                serde_json::Map::new()
            }
        };

    Json(json!({
        "processing": processing,
        "avoid_hosts": avoid_hosts,
        "rate_limit_hosts": rate_limit_hosts,
    }))
}

/// `GET /log/{run_id}` -- list the log files the live worker for
/// this run currently has. Looks up the active run, calls
/// `backchannel.list_log_files()`, and returns the bare list.
/// 404s if the run isn't active.
async fn log_index(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let active_run = match state.active_runs.get(&id).await {
        Some(run) => run,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"reason": format!("No such current run: {}", id)})),
            );
        }
    };
    match active_run.backchannel.list_log_files().await {
        Ok(files) => (StatusCode::OK, Json(json!(files))),
        Err(e) => {
            log::warn!("Failed to list log files for {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to list log files: {}", e)})),
            )
        }
    }
}

/// `GET /active-runs/{run_id}/current-stage` -- query the live
/// worker for which lifecycle stage it's currently in (`setup` /
/// `codemod` / `build` / `push` / `finish` / unset). The site's
/// active-run page uses this to pick which log file to auto-open
/// in the `<details open>` block -- opening `build.log` mid-build
/// and `worker.log` once the worker has moved on to push/finish --
/// so the operator's eye lands on the file currently being
/// written. 404s if the run isn't active. Returns
/// `{"current_stage": "build"}` or `{"current_stage": null}`.
async fn current_stage(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let active_run = match state.active_runs.get(&id).await {
        Some(run) => run,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"reason": format!("No such current run: {}", id)})),
            );
        }
    };
    match active_run.backchannel.get_current_stage().await {
        Ok(stage) => (StatusCode::OK, Json(json!({"current_stage": stage}))),
        Err(e) => {
            log::warn!("Failed to fetch current_stage for {}: {}", id, e);
            (
                StatusCode::OK,
                Json(json!({"current_stage": null, "error": format!("{}", e)})),
            )
        }
    }
}

/// `GET /log/{run_id}/{filename}` -- stream a single log file from
/// the live worker for this run. Returns 400 for filenames
/// containing `/`, 404 when the run isn't active or the file
/// doesn't exist on the worker.
async fn log(
    State(state): State<Arc<AppState>>,
    Path((id, filename)): Path<(String, String)>,
) -> impl IntoResponse {
    if filename.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            [("content-type", "text/plain")],
            format!("Invalid filename {}", filename).into_bytes(),
        );
    }
    let active_run = match state.active_runs.get(&id).await {
        Some(run) => run,
        None => {
            return (
                StatusCode::NOT_FOUND,
                [("content-type", "text/plain")],
                format!("No such current run: {}", id).into_bytes(),
            );
        }
    };
    match active_run.backchannel.get_log_file(&filename).await {
        Ok(content) => (StatusCode::OK, [("content-type", "text/plain")], content),
        Err(crate::PingError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            [("content-type", "text/plain")],
            format!("No such log file: {}", filename).into_bytes(),
        ),
        Err(e) => {
            log::warn!("Failed to get log {} for run {}: {}", filename, id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "text/plain")],
                format!("Failed to read log file: {}", e).into_bytes(),
            )
        }
    }
}

/// `POST /kill/{run_id}` -- terminate a live worker via its
/// backchannel. On success, returns the active run's JSON snapshot;
/// 404 when the run isn't active; 501 when the worker doesn't
/// support kill.
async fn kill(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let active_run = match state.active_runs.get(&id).await {
        Some(run) => run,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"reason": format!("No such current run: {}", id)})),
            );
        }
    };
    let snapshot = active_run.to_json();
    match active_run.backchannel.kill().await {
        Ok(()) => (StatusCode::OK, Json(snapshot)),
        Err(crate::PingError::NotSupported(msg)) => {
            // 501 with this exact text -- existing clients rely on the
            // wording.
            let reason = if msg.is_empty() {
                "kill not supported for this type of run".to_string()
            } else {
                msg
            };
            (StatusCode::NOT_IMPLEMENTED, Json(json!({"error": reason})))
        }
        Err(crate::PingError::NoActiveRun(msg)) => {
            // Worker has no active run (it likely restarted while this
            // run was in progress); map to 410 Gone.
            (StatusCode::GONE, Json(json!({"error": msg})))
        }
        Err(e) => {
            log::error!("Failed to kill run {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to kill run: {}", e)})),
            )
        }
    }
}

/// `GET /codebases` -- return the codebase table as a JSON list.
async fn get_codebases(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.database.get_codebases().await {
        Ok(codebases) => (StatusCode::OK, Json(serde_json::Value::Array(codebases))),
        Err(e) => {
            log::error!("Failed to get codebases: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

/// `POST /codebases` -- bulk upsert codebase rows. Mirrors
/// Best-effort name of the codebase's main branch, for deciding which
/// colocated branches a build needs.
///
/// Python opens the branch and reads `main_branch.name`; the Rust
/// runner never opens it during assignment, so recover the name from
/// the stored branch URL instead -- breezy's `,branch=<name>` segment
/// or a `?branch=<name>` query -- and fall back to "main" when the URL
/// carries no branch at all (the common case for a default branch).
fn main_branch_name(branch_url: Option<&str>) -> String {
    const DEFAULT: &str = "main";
    let url = match branch_url {
        Some(u) => u,
        None => return DEFAULT.to_string(),
    };
    for sep in [",branch=", "?branch=", "&branch="] {
        if let Some((_, rest)) = url.split_once(sep) {
            let name = rest.split(['&', ',', '#']).next().unwrap_or("");
            if !name.is_empty() {
                // The value is percent-encoded in the `?branch=` form.
                return percent_encoding::percent_decode_str(name)
                    .decode_utf8_lossy()
                    .into_owned();
            }
        }
    }
    // Debian Vcs-Git style: "https://.../pkg.git -b <branch>".
    if let Some((_, rest)) = url.split_once(" -b ") {
        let name = rest.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    DEFAULT.to_string()
}

/// `POST /codebases` -- bulk upsert codebases. Returns 200 with an
/// empty object on success.
async fn update_codebases(
    State(state): State<Arc<AppState>>,
    Json(codebases): Json<Vec<serde_json::Value>>,
) -> impl IntoResponse {
    match state.database.upload_codebases(&codebases).await {
        Ok(()) => (StatusCode::OK, Json(json!({}))),
        Err(e) => {
            log::error!("Failed to upload codebases: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

/// `DELETE /candidates/{id}` -- delete a candidate row plus its
/// followup entries and any queue items for the same (suite,
/// codebase). Returns 200 with an empty object on success, or 404
/// when no such candidate exists.
async fn delete_candidate(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let candidate_id = match id.parse::<i64>() {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"reason": "Invalid candidate ID"})),
            );
        }
    };

    match state.database.delete_candidate(candidate_id).await {
        Ok(true) => (StatusCode::OK, Json(json!({}))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"reason": "No such candidate"})),
        ),
        Err(e) => {
            log::error!("Failed to delete candidate {}: {}", candidate_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

/// `GET /candidates` -- return the candidate table as a JSON list.
async fn get_candidates(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.database.get_candidates().await {
        Ok(candidates) => (StatusCode::OK, Json(serde_json::Value::Array(candidates))),
        Err(e) => {
            log::error!("Failed to get candidates: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

/// `POST /candidates` -- bulk upsert candidates.
///
/// Each entry is validated against the runner config and then
/// inserted (via `ON CONFLICT DO UPDATE`), with per-candidate
/// fallback behavior:
///
/// - Missing `codebase` or `campaign` -> 400.
/// - Unknown campaign -> collected in `unknown_campaigns`, skipped.
/// - No `command` in entry -> fall back to the campaign config's
///   `command`; if neither is present the entry is rejected into
///   `invalid_command`.
/// - `value == 0` -> rejected into `invalid_value`.
/// - FK violation on `candidate_codebase_fkey` ->
///   `unknown_codebases`. FK violation on
///   `candidate_publish_policy_fkey` -> `unknown_publish_policies`.
///
/// After a successful candidate INSERT we look for an open merge
/// proposal on the same codebase+campaign with a *different*
/// command. If one exists we reschedule with
/// `bucket="update-existing-mp"` and `refresh=true` so the next
/// run refreshes the existing proposal; otherwise we use the
/// caller-supplied bucket.
///
/// `followup_for` entries are inserted into the `followup` table
/// before scheduling.
///
/// Response shape:
/// ```json
/// {
///   "success": [...],
///   "invalid_command": [...],
///   "invalid_value": [...],
///   "unknown_campaigns": [...],
///   "unknown_codebases": [...],
///   "unknown_publish_policies": [...]
/// }
/// ```
/// Outcome of validating a queue assignment in `assign_work_internal`.
/// Abort branches: unknown campaign -> "unknown-campaign",
/// non-default_empty campaign with no branch_url -> "not-in-vcs".
/// Pulled out as a 3-bool decision matrix so all 8 combinations can
/// be exhaustively unit-tested.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AssignmentValidation {
    /// Assignment passes validation; can be handed to a worker.
    Ok,
    /// Campaign is not in the runner config; aborted with result_code
    /// "unknown-campaign".
    UnknownCampaign,
    /// Campaign is not default_empty and the codebase has no
    /// branch_url; aborted with result_code "not-in-vcs".
    NotInVcs,
}

/// Parse a comma-separated list of hosts from the
/// `JANITOR_AVOID_HOSTS` environment variable. Empty entries
/// (consecutive commas or trailing commas) are dropped, and
/// whitespace around each entry is trimmed. Returns an empty Vec
/// when the input is empty or all-whitespace. Pulled out of
/// `assign_work_internal` so the parsing rules can be exhaustively
/// tested without touching the env.
pub(crate) fn parse_avoid_hosts_csv(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Pure decision matrix for assignment validation. Inputs:
///
/// - `campaign_known`: whether the queue item's campaign appears
///   in the runner config
/// - `default_empty`: the campaign's `default_empty` flag (only
///   meaningful when `campaign_known`)
/// - `has_branch_url`: whether the codebase has a branch_url
pub(crate) fn assignment_validation_outcome(
    campaign_known: bool,
    default_empty: bool,
    has_branch_url: bool,
) -> AssignmentValidation {
    if !campaign_known {
        AssignmentValidation::UnknownCampaign
    } else if !default_empty && !has_branch_url {
        AssignmentValidation::NotInVcs
    } else {
        AssignmentValidation::Ok
    }
}

/// Outcome of the pure preflight validation for a single uploaded
/// candidate. Mirrors the checks at the top of the
/// `upload_candidates` loop, separated out so the per-candidate
/// decision matrix (missing field -> 400; unknown campaign -> skip
/// into `unknown_campaigns`; empty command -> `invalid_command`;
/// `value == 0` -> `invalid_value`) can be unit-tested without a
/// database or runner config.
#[derive(Debug, PartialEq)]
pub(crate) enum CandidatePreflight {
    /// Candidate passed the pure-logic checks. Callers still need
    /// to perform the DB insert and follow-up scheduling.
    Ok {
        codebase: String,
        campaign: String,
        command: String,
        value: Option<i64>,
    },
    /// Request is malformed: `codebase` or `campaign` field missing
    /// or empty. Python raises HTTPBadRequest; the handler returns
    /// 400 with the given message.
    BadRequest(String),
    /// Candidate references a campaign not in the runner config.
    /// Python adds to `unknown_campaigns` and continues.
    UnknownCampaign(String),
    /// Neither the candidate nor the campaign config supplies a
    /// command. Python adds to `invalid_command` and continues.
    InvalidCommand,
    /// `value == 0`. Python adds to `invalid_value` and continues.
    InvalidValue,
}

/// Pure preflight for a single candidate upload entry. Takes the
/// candidate's JSON value plus a view into the runner config:
///
/// - `known_campaigns`: set of campaigns known to the runner
/// - `campaign_default_command`: looks up the config-level
///   fallback command for a campaign name, returning `None` if
///   absent or empty
pub(crate) fn candidate_preflight(
    candidate: &serde_json::Value,
    known_campaigns: &std::collections::HashSet<String>,
    campaign_default_command: impl Fn(&str) -> Option<String>,
) -> CandidatePreflight {
    let codebase = match candidate.get("codebase").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return CandidatePreflight::BadRequest(format!(
                "no codebase field for candidate {}",
                candidate
            ));
        }
    };
    let campaign = match candidate.get("campaign").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return CandidatePreflight::BadRequest(format!(
                "no campaign field for candidate {}",
                candidate
            ));
        }
    };

    if !known_campaigns.contains(&campaign) {
        return CandidatePreflight::UnknownCampaign(campaign);
    }

    let command = candidate
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .or_else(|| campaign_default_command(&campaign).filter(|s| !s.is_empty()));
    let command = match command {
        Some(c) => c,
        None => return CandidatePreflight::InvalidCommand,
    };

    let value = candidate.get("value").and_then(|v| v.as_i64());
    if value == Some(0) {
        return CandidatePreflight::InvalidValue;
    }

    CandidatePreflight::Ok {
        codebase,
        campaign,
        command,
        value,
    }
}

async fn upload_candidates(
    State(state): State<Arc<AppState>>,
    Json(candidates): Json<Vec<serde_json::Value>>,
) -> Response {
    let pool = state.database.pool();

    let mut success = Vec::<serde_json::Value>::new();
    let mut unknown_codebases = std::collections::BTreeSet::<String>::new();
    let mut unknown_campaigns = std::collections::BTreeSet::<String>::new();
    let mut invalid_command = std::collections::BTreeSet::<String>::new();
    let mut invalid_value = std::collections::BTreeSet::<i64>::new();
    let mut unknown_publish_policies = std::collections::BTreeSet::<String>::new();

    let known_campaign_names: std::collections::HashSet<String> = state
        .config
        .campaign
        .iter()
        .filter_map(|c| c.name.clone())
        .collect();

    for candidate in &candidates {
        let (codebase, campaign, command, value) =
            match candidate_preflight(candidate, &known_campaign_names, |name| {
                state
                    .config
                    .get_campaign(name)
                    .and_then(|c| c.command.clone())
                    .filter(|s| !s.is_empty())
            }) {
                CandidatePreflight::Ok {
                    codebase,
                    campaign,
                    command,
                    value,
                } => (codebase, campaign, command, value),
                CandidatePreflight::BadRequest(msg) => {
                    return (StatusCode::BAD_REQUEST, msg).into_response();
                }
                CandidatePreflight::UnknownCampaign(campaign) => {
                    log::warn!("unknown campaign {:?}", campaign);
                    unknown_campaigns.insert(campaign);
                    continue;
                }
                CandidatePreflight::InvalidCommand => {
                    log::warn!("No command in candidate or campaign config");
                    invalid_command.insert(String::new());
                    continue;
                }
                CandidatePreflight::InvalidValue => {
                    log::warn!("invalid value for candidate");
                    invalid_value.insert(0);
                    continue;
                }
            };

        let publish_policy = candidate
            .get("publish-policy")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let change_set = candidate
            .get("change_set")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let context = candidate
            .get("context")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let success_chance = candidate.get("success_chance").and_then(|v| v.as_f64());

        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                log::error!("Failed to begin transaction: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "Database error"})),
                )
                    .into_response();
            }
        };

        let insert_result = sqlx::query_scalar::<_, i32>(
            r#"
            INSERT INTO candidate
            (suite, command, change_set, context, value, success_chance, publish_policy, codebase)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (codebase, suite, coalesce(change_set, ''::text))
            DO UPDATE SET
                context = EXCLUDED.context,
                value = EXCLUDED.value,
                success_chance = EXCLUDED.success_chance,
                command = EXCLUDED.command,
                publish_policy = EXCLUDED.publish_policy,
                codebase = EXCLUDED.codebase
            RETURNING id
            "#,
        )
        .bind(&campaign)
        .bind(&command)
        .bind(change_set.as_deref())
        .bind(context.as_deref())
        .bind(value)
        .bind(success_chance)
        .bind(publish_policy.as_deref())
        .bind(&codebase)
        .fetch_one(&mut *tx)
        .await;

        let candidate_id: i32 = match insert_result {
            Ok(id) => id,
            Err(sqlx::Error::Database(db_err))
                if db_err
                    .constraint()
                    .map(|c| c == "candidate_codebase_fkey")
                    .unwrap_or(false) =>
            {
                log::warn!(
                    "ignoring candidate {}/{}; codebase unknown",
                    codebase,
                    campaign
                );
                unknown_codebases.insert(codebase);
                let _ = tx.rollback().await;
                continue;
            }
            Err(sqlx::Error::Database(db_err))
                if db_err
                    .constraint()
                    .map(|c| c == "candidate_publish_policy_fkey")
                    .unwrap_or(false) =>
            {
                log::warn!("unknown publish policy {:?}", publish_policy);
                if let Some(pp) = publish_policy {
                    unknown_publish_policies.insert(pp);
                }
                let _ = tx.rollback().await;
                continue;
            }
            Err(e) => {
                log::error!(
                    "Failed to insert candidate for {}/{}: {}",
                    codebase,
                    campaign,
                    e
                );
                let _ = tx.rollback().await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "Database error"})),
                )
                    .into_response();
            }
        };

        // Detect an existing open MP with a different command.
        let existing_mp = sqlx::query_as::<_, (Option<String>, String)>(
            r#"
            SELECT merge_proposal.url AS mp_url,
                   last_effective_runs.command AS command
            FROM last_effective_runs
            LEFT JOIN merge_proposal
              ON last_effective_runs.revision = merge_proposal.revision
            WHERE merge_proposal.status = 'open'
              AND last_effective_runs.codebase = $1
              AND last_effective_runs.suite = $2
              AND last_effective_runs.command != $3
            "#,
        )
        .bind(&codebase)
        .bind(&campaign)
        .bind(&command)
        .fetch_optional(&mut *tx)
        .await;

        let (bucket_override, refresh, requester): (Option<String>, bool, String) =
            match existing_mp {
                Ok(Some((mp_url, prev_command))) => {
                    if mp_url.is_some() {
                        (
                            Some("update-existing-mp".to_string()),
                            true,
                            format!(
                                "command changed for existing mp: {:?} -> {:?}",
                                prev_command, command
                            ),
                        )
                    } else {
                        (
                            candidate
                                .get("bucket")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            true,
                            format!("command changed: {:?} -> {:?}", prev_command, command),
                        )
                    }
                }
                Ok(None) => (
                    candidate
                        .get("bucket")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    false,
                    "candidate update".to_string(),
                ),
                Err(e) => {
                    log::error!("Failed to look up existing MPs: {}", e);
                    let _ = tx.rollback().await;
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "Database error"})),
                    )
                        .into_response();
                }
            };

        let requester = if let Some(extra) = candidate.get("requester").and_then(|v| v.as_str()) {
            format!("{} {}", requester, extra)
        } else {
            requester
        };

        if let Some(followups) = candidate.get("followup_for").and_then(|v| v.as_array()) {
            for origin in followups {
                if let Some(origin_id) = origin.as_i64() {
                    if let Err(e) = sqlx::query(
                        "INSERT INTO followup (origin, candidate) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    )
                    .bind(origin_id as i32)
                    .bind(candidate_id)
                    .execute(&mut *tx)
                    .await
                    {
                        log::warn!("Failed to insert followup {}->{}: {}", origin_id, candidate_id, e);
                    }
                }
            }
        }

        if let Err(e) = tx.commit().await {
            log::error!("Failed to commit candidate insert: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
                .into_response();
        }

        // Schedule on the pool (do_schedule_regular uses its own
        // connection; Python passed the outer conn but Rust's
        // schedule helpers take &PgPool).
        match janitor::schedule::do_schedule_regular(
            pool,
            &codebase,
            &campaign,
            Some(&command),
            value.map(|v| v as f64),
            success_chance,
            None,
            Some(&requester),
            0.0,
            context.as_deref(),
            change_set.as_deref(),
            false,
            refresh,
            bucket_override.as_deref(),
        )
        .await
        {
            Ok((offset, estimated_duration, queue_id, bucket)) => {
                success.push(json!({
                    "campaign": campaign,
                    "codebase": codebase,
                    "bucket": bucket,
                    "change_set": change_set,
                    "offset": offset,
                    "estimated_duration": estimated_duration.num_seconds(),
                    "queue-id": queue_id,
                    "refresh": refresh,
                }));
            }
            Err(janitor::schedule::Error::CandidateUnavailable { .. }) => {
                // Shouldn't happen -- we just inserted the candidate.
                log::warn!(
                    "candidate unavailable right after insert: {}/{}",
                    codebase,
                    campaign
                );
            }
            Err(e) => {
                log::error!("Failed to schedule {}/{}: {}", codebase, campaign, e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("Schedule error: {}", e)})),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "success": success,
            "invalid_command": invalid_command.into_iter().collect::<Vec<_>>(),
            "invalid_value": invalid_value.into_iter().collect::<Vec<_>>(),
            "unknown_campaigns": unknown_campaigns.into_iter().collect::<Vec<_>>(),
            "unknown_codebases": unknown_codebases.into_iter().collect::<Vec<_>>(),
            "unknown_publish_policies": unknown_publish_policies.into_iter().collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

/// `GET /runs/{run_id}` -- return a run's `{codebase, campaign,
/// publish_status}` triple. Only those three fields are returned,
/// not the entire run row.
async fn get_run(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    let row: Result<Option<(String, String, String)>, _> =
        sqlx::query_as("SELECT codebase, suite, publish_status::text FROM run WHERE id = $1")
            .bind(&id)
            .fetch_optional(state.database.pool())
            .await;

    match row {
        Ok(Some((codebase, campaign, publish_status))) => (
            StatusCode::OK,
            Json(json!({
                "codebase": codebase,
                "campaign": campaign,
                "publish_status": publish_status,
            })),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"reason": format!("no such run: {}", id)})),
        ),
        Err(e) => {
            log::error!("Failed to get run {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

/// `POST /runs/{run_id}` -- set the publish_status of a run, then
/// publish a notification on the Redis `publish-status` channel so
/// other services can react.
async fn update_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<UpdateRunRequest>,
) -> impl IntoResponse {
    match state
        .database
        .update_run_publish_status(&id, &request.publish_status)
        .await
    {
        Ok(Some((run_id, codebase, suite))) => {
            let payload = json!({
                "run_id": run_id,
                "publish_status": request.publish_status,
                "codebase": codebase,
                "campaign": suite,
            });
            // Best-effort fanout to subscribers. A failed publish
            // shouldn't fail the HTTP request -- the DB row is already
            // updated. Alert on `janitor_runner_redis_operations_total
            // {operation="publish_publish_status",status="error"}`.
            if let Err(e) = state.database.publish("publish-status", &payload).await {
                log::error!("Failed to publish publish-status event: {}", e);
                crate::metrics::REDIS_OPERATIONS_TOTAL
                    .with_label_values(&["publish_publish_status", "error"])
                    .inc();
            }
            (StatusCode::OK, Json(payload))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"reason": format!("no such run: {}", id)})),
        ),
        Err(e) => {
            log::error!("Failed to update run {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

async fn get_active_runs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let active_runs = state.active_runs.list().await;
    let runs_json: Vec<_> = active_runs.iter().map(|r| r.to_json()).collect();
    (StatusCode::OK, Json(runs_json))
}

/// `GET /active-runs/{run_id}` -- return a single active run's JSON
/// snapshot. 404s when the run isn't currently active.
async fn get_active_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.active_runs.get(&id).await {
        Some(active_run) => (StatusCode::OK, Json(active_run.to_json())),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"reason": format!("no such run {}", id)})),
        ),
    }
}

/// `GET /active-runs/+peek` -- preview the next assignment without
/// claiming it.
///
/// Returns:
///   * 503 `{reason: "queue empty"}` when there's nothing to assign
///   * 429 `{reason}` + `Retry-After` header when forge-rate-limited
///   * 201 `<assignment dict>` + `Location: /active-runs/{run_id}`
///     header on success
///
/// Currently uses the simplified `next_queue_item_with_rate_limiting`
/// helper, so the success body is the build_config / queue_item /
/// vcs_info shape it produces rather than a full assignment dict.
async fn peek_active_run(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let avoided_hosts = get_avoided_hosts(&state.config);
    match state
        .database
        .next_queue_item_with_rate_limiting(None, None, &avoided_hosts)
        .await
    {
        Ok(Some(assignment)) => {
            let campaign_config = create_campaign_config(&assignment.queue_item, &state.config);
            let build_config = match get_builder(&campaign_config, None, None) {
                Ok(builder) => {
                    let mut config = HashMap::new();
                    config.insert("builder_kind".to_string(), builder.kind().to_string());
                    config
                }
                Err(_) => HashMap::new(),
            };

            (
                StatusCode::CREATED,
                Json(json!({
                    "queue_item": assignment.queue_item.to_json(),
                    "vcs_info": assignment.vcs_info,
                    "build_config": build_config,
                    "estimated_duration": assignment
                        .queue_item
                        .estimated_duration
                        .map(|d| d.as_secs()),
                })),
            )
        }
        Ok(None) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"reason": "queue empty"})),
        ),
        Err(e) => {
            log::error!("Failed to peek queue item: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

/// Query parameters for `GET /queue` (`?limit=N`).
#[derive(serde::Deserialize, Default)]
struct GetQueueQuery {
    limit: Option<i64>,
}

/// `GET /queue` -- return the queued items as
/// `[{queue_id, codebase, campaign, context, command}]`.
async fn get_queue(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GetQueueQuery>,
) -> impl IntoResponse {
    let queue = janitor::queue::Queue::new(state.database.pool());
    match queue.iter_queue(query.limit, None).await {
        Ok(items) => {
            let payload: Vec<serde_json::Value> = items
                .into_iter()
                .map(|entry| {
                    json!({
                        "queue_id": entry.id,
                        "codebase": entry.codebase,
                        "campaign": entry.campaign,
                        "context": entry.context,
                        "command": entry.command,
                    })
                })
                .collect();
            (StatusCode::OK, Json(serde_json::Value::Array(payload)))
        }
        // The database rejects a limit that is not a valid row count.
        Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("2201W") => (
            StatusCode::BAD_REQUEST,
            Json(json!({"reason": db_err.message()})),
        ),
        Err(e) => {
            log::error!("Failed to iterate queue: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Database error"})),
            )
        }
    }
}

async fn metrics() -> impl IntoResponse {
    match MetricsCollector::collect_metrics() {
        Ok(metrics) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
            metrics,
        ),
        Err(e) => {
            log::error!("Failed to collect metrics: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "text/plain")],
                "Failed to collect metrics".to_string(),
            )
        }
    }
}

/// Public endpoint to list workers with basic information.
async fn list_workers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.auth_service.list_workers().await {
        Ok(workers) => {
            let active_runs = state.active_runs.list().await;
            let active_workers: std::collections::HashSet<&str> =
                active_runs.iter().map(|r| r.worker_name.as_str()).collect();

            let last_seen_times = state
                .database
                .get_workers_last_seen()
                .await
                .unwrap_or_default();

            let worker_infos: Vec<serde_json::Value> = workers
                .iter()
                .map(|worker| {
                    let status = if active_workers.contains(worker.name.as_str()) {
                        "active"
                    } else {
                        "idle"
                    };

                    let last_seen = last_seen_times.get(&worker.name).copied();

                    let mut worker_info = json!({
                        "name": worker.name,
                        "status": status,
                        // Don't expose worker link in public endpoint
                    });

                    // Only include last_seen if we have the data
                    if let Some(last_seen_time) = last_seen {
                        worker_info["last_seen"] = json!(last_seen_time);
                    }

                    worker_info
                })
                .collect();

            let active_count = active_workers.len();
            let total_count = workers.len();
            let idle_count = total_count - active_count;

            Json(json!({
                "workers": worker_infos,
                "total_workers": total_count,
                "active_workers": active_count,
                "idle_workers": idle_count,
                "summary": {
                    "total": total_count,
                    "active": active_count,
                    "idle": idle_count,
                },
                "timestamp": chrono::Utc::now()
            }))
        }
        Err(e) => {
            log::error!("Failed to list workers: {}", e);
            Json(json!({
                "error": "Failed to list workers",
                "workers": [],
                "total_workers": 0,
                "active_workers": 0,
                "idle_workers": 0
            }))
        }
    }
}

/// Admin endpoint to list workers.
async fn admin_list_workers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.auth_service.list_workers().await {
        Ok(workers) => {
            let active_runs = state.active_runs.list().await;
            let active_workers: std::collections::HashSet<&str> =
                active_runs.iter().map(|r| r.worker_name.as_str()).collect();

            let last_seen_times = state
                .database
                .get_workers_last_seen()
                .await
                .unwrap_or_default();

            // Workers not seen in the last 30 minutes are considered failed.
            let failed_workers = state
                .database
                .get_failed_workers(30)
                .await
                .unwrap_or_default();
            let failed_count = failed_workers.len();

            let worker_infos: Vec<serde_json::Value> = workers
                .iter()
                .map(|worker| {
                    let status = if failed_workers.contains(&worker.name) {
                        "failed"
                    } else if active_workers.contains(worker.name.as_str()) {
                        "active"
                    } else {
                        "idle"
                    };

                    let last_seen = last_seen_times
                        .get(&worker.name)
                        .copied()
                        .unwrap_or_else(chrono::Utc::now);

                    json!({
                        "name": worker.name,
                        "link": worker.link,
                        "status": status,
                        "last_seen": last_seen,
                    })
                })
                .collect();

            let active_count = active_workers.len();
            let total_count = workers.len();
            let idle_count = total_count - active_count - failed_count;

            Json(json!({
                "workers": worker_infos,
                "total_workers": total_count,
                "active_workers": active_count,
                "idle_workers": idle_count,
                "summary": {
                    "total": total_count,
                    "active": active_count,
                    "idle": idle_count,
                    "failed": failed_count
                },
                "timestamp": chrono::Utc::now()
            }))
        }
        Err(e) => {
            log::error!("Failed to list workers: {}", e);
            Json(json!({
                "error": "Failed to list workers",
                "workers": [],
                "total_workers": 0,
                "active_workers": 0,
                "idle_workers": 0
            }))
        }
    }
}

/// Admin endpoint to create a worker.
#[derive(Deserialize)]
struct CreateWorkerRequest {
    name: String,
    password: String,
    link: Option<String>,
}

async fn admin_create_worker(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateWorkerRequest>,
) -> impl IntoResponse {
    match state
        .auth_service
        .create_worker(&request.name, &request.password, request.link.as_deref())
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            Json(MessageResponse {
                message: "Worker created successfully".to_string(),
            }),
        ),
        Err(e) => {
            log::error!("Failed to create worker: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(MessageResponse {
                    message: "Failed to create worker".to_string(),
                }),
            )
        }
    }
}

/// Admin endpoint to delete a worker.
async fn admin_delete_worker(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match state.auth_service.delete_worker(&name).await {
        Ok(true) => Json(MessageResponse {
            message: "Worker deleted successfully".to_string(),
        }),
        Ok(false) => Json(MessageResponse {
            message: "Worker not found".to_string(),
        }),
        Err(e) => {
            log::error!("Failed to delete worker: {}", e);
            Json(MessageResponse {
                message: "Failed to delete worker".to_string(),
            })
        }
    }
}

/// Admin endpoint to get security statistics.
async fn admin_security_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.security_service.get_security_stats().await {
        Ok(stats) => match serde_json::to_value(stats) {
            Ok(value) => Json(value),
            Err(e) => {
                log::error!("Failed to serialize security stats: {}", e);
                Json(json!({"error": "Failed to serialize stats"}))
            }
        },
        Err(e) => {
            log::error!("Failed to get security stats: {}", e);
            Json(json!({"error": "Failed to get security stats"}))
        }
    }
}

/// `POST /admin/runs/cleanup` -- reschedule (and optionally delete)
/// run rows that match a result-code (and optional time range /
/// campaign) filter. For each matched row, a fresh queue entry is
/// inserted at high priority for the same (codebase, suite, command).
/// When `delete` is truthy, the `last_run` / `run.resume_from` FK
/// references are cleared and the row is deleted.
///
/// Query parameters:
/// * `result_code` (required) -- exact result code to match.
/// * `campaign` (optional) -- restrict to a single suite.
/// * `description_like` (optional) -- `ILIKE` pattern to match against
///   `run.description`. The caller supplies the wildcards (`%foo%`),
///   matching SQL semantics; needed because a single result_code
///   covers multiple bug families (e.g. `result-code=lintian` spans
///   both genuine lintian failures and the `invalid type: map`
///   deserialise bug fixed in #90).
/// * `min_finish_time` / `max_finish_time` (optional, RFC 3339
///   timestamps) -- bound the finish_time range, half-open
///   `[min, max)` semantics.
/// * `delete` (optional, default `false`) -- when truthy
///   (`1`/`true`/`yes`/`on`), also clear FK references and DELETE the
///   matched run rows. Browsers omit unchecked checkboxes, so the
///   cupboard form's default submission reschedules without deleting.
/// * `dry_run=1` -- return the matched run IDs without modifying
///   anything. Useful to confirm what would be touched.
/// * `max=N` -- cap batch size (default 1000, hard cap 5000).
#[derive(serde::Deserialize)]
struct CleanupRunsQuery {
    result_code: String,
    #[serde(default)]
    campaign: Option<String>,
    #[serde(default)]
    description_like: Option<String>,
    #[serde(default)]
    min_finish_time: Option<String>,
    #[serde(default)]
    max_finish_time: Option<String>,
    #[serde(default)]
    delete: Option<String>,
    #[serde(default)]
    dry_run: Option<String>,
    #[serde(default)]
    max: Option<i64>,
}

async fn admin_cleanup_runs(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(q): axum::extract::Query<CleanupRunsQuery>,
) -> impl IntoResponse {
    let dry_run = q
        .dry_run
        .as_deref()
        .map(|s| !matches!(s, "" | "0" | "false" | "no"))
        .unwrap_or(false);
    // Default is false -- deletion is opt-in. Browsers omit unchecked
    // checkboxes from form submissions, so `delete` arrives as None
    // unless the admin explicitly ticked the box.
    let delete = q
        .delete
        .as_deref()
        .map(|s| !matches!(s, "" | "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    let max = q.max.unwrap_or(1000).clamp(1, 5000);

    // Parse the optional timestamp bounds up front so we can fail
    // fast with a 400 instead of a 500 from sqlx.
    let parse_ts =
        |s: &str| chrono::DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&chrono::Utc));
    let min_ts = match q.min_finish_time.as_deref().map(parse_ts).transpose() {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid min_finish_time (expect RFC 3339): {}", e)
                })),
            );
        }
    };
    let max_ts = match q.max_finish_time.as_deref().map(parse_ts).transpose() {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid max_finish_time (expect RFC 3339): {}", e)
                })),
            );
        }
    };

    // Build the WHERE clause incrementally so optional filters drop
    // out cleanly. `run.finish_time` is TIMESTAMP (no TZ) on the
    // schema; cast to timestamptz for the comparison so the bound
    // we received in UTC matches the stored value's intent.
    let mut sql = String::from(
        "SELECT id, codebase, suite::text AS suite, command \
         FROM run \
         WHERE result_code = $1",
    );
    let mut next_param = 2;
    if q.campaign.is_some() {
        sql.push_str(&format!(" AND suite::text = ${}", next_param));
        next_param += 1;
    }
    if q.description_like.is_some() {
        sql.push_str(&format!(" AND description ILIKE ${}", next_param));
        next_param += 1;
    }
    if min_ts.is_some() {
        sql.push_str(&format!(
            " AND (finish_time AT TIME ZONE 'UTC') >= ${}",
            next_param
        ));
        next_param += 1;
    }
    if max_ts.is_some() {
        sql.push_str(&format!(
            " AND (finish_time AT TIME ZONE 'UTC') < ${}",
            next_param
        ));
        next_param += 1;
    }
    sql.push_str(&format!(" ORDER BY finish_time ASC LIMIT ${}", next_param));

    let mut query =
        sqlx::query_as::<_, (String, String, String, String)>(sqlx::AssertSqlSafe(&*sql))
            .bind(&q.result_code);
    if let Some(ref c) = q.campaign {
        query = query.bind(c);
    }
    if let Some(ref d) = q.description_like {
        query = query.bind(d);
    }
    if let Some(ts) = min_ts {
        query = query.bind(ts);
    }
    if let Some(ts) = max_ts {
        query = query.bind(ts);
    }
    query = query.bind(max);

    let rows: Vec<(String, String, String, String)> =
        match query.fetch_all(state.database.pool()).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("cleanup-runs SELECT failed: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("select failed: {}", e)})),
                );
            }
        };

    if rows.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "matched": 0,
                "rescheduled": 0,
                "deleted": 0,
                "dry_run": dry_run,
                "result_code": q.result_code,
            })),
        );
    }

    let run_ids: Vec<String> = rows.iter().map(|(id, ..)| id.clone()).collect();

    if dry_run {
        return (
            StatusCode::OK,
            Json(json!({
                "matched": rows.len(),
                "rescheduled": 0,
                "deleted": 0,
                "dry_run": true,
                "result_code": q.result_code,
                "run_ids": run_ids,
            })),
        );
    }

    let mut tx = match state.database.pool().begin().await {
        Ok(t) => t,
        Err(e) => {
            log::error!("cleanup-runs begin tx failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("tx begin failed: {}", e)})),
            );
        }
    };

    // Reschedule each (codebase, suite, command). The queue table
    // has a unique index on `(codebase, suite, coalesce(change_set,
    // ''))`. Use ON CONFLICT so a re-run is idempotent: bump priority
    // on an existing queue row instead of failing.
    let requester = format!("cleanup:{}", q.result_code);
    let mut rescheduled = 0i64;
    for (_id, codebase, suite, command) in rows.iter() {
        let r = sqlx::query(
            "INSERT INTO queue (codebase, suite, command, priority, bucket, requester) \
             VALUES ($1, $2::suite_name, $3, 0, 'reschedule', $4) \
             ON CONFLICT (codebase, suite, coalesce(change_set, '')) DO UPDATE \
             SET priority = LEAST(queue.priority, 0), \
                 bucket = 'reschedule', \
                 requester = EXCLUDED.requester",
        )
        .bind(codebase)
        .bind(suite)
        .bind(command)
        .bind(&requester)
        .execute(&mut *tx)
        .await;
        match r {
            Ok(res) => rescheduled += res.rows_affected() as i64,
            Err(e) => {
                log::warn!(
                    "cleanup-runs: failed to reschedule {} ({}/{}): {}",
                    codebase,
                    suite,
                    command,
                    e
                );
                let _ = tx.rollback().await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("reschedule failed for {}: {}", codebase, e)})),
                );
            }
        }
    }

    let deleted = if delete {
        // Clear FK references that block the DELETE. None of the FKs on
        // `last_run` declare ON DELETE behaviour. We're re-queuing the
        // codebases anyway, so dropping the last_run pointers is the
        // right semantic -- a fresh run will repopulate them. Same for
        // `run.resume_from` self-references.
        for (col, label) in [
            ("last_run_id", "last_run_id"),
            ("last_effective_run_id", "last_effective_run_id"),
            ("last_unabsorbed_run_id", "last_unabsorbed_run_id"),
        ] {
            let stmt = format!("UPDATE last_run SET {} = NULL WHERE {} = ANY($1)", col, col);
            if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(&*stmt))
                .bind(&run_ids)
                .execute(&mut *tx)
                .await
            {
                log::error!("cleanup-runs: clear last_run.{} failed: {}", label, e);
                let _ = tx.rollback().await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("clear {} failed: {}", label, e)})),
                );
            }
        }
        if let Err(e) = sqlx::query("UPDATE run SET resume_from = NULL WHERE resume_from = ANY($1)")
            .bind(&run_ids)
            .execute(&mut *tx)
            .await
        {
            log::error!("cleanup-runs: clear run.resume_from failed: {}", e);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("clear resume_from failed: {}", e)})),
            );
        }

        // Per-run-id child rows in tables that FK to `run.id` without
        // an `ON DELETE CASCADE` clause. `new_result_branch` already
        // cascades, and `run.resume_from` is `ON DELETE SET NULL`,
        // so neither needs a hand DELETE; the tables below do.
        for (table, fk_col, label) in [
            ("debian_build", "run_id", "debian_build"),
            ("review", "run_id", "review"),
            ("followup", "origin", "followup"),
        ] {
            let stmt = format!("DELETE FROM {} WHERE {} = ANY($1)", table, fk_col);
            if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(&*stmt))
                .bind(&run_ids)
                .execute(&mut *tx)
                .await
            {
                log::error!("cleanup-runs: delete {} children failed: {}", label, e);
                let _ = tx.rollback().await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("delete {} children failed: {}", label, e)})),
                );
            }
        }

        match sqlx::query("DELETE FROM run WHERE id = ANY($1)")
            .bind(&run_ids)
            .execute(&mut *tx)
            .await
        {
            Ok(r) => r.rows_affected() as i64,
            Err(e) => {
                log::error!("cleanup-runs DELETE failed: {}", e);
                let _ = tx.rollback().await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("delete failed: {}", e)})),
                );
            }
        }
    } else {
        0
    };

    if let Err(e) = tx.commit().await {
        log::error!("cleanup-runs commit failed: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("commit failed: {}", e)})),
        );
    }

    log::info!(
        "cleanup-runs: result_code={}, campaign={:?}, matched={}, rescheduled={}, deleted={}",
        q.result_code,
        q.campaign,
        rows.len(),
        rescheduled,
        deleted
    );

    (
        StatusCode::OK,
        Json(json!({
            "matched": rows.len(),
            "rescheduled": rescheduled,
            "deleted": deleted,
            "dry_run": false,
            "result_code": q.result_code,
        })),
    )
}

/// Check for resume information for a campaign and branch.
async fn check_resume_info(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ResumeInfoRequest>,
) -> impl IntoResponse {
    match state
        .resume_service
        .check_resume_result(&request.campaign, &request.resume_revision)
        .await
    {
        Ok(Some(resume_info)) => Json(json!({
            "resume_available": true,
            "resume_info": resume_info
        })),
        Ok(None) => Json(json!({
            "resume_available": false,
            "message": "No resume information found"
        })),
        Err(e) => {
            log::error!("Failed to check resume info: {}", e);
            Json(json!({
                "error": "Failed to check resume information"
            }))
        }
    }
}

/// Get the resume chain for a specific run.
async fn get_resume_chain(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    match state.resume_service.get_resume_chain(&run_id).await {
        Ok(chain) => Json(json!({
            "run_id": run_id,
            "resume_chain": chain
        })),
        Err(e) => {
            log::error!("Failed to get resume chain for {}: {}", run_id, e);
            Json(json!({
                "error": "Failed to get resume chain"
            }))
        }
    }
}

/// Get all runs that resume from a specific run.
async fn get_resume_descendants(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    match state.resume_service.get_resume_descendants(&run_id).await {
        Ok(descendants) => Json(json!({
            "run_id": run_id,
            "descendants": descendants
        })),
        Err(e) => {
            log::error!("Failed to get resume descendants for {}: {}", run_id, e);
            Json(json!({
                "error": "Failed to get resume descendants"
            }))
        }
    }
}

/// Validate resume consistency across the database.
async fn validate_resume_consistency(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.resume_service.validate_resume_consistency().await {
        Ok(errors) => {
            if errors.is_empty() {
                Json(json!({
                    "status": "consistent",
                    "message": "All resume relationships are valid"
                }))
            } else {
                Json(json!({
                    "status": "inconsistent",
                    "errors": errors
                }))
            }
        }
        Err(e) => {
            log::error!("Failed to validate resume consistency: {}", e);
            Json(json!({
                "error": "Failed to validate resume consistency"
            }))
        }
    }
}

/// Body for the 409 response that `finish_run` returns when the run
/// has already been persisted. Private handler returns
/// `{id, result, reason}` and public handler returns `{id, reason}`;
/// worker/tooling clients pattern-match on these fields.
fn finish_conflict_body(
    run_id: &str,
    janitor_result: &crate::JanitorResult,
    public: bool,
) -> serde_json::Value {
    let reason = "This run was already stored by a previous /finish call. \
                  Treat this as terminal -- do not retry.";
    if public {
        json!({"id": run_id, "reason": reason})
    } else {
        json!({
            "id": run_id,
            "result": janitor_result.to_json(),
            "reason": reason,
        })
    }
}

async fn finish_active_run_multipart(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    multipart: Multipart,
) -> impl IntoResponse {
    finish_run_multipart_internal(state, id, multipart, false).await
}

/// Publish the side effects that follow a committed `finish_run`:
/// pub/sub `result`, drop the in-memory active-run entry, drop the
/// queue-item assignment, pub/sub `queue`, bump the last-success
/// Prometheus gauge, and spawn a follow-up
/// [`janitor::schedule::do_schedule_regular`] for the same
/// `(codebase, campaign)`. Errors are logged but not propagated: the
/// run has already been persisted and the caller's HTTP response
/// should reflect that, not a transient pub/sub or follow-up
/// scheduling failure.
async fn publish_finish_events(
    state: &Arc<AppState>,
    run_id: &str,
    janitor_result: &crate::JanitorResult,
    queue_id: i64,
) {
    // The DB writes already committed. If any of these Redis side
    // effects fails we can't roll back, so we log at error, bump the
    // redis-operations counter, and continue -- dashboards/alerts
    // should key off `janitor_runner_redis_operations_total{status=
    // "error"}`. Python surfaces the same failures as a 500 back to
    // the worker; Rust deliberately swallows them because the run is
    // already persisted and the worker would just get 409 on retry.
    if let Err(e) = state
        .database
        .publish("result", &janitor_result.to_json())
        .await
    {
        log::error!("Failed to publish result event for {}: {}", run_id, e);
        crate::metrics::REDIS_OPERATIONS_TOTAL
            .with_label_values(&["publish_result", "error"])
            .inc();
    }

    state.active_runs.remove(run_id).await;

    if let Err(e) = state.database.unassign_queue_item(queue_id).await {
        log::error!("Failed to unassign queue item from Redis: {}", e);
        crate::metrics::REDIS_OPERATIONS_TOTAL
            .with_label_values(&["unassign_queue_item", "error"])
            .inc();
    }

    let processing: Vec<serde_json::Value> = state
        .active_runs
        .list()
        .await
        .iter()
        .map(|r| r.to_json())
        .collect();
    let status_payload = serde_json::json!({
        "processing": processing,
        "avoid_hosts": [],
        "rate_limit_hosts": {},
    });
    if let Err(e) = state.database.publish("queue", &status_payload).await {
        log::error!("Failed to publish queue status event: {}", e);
        crate::metrics::REDIS_OPERATIONS_TOTAL
            .with_label_values(&["publish_queue_status", "error"])
            .inc();
    }

    crate::metrics::LAST_SUCCESS_GAUGE.set(chrono::Utc::now().timestamp() as f64);

    // Spawn a follow-up regular schedule for the same codebase +
    // campaign. CandidateUnavailable is non-fatal: a one-off schedule
    // may have had no underlying candidate, or the candidate may have
    // been removed since this run started.
    let pool = state.database.pool().clone();
    let codebase = janitor_result.codebase.clone();
    let campaign = janitor_result.campaign.clone();
    let change_set = janitor_result.change_set.clone();
    let context_str = janitor_result
        .context
        .as_ref()
        .and_then(|v| v.as_str().map(|s| s.to_string()));
    let log_id_owned = run_id.to_string();
    // Also pair a control run. A control (campaign="control",
    // bucket="control") rebuilds the codebase at the same main
    // branch revision with no codemod applied, which is what the
    // publisher later needs to compute a debdiff. We pre-schedule it
    // here so the diff is ready by the time the MR review happens,
    // rather than discovering it's missing mid-publish and scheduling
    // it reactively. Only worth doing for successful runs that
    // actually produced a main_branch_revision -- without a revision
    // there's nothing to compare against.
    let result_code = janitor_result.code.clone();
    let main_branch_revision = janitor_result.main_branch_revision.clone();
    tokio::spawn(async move {
        match janitor::schedule::do_schedule_regular(
            &pool,
            &codebase,
            &campaign,
            None,
            None,
            None,
            None,
            Some("after run schedule"),
            0.0,
            context_str.as_deref(),
            change_set.as_deref(),
            false,
            false,
            None,
        )
        .await
        {
            Ok(_) => {}
            Err(janitor::schedule::Error::CandidateUnavailable { .. }) => {
                log::debug!(
                    "not rescheduling {}/{} after {}: no candidate available",
                    codebase,
                    campaign,
                    log_id_owned
                );
            }
            Err(e) => {
                log::warn!(
                    "Failed to schedule follow-up for {}/{} after {}: {}",
                    codebase,
                    campaign,
                    log_id_owned,
                    e
                );
            }
        }

        // Skip if the run itself is a control campaign (no point
        // producing a control-of-control) or if it didn't succeed /
        // has no base revision to rebuild against.
        if campaign != "control"
            && campaign != "unchanged"
            && result_code == "success"
            && main_branch_revision.is_some()
        {
            match janitor::schedule::do_schedule_control(
                &pool,
                &codebase,
                change_set.as_deref(),
                main_branch_revision.as_ref(),
                None,
                false,
                Some("control"),
                Some("after successful run"),
                None,
            )
            .await
            {
                Ok(_) => {}
                Err(janitor::schedule::Error::CandidateUnavailable { .. }) => {
                    log::debug!(
                        "no control candidate for {} after {}",
                        codebase,
                        log_id_owned
                    );
                }
                Err(e) => {
                    log::warn!(
                        "Failed to schedule control for {} after {}: {}",
                        codebase,
                        log_id_owned,
                        e
                    );
                }
            }
        }
    });
}

async fn finish_run_multipart_internal(
    state: Arc<AppState>,
    run_id: String,
    multipart: Multipart,
    public: bool,
) -> (StatusCode, Json<serde_json::Value>) {
    let active_run = match state.active_runs.get(&run_id).await {
        Some(run) => run,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"reason": format!("no such run {}", run_id)})),
            );
        }
    };

    // Process multipart upload. Pass the codebase so logs land in
    // the `{root}/{codebase}/{run_id}/<name>` layout the site's
    // FileSystemLogFileManager expects.
    let uploaded_result = match state
        .upload_processor
        .process_upload(multipart, &run_id, &active_run.codebase)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            log::error!("Failed to process upload for run {}: {}", run_id, e);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Upload processing failed: {}", e)})),
            );
        }
    };

    // Extract worker result and builder result. The worker's wire
    // format puts build metadata under `target: {name, details}`
    // not under `builder_result`, so when `extract_builder_result`
    // comes back `None` we fall back to converting `target`. See
    // `BuilderResult::from_target_details` for the schema mapping
    // and the regression it fixes (per-run page wrongly saying
    // "this run did not produce a build" on successful runs).
    let worker_result = uploaded_result.worker_result.clone();
    let builder_result = match state
        .upload_processor
        .extract_builder_result(&uploaded_result)
    {
        Ok(Some(br)) => Some(br),
        Ok(None) => worker_result
            .target
            .as_ref()
            .and_then(BuilderResult::from_target_details),
        Err(e) => {
            log::warn!("Failed to extract builder result: {}", e);
            worker_result.builder_result.clone().or_else(|| {
                worker_result
                    .target
                    .as_ref()
                    .and_then(BuilderResult::from_target_details)
            })
        }
    };

    let mut janitor_result = active_run.create_result(
        worker_result.code.clone(),
        worker_result.description.clone(),
    );

    janitor_result.codemod = worker_result.codemod;
    janitor_result.main_branch_revision = worker_result.main_branch_revision;
    janitor_result.revision = worker_result.revision;
    janitor_result.value = worker_result.value.map(|v| v as u64);
    janitor_result.branches = worker_result.branches;
    janitor_result.tags = worker_result.tags;
    janitor_result.remotes = worker_result.remotes.map(|remotes| {
        remotes
            .into_iter()
            .map(|(name, data)| {
                let url = data.get("url").and_then(|u| u.as_str()).unwrap_or_default();
                (
                    name,
                    crate::ResultRemote {
                        url: url.to_string(),
                    },
                )
            })
            .collect()
    });
    janitor_result.failure_details = worker_result.details;
    janitor_result.failure_stage = worker_result
        .stage
        .as_deref()
        .map(|s| s.split('/').map(|p| p.to_string()).collect());
    janitor_result.builder_result = builder_result;
    janitor_result.transient = worker_result.transient;
    janitor_result.target_branch_url = worker_result.target_branch_url;
    janitor_result.branch_url = worker_result
        .branch_url
        .unwrap_or(janitor_result.branch_url);
    janitor_result.vcs_type = worker_result.vcs_type;
    janitor_result.subpath = worker_result.subpath;

    let log_filenames: Vec<String> = uploaded_result
        .log_files
        .iter()
        .map(|f| f.filename.clone())
        .collect();
    janitor_result.logfilenames = log_filenames.clone();

    // Atomically insert the `run` row and related result state. See
    // the multipart-finish branch above for why AlreadyStored surfaces
    // as 409 Conflict.
    match state
        .database
        .finish_run(
            &mut janitor_result,
            &active_run.command,
            active_run.instigated_context.as_ref(),
            active_run.queue_id,
        )
        .await
    {
        Ok(crate::database::FinishOutcome::Stored) => {}
        Ok(crate::database::FinishOutcome::AlreadyStored) => {
            log::info!(
                "Run {} already stored; replying 409 so the worker stops retrying",
                run_id
            );
            return (
                StatusCode::CONFLICT,
                Json(finish_conflict_body(&run_id, &janitor_result, public)),
            );
        }
        Err(e) => {
            log::error!("Failed to store run result: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"reason": format!("Failed to store result: {}", e)})),
            );
        }
    }

    // Set resume information if this run resumed from another
    if let Some(ref resume_from_id) = active_run.resume_from {
        if let Err(e) = state
            .resume_service
            .set_resume_from(&run_id, resume_from_id)
            .await
        {
            log::warn!("Failed to set resume information for run {}: {}", run_id, e);
            // Continue anyway, main result was stored
        }
    }

    // The ArtifactManager expects a directory containing all artifacts,
    // so stage them into a temp dir before storing.
    if !uploaded_result.artifact_files.is_empty() || !uploaded_result.build_files.is_empty() {
        let temp_dir = std::env::temp_dir().join(format!("janitor-artifacts-{}", &run_id));
        let artifacts_dir = temp_dir.join("artifacts");
        if let Err(e) = tokio::fs::create_dir_all(&artifacts_dir).await {
            log::warn!("Failed to create artifacts directory: {}", e);
        } else {
            for artifact_file in &uploaded_result.artifact_files {
                let dest = artifacts_dir.join(&artifact_file.filename);
                if let Err(e) = tokio::fs::copy(&artifact_file.stored_path, &dest).await {
                    log::warn!(
                        "Failed to copy artifact {} to artifacts dir: {}",
                        artifact_file.filename,
                        e
                    );
                }
            }

            for build_file in &uploaded_result.build_files {
                let dest = artifacts_dir.join(&build_file.filename);
                if let Err(e) = tokio::fs::copy(&build_file.stored_path, &dest).await {
                    log::warn!(
                        "Failed to copy build file {} to artifacts dir: {}",
                        build_file.filename,
                        e
                    );
                }
            }

            // Store all artifacts at once
            if let Err(e) = state
                .artifact_manager
                .store_artifacts(&run_id, &artifacts_dir, None)
                .await
            {
                log::warn!("Failed to store artifacts for run {}: {}", run_id, e);
            }
        }
    }

    for log_file in &uploaded_result.log_files {
        let path_str = match log_file.stored_path.to_str() {
            Some(path) => path,
            None => {
                log::warn!("Invalid UTF-8 in log file path: {:?}", log_file.stored_path);
                continue;
            }
        };

        if let Err(e) = state
            .log_manager
            .import_log(
                &active_run.codebase,
                &run_id,
                path_str,
                None,
                Some(&log_file.filename),
            )
            .await
        {
            log::warn!(
                "Failed to store log file {} from run {}: {}",
                log_file.filename,
                run_id,
                e
            );
        }
    }

    // Publish the completed result to subscribers and tear down the
    // active-run state.
    publish_finish_events(&state, &run_id, &janitor_result, active_run.queue_id).await;
    crate::metrics::MetricsCollector::set_active_runs(
        &active_run.worker_name,
        state
            .active_runs
            .count_for_worker(&active_run.worker_name)
            .await as i64,
    );
    crate::metrics::RUNS_COMPLETED_TOTAL
        .with_label_values(&[
            &janitor_result.campaign,
            &janitor_result.code,
            &active_run.worker_name,
        ])
        .inc();

    // Python's `filenames` is every uploaded part (logs + artifacts +
    // build files); `logs` is just the log entries; `artifacts` is
    // just the build/artifact entries.
    let artifact_filenames: Vec<String> = uploaded_result
        .artifact_files
        .iter()
        .chain(uploaded_result.build_files.iter())
        .map(|f| f.filename.clone())
        .collect();
    let all_filenames: Vec<String> = log_filenames
        .iter()
        .cloned()
        .chain(artifact_filenames.iter().cloned())
        .collect();

    let response = FinishResponse {
        id: run_id,
        filenames: all_filenames,
        logs: log_filenames,
        artifacts: artifact_filenames,
        result: janitor_result.to_json(),
    };

    log::info!(
        "Successfully processed multipart upload for run {}",
        response.id
    );
    match serde_json::to_value(response) {
        Ok(json) => (StatusCode::CREATED, Json(json)),
        Err(e) => {
            log::error!("Failed to serialize finish response: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Serialization failed"})),
            )
        }
    }
}

async fn public_root() -> impl IntoResponse {
    ""
}

/// Authentication middleware for worker endpoints.
async fn authenticate_worker(
    State(state): State<Arc<AppState>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    // Extract authorization header
    let auth_header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());

    if let Some(auth_value) = auth_header {
        // Use authenticate_worker which handles both Bearer and Basic auth
        match state.auth_service.authenticate_worker(auth_value).await {
            Ok(worker_auth) => {
                // Track worker activity
                if let Err(e) = state
                    .database
                    .track_worker_activity(&worker_auth.name)
                    .await
                {
                    log::warn!(
                        "Failed to track worker activity for {}: {}",
                        worker_auth.name,
                        e
                    );
                }

                // Add worker name to request extensions
                req.extensions_mut().insert(worker_auth.name);
                Ok(next.run(req).await)
            }
            Err(_) => {
                log::warn!("Invalid worker credentials provided");
                Err(StatusCode::UNAUTHORIZED)
            }
        }
    } else {
        log::warn!("No authorization header provided for worker endpoint");
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn public_assign(
    State(state): State<Arc<AppState>>,
    Extension(worker_name): Extension<String>,
    Json(request): Json<AssignRequest>,
) -> impl IntoResponse {
    // Worker identity comes from `authenticate_worker` middleware, not
    // from the request body. Workers can't lie about who they are.
    assign_work_internal(state, worker_name, request).await
}

/// Unauthenticated assign for the private (intra-cluster) app. No
/// `Extension<String>` from `authenticate_worker`, so identity comes
/// from the body. Required field -- no fallback to "unknown" (that
/// fallback led to FK 500s on /finish when the made-up name wasn't
/// registered).
///
/// Auth middleware on the public route validates the worker against
/// the `worker` table; this private route has no middleware so we
/// re-validate here. Otherwise an admin tool with a typo'd worker
/// name gets a useful run assignment, does the work, then trips
/// `run_worker_fkey` at /finish -- by which time the worker has
/// already burned the time and the surface is a database 500.
async fn private_assign(
    State(state): State<Arc<AppState>>,
    Json(request): Json<AssignRequest>,
) -> impl IntoResponse {
    let worker_name = match request.worker.as_deref() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "missing_worker",
                    "reason": "Set `worker` (or `node`) on the body. \
                               This route does not authenticate the caller, \
                               so identity must be supplied explicitly.",
                })),
            )
                .into_response();
        }
    };
    match state.auth_service.worker_exists(&worker_name).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "worker_not_registered",
                    "worker": worker_name,
                    "reason": "Worker name is not in the `worker` table. \
                               Register it via POST /admin/workers before \
                               assigning runs (otherwise /finish would \
                               fail later with a FK violation).",
                })),
            )
                .into_response();
        }
        Err(e) => {
            log::error!("private_assign: worker_exists lookup failed: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "worker_lookup_failed"})),
            )
                .into_response();
        }
    }
    assign_work_internal(state, worker_name, request)
        .await
        .into_response()
}

/// Abort a queue assignment by recording a failed run with the given
/// result code. Used by the validation retry loop: when a pulled
/// queue item has an unknown campaign or no branch_url for a
/// non-default_empty campaign, we record the rejection in the `run`
/// table (which also removes the row from `queue`) and then go back
/// to pull the next item.
///
/// All errors are swallowed to a `log::warn!`: if we fail to
/// persist the rejection, the broken queue item may reappear on the
/// next loop iteration, but we'll still hit the retry cap and bail
/// out with a useful 503 rather than looping forever.
async fn abort_assignment(
    state: &Arc<AppState>,
    assignment: &crate::QueueAssignment,
    code: &str,
    description: &str,
) {
    let now = Utc::now();
    let log_id = Uuid::new_v4().to_string();
    let mut result = crate::JanitorResult {
        log_id,
        branch_url: assignment.vcs_info.branch_url.clone().unwrap_or_default(),
        subpath: assignment.vcs_info.subpath.clone(),
        code: code.to_string(),
        transient: Some(false),
        codebase: assignment.queue_item.codebase.clone(),
        campaign: assignment.queue_item.campaign.clone(),
        description: Some(description.to_string()),
        codemod: None,
        value: None,
        logfilenames: Vec::new(),
        start_time: now,
        finish_time: now,
        revision: None,
        main_branch_revision: None,
        change_set: assignment.queue_item.change_set.clone(),
        tags: None,
        remotes: None,
        branches: None,
        failure_details: None,
        failure_stage: None,
        resume: None,
        target: None,
        worker_name: None,
        vcs_type: assignment.vcs_info.vcs_type.clone(),
        target_branch_url: None,
        context: assignment.queue_item.context.clone(),
        builder_result: None,
    };
    if let Err(e) = state
        .database
        .finish_run(
            &mut result,
            &assignment.queue_item.command,
            assignment.queue_item.context.as_ref(),
            assignment.queue_item.id,
        )
        .await
    {
        log::warn!(
            "Failed to record aborted assignment for queue item {} ({}): {}",
            assignment.queue_item.id,
            code,
            e
        );
    }
}

/// Outcome of `compute_resume_from`. Carries the resume metadata (if
/// any) and a forge rate-limit signal so the caller can record it in
/// Redis via `rate_limit_host`.
struct ResumeOutcome {
    /// Full resume metadata for the worker (run_id, previous result,
    /// per-role branches, resume-branch URL). Populated only when a
    /// resume candidate was found.
    resume: Option<ResumeAssignment>,
    rate_limit: Option<(String, Option<f64>)>,
}

/// Resume information as returned in the assign response's `resume`
/// field: `{run_id, result, branch_url, branches}`.
#[derive(Debug, Clone, Serialize)]
struct ResumeAssignment {
    run_id: String,
    /// Previous run's `result` JSON blob.
    result: Option<serde_json::Value>,
    /// URL of the resume branch as opened via the forge or the VCS
    /// manager fallback.
    branch_url: String,
    /// Per-role result branches:
    /// `[(role, remote_name, base_revision, revision), ...]`.
    branches: Vec<(String, Option<String>, Option<String>, Option<String>)>,
}

/// Resume-branch lookup. Opens the main branch, asks the forge (via
/// silver_platter) for a previously proposed branch matching the
/// campaign's branch name, and if found looks up a prior successful
/// run keyed on (campaign, tip revision). Returns the run_id to
/// resume from (or `None` if no resume is available) plus an optional
/// `(host, retry_after)` pair when the forge is rate-limiting us:
/// the caller feeds that into `RunnerDatabase::rate_limit_host` so
/// subsequent assignments skip the host. Non-rate-limit errors are
/// swallowed since resume is best-effort.
async fn compute_resume_from(
    state: &Arc<AppState>,
    assignment: &crate::QueueAssignment,
) -> ResumeOutcome {
    let empty = || ResumeOutcome {
        resume: None,
        rate_limit: None,
    };
    let Some(branch_url) = assignment.vcs_info.branch_url.as_deref() else {
        return empty();
    };
    if branch_url.is_empty() {
        return empty();
    }
    let url = match url::Url::parse(branch_url) {
        Ok(u) => u,
        Err(e) => {
            log::warn!("Invalid branch_url {}: {}", branch_url, e);
            return empty();
        }
    };

    let campaign_name = assignment.queue_item.campaign.clone();
    let codebase = assignment.queue_item.codebase.clone();

    // Look up the campaign's derived-branch name from protobuf config.
    let campaign_branch_name = state
        .config
        .campaign
        .iter()
        .find(|c| c.name() == campaign_name)
        .map(|c| c.branch_name().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| campaign_name.clone());

    // Blocking forge work: open main branch + find_existing_proposed.
    // Use silver_platter::vcs::open_branch (not breezyshim's
    // open_as_generic) so the 429-detection baked into
    // BranchOpenError::from_err fires on the initial open too; then
    // hand off to open_resume_branch which surfaces its own
    // ResumeLookup::RateLimited for forge-lookup errors.
    let forge_campaign_branch_name = campaign_branch_name.clone();
    let forge_codebase = codebase.clone();
    let open_url = url.clone();
    enum BlockingResult {
        /// (resume-branch revision, resume-branch URL as reported by the forge)
        Found(String, String),
        NotFound,
        RateLimited {
            host: String,
            retry_after: Option<f64>,
        },
    }
    let open_fut = tokio::task::spawn_blocking(move || {
        use silver_platter::vcs::BranchOpenError;
        let main_branch = match silver_platter::vcs::open_branch(&open_url, None, None, None) {
            Ok(b) => b,
            Err(BranchOpenError::RateLimited {
                url,
                description,
                retry_after,
            }) => {
                log::warn!(
                    "Rate-limited opening main branch for {}: {}",
                    url,
                    description
                );
                let host = url
                    .host_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| url.to_string());
                return BlockingResult::RateLimited { host, retry_after };
            }
            Err(e) => {
                log::debug!("Failed to open main branch {}: {}", open_url, e);
                return BlockingResult::NotFound;
            }
        };
        match crate::resume::open_resume_branch(
            &main_branch,
            &forge_campaign_branch_name,
            &forge_codebase,
        ) {
            crate::resume::ResumeLookup::Found(b) => {
                use breezyshim::branch::Branch as _;
                let rev = b.last_revision().to_string();
                let url = b.get_user_url().to_string();
                BlockingResult::Found(rev, url)
            }
            crate::resume::ResumeLookup::NotFound => BlockingResult::NotFound,
            crate::resume::ResumeLookup::RateLimited { host, retry_after } => {
                BlockingResult::RateLimited { host, retry_after }
            }
        }
    });

    let (forge_result, rate_limit) =
        match tokio::time::timeout(std::time::Duration::from_secs(60), open_fut).await {
            Ok(Ok(BlockingResult::Found(rev, url))) => (Some((rev, url)), None),
            Ok(Ok(BlockingResult::NotFound)) => (None, None),
            Ok(Ok(BlockingResult::RateLimited { host, retry_after })) => {
                (None, Some((host, retry_after)))
            }
            Ok(Err(e)) => {
                log::warn!("spawn_blocking for open_resume_branch panicked: {}", e);
                (None, None)
            }
            Err(_) => {
                log::warn!(
                    "Timeout opening resume branch via forge for {}/{}",
                    assignment.queue_item.codebase,
                    assignment.queue_item.campaign,
                );
                (None, None)
            }
        };
    // If the forge rate-limited us, short-circuit: don't bother with
    // the VCS-manager fallback (same codebase, same rate limit) and
    // don't look up a resume run. Hand the signal back so the caller
    // can hit rate_limit_host and abort the assignment.
    if let Some(rl) = rate_limit {
        return ResumeOutcome {
            resume: None,
            rate_limit: Some(rl),
        };
    }

    // Fallback: if the forge didn't turn up a resume branch, ask the
    // public VCS manager for `<campaign>/main` on this codebase.
    let (resume_revision, resume_branch_url) = if let Some((rev, br_url)) = forge_result {
        (rev, br_url)
    } else {
        let Some(vcs_type_str) = assignment.vcs_info.vcs_type.as_deref() else {
            return empty();
        };
        let vcs_type: crate::vcs::VcsType = match vcs_type_str.parse() {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "Unsupported vcs {} for resume branch of {}: {}",
                    vcs_type_str,
                    assignment.queue_item.codebase,
                    e,
                );
                return empty();
            }
        };
        let vcs_branch_name = format!("{}/main", campaign_name);
        let open =
            state
                .vcs_manager
                .open_branch_with_metrics(vcs_type, &codebase, &vcs_branch_name);
        let branch = match tokio::time::timeout(std::time::Duration::from_secs(30), open).await {
            Ok(Ok(Some(b))) => b,
            Ok(Ok(None)) => return empty(),
            Ok(Err(e)) => {
                log::debug!(
                    "VCS-manager resume fallback failed for {}/{}: {}",
                    codebase,
                    vcs_branch_name,
                    e.description,
                );
                return empty();
            }
            Err(_) => {
                log::warn!(
                    "Timeout opening resume branch via VCS manager for {}/{}",
                    codebase,
                    vcs_branch_name,
                );
                return empty();
            }
        };
        let Ok((rev, br_url)) = tokio::task::spawn_blocking(move || {
            use breezyshim::branch::Branch as _;
            let rev = branch.last_revision().to_string();
            let url = branch.get_user_url().to_string();
            (rev, url)
        })
        .await
        else {
            return empty();
        };
        (rev, br_url)
    };

    // The database stores raw revision strings; breezy's
    // `last_revision()` returns them in the same form. A
    // `revid:`-prefixed value would silently break resume lookups,
    // so treat it as a programming error rather than papering over
    // it.
    assert!(
        !resume_revision.starts_with("revid:"),
        "breezy returned a revid:-prefixed revision: {:?}",
        resume_revision,
    );

    let resume_service = crate::resume::ResumeService::new((*state.database).clone());
    let resume = match resume_service
        .check_resume_result(&assignment.queue_item.campaign, &resume_revision)
        .await
    {
        Ok(Some(info)) => {
            log::info!(
                "Resuming {}/{} from run {}",
                assignment.queue_item.codebase,
                assignment.queue_item.campaign,
                info.run_id,
            );
            Some(ResumeAssignment {
                run_id: info.run_id,
                result: info.result,
                branch_url: resume_branch_url,
                branches: info.result_branches,
            })
        }
        Ok(None) => None,
        Err(e) => {
            log::warn!("check_resume_result failed: {}", e);
            None
        }
    };
    ResumeOutcome {
        resume,
        rate_limit: None,
    }
}

async fn assign_work_internal(
    state: Arc<AppState>,
    worker_name: String,
    request: AssignRequest,
) -> impl IntoResponse {
    // JANITOR_AVOID_HOSTS overrides the config-file `avoid_hosts` list.
    let excluded_hosts: Vec<String> =
        if let Ok(avoid_hosts_env) = std::env::var("JANITOR_AVOID_HOSTS") {
            parse_avoid_hosts_csv(&avoid_hosts_env)
        } else if let Ok(runner_config_path) = std::env::var("RUNNER_CONFIG") {
            // Try to load runner-specific config if available
            match crate::config::RunnerConfig::from_file(&runner_config_path) {
                Ok(runner_config) => runner_config.worker.avoid_hosts,
                Err(e) => {
                    log::warn!(
                        "Failed to load runner config from {}: {}",
                        runner_config_path,
                        e
                    );
                    vec![]
                }
            }
        } else {
            // Default to empty list
            vec![]
        };
    // Pull queue items one at a time and, for each, validate that the
    // campaign is known and (unless `default_empty`) that the codebase
    // has a branch_url. On a failure we finish the run with that
    // result_code -- recording the rejection in the `run` table and
    // removing the row from `queue` -- and then loop to pull the next
    // item.
    //
    // The cap protects against a pathologically broken queue wedging
    // a worker: if we burn through MAX_VALIDATION_RETRIES items in a
    // row, give up and tell the caller the queue is effectively empty.
    const MAX_VALIDATION_RETRIES: usize = 20;
    let mut validation_retries = 0usize;
    let (assignment, log_id) = loop {
        if validation_retries >= MAX_VALIDATION_RETRIES {
            log::warn!(
                "assign: gave up after {} consecutive validation failures",
                MAX_VALIDATION_RETRIES
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "reason": "queue empty",
                    "detail": "only broken queue items available",
                })),
            );
        }

        let assignment = match state
            .database
            .next_queue_item_with_rate_limiting(
                request.codebase.as_deref(),
                request.campaign.as_deref(),
                &excluded_hosts,
            )
            .await
        {
            Ok(Some(assignment)) => assignment,
            Ok(None) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"reason": "queue empty"})),
                );
            }
            Err(e) => {
                log::error!("Failed to get next queue item: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "Database error"})),
                );
            }
        };

        let campaign_cfg_opt = state.config.get_campaign(&assignment.queue_item.campaign);
        let outcome = assignment_validation_outcome(
            campaign_cfg_opt.is_some(),
            campaign_cfg_opt.map(|c| c.default_empty()).unwrap_or(false),
            assignment.vcs_info.branch_url.is_some(),
        );
        match outcome {
            AssignmentValidation::Ok => {}
            AssignmentValidation::UnknownCampaign => {
                log::warn!(
                    "Unable to find details for campaign {:?} on queue item {}; aborting and retrying",
                    assignment.queue_item.campaign,
                    assignment.queue_item.id
                );
                abort_assignment(
                    &state,
                    &assignment,
                    "unknown-campaign",
                    &format!("Campaign {} unknown", assignment.queue_item.campaign),
                )
                .await;
                validation_retries += 1;
                continue;
            }
            AssignmentValidation::NotInVcs => {
                log::warn!(
                    "Queue item {} for {}/{} has no branch_url and campaign is not default_empty; aborting and retrying",
                    assignment.queue_item.id,
                    assignment.queue_item.codebase,
                    assignment.queue_item.campaign
                );
                abort_assignment(
                    &state,
                    &assignment,
                    "not-in-vcs",
                    "No VCS URL known for codebase.",
                )
                .await;
                validation_retries += 1;
                continue;
            }
        }

        // Reserve the queue item in Redis before handing the assignment
        // to a worker. If another worker claimed it first (race between
        // `next_queue_item_with_rate_limiting` and this HSET NX),
        // discard this item and loop.
        let candidate_log_id = Uuid::new_v4().to_string();
        match state
            .database
            .assign_queue_item(assignment.queue_item.id, &worker_name, &candidate_log_id)
            .await
        {
            Ok(()) => break (assignment, candidate_log_id),
            Err(e) => {
                if e.to_string().contains("already assigned") {
                    log::info!(
                        "Queue item {} was already claimed by another worker; retrying",
                        assignment.queue_item.id
                    );
                    validation_retries += 1;
                    continue;
                }
                // Non-conflict Redis error (connection lost etc.) --
                // don't spin forever; fall through without the
                // distributed lock, same as the previous behaviour.
                log::warn!(
                    "Failed to reserve queue item {} in Redis, proceeding without lock: {}",
                    assignment.queue_item.id,
                    e
                );
                break (assignment, candidate_log_id);
            }
        }
    };

    // The worker sends its backchannel as `{"kind": "http"|"jenkins",
    // "url": "..."}` (see janitor-worker::client::get_assignment_raw)
    // but Backchannel's own deserializer expects `{"my_url": ...}` for
    // Polling or `{"my_url": ..., "jenkins": ...}` for Jenkins. Translate
    // the wire format here so we don't silently fall back to
    // Backchannel::None and then fail every health-check with "No
    // backchannel available".
    let backchannel = match request.backchannel.as_ref() {
        Some(bc_json) if bc_json.is_object() => {
            let kind = bc_json.get("kind").and_then(|v| v.as_str());
            let url = bc_json
                .get("url")
                .or_else(|| bc_json.get("my_url"))
                .and_then(|v| v.as_str())
                .map(String::from);
            match (kind, url) {
                (Some("jenkins"), Some(u)) => Backchannel::Jenkins {
                    my_url: u,
                    jenkins: bc_json.get("jenkins").cloned(),
                },
                // Default to polling for `http`, no kind, or anything
                // unknown: the worker's reported url is a polling target.
                (_, Some(u)) => Backchannel::Polling { my_url: u },
                _ => {
                    log::warn!("Invalid backchannel configuration (no url): {:?}", bc_json);
                    Backchannel::default()
                }
            }
        }
        _ => Backchannel::default(),
    };

    // `log_id` was allocated together with the Redis claim above so we
    // don't hand the worker a fresh log id after a lost claim race.
    let start_time = Utc::now();

    // Open the main branch via silver_platter (which classifies 429s
    // as BranchOpenError::RateLimited), contact the forge via
    // silver_platter's find_existing_proposed_classified to locate a
    // previously-proposed branch, then look up a prior successful run
    // keyed on (campaign, tip-revision). All forge work is blocking
    // (PyO3), so we run it on spawn_blocking with a short timeout.
    // Non-rate-limit errors (no forge, no credentials, network
    // failure) are swallowed as "no resume". Rate-limit errors are
    // surfaced so we can record the host and refuse the assignment.
    let resume_outcome = if assignment.queue_item.refresh {
        ResumeOutcome {
            resume: None,
            rate_limit: None,
        }
    } else {
        compute_resume_from(&state, &assignment).await
    };

    if let Some((host, retry_after)) = resume_outcome.rate_limit {
        // Record the host in Redis so `next_queue_item_with_rate_limiting`
        // skips it until the forge-supplied `retry_after`. Fall back
        // to a conservative 30 minutes if the forge didn't include a
        // Retry-After header.
        let wait_secs = retry_after.unwrap_or(1800.0).max(0.0);
        let until = chrono::Utc::now() + chrono::Duration::seconds(wait_secs as i64);
        if let Err(e) = state.database.rate_limit_host(&host, until).await {
            log::warn!("Failed to record rate-limit for host {}: {}", host, e);
        }

        abort_assignment(
            &state,
            &assignment,
            "resume-rate-limited",
            &format!("Forge {} rate-limited us; retry after {}s", host, wait_secs),
        )
        .await;

        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "reason": "rate limited",
                "host": host,
                "retry_after": retry_after,
            })),
        );
    }

    let resume_assignment: Option<ResumeAssignment> = resume_outcome.resume;
    let resume_from: Option<String> = resume_assignment.as_ref().map(|r| r.run_id.clone());

    let active_run = ActiveRun {
        worker_name: worker_name.clone(),
        worker_link: request.worker_link,
        queue_id: assignment.queue_item.id,
        log_id: log_id.clone(),
        start_time,
        finish_time: None,
        estimated_duration: assignment.queue_item.estimated_duration,
        campaign: assignment.queue_item.campaign.clone(),
        change_set: assignment.queue_item.change_set.clone(),
        command: assignment.queue_item.command.clone(),
        backchannel,
        vcs_info: assignment.vcs_info.clone(),
        codebase: assignment.queue_item.codebase.clone(),
        instigated_context: assignment.queue_item.context.clone(),
        resume_from,
    };

    // Store active run in the in-memory store
    state.active_runs.store(active_run.clone()).await;
    crate::metrics::MetricsCollector::set_active_runs(
        &active_run.worker_name,
        state
            .active_runs
            .count_for_worker(&active_run.worker_name)
            .await as i64,
    );

    // Generate build configuration for the worker
    let campaign_config = create_campaign_config(&assignment.queue_item, &state.config);
    let build_config =
        match get_builder(&campaign_config, None, None) {
            Ok(builder) => {
                // Use the database connection for config generation.
                let mut config = HashMap::new();
                config.insert("builder_kind".to_string(), builder.kind().to_string());

                match state
                    .database
                    .get_codebase_config(&assignment.queue_item.codebase)
                    .await
                {
                    Ok(Some(codebase_config)) => {
                        if let Some(ref branch_url) = codebase_config.branch_url {
                            config.insert("branch_url".to_string(), branch_url.clone());
                        }
                        if let Some(ref vcs_type) = codebase_config.vcs_type {
                            config.insert("vcs_type".to_string(), vcs_type.clone());
                        }
                        if let Some(ref subpath) = codebase_config.subpath {
                            config.insert("subpath".to_string(), subpath.clone());
                        }
                    }
                    Ok(None) => {
                        log::warn!(
                            "No codebase config found for: {}",
                            assignment.queue_item.codebase
                        );
                    }
                    Err(e) => {
                        log::warn!("Failed to get codebase config from database: {}", e);
                    }
                }

                // Distribution config comes from the loaded textproto
                // (`Config.distribution`), not the database -- the former DB
                // query here was a stub that always returned None and
                // logged a warning on every worker assignment. Look the
                // distribution up in-memory and copy the fields the worker
                // needs onto the per-assignment config map.
                if let Some(debian_config) = &campaign_config.debian_build {
                    if let Some(dist) = state.config.distribution.iter().find(|d| {
                        d.name.as_deref() == Some(debian_config.base_distribution.as_str())
                    }) {
                        if let Some(ref name) = dist.name {
                            config.insert("distribution".to_string(), name.clone());
                        }
                        if let Some(ref m) = dist.archive_mirror_uri {
                            config.insert("archive_mirror".to_string(), m.clone());
                        }
                        if let Some(ref c) = dist.chroot {
                            config.insert("chroot".to_string(), c.clone());
                        }
                        if let Some(ref v) = dist.vendor {
                            config.insert("vendor".to_string(), v.clone());
                        }
                    } else {
                        log::warn!(
                            "No distribution config found for: {}",
                            debian_config.base_distribution
                        );
                    }
                }

                // Committer comes straight from the textproto (`Config.committer`).
                // We used to also query a `campaign_config` table for a
                // per-campaign override, but that table has never been in
                // schema/state.sql and the query always failed -- logging
                // a warning on every assignment and, worse, dropping back
                // to None on Err instead of the global fallback. There's
                // no `committer` field on `Campaign` in proto/config.proto
                // either, so the per-campaign path was dead. Just use the
                // global committer.
                let committer = if state.config.committer().is_empty() {
                    None
                } else {
                    Some(state.config.committer().to_string())
                };

                // Extract environment variables from command
                let (extra_env, _clean_command) =
                    janitor::utils::splitout_env(&assignment.queue_item.command);

                // Add environment setup with proper committer
                let mut env = crate::committer_env(committer.as_deref());

                // Add extracted environment variables from command
                for (key, value) in extra_env {
                    env.insert(key, value);
                }

                for (key, value) in env {
                    config.insert(format!("env_{}", key), value);
                }

                // Add campaign-specific metadata
                config.insert(
                    "campaign".to_string(),
                    assignment.queue_item.campaign.clone(),
                );
                config.insert(
                    "codebase".to_string(),
                    assignment.queue_item.codebase.clone(),
                );

                if let Some(ref change_set) = assignment.queue_item.change_set {
                    config.insert("change_set".to_string(), change_set.clone());
                }

                config
            }
            Err(e) => {
                log::warn!("Failed to create builder for assignment: {}", e);
                HashMap::new()
            }
        };

    // Return assignment in the flat shape the worker's
    // `janitor::api::worker::Assignment` deserializer expects:
    // top-level id/queue_id/campaign/codebase/branch/target_repository/
    // codemod/build/env/force-build/skip-setup-validation. Anything
    // else from the old wrapped `{queue_item, vcs_info, active_run,
    // build_config}` response is preserved alongside for backwards
    // compatibility with tools that read those keys.
    //
    // See janitor/src/api/worker.rs::Assignment for the exact shape.
    let (extra_env, clean_command) = janitor::utils::splitout_env(&assignment.queue_item.command);

    // Config is protobuf-generated; fields are accessed via methods.
    // `git_location()` returns "" when unset.
    let public_vcs_location = state.config.git_location();
    let target_repo_url = format!(
        "{}/{}",
        public_vcs_location.trim_end_matches('/'),
        assignment.queue_item.codebase
    );

    let cached_url = assignment
        .vcs_info
        .branch_url
        .clone()
        .map(|_| target_repo_url.clone());

    // Colocated branches the worker must fetch alongside the main
    // branch (`upstream`, `pristine-tar`, … for Debian packaging).
    // Python's `runner.py::next_item` asks the builder for these and
    // normalises the result to a list before serialising, because the
    // worker's `Branch::additional_colocated_branches` is a
    // `Vec<String>`, not a map (upstream 6594167a3). The Rust builder
    // likewise returns a `HashMap<name, branch>`, so send its keys.
    let additional_colocated_branches: Option<Vec<String>> =
        match get_builder(&campaign_config, None, None) {
            Ok(builder) => {
                let main_branch = main_branch_name(assignment.vcs_info.branch_url.as_deref());
                let mut names: Vec<String> = builder
                    .additional_colocated_branches(&main_branch)
                    .into_keys()
                    .collect();
                // Stable order so the assignment JSON doesn't churn
                // between requests (HashMap iteration is unordered).
                names.sort();
                Some(names)
            }
            Err(e) => {
                log::warn!(
                    "Failed to create builder for colocated branches: {}; sending none",
                    e
                );
                None
            }
        };

    let branch = json!({
        "cached_url": cached_url,
        "vcs_type": assignment.vcs_info.vcs_type,
        "url": assignment.vcs_info.branch_url,
        "subpath": assignment.vcs_info.subpath.clone().unwrap_or_default(),
        "additional_colocated_branches": additional_colocated_branches,
        "default-empty": campaign_config.default_empty,
    });

    let target_repository = json!({ "url": target_repo_url });

    // Environment merged from config committer + command prefix
    // (DEB_UPDATE_CHANGELOG=auto ...). Copy into both codemod and
    // build environments so each execution context has a consistent
    // view.
    let mut env: HashMap<String, String> = crate::committer_env(Some(state.config.committer()));
    for (k, v) in extra_env {
        env.insert(k, v);
    }

    // The worker's `DebianBuildConfig` reads these hyphen-cased keys,
    // and a missing `build-command` causes
    // `worker/src/debian/build.rs::build` to silently fall through to
    // a no-op (no debian_build row, no debdiffs) -- every run since
    // this Rust port stood up has hit that path.
    //
    // The fields below match `janitor::api::worker::DebianBuildConfig`'s
    // serde-renamed names so the JSON the worker sees deserialises
    // directly.
    #[derive(serde::Serialize)]
    struct DebianBuildAssignment {
        #[serde(rename = "build-distribution")]
        build_distribution: String,
        #[serde(rename = "build-suffix")]
        build_suffix: String,
        // Skipped when None: `build-command` is only sent when
        // either the campaign or the distribution defines it; the
        // worker silently no-ops otherwise.
        #[serde(rename = "build-command", skip_serializing_if = "Option::is_none")]
        build_command: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        chroot: Option<String>,
        #[serde(rename = "build-extra-repositories")]
        extra_repositories: Vec<String>,
    }

    let (build_target, build_cfg) = if let Some(dcfg) = campaign_config.debian_build.as_ref() {
        let dist = state
            .config
            .distribution
            .iter()
            .find(|d| d.name.as_deref() == Some(dcfg.base_distribution.as_str()));

        let build_assignment = DebianBuildAssignment {
            // Python: `campaign_config.debian_build.build_distribution
            //          or campaign_config.name`.
            build_distribution: dcfg
                .build_distribution
                .clone()
                .unwrap_or_else(|| assignment.queue_item.campaign.clone()),
            build_suffix: dcfg.build_suffix.clone().unwrap_or_default(),
            build_command: dcfg
                .build_command
                .clone()
                .or_else(|| dist.and_then(|d| d.build_command.clone())),
            chroot: dcfg
                .chroot
                .clone()
                .or_else(|| dist.and_then(|d| d.chroot.clone())),
            // When the runner has a `--public-apt-archive-location`,
            // expand each campaign's `extra_build_distribution` (plus
            // a `cs/{change_set}` entry, if any) into a fully formed
            // `deb [trusted=yes] {url} {suite} main` line. The worker
            // writes those verbatim into
            // `/etc/apt/sources.list.d/sbuild-extra-repositories.list`;
            // passing bare campaign names instead produces
            // `Malformed line 1 in source list ... (type)` and aborts
            // the build. Empty list when the URL isn't set.
            extra_repositories: state
                .public_apt_archive_location
                .as_deref()
                .map(|base| {
                    let trimmed = base.trim_end_matches('/');
                    let mut suites: Vec<String> = dcfg.extra_build_distribution.clone();
                    if let Some(cs) = assignment.queue_item.change_set.as_deref() {
                        suites.push(format!("cs/{}", cs));
                    }
                    suites
                        .into_iter()
                        .map(|suite| format!("deb [trusted=yes] {} {} main", trimmed, suite))
                        .collect()
                })
                .unwrap_or_default(),
        };

        (
            "debian",
            serde_json::to_value(&build_assignment).expect("DebianBuildAssignment is plain data"),
        )
    } else {
        ("generic", json!({}))
    };

    let codemod = json!({
        "command": clean_command,
        "environment": env,
    });

    let build = json!({
        "target": build_target,
        "config": build_cfg,
        "environment": env,
    });

    let body = json!({
        "id": active_run.log_id,
        "queue_id": assignment.queue_item.id,
        "campaign": assignment.queue_item.campaign,
        "codebase": assignment.queue_item.codebase,
        "force-build": campaign_config.force_build,
        "branch": branch,
        "resume": resume_assignment,
        "target_repository": target_repository,
        "skip-setup-validation": false,
        "codemod": codemod,
        "env": env,
        "build": build,
        // Retain the legacy keys so nothing that already reads the
        // wrapped shape (e.g. test fixtures, logs) breaks silently.
        "queue_item": assignment.queue_item,
        "vcs_info": assignment.vcs_info,
        "active_run": active_run.to_json(),
        "build_config": build_config,
    });

    (StatusCode::CREATED, Json(body))
}

async fn public_finish(
    State(state): State<Arc<AppState>>,
    Extension(worker_name): Extension<String>,
    Path(id): Path<String>,
    multipart: Multipart,
) -> impl IntoResponse {
    // Worker credentials are verified by authentication middleware
    // Verify that this worker is authorized to finish this specific run
    match state.active_runs.get(&id).await {
        Some(active_run) => {
            if active_run.worker_name != worker_name {
                log::warn!(
                    "Worker {} attempted to finish run {} assigned to worker {}",
                    worker_name,
                    id,
                    active_run.worker_name
                );
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "Not authorized to finish this run"})),
                );
            }
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Run not found"})),
            );
        }
    }

    // Workers POST a multipart body (metadata field + one `file`
    // part per result file) to /finish.
    log::info!("Worker {} finishing run {} (multipart)", worker_name, id);
    finish_run_multipart_internal(state, id, multipart, true).await
}

async fn public_finish_multipart(
    State(state): State<Arc<AppState>>,
    Extension(worker_name): Extension<String>,
    Path(id): Path<String>,
    multipart: Multipart,
) -> impl IntoResponse {
    // Worker credentials are verified by authentication middleware
    // Verify that this worker is authorized to finish this specific run
    match state.active_runs.get(&id).await {
        Some(active_run) => {
            if active_run.worker_name != worker_name {
                log::warn!(
                    "Worker {} attempted to finish run {} assigned to worker {}",
                    worker_name,
                    id,
                    active_run.worker_name
                );
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "Not authorized to finish this run"})),
                );
            }
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Run not found"})),
            );
        }
    }

    log::info!(
        "Worker {} finishing run {} with multipart upload",
        worker_name,
        id
    );
    finish_run_multipart_internal(state, id, multipart, true).await
}

async fn public_get_active_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Returns the full ActiveRun.json() shape (same as the private
    // endpoint) and 404s on missing -- there's no "public view"
    // stripping.
    match state.active_runs.get(&id).await {
        Some(active_run) => (StatusCode::OK, Json(active_run.to_json())),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"reason": format!("no such run {}", id)})),
        ),
    }
}

/// Get watchdog health information for all active runs.
async fn public_watchdog_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let watchdog_config = crate::WatchdogConfig::default();
    let watchdog = crate::Watchdog::new(
        Arc::clone(&state.database),
        state.active_runs.clone(),
        watchdog_config,
    );

    match watchdog.get_detailed_health_status().await {
        Ok(health_statuses) => {
            // Filter to public information only
            let public_statuses: Vec<_> = health_statuses.into_iter().map(|status| {
                json!({
                    "log_id": status.log_id,
                    "worker_name": status.worker_name,
                    "start_time": status.start_time,
                    "estimated_duration": status.estimated_duration.map(|d| d.as_secs()),
                    "failure_count": status.failure_count,
                    "max_failures": status.max_failures,
                    "alive": status.health.as_ref().map(|h| h.alive).unwrap_or(false),
                    "status": status.health.as_ref().map(|h| h.status.clone()).unwrap_or_else(|| "unknown".to_string()),
                    "last_ping": status.health.as_ref().and_then(|h| h.last_ping),
                })
            }).collect();

            Json(json!({
                "status": "ok",
                "active_runs": public_statuses.len(),
                "health_statuses": public_statuses
            }))
        }
        Err(e) => {
            log::error!("Failed to get watchdog health status: {}", e);
            Json(json!({
                "status": "error",
                "error": "Failed to get health status"
            }))
        }
    }
}

/// Get public queue statistics.
async fn public_queue_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Active runs live in Redis, not Postgres; source them here so
    // the count reflects reality.
    let active_runs = state.active_runs.len().await as i64;
    match state.database.get_queue_stats().await {
        Ok(stats) => Json(json!({
            "queue_length": stats.get("total").unwrap_or(&0),
            "active_runs": active_runs,
            "succeeded": stats.get("succeeded").unwrap_or(&0),
            "failed": stats.get("failed").unwrap_or(&0),
            "status": "operational"
        })),
        Err(e) => {
            log::error!("Failed to get queue stats: {}", e);
            Json(json!({
                "status": "error",
                "error": "Database error"
            }))
        }
    }
}

/// `GET /health` -- full component health report.
async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let report = state.health_checker.report().await;
    let code = match report.status {
        crate::ServiceHealthStatus::Healthy | crate::ServiceHealthStatus::Degraded => {
            StatusCode::OK
        }
        crate::ServiceHealthStatus::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
    };
    (code, Json(report))
}

/// `GET /health/live` -- cheap liveness probe: the process is
/// responsive if we can service the request at all.
async fn liveness(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    (StatusCode::OK, "alive")
}

/// `GET /health/ready` (and `/ready`) -- readiness probe: service is
/// ready iff every component reports healthy.
async fn readiness(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.health_checker.is_ready().await {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// Axum middleware: record request count and duration by
/// method/path/status.
async fn record_http_metrics(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use std::time::Instant;
    let start = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    let response = next.run(req).await;
    let status = response.status().as_u16().to_string();

    crate::metrics::HTTP_REQUEST_DURATION
        .with_label_values(&[&method, &path])
        .observe(start.elapsed().as_secs_f64());
    crate::metrics::HTTP_REQUESTS_TOTAL
        .with_label_values(&[&method, &path, &status])
        .inc();

    response
}

/// Create a router for the public API endpoints.
pub fn public_app(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let public_routes = Router::new()
        .route("/", get(public_root))
        .route("/health", get(health))
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .route("/queue/stats", get(public_queue_stats))
        .route("/watchdog/health", get(public_watchdog_health));

    // axum's default request body limit is 2 MiB. Worker /finish
    // uploads bundle the metadata JSON, all logs, and every artifact
    // (.changes, .deb, orig.tar.gz, debian.tar.xz, .dsc, .buildinfo)
    // -- easily hundreds of MB for any real package. Hitting the cap
    // lands as a 5ms 400 with "Multipart error: Failed to read file
    // data: Error parsing `multipart/form-data` request" the moment
    // axum starts decoding the first file part. Disable the limit
    // on the upload routes; the per-file size cap is enforced by
    // the upload processor itself (`max_file_size`), and the ingress
    // proxy-body-size is the network-edge ceiling.
    // The `/runner/` prefix that workers use is an ingress concern,
    // not a routing concern in this binary. nginx matches
    // `/runner(/|$)(.*)` on the public site and rewrites to `/$2`
    // before forwarding to this port, so handlers see the same
    // shapes as the private app -- just gated by authenticate_worker.
    let worker_routes = Router::new()
        .route("/active-runs", post(public_assign))
        .route(
            "/active-runs/{id}/finish",
            post(public_finish).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/active-runs/{id}/finish-multipart",
            post(public_finish_multipart).layer(DefaultBodyLimit::disable()),
        )
        .route("/active-runs/{id}", get(public_get_active_run))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            authenticate_worker,
        ));

    // Combine both routers
    public_routes.merge(worker_routes).with_state(state)
}

/// Create a router for the private API endpoints.
pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/queue/position", get(queue_position))
        .route("/schedule-control", post(schedule_control))
        .route("/schedule", post(schedule))
        .route("/status", get(status))
        .route("/log/{id}", get(log_index))
        .route("/kill/{id}", post(kill))
        .route("/log/{id}/{filename}", get(log))
        .route("/codebases", get(get_codebases))
        .route("/codebases", post(update_codebases))
        .route("/candidates", get(get_candidates))
        .route("/candidates", post(upload_candidates))
        .route("/candidates/{id}", delete(delete_candidate))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}", post(update_run))
        .route("/active-runs", get(get_active_runs))
        // Unauthenticated assign for intra-cluster admin tools (e.g.
        // schedule-codebases CronJob). Body must include `worker`/
        // `node`. Workers themselves should hit /runner/active-runs
        // on the public app, which uses authenticated identity.
        .route("/active-runs", post(private_assign))
        .route("/active-runs/{id}", get(get_active_run))
        .route("/active-runs/{id}/current-stage", get(current_stage))
        // Workers POST `/active-runs/{id}/finish` with a multipart
        // body (metadata field + one `file` part per result file).
        // The `-multipart` alias exists for callers that use the
        // explicit path.
        .route(
            "/active-runs/{id}/finish",
            post(finish_active_run_multipart).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/active-runs/{id}/finish-multipart",
            post(finish_active_run_multipart).layer(DefaultBodyLimit::disable()),
        )
        .route("/active-runs/+peek", get(peek_active_run))
        .route("/queue", get(get_queue))
        .route("/health", get(health))
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .route("/ready", get(readiness))
        .route("/metrics", get(metrics))
        .route("/workers", get(list_workers))
        .route("/admin/workers", get(admin_list_workers))
        .route("/admin/workers", post(admin_create_worker))
        .route("/admin/workers/{name}", delete(admin_delete_worker))
        .route("/admin/security/stats", get(admin_security_stats))
        .route("/admin/runs/cleanup", post(admin_cleanup_runs))
        // Resume-related endpoints
        .route("/resume/check", post(check_resume_info))
        .route("/resume/chain/{run_id}", get(get_resume_chain))
        .route("/resume/descendants/{run_id}", get(get_resume_descendants))
        .route("/resume/validate", get(validate_resume_consistency))
        .layer(axum::middleware::from_fn(record_http_metrics))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::{
        assignment_validation_outcome, candidate_preflight, main_branch_name, AssignmentValidation,
        CandidatePreflight,
    };
    use serde_json::json;
    use std::collections::HashSet;

    /// Exhaustive 2x2x2 matrix for assignment_validation_outcome:
    ///   - unknown campaign always loses (regardless of other inputs)
    ///   - default_empty=true accepts even when no branch_url
    ///   - default_empty=false requires a branch_url
    #[test]
    fn test_assignment_validation_unknown_campaign_always_loses() {
        for default_empty in [false, true] {
            for has_branch_url in [false, true] {
                assert_eq!(
                    assignment_validation_outcome(false, default_empty, has_branch_url),
                    AssignmentValidation::UnknownCampaign,
                    "campaign_known=false default_empty={} has_branch_url={}",
                    default_empty,
                    has_branch_url,
                );
            }
        }
    }

    #[test]
    fn test_assignment_validation_default_empty_accepts_without_branch() {
        assert_eq!(
            assignment_validation_outcome(true, true, false),
            AssignmentValidation::Ok
        );
    }

    #[test]
    fn test_assignment_validation_default_empty_accepts_with_branch() {
        assert_eq!(
            assignment_validation_outcome(true, true, true),
            AssignmentValidation::Ok
        );
    }

    #[test]
    fn test_assignment_validation_non_default_empty_requires_branch() {
        assert_eq!(
            assignment_validation_outcome(true, false, false),
            AssignmentValidation::NotInVcs
        );
    }

    #[test]
    fn test_assignment_validation_non_default_empty_with_branch_is_ok() {
        assert_eq!(
            assignment_validation_outcome(true, false, true),
            AssignmentValidation::Ok
        );
    }

    #[test]
    fn test_parse_avoid_hosts_csv_simple() {
        assert_eq!(
            super::parse_avoid_hosts_csv("github.com,gitlab.com"),
            vec!["github.com".to_string(), "gitlab.com".to_string()],
        );
    }

    #[test]
    fn test_parse_avoid_hosts_csv_trims_whitespace() {
        assert_eq!(
            super::parse_avoid_hosts_csv(" github.com , gitlab.com  "),
            vec!["github.com".to_string(), "gitlab.com".to_string()],
        );
    }

    #[test]
    fn test_parse_avoid_hosts_csv_drops_empty_entries() {
        assert_eq!(
            super::parse_avoid_hosts_csv("github.com,,gitlab.com,"),
            vec!["github.com".to_string(), "gitlab.com".to_string()],
        );
    }

    #[test]
    fn test_parse_avoid_hosts_csv_empty_input() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(super::parse_avoid_hosts_csv(""), empty);
        assert_eq!(super::parse_avoid_hosts_csv("   "), empty);
        assert_eq!(super::parse_avoid_hosts_csv(",,,"), empty);
    }

    #[test]
    fn test_parse_avoid_hosts_csv_single_entry() {
        assert_eq!(
            super::parse_avoid_hosts_csv("github.com"),
            vec!["github.com".to_string()],
        );
    }

    fn known(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn no_default(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn test_candidate_preflight_missing_codebase_is_bad_request() {
        let c = json!({"campaign": "lintian-fixes"});
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        assert!(matches!(result, CandidatePreflight::BadRequest(_)));
    }

    #[test]
    fn test_candidate_preflight_empty_codebase_is_bad_request() {
        let c = json!({"codebase": "", "campaign": "lintian-fixes"});
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        assert!(matches!(result, CandidatePreflight::BadRequest(_)));
    }

    #[test]
    fn test_candidate_preflight_missing_campaign_is_bad_request() {
        let c = json!({"codebase": "pkg"});
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        assert!(matches!(result, CandidatePreflight::BadRequest(_)));
    }

    #[test]
    fn test_candidate_preflight_unknown_campaign_is_bucketed() {
        let c = json!({"codebase": "pkg", "campaign": "nope", "command": "true"});
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        assert_eq!(result, CandidatePreflight::UnknownCampaign("nope".into()));
    }

    #[test]
    fn test_candidate_preflight_missing_command_without_fallback_is_invalid() {
        let c = json!({"codebase": "pkg", "campaign": "lintian-fixes"});
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        assert_eq!(result, CandidatePreflight::InvalidCommand);
    }

    #[test]
    fn test_candidate_preflight_empty_command_falls_back_to_config() {
        let c = json!({"codebase": "pkg", "campaign": "lintian-fixes", "command": ""});
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), |name| {
            if name == "lintian-fixes" {
                Some("lintian-brush".to_string())
            } else {
                None
            }
        });
        match result {
            CandidatePreflight::Ok { command, .. } => assert_eq!(command, "lintian-brush"),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_candidate_preflight_explicit_command_wins() {
        let c = json!({
            "codebase": "pkg",
            "campaign": "lintian-fixes",
            "command": "explicit-cmd",
        });
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), |_| {
            Some("config-fallback".to_string())
        });
        match result {
            CandidatePreflight::Ok { command, .. } => assert_eq!(command, "explicit-cmd"),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_candidate_preflight_value_zero_is_invalid() {
        let c = json!({
            "codebase": "pkg",
            "campaign": "lintian-fixes",
            "command": "true",
            "value": 0,
        });
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        assert_eq!(result, CandidatePreflight::InvalidValue);
    }

    #[test]
    fn test_candidate_preflight_positive_value_passes_through() {
        let c = json!({
            "codebase": "pkg",
            "campaign": "lintian-fixes",
            "command": "true",
            "value": 42,
        });
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        match result {
            CandidatePreflight::Ok { value, .. } => assert_eq!(value, Some(42)),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_candidate_preflight_missing_value_is_none() {
        let c = json!({
            "codebase": "pkg",
            "campaign": "lintian-fixes",
            "command": "true",
        });
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        match result {
            CandidatePreflight::Ok { value, .. } => assert_eq!(value, None),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[test]
    fn test_candidate_preflight_happy_path_full_fields() {
        let c = json!({
            "codebase": "pkg",
            "campaign": "lintian-fixes",
            "command": "lintian-brush",
            "value": 10,
        });
        let result = candidate_preflight(&c, &known(&["lintian-fixes"]), no_default);
        match result {
            CandidatePreflight::Ok {
                codebase,
                campaign,
                command,
                value,
            } => {
                assert_eq!(codebase, "pkg");
                assert_eq!(campaign, "lintian-fixes");
                assert_eq!(command, "lintian-brush");
                assert_eq!(value, Some(10));
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    /// Regression for `admin_cleanup_runs`. The handler selects
    /// run IDs matching `result_code` plus optional `campaign` and
    /// `[min_finish_time, max_finish_time)`, then reschedules each
    /// `(codebase, suite, command)` and clears the `last_run.*` /
    /// `run.resume_from` FKs before deleting. The Python original
    /// had no equivalent; this is the canonical contract test.
    ///
    /// We don't drive the handler through axum here -- that would
    /// require building a full `AppState`. Instead reproduce the
    /// SQL the handler runs against a freshly seeded test DB and
    /// assert the side effects are right.
    #[tokio::test]
    async fn test_admin_cleanup_runs_filters_and_cleans_up() {
        use chrono::{TimeZone, Utc};
        let Ok(Some(db)) = crate::test_utils::TestDatabase::new_optional().await else {
            eprintln!("Skipping: no Postgres available");
            return;
        };
        if let Err(e) = janitor::schema::setup_test_database(&db.pool).await {
            eprintln!("Skipping: schema setup failed: {}", e);
            return;
        }
        let pool = &db.pool;

        // Schema has change_set as a non-null FK. Insert one shared
        // change_set so the run rows can attach to it.
        sqlx::query(
            "INSERT INTO change_set (id, campaign) VALUES \
             ('cs-keep', 'lintian-fixes'), ('cs-drop', 'lintian-fixes')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO codebase (name) VALUES ('alpha'), ('beta')")
            .execute(pool)
            .await
            .unwrap();

        let make_run = |id: &str,
                        codebase: &str,
                        suite: &str,
                        cs: &str,
                        cmd: &str,
                        rc: &str,
                        finish: chrono::DateTime<Utc>| {
            let pool = pool.clone();
            let id = id.to_string();
            let codebase = codebase.to_string();
            let suite = suite.to_string();
            let cs = cs.to_string();
            let cmd = cmd.to_string();
            let rc = rc.to_string();
            async move {
                sqlx::query(
                    "INSERT INTO run \
                     (id, command, start_time, finish_time, result_code, \
                      suite, change_set, codebase, logfilenames) \
                     VALUES ($1, $2, $3, $4, $5, $6::suite_name, $7, $8, ARRAY[]::text[])",
                )
                .bind(id)
                .bind(cmd)
                .bind(finish - chrono::Duration::minutes(1))
                .bind(finish)
                .bind(rc)
                .bind(suite)
                .bind(cs)
                .bind(codebase)
                .execute(&pool)
                .await
                .unwrap();
            }
        };

        let early = Utc.with_ymd_and_hms(2026, 1, 10, 12, 0, 0).unwrap();
        let mid = Utc.with_ymd_and_hms(2026, 2, 15, 12, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap();

        // 4 runs, all result_code='success' -- the cleanup target
        // should be the two in the [early, late) window for the
        // matching campaign. The fifth (`other-campaign`) and the
        // sixth (`success-after-window`) must be left alone.
        make_run(
            "r-old",
            "alpha",
            "lintian-fixes",
            "cs-drop",
            "cmd1",
            "success",
            early,
        )
        .await;
        make_run(
            "r-mid",
            "beta",
            "lintian-fixes",
            "cs-drop",
            "cmd2",
            "success",
            mid,
        )
        .await;
        make_run(
            "r-late",
            "alpha",
            "lintian-fixes",
            "cs-keep",
            "cmd1",
            "success",
            late,
        )
        .await;
        make_run(
            "r-othercamp",
            "alpha",
            "multiarch-fixes",
            "cs-keep",
            "cmdx",
            "success",
            mid,
        )
        .await;
        make_run(
            "r-fail",
            "alpha",
            "lintian-fixes",
            "cs-keep",
            "cmdy",
            "branch-unavailable",
            mid,
        )
        .await;

        // last_run rows pointing at r-old / r-mid -- the cleanup
        // must NULL them or the DELETE fails on FK. The `run` insert
        // trigger already populates (codebase, campaign) rows via
        // `refresh_last_run`, so we UPDATE rather than INSERT here.
        sqlx::query(
            "UPDATE last_run \
             SET last_run_id = 'r-old', last_effective_run_id = 'r-old' \
             WHERE codebase = 'alpha' AND campaign = 'lintian-fixes'",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE last_run \
             SET last_run_id = 'r-mid', last_effective_run_id = 'r-mid' \
             WHERE codebase = 'beta' AND campaign = 'lintian-fixes'",
        )
        .execute(pool)
        .await
        .unwrap();

        // -- Run the same SELECT the handler builds for the filter
        //    `result_code=success`, `campaign=lintian-fixes`,
        //    `min=early` (inclusive), `max=late` (exclusive).
        let max_window = Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap();
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT id, codebase, suite::text AS suite, command \
             FROM run \
             WHERE result_code = $1 \
               AND suite::text = $2 \
               AND (finish_time AT TIME ZONE 'UTC') >= $3 \
               AND (finish_time AT TIME ZONE 'UTC') < $4 \
             ORDER BY finish_time ASC",
        )
        .bind("success")
        .bind("lintian-fixes")
        .bind(early)
        .bind(max_window)
        .fetch_all(pool)
        .await
        .unwrap();

        let ids: Vec<&str> = rows.iter().map(|(id, _, _, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["r-old", "r-mid"],
            "filter should match the two in-window successes only"
        );

        // -- Now run the cleanup mutation: clear FKs, delete rows.
        //    Mirrors admin_cleanup_runs's UPDATE/DELETE block.
        let run_ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
        let mut tx = pool.begin().await.unwrap();
        for col in [
            "last_run_id",
            "last_effective_run_id",
            "last_unabsorbed_run_id",
        ] {
            let stmt = format!("UPDATE last_run SET {} = NULL WHERE {} = ANY($1)", col, col);
            sqlx::query(sqlx::AssertSqlSafe(&*stmt))
                .bind(&run_ids)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE run SET resume_from = NULL WHERE resume_from = ANY($1)")
            .bind(&run_ids)
            .execute(&mut *tx)
            .await
            .unwrap();
        let deleted = sqlx::query("DELETE FROM run WHERE id = ANY($1)")
            .bind(&run_ids)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        tx.commit().await.unwrap();
        assert_eq!(deleted, 2);

        // -- Untouched rows survive.
        let surviving: Vec<String> = sqlx::query_scalar("SELECT id FROM run ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
        assert_eq!(
            surviving,
            vec![
                "r-fail".to_string(),
                "r-late".to_string(),
                "r-othercamp".to_string()
            ],
            "only the in-window successes should be deleted"
        );

        // After DELETE, the `run_refresh_last_run` trigger re-runs
        // `refresh_last_run` for each (codebase, campaign): if any
        // run survives it repoints `last_run_id`, otherwise it drops
        // the last_run row entirely (see refresh_last_run in the
        // schema). So we assert on the surviving runs, not on all
        // last_run_ids being NULL.
        let last_rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT codebase, last_run_id FROM last_run \
             WHERE campaign = 'lintian-fixes' ORDER BY codebase",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        // beta has no surviving lintian-fixes runs -> last_run row gone.
        // alpha still has r-late and r-fail -> last_run points at one.
        assert!(
            last_rows.iter().all(|(cb, _)| cb == "alpha"),
            "expected only alpha's last_run row to survive, got {last_rows:?}"
        );
        for (_, last_run_id) in &last_rows {
            let id = last_run_id
                .as_deref()
                .expect("alpha's last_run_id must repopulate to a surviving run");
            assert!(
                id == "r-late" || id == "r-fail",
                "alpha's last_run_id should point at one of the surviving runs, got {id:?}"
            );
        }
    }

    /// Regression for the `description_like` filter on
    /// `admin_cleanup_runs`. A single result_code (e.g. `lintian`)
    /// covers multiple bug families; cleaning up runs from a
    /// recently-fixed deserialise bug requires matching its
    /// description substring without sweeping unrelated genuine
    /// failures back onto the queue.
    #[tokio::test]
    async fn test_admin_cleanup_runs_description_like_filter() {
        use chrono::{TimeZone, Utc};
        let Ok(Some(db)) = crate::test_utils::TestDatabase::new_optional().await else {
            eprintln!("Skipping: no Postgres available");
            return;
        };
        if let Err(e) = janitor::schema::setup_test_database(&db.pool).await {
            eprintln!("Skipping: schema setup failed: {}", e);
            return;
        }
        let pool = &db.pool;

        sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs1', 'lintian-fixes')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO codebase (name) VALUES ('alpha'), ('beta'), ('gamma')")
            .execute(pool)
            .await
            .unwrap();

        let when = Utc.with_ymd_and_hms(2026, 4, 29, 12, 0, 0).unwrap();
        let mk = |id: &str, codebase: &str, desc: &str| {
            let pool = pool.clone();
            let id = id.to_string();
            let codebase = codebase.to_string();
            let desc = desc.to_string();
            async move {
                sqlx::query(
                    "INSERT INTO run \
                     (id, command, start_time, finish_time, result_code, description, \
                      suite, change_set, codebase, logfilenames) \
                     VALUES ($1, 'cmd', $2, $3, 'lintian', $4, \
                             'lintian-fixes'::suite_name, 'cs1', $5, ARRAY[]::text[])",
                )
                .bind(id)
                .bind(when - chrono::Duration::minutes(1))
                .bind(when)
                .bind(desc)
                .bind(codebase)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        mk(
            "r-bug",
            "alpha",
            "Error running lintian: Lintian output invalid: invalid type: map, expected a string at line 12 column 18",
        )
        .await;
        mk(
            "r-bug2",
            "beta",
            "Error running lintian: Lintian output invalid: invalid type: map, expected a string at line 8 column 4",
        )
        .await;
        mk(
            "r-real",
            "gamma",
            "Error running lintian: lintian process exited with status 25",
        )
        .await;

        // Same SQL fragment the extended handler builds when
        // description_like is supplied. The %map% pattern targets
        // only the deserialise-bug rows, leaving the genuine
        // exit-code failure alone.
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM run \
             WHERE result_code = $1 \
               AND description ILIKE $2 \
             ORDER BY id ASC",
        )
        .bind("lintian")
        .bind("%invalid type: map%")
        .fetch_all(pool)
        .await
        .unwrap();
        assert_eq!(rows, vec!["r-bug".to_string(), "r-bug2".to_string()]);
    }

    /// Regression: `admin_cleanup_runs` originally only NULL'd the
    /// `last_run.*` and `run.resume_from` FKs before DELETE. Three
    /// other tables FK to `run.id` *without* `ON DELETE CASCADE` --
    /// `debian_build`, `review`, `followup` -- and a run that
    /// produced a built `.deb` (every successful build does) leaves
    /// a `debian_build` row that blocks the cleanup with
    /// `update or delete on table "run" violates foreign key
    /// constraint "debian_build_run_id_fkey"`. Pin that the handler
    /// now wipes those children too.
    ///
    /// Reproduces the SQL block exactly; not a handler-level smoke
    /// test (those need a full AppState).
    #[tokio::test]
    async fn test_admin_cleanup_runs_deletes_run_children() {
        use chrono::{TimeZone, Utc};
        let Ok(Some(db)) = crate::test_utils::TestDatabase::new_optional().await else {
            eprintln!("Skipping: no Postgres available");
            return;
        };
        // Need the Debian schema overlay so `debian_build` exists.
        if let Err(e) = janitor::schema::setup_debian_test_database(&db.pool).await {
            eprintln!("Skipping: schema setup failed: {}", e);
            return;
        }
        let pool = &db.pool;

        sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('cs1', 'lintian-fixes')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO codebase (name) VALUES ('alpha'), ('beta')")
            .execute(pool)
            .await
            .unwrap();

        let when = Utc.with_ymd_and_hms(2026, 5, 7, 12, 0, 0).unwrap();
        for (id, codebase) in [("r-doomed", "alpha"), ("r-keep", "beta")] {
            sqlx::query(
                "INSERT INTO run \
                 (id, command, start_time, finish_time, result_code, \
                  suite, change_set, codebase, logfilenames) \
                 VALUES ($1, 'cmd', $2, $3, 'success', \
                         'lintian-fixes'::suite_name, 'cs1', $4, ARRAY[]::text[])",
            )
            .bind(id)
            .bind(when - chrono::Duration::minutes(1))
            .bind(when)
            .bind(codebase)
            .execute(pool)
            .await
            .unwrap();
        }

        // candidate row needed for followup (origin + candidate).
        sqlx::query(
            "INSERT INTO candidate (id, codebase, suite, command) \
             VALUES (1, 'alpha', 'lintian-fixes'::suite_name, 'cmd'), \
                    (2, 'beta',  'lintian-fixes'::suite_name, 'cmd')",
        )
        .execute(pool)
        .await
        .unwrap();

        // Children pointing at r-doomed (will be deleted) and r-keep
        // (must survive). Pre-fix the cleanup blew up here because
        // the debian_build row was the first FK violation; reach
        // each child table to pin all three.
        sqlx::query(
            "INSERT INTO debian_build (run_id, version, distribution, source) \
             VALUES ('r-doomed', '1.0-1'::debversion, 'sid', 'alpha'), \
                    ('r-keep',   '2.0-1'::debversion, 'sid', 'beta')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO review (run_id, verdict, reviewer) \
             VALUES ('r-doomed', 'approved'::verdict, 'rev1'), \
                    ('r-keep',   'approved'::verdict, 'rev2')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO followup (origin, candidate) \
             VALUES ('r-doomed', 1), ('r-keep', 2)",
        )
        .execute(pool)
        .await
        .unwrap();

        // Mirror the handler's transactional cleanup block for the
        // single doomed run id.
        let run_ids = vec!["r-doomed".to_string()];
        let mut tx = pool.begin().await.unwrap();
        for col in [
            "last_run_id",
            "last_effective_run_id",
            "last_unabsorbed_run_id",
        ] {
            let stmt = format!("UPDATE last_run SET {} = NULL WHERE {} = ANY($1)", col, col);
            sqlx::query(sqlx::AssertSqlSafe(&*stmt))
                .bind(&run_ids)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE run SET resume_from = NULL WHERE resume_from = ANY($1)")
            .bind(&run_ids)
            .execute(&mut *tx)
            .await
            .unwrap();
        for (table, fk_col) in [
            ("debian_build", "run_id"),
            ("review", "run_id"),
            ("followup", "origin"),
        ] {
            let stmt = format!("DELETE FROM {} WHERE {} = ANY($1)", table, fk_col);
            sqlx::query(sqlx::AssertSqlSafe(&*stmt))
                .bind(&run_ids)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        let deleted = sqlx::query("DELETE FROM run WHERE id = ANY($1)")
            .bind(&run_ids)
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        tx.commit().await.unwrap();
        assert_eq!(deleted, 1, "the doomed run row should be gone");

        // r-keep's children survive; r-doomed's are gone.
        let surviving_runs: Vec<String> = sqlx::query_scalar("SELECT id FROM run ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
        assert_eq!(surviving_runs, vec!["r-keep".to_string()]);

        let surviving_builds: Vec<String> =
            sqlx::query_scalar("SELECT run_id FROM debian_build ORDER BY run_id")
                .fetch_all(pool)
                .await
                .unwrap();
        assert_eq!(
            surviving_builds,
            vec!["r-keep".to_string()],
            "debian_build child of doomed run must be deleted"
        );

        let surviving_reviews: Vec<String> =
            sqlx::query_scalar("SELECT run_id FROM review ORDER BY run_id")
                .fetch_all(pool)
                .await
                .unwrap();
        assert_eq!(surviving_reviews, vec!["r-keep".to_string()]);

        let surviving_followups: Vec<String> =
            sqlx::query_scalar("SELECT origin FROM followup ORDER BY origin")
                .fetch_all(pool)
                .await
                .unwrap();
        assert_eq!(surviving_followups, vec!["r-keep".to_string()]);
    }

    /// The assignment's `additional_colocated_branches` depends on the
    /// main branch's name, which the Rust runner has to recover from
    /// the stored branch URL (Python reads it off the opened branch).
    #[test]
    fn test_main_branch_name() {
        // No URL and no branch marker: the default branch.
        assert_eq!(main_branch_name(None), "main");
        assert_eq!(
            main_branch_name(Some("https://salsa.debian.org/foo/bar.git")),
            "main"
        );

        // breezy's native form.
        assert_eq!(
            main_branch_name(Some(
                "https://salsa.debian.org/foo/bar.git,branch=debian/sid"
            )),
            "debian/sid"
        );

        // Query-string form, including percent-encoded separators.
        assert_eq!(
            main_branch_name(Some("https://example.com/foo?branch=debian%2Fmaster")),
            "debian/master"
        );
        assert_eq!(
            main_branch_name(Some("https://example.com/foo?x=1&branch=upstream")),
            "upstream"
        );

        // Debian Vcs-Git style, as it round-trips through the codebase
        // table.
        assert_eq!(
            main_branch_name(Some(
                "https://salsa.debian.org/foo/bar.git -b debian/master"
            )),
            "debian/master"
        );

        // An empty branch value is not a branch name.
        assert_eq!(
            main_branch_name(Some("https://example.com/foo?branch=")),
            "main"
        );
    }
}
