//! A failed push of the cache branch must not fail the run.

use breezyshim::branch::Branch;
use breezyshim::tree::MutableTree;
use breezyshim::workingtree::WorkingTree;
use janitor::api::worker::Metadata;
use janitor::vcs::VcsType;
use std::collections::HashMap;
use std::io::{Read, Write};
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

/// An https cache URL that reaches a plain HTTP server gives a warning, not a panic.
#[test]
fn cache_branch_push_to_plain_http_only_warns() {
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

    // A plain HTTP server, so the TLS handshake fails.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let _ = conn.read(&mut [0; 4096]);
            let _ = conn.write_all(b"HTTP/1.0 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        }
    });
    let cached_branch_url =
        url::Url::parse(&format!("https://worker:hunter2@127.0.0.1:{}/foo", port)).unwrap();

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
    let prefix = format!(
        "unable to push to cache URL https://127.0.0.1:{}/foo: ",
        port
    );
    let warnings: Vec<&String> = messages.iter().filter(|m| m.starts_with(&prefix)).collect();
    assert_eq!(warnings.len(), 1, "{:#?}", messages);
    assert!(
        warnings[0].len() > prefix.len(),
        "no error text: {}",
        warnings[0]
    );
}
