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
