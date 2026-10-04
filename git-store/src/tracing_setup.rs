//! Tracing initialisation: console/JSON log output + optional OTLP
//! span export when the janitor config supplies a collector address.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::{trace::SdkTracerProvider, Resource};
use thiserror::Error;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Errors from [`init`].
#[derive(Debug, Error)]
pub enum TracingInitError {
    #[error("install tracing subscriber: {0}")]
    Subscriber(#[from] tracing_subscriber::util::TryInitError),

    #[error("build OTLP exporter for {endpoint}: {source}")]
    ExporterBuild {
        endpoint: String,
        #[source]
        source: opentelemetry_otlp::ExporterBuildError,
    },
}

/// Guard returned by [`init`]. Keeps the OTel tracer provider alive
/// and flushes buffered spans on drop so a short-lived process (tests,
/// one-shot CLI invocations) doesn't lose its last batch.
pub struct TracingGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            if let Err(e) = p.shutdown() {
                eprintln!("tracing: OTLP provider shutdown failed: {}", e);
            }
        }
    }
}

/// Initialise the tracing subscriber. When `collector_endpoint` is
/// Some, also export spans via OTLP (HTTP/protobuf) to that collector
/// under the given `service_name`.
///
/// `debug` enables `debug` level across the service; `json_format`
/// switches console output to JSON for Google Cloud Logging.
///
/// The endpoint matches the historical `config.zipkin_address` field;
/// modern zipkin servers accept OTLP natively, so a URL like
/// `http://zipkin.local:9411/v1/traces` or an OTLP-collector URL both
/// work.
pub fn init(
    service_name: &str,
    debug: bool,
    json_format: bool,
    collector_endpoint: Option<&str>,
) -> Result<TracingGuard, TracingInitError> {
    let level = if debug { "debug" } else { "info" };
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},hyper=warn,h2=warn")));

    let fmt_layer: Box<dyn Layer<_> + Send + Sync> = if json_format {
        Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(true)
                .with_current_span(true)
                .with_span_list(true),
        )
    } else {
        Box::new(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_thread_ids(false),
        )
    };

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer);

    // Export spans when a collector is configured. Python's
    // aiozipkin.create defaults to `sample_rate=0.1`; we match that
    // so dashboards don't shift when switching implementations.
    let guard = if let Some(endpoint) = collector_endpoint {
        let provider = build_otlp_provider(service_name, endpoint)?;
        let tracer = provider.tracer(service_name.to_string());
        opentelemetry::global::set_tracer_provider(provider.clone());
        registry
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init()?;
        TracingGuard {
            provider: Some(provider),
        }
    } else {
        registry.try_init()?;
        TracingGuard { provider: None }
    };

    tracing::info!(
        service = service_name,
        otlp_endpoint = collector_endpoint.unwrap_or("<disabled>"),
        "tracing initialised"
    );
    Ok(guard)
}

fn build_otlp_provider(
    service_name: &str,
    collector_url: &str,
) -> Result<SdkTracerProvider, TracingInitError> {
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
    use opentelemetry_sdk::trace::Sampler;

    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(collector_url)
        .build()
        .map_err(|source| TracingInitError::ExporterBuild {
            endpoint: collector_url.to_string(),
            source,
        })?;

    Ok(SdkTracerProvider::builder()
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            0.1,
        ))))
        .with_resource(
            Resource::builder_empty()
                .with_service_name(service_name.to_string())
                .build(),
        )
        .with_batch_exporter(exporter)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build_otlp_provider` must accept the common HTTP endpoint
    /// forms (both the OTLP collector path and a bare host:port).
    #[test]
    fn build_otlp_provider_accepts_http_endpoint() {
        let provider = build_otlp_provider("test-service", "http://localhost:4318/v1/traces")
            .expect("build provider for a well-formed endpoint URL");
        // Shutdown should also succeed on a never-used provider.
        provider.shutdown().expect("shutdown provider");
    }

    /// A malformed endpoint URL should fail cleanly via
    /// `TracingInitError::ExporterBuild`.
    #[test]
    fn build_otlp_provider_rejects_bad_endpoint() {
        let err = build_otlp_provider("svc", "not a url").unwrap_err();
        assert!(
            matches!(err, TracingInitError::ExporterBuild { .. }),
            "expected ExporterBuild error, got {:?}",
            err
        );
    }
}
