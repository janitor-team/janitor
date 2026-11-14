use super::*;
use chrono::{DateTime, Utc};

#[test]
fn test_calculate_next_try_time_no_attempts() {
    let finish_time = Utc::now();
    let next_time = calculate_next_try_time(finish_time, 0);
    assert_eq!(next_time, finish_time);
}

#[test]
fn test_calculate_next_try_time_exponential_backoff() {
    let finish_time = DateTime::parse_from_rfc3339("2023-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // First attempt: 2 hours
    let next_time = calculate_next_try_time(finish_time, 1);
    assert_eq!(next_time, finish_time + chrono::Duration::hours(2));

    // Second attempt: 4 hours
    let next_time = calculate_next_try_time(finish_time, 2);
    assert_eq!(next_time, finish_time + chrono::Duration::hours(4));

    // Third attempt: 8 hours
    let next_time = calculate_next_try_time(finish_time, 3);
    assert_eq!(next_time, finish_time + chrono::Duration::hours(8));

    // Fourth attempt: 16 hours
    let next_time = calculate_next_try_time(finish_time, 4);
    assert_eq!(next_time, finish_time + chrono::Duration::hours(16));

    // Fifth attempt: 32 hours
    let next_time = calculate_next_try_time(finish_time, 5);
    assert_eq!(next_time, finish_time + chrono::Duration::hours(32));

    // Sixth attempt: 64 hours
    let next_time = calculate_next_try_time(finish_time, 6);
    assert_eq!(next_time, finish_time + chrono::Duration::hours(64));

    // Seventh attempt: 128 hours
    let next_time = calculate_next_try_time(finish_time, 7);
    assert_eq!(next_time, finish_time + chrono::Duration::hours(128));

    // Eighth attempt and beyond: capped at 168 hours (7 days)
    let next_time = calculate_next_try_time(finish_time, 8);
    assert_eq!(next_time, finish_time + chrono::Duration::days(7));

    let next_time = calculate_next_try_time(finish_time, 10);
    assert_eq!(next_time, finish_time + chrono::Duration::days(7));

    let next_time = calculate_next_try_time(finish_time, 100);
    assert_eq!(next_time, finish_time + chrono::Duration::days(7));
}

#[test]
fn test_debdiff_error_display() {
    // Skip testing HTTP error as creating a reqwest::Error requires complex setup

    let err = DebdiffError::MissingRun("run-123".to_string());
    assert_eq!(err.to_string(), "Missing run: run-123");

    let err = DebdiffError::Unavailable("No debdiff available".to_string());
    assert_eq!(err.to_string(), "Unavailable: No debdiff available");
}

#[test]
fn test_publish_error_code() {
    let err = PublishError::Failure {
        code: "test-code".to_string(),
        description: "Test description".to_string(),
    };
    assert_eq!(err.code(), "test-code");

    let err = PublishError::NothingToDo("Nothing to do".to_string());
    assert_eq!(err.code(), "nothing-to-do");

    let err = PublishError::BranchBusy(url::Url::parse("https://example.com").unwrap());
    assert_eq!(err.code(), "branch-busy");

    let err = PublishError::AuthenticationFailed;
    assert_eq!(err.code(), "authentication-failed");

    let err = PublishError::NetworkError("Network error".to_string());
    assert_eq!(err.code(), "network-error");

    let err = PublishError::DatabaseError(sqlx::Error::RowNotFound);
    assert_eq!(err.code(), "database-error");
}

#[test]
fn test_publish_error_description() {
    let err = PublishError::Failure {
        code: "test-code".to_string(),
        description: "Test description".to_string(),
    };
    assert_eq!(err.description(), "Test description");

    let err = PublishError::NothingToDo("Nothing to do".to_string());
    assert_eq!(err.description(), "Nothing to do");

    let err = PublishError::BranchBusy(url::Url::parse("https://example.com").unwrap());
    assert_eq!(err.description(), "Branch is busy");

    let err = PublishError::AuthenticationFailed;
    assert_eq!(err.description(), "Authentication failed");

    let err = PublishError::NetworkError("Network connection failed".to_string());
    assert_eq!(err.description(), "Network connection failed");

    let err = PublishError::DatabaseError(sqlx::Error::RowNotFound);
    assert_eq!(err.description(), "Database error");
}

#[test]
fn test_publish_error_display() {
    let err = PublishError::Failure {
        code: "test-code".to_string(),
        description: "Test description".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "PublishError::Failure: test-code: Test description"
    );

    let err = PublishError::NothingToDo("Nothing to do".to_string());
    assert_eq!(
        err.to_string(),
        "PublishError::PublishNothingToDo: Nothing to do"
    );

    let err = PublishError::BranchBusy(url::Url::parse("https://example.com").unwrap());
    assert_eq!(
        err.to_string(),
        "PublishError::BranchBusy: Branch is busy: https://example.com/"
    );
}

#[test]
fn test_worker_invalid_response_display() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "File not found");
    let err = WorkerInvalidResponse::Io(io_err);
    assert!(err.to_string().contains("IO error"));

    let serde_err = serde_json::from_str::<String>("invalid json").unwrap_err();
    let err = WorkerInvalidResponse::Serde(serde_err);
    assert!(err.to_string().contains("Serde error"));

    let err = WorkerInvalidResponse::WorkerError("Worker failed".to_string());
    assert_eq!(err.to_string(), "Worker error: Worker failed");
}

// Skip tests for functions that require complex protobuf-generated structs (Campaign)
// or external dependencies (breezyshim). These would be better as integration tests.

