use crate::get_build_arch;
use backoff::ExponentialBackoff;
use janitor::api::worker::{Assignment, Metadata};
use reqwest::multipart::{Form, Part};
use reqwest::{Error as ReqwestError, Response, StatusCode, Url};
use std::error::Error;
use std::path::Path;

#[derive(Debug)]
pub enum AssignmentError {
    Failure(String),
    EmptyQueue,
}

impl std::fmt::Display for AssignmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AssignmentError::Failure(msg) => write!(f, "AssignmentError: {}", msg),
            AssignmentError::EmptyQueue => write!(f, "AssignmentError: EmptyQueue"),
        }
    }
}

impl std::error::Error for AssignmentError {}

/// Get an assignment from the queue.
pub async fn get_assignment_raw(
    session: &reqwest::Client,
    credentials: &Credentials,
    my_url: Option<&Url>,
    base_url: &Url,
    node_name: &str,
    jenkins_build_url: Option<&Url>,
    codebase: Option<&str>,
    campaign: Option<&str>,
    exclude_hosts: &[String],
) -> Result<serde_json::Value, AssignmentError> {
    let assign_url = base_url
        .join("active-runs")
        .map_err(|e| AssignmentError::Failure(format!("Failed to build assignment URL: {}", e)))?;
    let build_arch = match get_build_arch() {
        Ok(arch) => arch,
        Err(e) => {
            return Err(AssignmentError::Failure(format!(
                "Failed to get build arch: {}",
                e
            )))
        }
    };
    let mut json = serde_json::json!({
        "node": node_name,
        "archs": [build_arch],
        "worker_link": null,
        "codebase": codebase,
        "campaign": campaign,
    });
    if !exclude_hosts.is_empty() {
        json["exclude_hosts"] = serde_json::json!(exclude_hosts);
    }
    json["backchannel"] = if let Some(ref url) = my_url {
        serde_json::json!({
            "kind": "http",
            "url": url.to_string(),
        })
    } else if let Some(url) = jenkins_build_url {
        serde_json::json!({
            "kind": "jenkins",
            "url": url,
        })
    } else {
        serde_json::json![null]
    };
    if let Some(url) = jenkins_build_url.or(my_url) {
        json["worker_link"] = serde_json::Value::String(url.to_string());
    }
    async fn send_assignment_request(
        session: reqwest::Client,
        assign_url: Url,
        credentials: &Credentials,
        json: &serde_json::Value,
    ) -> Result<Response, ReqwestError> {
        let mut builder = session.post(assign_url);
        builder = credentials.set_credentials(builder);
        builder = builder.header("Content-Type", "application/json");
        log::debug!("Sending assignment request: {:?}", json);
        builder = builder.json(json);
        builder.send().await
    }
    let assignment = backoff::future::retry(ExponentialBackoff::default(), || async {
        let session = session.clone();
        let assign_url = assign_url.clone();
        match send_assignment_request(session, assign_url, credentials, &json).await {
            Ok(resp) => {
                if resp.status().as_u16() == StatusCode::CREATED {
                    let data = resp.json::<serde_json::Value>().await.map_err(|e| {
                        backoff::Error::Permanent(AssignmentError::Failure(format!(
                            "Failed to parse assignment response: {}",
                            e
                        )))
                    })?;
                    Ok(data)
                } else if resp.status().as_u16() == 503 {
                    let json = resp.json::<serde_json::Value>().await.map_err(|e| {
                        backoff::Error::Permanent(AssignmentError::Failure(format!(
                            "Failed to parse assignment response: {}",
                            e
                        )))
                    })?;
                    if json["reason"] == "queue empty" {
                        Err(backoff::Error::Permanent(AssignmentError::EmptyQueue))
                    } else {
                        Err(backoff::Error::transient(AssignmentError::Failure(
                            format!("Failed to get assignment: {:?}", json),
                        )))
                    }
                } else if resp.status().is_server_error() {
                    Err(backoff::Error::transient(AssignmentError::Failure(
                        format!("Failed to get assignment: {:?}", resp),
                    )))
                } else {
                    let data = resp.text().await.map_err(|e| {
                        backoff::Error::permanent(AssignmentError::Failure(format!(
                            "Failed to read assignment response: {}",
                            e
                        )))
                    })?;
                    Err(backoff::Error::permanent(AssignmentError::Failure(data)))
                }
            }
            Err(e) => Err(backoff::Error::transient(AssignmentError::Failure(
                e.to_string(),
            ))),
        }
    })
    .await?;
    log::debug!("Got assignment: {:?}", assignment);
    Ok(assignment)
}

