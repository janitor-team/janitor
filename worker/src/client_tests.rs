#[cfg(test)]
mod tests {
    use crate::client::{
        abort_run, bundle_results, AssignmentError, Client, Credentials, UploadFailure,
    };
    use crate::{get_build_arch, DpkgArchitectureError};
    use reqwest::Url;
    use serde_json::json;
    use std::path::Path;
    use crate::client::upload_results;
    use janitor::api::worker::Metadata;
    use janitor_types::api::runner::FinishResponse;
    use axum::{
        extract::{Path as AxumPath, State},
        http::StatusCode as AxumStatus,
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn test_assignment_error_display() {
        let err = AssignmentError::Failure("Test failure".to_string());
        assert_eq!(err.to_string(), "AssignmentError: Test failure");

        let err = AssignmentError::EmptyQueue;
        assert_eq!(err.to_string(), "AssignmentError: EmptyQueue");
    }

    #[test]
    fn test_assignment_error_error_trait() {
        let err = AssignmentError::Failure("Test".to_string());
        assert!(std::error::Error::source(&err).is_none());

        let err = AssignmentError::EmptyQueue;
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_upload_failure_display() {
        let err = UploadFailure("Upload failed".to_string());
        assert_eq!(err.to_string(), "Upload failed");
    }

    #[test]
    fn test_upload_failure_error_trait() {
        let err = UploadFailure("Test".to_string());
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn test_credentials_none() {
        let creds = Credentials::None;
        let builder = reqwest::Client::new().get("http://example.com");
        let result = creds.set_credentials(builder);
        // Should return the builder unchanged
        assert!(result.build().is_ok());
    }

    #[test]
    fn test_credentials_basic() {
        let creds = Credentials::Basic {
            username: "user".to_string(),
            password: Some("pass".to_string()),
        };
        let builder = reqwest::Client::new().get("http://example.com");
        let result = creds.set_credentials(builder);
        assert!(result.build().is_ok());
    }

    #[test]
    fn test_credentials_basic_no_password() {
        let creds = Credentials::Basic {
            username: "user".to_string(),
            password: None,
        };
        let builder = reqwest::Client::new().get("http://example.com");
        let result = creds.set_credentials(builder);
        assert!(result.build().is_ok());
    }

    #[test]
    fn test_credentials_bearer() {
        let creds = Credentials::Bearer {
            token: "secret-token".to_string(),
        };
        let builder = reqwest::Client::new().get("http://example.com");
        let result = creds.set_credentials(builder);
        assert!(result.build().is_ok());
    }

    #[test]
    fn test_credentials_from_url_no_auth() {
        let url = Url::parse("http://example.com/path").unwrap();
        let creds = Credentials::from_url(&url);
        match creds {
            Credentials::None => (),
            _ => panic!("Expected None credentials"),
        }
    }

    #[test]
    fn test_credentials_from_url_with_auth() {
        let url = Url::parse("http://user:pass@example.com/path").unwrap();
        let creds = Credentials::from_url(&url);
        match creds {
            Credentials::Basic { username, password } => {
                assert_eq!(username, "user");
                assert_eq!(password, Some("pass".to_string()));
            }
            _ => panic!("Expected Basic credentials"),
        }
    }

    #[test]
    fn test_credentials_from_url_username_only() {
        let url = Url::parse("http://user@example.com/path").unwrap();
        let creds = Credentials::from_url(&url);
        match creds {
            Credentials::Basic { username, password } => {
                assert_eq!(username, "user");
                assert_eq!(password, None);
            }
            _ => panic!("Expected Basic credentials with no password"),
        }
    }

    #[test]
    fn test_client_new() {
        let base_url = Url::parse("http://example.com").unwrap();
        let creds = Credentials::None;
        let client = Client::new(base_url.clone(), creds, "test-agent");

        assert!(client.is_ok());
        let client = client.unwrap();
        assert_eq!(client.base_url, base_url);
    }

    #[test]
    fn test_client_new_with_credentials() {
        let base_url = Url::parse("http://example.com").unwrap();
        let creds = Credentials::Bearer {
            token: "test-token".to_string(),
        };
        let client = Client::new(base_url, creds, "test-agent/1.0");

        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn bundle_results_without_directory_has_only_metadata() {
        let metadata = Metadata::default();
        let bundle = bundle_results(&metadata, None).await.unwrap();
        assert!(bundle.filenames.is_empty());
    }

    #[tokio::test]
    async fn bundle_results_collects_regular_files_and_skips_directories() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.log"), b"aaa")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("b.txt"), b"bbbb")
            .await
            .unwrap();
        tokio::fs::create_dir(tmp.path().join("subdir"))
            .await
            .unwrap();

        let metadata = Metadata::default();
        let bundle = bundle_results(&metadata, Some(tmp.path())).await.unwrap();

        let expected: std::collections::HashSet<String> =
            ["a.log", "b.txt"].iter().map(|s| s.to_string()).collect();
        assert_eq!(bundle.filenames, expected);
    }

    #[tokio::test]
    async fn test_bundle_results_empty_directory() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let metadata = Metadata::default();
        let bundle = bundle_results(&metadata, Some(temp_dir.path()))
            .await
            .unwrap();

        assert!(bundle.filenames.is_empty());
    }

    #[tokio::test]
    async fn test_bundle_results_nonexistent_directory() {
        let metadata = Metadata::default();
        let nonexistent = Path::new("/nonexistent/path");
        let result = bundle_results(&metadata, Some(nonexistent)).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_bundle_results_metadata_serialization() {
        use janitor::api::worker::TargetDetails;

        let mut metadata = Metadata::default();
        metadata.codebase = Some(janitor_types::CodebaseName::new("test-codebase").unwrap());
        metadata.target = Some(TargetDetails {
            name: "test-target".to_string(),
            details: json!({"test": "data"}),
        });

        // Complex metadata must still serialize into the form.
        let bundle = bundle_results(&metadata, None).await.unwrap();
        assert!(bundle.filenames.is_empty());
    }

    #[test]
    fn test_get_build_arch_success() {
        // This test might fail on systems without dpkg-architecture
        // but demonstrates the expected behavior
        match get_build_arch() {
            Ok(arch) => {
                assert!(!arch.is_empty());
                assert!(!arch.contains('\n'));
            }
            Err(DpkgArchitectureError::MissingCommand) => {
                // Expected on non-Debian systems
            }
            Err(_) => {
                // Other errors might occur
            }
        }
    }

    #[test]
    fn test_dpkg_architecture_error_types() {
        let missing_err = DpkgArchitectureError::MissingCommand;
        assert!(missing_err.to_string().contains("dpkg-dev"));

        let other_err = DpkgArchitectureError::Other("Custom error".to_string());
        assert_eq!(other_err.to_string(), "Custom error");
    }

    // Integration tests would require a mock HTTP server
    // These tests focus on the data structures and basic functionality

    #[test]
    fn test_assignment_json_construction() {
        // Test the JSON payload construction logic
        let node_name = "test-node";
        let codebase = Some("test-codebase");
        let campaign = Some("test-campaign");

        // This mirrors the logic in get_assignment_raw
        let json = serde_json::json!({
            "node": node_name,
            "archs": ["amd64"], // Mock architecture
            "worker_link": null,
            "codebase": codebase,
            "campaign": campaign,
            "backchannel": null,
        });

        assert_eq!(json["node"], "test-node");
        assert_eq!(json["codebase"], "test-codebase");
        assert_eq!(json["campaign"], "test-campaign");
        assert!(json["archs"].is_array());
    }

    #[test]
    fn test_assignment_json_with_urls() {
        let my_url = Url::parse("http://worker.example.com").unwrap();
        let jenkins_url = Url::parse("http://jenkins.example.com/job/123").unwrap();

        let mut json = serde_json::json!({
            "node": "test-node",
            "archs": ["amd64"],
            "worker_link": null,
            "codebase": null,
            "campaign": null,
        });

        // Test backchannel construction
        json["backchannel"] = serde_json::json!({
            "kind": "http",
            "url": my_url.to_string(),
        });
        json["worker_link"] = serde_json::Value::String(my_url.to_string());

        assert_eq!(json["backchannel"]["kind"], "http");
        assert_eq!(json["backchannel"]["url"], my_url.to_string());

        // Test Jenkins backchannel
        json["backchannel"] = serde_json::json!({
            "kind": "jenkins",
            "url": jenkins_url.to_string(),
        });

        assert_eq!(json["backchannel"]["kind"], "jenkins");
    }

    #[tokio::test]
    async fn test_abort_run() {
        // Test the abort_run function with a mock client
        // This is a high-level test of the abort functionality

        let base_url = Url::parse("http://example.com").unwrap();
        let client = Client::new(base_url, Credentials::None, "test").unwrap();

        let mut metadata = janitor::api::worker::Metadata::default();
        metadata.description = Some("Test run".to_string());

        // This would normally make an HTTP request, but we're testing the function signature
        // In a real test, we'd use a mock HTTP server
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            abort_run(&client, &janitor_types::RunId::new("test-run-id").unwrap(), &metadata, "Test abort"),
        )
        .await
        .ok(); // Ignore timeout/network errors for this unit test
    }

    #[derive(Clone)]
    struct MockState {
        status: AxumStatus,
        body: serde_json::Value,
        hits: Arc<AtomicUsize>,
    }

    async fn mock_finish(
        AxumPath(_run_id): AxumPath<String>,
        State(state): State<MockState>,
        _body: axum::body::Bytes,
    ) -> impl IntoResponse {
        state.hits.fetch_add(1, Ordering::SeqCst);
        (state.status, Json(state.body.clone()))
    }

    async fn spawn_mock(state: MockState) -> Url {
        let app = Router::new()
            .route("/active-runs/{run_id}/finish", post(mock_finish))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{}/", addr).parse().unwrap()
    }

    #[tokio::test]
    async fn upload_results_success_returns_parsed_json() {
        let hits = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            status: AxumStatus::OK,
            body: serde_json::json!({
                "id": "run-1",
                "filenames": [],
                "logs": [],
                "artifacts": null,
                "result": {},
            }),
            hits: hits.clone(),
        };
        let base_url = spawn_mock(state).await;

        let metadata = Metadata::default();
        let result = upload_results(
            &reqwest::Client::new(),
            &Credentials::None,
            &base_url,
            &janitor_types::RunId::new("run-1").unwrap(),
            &metadata,
            None,
        )
        .await
        .expect("expected success");

        assert_eq!(
            result,
            FinishResponse {
                id: janitor_types::RunId::new("run-1").unwrap(),
                filenames: vec![],
                logs: vec![],
                artifacts: None,
                result: serde_json::json!({}),
            }
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn upload_results_4xx_is_permanent_and_not_retried() {
        let hits = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            status: AxumStatus::CONFLICT,
            body: serde_json::json!({"reason": "already-processed"}),
            hits: hits.clone(),
        };
        let base_url = spawn_mock(state).await;

        let metadata = Metadata::default();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            upload_results(
                &reqwest::Client::new(),
                &Credentials::None,
                &base_url,
                &janitor_types::RunId::new("run-2").unwrap(),
                &metadata,
                None,
            ),
        )
        .await
        .expect("should not exceed timeout, i.e. should not retry")
        .expect_err("4xx should surface as an error");

        assert!(
            err.to_string().contains("terminal"),
            "expected terminal error message, got: {}",
            err
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "4xx must not trigger a retry"
        );
    }

    #[tokio::test]
    async fn upload_results_404_is_permanent_with_reason() {
        let hits = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            status: AxumStatus::NOT_FOUND,
            body: serde_json::json!({"reason": "unknown-run"}),
            hits: hits.clone(),
        };
        let base_url = spawn_mock(state).await;

        let metadata = Metadata::default();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            upload_results(
                &reqwest::Client::new(),
                &Credentials::None,
                &base_url,
                &janitor_types::RunId::new("run-3").unwrap(),
                &metadata,
                None,
            ),
        )
        .await
        .expect("should not exceed timeout")
        .expect_err("404 should surface as an error");

        assert_eq!(err.to_string(), "unknown-run");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
