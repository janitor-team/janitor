use super::*;

#[test]
fn test_drop_env() {
    let mut args = vec![
        "FOO=bar".to_string(),
        "BAZ=qux".to_string(),
        "actual".to_string(),
        "command".to_string(),
    ];
    drop_env(&mut args);
    assert_eq!(args, vec!["actual", "command"]);
}

#[test]
fn test_drop_env_no_env_vars() {
    let mut args = vec!["actual".to_string(), "command".to_string()];
    let original = args.clone();
    drop_env(&mut args);
    assert_eq!(args, original);
}

#[test]
fn test_drop_env_empty() {
    let mut args = vec![];
    drop_env(&mut args);
    assert!(args.is_empty());
}

#[test]
fn test_drop_env_only_env_vars() {
    let mut args = vec!["FOO=bar".to_string(), "BAZ=qux".to_string()];
    drop_env(&mut args);
    assert!(args.is_empty());
}

#[test]
fn test_drop_env_with_equals_in_arg() {
    let mut args = vec![
        "FOO=bar".to_string(),
        "command".to_string(),
        "--option=value".to_string(),
    ];
    drop_env(&mut args);
    assert_eq!(args, vec!["command", "--option=value"]);
}

#[test]
fn test_branch_open_failure() {
    let url = url::Url::parse("https://example.com/repo").unwrap();
    let other = |e: BranchOpenError| PublishError::failure("other", e.to_string());
    let cases = [
        (
            BranchOpenError::RateLimited {
                url: url.clone(),
                description: "slow down".to_string(),
                retry_after: None,
            },
            "resume-branch-rate-limited",
            "Resume branch rate limited: slow down",
        ),
        (
            BranchOpenError::TemporarilyUnavailable {
                url: url.clone(),
                description: "503".to_string(),
            },
            "resume-branch-temporarily-unavailable",
            "Resume branch temporarily unavailable: 503",
        ),
        (
            BranchOpenError::Unavailable {
                url: url.clone(),
                description: "gone".to_string(),
            },
            "resume-branch-unavailable",
            "Resume branch unavailable: gone",
        ),
        (
            BranchOpenError::Missing {
                url: url.clone(),
                description: "404".to_string(),
            },
            "resume-branch-missing",
            "Resume branch missing: 404",
        ),
    ];
    for (e, code, description) in cases {
        let err = branch_open_failure(e, "Resume branch", "resume-branch-", other);
        assert_eq!((err.code(), err.description()), (code, description));
    }
    let err = branch_open_failure(
        BranchOpenError::Other("boom".to_string()),
        "Resume branch",
        "resume-branch-",
        other,
    );
    assert_eq!(err.code(), "other");
}
