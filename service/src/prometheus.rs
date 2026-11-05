use reqwest::Client;
use std::collections::HashMap;
use url::Url;

fn push_url(prometheus: &Url, job: &str, grouping_key: &HashMap<&str, &str>) -> Url {
    let mut url = prometheus.join("/metrics/job/").unwrap().join(job).unwrap();
    for (k, v) in grouping_key {
        url.path_segments_mut().unwrap().push(k).push(v);
    }
    url
}

pub async fn push_to_gateway(
    prometheus: &Url,
    job: &str,
    grouping_key: HashMap<&str, &str>,
    registry: &prometheus::Registry,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();
    let mut buffer = String::new();
    let encoder = prometheus::TextEncoder::new();
    let metric_families = registry.gather();
    encoder.encode_utf8(&metric_families, &mut buffer).unwrap();
    let url = push_url(prometheus, job, &grouping_key);
    let response = client
        .post(url)
        .header("Content-Type", "text/plain")
        .body(buffer)
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
    use prometheus::{Counter, Registry};
    use std::collections::HashMap;
    use url::Url;

    #[test]
    fn test_url_construction() {
        let base_url = Url::parse("http://localhost:9091").unwrap();
        let url = push_url(
            &base_url,
            "test_job",
            &HashMap::from([("instance", "test_instance")]),
        );
        assert_eq!(
            url.as_str(),
            "http://localhost:9091/metrics/job/test_job/instance/test_instance"
        );

        let url = push_url(
            &base_url,
            "test_job",
            &HashMap::from([("instance", "test_instance"), ("region", "us-west-1")]),
        );
        let segments: Vec<&str> = url.path_segments().unwrap().collect();
        assert_eq!(segments[..3], ["metrics", "job", "test_job"]);
        let mut labels: Vec<(&str, &str)> = segments[3..].chunks(2).map(|c| (c[0], c[1])).collect();
        labels.sort();
        assert_eq!(
            labels,
            [("instance", "test_instance"), ("region", "us-west-1")]
        );
        assert_eq!(url.query(), None);
    }

    #[test]
    fn test_metrics_encoding() {
        let registry = Registry::new();
        let counter = Counter::new("test_counter", "A test counter").unwrap();
        counter.inc();
        registry.register(Box::new(counter)).unwrap();

        let encoder = prometheus::TextEncoder::new();
        let metric_families = registry.gather();
        let mut buffer = String::new();
        encoder.encode_utf8(&metric_families, &mut buffer).unwrap();

        assert!(buffer.contains("test_counter"));
        assert!(buffer.contains("1"));
    }

    #[tokio::test]
    async fn test_push_to_gateway_invalid_url() {
        let registry = Registry::new();
        let invalid_url = Url::parse("http://nonexistent.invalid:9091").unwrap();
        let job = "test_job";
        let grouping_key = HashMap::new();

        let result = push_to_gateway(&invalid_url, job, grouping_key, &registry).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_grouping_key() {
        let base_url = Url::parse("http://localhost:9091").unwrap();
        let url = push_url(&base_url, "test_job", &HashMap::new());
        assert_eq!(url.as_str(), "http://localhost:9091/metrics/job/test_job");
    }

    #[test]
    fn test_registry_with_multiple_metrics() {
        let registry = Registry::new();

        let counter1 = Counter::new("counter_1", "First counter").unwrap();
        let counter2 = Counter::new("counter_2", "Second counter").unwrap();

        counter1.inc_by(5.0);
        counter2.inc_by(10.0);

        registry.register(Box::new(counter1)).unwrap();
        registry.register(Box::new(counter2)).unwrap();

        let encoder = prometheus::TextEncoder::new();
        let metric_families = registry.gather();
        let mut buffer = String::new();
        encoder.encode_utf8(&metric_families, &mut buffer).unwrap();

        assert!(buffer.contains("counter_1"));
        assert!(buffer.contains("counter_2"));
        assert!(buffer.contains("5"));
        assert!(buffer.contains("10"));
    }
}
