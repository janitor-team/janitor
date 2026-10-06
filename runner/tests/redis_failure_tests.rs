//! Redis failures in the runner's HTTP handlers must surface as errors,
//! like they did in the Python runner, rather than being logged and
//! ignored.

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use janitor_runner::database::RunnerDatabase;
use janitor_runner::{test_utils, AppState};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn setup() -> Option<(axum::Router, Arc<AppState>)> {
    let builder = test_utils::TestConfigBuilder::new().with_campaign("test-campaign", "true");
    test_utils::create_test_app_with_state_with_config_if_available(builder)
        .await
        .expect("test app setup should either succeed or return None cleanly")
}

/// A router sharing `state`'s database and active run store, but whose
/// Redis connections for queue claims, rate limits and pub/sub fail.
fn app_with_broken_redis(state: &Arc<AppState>) -> axum::Router {
    let mut broken = (**state).clone();
    let client = redis::Client::open("redis://127.0.0.1:1").unwrap();
    broken.database = Arc::new(RunnerDatabase::new_with_redis(
        state.database.pool().clone(),
        client,
    ));
    janitor_runner::web::app(Arc::new(broken))
}

async fn get_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn post_json(app: axum::Router, uri: &str, body: Value) -> axum::response::Response {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

async fn enqueue_candidate(app: axum::Router, state: &Arc<AppState>, codebase: &str) {
    let url = format!("https://example.invalid/{codebase}");
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type) VALUES ($1, $2, $2, 'git')",
    )
    .bind(codebase)
    .bind(&url)
    .execute(state.database.pool())
    .await
    .unwrap();
    let response = post_json(
        app,
        "/candidates",
        json!([{"codebase": codebase, "campaign": "test-campaign"}]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn status_fails_when_reading_rate_limited_hosts_fails() {
    let Some((_app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };

    let req = Request::builder()
        .method(Method::GET)
        .uri("/status")
        .body(Body::empty())
        .unwrap();
    let response = app_with_broken_redis(&state).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn update_run_fails_when_publishing_fails() {
    let Some((_app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool();
    sqlx::query(
        "INSERT INTO codebase (name, branch_url, url, vcs_type)
         VALUES ('redis-upd-cb', 'https://example.invalid/x', 'https://example.invalid/x', 'git')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO change_set (id, campaign) VALUES ('redis-upd-cs', 'test-campaign')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO run (
             id, suite, codebase, result_code, revision,
             start_time, finish_time, logfilenames, change_set
         )
         VALUES ('redis-upd-run', 'test-campaign', 'redis-upd-cb', 'success', 'rev',
                 NOW() - INTERVAL '1 minute', NOW(), '{}', 'redis-upd-cs')",
    )
    .execute(pool)
    .await
    .unwrap();

    let response = post_json(
        app_with_broken_redis(&state),
        "/runs/redis-upd-run",
        json!({"publish_status": "approved"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn assign_fails_when_claiming_queue_item_fails() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    enqueue_candidate(app.clone(), &state, "redis-claim-cb").await;

    // Make the per-worker queue item set unwritable, so that claiming the
    // queue item fails with something other than a conflict.
    let worker = format!("redis-claim-worker-{}", uuid::Uuid::new_v4().simple());
    let key = format!("worker-queue-items:{worker}");
    let redis = state.database.redis().unwrap().clone();
    let mut conn = redis.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("not-a-set")
        .query_async(&mut conn)
        .await
        .unwrap();

    state
        .auth_service
        .create_worker(&worker, "pw", None)
        .await
        .unwrap();
    let response = post_json(app, "/active-runs", json!({"worker": worker})).await;
    let status = response.status();

    let _: () = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn finish_fails_when_publishing_result_fails() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    enqueue_candidate(app.clone(), &state, "redis-finish-cb").await;

    state
        .auth_service
        .create_worker("redis-finish-worker", "pw", None)
        .await
        .unwrap();
    let response = post_json(
        app,
        "/active-runs",
        json!({"worker": "redis-finish-worker"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let assignment = get_body(response).await;
    let run_id = assignment["id"].as_str().unwrap();

    let boundary = "redis-boundary";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"metadata\"; filename=\"result.json\"\r\n\
         Content-Type: application/json\r\n\r\n\
         {{\"code\":\"success\",\"description\":\"done\"}}\r\n\
         --{boundary}--\r\n"
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/active-runs/{run_id}/finish"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
    let response = app_with_broken_redis(&state).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
