//! Logging and tracing integration for the runner.

/// Service name spans are exported under, as in the Python runner.
pub const SERVICE_NAME: &str = "janitor.runner";

/// Initialize tracing and logging system.
///
/// When `zipkin_address` is set, spans are also exported there; the
/// returned guard must then be kept alive for as long as the runner
/// runs.
pub fn init_tracing(
    log_level: &str,
    zipkin_address: Option<&str>,
) -> Result<Option<janitor::otlp::TracerGuard>, TracingError> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

    let env_filter = EnvFilter::try_new(log_level)
        .or_else(|_| EnvFilter::try_new("info"))
        .map_err(|e| TracingError::Configuration(format!("Invalid log level: {}", e)))?;
    let (otlp_layer, otlp_guard) = span_export_layer(zipkin_address)?;

    Registry::default()
        .with(env_filter)
        .with(otlp_layer)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .try_init()
        .map_err(|e| {
            TracingError::Initialization(format!("Failed to initialize console logging: {}", e))
        })?;

    if let Some(hostname) = hostname::get().ok().and_then(|h| h.into_string().ok()) {
        log::info!("Hostname: {}", hostname);
    }
    log::info!("Process ID: {}", std::process::id());
    log::info!("Log level: {}", log_level);
    log::info!(
        "Distributed tracing: {}",
        zipkin_address.unwrap_or("disabled")
    );

    Ok(otlp_guard)
}

/// A layer exporting spans to `zipkin_address`, or no layer when it
/// is unset.
#[allow(clippy::type_complexity)]
fn span_export_layer<S>(
    zipkin_address: Option<&str>,
) -> Result<
    (
        Option<impl tracing_subscriber::Layer<S>>,
        Option<janitor::otlp::TracerGuard>,
    ),
    TracingError,
>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let Some(address) = zipkin_address else {
        return Ok((None, None));
    };
    let (layer, guard) = janitor::otlp::layer(SERVICE_NAME, address)
        .map_err(|e| TracingError::Configuration(e.to_string()))?;
    Ok((Some(layer), Some(guard)))
}

/// Set up only span export to `zipkin_address`, for when log output
/// is handled elsewhere (e.g. by GCP logging).
pub fn init_span_export(zipkin_address: &str) -> Result<janitor::otlp::TracerGuard, TracingError> {
    use tracing_subscriber::{layer::SubscriberExt, Registry};

    let (layer, guard) = janitor::otlp::layer(SERVICE_NAME, zipkin_address)
        .map_err(|e| TracingError::Configuration(e.to_string()))?;
    tracing::subscriber::set_global_default(Registry::default().with(layer))
        .map_err(|e| TracingError::Initialization(e.to_string()))?;
    Ok(guard)
}

/// Tracing middleware for HTTP requests.
pub async fn http_tracing_middleware(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let user_agent = request
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let start = std::time::Instant::now();

    let span = tracing::info_span!(
        "http_request",
        method = %method,
        uri = %uri,
        user_agent = %user_agent,
    );

    let response = tracing::instrument::Instrument::instrument(next.run(request), span).await;

    let duration = start.elapsed();
    let status = response.status();

    tracing::info!(
        method = ?method,
        uri = ?uri,
        status = %status,
        duration_ms = duration.as_millis(),
        user_agent = %user_agent,
        "HTTP request completed"
    );

    // Log slow requests
    if duration.as_millis() > 1000 {
        tracing::warn!(
            method = ?method,
            uri = ?uri,
            duration_ms = duration.as_millis(),
            "Slow HTTP request detected"
        );
    }

    response
}

/// Tracing error types.
#[derive(Debug, thiserror::Error)]
pub enum TracingError {
    /// Configuration-related errors.
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Initialization-related errors.
    #[error("Initialization error: {0}")]
    Initialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_span_export_skipped_without_zipkin_address() {
        let (layer, guard) =
            span_export_layer::<tracing_subscriber::Registry>(None).expect("no export");
        assert!(layer.is_none());
        assert!(guard.is_none());
    }

    #[tokio::test]
    async fn test_span_export_with_zipkin_address() {
        let (layer, guard) = span_export_layer::<tracing_subscriber::Registry>(Some(
            "http://localhost:9411/v1/traces",
        ))
        .expect("export layer");
        assert!(layer.is_some());
        assert!(guard.is_some());
    }

    #[test]
    fn test_span_export_rejects_bad_zipkin_address() {
        assert!(matches!(
            span_export_layer::<tracing_subscriber::Registry>(Some("not a url")),
            Err(TracingError::Configuration(_))
        ));
    }
}
