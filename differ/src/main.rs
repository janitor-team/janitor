use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use breezyshim::RevisionId;
use clap::Parser;
use janitor::artifacts::{ArtifactManager, Error as ArtifactError};
use janitor_differ::diffoscope::{self, DiffoscopeOutput};
use janitor_differ::{find_binaries, is_binary, Error, Result};
use prometheus::{
    register_int_counter_vec_with_registry, Encoder, IntCounterVec, Registry, TextEncoder,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Notify, Semaphore};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info, info_span, warn, Instrument};

const TMP_PREFIX: &str = "janitor-differ";
/// Matches Python's PRECACHE_RETRIEVE_TIMEOUT. Also used by request-path
/// artifact retrieval so a hung backend surfaces as 504 rather than
/// hanging the handler indefinitely.
const ARTIFACT_RETRIEVE_TIMEOUT_SECS: u64 = 300;

/// Only characters we allow in run ids that get interpolated into cache
/// filenames. Defence in depth; the DB round-trip already gates real
/// requests, but precache callers pass ids straight through.
fn validate_run_id(id: &str) -> Result<()> {
    if id.is_empty() || id.starts_with('.') {
        return Err(Error::InvalidRunId(id.to_string()));
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !ok {
        return Err(Error::InvalidRunId(id.to_string()));
    }
    Ok(())
}

/// Prometheus metrics owned by this binary. We register once in
/// `register_metrics()` and share the counters via `Arc` so multiple test
/// runs and the real binary don't fight over the default registry.
static METRICS: OnceLock<Arc<DifferMetrics>> = OnceLock::new();

struct DifferMetrics {
    requests: IntCounterVec,
    cache_hits: IntCounterVec,
    errors: IntCounterVec,
    cache_write_errors: IntCounterVec,
    precache_started: IntCounterVec,
}

impl DifferMetrics {
    fn new(registry: &Registry) -> prometheus::Result<Self> {
        Ok(Self {
            requests: register_int_counter_vec_with_registry!(
                "differ_requests_total",
                "Number of requests served, by op",
                &["op"],
                registry
            )?,
            cache_hits: register_int_counter_vec_with_registry!(
                "differ_cache_hits_total",
                "Number of cache hits, by op",
                &["op"],
                registry
            )?,
            errors: register_int_counter_vec_with_registry!(
                "differ_errors_total",
                "Number of errors served, by op and reason",
                &["op", "reason"],
                registry
            )?,
            cache_write_errors: register_int_counter_vec_with_registry!(
                "differ_cache_write_errors_total",
                "Number of cache write failures, by op",
                &["op"],
                registry
            )?,
            precache_started: register_int_counter_vec_with_registry!(
                "differ_precache_started_total",
                "Number of precache jobs spawned, by trigger",
                &["trigger"],
                registry
            )?,
        })
    }
}

fn metrics() -> Arc<DifferMetrics> {
    METRICS
        .get_or_init(|| {
            Arc::new(
                DifferMetrics::new(prometheus::default_registry())
                    .expect("failed to register metrics"),
            )
        })
        .clone()
}

/// Coordinates concurrent generation of the same cache entry. When one
/// task is generating for key K, others wait on the Notify instead of
/// re-running the (expensive) command. Weak refs let entries evaporate
/// once no waiter holds them, so the map does not grow unbounded.
#[derive(Default)]
struct InFlight {
    map: Mutex<HashMap<(String, String), Weak<Notify>>>,
}

impl InFlight {
    /// Return either a fresh `Notify` guard (caller must generate and
    /// then call `finish`) or an existing `Notify` to wait on.
    fn acquire(&self, key: (String, String)) -> InFlightHandle<'_> {
        let mut m = self.map.lock().expect("in-flight mutex poisoned");
        if let Some(existing) = m.get(&key).and_then(Weak::upgrade) {
            InFlightHandle::Wait(existing)
        } else {
            let notify = Arc::new(Notify::new());
            m.insert(key.clone(), Arc::downgrade(&notify));
            InFlightHandle::Generate(GenerateGuard {
                inflight: self,
                key,
                notify,
            })
        }
    }
}

enum InFlightHandle<'a> {
    Generate(GenerateGuard<'a>),
    Wait(Arc<Notify>),
}

struct GenerateGuard<'a> {
    inflight: &'a InFlight,
    key: (String, String),
    notify: Arc<Notify>,
}

impl Drop for GenerateGuard<'_> {
    fn drop(&mut self) {
        // Remove the entry (Weak may still point to notify but new
        // callers should generate anew if they arrive after us) and
        // wake any waiters so they retry the cache read.
        self.inflight
            .map
            .lock()
            .expect("in-flight mutex poisoned")
            .remove(&self.key);
        self.notify.notify_waiters();
    }
}

#[derive(Parser)]
struct Args {
    #[clap(long, default_value = "localhost")]
    listen_address: String,

    #[clap(long, default_value_t = 9920)]
    port: u16,

    #[clap(long, default_value = "janitor.conf")]
    config: PathBuf,

    #[clap(long)]
    cache_path: Option<PathBuf>,

    #[clap(long, default_value_t = 1500)]
    task_memory_limit: u64,

    #[clap(long, default_value_t = 60)]
    task_timeout: u64,

    #[clap(long, default_value = "diffoscope")]
    diffoscope_command: String,

    /// Cap on concurrent precache jobs (each job fetches artifacts and
    /// runs debdiff+diffoscope). Keeps `/precache-all` from stampeding
    /// the artifact backend.
    #[clap(long, default_value_t = 4)]
    precache_concurrency: usize,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,
}

struct AppState {
    pool: sqlx::PgPool,
    artifact_manager: Arc<dyn ArtifactManager>,
    task_memory_limit: u64,
    task_timeout: u64,
    diffoscope_command: String,
    diffoscope_cache_dir: Option<PathBuf>,
    debdiff_cache_dir: Option<PathBuf>,
    /// Tracks background precache tasks so shutdown can await them.
    background_tasks: TaskTracker,
    /// Caps concurrent precache work so `/precache-all` cannot fan out
    /// thousands of parallel artifact fetches.
    precache_slots: Arc<Semaphore>,
    /// Deduplicates in-flight generation of the same debdiff or
    /// diffoscope cache entry.
    debdiff_inflight: Arc<InFlight>,
    diffoscope_inflight: Arc<InFlight>,
    metrics: Arc<DifferMetrics>,
}

impl AppState {
    /// Only callers who already have validated ids call this.
    fn diffoscope_cache_path(&self, old_id: &str, new_id: &str) -> Option<PathBuf> {
        self.diffoscope_cache_dir
            .as_ref()
            .map(|d| d.join(format!("{old_id}_{new_id}.json")))
    }

    fn debdiff_cache_path(&self, old_id: &str, new_id: &str) -> Option<PathBuf> {
        self.debdiff_cache_dir
            .as_ref()
            .map(|d| d.join(format!("{old_id}_{new_id}")))
    }
}

#[derive(Debug, sqlx::FromRow)]
struct Run {
    result_code: String,
    /// LEFT JOIN debian_build makes this NULL for runs that never
    /// produced a Debian build (failed early, control runs, ...).
    build_source: Option<String>,
    campaign: String,
    id: String,
    build_version: Option<debversion::Version>,
}

