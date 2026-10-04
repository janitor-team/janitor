use chrono::{DateTime, Duration, Utc};
use janitor_publish::*;

#[test]
fn publish_error_display_messages() {
    let display = format!("{}", PublishError::AuthenticationFailed);
    assert!(display.to_lowercase().contains("authentication"));

    let display = format!(
        "{}",
        PublishError::NetworkError("Connection timeout".into())
    );
    assert!(display.contains("Connection timeout"));

    let display = format!("{}", PublishError::DatabaseError(sqlx::Error::RowNotFound));
    assert!(display.to_lowercase().contains("database"));
}

#[test]
fn check_mp_error_from_brz_login_required() {
    let converted: CheckMpError = breezyshim::error::Error::ForgeLoginRequired.into();
    assert!(matches!(converted, CheckMpError::ForgeLoginRequired));
}

#[test]
fn check_mp_error_no_run_display_includes_url() {
    let err = CheckMpError::NoRunForMergeProposal(
        url::Url::parse("https://github.com/owner/repo/pull/123").unwrap(),
    );
    assert!(format!("{}", err).contains("github.com/owner/repo/pull/123"));
}

#[test]
fn merge_proposal_list_entry_serializes_to_url_status_pair() {
    let entry = janitor_publish::web::MergeProposalListEntry {
        url: "https://github.com/owner/repo/pull/1".into(),
        status: Some("open".into()),
    };
    let json = serde_json::to_value(&entry).unwrap();
    let obj = json.as_object().expect("JSON object");
    assert_eq!(obj.len(), 2);
    assert_eq!(json["url"], "https://github.com/owner/repo/pull/1");
    assert_eq!(json["status"], "open");
}

#[test]
fn exponential_backoff_gates_re_runs() {
    let now = Utc::now();
    let old_finish = now - Duration::hours(10);

    // After 10 hours, attempts 0..=3 (needing 0, 2, 4, 8 hours) all release.
    for attempts in [0, 1, 2, 3] {
        let next = calculate_next_try_time(old_finish, attempts);
        assert!(now >= next, "attempts={} should be ready", attempts);
    }
    // But attempt 4 needs 16 hours: not ready yet.
    let next = calculate_next_try_time(old_finish, 4);
    assert!(now < next);
}

// Sanity check pinned against a known base time so the test is
// reproducible instead of only relative.
#[test]
fn backoff_from_fixed_base_time() {
    let base = DateTime::parse_from_rfc3339("2023-06-15T09:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    for (attempts, iso) in [
        (1, "2023-06-15T11:00:00Z"),
        (2, "2023-06-15T13:00:00Z"),
        (3, "2023-06-15T17:00:00Z"),
        (4, "2023-06-16T01:00:00Z"),
    ] {
        let want = DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(calculate_next_try_time(base, attempts), want);
    }
}
