//! Health, metrics, middleware and worker authentication helpers shared
//! by the web services.

use axum::{
    extract::{DefaultBodyLimit, MatchedPath, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use prometheus::{HistogramVec, IntCounterVec};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

/// Per-request counter labelled by method / route / status, named like
/// the `aiohttp_openmetrics` counterpart so dashboards keep working.
fn http_requests_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        prometheus::register_int_counter_vec!(
            "http_requests_total",
            "Total HTTP requests handled",
            &["method", "path", "status"]
        )
        .expect("register http_requests_total")
    })
}

/// Per-request latency histogram (seconds). The slow buckets cover
/// long-running smart-protocol pushes.
fn http_request_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        prometheus::register_histogram_vec!(
            prometheus::histogram_opts!(
                "http_request_duration_seconds",
                "HTTP request duration in seconds",
                vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]
            ),
            &["method", "path"]
        )
        .expect("register http_request_duration_seconds")
    })
}

/// axum middleware that records request-count and latency metrics.
///
/// The path label uses `MatchedPath` when available so `/foo/{id}`
/// buckets identically for every `id`; otherwise falls back to the
/// raw URI path.
pub async fn record_http_metrics(req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_string();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let started = Instant::now();
    let response = next.run(req).await;
    let elapsed = started.elapsed().as_secs_f64();

    let status = response.status().as_u16().to_string();
    http_requests_total()
        .with_label_values(&[&method, &path, &status])
        .inc();
    http_request_duration_seconds()
        .with_label_values(&[&method, &path])
        .observe(elapsed);

    response
}

/// Backs `/health` and `/ready`. `/ready` runs `SELECT 1` when a pool
/// is attached; `/health` always reports alive.
#[derive(Default)]
pub struct HealthChecker {
    db: Option<sqlx::PgPool>,
}

impl HealthChecker {
    /// Create a new health checker with no database dependency.
    pub fn new() -> Self {
        Self { db: None }
    }

    /// Attach a database pool: `/ready` will probe it with `SELECT 1`.
    pub fn with_db(mut self, pool: sqlx::PgPool) -> Self {
        self.db = Some(pool);
        self
    }

    async fn is_ready(&self) -> bool {
        match &self.db {
            Some(pool) => crate::database::health_check(pool).await.is_ok(),
            None => true,
        }
    }
}

/// `GET /health`: alive, independent of downstream state.
pub async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// `GET /ready`: alive and the configured database is reachable.
pub async fn ready_handler(State(checker): State<Arc<HealthChecker>>) -> impl IntoResponse {
    if checker.is_ready().await {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// `GET /metrics`: Prometheus text-format exposition of every
/// registered metric.
pub async fn metrics_handler() -> Response {
    use prometheus::{Encoder, TextEncoder};

    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&prometheus::gather(), &mut buffer) {
        tracing::error!("failed to encode prometheus metrics: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain")],
            "failed to gather metrics".to_string(),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, encoder.format_type())],
        buffer,
    )
        .into_response()
}

/// Router serving `/health`, `/ready` and `/metrics`.
pub fn health_router(checker: Arc<HealthChecker>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(checker)
}

/// Attach the standard middleware stack: request metrics, body size
/// limit, timeout, compression, request logging and optionally
/// permissive CORS.
///
/// `client_max_size` bounds request bodies in bytes; 0 means unlimited.
pub fn apply_standard_middleware<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    client_max_size: usize,
    request_timeout: Duration,
    cors: bool,
) -> Router<S> {
    let (body_limit, max_request_size) = if client_max_size == 0 {
        (DefaultBodyLimit::disable(), usize::MAX)
    } else {
        (DefaultBodyLimit::max(client_max_size), client_max_size)
    };
    let mut router = router
        .layer(body_limit)
        .layer(axum::middleware::from_fn(record_http_metrics))
        .layer(RequestBodyLimitLayer::new(max_request_size))
        .layer(TimeoutLayer::new(request_timeout))
        .layer(CompressionLayer::new());
    if cors {
        router = router.layer(
            CorsLayer::new()
                .allow_methods(Any)
                .allow_origin(Any)
                .allow_headers(Any),
        );
    }
    router.layer(TraceLayer::new_for_http())
}

/// Extract the username and password from an HTTP Basic
/// `Authorization` header. `None` if it is missing or malformed.
pub fn basic_auth_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    let decoded = String::from_utf8(BASE64.decode(encoded).ok()?).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

/// Return the worker name if the request carries HTTP Basic
/// credentials matching a row in the `worker` table.
///
/// The password comparison runs inside Postgres via `crypt($2, ...)`
/// so the hash algorithm matches whatever pgcrypto is configured to
/// use.
pub async fn authenticated_worker(
    pool: &sqlx::PgPool,
    headers: &HeaderMap,
) -> Result<Option<String>, sqlx::Error> {
    let Some((username, password)) = basic_auth_credentials(headers) else {
        return Ok(None);
    };
    sqlx::query_scalar("SELECT name FROM worker WHERE name = $1 AND password = crypt($2, password)")
        .bind(username)
        .bind(password)
        .fetch_optional(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn basic_auth_credentials_decodes() {
        let headers = headers_with_auth(&format!("Basic {}", BASE64.encode("worker:pa:ss")));
        assert_eq!(
            basic_auth_credentials(&headers),
            Some(("worker".to_string(), "pa:ss".to_string()))
        );
    }

    #[test]
    fn basic_auth_credentials_rejects_malformed() {
        assert_eq!(basic_auth_credentials(&HeaderMap::new()), None);
        assert_eq!(basic_auth_credentials(&headers_with_auth("Bearer x")), None);
        assert_eq!(
            basic_auth_credentials(&headers_with_auth("Basic !!!")),
            None
        );
        assert_eq!(
            basic_auth_credentials(&headers_with_auth(&format!(
                "Basic {}",
                BASE64.encode("nocolon")
            ))),
            None
        );
    }

    #[tokio::test]
    async fn ready_without_db_is_ok() {
        assert!(HealthChecker::new().is_ready().await);
    }

    #[tokio::test]
    async fn record_http_metrics_bumps_counters() {
        let before = http_requests_total()
            .with_label_values(&["GET", "/x", "200"])
            .get();

        let app = Router::new()
            .route("/x", get(|| async { "hi" }))
            .layer(axum::middleware::from_fn(record_http_metrics));

        let resp = app
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let after = http_requests_total()
            .with_label_values(&["GET", "/x", "200"])
            .get();
        assert_eq!(after, before + 1);
    }
}
