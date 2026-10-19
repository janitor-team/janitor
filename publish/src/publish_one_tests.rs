use super::*;

#[test]
fn test_drop_env() {
    for (args, expected) in [
        (
            vec!["FOO=bar", "BAZ=qux", "actual", "command"],
            vec!["actual", "command"],
        ),
        (vec!["actual", "command"], vec!["actual", "command"]),
        (vec![], vec![]),
        (vec!["FOO=bar", "BAZ=qux"], vec![]),
        (
            vec!["FOO=bar", "command", "--option=value"],
            vec!["command", "--option=value"],
        ),
    ] {
        let mut args: Vec<String> = args.into_iter().map(String::from).collect();
        drop_env(&mut args);
        assert_eq!(args, expected);
    }
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