/// Get an assignment from the queue.
///
/// # Arguments
/// * `session` - A reqwest session.
/// * `credentials` - Credentials for the server.
/// * `my_url` - The URL of the worker.
/// * `base_url` - The base URL of the server.
/// * `node_name` - The name of the node.
/// * `jenkins_build_url` - The URL of the Jenkins build.
/// * `codebase` - Request an assignment for a specific codebase.
/// * `campaign` - Request an assignment for a specific campaign.
/// * `exclude_hosts` - Hosts this worker has recently been rate limited by;
///   the runner will skip candidates targeting these hosts for this request.
///
/// # Returns
///
/// An assignment.
pub async fn get_assignment(
    session: &reqwest::Client,
    credentials: &Credentials,
    my_url: Option<&Url>,
    base_url: &Url,
    node_name: &str,
    jenkins_build_url: Option<&Url>,
    codebase: Option<&str>,
    campaign: Option<&str>,
    exclude_hosts: &[String],
) -> Result<Assignment, AssignmentError> {
    let assignment = get_assignment_raw(
        session,
        credentials,
        my_url,
        base_url,
        node_name,
        jenkins_build_url,
        codebase,
        campaign,
        exclude_hosts,
    )
    .await?;
    serde_json::from_value(assignment)
        .map_err(|e| AssignmentError::Failure(format!("Failed to parse assignment: {}", e)))
}

pub struct Client {
    client: reqwest::Client,
    base_url: Url,
    credentials: Credentials,
}

pub enum Credentials {
    None,
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: Option<String>,
    },
}

impl Credentials {
    fn set_credentials(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Credentials::None => builder,
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, password.as_ref())
            }
            Credentials::Bearer { token } => builder.bearer_auth(token),
        }
    }

    pub fn from_url(url: &Url) -> Self {
        let mut credentials = Credentials::None;
        if !url.username().is_empty() {
            let password = url.password().map(|p| p.to_string());
            credentials = Credentials::Basic {
                username: url.username().to_string(),
                password,
            };
        }
        credentials
    }

    /// Return a copy of `url` with the worker's Basic-auth credentials
    /// embedded as `userinfo`, so transports that take URLs as their
    /// only auth-bearing parameter (notably breezy/dulwich's HTTP git
    /// push, which doesn't read our `reqwest` client) still talk to
    /// the same authenticated endpoint we use for `/assign` and
    /// `/finish`. For `Credentials::None` or `Credentials::Bearer`
    /// the URL is returned unchanged; bearer-token push isn't a
    /// thing on the wire this targets.
    pub fn embed_in_url(&self, url: &Url) -> Url {
        if let Credentials::Basic { username, password } = self {
            let mut out = url.clone();
            // `set_username`/`set_password` only fail when the URL
            // cannot have a userinfo component (cannot-be-a-base
            // URLs like `data:` or `mailto:`); for the http(s) URLs
            // workers actually push to this is unreachable. Leave
            // the URL untouched if we somehow hit one.
            if out.set_username(username).is_ok() {
                let _ = out.set_password(password.as_deref());
            }
            out
        } else {
            url.clone()
        }
    }
}

impl Client {
    pub fn new(
        base_url: Url,
        credentials: Credentials,
        user_agent: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut builder = reqwest::Client::builder();
        builder = builder.user_agent(user_agent);
        let client = builder
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
        Ok(Self {
            client,
            base_url,
            credentials,
        })
    }

    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    pub async fn get_assignment(
        &self,
        my_url: Option<&Url>,
        node_name: &str,
        jenkins_build_url: Option<&Url>,
        codebase: Option<&str>,
        campaign: Option<&str>,
        exclude_hosts: &[String],
    ) -> Result<Assignment, AssignmentError> {
        get_assignment(
            &self.client,
            &self.credentials,
            my_url,
            &self.base_url,
            node_name,
            jenkins_build_url,
            codebase,
            campaign,
            exclude_hosts,
        )
        .await
    }

    pub async fn get_assignment_raw(
        &self,
        my_url: Option<&Url>,
        node_name: &str,
        jenkins_build_url: Option<&Url>,
        codebase: Option<&str>,
        campaign: Option<&str>,
        exclude_hosts: &[String],
    ) -> Result<serde_json::Value, AssignmentError> {
        get_assignment_raw(
            &self.client,
            &self.credentials,
            my_url,
            &self.base_url,
            node_name,
            jenkins_build_url,
            codebase,
            campaign,
            exclude_hosts,
        )
        .await
    }

    pub async fn upload_results(
        &self,
        run_id: &str,
        metadata: &Metadata,
        output_directory: Option<&Path>,
    ) -> Result<serde_json::Value, UploadFailure> {
        upload_results(
            &self.client,
            &self.credentials,
            &self.base_url,
            run_id,
            metadata,
            output_directory,
        )
        .await
    }
}

