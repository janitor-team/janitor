use crate::AppState;
use askama::Template;
use axum::response::IntoResponse;
use axum::{
    extract::Path, extract::State, http::HeaderMap, http::StatusCode, response::Json,
    response::Response, routing::get, routing::post, Router,
};
use janitor::api::worker::{Assignment, Metadata};
use std::sync::{Arc, RwLock};
#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate<'a> {
    pub assignment: Option<&'a Assignment>,
    pub metadata: Option<&'a Metadata>,
    pub lognames: Option<Vec<String>>,
}

#[derive(Template)]
#[template(path = "artifact_index.html")]
pub struct ArtifactIndexTemplate {
    pub names: Vec<String>,
}

#[derive(Template)]
#[template(path = "log_index.html")]
pub struct LogIndexTemplate {
    pub names: Vec<String>,
}

async fn index(State(state): State<Arc<RwLock<AppState>>>) -> Response {
    let state = state.read().unwrap();
    let lognames: Option<Vec<String>> =
        if let Some(output_directory) = state.output_directory.as_ref() {
            Some(
                output_directory
                    .read_dir()
                    .unwrap()
                    .filter_map(|entry| {
                        let entry = entry.ok()?;
                        let filename = entry.file_name();
                        let name = filename.to_str()?;
                        if name.ends_with(".log") {
                            Some(name.to_owned())
                        } else {
                            None
                        }
                    })
                    .collect(),
            )
        } else {
            None
        };

    let template = IndexTemplate {
        assignment: state.assignment.as_ref(),
        lognames,
        metadata: state.metadata.as_ref(),
    };
    match template.render() {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn kill(State(state): State<Arc<RwLock<AppState>>>) -> Response {
    // There is no cancellation handle threaded through run_worker's actual
    // command execution yet, so an in-progress build cannot be forcibly
    // stopped from here. Reporting this honestly (501) rather than 404ing
    // lets the runner's existing "kill not supported" handling deal with
    // it cleanly instead of crashing on an unexpected response.
    if state.read().unwrap().assignment.is_none() {
        // The run existed but is gone now (e.g. this worker restarted
        // since the run was assigned) - GONE fits that better than
        // NOT_FOUND.
        return (StatusCode::GONE, "no run in progress").into_response();
    }
    (StatusCode::NOT_IMPLEMENTED, "kill is not yet supported").into_response()
}

async fn health() -> String {
    "ok".to_string()
}

async fn assignment(State(state): State<Arc<RwLock<AppState>>>) -> Json<Option<Assignment>> {
    Json(state.read().unwrap().assignment.clone())
}

async fn get_logs(State(state): State<Arc<RwLock<AppState>>>, headers: HeaderMap) -> Response {
    let output_directory = &state.read().unwrap().output_directory;
    if output_directory.is_none() {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Log directory not created yet".into())
            .unwrap();
    }
    let output_directory = output_directory.as_ref().unwrap();
    let names: Vec<String> = match std::fs::read_dir(output_directory) {
        Ok(dir) => dir
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension()?.to_str()? == "log" {
                    Some(entry.file_name().to_str()?.to_owned())
                } else {
                    None
                }
            })
            .collect(),
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!("Error reading log directory: {}", e).into())
                .unwrap();
        }
    };

    match headers
        .get(axum::http::header::ACCEPT)
        .map(|x| x.to_str().unwrap())
    {
        Some("application/json") => Json(names).into_response(),
        _ => match (LogIndexTemplate { names }).render() {
            Ok(html) => axum::response::Html(html).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
    }
}

async fn get_artifacts(State(state): State<Arc<RwLock<AppState>>>, headers: HeaderMap) -> Response {
    let output_directory = &state.read().unwrap().output_directory;
    if output_directory.is_none() {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Artifact directory not created yet".into())
            .unwrap();
    }
    let output_directory = output_directory.as_ref().unwrap();
    let names: Vec<String> = match std::fs::read_dir(output_directory) {
        Ok(dir) => dir
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension()?.to_str()? != "log" {
                    Some(entry.file_name().to_str()?.to_owned())
                } else {
                    None
                }
            })
            .collect(),
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!("Error reading log directory: {}", e).into())
                .unwrap();
        }
    };

    match headers
        .get(axum::http::header::ACCEPT)
        .map(|x| x.to_str().unwrap())
    {
        Some("application/json") => Json(names).into_response(),
        _ => match (ArtifactIndexTemplate { names }).render() {
            Ok(html) => axum::response::Html(html).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        },
    }
}

