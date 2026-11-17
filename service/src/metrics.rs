//! Prometheus exposition for metrics recorded through the `metrics` crate.
//!
//! Each service calls [`install`] once at startup, before recording any
//! metrics; anything recorded before then is dropped by the no-op default
//! recorder.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

/// Histogram buckets used unless a service overrides them for a specific
/// metric.
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Content type of the Prometheus text exposition format.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Per-metric histogram bucket overrides, as passed to [`install`].
pub type BucketOverrides = &'static [(&'static str, &'static [f64])];

struct Installed {
    handle: PrometheusHandle,
    process: metrics_process::Collector,
    buckets: BucketOverrides,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// Build a Prometheus recorder configuration.
///
/// `buckets` overrides [`DEFAULT_BUCKETS`] for the named histograms.
pub fn builder(buckets: BucketOverrides) -> PrometheusBuilder {
    let mut builder = PrometheusBuilder::new()
        .set_buckets(DEFAULT_BUCKETS)
        .expect("default buckets are not empty");
    for (name, values) in buckets {
        builder = builder
            .set_buckets_for_metric(Matcher::Full(name.to_string()), values)
            .unwrap_or_else(|e| panic!("invalid buckets for {name}: {e}"));
    }
    builder
}

/// Install the global Prometheus recorder, configured by [`builder`].
///
/// Calling this again is a no-op as long as the same overrides are
/// passed; passing different ones panics, since the recorder can only be
/// configured once per process.
pub fn install(buckets: BucketOverrides) {
    let installed = INSTALLED.get_or_init(|| {
        let handle = builder(buckets)
            .install_recorder()
            .expect("failed to install metrics recorder");

        // install_recorder doesn't spawn the upkeep task that drains
        // histogram samples; without it they accumulate between scrapes.
        let upkeep = handle.clone();
        std::thread::Builder::new()
            .name("metrics-upkeep".to_string())
            .spawn(move || loop {
                std::thread::sleep(UPKEEP_INTERVAL);
                upkeep.run_upkeep();
            })
            .expect("failed to spawn metrics upkeep thread");

        let process = metrics_process::Collector::default();
        process.describe();
        Installed {
            handle,
            process,
            buckets,
        }
    });
    assert_eq!(
        installed.buckets, buckets,
        "metrics recorder already installed with different histogram buckets"
    );
}

/// Render all recorded metrics in the Prometheus text format.
///
/// Panics if [`install`] hasn't been called.
pub fn render() -> String {
    let installed = INSTALLED
        .get()
        .expect("metrics recorder not installed; call janitor_service::metrics::install");
    installed.process.collect();
    installed.handle.render()
}

/// axum handler for `GET /metrics`.
pub async fn metrics_handler() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, CONTENT_TYPE)],
        render(),
    )
        .into_response()
}

fn push_url(gateway: &Url, job: &str, grouping_key: &HashMap<&str, &str>) -> Url {
    let mut url = gateway.join("/metrics/job/").unwrap().join(job).unwrap();
    for (k, v) in grouping_key {
        url.query_pairs_mut().append_pair(k, v);
    }
    url
}

/// Push all recorded metrics to a Prometheus push gateway.
pub async fn push_to_gateway(
    gateway: &Url,
    job: &str,
    grouping_key: HashMap<&str, &str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = Client::new()
        .post(push_url(gateway, job, &grouping_key))
        .header(header::CONTENT_TYPE, CONTENT_TYPE)
        .body(render())
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("Unexpected status code: {}", response.status()).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BUCKETS: BucketOverrides = &[("test_histogram", &[1.0, 2.0])];

    #[test]
    fn test_push_url() {
        let base_url = Url::parse("http://localhost:9091").unwrap();
        let grouping_key = HashMap::from([("instance", "test_instance")]);
        assert_eq!(
            push_url(&base_url, "test_job", &grouping_key).as_str(),
            "http://localhost:9091/metrics/job/test_job?instance=test_instance"
        );
    }

    #[test]
    fn test_push_url_empty_grouping_key() {
        let base_url = Url::parse("http://localhost:9091").unwrap();
        assert_eq!(
            push_url(&base_url, "test_job", &HashMap::new()).as_str(),
            "http://localhost:9091/metrics/job/test_job"
        );
    }

    /// Render what `f` records with a recorder configured like
    /// [`install`]. Families render in hash order, so callers should
    /// record a single metric.
    fn render_local(f: impl FnOnce()) -> String {
        let recorder = builder(TEST_BUCKETS).build_recorder();
        metrics::with_local_recorder(&recorder, f);
        recorder.handle().render()
    }

    #[test]
    fn test_counter() {
        assert_eq!(
            render_local(|| {
                metrics::describe_counter!("test_counter", "A counter");
                metrics::counter!("test_counter", "label" => "a").increment(5);
            }),
            "# HELP test_counter A counter
# TYPE test_counter counter
test_counter{label=\"a\"} 5

"
        );
    }

    #[test]
    fn test_bucket_override() {
        assert_eq!(
            render_local(|| metrics::histogram!("test_histogram").record(1.5)),
            "# TYPE test_histogram histogram
test_histogram_bucket{le=\"1\"} 0
test_histogram_bucket{le=\"2\"} 1
test_histogram_bucket{le=\"+Inf\"} 1
test_histogram_sum 1.5
test_histogram_count 1

"
        );
    }

    #[test]
    fn test_default_buckets() {
        assert_eq!(
            render_local(|| metrics::histogram!("other_histogram").record(0.3)),
            "# TYPE other_histogram histogram
other_histogram_bucket{le=\"0.005\"} 0
other_histogram_bucket{le=\"0.01\"} 0
other_histogram_bucket{le=\"0.025\"} 0
other_histogram_bucket{le=\"0.05\"} 0
other_histogram_bucket{le=\"0.1\"} 0
other_histogram_bucket{le=\"0.25\"} 0
other_histogram_bucket{le=\"0.5\"} 1
other_histogram_bucket{le=\"1\"} 1
other_histogram_bucket{le=\"2.5\"} 1
other_histogram_bucket{le=\"5\"} 1
other_histogram_bucket{le=\"10\"} 1
other_histogram_bucket{le=\"+Inf\"} 1
other_histogram_sum 0.3
other_histogram_count 1

"
        );
    }

    #[test]
    fn test_render_includes_process_metrics() {
        install(TEST_BUCKETS);
        let text = render();
        assert_eq!(
            text.lines()
                .find(|l| l.starts_with("# TYPE process_resident_memory_bytes ")),
            Some("# TYPE process_resident_memory_bytes gauge")
        );
    }

    #[test]
    #[should_panic(expected = "different histogram buckets")]
    fn test_install_with_different_buckets() {
        install(TEST_BUCKETS);
        install(&[]);
    }

    #[tokio::test]
    async fn test_push_to_gateway_invalid_url() {
        install(TEST_BUCKETS);
        let invalid_url = Url::parse("http://nonexistent.invalid:9091").unwrap();
        let result = push_to_gateway(&invalid_url, "test_job", HashMap::new()).await;
        assert!(result.is_err());
    }
}