impl Run {
    fn build_version_str(&self) -> String {
        self.build_version
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default()
    }

    fn build_source_str(&self) -> &str {
        self.build_source.as_deref().unwrap_or("")
    }
}

async fn get_run(pool: &sqlx::PgPool, run_id: &str) -> sqlx::Result<Option<Run>> {
    sqlx::query_as::<_, Run>(
        r#"SELECT result_code, source AS build_source, suite::text AS campaign, id,
                  debian_build.version AS build_version
           FROM run
           LEFT JOIN debian_build ON debian_build.run_id = run.id
           WHERE id = $1"#,
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await
}

async fn get_unchanged_run(
    pool: &sqlx::PgPool,
    codebase: &str,
    main_branch_revision: &RevisionId,
) -> sqlx::Result<Option<Run>> {
    sqlx::query_as::<_, Run>(
        r#"SELECT result_code, source AS build_source, suite::text AS campaign, id,
                  debian_build.version AS build_version
           FROM run
           LEFT JOIN debian_build ON debian_build.run_id = run.id
           WHERE revision = $1 AND codebase = $2 AND result_code = 'success'
             AND run.id = run.change_set
           ORDER BY finish_time DESC"#,
    )
    .bind(main_branch_revision)
    .bind(codebase)
    .fetch_optional(pool)
    .await
}

/// Fetch two successful runs. Distinguishes missing rows (404 run-not-found)
/// from present-but-failed rows (422 run-not-successful).
async fn get_run_pair(pool: &sqlx::PgPool, old_id: &str, new_id: &str) -> Result<(Run, Run)> {
    let new_run = get_run(pool, new_id).await?;
    let old_run = get_run(pool, old_id).await?;
    let old_run = check_successful(old_id, old_run)?;
    let new_run = check_successful(new_id, new_run)?;
    Ok((old_run, new_run))
}

fn check_successful(run_id: &str, run: Option<Run>) -> Result<Run> {
    match run {
        None => Err(Error::RunNotFound(run_id.to_string())),
        Some(r) if r.result_code != "success" => Err(Error::RunNotSuccessful(run_id.to_string())),
        Some(r) => Ok(r),
    }
}

/// Retrieve the two runs' binaries into fresh temp dirs.
async fn fetch_binaries(
    artifact_manager: &dyn ArtifactManager,
    old_id: &str,
    new_id: &str,
) -> Result<(
    tempfile::TempDir,
    Vec<(std::ffi::OsString, PathBuf)>,
    tempfile::TempDir,
    Vec<(std::ffi::OsString, PathBuf)>,
)> {
    let old_dir = tempfile::TempDir::with_prefix(TMP_PREFIX)?;
    let new_dir = tempfile::TempDir::with_prefix(TMP_PREFIX)?;

    let timeout = std::time::Duration::from_secs(ARTIFACT_RETRIEVE_TIMEOUT_SECS);
    let span = info_span!("fetch-artifacts", old_run_id = old_id, new_run_id = new_id);
    let (old_res, new_res) = async {
        tokio::join!(
            tokio::time::timeout(
                timeout,
                artifact_manager.retrieve_artifacts(old_id, old_dir.path(), Some(&is_binary)),
            ),
            tokio::time::timeout(
                timeout,
                artifact_manager.retrieve_artifacts(new_id, new_dir.path(), Some(&is_binary)),
            ),
        )
    }
    .instrument(span)
    .await;
    for (res, id) in [(old_res, old_id), (new_res, new_id)] {
        classify_retrieval_result(id, res)?;
    }

    let old_binaries = find_binaries(old_dir.path())?;
    if old_binaries.is_empty() {
        return Err(Error::ArtifactsMissing(old_id.to_string()));
    }
    let new_binaries = find_binaries(new_dir.path())?;
    if new_binaries.is_empty() {
        return Err(Error::ArtifactsMissing(new_id.to_string()));
    }
    Ok((old_dir, old_binaries, new_dir, new_binaries))
}

fn classify_retrieval_result(
    run_id: &str,
    res: std::result::Result<std::result::Result<(), ArtifactError>, tokio::time::error::Elapsed>,
) -> Result<()> {
    match res {
        Err(_) => Err(Error::ArtifactRetrievalTimeout(run_id.to_string())),
        Ok(Ok(())) => Ok(()),
        Ok(Err(ArtifactError::ArtifactsMissing)) => {
            Err(Error::ArtifactsMissing(run_id.to_string()))
        }
        Ok(Err(e)) => Err(Error::ArtifactRetrievalFailed {
            run_id: run_id.to_string(),
            reason: e.to_string(),
        }),
    }
}

fn build_router(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/health", get(handle_health))
        .route("/ready", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route("/debdiff/{old_id}/{new_id}", get(handle_debdiff))
        .route("/diffoscope/{old_id}/{new_id}", get(handle_diffoscope))
        .route("/precache/{old_id}/{new_id}", post(handle_precache))
        .route("/precache-all", post(handle_precache_all))
        .with_state(state)
}

async fn handle_health(State(state): State<Arc<AppState>>) -> Result<&'static str> {
    sqlx::query("SELECT 1").execute(&state.pool).await?;
    Ok("ok")
}

async fn handle_metrics() -> Result<Response> {
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    encoder
        .encode(&prometheus::default_registry().gather(), &mut buf)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, encoder.format_type())],
        buf,
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct DiffoscopeQuery {
    #[serde(default)]
    filter_boring: Option<String>,
    #[serde(default)]
    css_url: Option<String>,
}

async fn handle_diffoscope(
    Path((old_id, new_id)): Path<(String, String)>,
    Query(query): Query<DiffoscopeQuery>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    state
        .metrics
        .requests
        .with_label_values(&["diffoscope"])
        .inc();
    let result = handle_diffoscope_inner(&old_id, &new_id, query, &state, &headers).await;
    if let Err(e) = &result {
        state
            .metrics
            .errors
            .with_label_values(&["diffoscope", e.reason()])
            .inc();
    }
    result
}

async fn handle_diffoscope_inner(
    old_id: &str,
    new_id: &str,
    query: DiffoscopeQuery,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Response> {
    validate_run_id(old_id)?;
    validate_run_id(new_id)?;

    let offered = &[
        "text/plain",
        "text/html",
        "application/json",
        "text/markdown",
    ];
    let content_type =
        negotiate(headers, offered).ok_or_else(|| Error::ContentNegotiationFailed {
            offered: offered.join(", "),
        })?;

    let (old_run, new_run) = get_run_pair(&state.pool, old_id, new_id).await?;

    let cache_path = state.diffoscope_cache_path(&old_run.id, &new_run.id);
    if let Some(parent) = cache_path.as_deref().and_then(|p| p.parent()) {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            error!("failed to create diffoscope cache dir: {e}");
        }
    }

    let mut diff =
        load_or_generate_diffoscope(state, &old_run, &new_run, cache_path.as_deref()).await?;

    diff.source1 = format!(
        "{} version {} ({})",
        old_run.build_source_str(),
        old_run.build_version_str(),
        old_run.campaign
    )
    .into();
    diff.source2 = format!(
        "{} version {} ({})",
        new_run.build_source_str(),
        new_run.build_version_str(),
        new_run.campaign
    )
    .into();

    diffoscope::filter_irrelevant(&mut diff);

    let mut title = format!(
        "diffoscope for {} applied to {}",
        new_run.campaign,
        new_run.build_source_str()
    );

    if query.filter_boring.is_some() {
        diffoscope::filter_boring(
            &mut diff,
            &old_run.build_version_str(),
            &new_run.build_version_str(),
            &old_run.campaign,
            &new_run.campaign,
        );
        title.push_str(" (filtered)");
    }

    let body = {
        let _span = info_span!("format-diffoscope", content_type).entered();
        diffoscope::format_diffoscope(&diff, content_type, &title, query.css_url.as_deref())?
    };
    // Python uses `web.Response(text=..., content_type=...)` for all
    // diffoscope responses, which adds `; charset=utf-8`.
    Ok(typed_text_response(content_type, body))
}

