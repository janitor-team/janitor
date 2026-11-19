//! Shared web server setup and middleware for Janitor services
//!
//! This module provides common functionality for setting up web servers across
//! all Janitor services, including standard middleware, health checks, and routing.

use axum::{
    Router,
    routing::get,
    http::StatusCode,
    response::IntoResponse,
};
use tower_http::{
    cors::{CorsLayer, Any},
    trace::TraceLayer,
    timeout::TimeoutLayer,
    limit::RequestBodyLimitLayer,
    compression::CompressionLayer,
};
use std::time::Duration;
use std::sync::Arc;

pub mod health;
pub mod middleware;
pub mod error;

pub use health::{
    HealthCheck, HealthStatus, HealthCheckHandler, ComponentChecker, BasicHealthChecker,
    DatabaseHealthChecker, RedisHealthChecker,
    UptimeHealthChecker,
};
pub use middleware::{
    logging_middleware, metrics_middleware, metrics_path_label, request_id_middleware,
    security_headers_middleware,
};
pub use error::{WebError, WebResult};

// The consolidation utilities are defined directly in this module

use crate::shared_config::WebConfig;
use std::collections::HashMap;
use std::any::Any as StdAny;

/// Advanced CORS configuration for API services
#[derive(Debug, Clone)]
pub struct CorsConfig {
    /// Allowed origins (None = any origin)
    pub allowed_origins: Option<Vec<String>>,
    /// Allowed methods
    pub allowed_methods: Vec<axum::http::Method>,
    /// Allowed headers
    pub allowed_headers: Vec<String>,
    /// Whether to allow credentials
    pub allow_credentials: bool,
    /// Max age for preflight requests
    pub max_age_seconds: Option<u64>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: None, // Allow any origin
            allowed_methods: vec![
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::DELETE,
                axum::http::Method::OPTIONS,
            ],
            allowed_headers: vec![
                "content-type".to_string(),
                "authorization".to_string(),
                "accept".to_string(),
                "x-requested-with".to_string(),
                "x-api-key".to_string(),
            ],
            allow_credentials: false,
            max_age_seconds: Some(86400), // 24 hours
        }
    }
}

/// Base application state that all services should include
pub trait BaseAppState: Clone + Send + Sync + 'static {
    /// Get the service name
    fn service_name(&self) -> &str;
    
    /// Get the service version
    fn service_version(&self) -> &str;
    
    /// Get health check handler
    fn health_checker(&self) -> Arc<dyn HealthCheckHandler>;
}

// Blanket implementation for Arc<T> where T: BaseAppState
impl<T: BaseAppState> BaseAppState for Arc<T> {
    fn service_name(&self) -> &str {
        (**self).service_name()
    }
    
    fn service_version(&self) -> &str {
        (**self).service_version()
    }
    
    fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
        (**self).health_checker()
    }
}

/// Create a base router with standard middleware and routes
pub fn create_base_router<S: BaseAppState>(
    _state: S,
    config: &WebConfig,
) -> Router<S> {
    let mut router = Router::new()
        // Standard health check endpoints
        .route("/health", get(health::health_check_handler::<S>))
        .route("/health/live", get(health::liveness_handler::<S>))
        .route("/health/ready", get(health::readiness_handler::<S>))
        // Metrics endpoint
        .route("/metrics", get(metrics_handler));
    
    // Apply standard middleware
    router = apply_standard_middleware(router, config);
    
    router
}

/// Apply standard middleware stack to a router
pub fn apply_standard_middleware<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    config: &WebConfig,
) -> Router<S> {
    apply_advanced_middleware(router, config, &CorsConfig::default(), None)
}