async fn get_log_id(State(state): State<Arc<RwLock<AppState>>>) -> Response {
    match state.read().unwrap().assignment.as_ref() {
        Some(a) => a.id.clone().into_response(),
        None => (StatusCode::NOT_FOUND, "no run in progress").into_response(),
    }
}

async fn get_log_file(
    State(state): State<Arc<RwLock<AppState>>>,
    Path(filename): Path<String>,
) -> Response {
    // filenames should only contain characters that are safe to use in URLs
    if filename.contains('/') || filename.contains('\\') {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body("Invalid filename".into())
            .unwrap();
    }

    let p = if let Some(output_directory) = state.read().unwrap().output_directory.as_ref() {
        output_directory.join(filename)
    } else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Log directory not created yet".into())
            .unwrap();
    };

    let file = match tokio::fs::File::open(&p).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body("No such log file".into())
                .unwrap();
        }
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!("Error opening log file: {}", e).into())
                .unwrap();
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);

    let body = axum::body::Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/plain".parse().unwrap(),
    );

    (StatusCode::OK, headers, body).into_response()
}

async fn get_artifact_file(
    State(state): State<Arc<RwLock<AppState>>>,
    Path(filename): Path<String>,
) -> Response {
    // filenames should only contain characters that are safe to use in URLs
    if filename.contains('/') || filename.contains('\\') {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body("Invalid filename".into())
            .unwrap();
    }

    let p = if let Some(output_directory) = state.read().unwrap().output_directory.as_ref() {
        output_directory.join(filename)
    } else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Artifact directory not created yet".into())
            .unwrap();
    };

    let file = match tokio::fs::File::open(&p).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body("No such artifact file".into())
                .unwrap();
        }
        Err(e) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!("Error opening artifact file: {}", e).into())
                .unwrap();
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);

    let body = axum::body::Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );

    (StatusCode::OK, headers, body).into_response()
}

