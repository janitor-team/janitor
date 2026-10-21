//! Run the janitor-mail-filter binary against a local HTTP server.

use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;

const ROUTE: &str = "/api/refresh-proposal-status";

/// Logged before every refresh, at the default `info` level.
const FOUND: &str =
    "Found merge proposal URL: https://github.com/UbuntuBudgie/budgie-desktop/pull/78";

#[derive(Debug, PartialEq)]
struct Seen {
    authorization: Option<String>,
    content_type: Option<String>,
    body: String,
}

/// Serve `status` on `ROUTE`, returning the port and the requests seen.
fn serve(status: StatusCode) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().route(
        ROUTE,
        post(move |headers: HeaderMap, body: String| async move {
            let header = |name| headers.get(name).map(|v| v.to_str().unwrap().to_string());
            log.lock().unwrap().push(Seen {
                authorization: header("authorization"),
                content_type: header("content-type"),
                body,
            });
            status
        }),
    );
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
    });
    (port, seen)
}

fn run_filter(refresh_url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_janitor-mail-filter"))
        .arg("--refresh-url")
        .arg(refresh_url)
        .arg("--input")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/github-merged-email.txt"
        ))
        .env_remove("RUST_LOG")
        .env_remove("RUST_LOG_STYLE")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("all_proxy")
        .env_remove("ALL_PROXY")
        .output()
        .unwrap()
}

/// The logged messages, without the timestamp, level and target.
fn logged(output: &Output) -> Vec<String> {
    String::from_utf8(output.stderr.clone())
        .unwrap()
        .lines()
        .map(|line| line.split_once("] ").unwrap().1.to_string())
        .collect()
}

#[test]
fn test_refresh_posts_form_with_basic_auth() {
    let (port, seen) = serve(StatusCode::ACCEPTED);
    let output = run_filter(&format!("http://bot:secret@127.0.0.1:{}{}", port, ROUTE));

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(logged(&output), vec![FOUND]);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Seen {
            authorization: Some("Basic Ym90OnNlY3JldA==".to_string()),
            content_type: Some("application/x-www-form-urlencoded".to_string()),
            body: "url=https%3A%2F%2Fgithub.com%2FUbuntuBudgie%2Fbudgie-desktop%2Fpull%2F78"
                .to_string(),
        }]
    );
}

#[test]
fn test_refresh_failure_logs_redacted_url() {
    let (port, _) = serve(StatusCode::UNAUTHORIZED);
    let output = run_filter(&format!(
        "http://bot:secret@127.0.0.1:{}{}?token=sesame",
        port, ROUTE
    ));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        logged(&output),
        vec![
            FOUND.to_string(),
            format!(
                "Error: error 401 triggering refresh for http://127.0.0.1:{}{}",
                port, ROUTE
            )
        ]
    );
}

#[test]
fn test_refresh_send_error_logs_redacted_url() {
    // Bind and drop a listener to get a port that refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let output = run_filter(&format!(
        "http://bot:secret@127.0.0.1:{}{}?token=sesame",
        port, ROUTE
    ));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        logged(&output),
        vec![
            FOUND.to_string(),
            format!(
                "Error: error triggering refresh for http://127.0.0.1:{}{}: error sending request",
                port, ROUTE
            )
        ]
    );

    let output = run_filter("bot:secret@localhost/api?token=sesame");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        logged(&output),
        vec![
            FOUND,
            "Error: error triggering refresh for <invalid URL>: builder error"
        ]
    );
}
