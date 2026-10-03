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
fn test_source_branch_name_round_trips_the_publisher_url() {
    // publish::Publisher builds the source branch URL as
    // get_branch_url(codebase, "<campaign>/<role>"), so the name always
    // contains a /. publish_one has to hand that name to open_branch
    // unescaped.
    use janitor::vcs::VcsManager;
    let mgr = janitor::vcs::RemoteGitVcsManager::new(
        url::Url::parse("https://vcs.example.com/git/").unwrap(),
    );
    let url = mgr.get_branch_url("mycodebase", &format!("{}/{}", "lintian-fixes", "main"));
    assert_eq!(
        janitor::vcs::segment_branch_name(&url).as_deref(),
        Some("lintian-fixes/main")
    );
}

#[test]
fn test_publisher_url_segment_param_is_not_a_usable_branch_name() {
    // What open_branch would resolve on its own when passed None: the raw,
    // still-escaped parameter, which is not a ref that exists.
    use janitor::vcs::VcsManager;
    let mgr = janitor::vcs::RemoteGitVcsManager::new(
        url::Url::parse("https://vcs.example.com/git/").unwrap(),
    );
    let url = mgr.get_branch_url("mycodebase", "lintian-fixes/main");
    let (base, params) = breezyshim::urlutils::split_segment_parameters(&url);
    assert_eq!(base.as_str(), "https://vcs.example.com/git/mycodebase");
    assert_eq!(
        params.get("branch").map(|s| s.as_str()),
        Some("lintian-fixes%2Fmain")
    );
    assert_ne!(
        params.get("branch").map(|s| s.as_str()),
        janitor::vcs::segment_branch_name(&url).as_deref()
    );
}
