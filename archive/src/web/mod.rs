//! axum handlers: serve `/dists/...` files, expose `/publish`, `/ready`,
//! `/last-publish`, `/pgp_keys`, `/gpg-key`, `/pool/...`, `/metrics`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::ArchiveConfig;
use crate::database::BuildManager;
use crate::error::{ArchiveError, ArchiveResult};
use crate::manager::GeneratorManager;
use crate::repository::RepositoryGenerator;
use crate::scanner::PackageScanner;

mod shared;

/// Shared map of `apt_repository.name -> last successful publish
/// time`. Populated by the generator manager when a publish task
/// completes; consumed by `/ready` (missing entries -> 500) and
/// `/last-publish` (returned as a `{suite: iso8601}` object).
pub type LastPublishTimes = Arc<RwLock<HashMap<String, chrono::DateTime<chrono::Utc>>>>;

/// Construct an empty [`LastPublishTimes`] handle. Exported so the
/// service entry point in `main.rs` can share the same map between
/// the web service, the generator manager, and the periodic
/// republish loop.
pub fn new_last_publish_times() -> LastPublishTimes {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Shared state cloned into every axum handler.
///
/// `generator_manager` is optional so tests and one-shot generation
/// paths (which don't want a background scheduler) can still build
/// an `AppState`. `last_publish_times` is populated by the manager
/// when a publish job completes.
#[derive(Clone)]
#[allow(missing_docs)]
pub struct AppState {
    pub config: Arc<ArchiveConfig>,
    pub generator: Arc<RepositoryGenerator>,
    pub scanner: Arc<PackageScanner>,
    pub database: Arc<BuildManager>,
    pub generator_manager: Option<Arc<GeneratorManager>>,
    pub last_publish_times: LastPublishTimes,
}

/// Body accepted by `POST /publish`. Plain form body with an
/// optional `campaign` field. Absent campaign means "trigger every
/// apt_repository that consumes a debian_build campaign".
#[derive(Debug, Default, Deserialize)]
pub struct PublishRequest {
    /// Optional campaign filter. When set, only apt_repositories that
    /// select this campaign are triggered.
    pub campaign: Option<String>,
}

/// Response payload for `POST /publish`. Returns `{}` on success.
#[derive(Debug, Default, Serialize)]
pub struct PublishResponse {}

/// Archive web service.
pub struct ArchiveWebService {
    state: AppState,
}

impl ArchiveWebService {
    /// Create a new archive web service. No generator manager and an
    /// empty last-publish map; suited for standalone tooling that
    /// only serves files.
    pub async fn new(
        config: ArchiveConfig,
        generator: RepositoryGenerator,
        scanner: PackageScanner,
        database: BuildManager,
    ) -> ArchiveResult<Self> {
        Self::build(
            config,
            generator,
            scanner,
            database,
            None,
            new_last_publish_times(),
        )
        .await
    }

    /// Create a web service wired to a shared generator manager and
    /// the shared [`LastPublishTimes`] map. Used by the main server
    /// entry point so `/publish`, `/ready`, `/last-publish` all
    /// observe the same publish state.
    pub async fn with_publish_observer(
        config: ArchiveConfig,
        generator: RepositoryGenerator,
        scanner: PackageScanner,
        database: BuildManager,
        generator_manager: Arc<GeneratorManager>,
        last_publish_times: LastPublishTimes,
    ) -> ArchiveResult<Self> {
        Self::build(
            config,
            generator,
            scanner,
            database,
            Some(generator_manager),
            last_publish_times,
        )
        .await
    }

    async fn build(
        config: ArchiveConfig,
        generator: RepositoryGenerator,
        scanner: PackageScanner,
        database: BuildManager,
        generator_manager: Option<Arc<GeneratorManager>>,
        last_publish_times: LastPublishTimes,
    ) -> ArchiveResult<Self> {
        let state = AppState {
            config: Arc::new(config),
            generator: Arc::new(generator),
            scanner: Arc::new(scanner),
            database: Arc::new(database),
            generator_manager,
            last_publish_times,
        };

        Ok(Self { state })
    }

    /// Create the Axum router with all routes.
    pub fn router(&self) -> Router {
        let router = Router::new()
            // Plain "ok" health endpoint. No health aggregation --
            // the periodic services keep their own /health state
            // internally.
            .route("/health", get(shared::health_ok))
            // /ready returns 500 with a list of apt_repository
            // suites that have never been published yet, plaintext
            // body listing whatever *has* been published. Handled
            // here rather than by the shared readiness handler
            // because the shared one only knows about generic
            // health checks and can't tell that a suite is still
            // missing its first Release file.
            .route("/ready", get(archive_ready_handler))
            // Repository serving endpoints
            .route("/dists/{suite}/Release", get(serve_release))
            .route("/dists/{suite}/Release.gpg", get(serve_release_gpg))
            .route("/dists/{suite}/InRelease", get(serve_inrelease))
            // axum 0.8 requires whole-segment captures, so the
            // `binary-:arch` partial captures of axum 0.7 become
            // `{binary_arch}` here; the handlers strip the
            // `binary-` prefix from the captured value.
            .route(
                "/dists/{suite}/{component}/{binary_arch}/Packages",
                get(serve_packages),
            )
            .route(
                "/dists/{suite}/{component}/{binary_arch}/Packages.gz",
                get(serve_packages_gz),
            )
            .route(
                "/dists/{suite}/{component}/{binary_arch}/Packages.bz2",
                get(serve_packages_bz2),
            )
            .route(
                "/dists/{suite}/{component}/source/Sources",
                get(serve_sources),
            )
            .route(
                "/dists/{suite}/{component}/source/Sources.gz",
                get(serve_sources_gz),
            )
            .route(
                "/dists/{suite}/{component}/source/Sources.bz2",
                get(serve_sources_bz2),
            )
            // By-hash serving
            .route(
                "/dists/{suite}/{component}/{binary_arch}/by-hash/{algo}/{hash}",
                get(serve_by_hash),
            )
            .route(
                "/dists/{suite}/{component}/source/by-hash/{algo}/{hash}",
                get(serve_by_hash),
            )
            // On-demand dists: /dists/{kind=cs|run|<campaign>}/{id}/...
            .route(
                "/dists/{kind}/{id}/{file}",
                get(serve_on_demand_release_file),
            )
            .route(
                "/dists/{kind}/{id}/{component}/{binary_arch}/{file}",
                get(serve_on_demand_component_file),
            )
            .route(
                "/dists/{kind}/{id}/{component}/source/{file}",
                get(serve_on_demand_source_file),
            )
            .route(
                "/dists/{kind}/{id}/{component}/{binary_arch}/by-hash/{algo}/{hash}",
                get(serve_on_demand_binary_by_hash),
            )
            .route(
                "/dists/{kind}/{id}/{component}/source/by-hash/{algo}/{hash}",
                get(serve_on_demand_source_by_hash),
            )
            // Publishing and management endpoints
            .route("/publish", post(publish_repository))
            .route("/last-publish", get(last_publish_status))
            .route("/gpg-key", get(serve_gpg_key))
            .route("/pgp_keys", get(handle_pgp_keys))
            // Static file serving for pool. Catch-all uses the
            // axum 0.8 `{*name}` syntax (was `*path` in 0.7).
            .route("/pool/{*path}", get(serve_pool_file))
            .route("/metrics", get(shared::metrics_ok))
            .with_state(self.state.clone());

        // Apply standard middleware. Currently a no-op beyond what
        // individual handlers already do.
        if let Some(ref web_config) = self.state.config.base.web {
            shared::apply_standard_middleware(router, web_config)
        } else {
            let default_web_config = janitor::shared_config::WebConfig::default();
            shared::apply_standard_middleware(router, &default_web_config)
        }
    }

    /// Start the web service on the specified address.
    pub async fn serve(&self, bind_address: &str) -> ArchiveResult<()> {
        let app = self.router();

        let listener = tokio::net::TcpListener::bind(bind_address)
            .await
            .map_err(|e| {
                ArchiveError::InvalidConfiguration(format!(
                    "Failed to bind to {}: {}",
                    bind_address, e
                ))
            })?;

        info!("Archive web service listening on {}", bind_address);

        axum::serve(listener, app)
            .await
            .map_err(|e| ArchiveError::InvalidConfiguration(format!("Server error: {}", e)))?;

        Ok(())
    }
}

/// Serve the on-disk Release file for a configured suite. If the
/// Release file has not been generated yet, return 404. Callers
/// depending on this signal (e.g. deployment scripts polling for
/// first-publish completion) rely on the difference between a real
/// 200 and "still generating".
async fn serve_release(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;

    debug!("Serving Release file for suite: {}", suite);

    let repo_config = state
        .config
        .repositories
        .get(suite)
        .ok_or(StatusCode::NOT_FOUND)?;

    let release_path = repo_config.suite_path().join("Release");
    let content = fs::read(&release_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("text/plain"));
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("public, max-age=300"),
    );
    Ok((headers, content).into_response())
}