pub fn app(state: Arc<RwLock<AppState>>) -> Router {
    // build our application with a route
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/kill", post(kill))
        .route("/assignment", get(assignment))
        .route("/logs", get(get_logs))
        .route("/logs/{filename}", get(get_log_file))
        .route("/log-id", get(get_log_id))
        .route("/artifacts", get(get_artifacts))
        .route("/artifacts/{filename}", get(get_artifact_file))
        .with_state(state.clone().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt; // for `collect`

    async fn get_body(response: Response<Body>) -> String {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(body[..].to_vec()).unwrap()
    }

    #[tokio::test]
    async fn test_health() {
        let app = app(Arc::new(RwLock::new(AppState::default())));
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body = get_body(response).await;
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn test_assignment_empty() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let app = app(state.clone());
        let request = Request::builder()
            .uri("/assignment")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body = get_body(response).await;
        assert_eq!(body, "null");

        if let Ok(mut state) = state.write() {
            state.assignment = Some(Assignment {
                id: "test".to_string(),
                queue_id: 1,
                campaign: "lintian-fixes".to_string(),
                codebase: "foo".to_string(),
                force_build: true,
                branch: janitor::api::worker::Branch {
                    cached_url: None,
                    vcs_type: janitor::vcs::VcsType::Git,
                    url: Some("https://example.com/vcs".parse().unwrap()),
                    subpath: std::path::PathBuf::from(""),
                    additional_colocated_branches: Some(vec![]),
                    default_empty: false,
                },
                resume: None,
                target_repository: janitor::api::worker::TargetRepository {
                    url: "https://example.com/".parse().unwrap(),
                },
                skip_setup_validation: false,
                codemod: janitor::api::worker::Codemod {
                    command: "echo".to_string(),
                    environment: std::collections::HashMap::new(),
                },
                env: std::collections::HashMap::new(),
                build: janitor::api::worker::Build {
                    config: serde_json::Value::Null,
                    environment: None,
                    target: "test".to_string(),
                },
            });
        };

        let request = Request::builder()
            .uri("/assignment")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body: Assignment = serde_json::from_str(&get_body(response).await).unwrap();
        assert_eq!(body.id, "test");
        assert_eq!(body.queue_id, 1);
    }

    #[tokio::test]
    async fn test_kill_no_assignment() {
        let app = app(Arc::new(RwLock::new(AppState::default())));
        let request = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/kill")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 410);

        let body = get_body(response).await;
        assert_eq!(body, "no run in progress");
    }

    #[tokio::test]
    async fn test_kill_with_assignment() {
        let state = Arc::new(RwLock::new(AppState::default()));
        if let Ok(mut state) = state.write() {
            state.assignment = Some(Assignment {
                id: "test".to_string(),
                queue_id: 1,
                campaign: "lintian-fixes".to_string(),
                codebase: "foo".to_string(),
                force_build: true,
                branch: janitor::api::worker::Branch {
                    cached_url: None,
                    vcs_type: janitor::vcs::VcsType::Git,
                    url: Some("https://example.com/vcs".parse().unwrap()),
                    subpath: std::path::PathBuf::from(""),
                    additional_colocated_branches: Some(vec![]),
                    default_empty: false,
                },
                resume: None,
                target_repository: janitor::api::worker::TargetRepository {
                    url: "https://example.com/".parse().unwrap(),
                },
                skip_setup_validation: false,
                codemod: janitor::api::worker::Codemod {
                    command: "echo".to_string(),
                    environment: std::collections::HashMap::new(),
                },
                env: std::collections::HashMap::new(),
                build: janitor::api::worker::Build {
                    config: serde_json::Value::Null,
                    environment: None,
                    target: "test".to_string(),
                },
            });
        };

        let app = super::app(state.clone());
        let request = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/kill")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 501);

        let body = get_body(response).await;
        assert_eq!(body, "kill is not yet supported");
    }

    #[tokio::test]
    async fn test_logs_empty() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let app = app(state.clone());
        let request = Request::builder().uri("/logs").body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);

        let body = get_body(response).await;
        assert_eq!(body, "Log directory not created yet");

        let td = tempfile::tempdir().unwrap();

        if let Ok(mut state) = state.write() {
            state.output_directory = Some(td.path().to_path_buf());
        };

        let request = Request::builder()
            .uri("/logs")
            .header("Accept", "application/json")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body: Vec<String> = serde_json::from_str(&get_body(response).await).unwrap();
        assert_eq!(body, Vec::<String>::new());

        // create a log file
        std::fs::write(td.path().join("test.log"), "test content").unwrap();

        let request = Request::builder()
            .uri("/logs")
            .header("Accept", "application/json")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), 200);

        let body: Vec<String> = serde_json::from_str(&get_body(response).await).unwrap();

        assert_eq!(body, vec!["test.log"]);

        // get the log file
        let request = Request::builder()
            .uri("/logs/test.log")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), 200);

        let body = get_body(response).await;

        assert_eq!(body, "test content");
    }

    #[tokio::test]
    async fn test_artifacts_empty() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let app = app(state.clone());
        let request = Request::builder()
            .uri("/artifacts")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);

        let body = get_body(response).await;
        assert_eq!(body, "Artifact directory not created yet");

        let td = tempfile::tempdir().unwrap();

        if let Ok(mut state) = state.write() {
            state.output_directory = Some(td.path().to_path_buf());
        };

        let request = Request::builder()
            .uri("/artifacts")
            .header("Accept", "application/json")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body: Vec<String> = serde_json::from_str(&get_body(response).await).unwrap();
        assert_eq!(body, Vec::<String>::new());

        // create a log file
        std::fs::write(td.path().join("test.log"), "test").unwrap();

        let request = Request::builder()
            .uri("/artifacts")
            .header("Accept", "application/json")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), 200);

        let body: Vec<String> = serde_json::from_str(&get_body(response).await).unwrap();

        assert_eq!(body, Vec::<String>::new());

        // create an artifact file
        std::fs::write(td.path().join("test.artifact"), "test").unwrap();

        let request = Request::builder()
            .uri("/artifacts")
            .header("Accept", "application/json")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), 200);

        let body: Vec<String> = serde_json::from_str(&get_body(response).await).unwrap();

        assert_eq!(body, vec!["test.artifact"]);

        // get the artifact file

        let request = Request::builder()
            .uri("/artifacts/test.artifact")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), 200);

        let body = get_body(response).await;

        assert_eq!(body, "test");
    }

    #[tokio::test]
    async fn test_index() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let app = app(state.clone());
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body = get_body(response).await;
        assert!(body.contains("Job"));
    }

    #[tokio::test]
    async fn test_log_file_not_found() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/logs/nonexistent.log")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(get_body(response).await, "No such log file");
    }

    #[tokio::test]
    async fn test_log_file_path_traversal() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/logs/..%2F..%2Fetc%2Fpasswd")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        // Should reject paths with slashes after URL decoding
        assert_eq!(response.status(), 400);
        assert!(get_body(response).await.starts_with("Invalid filename"));
    }

    #[tokio::test]
    async fn test_artifact_file_not_found() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/artifacts/nonexistent.deb")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(get_body(response).await, "No such artifact file");
    }

    #[tokio::test]
    async fn test_artifact_file_path_traversal() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/artifacts/..%5Ctest.txt")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        // Should reject paths with backslashes
        assert_eq!(response.status(), 400);
        assert!(get_body(response).await.starts_with("Invalid filename"));
    }

    #[tokio::test]
    async fn test_log_file_content_type() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());
        std::fs::write(td.path().join("build.log"), "log content here").unwrap();

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/logs/build.log")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/plain"
        );
        assert_eq!(get_body(response).await, "log content here");
    }

    #[tokio::test]
    async fn test_artifact_file_content_type() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());
        std::fs::write(td.path().join("package.deb"), "fake deb content").unwrap();

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/artifacts/package.deb")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(get_body(response).await, "fake deb content");
    }

    #[tokio::test]
    async fn test_logs_html_response() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());
        std::fs::write(td.path().join("build.log"), "content").unwrap();

        let app = app(state.clone());
        // Without Accept: application/json, should get HTML
        let request = Request::builder().uri("/logs").body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body = get_body(response).await;
        // Should be HTML, not JSON
        assert!(body.contains("<"), "Expected HTML response");
    }

    #[tokio::test]
    async fn test_artifacts_html_response() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());
        std::fs::write(td.path().join("package.deb"), "content").unwrap();

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/artifacts")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body = get_body(response).await;
        assert!(body.contains("<"), "Expected HTML response");
    }

    #[tokio::test]
    async fn test_artifacts_excludes_log_files() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());
        std::fs::write(td.path().join("build.log"), "log").unwrap();
        std::fs::write(td.path().join("package.deb"), "deb").unwrap();
        std::fs::write(td.path().join("result.tar.gz"), "tar").unwrap();

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/artifacts")
            .header("Accept", "application/json")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body: Vec<String> = serde_json::from_str(&get_body(response).await).unwrap();
        // build.log should be excluded from artifacts
        assert!(!body.contains(&"build.log".to_string()));
        assert!(body.contains(&"package.deb".to_string()));
        assert!(body.contains(&"result.tar.gz".to_string()));
    }

    #[tokio::test]
    async fn test_logs_only_log_files() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let td = tempfile::tempdir().unwrap();
        state.write().unwrap().output_directory = Some(td.path().to_path_buf());
        std::fs::write(td.path().join("build.log"), "log").unwrap();
        std::fs::write(td.path().join("package.deb"), "deb").unwrap();

        let app = app(state.clone());
        let request = Request::builder()
            .uri("/logs")
            .header("Accept", "application/json")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body: Vec<String> = serde_json::from_str(&get_body(response).await).unwrap();
        assert_eq!(body, vec!["build.log"]);
    }

    #[tokio::test]
    async fn test_log_file_no_output_dir() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let app = app(state.clone());
        let request = Request::builder()
            .uri("/logs/build.log")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(get_body(response).await, "Log directory not created yet");
    }

    #[tokio::test]
    async fn test_artifact_file_no_output_dir() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let app = app(state.clone());
        let request = Request::builder()
            .uri("/artifacts/package.deb")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(
            get_body(response).await,
            "Artifact directory not created yet"
        );
    }

    #[tokio::test]
    async fn test_log_id() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let app = app(state.clone());
        let request = Request::builder()
            .uri("/log-id")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 404);

        let body = get_body(response).await;
        assert_eq!(body, "no run in progress");

        if let Ok(mut state) = state.write() {
            state.assignment = Some(Assignment {
                id: "test".to_string(),
                queue_id: 1,
                campaign: "lintian-fixes".to_string(),
                codebase: "foo".to_string(),
                force_build: true,
                branch: janitor::api::worker::Branch {
                    cached_url: None,
                    vcs_type: janitor::vcs::VcsType::Git,
                    url: Some("https://example.com/vcs".parse().unwrap()),
                    subpath: std::path::PathBuf::from(""),
                    additional_colocated_branches: Some(vec![]),
                    default_empty: false,
                },
                resume: None,
                target_repository: janitor::api::worker::TargetRepository {
                    url: "https://example.com/".parse().unwrap(),
                },
                skip_setup_validation: false,
                codemod: janitor::api::worker::Codemod {
                    command: "echo".to_string(),
                    environment: std::collections::HashMap::new(),
                },
                env: std::collections::HashMap::new(),
                build: janitor::api::worker::Build {
                    config: serde_json::Value::Null,
                    environment: None,
                    target: "test".to_string(),
                },
            });
        };

        let request = Request::builder()
            .uri("/log-id")
            .body(Body::empty())
            .unwrap();

        let app = super::app(state.clone());
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);

        let body = get_body(response).await;
        assert_eq!(body, "test");
    }
}