async fn load_or_generate_diffoscope(
    state: &AppState,
    old_run: &Run,
    new_run: &Run,
    cache_path: Option<&StdPath>,
) -> Result<DiffoscopeOutput> {
    // Try the cache once up front.
    if let Some(p) = cache_path {
        if let Some(d) = try_load_diffoscope_cache(p).await {
            state
                .metrics
                .cache_hits
                .with_label_values(&["diffoscope"])
                .inc();
            return Ok(d);
        }
    }

    // Dedupe: if another task is already generating for this pair,
    // wait for them and re-read the cache before falling back to
    // running diffoscope ourselves.
    let key = (old_run.id.clone(), new_run.id.clone());
    let guard = match state.diffoscope_inflight.acquire(key) {
        InFlightHandle::Generate(g) => g,
        InFlightHandle::Wait(notify) => {
            notify.notified().await;
            if let Some(p) = cache_path {
                if let Some(d) = try_load_diffoscope_cache(p).await {
                    state
                        .metrics
                        .cache_hits
                        .with_label_values(&["diffoscope"])
                        .inc();
                    return Ok(d);
                }
            }
            // Cache wasn't populated (generator failed or caching
            // disabled). Fall through and generate ourselves.
            match state
                .diffoscope_inflight
                .acquire((old_run.id.clone(), new_run.id.clone()))
            {
                InFlightHandle::Generate(g) => g,
                InFlightHandle::Wait(_) => {
                    // Extremely unlikely race; just generate without a
                    // guard rather than looping.
                    return generate_diffoscope(state, old_run, new_run, cache_path).await;
                }
            }
        }
    };

    let diff = generate_diffoscope(state, old_run, new_run, cache_path).await;
    drop(guard);
    diff
}

async fn try_load_diffoscope_cache(p: &StdPath) -> Option<DiffoscopeOutput> {
    let bytes = match tokio::fs::read(p).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            error!("failed to read diffoscope cache {}: {e}", p.display());
            return None;
        }
    };
    match serde_json::from_slice::<DiffoscopeOutput>(&bytes) {
        Ok(d) => Some(d),
        Err(e) => {
            error!("failed to parse diffoscope cache {}: {e}", p.display());
            None
        }
    }
}

async fn generate_diffoscope(
    state: &AppState,
    old_run: &Run,
    new_run: &Run,
    cache_path: Option<&StdPath>,
) -> Result<DiffoscopeOutput> {
    info!(
        old_run_id = old_run.id,
        new_run_id = new_run.id,
        "Generating diffoscope between {} ({}/{}/{}) and {} ({}/{}/{})",
        old_run.id,
        old_run.build_source_str(),
        old_run.build_version_str(),
        old_run.campaign,
        new_run.id,
        new_run.build_source_str(),
        new_run.build_version_str(),
        new_run.campaign,
    );

    let (_old_dir, old_bins, _new_dir, new_bins) =
        fetch_binaries(&*state.artifact_manager, &old_run.id, &new_run.id).await?;

    let old_args = os_pairs(&old_bins).map_err(|r| Error::DiffCommandError {
        command: "diffoscope",
        reason: r,
    })?;
    let new_args = os_pairs(&new_bins).map_err(|r| Error::DiffCommandError {
        command: "diffoscope",
        reason: r,
    })?;
    let old_slice: Vec<(&str, &str)> = old_args
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let new_slice: Vec<(&str, &str)> = new_args
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();

    let diff = diffoscope::run_diffoscope(
        &old_slice,
        &new_slice,
        Some(state.task_timeout as f64),
        Some(state.task_memory_limit),
        Some(&state.diffoscope_command),
    )
    .instrument(info_span!("run-diffoscope"))
    .await?;

    if let Some(p) = cache_path {
        if let Err(e) = write_diffoscope_cache(p, &diff).await {
            error!("failed to write diffoscope cache {}: {e}", p.display());
            state
                .metrics
                .cache_write_errors
                .with_label_values(&["diffoscope"])
                .inc();
        }
    }

    Ok(diff)
}

async fn write_diffoscope_cache(p: &StdPath, diff: &DiffoscopeOutput) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(diff).map_err(std::io::Error::other)?;
    tokio::fs::write(p, bytes).await
}

#[derive(Debug, Deserialize)]
struct DebdiffQuery {
    #[serde(default)]
    filter_boring: Option<String>,
}

