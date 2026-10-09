//! The password in the cache branch URL must not reach the log.

use breezyshim::branch::Branch;
use breezyshim::tree::MutableTree;
use breezyshim::workingtree::WorkingTree;
use janitor::api::worker::Metadata;
use janitor::vcs::VcsType;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Logger that keeps every message so the test can read them back.
struct Capture(Mutex<Vec<String>>);

impl log::Log for Capture {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.0.lock().unwrap().push(record.args().to_string());
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

const COMMITTER: &str = "Committer <committer@example.com>";

/// A run that pushes to a cache branch URL with a password logs the URL without it.
#[test]
fn cache_branch_push_does_not_log_the_password() {
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Trace);

    let tmp = tempfile::tempdir().unwrap();
    let wt = breezyshim::controldir::create_standalone_workingtree(
        &tmp.path().join("main"),
        &breezyshim::controldir::FORMAT_REGISTRY
            .make_controldir("git")
            .unwrap(),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("main/Makefile"),
        "all:\n\ntest:\n\ncheck:\n",
    )
    .unwrap();
    wt.add(&[Path::new("Makefile")]).unwrap();
    wt.build_commit()
        .message("Add makefile")
        .committer(COMMITTER)
        .commit()
        .unwrap();
    std::fs::create_dir(tmp.path().join("target")).unwrap();
    let output_dir = tmp.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();

    // Nothing listens on port 1, so the push to the cache fails at once.
    let cached_branch_url = url::Url::parse("http://worker:hunter2@127.0.0.1:1/foo").unwrap();

    janitor_worker::run_worker(
        "mycodebase",
        "mycampaign",
        Some(&wt.branch().get_user_url()),
        "run-id",
        &serde_json::json!({"chroot": null, "dep_server_url": null}),
        HashMap::from([("COMMITTER".to_string(), COMMITTER.to_string())]),
        vec!["sh", "-c", "echo foo > bar"],
        &output_dir,
        &mut Metadata::default(),
        &url::Url::from_directory_path(tmp.path().join("target")).unwrap(),
        "foo",
        "generic",
        Some(VcsType::Git),
        Path::new(""),
        None,
        Some(&cached_branch_url),
        None,
        None,
        &mut None,
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let messages = CAPTURE.0.lock().unwrap().clone();
    let about_cache_push: Vec<&String> = messages
        .iter()
        .filter(|m| m.contains("ackaging branch cache to") || m.contains("push to cache URL"))
        .collect();
    assert_eq!(about_cache_push.len(), 2, "{:#?}", messages);
    let leaked: Vec<&String> = messages.iter().filter(|m| m.contains("hunter2")).collect();
    assert!(leaked.is_empty(), "password in the log: {:#?}", leaked);
}