/// Apply advanced middleware stack with custom CORS configuration
pub fn apply_advanced_middleware<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    config: &WebConfig,
    cors_config: &CorsConfig,
    request_id_header: Option<&str>,
) -> Router<S> {
    let mut router = router;

    // HTTP metrics (request count + latency per method/path/status)
    router = router.layer(axum::middleware::from_fn(middleware::metrics_middleware));

    // Request body limit
    router = router.layer(RequestBodyLimitLayer::new(config.max_request_size_bytes));
    
    // Timeout
    router = router.layer(TimeoutLayer::new(Duration::from_secs(
        config.request_timeout_seconds
    )));
    
    // Compression
    if config.enable_compression {
        router = router.layer(CompressionLayer::new());
    }
    
    // Advanced CORS configuration
    if config.enable_cors {
        let mut cors = CorsLayer::new()
            .allow_methods(cors_config.allowed_methods.clone());
        
        // Configure origins
        cors = if let Some(ref origins) = cors_config.allowed_origins {
            let origins: Vec<_> = origins.iter()
                .filter_map(|origin| origin.parse().ok())
                .collect();
            cors.allow_origin(origins)
        } else {
            cors.allow_origin(Any)
        };
        
        // Configure headers
        if cors_config.allowed_headers.is_empty() {
            cors = cors.allow_headers(Any);
        } else {
            let headers: Vec<_> = cors_config.allowed_headers.iter()
                .filter_map(|h| h.parse().ok())
                .collect();
            cors = cors.allow_headers(headers);
        }
        
        // Configure credentials
        if cors_config.allow_credentials {
            cors = cors.allow_credentials(true);
        }
        
        // Configure max age
        if let Some(max_age) = cors_config.max_age_seconds {
            cors = cors.max_age(Duration::from_secs(max_age));
        }
        
        router = router.layer(cors);
    }
    
    // Request tracing/logging with optional custom request ID header
    if config.enable_request_logging {
        let header_name = request_id_header.unwrap_or("x-request-id").to_string();
        router = router.layer(
            TraceLayer::new_for_http()
                .make_span_with(move |request: &axum::http::Request<_>| {
                    let request_id = request
                        .headers()
                        .get(&header_name)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("unknown");
                    
                    tracing::info_span!(
                        "http_request",
                        request_id = request_id,
                        method = %request.method(),
                        uri = %request.uri(),
                        version = ?request.version(),
                    )
                })
                .on_response(|response: &axum::http::Response<_>, latency: Duration, _span: &tracing::Span| {
                    tracing::info!(
                        status = response.status().as_u16(),
                        latency = ?latency,
                        "response"
                    );
                })
        );
    }
    
    router
}

/// Standard metrics handler
pub async fn metrics_handler() -> impl IntoResponse {
    // Use prometheus crate to gather metrics
    use prometheus::{TextEncoder, Encoder};
    
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => {
            let output = String::from_utf8(buffer).unwrap_or_default();
            (StatusCode::OK, output)
        }
        Err(e) => {
            tracing::error!("Failed to encode metrics: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to gather metrics".to_string())
        }
    }
}

/// Configuration for the web server
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Socket address to bind to
    pub bind_addr: std::net::SocketAddr,
    /// Graceful shutdown timeout
    pub shutdown_timeout: Duration,
    /// Number of worker threads (None = use default)
    pub workers: Option<usize>,
}

impl ServerConfig {
    /// Create from WebConfig
    pub fn from_web_config(config: &WebConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let bind_addr = format!("{}:{}", config.listen_address, config.port).parse()?;
        
        Ok(Self {
            bind_addr,
            shutdown_timeout: Duration::from_secs(30),
            workers: config.workers,
        })
    }
}

/// Run a web server with graceful shutdown
pub async fn run_server(
    app: Router,
    config: ServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    
    tracing::info!("Server listening on {}", config.bind_addr);
    
    // Create the server - app must be Router (not Router<S>)
    let server = axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal());
    
    // Run the server
    server.await?;
    
    tracing::info!("Server shutdown complete");
    Ok(())
}

/// Shutdown signal handler
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, starting graceful shutdown");
        },
        _ = terminate => {
            tracing::info!("Received terminate signal, starting graceful shutdown");
        },
    }
}

/// Create a middleware stack for API routes with enhanced features
pub fn create_api_middleware_stack<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .layer(axum::middleware::from_fn(middleware::request_id_middleware))
        .layer(axum::middleware::from_fn(middleware::security_headers_middleware))
        .layer(axum::middleware::from_fn(middleware::logging_middleware))
        .layer(axum::middleware::from_fn(middleware::metrics_middleware))
}

/// Apply API-specific middleware including request ID, security headers, and detailed logging
pub fn apply_api_middleware<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    config: &WebConfig,
    cors_config: &CorsConfig,
) -> Router<S> {
    let mut router = apply_advanced_middleware(router, config, cors_config, Some("x-request-id"));
    
    // Add API-specific middleware
    router = router
        .layer(axum::middleware::from_fn(middleware::request_id_middleware))
        .layer(axum::middleware::from_fn(middleware::security_headers_middleware));
    
    router
}