async fn handle_debdiff(
    Path((old_id, new_id)): Path<(String, String)>,
    Query(query): Query<DebdiffQuery>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response> {
    state.metrics.requests.with_label_values(&["debdiff"]).inc();
    let result = handle_debdiff_inner(&old_id, &new_id, query, &state, &headers).await;
    if let Err(e) = &result {
        state
            .metrics
            .errors
            .with_label_values(&["debdiff", e.reason()])
            .inc();
    }
    result
}

async fn handle_debdiff_inner(
    old_id: &str,
    new_id: &str,
    query: DebdiffQuery,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Response> {
    validate_run_id(old_id)?;
    validate_run_id(new_id)?;

    let offered = &["text/x-diff", "text/plain", "text/markdown", "text/html"];
    let content_type =
        negotiate(headers, offered).ok_or_else(|| Error::ContentNegotiationFailed {
            offered: offered.join(", "),
        })?;

    let (old_run, new_run) = get_run_pair(&state.pool, old_id, new_id).await?;

    let cache_path = state.debdiff_cache_path(&old_run.id, &new_run.id);
    if let Some(parent) = cache_path.as_deref().and_then(|p| p.parent()) {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            error!("failed to create debdiff cache dir: {e}");
        }
    }

    // Fast path: cached plain-text response with no post-processing streams
    // straight from the cache file without loading it into memory.
    if query.filter_boring.is_none() && matches!(content_type, "text/x-diff" | "text/plain") {
        if let Some(p) = cache_path.as_deref() {
            if let Ok(file) = tokio::fs::File::open(p).await {
                state
                    .metrics
                    .cache_hits
                    .with_label_values(&["debdiff"])
                    .inc();
                return Ok((
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/plain")],
                    Body::from_stream(ReaderStream::new(file)),
                )
                    .into_response());
            }
        }
    }

    let debdiff =
        load_or_generate_debdiff(state, &old_run, &new_run, cache_path.as_deref()).await?;

    let mut debdiff = String::from_utf8(debdiff).map_err(|e| Error::DiffCommandError {
        command: "debdiff",
        reason: format!("debdiff output is not UTF-8: {e}"),
    })?;

    if query.filter_boring.is_some() {
        debdiff = janitor::debdiff::filter_boring(
            &debdiff,
            &old_run.build_version_str(),
            &new_run.build_version_str(),
        );
    }

    match content_type {
        // Python `web.Response(body=..., content_type="text/plain")` — no charset.
        "text/x-diff" | "text/plain" => Ok(typed_response("text/plain", debdiff)),
        // Python `web.Response(text=..., content_type=...)` — adds charset.
        "text/markdown" => Ok(typed_text_response(
            "text/markdown",
            janitor::debdiff::markdownify_debdiff(&debdiff),
        )),
        "text/html" => Ok(typed_text_response(
            "text/html",
            janitor::debdiff::htmlize_debdiff(&debdiff),
        )),
        _ => unreachable!("negotiate() returns only from the allowed set"),
    }
}

async fn load_or_generate_debdiff(
    state: &AppState,
    old_run: &Run,
    new_run: &Run,
    cache_path: Option<&StdPath>,
) -> Result<Vec<u8>> {
    if let Some(p) = cache_path {
        match tokio::fs::read(p).await {
            Ok(bytes) => {
                state
                    .metrics
                    .cache_hits
                    .with_label_values(&["debdiff"])
                    .inc();
                return Ok(bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => error!("failed to read debdiff cache {}: {e}", p.display()),
        }
    }

    let key = (old_run.id.clone(), new_run.id.clone());
    let guard = match state.debdiff_inflight.acquire(key) {
        InFlightHandle::Generate(g) => g,
        InFlightHandle::Wait(notify) => {
            notify.notified().await;
            if let Some(p) = cache_path {
                if let Ok(bytes) = tokio::fs::read(p).await {
                    state
                        .metrics
                        .cache_hits
                        .with_label_values(&["debdiff"])
                        .inc();
                    return Ok(bytes);
                }
            }
            match state
                .debdiff_inflight
                .acquire((old_run.id.clone(), new_run.id.clone()))
            {
                InFlightHandle::Generate(g) => g,
                InFlightHandle::Wait(_) => {
                    return generate_debdiff(state, old_run, new_run, cache_path).await;
                }
            }
        }
    };

    let bytes = generate_debdiff(state, old_run, new_run, cache_path).await;
    drop(guard);
    bytes
}

async fn generate_debdiff(
    state: &AppState,
    old_run: &Run,
    new_run: &Run,
    cache_path: Option<&StdPath>,
) -> Result<Vec<u8>> {
    info!(
        "Generating debdiff between {} ({}/{}/{}) and {} ({}/{}/{})",
        old_run.id,
        old_run.build_source_str(),
        old_run.build_version_str(),
        old_run.campaign,
        new_run.id,
        new_run.build_source_str(),
        new_run.build_version_str(),
        new_run.campaign,
    );

    let (_old_dir, old_bins, _new_dir, new_bins) =
        fetch_binaries(&*state.artifact_manager, &old_run.id, &new_run.id).await?;

    let old_paths = str_paths(&old_bins).map_err(|r| Error::DiffCommandError {
        command: "debdiff",
        reason: r,
    })?;
    let new_paths = str_paths(&new_bins).map_err(|r| Error::DiffCommandError {
        command: "debdiff",
        reason: r,
    })?;

    let run =
        janitor::debdiff::run_debdiff(old_paths, new_paths).instrument(info_span!("run-debdiff"));
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(state.task_timeout), run)
        .await
        .map_err(|_| Error::DiffCommandTimeout("debdiff"))?
        .map_err(|e| Error::DiffCommandError {
            command: "debdiff",
            reason: e.message().to_string(),
        })?;

    if let Some(p) = cache_path {
        if let Err(e) = tokio::fs::write(p, &bytes).await {
            error!("failed to write debdiff cache {}: {e}", p.display());
            state
                .metrics
                .cache_write_errors
                .with_label_values(&["debdiff"])
                .inc();
        }
    }
    Ok(bytes)
}

async fn precache(state: &AppState, old_id: String, new_id: String) -> Result<()> {
    validate_run_id(&old_id)?;
    validate_run_id(&new_id)?;

    // Bound concurrent precache work. Acquire before we do anything
    // expensive; a full queue makes callers wait, not accumulate.
    let _permit = state
        .precache_slots
        .clone()
        .acquire_owned()
        .await
        .expect("precache semaphore closed");

    let diffoscope_cache_path = state.diffoscope_cache_path(&old_id, &new_id);
    let debdiff_cache_path = state.debdiff_cache_path(&old_id, &new_id);

    // Skip retrieval if both cache paths already exist.
    let debdiff_needed = match debdiff_cache_path.as_deref() {
        Some(p) => !tokio::fs::try_exists(p).await.unwrap_or(false),
        None => false,
    };
    let diffoscope_needed = match diffoscope_cache_path.as_deref() {
        Some(p) => !tokio::fs::try_exists(p).await.unwrap_or(false),
        None => false,
    };
    if !debdiff_needed && !diffoscope_needed {
        return Ok(());
    }

    let (_old_dir, old_binaries, _new_dir, new_binaries) =
        fetch_binaries(&*state.artifact_manager, &old_id, &new_id).await?;

    if debdiff_needed {
        if let Some(p) = debdiff_cache_path.as_deref() {
            if let Some(parent) = p.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        generate_debdiff_with_binaries(
            state,
            &old_binaries,
            &new_binaries,
            debdiff_cache_path.as_deref(),
        )
        .await?;
        info!(
            old_run_id = old_id,
            new_run_id = new_id,
            "Precached debdiff"
        );
    }

    if diffoscope_needed {
        if let Some(p) = diffoscope_cache_path.as_deref() {
            if let Some(parent) = p.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        generate_diffoscope_with_binaries(
            state,
            &old_binaries,
            &new_binaries,
            diffoscope_cache_path.as_deref(),
        )
        .await?;
        info!(
            old_run_id = old_id,
            new_run_id = new_id,
            "Precached diffoscope"
        );
    }

    Ok(())
}

/// Run debdiff on already-fetched binaries. Precache uses this to avoid
/// refetching artifacts once for debdiff and once for diffoscope.
async fn generate_debdiff_with_binaries(
    state: &AppState,
    old_bins: &[(std::ffi::OsString, PathBuf)],
    new_bins: &[(std::ffi::OsString, PathBuf)],
    cache_path: Option<&StdPath>,
) -> Result<Vec<u8>> {
    let old_paths = str_paths(old_bins).map_err(|r| Error::DiffCommandError {
        command: "debdiff",
        reason: r,
    })?;
    let new_paths = str_paths(new_bins).map_err(|r| Error::DiffCommandError {
        command: "debdiff",
        reason: r,
    })?;
    let run = janitor::debdiff::run_debdiff(old_paths, new_paths);
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(state.task_timeout), run)
        .await
        .map_err(|_| Error::DiffCommandTimeout("debdiff"))?
        .map_err(|e| Error::DiffCommandError {
            command: "debdiff",
            reason: e.to_string(),
        })?;
    if let Some(p) = cache_path {
        if let Err(e) = tokio::fs::write(p, &bytes).await {
            error!("failed to write debdiff cache {}: {e}", p.display());
            state
                .metrics
                .cache_write_errors
                .with_label_values(&["debdiff"])
                .inc();
        }
    }
    Ok(bytes)
}

async fn generate_diffoscope_with_binaries(
    state: &AppState,
    old_bins: &[(std::ffi::OsString, PathBuf)],
    new_bins: &[(std::ffi::OsString, PathBuf)],
    cache_path: Option<&StdPath>,
) -> Result<DiffoscopeOutput> {
    let old_args = os_pairs(old_bins).map_err(|r| Error::DiffCommandError {
        command: "diffoscope",
        reason: r,
    })?;
    let new_args = os_pairs(new_bins).map_err(|r| Error::DiffCommandError {
        command: "diffoscope",
        reason: r,
    })?;
    let old_slice: Vec<(&str, &str)> = old_args
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let new_slice: Vec<(&str, &str)> = new_args
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let diff = diffoscope::run_diffoscope(
        &old_slice,
        &new_slice,
        Some(state.task_timeout as f64),
        Some(state.task_memory_limit),
        Some(&state.diffoscope_command),
    )
    .await?;
    if let Some(p) = cache_path {
        if let Err(e) = write_diffoscope_cache(p, &diff).await {
            error!("failed to write diffoscope cache {}: {e}", p.display());
            state
                .metrics
                .cache_write_errors
                .with_label_values(&["diffoscope"])
                .inc();
        }
    }
    Ok(diff)
}

async fn handle_precache(
    Path((old_id, new_id)): Path<(String, String)>,
    State(state): State<Arc<AppState>>,
) -> Result<Response> {
    validate_run_id(&old_id)?;
    validate_run_id(&new_id)?;
    let (old_run, new_run) = get_run_pair(&state.pool, &old_id, &new_id).await?;
    spawn_precache(&state, old_run.id, new_run.id, "request");
    Ok((StatusCode::ACCEPTED, "Precaching started").into_response())
}

async fn handle_precache_all(State(state): State<Arc<AppState>>) -> Result<Response> {
    let rows = sqlx::query_as::<_, (String, String)>(
        r#"SELECT run.id, unchanged_run.id
           FROM run
           INNER JOIN run AS unchanged_run
             ON run.main_branch_revision = unchanged_run.revision
           WHERE run.result_code = 'success'
             AND unchanged_run.result_code = 'success'
             AND run.main_branch_revision != run.revision
             AND run.suite NOT IN ('control', 'unchanged')
           ORDER BY run.finish_time DESC, unchanged_run.finish_time DESC"#,
    )
    .fetch_all(&state.pool)
    .await?;

    if rows.is_empty() {
        return Ok((StatusCode::OK, Json(serde_json::json!({"count": 0}))).into_response());
    }

    let count = rows.len();
    for (new_id, old_id) in rows {
        spawn_precache(&state, old_id, new_id, "precache-all");
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"count": count})),
    )
        .into_response())
}

