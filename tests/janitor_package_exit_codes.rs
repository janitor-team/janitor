//! Exit codes of the `janitor-package` binary.

use std::process::Command;

/// A port nothing listens on, so the request fails without reaching a network.
const UNREACHABLE: &str = "http://127.0.0.1:1/";

#[test]
fn a_failing_command_exits_one_and_says_why() {
    let out = Command::new(env!("CARGO_BIN_EXE_janitor-package"))
        .args(["--url", UNREACHABLE, "status"])
        // reqwest honours the proxy variables, and a proxy would send this
        // off the machine instead of to the closed port.
        .env("NO_PROXY", "*")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy")
        .output()
        .expect("run janitor-package");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(1),
        "exit code was {:?}, stderr was {}",
        out.status.code(),
        stderr
    );
    assert!(
        stderr.contains("error: "),
        "stderr did not report the error: {}",
        stderr
    );
}
