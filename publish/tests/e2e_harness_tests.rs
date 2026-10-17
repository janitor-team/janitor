//! Checks that the shared e2e harness itself works.

mod e2e_harness;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn setup_serves_health_from_a_real_app_state() {
    let Some(harness) = e2e_harness::setup().await else {
        return;
    };
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = harness.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(harness
        .state
        .vcs_managers
        .contains_key(&janitor::vcs::VcsType::Git));
}

#[test]
fn fixture_builds_a_branch_the_publisher_url_points_at() {
    let fx = e2e_harness::GitFixture::new();
    let repo = fx.init_repo("cb");
    let tip = fx.add_branch(&repo, "lintian-fixes/main");
    assert_eq!(fx.tip(&repo, "lintian-fixes/main"), tip);
    assert_ne!(fx.tip(&repo, "main"), tip);
    let url = fx.source_branch_url("cb");
    assert_eq!(
        janitor::vcs::segment_branch_name(&url).as_deref(),
        Some("lintian-fixes/main")
    );
}

#[test]
fn daemon_serves_the_branch_the_source_url_names() {
    let fx = e2e_harness::GitFixture::new();
    let repo = fx.init_repo("cb");
    let tip = fx.add_branch(&repo, "lintian-fixes/main");
    let daemon = e2e_harness::GitDaemon::start(&fx);
    let url = daemon.source_branch_url("cb");
    assert_eq!(
        janitor::vcs::segment_branch_name(&url).as_deref(),
        Some("lintian-fixes/main")
    );
    let refs = fx.git(fx.base_path(), &["ls-remote", daemon.url_of("cb").as_str()]);
    assert!(refs.contains(&format!("{}\trefs/heads/lintian-fixes/main", tip)));
}