fn spawn_precache(state: &Arc<AppState>, old_id: String, new_id: String, trigger: &'static str) {
    state
        .metrics
        .precache_started
        .with_label_values(&[trigger])
        .inc();
    let state = state.clone();
    state.clone().background_tasks.spawn(async move {
        if let Err(e) = precache(&state, old_id, new_id).await {
            info!("Error precaching diff: {e}");
        }
    });
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct ResultMessage {
    code: String,
    log_id: String,
    revision: String,
    main_branch_revision: String,
    codebase: String,
}

impl janitor::redis::PubSubMessage for ResultMessage {
    fn channel() -> &'static str {
        "result"
    }
}

async fn listen_to_runner(redis_config: janitor::redis::RedisConfig, state: Arc<AppState>) {
    let manager = match janitor::redis::RedisManager::new(redis_config) {
        Ok(m) => m,
        Err(e) => {
            error!("failed to create Redis manager: {e}");
            return;
        }
    };
    let subscriber = manager.subscriber();
    let state_arc = state.clone();
    let res = subscriber
        .subscribe::<ResultMessage, _, _>(move |msg| {
            let state = state_arc.clone();
            async move {
                if msg.code != "success" {
                    return Ok(());
                }
                let candidates = match candidate_pairs(&state.pool, &msg).await {
                    Ok(c) => c,
                    Err(e) => {
                        error!("failed to look up unchanged runs: {e}");
                        return Ok(());
                    }
                };
                for (old_id, new_id) in candidates {
                    spawn_precache(&state, old_id, new_id, "runner");
                }
                Ok(())
            }
        })
        .await;
    if let Err(e) = res {
        error!("Redis subscription ended: {e}");
    }
}

async fn candidate_pairs(
    pool: &sqlx::PgPool,
    msg: &ResultMessage,
) -> sqlx::Result<Vec<(String, String)>> {
    if msg.revision == msg.main_branch_revision {
        let rows = sqlx::query_as::<_, (String,)>(
            r#"SELECT id FROM run
               WHERE result_code = 'success' AND main_branch_revision = $1"#,
        )
        .bind(&msg.revision)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id,)| (msg.log_id.clone(), id))
            .collect())
    } else {
        let rev = RevisionId::from(msg.main_branch_revision.as_bytes().to_vec());
        let unchanged = get_unchanged_run(pool, &msg.codebase, &rev).await?;
        Ok(unchanged
            .into_iter()
            .map(|run| (run.id, msg.log_id.clone()))
            .collect())
    }
}

/// 200 response with `Content-Type: <ct>` — no charset, matching Python's
/// `web.Response(body=..., content_type=...)`.
fn typed_response(content_type: &str, body: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, content_type)],
        body.into(),
    )
        .into_response()
}

/// 200 response with `Content-Type: <ct>; charset=utf-8`, matching Python's
/// `web.Response(text=..., content_type=...)` (aiohttp default).
fn typed_text_response(content_type: &str, body: impl Into<String>) -> Response {
    let ct = format!("{content_type}; charset=utf-8");
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, ct.as_str())],
        body.into(),
    )
        .into_response()
}