/// A multipart form ready to be uploaded, plus the set of filenames it contains.
#[derive(Debug)]
pub struct BundleInfo {
    pub form: Form,
    pub filenames: std::collections::HashSet<String>,
}

/// Build a multipart form containing `metadata` and every file directly under `directory`.
///
/// Files are attached as streaming parts so the whole bundle never has to be held in memory.
pub async fn bundle_results<'a>(
    metadata: &'a Metadata,
    directory: Option<&'a Path>,
) -> Result<BundleInfo, Box<dyn Error + Send + Sync>> {
    let mut form = Form::new();
    let mut filenames = std::collections::HashSet::new();

    let json_part = Part::text(serde_json::to_string(metadata)?)
        .file_name("result.json")
        .mime_str("application/json")?;
    form = form.part("metadata", json_part);

    if let Some(directory) = directory {
        let mut dir = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = dir.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().into_owned();
            filenames.insert(file_name.clone());

            let file = tokio::fs::File::open(entry.path()).await?;
            let file_size = file.metadata().await?.len();
            let buffer_size = if file_size > 1024 * 1024 {
                64 * 1024
            } else {
                8 * 1024
            };

            let stream = tokio_util::io::ReaderStream::with_capacity(file, buffer_size);
            let body = reqwest::Body::wrap_stream(stream);
            let part = Part::stream_with_length(body, file_size)
                .file_name(file_name)
                .mime_str("application/octet-stream")?;
            form = form.part("file", part);
        }
    }

    Ok(BundleInfo { form, filenames })
}

#[derive(Debug)]
pub struct UploadFailure(String);

impl std::fmt::Display for UploadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UploadFailure {}

pub async fn upload_results(
    client: &reqwest::Client,
    credentials: &Credentials,
    base_url: &Url,
    run_id: &str,
    metadata: &Metadata,
    output_directory: Option<&Path>,
) -> Result<serde_json::Value, UploadFailure> {
    backoff::future::retry(ExponentialBackoff::default(), || async {
        let finish_url = base_url
            .join(&format!("active-runs/{}/finish", run_id))
            .map_err(|e| UploadFailure(format!("Error building finish URL: {}", e)))?;
        log::info!("Uploading results to {}", &finish_url);
        let builder = client.post(finish_url).timeout(std::time::Duration::from_secs(60));
        let builder = credentials.set_credentials(builder);
        log::debug!("Uploading results: {}", serde_json::to_string(metadata).unwrap());
        let bundle_info = bundle_results(metadata, output_directory)
            .await
            .map_err(|e| {
                backoff::Error::permanent(UploadFailure(format!("Error creating multipart: {}", e)))
            })?;
        let response = builder.multipart(bundle_info.form).send().await.map_err(|e| {
            backoff::Error::permanent(UploadFailure(format!("Error creating multipart: {}", e)))
        })?;

        match response.status() {
            StatusCode::NOT_FOUND => {
                let resp_json = response.json::<serde_json::Value>().await.map_err(|e| {
                    backoff::Error::permanent(UploadFailure(format!(
                        "Error parsing 404 response: {}",
                        e
                    )))
                })?;
                let reason = resp_json
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("Runner returned 404");
                Err(backoff::Error::permanent(
                    UploadFailure(reason.to_string()),
                ))
            }
            StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE => {
                let status = response.status();
                let text = response.text().await.map_err(|e| {
                    backoff::Error::transient(UploadFailure(format!(
                        "Error reading response text for {}: {}",
                        status,
                        e
                    )))
                })?;
                Err(backoff::Error::transient(UploadFailure(format!(
                    "RetriableResultUploadFailure: Unable to submit result: {}: {}",
                    text, status
                ))))
            }
            StatusCode::OK | StatusCode::CREATED => {
                let json = response.json::<serde_json::Value>().await.map_err(|e| {
                    backoff::Error::permanent(UploadFailure(format!(
                        "Error parsing response: {}",
                        e
                    )))
                })?;
                if output_directory.is_some() {
                    // Use the filenames we already collected during bundle creation
                    let local_filenames = &bundle_info.filenames;

                    let runner_filenames: std::collections::HashSet<_> = json
                        .get("filenames")
                        .and_then(|f| f.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|name| name.as_str())
                                .map(|name| name.to_owned())
                                .collect()
                        })
                        .unwrap_or_else(std::collections::HashSet::new);

                    if local_filenames != &runner_filenames {
                        log::warn!(
                        "Difference between local filenames and runner reported filenames: {:?} != {:?}",
                        local_filenames,
                        runner_filenames
                    );
                    }
                }

                Ok(json)
            }
            _ => {
                let status = response.status();
                let text = response.text().await.map_err(|e| {
                    backoff::Error::permanent(UploadFailure(format!(
                        "Error reading response text for {}: {}",
                        status,
                        e
                    )))
                })?;
                log::warn!("Error uploading results: {}: {}", status, text);
                // 4xx means the runner rejected the request (e.g. 409
                // Conflict when the run was already stored); no point
                // retrying. 5xx and connection errors stay transient.
                if status.is_client_error() {
                    Err(backoff::Error::permanent(UploadFailure(format!(
                        "ResultUploadFailure: terminal {}: {}",
                        status, text
                    ))))
                } else {
                    Err(backoff::Error::transient(UploadFailure(format!(
                        "ResultUploadFailure: Unable to submit result: {}: {}",
                        text, status
                    ))))
                }
            }
        }
    }).await
}