/// Serve Release.gpg file.
async fn serve_release_gpg(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;

    debug!("Serving Release.gpg file for suite: {}", suite);

    let repo_config = state
        .config
        .repositories
        .get(suite)
        .ok_or(StatusCode::NOT_FOUND)?;

    let release_gpg_path = repo_config.suite_path().join("Release.gpg");

    match fs::read(&release_gpg_path).await {
        Ok(content) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Content-Type",
                HeaderValue::from_static("application/pgp-signature"),
            );
            headers.insert(
                "Cache-Control",
                HeaderValue::from_static("public, max-age=300"),
            );

            Ok((headers, content).into_response())
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// Serve InRelease file.
async fn serve_inrelease(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;

    debug!("Serving InRelease file for suite: {}", suite);

    let repo_config = state
        .config
        .repositories
        .get(suite)
        .ok_or(StatusCode::NOT_FOUND)?;

    let inrelease_path = repo_config.suite_path().join("InRelease");

    let content = fs::read(&inrelease_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("text/plain"));
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("public, max-age=300"),
    );
    Ok((headers, content).into_response())
}

/// Serve Packages file.
async fn serve_packages(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    let component = params.get("component").ok_or(StatusCode::BAD_REQUEST)?;
    // axum 0.8 captures whole segments only; the route binds the full
    // `binary-<arch>` directory name as `binary_arch`, and we strip
    // the `binary-` prefix here so the rest of the handler keeps the
    // bare arch (`amd64` etc.) it expected before.
    let binary_arch = params.get("binary_arch").ok_or(StatusCode::BAD_REQUEST)?;
    let arch = binary_arch
        .strip_prefix("binary-")
        .ok_or(StatusCode::NOT_FOUND)?;

    debug!(
        "Serving Packages file for {}/{}/binary-{}",
        suite, component, arch
    );

    serve_component_file(
        &state,
        suite,
        component,
        &format!("binary-{}/Packages", arch),
        "text/plain",
    )
    .await
}