/// Content negotiation matching Python `mimeparse.best_match`: for each
/// supported type, score against the Accept header; on ties, the last
/// supported type wins (so the caller lists least-preferred first).
fn negotiate<'a>(headers: &HeaderMap, supported: &'a [&'a str]) -> Option<&'a str> {
    let raw = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*/*");
    let ranges = parse_accept(raw);

    let mut best: Option<(f32, &str)> = None;
    for candidate in supported {
        let (ct, cs) = split_mime(candidate)?;
        let score = ranges
            .iter()
            .filter_map(|r| match_range(r, ct, cs))
            .fold(0.0_f32, f32::max);
        if score > 0.0 {
            // `>=` so later entries overwrite earlier ones on ties.
            if best.is_none_or(|(prev, _)| score >= prev) {
                best = Some((score, candidate));
            }
        }
    }
    best.map(|(_, s)| s)
}

/// Accept range: (type, subtype, q, specificity). Specificity is 3 for
/// `type/subtype`, 2 for `type/*`, 1 for `*/*`.
fn parse_accept(header: &str) -> Vec<(&str, &str, f32, u8)> {
    header
        .split(',')
        .filter_map(|entry| {
            let mut parts = entry.trim().split(';').map(str::trim);
            let mime = parts.next()?;
            let (t, s) = split_mime(mime)?;
            let mut q = 1.0;
            for p in parts {
                if let Some(v) = p.strip_prefix("q=") {
                    q = v.parse().unwrap_or(1.0);
                }
            }
            let spec = match (t, s) {
                ("*", "*") => 1,
                (_, "*") => 2,
                _ => 3,
            };
            Some((t, s, q, spec))
        })
        .collect()
}

fn split_mime(s: &str) -> Option<(&str, &str)> {
    s.split_once('/')
}

fn match_range(range: &(&str, &str, f32, u8), ct: &str, cs: &str) -> Option<f32> {
    let (rt, rs, q, spec) = *range;
    let type_ok = rt == "*" || rt.eq_ignore_ascii_case(ct);
    let sub_ok = rs == "*" || rs.eq_ignore_ascii_case(cs);
    if type_ok && sub_ok {
        // Boost by specificity so `text/plain` beats `text/*` beats `*/*`
        // when the same client sends multiple ranges.
        Some(q * spec as f32)
    } else {
        None
    }
}

fn os_pairs(
    bins: &[(std::ffi::OsString, PathBuf)],
) -> std::result::Result<Vec<(String, String)>, String> {
    bins.iter()
        .map(|(name, path)| {
            let n = name
                .to_str()
                .ok_or_else(|| format!("invalid UTF-8 in filename {name:?}"))?;
            let p = path
                .to_str()
                .ok_or_else(|| format!("invalid UTF-8 in path {path:?}"))?;
            Ok((n.to_string(), p.to_string()))
        })
        .collect()
}

fn str_paths(bins: &[(std::ffi::OsString, PathBuf)]) -> std::result::Result<Vec<&str>, String> {
    bins.iter()
        .map(|(_, path)| {
            path.to_str()
                .ok_or_else(|| format!("invalid UTF-8 in path {path:?}"))
        })
        .collect()
}

/// Errors that abort startup or terminate the server abnormally.
#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("failed to read config {path}: {reason}")]
    Config { path: PathBuf, reason: String },
    #[error("failed to create database pool: {0}")]
    Pool(#[source] sqlx::Error),
    #[error("artifact_location is not configured")]
    NoArtifactLocation,
    #[error("failed to create artifact manager: {0}")]
    ArtifactManager(#[source] janitor::artifacts::Error),
    #[error("failed to create cache dir {path}: {source}")]
    CacheDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to resolve listen address {address}: {source}")]
    Resolve {
        address: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} did not resolve to any address")]
    NoAddress(String),
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: std::net::SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("server error: {0}")]
    Server(#[source] std::io::Error),
    #[error("server task panicked: {0}")]
    ServerPanic(#[source] tokio::task::JoinError),
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> std::result::Result<(), StartupError> {
    let args = Args::parse();
    args.logging.init();

    let config = janitor::config::read_file(&args.config).map_err(|e| StartupError::Config {
        path: args.config.clone(),
        reason: e.to_string(),
    })?;

    if let Err(e) =
        janitor::utils::set_user_agent(config.has_user_agent().then(|| config.user_agent()))
    {
        warn!("failed to install user agent override: {e}");
    }

    let db = janitor::state::create_pool(&config)
        .await
        .map_err(StartupError::Pool)?;

    let artifact_location = config
        .artifact_location
        .clone()
        .ok_or(StartupError::NoArtifactLocation)?;
    let artifact_manager = janitor::artifacts::get_artifact_manager(&artifact_location)
        .await
        .map_err(StartupError::ArtifactManager)?;

    if let Some(p) = args.cache_path.as_ref() {
        std::fs::create_dir_all(p).map_err(|e| StartupError::CacheDir {
            path: p.clone(),
            source: e,
        })?;
    }

    let background_tasks = TaskTracker::new();
    let state = Arc::new(AppState {
        pool: db,
        artifact_manager: Arc::from(artifact_manager),
        task_memory_limit: args.task_memory_limit,
        task_timeout: args.task_timeout,
        diffoscope_command: args.diffoscope_command,
        diffoscope_cache_dir: args.cache_path.as_ref().map(|p| p.join("diffoscope")),
        debdiff_cache_dir: args.cache_path.as_ref().map(|p| p.join("debdiff")),
        background_tasks: background_tasks.clone(),
        precache_slots: Arc::new(Semaphore::new(args.precache_concurrency.max(1))),
        debdiff_inflight: Arc::new(InFlight::default()),
        diffoscope_inflight: Arc::new(InFlight::default()),
        metrics: metrics(),
    });

    let shutdown = CancellationToken::new();

    if let Some(redis_location) = config.redis_location.as_ref() {
        let redis_config = janitor::redis::RedisConfig::new(redis_location.to_string());
        let listener_state = state.clone();
        let listener_shutdown = shutdown.clone();
        background_tasks.spawn(async move {
            tokio::select! {
                _ = listen_to_runner(redis_config, listener_state) => {}
                _ = listener_shutdown.cancelled() => {}
            }
        });
    }

    let app = build_router(state);

    let addrs: Vec<_> = tokio::net::lookup_host((args.listen_address.as_str(), args.port))
        .await
        .map_err(|e| StartupError::Resolve {
            address: args.listen_address.clone(),
            source: e,
        })?
        .collect();
    if addrs.is_empty() {
        return Err(StartupError::NoAddress(args.listen_address.clone()));
    }
    // Python's aiohttp binds every address returned by getaddrinfo; do the
    // same so `localhost` covers both ::1 and 127.0.0.1.
    let mut listeners = Vec::with_capacity(addrs.len());
    for addr in &addrs {
        let listener =
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| StartupError::Bind {
                    addr: *addr,
                    source: e,
                })?;
        info!("listening on {addr}");
        listeners.push(listener);
    }

    let mut servers = tokio::task::JoinSet::new();
    for listener in listeners {
        let app = app.clone();
        let server_shutdown = shutdown.clone();
        servers.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
                .await
        });
    }

    // Trigger shutdown on ctrl_c or SIGTERM. Once triggered, the axum
    // servers stop accepting new connections and we await in-flight
    // precaches (background_tasks) before exiting.
    let shutdown_signal = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = wait_for_shutdown().await {
            warn!("failed to install signal handlers: {e}");
        }
        info!("shutdown signal received, draining in-flight tasks");
        shutdown_signal.cancel();
    });

    let mut fatal: Option<StartupError> = None;
    while let Some(res) = servers.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                fatal.get_or_insert(StartupError::Server(e));
                shutdown.cancel();
            }
            Err(e) => {
                fatal.get_or_insert(StartupError::ServerPanic(e));
                shutdown.cancel();
            }
        }
    }

    background_tasks.close();
    background_tasks.wait().await;

    match fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn wait_for_shutdown() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => res,
            _ = term.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderValue;
    use std::str::FromStr;

    fn run_with(build_version: Option<debversion::Version>, build_source: Option<String>) -> Run {
        Run {
            result_code: "success".to_string(),
            build_source,
            campaign: "lintian-fixes".to_string(),
            id: "run-1".to_string(),
            build_version,
        }
    }

    #[test]
    fn build_version_str_renders_or_empty() {
        let v = debversion::Version::from_str("1.2.3-4~jan+lint1").unwrap();
        assert_eq!(
            run_with(Some(v), None).build_version_str(),
            "1.2.3-4~jan+lint1"
        );
        assert_eq!(run_with(None, None).build_version_str(), "");
    }

    #[test]
    fn build_source_str_borrows_or_empty() {
        assert_eq!(run_with(None, None).build_source_str(), "");
        assert_eq!(
            run_with(None, Some("dokujclient".into())).build_source_str(),
            "dokujclient"
        );
    }

    #[test]
    fn negotiate_prefers_specific() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("text/html, text/plain;q=0.5"),
        );
        assert_eq!(
            negotiate(&headers, &["text/plain", "text/html"]),
            Some("text/html"),
        );
    }

    #[test]
    fn negotiate_wildcard_picks_last_supported() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::ACCEPT, HeaderValue::from_static("*/*"));
        assert_eq!(
            negotiate(&headers, &["text/plain", "text/html"]),
            Some("text/html"),
        );
    }

    #[test]
    fn negotiate_diffoscope_default_is_markdown() {
        // Matches Python's mimeparse.best_match: */* against the debdiff
        // list gives text/markdown (last entry).
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::ACCEPT, HeaderValue::from_static("*/*"));
        assert_eq!(
            negotiate(
                &headers,
                &[
                    "text/plain",
                    "text/html",
                    "application/json",
                    "text/markdown"
                ]
            ),
            Some("text/markdown"),
        );
    }

    #[test]
    fn negotiate_returns_none_on_mismatch() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("image/png"),
        );
        assert_eq!(negotiate(&headers, &["text/plain", "text/html"]), None);
    }

    #[test]
    fn negotiate_respects_quality() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("text/plain;q=0.5, text/html"),
        );
        assert_eq!(
            negotiate(&headers, &["text/plain", "text/html"]),
            Some("text/html"),
        );
    }

    #[test]
    fn typed_response_omits_charset() {
        let resp = typed_response("text/plain", "hi");
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/plain",
        );
    }

    #[test]
    fn typed_text_response_adds_charset() {
        let resp = typed_text_response("text/markdown", "hi");
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/markdown; charset=utf-8",
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_text() {
        let counter = prometheus::IntCounter::new("differ_test_counter", "test").unwrap();
        prometheus::default_registry()
            .register(Box::new(counter.clone()))
            .unwrap();
        counter.inc();

        let response = handle_metrics().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            TextEncoder::new().format_type(),
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("differ_test_counter"));
    }

    #[tokio::test]
    async fn error_status_and_body_for_each_variant() {
        use serde_json::Value;

        async fn body_of(resp: Response) -> (StatusCode, Value) {
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: Value = serde_json::from_slice(&bytes).unwrap();
            (status, json)
        }

        let cases: Vec<(Error, StatusCode, &str, Option<&str>)> = vec![
            (
                Error::RunNotFound("r1".into()),
                StatusCode::NOT_FOUND,
                "run-not-found",
                Some("r1"),
            ),
            (
                Error::RunNotSuccessful("r2".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "run-not-successful",
                Some("r2"),
            ),
            (
                Error::ArtifactsMissing("r3".into()),
                StatusCode::NOT_FOUND,
                "artifacts-missing",
                Some("r3"),
            ),
            (
                Error::ArtifactRetrievalTimeout("r4".into()),
                StatusCode::GATEWAY_TIMEOUT,
                "artifact-retrieval-timeout",
                Some("r4"),
            ),
            (
                Error::ArtifactRetrievalFailed {
                    run_id: "r5".into(),
                    reason: "network down".into(),
                },
                StatusCode::BAD_GATEWAY,
                "artifact-retrieval-failed",
                Some("r5"),
            ),
            (
                Error::DiffCommandTimeout("diffoscope"),
                StatusCode::GATEWAY_TIMEOUT,
                "diff-command-timeout",
                None,
            ),
            (
                Error::DiffCommandMemoryError("diffoscope"),
                StatusCode::INSUFFICIENT_STORAGE,
                "diff-command-memory-error",
                None,
            ),
            (
                Error::ContentNegotiationFailed {
                    offered: "text/plain".into(),
                },
                StatusCode::NOT_ACCEPTABLE,
                "not-acceptable",
                None,
            ),
            (
                Error::InvalidRunId("../etc/passwd".into()),
                StatusCode::BAD_REQUEST,
                "invalid-run-id",
                None,
            ),
        ];

        for (err, want_status, want_reason, want_run_id) in cases {
            let resp = err.into_response();
            let unavailable = resp
                .headers()
                .get("unavailable_run_id")
                .map(|v| v.to_str().unwrap().to_string());
            let (status, body) = body_of(resp).await;
            assert_eq!(status, want_status, "status for {want_reason}");
            assert_eq!(body["reason"], want_reason);
            assert!(body["message"].is_string(), "message for {want_reason}");
            assert_eq!(
                unavailable.as_deref(),
                want_run_id,
                "unavailable_run_id header for {want_reason}",
            );
        }
    }

    #[test]
    fn database_error_maps_pool_timeout_to_503() {
        let err = Error::Database(sqlx::Error::PoolTimedOut);
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn check_successful_distinguishes_missing_from_failed() {
        let missing = check_successful("r1", None).unwrap_err();
        assert!(matches!(missing, Error::RunNotFound(id) if id == "r1"));

        let failed = check_successful(
            "r2",
            Some(Run {
                result_code: "failed".to_string(),
                build_source: None,
                campaign: "c".to_string(),
                id: "r2".to_string(),
                build_version: None,
            }),
        )
        .unwrap_err();
        assert!(matches!(failed, Error::RunNotSuccessful(id) if id == "r2"));
    }

    #[test]
    fn validate_run_id_accepts_and_rejects() {
        // Accepts real-world run ids.
        for ok in ["run-1", "abcdef1234567890", "b1c2d3-e4f5", "log_id_42", "A"] {
            assert!(validate_run_id(ok).is_ok(), "should accept {ok:?}");
        }
        // Rejects anything that could escape the cache dir or is empty.
        for bad in [
            "", "..", ".hidden", "a/b", "a\\b", "a b", "foo.json", "foo;bar",
        ] {
            let err = validate_run_id(bad).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRunId(_)),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn inflight_dedupes_and_wakes_waiters() {
        let inflight = InFlight::default();
        let key = ("a".to_string(), "b".to_string());
        let g = match inflight.acquire(key.clone()) {
            InFlightHandle::Generate(g) => g,
            InFlightHandle::Wait(_) => panic!("first acquire should generate"),
        };
        // Second concurrent acquire returns Wait.
        let notify = match inflight.acquire(key.clone()) {
            InFlightHandle::Wait(n) => n,
            InFlightHandle::Generate(_) => panic!("second acquire should wait"),
        };
        // Dropping the guard fires notify_waiters and removes the entry.
        drop(g);
        assert_eq!(
            Arc::strong_count(&notify),
            1,
            "map should have dropped its strong ref"
        );
        // A subsequent acquire generates again.
        assert!(matches!(inflight.acquire(key), InFlightHandle::Generate(_)));
    }

    /// Router-level tests: build a real axum Router with the same
    /// `build_router()` used in production, send requests via
    /// `oneshot()`, and assert on status/headers/body. Handlers that
    /// need a live database are exercised only via paths that fail
    /// before the DB is touched (validation, negotiation, routing).
    mod router {
        use super::*;
        use axum::body::Body;
        use axum::http::{Method, Request};
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
        use tower::ServiceExt;

        /// Build an `AppState` suitable for tests: a lazy pool that
        /// never actually connects and a `LocalArtifactManager`
        /// backed by a temp directory. `Router::oneshot` requests
        /// that never reach the DB or artifact manager succeed
        /// without any external services.
        fn test_state(cache_root: &StdPath, artifacts_root: &StdPath) -> Arc<AppState> {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect_lazy_with(PgConnectOptions::new());
            let artifact_manager = janitor::artifacts::LocalArtifactManager::new(artifacts_root)
                .expect("LocalArtifactManager::new");
            Arc::new(AppState {
                pool,
                artifact_manager: Arc::new(artifact_manager),
                task_memory_limit: 100,
                task_timeout: 10,
                diffoscope_command: "diffoscope".to_string(),
                diffoscope_cache_dir: Some(cache_root.join("diffoscope")),
                debdiff_cache_dir: Some(cache_root.join("debdiff")),
                background_tasks: TaskTracker::new(),
                precache_slots: Arc::new(Semaphore::new(2)),
                debdiff_inflight: Arc::new(InFlight::default()),
                diffoscope_inflight: Arc::new(InFlight::default()),
                metrics: metrics(),
            })
        }

        async fn body_bytes(resp: Response) -> Vec<u8> {
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec()
        }

        async fn body_json(resp: Response) -> serde_json::Value {
            serde_json::from_slice(&body_bytes(resp).await).unwrap()
        }

        fn get(uri: &str) -> Request<Body> {
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        }

        fn get_with_accept(uri: &str, accept: &str) -> Request<Body> {
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .header(axum::http::header::ACCEPT, accept)
                .body(Body::empty())
                .unwrap()
        }

        fn post(uri: &str) -> Request<Body> {
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        }

        #[tokio::test]
        async fn invalid_run_id_in_path_returns_400_json_envelope() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));
            let resp = router
                .oneshot(get("/debdiff/..%2Fetc%2Fpasswd/x"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let body = body_json(resp).await;
            assert_eq!(body["reason"], "invalid-run-id");
        }

        #[tokio::test]
        async fn invalid_run_id_in_precache_returns_400() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));
            let resp = router
                .oneshot(post("/precache/dot.slash/valid-id"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_eq!(body_json(resp).await["reason"], "invalid-run-id");
        }

        #[tokio::test]
        async fn unacceptable_accept_returns_406_before_db() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));
            // Valid ids so path validation passes; Accept mismatch
            // fires before get_run_pair touches the (lazy) DB.
            let resp = router
                .oneshot(get_with_accept("/debdiff/a/b", "image/png"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_ACCEPTABLE);
            let body = body_json(resp).await;
            assert_eq!(body["reason"], "not-acceptable");
            // The envelope lists the types we do offer.
            let msg = body["message"].as_str().unwrap();
            for offered in ["text/x-diff", "text/plain", "text/markdown", "text/html"] {
                assert!(msg.contains(offered), "message missing {offered}: {msg:?}");
            }
        }

        #[tokio::test]
        async fn diffoscope_unacceptable_accept_returns_406() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));
            let resp = router
                .oneshot(get_with_accept("/diffoscope/a/b", "image/png"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_ACCEPTABLE);
            assert_eq!(body_json(resp).await["reason"], "not-acceptable");
        }

        #[tokio::test]
        async fn wrong_method_returns_405() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));

            // /debdiff is GET, POST must be rejected.
            let resp = router.clone().oneshot(post("/debdiff/a/b")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

            // /precache is POST, GET must be rejected.
            let resp = router.clone().oneshot(get("/precache/a/b")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

            // /precache-all is POST, GET must be rejected.
            let resp = router.oneshot(get("/precache-all")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        }

        #[tokio::test]
        async fn unknown_path_returns_404() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));

            let resp = router.clone().oneshot(get("/nope")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);

            // /debdiff expects two segments; one segment must not match.
            let resp = router.oneshot(get("/debdiff/only-one")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn metrics_endpoint_serves_prometheus_text() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));

            // Hit a handler first so counters have at least one
            // series to emit (Prometheus vecs are silent until any
            // label combination is touched).
            let _ = router
                .clone()
                .oneshot(get_with_accept("/debdiff/a/b", "image/png"))
                .await
                .unwrap();

            let resp = router.oneshot(get("/metrics")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let ct = resp
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert!(ct.contains("text/plain"), "unexpected content-type {ct:?}");
            let body = String::from_utf8(body_bytes(resp).await).unwrap();
            assert!(
                body.contains("differ_requests_total"),
                "metrics missing differ_requests_total: {body}"
            );
            assert!(
                body.contains("differ_errors_total"),
                "metrics missing differ_errors_total: {body}"
            );
        }

        #[tokio::test]
        async fn health_with_unreachable_db_surfaces_5xx() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            // `test_state`'s pool uses libpq env defaults, which in CI
            // happily find the test Postgres on localhost:5432.
            // Explicitly point at a port nothing listens on so SELECT
            // 1 is forced to fail -- otherwise this assertion only
            // holds on dev workstations without a local Postgres.
            let bad_opts = PgConnectOptions::new()
                .host("127.0.0.1")
                .port(1)
                .username("nobody")
                .database("janitor-health-test-unreachable");
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_millis(500))
                .connect_lazy_with(bad_opts);
            let artifact_manager = janitor::artifacts::LocalArtifactManager::new(art.path())
                .expect("LocalArtifactManager::new");
            let state = Arc::new(AppState {
                pool,
                artifact_manager: Arc::new(artifact_manager),
                task_memory_limit: 100,
                task_timeout: 10,
                diffoscope_command: "diffoscope".to_string(),
                diffoscope_cache_dir: Some(cache.path().join("diffoscope")),
                debdiff_cache_dir: Some(cache.path().join("debdiff")),
                background_tasks: TaskTracker::new(),
                precache_slots: Arc::new(Semaphore::new(2)),
                debdiff_inflight: Arc::new(InFlight::default()),
                diffoscope_inflight: Arc::new(InFlight::default()),
                metrics: metrics(),
            });
            let router = build_router(state);

            // SELECT 1 against the unreachable pool fails; Error::Database
            // maps to 500 (or 503 for pool-timeout / SQLSTATE 53 — both
            // are acceptable).
            let resp = router.oneshot(get("/health")).await.unwrap();
            let status = resp.status();
            assert!(
                status.is_server_error(),
                "expected 5xx from /health without a real DB, got {status}"
            );
            assert_eq!(body_json(resp).await["reason"], "database-error");
        }

        #[tokio::test]
        async fn precache_all_wrong_method_returns_405() {
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));
            let resp = router.oneshot(get("/precache-all")).await.unwrap();
            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        }

        #[tokio::test]
        async fn precache_records_started_metric_via_request_label() {
            // Verifies the "request" trigger label increments when
            // /precache/... is hit AND that validate_run_id blocks
            // before spawn_precache runs (so a bad id does NOT bump
            // the counter). We only assert on the negative side here
            // because the positive path hits the DB.
            let cache = tempfile::TempDir::new().unwrap();
            let art = tempfile::TempDir::new().unwrap();
            let router = build_router(test_state(cache.path(), art.path()));
            let before = metrics()
                .precache_started
                .with_label_values(&["request"])
                .get();
            let resp = router
                .oneshot(post("/precache/bad..id/valid"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let after = metrics()
                .precache_started
                .with_label_values(&["request"])
                .get();
            assert_eq!(after, before, "invalid id must not bump precache counter");
        }
    }
}