pub async fn abort_run(client: &Client, run_id: &str, metadata: &Metadata, description: &str) {
    let mut metadata: Metadata = metadata.clone();
    metadata.code = Some("aborted".to_string());
    metadata.description = Some(description.to_string());
    metadata.finish_time = Some(chrono::Utc::now());

    match client.upload_results(run_id, &metadata, None).await {
        Ok(_) => {}
        Err(e) => {
            log::warn!("Result upload for abort of {} failed: {}", run_id, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Path as AxumPath, State},
        http::StatusCode as AxumStatus,
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn bundle_results_without_directory_has_only_metadata() {
        let metadata = Metadata::default();
        let bundle = bundle_results(&metadata, None).await.unwrap();
        assert!(bundle.filenames.is_empty());
    }

    #[tokio::test]
    async fn bundle_results_collects_regular_files_and_skips_directories() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.log"), b"aaa")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("b.txt"), b"bbbb")
            .await
            .unwrap();
        tokio::fs::create_dir(tmp.path().join("subdir"))
            .await
            .unwrap();

        let metadata = Metadata::default();
        let bundle = bundle_results(&metadata, Some(tmp.path())).await.unwrap();

        let expected: std::collections::HashSet<String> =
            ["a.log", "b.txt"].iter().map(|s| s.to_string()).collect();
        assert_eq!(bundle.filenames, expected);
    }

    #[derive(Clone)]
    struct MockState {
        status: AxumStatus,
        body: serde_json::Value,
        hits: Arc<AtomicUsize>,
    }

    async fn mock_finish(
        AxumPath(_run_id): AxumPath<String>,
        State(state): State<MockState>,
        _body: axum::body::Bytes,
    ) -> impl IntoResponse {
        state.hits.fetch_add(1, Ordering::SeqCst);
        (state.status, Json(state.body.clone()))
    }

    async fn spawn_mock(state: MockState) -> Url {
        let app = Router::new()
            .route("/active-runs/{run_id}/finish", post(mock_finish))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        format!("http://{}/", addr).parse().unwrap()
    }

    #[tokio::test]
    async fn upload_results_success_returns_parsed_json() {
        let hits = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            status: AxumStatus::OK,
            body: serde_json::json!({"filenames": []}),
            hits: hits.clone(),
        };
        let base_url = spawn_mock(state).await;

        let metadata = Metadata::default();
        let result = upload_results(
            &reqwest::Client::new(),
            &Credentials::None,
            &base_url,
            "run-1",
            &metadata,
            None,
        )
        .await
        .expect("expected success");

        assert_eq!(result, serde_json::json!({"filenames": []}));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn upload_results_4xx_is_permanent_and_not_retried() {
        let hits = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            status: AxumStatus::CONFLICT,
            body: serde_json::json!({"reason": "already-processed"}),
            hits: hits.clone(),
        };
        let base_url = spawn_mock(state).await;

        let metadata = Metadata::default();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            upload_results(
                &reqwest::Client::new(),
                &Credentials::None,
                &base_url,
                "run-2",
                &metadata,
                None,
            ),
        )
        .await
        .expect("should not exceed timeout, i.e. should not retry")
        .expect_err("4xx should surface as an error");

        assert!(
            err.to_string().contains("terminal"),
            "expected terminal error message, got: {}",
            err
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "4xx must not trigger a retry"
        );
    }

    #[tokio::test]
    async fn upload_results_404_is_permanent_with_reason() {
        let hits = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            status: AxumStatus::NOT_FOUND,
            body: serde_json::json!({"reason": "unknown-run"}),
            hits: hits.clone(),
        };
        let base_url = spawn_mock(state).await;

        let metadata = Metadata::default();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            upload_results(
                &reqwest::Client::new(),
                &Credentials::None,
                &base_url,
                "run-3",
                &metadata,
                None,
            ),
        )
        .await
        .expect("should not exceed timeout")
        .expect_err("404 should surface as an error");

        assert_eq!(err.to_string(), "unknown-run");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