/// Serve compressed Packages file.
async fn serve_packages_gz(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    let component = params.get("component").ok_or(StatusCode::BAD_REQUEST)?;
    // axum 0.8 captures whole segments only; the route binds the full
    // `binary-<arch>` directory name as `binary_arch`, and we strip
    // the `binary-` prefix here so the rest of the handler keeps the
    // bare arch (`amd64` etc.) it expected before.
    let binary_arch = params.get("binary_arch").ok_or(StatusCode::BAD_REQUEST)?;
    let arch = binary_arch
        .strip_prefix("binary-")
        .ok_or(StatusCode::NOT_FOUND)?;

    serve_component_file(
        &state,
        suite,
        component,
        &format!("binary-{}/Packages.gz", arch),
        "application/gzip",
    )
    .await
}

/// Serve bzip2 compressed Packages file.
async fn serve_packages_bz2(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    let component = params.get("component").ok_or(StatusCode::BAD_REQUEST)?;
    // axum 0.8 captures whole segments only; the route binds the full
    // `binary-<arch>` directory name as `binary_arch`, and we strip
    // the `binary-` prefix here so the rest of the handler keeps the
    // bare arch (`amd64` etc.) it expected before.
    let binary_arch = params.get("binary_arch").ok_or(StatusCode::BAD_REQUEST)?;
    let arch = binary_arch
        .strip_prefix("binary-")
        .ok_or(StatusCode::NOT_FOUND)?;

    serve_component_file(
        &state,
        suite,
        component,
        &format!("binary-{}/Packages.bz2", arch),
        "application/x-bzip2",
    )
    .await
}

/// Serve Sources file.
async fn serve_sources(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    let component = params.get("component").ok_or(StatusCode::BAD_REQUEST)?;

    serve_component_file(&state, suite, component, "source/Sources", "text/plain").await
}

/// Serve compressed Sources file.
async fn serve_sources_gz(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    let component = params.get("component").ok_or(StatusCode::BAD_REQUEST)?;

    serve_component_file(
        &state,
        suite,
        component,
        "source/Sources.gz",
        "application/gzip",
    )
    .await
}

/// Serve bzip2 compressed Sources file.
async fn serve_sources_bz2(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    let component = params.get("component").ok_or(StatusCode::BAD_REQUEST)?;

    serve_component_file(
        &state,
        suite,
        component,
        "source/Sources.bz2",
        "application/x-bzip2",
    )
    .await
}

