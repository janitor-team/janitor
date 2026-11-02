//! Behaviour tests for `janitor_runner`. Each test round-trips real
//! data through the public API and asserts on the observable result.

use janitor_runner::{committer_env, is_log_filename, JanitorResult, QueueItem, VcsInfo};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

/// `committer_env` populates the eight expected env vars for a
/// well-formed committer string, and drops the ones lacking either
/// name or email when the input is partial.
#[test]
fn committer_env_matches_python_shape() {
    let result = committer_env(Some("John Doe <john@example.com>"));

    let expected = HashMap::from([
        ("DEBFULLNAME".to_string(), "John Doe".to_string()),
        ("GIT_COMMITTER_NAME".to_string(), "John Doe".to_string()),
        ("GIT_AUTHOR_NAME".to_string(), "John Doe".to_string()),
        ("DEBEMAIL".to_string(), "john@example.com".to_string()),
        (
            "GIT_COMMITTER_EMAIL".to_string(),
            "john@example.com".to_string(),
        ),
        (
            "GIT_AUTHOR_EMAIL".to_string(),
            "john@example.com".to_string(),
        ),
        ("EMAIL".to_string(), "john@example.com".to_string()),
        (
            "COMMITTER".to_string(),
            "John Doe <john@example.com>".to_string(),
        ),
        (
            "BRZ_EMAIL".to_string(),
            "John Doe <john@example.com>".to_string(),
        ),
    ]);
    assert_eq!(result, expected);

    assert!(committer_env(None).is_empty());

    let name_only = committer_env(Some("Name Only"));
    assert_eq!(name_only.get("DEBFULLNAME"), Some(&"Name Only".to_string()));
    assert!(!name_only.contains_key("DEBEMAIL"));

    let email_only = committer_env(Some("<email@only.com>"));
    assert_eq!(
        email_only.get("DEBEMAIL"),
        Some(&"email@only.com".to_string())
    );
    assert!(!email_only.contains_key("DEBFULLNAME"));
}

/// `is_log_filename` accepts the expected log-file shapes and
/// rejects others.
#[test]
fn is_log_filename_matches_python() {
    for good in [
        "foo.log",
        "foo.log.1",
        "foo.1.log",
        "build.log",
        "test.log.gz",
        "output.1.log",
        "script.log.2",
        "worker.log.10",
    ] {
        assert!(
            is_log_filename(good),
            "expected `{}` to be a log filename",
            good
        );
    }

    for bad in [
        "foo.1",
        "foo.1.log.1",
        "foo.1.notlog",
        "foo.txt",
        "log",
        "log.txt",
        "foo.LOG",
        "log.foo",
        "",
        ".",
        ".log",
    ] {
        assert!(
            !is_log_filename(bad),
            "expected `{}` to NOT be a log filename",
            bad
        );
    }
}

/// A queue item deserialises from the on-wire JSON without loss and
/// round-trips back to the same shape.
#[test]
fn queue_item_round_trips_python_json() {
    let python_json = r#"{
        "id": 12345,
        "context": {"branch": "main", "commit": "abc123"},
        "command": "lintian-fixes",
        "estimated_duration": 300,
        "campaign": "lintian-fixes",
        "refresh": true,
        "requester": "automated",
        "change_set": "cs-123",
        "codebase": "example-package"
    }"#;

    let queue_item: QueueItem = serde_json::from_str(python_json).unwrap();

    assert_eq!(queue_item.id, 12345);
    assert_eq!(queue_item.command, "lintian-fixes");
    assert_eq!(queue_item.campaign, "lintian-fixes");
    assert!(queue_item.refresh);
    assert_eq!(queue_item.requester.as_deref(), Some("automated"));
    assert_eq!(queue_item.change_set.as_deref(), Some("cs-123"));
    assert_eq!(queue_item.codebase, "example-package");
    assert_eq!(
        queue_item.estimated_duration,
        Some(Duration::from_secs(300))
    );

    let back: QueueItem =
        serde_json::from_str(&serde_json::to_string(&queue_item).unwrap()).unwrap();
    assert_eq!(back.id, 12345);
    assert_eq!(back.command, "lintian-fixes");
    assert_eq!(back.campaign, "lintian-fixes");
    assert!(back.refresh);
    assert_eq!(back.context, queue_item.context);
    assert_eq!(back.estimated_duration, Some(Duration::from_secs(300)));
}

/// Sparse `QueueItem` payloads (null optional fields) also deserialise.
#[test]
fn queue_item_accepts_null_optional_fields() {
    let minimal = r#"{
        "id": 1,
        "context": null,
        "command": "test",
        "estimated_duration": null,
        "campaign": "test-campaign",
        "refresh": false,
        "requester": null,
        "change_set": null,
        "codebase": "test-codebase"
    }"#;

    let item: QueueItem = serde_json::from_str(minimal).unwrap();
    assert_eq!(item.id, 1);
    assert_eq!(item.context, None);
    assert_eq!(item.estimated_duration, None);
    assert_eq!(item.requester, None);
    assert_eq!(item.change_set, None);
}

/// `JanitorResult` serialises with the field names downstream
/// consumers expect (`code`, `description`, `value`).
#[test]
fn janitor_result_serialises_expected_field_names() {
    let _vcs_info = VcsInfo {
        vcs_type: Some("git".to_string()),
        branch_url: Some("https://github.com/example/repo.git".to_string()),
        subpath: Some("debian".to_string()),
    };

    let result = JanitorResult {
        log_id: janitor::RunId::new("test-log-123").unwrap(),
        branch_url: "https://github.com/example/repo.git".to_string(),
        subpath: None,
        code: "success".to_string(),
        transient: Some(false),
        codebase: janitor::CodebaseName::new("example-repo").unwrap(),
        campaign: janitor::Campaign::new("lintian-fixes").unwrap(),
        description: Some("Successfully applied lintian fixes".to_string()),
        codemod: Some(json!({"fixes_applied": 5})),
        value: Some(100),
        logfilenames: vec!["build.log".to_string()],
        start_time: chrono::Utc::now(),
        finish_time: chrono::Utc::now(),
        revision: None,
        main_branch_revision: None,
        change_set: None,
        tags: None,
        remotes: None,
        branches: None,
        failure_details: None,
        failure_stage: None,
        resume: None,
        target: None,
        worker_name: None,
        vcs_type: None,
        target_branch_url: None,
        context: None,
        builder_result: None,
    };

    /// The fields a downstream consumer reads.
    #[derive(serde::Deserialize)]
    struct Consumer {
        code: String,
        description: Option<String>,
        value: Option<u64>,
        logfilenames: Vec<String>,
    }

    let consumer: Consumer =
        serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();
    assert_eq!(consumer.code, "success");
    assert_eq!(
        consumer.description.as_deref(),
        Some("Successfully applied lintian fixes")
    );
    assert_eq!(consumer.value, Some(100));
    assert_eq!(consumer.logfilenames, vec!["build.log"]);
}

/// The wire format for timestamps must round-trip through
/// RFC-3339 without losing precision, since that is what
/// `janitor.runner` emits over HTTP.
#[test]
fn timestamps_round_trip_rfc3339() {
    use chrono::{DateTime, Utc};

    let dt: DateTime<Utc> = "2023-10-15T14:30:00Z".parse().unwrap();
    let formatted = dt.to_rfc3339();
    assert_eq!(formatted, "2023-10-15T14:30:00+00:00");

    let parsed_back: DateTime<Utc> = formatted.parse().unwrap();
    assert_eq!(dt, parsed_back);
}
