//! Common middleware for Janitor web services

use axum::middleware::Next;
use std::time::Instant;
use uuid::Uuid;

/// Request ID middleware - adds a unique request ID to each request
pub async fn request_id_middleware(
    mut req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    // Generate or extract request ID
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    
    // Add to request extensions
    req.extensions_mut().insert(request_id.clone());
    
    // Add to response headers
    let mut response = next.run(req).await;
    response.headers_mut().insert(
        "x-request-id",
        request_id.parse().unwrap(),
    );
    
    response
}

/// Logging middleware - logs request and response details
pub async fn logging_middleware(
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let start = Instant::now();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let version = req.version();
    
    // Extract request ID if available
    let request_id = req
        .extensions()
        .get::<String>()
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    
    // Extract client IP from headers or connection info
    let client_ip = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.headers().get("x-real-ip").and_then(|v| v.to_str().ok()))
        .unwrap_or("unknown");
    
    // Log request
    tracing::info!(
        request_id = %request_id,
        method = %method,
        uri = %uri,
        version = ?version,
        client_ip = %client_ip,
        "Request received"
    );
    
    let response = next.run(req).await;
    let duration = start.elapsed();
    let status = response.status();
    
    // Log response
    if status.is_server_error() {
        tracing::error!(
            request_id = %request_id,
            method = %method,
            uri = %uri,
            status = %status,
            duration = ?duration,
            "Request failed"
        );
    } else {
        tracing::info!(
            request_id = %request_id,
            method = %method,
            uri = %uri,
            status = %status,
            duration = ?duration,
            "Request completed"
        );
    }
    
    response
}

/// The `path` label for HTTP request metrics: the matched route
/// template (e.g. `/pkg/{name}`), so each route is a single series no
/// matter how many distinct URLs hit it. Requests that matched no
/// route share the label `unmatched`.
pub fn metrics_path_label(req: &axum::extract::Request) -> String {
    req.extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string())
}

/// Metrics middleware - records request metrics
pub async fn metrics_middleware(
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = metrics_path_label(&req);
    
    let response = next.run(req).await;
    let duration = start.elapsed();
    let status = response.status().as_u16().to_string();
    
    // Record metrics using prometheus
    record_http_request_duration(&method, &path, &status, duration);
    record_http_request_count(&method, &path, &status);
    
    response
}

/// Record HTTP request duration metric
fn record_http_request_duration(method: &str, path: &str, status: &str, duration: std::time::Duration) {
    use prometheus::HistogramVec;
    use lazy_static::lazy_static;
    
    lazy_static! {
        static ref HTTP_REQUEST_DURATION: HistogramVec = {
            let histogram_opts = prometheus::HistogramOpts::new("http_request_duration_seconds", "HTTP request duration in seconds");
            let histogram = HistogramVec::new(histogram_opts, &["method", "path", "status"]).unwrap();
            prometheus::register(Box::new(histogram.clone())).unwrap();
            histogram
        };
    }
    
    HTTP_REQUEST_DURATION
        .with_label_values(&[method, path, status])
        .observe(duration.as_secs_f64());
}

/// Record HTTP request count metric
fn record_http_request_count(method: &str, path: &str, status: &str) {
    use prometheus::{CounterVec, Opts};
    use lazy_static::lazy_static;
    
    lazy_static! {
        static ref HTTP_REQUEST_COUNT: CounterVec = {
            let opts = Opts::new("http_requests_total", "Total number of HTTP requests");
            let counter = CounterVec::new(opts, &["method", "path", "status"]).unwrap();
            prometheus::register(Box::new(counter.clone())).unwrap();
            counter
        };
    }
    
    HTTP_REQUEST_COUNT
        .with_label_values(&[method, path, status])
        .inc();
}

