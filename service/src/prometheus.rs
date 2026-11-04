use reqwest::Client;
use std::collections::HashMap;
use url::Url;

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
    let mut url = prometheus.join("/metrics/job/").unwrap().join(job).unwrap();
    for (k, v) in grouping_key {
        url.query_pairs_mut().append_pair(k, v);
    }
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
        let job = "test_job";
        let mut grouping_key = HashMap::new();
        grouping_key.insert("instance", "test_instance");
        grouping_key.insert("region", "us-west-1");

        let mut url = base_url.join("/metrics/job/").unwrap().join(job).unwrap();
        for (k, v) in grouping_key {
            url.query_pairs_mut().append_pair(k, v);
        }

        assert!(url.to_string().contains("test_job"));
        assert!(url.to_string().contains("instance=test_instance"));
        assert!(url.to_string().contains("region=us-west-1"));
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
        let job = "test_job";
        let grouping_key: HashMap<&str, &str> = HashMap::new();

        let mut url = base_url.join("/metrics/job/").unwrap().join(job).unwrap();
        for (k, v) in grouping_key {
            url.query_pairs_mut().append_pair(k, v);
        }

        // Should still work with empty grouping key
        assert!(url.to_string().contains("test_job"));
        assert!(!url.to_string().contains("?"));
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
