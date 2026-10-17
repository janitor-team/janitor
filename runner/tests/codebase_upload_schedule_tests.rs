//! `POST /codebases` schedules the candidates of a codebase whose
//! branch URL or subpath changed.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

use janitor_runner::{test_utils, AppState};

async fn setup() -> Option<(axum::Router, Arc<AppState>)> {
    test_utils::create_test_app_with_state_if_available()
        .await
        .expect("test app setup should either succeed or return None cleanly")
}

async fn upload(app: &axum::Router, codebase: Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/codebases")
        .header("content-type", "application/json")
        .body(Body::from(json!([codebase]).to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

async fn insert_candidate(pool: &sqlx::PgPool, codebase: &str, suite: &str) {
    sqlx::query("INSERT INTO candidate (codebase, suite, command) VALUES ($1, $2, 'true')")
        .bind(codebase)
        .bind(suite)
        .execute(pool)
        .await
        .expect("candidate insert");
}

/// The queue as `(suite, bucket, requester)` rows.
async fn queue(pool: &sqlx::PgPool) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as("SELECT suite::text, bucket::text, requester FROM queue ORDER BY suite")
        .fetch_all(pool)
        .await
        .expect("queue lookup")
}

fn queued(suite: &str) -> (String, String, Option<String>) {
    (
        suite.to_string(),
        "default".to_string(),
        Some("codebase update".to_string()),
    )
}

#[tokio::test]
async fn changed_branch_url_schedules_all_candidates() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool().clone();
    upload(
        &app,
        json!({"name": "cb", "branch_url": "https://example.invalid/old"}),
    )
    .await;
    insert_candidate(&pool, "cb", "one").await;
    insert_candidate(&pool, "cb", "two").await;

    upload(
        &app,
        json!({"name": "cb", "branch_url": "https://example.invalid/new"}),
    )
    .await;
    assert_eq!(queue(&pool).await, [queued("one"), queued("two")]);

    // A second change updates the queued items, it does not add more.
    upload(
        &app,
        json!({"name": "cb", "branch_url": "https://example.invalid/newer"}),
    )
    .await;
    assert_eq!(queue(&pool).await, [queued("one"), queued("two")]);
}

#[tokio::test]
async fn changed_subpath_schedules_candidates() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool().clone();
    let url = "https://example.invalid/cb";
    upload(&app, json!({"name": "cb", "branch_url": url})).await;
    insert_candidate(&pool, "cb", "one").await;

    upload(
        &app,
        json!({"name": "cb", "branch_url": url, "subpath": "sub"}),
    )
    .await;
    assert_eq!(queue(&pool).await, [queued("one")]);
}

#[tokio::test]
async fn unchanged_or_new_codebase_schedules_nothing() {
    let Some((app, state)) = setup().await else {
        eprintln!("skipping: no test resources");
        return;
    };
    let pool = state.database.pool().clone();
    let url = "https://example.invalid/cb";
    // New codebase.
    upload(&app, json!({"name": "cb", "branch_url": url})).await;
    assert_eq!(queue(&pool).await, []);

    // Same branch URL and subpath; other fields do not count.
    insert_candidate(&pool, "cb", "one").await;
    upload(
        &app,
        json!({"name": "cb", "branch_url": url, "subpath": "", "vcs_last_revision": "abc", "value": 5}),
    )
    .await;
    assert_eq!(queue(&pool).await, []);
}
