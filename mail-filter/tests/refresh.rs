//! Run the janitor-mail-filter binary against a local HTTP server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::thread;

const FORM: &str = "application/x-www-form-urlencoded";

struct Seen {
    request_line: String,
    content_type: String,
    body: String,
}

/// Serve one request the way the publisher's /refresh-status does.
fn serve_once(listener: TcpListener, form_status: &'static str) -> Seen {
    let (stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut content_type = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "content-type" => content_type = value.trim().to_string(),
            "content-length" => length = value.trim().parse().unwrap(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let status = if content_type == FORM {
        form_status
    } else {
        "415 Unsupported Media Type"
    };
    let mut stream = reader.into_inner();
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status
    )
    .unwrap();
    Seen {
        request_line: request_line.trim_end().to_string(),
        content_type,
        body: String::from_utf8(body).unwrap(),
    }
}

fn run_filter(form_status: &'static str) -> (Output, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || serve_once(listener, form_status));
    let output = Command::new(env!("CARGO_BIN_EXE_janitor-mail-filter"))
        .arg("--refresh-url")
        .arg(format!("http://127.0.0.1:{}/refresh-status", port))
        .arg("--input")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/github-merged-email.txt"
        ))
        .env_remove("RUST_LOG")
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("all_proxy")
        .env_remove("ALL_PROXY")
        .output()
        .unwrap();
    (output, server.join().unwrap())
}

#[test]
fn test_refresh_posts_form() {
    let (output, seen) = run_filter("202 Accepted");

    assert_eq!(seen.request_line, "POST /refresh-status HTTP/1.1");
    assert_eq!(seen.content_type, FORM);
    assert_eq!(
        seen.body,
        "url=https%3A%2F%2Fgithub.com%2FUbuntuBudgie%2Fbudgie-desktop%2Fpull%2F78"
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn test_refresh_failure_logs_status() {
    let (output, _) = run_filter("401 Unauthorized");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "stderr: {:?}", stderr);
    assert!(stderr.contains("error 401"), "stderr: {:?}", stderr);
    assert!(!stderr.contains("budgie"), "stderr: {:?}", stderr);
    assert!(!stderr.contains("127.0.0.1"), "stderr: {:?}", stderr);
}

#[test]
fn test_refresh_send_error_hides_url() {
    // Bind and drop a listener to get a port that refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let output = Command::new(env!("CARGO_BIN_EXE_janitor-mail-filter"))
        .arg("--refresh-url")
        .arg(format!(
            "http://user:hunter2@127.0.0.1:{}/refresh-status?token=sesame",
            port
        ))
        .arg("--input")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/github-merged-email.txt"
        ))
        .env_remove("RUST_LOG")
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("all_proxy")
        .env_remove("ALL_PROXY")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "stderr: {:?}", stderr);
    assert!(!stderr.contains("hunter2"), "stderr: {:?}", stderr);
    assert!(!stderr.contains("sesame"), "stderr: {:?}", stderr);
    assert!(!stderr.contains("127.0.0.1"), "stderr: {:?}", stderr);
}