/// Generic application state that can be used by any service
#[derive(Clone)]
pub struct StandardAppState<C: Clone + Send + Sync + 'static> {
    /// Service configuration
    pub config: Arc<C>,
    /// Service name
    pub service_name: &'static str,
    /// Service version  
    pub service_version: &'static str,
    /// Health checker
    pub health_checker: Arc<dyn HealthCheckHandler>,
    /// Custom fields specific to the service
    pub custom_fields: HashMap<String, Arc<dyn StdAny + Send + Sync>>,
}

impl<C: Clone + Send + Sync + 'static> StandardAppState<C> {
    /// Create a new StandardAppState
    pub fn new(
        service_name: &'static str,
        config: Arc<C>,
        health_checker: Arc<dyn HealthCheckHandler>,
    ) -> Self {
        Self {
            config,
            service_name,
            service_version: env!("CARGO_PKG_VERSION"),
            health_checker,
            custom_fields: HashMap::new(),
        }
    }

    /// Add a custom field to the state
    pub fn with_custom_field<T: StdAny + Send + Sync>(
        mut self,
        key: String,
        value: Arc<T>,
    ) -> Self {
        self.custom_fields.insert(key, value);
        self
    }

    /// Get a custom field from the state
    pub fn get_custom_field<T: StdAny + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        self.custom_fields
            .get(key)
            .and_then(|v| v.clone().downcast().ok())
    }
}

impl<C: Clone + Send + Sync + 'static> BaseAppState for StandardAppState<C> {
    fn service_name(&self) -> &str {
        self.service_name
    }

    fn service_version(&self) -> &str {
        self.service_version
    }

    fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
        self.health_checker.clone()
    }
}

/// Builder for creating StandardAppState instances
pub struct AppStateBuilder<C: Clone + Send + Sync + 'static> {
    service_name: &'static str,
    config: Arc<C>,
    health_checkers: Vec<Arc<dyn ComponentChecker>>,
    custom_fields: HashMap<String, Arc<dyn StdAny + Send + Sync>>,
}

impl<C: Clone + Send + Sync + 'static> AppStateBuilder<C> {
    /// Create a new AppStateBuilder
    pub fn new(service_name: &'static str, config: Arc<C>) -> Self {
        Self {
            service_name,
            config,
            health_checkers: Vec::new(),
            custom_fields: HashMap::new(),
        }
    }

    /// Add a health checker component
    pub fn with_health_checker(mut self, checker: Arc<dyn ComponentChecker>) -> Self {
        self.health_checkers.push(checker);
        self
    }

    /// Add a custom field
    pub fn with_custom_field<T: StdAny + Send + Sync>(
        mut self,
        key: String,
        value: Arc<T>,
    ) -> Self {
        self.custom_fields.insert(key, value);
        self
    }

    /// Build the final StandardAppState
    pub fn build(self) -> StandardAppState<C> {
        let mut health_checker = BasicHealthChecker::with_info(
            self.service_name.to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        
        // Add all component checkers
        for checker in self.health_checkers {
            health_checker = health_checker.add_component_check(checker);
        }
        
        StandardAppState {
            config: self.config,
            service_name: self.service_name,
            service_version: env!("CARGO_PKG_VERSION"),
            health_checker: Arc::new(health_checker),
            custom_fields: self.custom_fields,
        }
    }
}

/// Create a complete service router with standard setup
/// The additional_routes should already have state applied via .with_state()
pub fn create_complete_service_router<S: BaseAppState>(
    _service_name: &str,
    _service_version: &str,
    state: Arc<S>,
    additional_routes: Router,
) -> Router {
    let default_config = WebConfig::default();
    
    // For Axum 0.8, the router_factory should return a Router (not Router<S>)
    // The router should already have .with_state() called on it
    additional_routes
}

/// Overloaded version for optional web config
pub fn create_complete_service_router_with_config<S: BaseAppState>(
    state: S,
    web_config: Option<&WebConfig>,
    additional_routes: Option<Router<S>>,
) -> Router<S> {
    let default_config = WebConfig::default();
    let config = web_config.unwrap_or(&default_config);
    
    // Start with base router (health, metrics)
    let mut router = create_base_router(state.clone(), config);
    
    // Add service-specific routes if provided
    if let Some(additional_routes) = additional_routes {
        router = router.merge(additional_routes);
    }
    
    router.with_state(state)
}

