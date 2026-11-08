//! Middleware and worker authentication helpers shared by the web
//! services.

use axum::{
    extract::{MatchedPath, Request},
    http::{header, HeaderMap},
    middleware::Next,
    response::Response,
    Router,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use prometheus::{HistogramVec, IntCounterVec};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tower_http::{
    compression::CompressionLayer,
    cors::{Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::shared_config::WebConfig;

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

/// Attach the standard middleware stack (metrics, body limit, timeout,
/// optional compression / CORS / request logging) to a router.
pub fn apply_standard_middleware<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    config: &WebConfig,
) -> Router<S> {
    let mut router = router
        .layer(axum::middleware::from_fn(record_http_metrics))
        .layer(RequestBodyLimitLayer::new(config.max_request_size_bytes))
        .layer(TimeoutLayer::new(Duration::from_secs(
            config.request_timeout_seconds,
        )));

    if config.enable_compression {
        router = router.layer(CompressionLayer::new());
    }

    if config.enable_cors {
        router = router.layer(
            CorsLayer::new()
                .allow_methods(Any)
                .allow_origin(Any)
                .allow_headers(Any),
        );
    }

    if config.enable_request_logging {
        router = router.layer(TraceLayer::new_for_http());
    }

    router
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
    use axum::{body::Body, routing::get};
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
