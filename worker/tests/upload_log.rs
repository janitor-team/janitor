//! The password in the base URL must not reach the log when uploading results.

use axum::{http::HeaderMap, routing::post, Json, Router};
use janitor::api::worker::Metadata;
use janitor_worker::client::{upload_results, Credentials};
use std::sync::Mutex;

/// Logger that keeps every message so the test can read them back.
struct Capture(Mutex<Vec<String>>);

impl log::Log for Capture {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.0.lock().unwrap().push(record.args().to_string());
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

/// Uploading to a base URL with a password logs the finish URL without it.
#[tokio::test]
async fn upload_results_does_not_log_the_password() {
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    let app = Router::new().route(
        "/active-runs/{run_id}/finish",
        post(|headers: HeaderMap, _body: axum::body::Bytes| async move {
            Json(serde_json::json!({
                "filenames": [],
                "authorization": headers.contains_key("authorization"),
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let base_url: url::Url = format!("http://worker:hunter2@{}/", addr).parse().unwrap();
    let result = upload_results(
        &reqwest::Client::new(),
        &Credentials::from_url(&base_url),
        &base_url,
        "run-id",
        &Metadata::default(),
        None,
    )
    .await
    .unwrap();
    // The request itself still carries the credentials.
    assert_eq!(result["authorization"], true);

    let messages = CAPTURE.0.lock().unwrap().clone();
    let expected = format!(
        "Uploading results to http://{}/active-runs/run-id/finish",
        addr
    );
    assert!(messages.contains(&expected), "{:#?}", messages);
    let leaked: Vec<&String> = messages.iter().filter(|m| m.contains("hunter2")).collect();
    assert!(leaked.is_empty(), "password in the log: {:#?}", leaked);
}