/// Security headers middleware - adds security headers to responses
pub async fn security_headers_middleware(
    req: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    
    // Add security headers
    headers.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    headers.insert("X-Frame-Options", "DENY".parse().unwrap());
    headers.insert("X-XSS-Protection", "1; mode=block".parse().unwrap());
    headers.insert(
        "Strict-Transport-Security",
        "max-age=31536000; includeSubDomains".parse().unwrap()
    );
    headers.insert(
        "Content-Security-Policy",
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'"
            .parse()
            .unwrap()
    );
    
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::StatusCode};
    use tower::util::ServiceExt;

    /// Echo the label `metrics_path_label` computes, from a middleware
    /// attached the way the services attach `metrics_middleware`.
    async fn label_header(
        req: axum::extract::Request,
        next: Next,
    ) -> axum::response::Response {
        let label = metrics_path_label(&req);
        let mut response = next.run(req).await;
        response
            .headers_mut()
            .insert("x-path-label", label.parse().unwrap());
        response
    }

    async fn label_for(app: axum::Router, uri: &str) -> String {
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        response.headers()["x-path-label"]
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn test_metrics_path_label_uses_route_template() {
        let inner = axum::Router::new()
            .route("/pkg/{package}", axum::routing::get(|| async { "ok" }));
        let app = axum::Router::new()
            .nest("/{campaign}", inner)
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn(label_header));

        assert_eq!(
            label_for(app.clone(), "/lintian-fixes/pkg/foo").await,
            "/{campaign}/pkg/{package}"
        );
        assert_eq!(
            label_for(app.clone(), "/fresh-snapshots/pkg/bar").await,
            "/{campaign}/pkg/{package}"
        );
        assert_eq!(label_for(app, "/no/such/page").await, "unmatched");
    }

    #[tokio::test]
    async fn test_request_id_middleware() {
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "Hello" }))
            .layer(axum::middleware::from_fn(request_id_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-request-id"));
    }

    /// When the caller already sends an `x-request-id`, the
    /// middleware must echo that same value back on the response so
    /// log pipelines can stitch the request across hops — it must
    /// not be overwritten with a freshly-minted UUID.
    #[tokio::test]
    async fn request_id_middleware_preserves_inbound_header() {
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "Hello" }))
            .layer(axum::middleware::from_fn(request_id_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("x-request-id", "caller-trace-42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-request-id")
                .unwrap()
                .to_str()
                .unwrap(),
            "caller-trace-42"
        );
    }

    /// Without an inbound header, the middleware should mint a
    /// UUID-v4 — its string form is always 36 chars with dashes in
    /// positions 8/13/18/23.
    #[tokio::test]
    async fn request_id_middleware_mints_uuid_when_header_absent() {
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "Hello" }))
            .layer(axum::middleware::from_fn(request_id_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let id = response
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        // Sanity: parseable as a UUID.
        assert!(
            Uuid::parse_str(id).is_ok(),
            "expected a parseable UUID, got {:?}",
            id
        );
    }

    /// Two requests without inbound headers must produce distinct
    /// IDs. If the middleware accidentally reused a single UUID
    /// (e.g. via a static initialisation), everyone would log the
    /// same request id.
    #[tokio::test]
    async fn request_id_middleware_generates_unique_ids_per_request() {
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "Hello" }))
            .layer(axum::middleware::from_fn(request_id_middleware));

        let first = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let second = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let a = first.headers().get("x-request-id").unwrap();
        let b = second.headers().get("x-request-id").unwrap();
        assert_ne!(a, b, "request ids should not collide across requests");
    }

    #[tokio::test]
    async fn test_security_headers_middleware() {
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "Hello" }))
            .layer(axum::middleware::from_fn(security_headers_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("X-Content-Type-Options").unwrap(),
            "nosniff"
        );
        assert_eq!(response.headers().get("X-Frame-Options").unwrap(), "DENY");
    }

    /// Pin the full set of security headers. If someone reorganises
    /// the middleware and drops one of these, a browser would start
    /// accepting content sniffing / XSS injection / plaintext HTTP
    /// on reload — a regression worth catching.
    #[tokio::test]
    async fn security_headers_middleware_sets_every_documented_header() {
        let app = axum::Router::new()
            .route("/", axum::routing::get(|| async { "Hello" }))
            .layer(axum::middleware::from_fn(security_headers_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let headers = response.headers();

        assert_eq!(headers.get("X-XSS-Protection").unwrap(), "1; mode=block");
        assert_eq!(
            headers.get("Strict-Transport-Security").unwrap(),
            "max-age=31536000; includeSubDomains"
        );
        assert_eq!(
            headers.get("Content-Security-Policy").unwrap(),
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'"
        );
    }

    /// Security headers must not overwrite headers the handler
    /// deliberately set (e.g. a relaxed CSP for an embed endpoint).
    /// This test guards against the middleware switching from
    /// `insert` to `append` or vice-versa by accident.
    ///
    /// Current behaviour: middleware uses `insert` → always wins.
    /// If we ever want handler overrides to take precedence, this
    /// test should flip and be renamed.
    #[tokio::test]
    async fn security_headers_overwrite_handler_set_values() {
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async {
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header("X-Frame-Options", "SAMEORIGIN")
                        .body(Body::from("hi"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(security_headers_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Middleware's DENY wins over the handler's SAMEORIGIN.
        assert_eq!(response.headers().get("X-Frame-Options").unwrap(), "DENY");
    }

}