/// Run a web service with standard startup  
/// Note: Due to axum's generic constraints, this needs to be called with a concrete Router type
pub async fn run_standard_web_service(
    app: axum::Router,
    bind_address: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    tracing::info!("Starting web server on {}", bind_address);
    
    // Serve the application
    axum::serve(listener, app).await?;
    Ok(())
}


/// Helper to create a bind address from WebConfig
pub fn create_bind_address(web_config: &WebConfig) -> String {
    format!("{}:{}", web_config.listen_address, web_config.port)
}

/// Convenience macro for creating service AppState
#[macro_export]
macro_rules! create_app_state {
    ($service_name:expr, $config:expr, $($checker:expr),*) => {{
        let mut builder = $crate::shared_web::AppStateBuilder::new($service_name, $config);
        $(
            builder = builder.with_health_checker($checker);
        )*
        builder.build()
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[derive(Clone)]
    struct TestAppState {
        name: String,
        version: String,
        health_checker: Arc<health::BasicHealthChecker>,
    }
    
    impl BaseAppState for TestAppState {
        fn service_name(&self) -> &str {
            &self.name
        }
        
        fn service_version(&self) -> &str {
            &self.version
        }
        
        fn health_checker(&self) -> Arc<dyn HealthCheckHandler> {
            self.health_checker.clone()
        }
    }
    
    #[tokio::test]
    async fn test_create_base_router() {
        let state = TestAppState {
            name: "test-service".to_string(),
            version: "1.0.0".to_string(),
            health_checker: Arc::new(health::BasicHealthChecker::new()),
        };

        let config = WebConfig::default();
        let router = create_base_router(state.clone(), &config);

        // Test that we can create a test client
        use tower::util::ServiceExt;
        let app = router.with_state(state);

        // Test health endpoint
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    fn test_state() -> TestAppState {
        TestAppState {
            name: "test-service".to_string(),
            version: "1.0.0".to_string(),
            health_checker: Arc::new(health::BasicHealthChecker::new()),
        }
    }

    /// The CORS default set encodes security-relevant choices
    /// (no credentials by default, x-api-key in allowed headers so
    /// API clients work out of the box, 24h max-age). Pin them.
    #[test]
    fn cors_config_defaults_are_sane() {
        let cfg = CorsConfig::default();
        assert!(
            cfg.allowed_origins.is_none(),
            "default allows any origin via None"
        );
        assert!(
            !cfg.allow_credentials,
            "credentials must default to false (otherwise an Any-origin \
             CORS policy would leak cookies)"
        );
        assert_eq!(cfg.max_age_seconds, Some(86400));

        // Method list: at minimum GET, POST, PUT, DELETE, OPTIONS
        for method in &[
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ] {
            assert!(
                cfg.allowed_methods.contains(method),
                "CORS default must allow {}",
                method
            );
        }

        // Header list includes both standard auth headers.
        for header in &["content-type", "authorization", "x-api-key"] {
            assert!(
                cfg.allowed_headers.iter().any(|h| h == header),
                "CORS default must allow {} header",
                header
            );
        }
    }

    /// `create_bind_address` is pure formatting — pin it so a
    /// refactor doesn't break a stable ops contract.
    #[test]
    fn create_bind_address_formats_host_port() {
        let mut config = WebConfig::default();
        config.listen_address = "0.0.0.0".into();
        config.port = 9820;
        assert_eq!(create_bind_address(&config), "0.0.0.0:9820");

        config.listen_address = "127.0.0.1".into();
        config.port = 0;
        assert_eq!(create_bind_address(&config), "127.0.0.1:0");
    }

    /// IPv6 bind addresses should round-trip as-is — if this ever
    /// needs bracketing for `host:port`, add it here deliberately.
    /// Current behavior: passes through the literal, which callers
    /// can opt into by supplying a bracketed string themselves.
    #[test]
    fn create_bind_address_passes_ipv6_through() {
        let mut config = WebConfig::default();
        config.listen_address = "[::1]".into();
        config.port = 9820;
        assert_eq!(create_bind_address(&config), "[::1]:9820");
    }

    #[test]
    fn server_config_from_web_config_parses_address() {
        let mut config = WebConfig::default();
        config.listen_address = "127.0.0.1".into();
        config.port = 8080;
        config.workers = Some(4);

        let sc = ServerConfig::from_web_config(&config).unwrap();
        assert_eq!(sc.bind_addr.to_string(), "127.0.0.1:8080");
        assert_eq!(sc.shutdown_timeout, Duration::from_secs(30));
        assert_eq!(sc.workers, Some(4));
    }

    #[test]
    fn server_config_rejects_unparseable_address() {
        let mut config = WebConfig::default();
        config.listen_address = "not-an-address".into();
        assert!(ServerConfig::from_web_config(&config).is_err());
    }

    /// `/metrics` must return 200 with a Prometheus text body, even
    /// when no metrics have been registered (empty body is valid).
    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_text() {
        use tower::util::ServiceExt;

        let state = test_state();
        let config = WebConfig::default();
        let router = create_base_router(state.clone(), &config).with_state(state);

        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        // Body should be valid UTF-8 (Prometheus text format), even
        // if empty.
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(std::str::from_utf8(&body).is_ok());
    }

    /// Both liveness + readiness endpoints are registered and 200
    /// when the checker is healthy (BasicHealthChecker is healthy
    /// by default).
    #[tokio::test]
    async fn liveness_and_readiness_endpoints_are_registered() {
        use tower::util::ServiceExt;

        let state = test_state();
        let config = WebConfig::default();
        let router = create_base_router(state.clone(), &config).with_state(state);

        for path in &["/health/live", "/health/ready"] {
            let response = router
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(*path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{} must return 200",
                path
            );
        }
    }

    /// Unknown paths on the base router must 404, not silently fall
    /// through to a handler that matches too broadly.
    #[tokio::test]
    async fn unknown_path_returns_404() {
        use tower::util::ServiceExt;

        let state = test_state();
        let config = WebConfig::default();
        let router = create_base_router(state.clone(), &config).with_state(state);

        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/not-a-real-endpoint")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// `Arc<T>` blanket BaseAppState impl: accessors must delegate
    /// to `&**self`, preserving identity. If we ever switched to
    /// `Arc::as_ref()` or similar this test catches an identity drift.
    #[test]
    fn arc_wrapped_state_delegates_to_inner() {
        let inner = test_state();
        let arc = Arc::new(inner.clone());
        assert_eq!(arc.service_name(), inner.service_name());
        assert_eq!(arc.service_version(), inner.service_version());
    }

    /// StandardAppState is a generic container keyed by arbitrary
    /// config type. Pin the type-safe custom-field downcast.
    #[test]
    fn standard_app_state_custom_field_roundtrips_typed_value() {
        #[derive(Debug, PartialEq)]
        struct Knob(u32);

        let state = StandardAppState::new(
            "svc",
            Arc::new(()),
            Arc::new(health::BasicHealthChecker::new()),
        )
        .with_custom_field("knob".into(), Arc::new(Knob(42)));

        let got: Arc<Knob> = state.get_custom_field("knob").expect("knob should be set");
        assert_eq!(*got, Knob(42));
    }

    /// A wrong-type downcast must return None rather than panic.
    /// Otherwise a typo in a service would crash at runtime instead
    /// of being a recoverable None.
    #[test]
    fn standard_app_state_custom_field_wrong_type_returns_none() {
        let state = StandardAppState::new(
            "svc",
            Arc::new(()),
            Arc::new(health::BasicHealthChecker::new()),
        )
        .with_custom_field("answer".into(), Arc::new(42u32));

        // Ask for a String where a u32 is stored → None, no panic.
        assert!(state.get_custom_field::<String>("answer").is_none());
    }

    /// Missing key returns None.
    #[test]
    fn standard_app_state_custom_field_missing_returns_none() {
        let state = StandardAppState::new(
            "svc",
            Arc::new(()),
            Arc::new(health::BasicHealthChecker::new()),
        );
        assert!(state.get_custom_field::<u32>("nothing").is_none());
    }

    /// StandardAppState sets its service_version to CARGO_PKG_VERSION
    /// automatically. If someone splits the crate and the version
    /// baseline disappears, this catches it.
    #[test]
    fn standard_app_state_uses_cargo_pkg_version() {
        let state = StandardAppState::new(
            "svc",
            Arc::new(()),
            Arc::new(health::BasicHealthChecker::new()),
        );
        assert_eq!(state.service_version, env!("CARGO_PKG_VERSION"));
    }

    /// AppStateBuilder must preserve custom fields through `build()`.
    #[test]
    fn app_state_builder_preserves_custom_fields() {
        let state = AppStateBuilder::new("svc", Arc::new(()))
            .with_custom_field("foo".into(), Arc::new("bar".to_string()))
            .build();
        let got: Arc<String> = state.get_custom_field("foo").expect("custom field missing");
        assert_eq!(&*got, "bar");
    }
}