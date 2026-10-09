//! `iter_all_mps` yields a failed proposal as an `Err` item.

use breezyshim::forge::MergeProposalStatus;
use breezyshim::testing::TestEnv;
use janitor_publish::iter_all_mps;

#[test]
fn per_proposal_error_is_yielded() {
    // Nothing listens on this port, so every forge request fails.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let closed = format!("http://127.0.0.1:{}/", port);
    let env = TestEnv::new();
    // Python is already up, so the proxy has to go into its own `os.environ`.
    pyo3::Python::attach(|py| {
        use pyo3::prelude::*;
        let environ = py.import("os").unwrap().getattr("environ").unwrap();
        for name in ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"] {
            environ.set_item(name, &closed).unwrap();
        }
        for name in ["no_proxy", "NO_PROXY"] {
            environ.call_method1("pop", (name, py.None())).unwrap();
        }
    });
    breezyshim::plugin::load_plugins();
    std::fs::write(
        env.home_dir.join(".python-gitlab.cfg"),
        format!("[local]\nurl = {}\nprivate_token = x\n", closed),
    )
    .unwrap();

    let errors = iter_all_mps(Some(&[MergeProposalStatus::Open]))
        .filter(|item| item.is_err())
        .count();
    assert!(errors >= 1, "no failed proposal was yielded");
}