/// Serve by-hash files.
async fn serve_by_hash(
    Path(params): Path<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let suite = params.get("suite").ok_or(StatusCode::BAD_REQUEST)?;
    let component = params.get("component").ok_or(StatusCode::BAD_REQUEST)?;
    let arch = params.get("arch");
    let algo = params.get("algo").ok_or(StatusCode::BAD_REQUEST)?;
    let hash = params.get("hash").ok_or(StatusCode::BAD_REQUEST)?;

    let repo_config = state
        .config
        .repositories
        .get(suite)
        .ok_or(StatusCode::NOT_FOUND)?;

    let by_hash_path = if let Some(arch) = arch {
        // Binary by-hash: /dists/suite/component/binary-arch/by-hash/algo/hash
        repo_config
            .component_arch_path(component, arch)
            .join("by-hash")
            .join(algo)
            .join(hash)
    } else {
        // Source by-hash: /dists/suite/component/source/by-hash/algo/hash
        repo_config
            .source_path(component)
            .join("by-hash")
            .join(algo)
            .join(hash)
    };

    match fs::read(&by_hash_path).await {
        Ok(content) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                "Content-Type",
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(
                "Cache-Control",
                HeaderValue::from_static("public, max-age=86400"),
            ); // 24 hours

            Ok((headers, content).into_response())
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// Serve pool files (package .deb files).
async fn serve_pool_file(
    Path(path): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    debug!("Serving pool file: {}", path);

    // Construct the full path to the pool file
    // Pool files are typically stored outside the dists directory
    let pool_path = state
        .config
        .repositories
        .values()
        .next()
        .map(|repo| repo.base_path.join("pool").join(&path))
        .ok_or(StatusCode::NOT_FOUND)?;

    match fs::read(&pool_path).await {
        Ok(content) => {
            let mut headers = HeaderMap::new();

            // Set appropriate content type based on file extension
            if path.ends_with(".deb") {
                headers.insert(
                    "Content-Type",
                    HeaderValue::from_static("application/vnd.debian.binary-package"),
                );
            } else if path.ends_with(".dsc") {
                headers.insert("Content-Type", HeaderValue::from_static("text/plain"));
            } else if path.ends_with(".tar.gz") || path.ends_with(".tar.xz") {
                headers.insert(
                    "Content-Type",
                    HeaderValue::from_static("application/x-tar"),
                );
            } else {
                headers.insert(
                    "Content-Type",
                    HeaderValue::from_static("application/octet-stream"),
                );
            }

            headers.insert(
                "Cache-Control",
                HeaderValue::from_static("public, max-age=86400"),
            ); // 24 hours

            Ok((headers, content).into_response())
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// `POST /publish` -- trigger a republish across every configured
/// apt_repository:
///
/// - reads an optional `campaign` field from the form body
/// - iterates every `config.campaign` with a `debian_build` block
/// - filters by `campaign` when supplied
/// - dispatches to `generator_manager.trigger_campaign` for each match
/// - returns `{}` on success
/// Enumerate the campaigns `/publish` should iterate over. Walks
/// `runtime_config.campaign` and skips those without a
/// `debian_build` block. When no protobuf runtime config is
/// available we fall back to the manager's fan-out map keys so
/// env-only deployments still have something to iterate over.
pub(crate) fn publish_campaign_candidates(
    runtime_config: Option<&janitor::config::Config>,
    fallback_campaigns: impl IntoIterator<Item = String>,
) -> Vec<String> {
    if let Some(rt) = runtime_config {
        rt.campaign
            .iter()
            .filter(|c| c.has_debian_build())
            .filter_map(|c| c.name.clone())
            .collect()
    } else {
        fallback_campaigns.into_iter().collect()
    }
}

/// Apply the optional `campaign` filter from a `/publish` request
/// body. Returns the list of campaign names to actually trigger.
/// Absent filter -> pass through; present filter keeps only the
/// candidate whose name matches exactly.
pub(crate) fn publish_apply_filter<'a>(
    candidates: &'a [String],
    filter: Option<&str>,
) -> Vec<&'a str> {
    candidates
        .iter()
        .map(String::as_str)
        .filter(|name| match filter {
            Some(f) => f == *name,
            None => true,
        })
        .collect()
}

async fn publish_repository(
    State(state): State<AppState>,
    Form(request): Form<PublishRequest>,
) -> Result<Json<PublishResponse>, StatusCode> {
    info!("Repository publish request: {:?}", request);

    let Some(manager) = state.generator_manager.as_ref() else {
        warn!("/publish invoked but no GeneratorManager is wired up");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let fallback: Vec<String> = if state.config.runtime_config.is_some() {
        Vec::new()
    } else {
        manager.get_campaign_mapping().await.into_keys().collect()
    };
    let candidates = publish_campaign_candidates(state.config.runtime_config.as_deref(), fallback);

    for campaign_name in publish_apply_filter(&candidates, request.campaign.as_deref()) {
        if let Err(e) = manager.trigger_campaign(campaign_name).await {
            warn!(
                "trigger_campaign({}) failed during /publish: {}",
                campaign_name, e
            );
        }
    }

    Ok(Json(PublishResponse::default()))
}

/// Serialize the shared last-publish map into the `{suite:
/// iso8601}` shape `/last-publish` returns. Kept pure so tests
/// don't need to construct an entire AppState.
pub(crate) fn last_publish_json(
    times: &HashMap<String, chrono::DateTime<chrono::Utc>>,
) -> HashMap<String, String> {
    times
        .iter()
        .map(|(name, dt)| (name.clone(), dt.to_rfc3339()))
        .collect()
}

/// `GET /last-publish` -- return the last successful publish time
/// for each apt_repository as a JSON object `{suite: iso8601}`.
async fn last_publish_status(State(state): State<AppState>) -> Json<HashMap<String, String>> {
    let times = state.last_publish_times.read().await;
    Json(last_publish_json(&times))
}

/// Compute the (status, body) pair for `/ready` from the current
/// last-publish map and the configured repository list. `Ok(body)`
/// -> 200; `Err(body)` -> 500. Extracted so tests can assert on
/// exact response shape without constructing an entire AppState.
pub(crate) fn compute_ready_response<'a>(
    repositories: impl IntoIterator<Item = &'a str>,
    times: &HashMap<String, chrono::DateTime<chrono::Utc>>,
) -> Result<String, String> {
    let mut missing: Vec<&str> = Vec::new();
    for name in repositories {
        if !times.contains_key(name) {
            missing.push(name);
        }
    }

    let mut status_body = String::new();
    for (name, dt) in times.iter() {
        status_body.push_str(&format!("{}: {}\n", name, dt.to_rfc3339()));
    }

    if !missing.is_empty() {
        Err(format!(
            "missing: {}\n\npresent:\n{}",
            missing.join(", "),
            status_body
        ))
    } else {
        Ok(status_body)
    }
}

/// `GET /ready` -- 500 with a list of apt_repositories that have
/// never been published, plaintext body listing those that have.
async fn archive_ready_handler(State(state): State<AppState>) -> Response {
    let times = state.last_publish_times.read().await;
    match compute_ready_response(
        state.config.repositories.values().map(|r| r.name.as_str()),
        &times,
    ) {
        Ok(body) => (StatusCode::OK, body).into_response(),
        Err(body) => (StatusCode::INTERNAL_SERVER_ERROR, body).into_response(),
    }
}

/// Serve GPG public key.
async fn serve_gpg_key(State(state): State<AppState>) -> Result<Response, StatusCode> {
    if let Some(gpg_config) = &state.config.gpg {
        // Try to export the public key using gpg command
        let mut cmd = tokio::process::Command::new("gpg");

        // Set GPG home directory if specified
        if let Some(gpg_home) = &gpg_config.gpg_home {
            cmd.arg("--homedir").arg(gpg_home);
        }

        // Export the public key in ASCII armor format
        cmd.args(["--armor", "--export", &gpg_config.key_id]);

        match cmd.output().await {
            Ok(output) => {
                if output.status.success() {
                    let key_data = String::from_utf8_lossy(&output.stdout);

                    if key_data.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----") {
                        let mut headers = HeaderMap::new();
                        headers.insert(
                            "Content-Type",
                            HeaderValue::from_static("application/pgp-keys"),
                        );
                        headers.insert(
                            "Cache-Control",
                            HeaderValue::from_static("public, max-age=86400"), // 24 hours
                        );

                        Ok((headers, key_data.to_string()).into_response())
                    } else {
                        warn!(
                            "GPG export returned unexpected output for key {}",
                            gpg_config.key_id
                        );
                        Err(StatusCode::INTERNAL_SERVER_ERROR)
                    }
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!(
                        "GPG export failed for key {}: {}",
                        gpg_config.key_id, stderr
                    );
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
            Err(e) => {
                warn!("Failed to execute gpg command: {}", e);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        debug!("No GPG configuration available");
        Err(StatusCode::NOT_FOUND)
    }
}

/// `GET /pgp_keys` -- return the configured PGP public keys as a
/// JSON array of armored strings.
///
/// The archive config currently carries a single key_id, so the
/// array will have 0 or 1 entries. When no GPG is configured,
/// returns an empty JSON array.
async fn handle_pgp_keys(State(state): State<AppState>) -> Response {
    let mut keys: Vec<String> = Vec::new();
    if let Some(gpg_config) = &state.config.gpg {
        let mut cmd = tokio::process::Command::new("gpg");
        if let Some(gpg_home) = &gpg_config.gpg_home {
            cmd.arg("--homedir").arg(gpg_home);
        }
        cmd.args(["--armor", "--export", &gpg_config.key_id]);
        match cmd.output().await {
            Ok(output) if output.status.success() => {
                let key_data = String::from_utf8_lossy(&output.stdout).into_owned();
                if key_data.starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----") {
                    keys.push(key_data);
                }
            }
            Ok(output) => {
                warn!(
                    "GPG export failed for key {}: {}",
                    gpg_config.key_id,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(e) => {
                warn!("Failed to execute gpg command: {}", e);
            }
        }
    }
    Json(keys).into_response()
}

/// Helper function to serve component files.
async fn serve_component_file(
    state: &AppState,
    suite: &str,
    component: &str,
    file_path: &str,
    content_type: &str,
) -> Result<Response, StatusCode> {
    let repo_config = state
        .config
        .repositories
        .get(suite)
        .ok_or(StatusCode::NOT_FOUND)?;

    let full_path = repo_config.suite_path().join(component).join(file_path);

    // 404 on missing artefacts rather than synthesising an empty
    // response -- otherwise a botched publish reads as success to
    // any client polling for freshness.
    let content = fs::read(&full_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_str(content_type).unwrap());
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("public, max-age=300"),
    );
    Ok((headers, content).into_response())
}

/// Return true when `kind` is a recognized on-demand dispatcher
/// segment. `cs` and `run` are always allowed; anything else must
/// match a configured campaign.
///
/// Extracted so tests can hit the same logic the handler uses
/// without spinning up an axum router.
pub(crate) fn is_valid_on_demand_kind(
    kind: &str,
    runtime_config: Option<&janitor::config::Config>,
    archive_config: &ArchiveConfig,
) -> bool {
    if kind.is_empty() {
        return false;
    }
    if matches!(kind, "cs" | "run") {
        return true;
    }
    if let Some(rt) = runtime_config {
        return rt.campaign.iter().any(|c| c.name.as_deref() == Some(kind));
    }
    // Env-only deployments: fall back to the repository suite set.
    archive_config
        .repositories
        .values()
        .any(|r| r.suite == kind)
}

/// Ensure on-demand dists exist for `(kind, id)`, returning the
/// directory under which generated files should live. Returns
/// `Ok(None)` on database NotFound so the caller can emit 404.
async fn prepare_on_demand(
    state: &AppState,
    kind: &str,
    id: &str,
) -> Result<Option<PathBuf>, StatusCode> {
    let dists_dir = state.config.archive_path.join("dists");

    if !is_valid_on_demand_kind(kind, state.config.runtime_config.as_deref(), &state.config) {
        debug!("Rejecting on-demand kind {}: not a known campaign", kind);
        return Ok(None);
    }

    // Prefer the repository config whose `suite` matches the
    // caller's `kind` (campaign name); honour the per-repository
    // components/architectures on AptRepositoryConfig when it does.
    // Fall back to the first repository entry (origin only) plus
    // `main` + default_architectures when we can't match.
    let matching_repo = state.config.repositories.values().find(|r| r.suite == kind);
    let (components, arches, origin) = if let Some(repo) = matching_repo {
        (
            repo.components.clone(),
            repo.architectures.clone(),
            repo.origin.clone(),
        )
    } else {
        let fallback_arches = if state.config.default_architectures.is_empty() {
            vec!["amd64".to_string()]
        } else {
            state.config.default_architectures.clone()
        };
        let fallback_origin = state
            .config
            .repositories
            .values()
            .next()
            .map(|r| r.origin.clone())
            .unwrap_or_else(|| "janitor".to_string());
        (vec!["main".to_string()], fallback_arches, fallback_origin)
    };

    match crate::on_demand::refresh_on_demand_dists(
        &dists_dir,
        state.database.as_ref(),
        state.scanner.clone(),
        &origin,
        &components,
        &arches,
        kind,
        id,
        state.config.gpg.as_ref(),
        state.config.runtime_config.as_deref(),
    )
    .await
    {
        Ok(()) => Ok(Some(dists_dir.join(kind).join(id))),
        Err(ArchiveError::NotFound(msg)) => {
            debug!("on-demand dists not found: {}", msg);
            Ok(None)
        }
        Err(e) => {
            warn!(
                "Failed to refresh on-demand dists for {}/{}: {}",
                kind, id, e
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn read_and_respond(
    path: PathBuf,
    content_type: &'static str,
) -> Result<Response, StatusCode> {
    match fs::read(&path).await {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert("Content-Type", HeaderValue::from_static(content_type));
            Ok((headers, bytes).into_response())
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

/// `GET /dists/:kind/:id/:file` -- serve Release, Release.gpg, or
/// InRelease from an on-demand dists tree.
async fn serve_on_demand_release_file(
    Path((kind, id, file)): Path<(String, String, String)>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    if !matches!(file.as_str(), "Release" | "Release.gpg" | "InRelease") {
        return Err(StatusCode::NOT_FOUND);
    }
    let base: PathBuf = match prepare_on_demand(&state, &kind, &id).await? {
        Some(p) => p,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let content_type = if file == "Release.gpg" {
        "application/pgp-signature"
    } else {
        "text/plain"
    };
    read_and_respond(base.join(&file), content_type).await
}

/// Map a Packages-family filename to its content type. `None` if
/// the filename is not a recognized Packages variant -- the handler
/// must then return 404. Pulled out as a pure helper so the
/// classification can be unit-tested.
fn packages_content_type(file: &str) -> Option<&'static str> {
    match file {
        "Packages" => Some("text/plain"),
        "Packages.gz" => Some("application/gzip"),
        "Packages.bz2" => Some("application/x-bzip2"),
        _ => None,
    }
}

/// Map a Sources-family filename to its content type. `None` if
/// the filename is not a recognized Sources variant.
fn sources_content_type(file: &str) -> Option<&'static str> {
    match file {
        "Sources" => Some("text/plain"),
        "Sources.gz" => Some("application/gzip"),
        "Sources.bz2" => Some("application/x-bzip2"),
        _ => None,
    }
}

/// `GET /dists/:kind/:id/:component/binary-:arch/:file` -- serve
/// Packages / Packages.gz / Packages.bz2 from an on-demand dists tree.
async fn serve_on_demand_component_file(
    Path((kind, id, component, arch, file)): Path<(String, String, String, String, String)>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let base: PathBuf = match prepare_on_demand(&state, &kind, &id).await? {
        Some(p) => p,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let path = base
        .join(&component)
        .join(format!("binary-{}", arch))
        .join(&file);
    let content_type = packages_content_type(&file).ok_or(StatusCode::NOT_FOUND)?;
    read_and_respond(path, content_type).await
}

/// `GET /dists/:kind/:id/:component/source/:file` -- serve
/// Sources / Sources.gz / Sources.bz2.
async fn serve_on_demand_source_file(
    Path((kind, id, component, file)): Path<(String, String, String, String)>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let base: PathBuf = match prepare_on_demand(&state, &kind, &id).await? {
        Some(p) => p,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let path = base.join(&component).join("source").join(&file);
    let content_type = sources_content_type(&file).ok_or(StatusCode::NOT_FOUND)?;
    read_and_respond(path, content_type).await
}

/// `GET /dists/:kind/:id/:component/binary-:arch/by-hash/:algo/:hash`
async fn serve_on_demand_binary_by_hash(
    Path((kind, id, component, arch, algo, hash)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
    )>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let base: PathBuf = match prepare_on_demand(&state, &kind, &id).await? {
        Some(p) => p,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let path = base
        .join(&component)
        .join(format!("binary-{}", arch))
        .join("by-hash")
        .join(&algo)
        .join(&hash);
    read_and_respond(path, "application/octet-stream").await
}

/// `GET /dists/:kind/:id/:component/source/by-hash/:algo/:hash`
async fn serve_on_demand_source_by_hash(
    Path((kind, id, component, algo, hash)): Path<(String, String, String, String, String)>,
    State(state): State<AppState>,
) -> Result<Response, StatusCode> {
    let base: PathBuf = match prepare_on_demand(&state, &kind, &id).await? {
        Some(p) => p,
        None => return Err(StatusCode::NOT_FOUND),
    };
    let path = base
        .join(&component)
        .join("source")
        .join("by-hash")
        .join(&algo)
        .join(&hash);
    read_and_respond(path, "application/octet-stream").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packages_content_type_known_variants() {
        assert_eq!(packages_content_type("Packages"), Some("text/plain"));
        assert_eq!(
            packages_content_type("Packages.gz"),
            Some("application/gzip")
        );
        assert_eq!(
            packages_content_type("Packages.bz2"),
            Some("application/x-bzip2")
        );
    }

    #[test]
    fn test_packages_content_type_unknown_returns_none() {
        // Unknown filename -> handler must 404. Common attempts an
        // attacker might make to trick the dispatcher:
        assert_eq!(packages_content_type("Packages.xz"), None);
        assert_eq!(packages_content_type("packages"), None); // wrong case
        assert_eq!(packages_content_type("Sources"), None); // wrong family
        assert_eq!(packages_content_type(""), None);
        assert_eq!(packages_content_type("../etc/passwd"), None);
    }

    #[test]
    fn test_sources_content_type_known_variants() {
        assert_eq!(sources_content_type("Sources"), Some("text/plain"));
        assert_eq!(sources_content_type("Sources.gz"), Some("application/gzip"));
        assert_eq!(
            sources_content_type("Sources.bz2"),
            Some("application/x-bzip2")
        );
    }

    #[test]
    fn test_sources_content_type_unknown_returns_none() {
        assert_eq!(sources_content_type("Sources.xz"), None);
        assert_eq!(sources_content_type("sources"), None);
        assert_eq!(sources_content_type("Packages"), None);
        assert_eq!(sources_content_type(""), None);
    }

    #[tokio::test]
    async fn test_health_ok_returns_ok_body() {
        // /health returns text "ok" -- some deployment probes
        // assert on the exact body.
        use axum::body::to_bytes;
        let response = shared::health_ok().await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    /// `/last-publish` returns `{suite: iso8601}` (RFC 3339). Empty
    /// map yields an empty object; a single entry yields one key.
    #[test]
    fn last_publish_json_maps_utc_to_rfc3339() {
        use chrono::{DateTime, Utc};
        let times: HashMap<String, DateTime<Utc>> = HashMap::new();
        assert!(last_publish_json(&times).is_empty());

        let mut times: HashMap<String, DateTime<Utc>> = HashMap::new();
        let ts: DateTime<Utc> = "2026-01-15T12:34:56+00:00".parse().unwrap();
        times.insert("unstable".to_string(), ts);
        let out = last_publish_json(&times);
        assert_eq!(out.len(), 1);
        // to_rfc3339 with a UTC timestamp yields `+00:00`, not `Z`.
        assert_eq!(out.get("unstable").unwrap(), "2026-01-15T12:34:56+00:00");
    }

    /// `/ready` returns 500 with a `missing: ...` header when at
    /// least one configured suite hasn't been published yet.
    #[test]
    fn compute_ready_response_missing_suite_returns_err() {
        let times = HashMap::new();
        let result = compute_ready_response(vec!["unstable", "stable"], &times);
        let body = result.expect_err("expected 500 body");
        assert!(body.starts_with("missing: "));
        // Order isn't guaranteed by config.repositories.values() so
        // check both names appear.
        assert!(body.contains("unstable"));
        assert!(body.contains("stable"));
        assert!(body.contains("\n\npresent:\n"));
    }

    /// When every suite has a publish time recorded, `/ready`
    /// returns 200 with the present-list as body.
    #[test]
    fn compute_ready_response_all_published_returns_ok() {
        use chrono::{DateTime, Utc};
        let ts: DateTime<Utc> = "2026-01-15T12:34:56+00:00".parse().unwrap();
        let mut times = HashMap::new();
        times.insert("unstable".to_string(), ts);
        times.insert("stable".to_string(), ts);
        let body = compute_ready_response(vec!["unstable", "stable"], &times).unwrap();
        assert!(body.contains("unstable: "));
        assert!(body.contains("stable: "));
        assert!(!body.starts_with("missing:"));
    }

    /// Partial publish state: one suite present, one missing. Must
    /// 500 and mention only the missing one in the `missing:`
    /// header while still listing the present one below.
    #[test]
    fn compute_ready_response_partial_missing() {
        use chrono::{DateTime, Utc};
        let ts: DateTime<Utc> = "2026-01-15T12:34:56+00:00".parse().unwrap();
        let mut times = HashMap::new();
        times.insert("stable".to_string(), ts);
        let body = compute_ready_response(vec!["unstable", "stable"], &times)
            .expect_err("expected 500 body");
        // Only unstable is missing.
        let missing_line = body.lines().next().unwrap();
        assert_eq!(missing_line, "missing: unstable");
        assert!(body.contains("stable: 2026-01-15T12:34:56+00:00"));
    }

    /// Empty repository list + empty times: 200 with empty body.
    /// A deployment with no apt_repositories configured is still
    /// considered ready -- the loop over configured repositories
    /// yields nothing, so `missing` stays empty.
    #[test]
    fn compute_ready_response_no_repos_is_ready() {
        let times = HashMap::new();
        let body = compute_ready_response(Vec::<&str>::new(), &times).unwrap();
        assert!(body.is_empty());
    }

    fn empty_archive_config() -> ArchiveConfig {
        ArchiveConfig {
            archive_path: PathBuf::from("/tmp/archive"),
            ..Default::default()
        }
    }

    /// `cs` and `run` are unconditionally valid on-demand kinds.
    /// Must hold even when there's no runtime config.
    #[test]
    fn is_valid_on_demand_kind_cs_and_run_always_valid() {
        let ac = empty_archive_config();
        assert!(is_valid_on_demand_kind("cs", None, &ac));
        assert!(is_valid_on_demand_kind("run", None, &ac));
    }

    /// Empty kind (`""`) is rejected.
    #[test]
    fn is_valid_on_demand_kind_empty_rejected() {
        let ac = empty_archive_config();
        assert!(!is_valid_on_demand_kind("", None, &ac));
    }

    /// Campaign names declared in the runtime protobuf config are
    /// valid. Anything else is rejected. Guards against the
    /// original bug where the port accepted any string and deferred
    /// validation to a downstream SQL query that quietly returned
    /// an empty set.
    #[test]
    fn is_valid_on_demand_kind_matches_runtime_campaigns() {
        let cfg = janitor::config::read_string(
            r#"
                campaign { name: "lintian-fixes" }
                campaign { name: "fresh-releases" }
            "#,
        )
        .unwrap();
        let ac = empty_archive_config();
        assert!(is_valid_on_demand_kind("lintian-fixes", Some(&cfg), &ac));
        assert!(is_valid_on_demand_kind("fresh-releases", Some(&cfg), &ac));
        assert!(!is_valid_on_demand_kind(
            "unknown-campaign",
            Some(&cfg),
            &ac
        ));
        assert!(!is_valid_on_demand_kind("../etc/passwd", Some(&cfg), &ac));
    }

    /// `/publish` candidate list must skip campaigns that lack a
    /// `debian_build { ... }` block -- only debian_build campaigns
    /// are iterated.
    #[test]
    fn publish_campaign_candidates_skips_non_debian() {
        let cfg = janitor::config::read_string(
            r#"
                campaign {
                    name: "lintian-fixes"
                    debian_build { base_distribution: "unstable" }
                }
                campaign { name: "no-debian-build" }
                campaign {
                    name: "fresh-releases"
                    debian_build { base_distribution: "unstable" }
                }
            "#,
        )
        .unwrap();
        let mut got = publish_campaign_candidates(Some(&cfg), std::iter::empty());
        got.sort();
        assert_eq!(got, vec!["fresh-releases", "lintian-fixes"]);
    }

    /// Without runtime config, candidates come from the fallback
    /// iterator (typically the manager's campaign->repo mapping
    /// keys). This is the env-only deployment path.
    #[test]
    fn publish_campaign_candidates_uses_fallback_when_no_runtime() {
        let fallback = vec!["lintian-fixes".to_string(), "unstable".to_string()];
        let got = publish_campaign_candidates(None, fallback);
        // Order preserved from the fallback iterator.
        assert_eq!(got, vec!["lintian-fixes", "unstable"]);
    }

    /// `?campaign=X` restricts the iteration to a single campaign
    /// -- only the exact-match candidate survives.
    #[test]
    fn publish_apply_filter_selects_named_campaign() {
        let candidates = vec![
            "lintian-fixes".to_string(),
            "fresh-releases".to_string(),
            "unchanged".to_string(),
        ];
        let filtered = publish_apply_filter(&candidates, Some("fresh-releases"));
        assert_eq!(filtered, vec!["fresh-releases"]);
    }

    /// A `campaign` filter that doesn't match any candidate must
    /// yield an empty list -- no trigger fires.
    #[test]
    fn publish_apply_filter_no_match_returns_empty() {
        let candidates = vec!["lintian-fixes".to_string()];
        let filtered = publish_apply_filter(&candidates, Some("unknown"));
        assert!(filtered.is_empty());
    }

    /// Absent filter (`None`) returns every candidate. This is the
    /// default when the form body has no `campaign` field.
    #[test]
    fn publish_apply_filter_none_returns_all() {
        let candidates = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let filtered = publish_apply_filter(&candidates, None);
        assert_eq!(filtered, vec!["a", "b", "c"]);
    }

    /// Without a runtime config we fall back to the repository
    /// suite set. Deployments that ship apt_repositories via env
    /// vars only should still get campaign-aware validation.
    #[test]
    fn is_valid_on_demand_kind_fallback_uses_repo_suites() {
        let mut ac = empty_archive_config();
        ac.repositories.insert(
            "lintian-fixes".to_string(),
            crate::config::AptRepositoryConfig {
                name: "lintian-fixes".to_string(),
                description: "".to_string(),
                origin: "Janitor".to_string(),
                label: "lintian-fixes".to_string(),
                suite: "lintian-fixes".to_string(),
                codename: "lintian-fixes".to_string(),
                architectures: vec!["amd64".to_string()],
                components: vec!["main".to_string()],
                base_url: "http://x/".to_string(),
                base_path: PathBuf::from("/x"),
                by_hash: true,
            },
        );
        assert!(is_valid_on_demand_kind("lintian-fixes", None, &ac));
        assert!(!is_valid_on_demand_kind("fresh-releases", None, &ac));
    }
}
