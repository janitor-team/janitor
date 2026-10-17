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
fn test_publish_push_to_git_branch() {
    use breezyshim::branch::open_as_generic;
    use breezyshim::controldir::{
        create_branch_convenience_as_generic, create_standalone_workingtree,
        ControlDirFormatRegistry,
    };
    use breezyshim::tree::MutableTree;
    use breezyshim::workingtree::WorkingTree;

    breezyshim::init();
    let dir = std::env::temp_dir().join(format!("publish-one-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let formats = ControlDirFormatRegistry::new();
    let commit = |tree: &breezyshim::workingtree::GenericWorkingTree, name: &str| {
        std::fs::write(tree.basedir().join(name), name).unwrap();
        tree.add(&[std::path::Path::new(name)]).unwrap();
        tree.build_commit()
            .message(name)
            .committer("Test <test@example.com>")
            .commit()
            .unwrap()
    };

    let source_path = dir.join("source");
    let source_tree =
        create_standalone_workingtree(&source_path, &formats.make_controldir("git").unwrap())
            .unwrap();
    commit(&source_tree, "a");
    let source_branch = source_tree.branch();

    let target_path = dir.join("target");
    let target_url = url::Url::from_file_path(&target_path).unwrap();
    let target = create_branch_convenience_as_generic(
        &target_url,
        Some(false),
        &formats.make_controldir("git-bare").unwrap(),
    )
    .unwrap();
    source_branch.push(&target, false, None, None).unwrap();
    let new_revision = commit(&source_tree, "b");

    // `git:///path` runs the `git` binary and opens the target as a remote git branch.
    let remote_url: url::Url = format!("git://{}", target_path.display()).parse().unwrap();
    let result = publish(
        Environment::new(),
        "campaign",
        None,
        None,
        &serde_json::json!({}),
        Mode::Push,
        "main",
        None,
        open_as_generic(&remote_url).unwrap(),
        source_branch,
        "campaign",
        None,
        "log-id",
        None,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );

    let pushed = open_as_generic(&target_url).unwrap().last_revision();
    std::fs::remove_dir_all(&dir).unwrap();
    if let Err(e) = result {
        panic!("push failed: {}: {}", e.code(), e.description());
    }
    assert_eq!(pushed, new_revision);
}
