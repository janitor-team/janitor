use crate::get_build_arch;
use backoff::ExponentialBackoff;
use janitor::api::worker::{Assignment, Metadata};
use janitor::{Campaign, CodebaseName, RunId};
use reqwest::multipart::{Form, Part};
use reqwest::{Error as ReqwestError, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::Path;

/// How the runner can reach back to the worker.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Backchannel<'a> {
    Http { url: &'a Url },
    Jenkins { url: &'a Url },
}

/// Body of a request for a new assignment.
#[derive(Debug, Serialize)]
struct AssignRequest<'a> {
    node: &'a str,
    archs: Vec<String>,
    worker_link: Option<&'a Url>,
    backchannel: Option<Backchannel<'a>>,
    codebase: Option<&'a CodebaseName>,
    campaign: Option<&'a Campaign>,
}

/// Error body returned by the runner.
#[derive(Debug, Serialize, Deserialize)]
struct ReasonResponse {
    reason: Option<String>,
}

/// Response to uploading the results of a run.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct FinishResponse {
    pub id: RunId,
    /// All files the runner received.
    pub filenames: Vec<String>,
    pub logs: Vec<String>,
    /// Stored artifact names; null when storing the artifacts failed.
    pub artifacts: Option<Vec<String>>,
    /// The stored run result, whose shape is up to the runner.
    pub result: serde_json::Value,
}

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
    codebase: Option<&CodebaseName>,
    campaign: Option<&Campaign>,
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
    let backchannel = my_url
        .map(|url| Backchannel::Http { url })
        .or_else(|| jenkins_build_url.map(|url| Backchannel::Jenkins { url }));
    let request = AssignRequest {
        node: node_name,
        archs: vec![build_arch],
        worker_link: jenkins_build_url.or(my_url),
        backchannel,
        codebase,
        campaign,
    };
    async fn send_assignment_request(
        session: reqwest::Client,
        assign_url: Url,
        credentials: &Credentials,
        request: &AssignRequest<'_>,
    ) -> Result<Response, ReqwestError> {
        let mut builder = session.post(assign_url);
        builder = credentials.set_credentials(builder);
        builder = builder.header("Content-Type", "application/json");
        log::debug!("Sending assignment request: {:?}", request);
        builder = builder.json(request);
        builder.send().await
    }
    let assignment = backoff::future::retry(ExponentialBackoff::default(), || async {
        let session = session.clone();
        let assign_url = assign_url.clone();
        match send_assignment_request(session, assign_url, credentials, &request).await {
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
                    let body = resp.json::<ReasonResponse>().await.map_err(|e| {
                        backoff::Error::Permanent(AssignmentError::Failure(format!(
                            "Failed to parse assignment response: {}",
                            e
                        )))
                    })?;
                    if body.reason.as_deref() == Some("queue empty") {
                        Err(backoff::Error::Permanent(AssignmentError::EmptyQueue))
                    } else {
                        Err(backoff::Error::transient(AssignmentError::Failure(
                            format!("Failed to get assignment: {:?}", body),
                        )))
                    }
                } else if resp.status().is_server_error() {
                    Err(backoff::Error::transient(AssignmentError::Failure(
                        format!("Failed to get assignment: {:?}", resp),
                    )))
                } else {
                    let status = resp.status();
                    let data = resp.text().await.map_err(|e| {
                        backoff::Error::permanent(AssignmentError::Failure(format!(
                            "Failed to read assignment response: {}",
                            e
                        )))
                    })?;
                    Err(backoff::Error::permanent(AssignmentError::Failure(
                        format!("{}: {}", status, data),
                    )))
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
    codebase: Option<&CodebaseName>,
    campaign: Option<&Campaign>,
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

    /// Let Breezy use these credentials for `host`, so that pushes to the
    /// VCS stores behind the runner authenticate. Only Basic credentials
    /// are handed to Breezy.
    pub fn register_with_breezy(&self, host: &str) -> Result<(), Box<breezyshim::error::Error>> {
        let Credentials::Basic { username, password } = self else {
            return Ok(());
        };
        breezyshim::config::credential_store_registry().register_fallback(
            "janitor-worker",
            Box::new(WorkerCredentialStore {
                host: host.to_string(),
                username: username.clone(),
                password: password.clone(),
            }),
        )?;
        Ok(())
    }
}

struct WorkerCredentialStore {
    host: String,
    username: String,
    password: Option<String>,
}

impl breezyshim::config::CredentialStore for WorkerCredentialStore {
    fn get_credentials(
        &self,
        scheme: &str,
        host: &str,
        port: Option<i64>,
        _user: Option<&str>,
        path: Option<&str>,
        realm: Option<&str>,
    ) -> breezyshim::Result<Option<breezyshim::config::Credentials>> {
        if host != self.host {
            return Ok(None);
        }
        Ok(Some(breezyshim::config::Credentials {
            scheme: Some(scheme.to_string()),
            username: Some(self.username.clone()),
            password: self.password.clone(),
            host: Some(host.to_string()),
            port,
            path: path.map(|p| p.to_string()),
            realm: realm.map(|r| r.to_string()),
            verify_certificates: Some(true),
        }))
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
        codebase: Option<&CodebaseName>,
        campaign: Option<&Campaign>,
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
        )
        .await
    }

    pub async fn get_assignment_raw(
        &self,
        my_url: Option<&Url>,
        node_name: &str,
        jenkins_build_url: Option<&Url>,
        codebase: Option<&CodebaseName>,
        campaign: Option<&Campaign>,
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
        )
        .await
    }

    pub async fn upload_results(
        &self,
        run_id: &RunId,
        metadata: &Metadata,
        output_directory: Option<&Path>,
    ) -> Result<FinishResponse, UploadFailure> {
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
    run_id: &RunId,
    metadata: &Metadata,
    output_directory: Option<&Path>,
) -> Result<FinishResponse, UploadFailure> {
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
                let body = response.json::<ReasonResponse>().await.map_err(|e| {
                    backoff::Error::permanent(UploadFailure(format!(
                        "Error parsing 404 response: {}",
                        e
                    )))
                })?;
                let reason = body
                    .reason
                    .unwrap_or_else(|| "Runner returned 404".to_string());
                Err(backoff::Error::permanent(UploadFailure(reason)))
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
                let finish = response.json::<FinishResponse>().await.map_err(|e| {
                    backoff::Error::permanent(UploadFailure(format!(
                        "Error parsing response: {}",
                        e
                    )))
                })?;
                if output_directory.is_some() {
                    // Use the filenames we already collected during bundle creation
                    let local_filenames = &bundle_info.filenames;

                    let runner_filenames: std::collections::HashSet<_> =
                        finish.filenames.iter().cloned().collect();

                    if local_filenames != &runner_filenames {
                        log::warn!(
                        "Difference between local filenames and runner reported filenames: {:?} != {:?}",
                        local_filenames,
                        runner_filenames
                    );
                    }
                }

                Ok(finish)
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

pub async fn abort_run(client: &Client, run_id: &RunId, metadata: &Metadata, description: &str) {
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
        Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn worker_credential_store_only_answers_for_its_host() {
        use breezyshim::config::CredentialStore;
        let store = WorkerCredentialStore {
            host: "runner.example.com".to_string(),
            username: "worker1".to_string(),
            password: Some("secret".to_string()),
        };
        let creds = store
            .get_credentials("https", "runner.example.com", None, None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(creds.username.as_deref(), Some("worker1"));
        assert_eq!(creds.password.as_deref(), Some("secret"));
        assert!(store
            .get_credentials("https", "github.com", None, None, None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn assign_request_wire_format() {
        let url: Url = "http://worker.example.com:8080/".parse().unwrap();
        let request = AssignRequest {
            node: "node1",
            archs: vec!["amd64".to_string()],
            worker_link: Some(&url),
            backchannel: Some(Backchannel::Http { url: &url }),
            codebase: None,
            campaign: None,
        };
        let expected: serde_json::Value = serde_json::from_str(
            r#"{
                "node": "node1",
                "archs": ["amd64"],
                "worker_link": "http://worker.example.com:8080/",
                "backchannel": {"kind": "http", "url": "http://worker.example.com:8080/"},
                "codebase": null,
                "campaign": null
            }"#,
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), expected);

        let request = AssignRequest {
            node: "node1",
            archs: vec![],
            worker_link: None,
            backchannel: None,
            codebase: None,
            campaign: None,
        };
        let expected: serde_json::Value = serde_json::from_str(
            r#"{
                "node": "node1",
                "archs": [],
                "worker_link": null,
                "backchannel": null,
                "codebase": null,
                "campaign": null
            }"#,
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), expected);
    }

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
        body: String,
        hits: Arc<AtomicUsize>,
    }

    async fn mock_finish(
        AxumPath(_run_id): AxumPath<RunId>,
        State(state): State<MockState>,
        _body: axum::body::Bytes,
    ) -> impl IntoResponse {
        state.hits.fetch_add(1, Ordering::SeqCst);
        (
            state.status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            state.body,
        )
    }

    fn reason_body(reason: &str) -> String {
        serde_json::to_string(&ReasonResponse {
            reason: Some(reason.to_string()),
        })
        .unwrap()
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
        let expected = FinishResponse {
            id: RunId::new("run-1").unwrap(),
            filenames: vec![],
            logs: vec![],
            artifacts: None,
            result: serde_json::Value::Null,
        };
        let state = MockState {
            status: AxumStatus::OK,
            body: serde_json::to_string(&expected).unwrap(),
            hits: hits.clone(),
        };
        let base_url = spawn_mock(state).await;

        let metadata = Metadata::default();
        let result = upload_results(
            &reqwest::Client::new(),
            &Credentials::None,
            &base_url,
            &RunId::new("run-1").unwrap(),
            &metadata,
            None,
        )
        .await
        .expect("expected success");

        assert_eq!(result, expected);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn upload_results_4xx_is_permanent_and_not_retried() {
        let hits = Arc::new(AtomicUsize::new(0));
        let state = MockState {
            status: AxumStatus::CONFLICT,
            body: reason_body("already-processed"),
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
                &RunId::new("run-2").unwrap(),
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
            body: reason_body("unknown-run"),
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
                &RunId::new("run-3").unwrap(),
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