#[test]
fn test_role_branch_url_without_branch_name() {
    let base_url = url::Url::parse("https://example.com/repo").unwrap();
    let result = role_branch_url(&base_url, None);
    assert_eq!(result, base_url);
}

#[test]
fn test_role_branch_url_with_branch_name() {
    let base_url = url::Url::parse("https://example.com/repo").unwrap();
    let result = role_branch_url(&base_url, Some("feature-branch"));

    // Should add branch parameter
    assert!(result.to_string().contains("branch="));
    assert!(result.to_string().contains("feature-branch"));
}

#[test]
fn test_role_branch_url_with_special_characters() {
    let base_url = url::Url::parse("https://example.com/repo").unwrap();
    let result = role_branch_url(&base_url, Some("feature/branch-name"));

    // Should properly escape special characters
    let result_str = result.to_string();
    assert!(result_str.contains("branch="));
    // The exact encoding may vary, but it should be encoded
    assert!(result_str.contains("feature") && result_str.contains("branch-name"));
}

/// Serve /old (redirects to /new), /new and /other; record each User-Agent.
async fn serve_redirect() -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::{http::HeaderMap, response::Redirect, routing::get, Router};
    let agents = Arc::new(Mutex::new(Vec::new()));
    let seen = agents.clone();
    let app = Router::new()
        .route("/old", get(|| async { Redirect::permanent("/new") }))
        .route("/new", get(|| async { "" }))
        .route("/other", get(|| async { "" }))
        .layer(axum::middleware::from_fn(
            move |headers: HeaderMap, req: axum::extract::Request, next: axum::middleware::Next| {
                let seen = seen.clone();
                async move {
                    let agent = headers.get("user-agent").map(|v| v.to_str().unwrap());
                    seen.lock().unwrap().push(agent.unwrap_or("").to_string());
                    next.run(req).await
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, agents)
}

#[tokio::test]
async fn test_resolve_redirects_follows_http_redirect() {
    let (base, agents) = serve_redirect().await;
    let old = url::Url::parse(&format!("{}/old", base)).unwrap();
    let client = http_client().unwrap();
    assert_eq!(
        resolve_redirects(&client, &old).await.as_str(),
        format!("{}/new", base)
    );
    let agent = janitor::utils::service_user_agent("publish");
    assert_eq!(*agents.lock().unwrap(), vec![agent.clone(), agent]);
}

#[tokio::test]
async fn test_branches_match_different_http_branches() {
    let (base, _agents) = serve_redirect().await;
    let old = url::Url::parse(&format!("{}/old", base)).unwrap();
    let other = url::Url::parse(&format!("{}/other", base)).unwrap();
    let client = http_client().unwrap();
    assert!(!branches_match(Some(&old), Some(&other), None, &client).await);
}

// Skip get_merged_by_user_url tests as they require breezyshim forge operations

#[test]
fn test_check_mp_error_display() {
    let err =
        CheckMpError::NoRunForMergeProposal(url::Url::parse("https://example.com/mp/1").unwrap());
    assert_eq!(
        err.to_string(),
        "No run for merge proposal: https://example.com/mp/1"
    );

    let err = CheckMpError::BranchRateLimited { retry_after: None };
    assert_eq!(err.to_string(), "Branch is rate limited");

    let err = CheckMpError::UnexpectedHttpStatus;
    assert_eq!(err.to_string(), "Unexpected HTTP status");

    let err = CheckMpError::ForgeLoginRequired;
    assert_eq!(err.to_string(), "Forge login required");
}

#[test]
fn test_publish_one_result_fields() {
    let result = PublishOneResult {
        proposal_url: Some(url::Url::parse("https://example.com/mp/1").unwrap()),
        proposal_web_url: Some(url::Url::parse("https://example.com/web/mp/1").unwrap()),
        is_new: Some(true),
        branch_name: "feature-branch".to_string(),
        target_branch_url: url::Url::parse("https://example.com/repo").unwrap(),
        target_branch_web_url: Some(url::Url::parse("https://example.com/web/repo").unwrap()),
        mode: Mode::Propose,
    };

    // Test serialization works
    let serialized = serde_json::to_string(&result).unwrap();
    assert!(serialized.contains("proposal_url"));
    assert!(serialized.contains("feature-branch"));

    // Test deserialization works
    let deserialized: PublishOneResult = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.branch_name, "feature-branch");
    assert_eq!(deserialized.is_new, Some(true));
}

#[test]
fn test_publish_one_error_fields() {
    let error = PublishOneError {
        code: "test-error".to_string(),
        description: "Test error description".to_string(),
    };

    // Test serialization
    let serialized = serde_json::to_string(&error).unwrap();
    assert!(serialized.contains("test-error"));
    assert!(serialized.contains("Test error description"));

    // Test deserialization
    let deserialized: PublishOneError = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.code, "test-error");
    assert_eq!(deserialized.description, "Test error description");
}

#[test]
fn classify_forge_rate_limit_no_entry_is_not_limited() {
    let now = Utc::now();
    assert_eq!(
        classify_forge_rate_limit(None, now),
        ForgeRateLimitDecision::NotLimited,
    );
}

#[test]
fn classify_forge_rate_limit_future_deadline_is_backoff() {
    let now = Utc::now();
    let deadline = now + chrono::Duration::minutes(5);
    assert_eq!(
        classify_forge_rate_limit(Some(deadline), now),
        ForgeRateLimitDecision::Backoff,
    );
}

#[test]
fn classify_forge_rate_limit_past_deadline_is_expired() {
    let now = Utc::now();
    let deadline = now - chrono::Duration::minutes(5);
    assert_eq!(
        classify_forge_rate_limit(Some(deadline), now),
        ForgeRateLimitDecision::Expired,
    );
}
