use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug)]
/// Error types for backchannel operations.
pub enum Error {
    /// Timeout while pinging job.
    PingTimeout,

    /// The requested resource was not found.
    NotFound,

    /// Failure in the communication with the intermediary.
    IntermediaryFailure(reqwest::Error),

    /// Failure to ping the job that's not retriable.
    FatalFailure(String),

    /// Worker is unreachable.
    WorkerUnreachable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::PingTimeout => write!(f, "Timeout while pinging job"),
            Error::NotFound => write!(f, "Job not found"),
            Error::IntermediaryFailure(e) => write!(f, "Intermediary failure: {}", e),
            Error::FatalFailure(msg) => write!(f, "Fatal failure: {}", msg),
            Error::WorkerUnreachable(msg) => write!(f, "Worker unreachable: {}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::IntermediaryFailure(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::IntermediaryFailure(e)
    }
}

/// Health status information from a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Whether the worker is alive and responding.
    pub alive: bool,
    /// Current run ID being processed.
    pub current_run_id: Option<String>,
    /// Worker status string (e.g., "healthy", "unhealthy", "failed").
    pub status: String,
    /// Last successful ping timestamp.
    pub last_ping: Option<DateTime<Utc>>,
    /// Worker's reported uptime.
    pub uptime: Option<Duration>,
}

#[async_trait]
/// Interface for communication with the worker processes.
pub trait Backchannel {
    /// Kill the worker process.
    async fn kill(&self) -> Result<(), Error>;

    /// Signal the worker to terminate gracefully.
    async fn terminate(&self, _log_id: &str) -> Result<(), Error> {
        // Default implementation falls back to kill
        self.kill().await
    }
    /// List available log files from the worker.
    async fn list_log_files(&self) -> Result<Vec<String>, Error>;
    /// Get the contents of a specific log file from the worker.
    async fn get_log_file(&self, name: &str) -> Result<Vec<u8>, Error>;
    /// Check if the worker is still alive and processing the expected log.
    async fn ping(&self, log_id: &str) -> Result<(), Error>;

    /// Get detailed health status from the worker.
    async fn get_health_status(&self, expected_log_id: &str) -> Result<HealthStatus, Error> {
        // Default implementation using ping
        match self.ping(expected_log_id).await {
            Ok(()) => Ok(HealthStatus {
                alive: true,
                current_run_id: Some(expected_log_id.to_string()),
                status: "healthy".to_string(),
                last_ping: Some(Utc::now()),
                uptime: None,
            }),
            Err(e) => match e {
                Error::FatalFailure(_) => Ok(HealthStatus {
                    alive: false,
                    current_run_id: None,
                    status: "failed".to_string(),
                    last_ping: Some(Utc::now()),
                    uptime: None,
                }),
                _ => Err(e),
            },
        }
    }
}

/// Backchannel implementation for Jenkins workers.
pub struct JenkinsBackchannel {
    /// URL of the Jenkins instance.
    my_url: url::Url,
}

impl JenkinsBackchannel {
    /// Create a new Jenkins backchannel with the specified URL.
    pub fn new(my_url: url::Url) -> Self {
        Self { my_url }
    }

    async fn get_job(&self, session: reqwest::Client) -> Result<serde_json::Value, reqwest::Error> {
        let url = self
            .my_url
            .join("api/json")
            .expect("static relative \"api/json\" always joins onto a valid base URL");
        log::info!("Fetching Jenkins URL {}", url);
        let resp = session.get(url).send().await?;
        resp.json().await
    }
}

#[async_trait]
impl Backchannel for JenkinsBackchannel {
    async fn kill(&self) -> Result<(), Error> {
        let session = reqwest::Client::new();
        let url = self
            .my_url
            .join("stop")
            .map_err(|_| Error::FatalFailure("Invalid Jenkins URL".to_string()))?;

        log::info!("Stopping Jenkins job at URL {}", url);

        let response = session
            .post(url)
            .send()
            .await
            .map_err(Error::IntermediaryFailure)?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(Error::WorkerUnreachable(format!(
                "Jenkins stop request failed with status: {}",
                response.status()
            )))
        }
    }

    async fn terminate(&self, _log_id: &str) -> Result<(), Error> {
        // For Jenkins, terminate is the same as kill (stop)
        self.kill().await
    }

    async fn list_log_files(&self) -> Result<Vec<String>, Error> {
        Ok(vec!["worker.log".to_string()])
    }

    async fn get_log_file(&self, name: &str) -> Result<Vec<u8>, Error> {
        if name != "worker.log" {
            return Err(Error::NotFound);
        }

        let url = self
            .my_url
            .join("logText/progressiveText")
            .map_err(|e| Error::FatalFailure(format!("Invalid Jenkins URL join: {}", e)))?;
        let resp = reqwest::get(url)
            .await
            .map_err(Error::IntermediaryFailure)?;
        Ok(resp
            .bytes()
            .await
            .map_err(Error::IntermediaryFailure)?
            .to_vec())
    }

    async fn ping(&self, expected_log_id: &str) -> Result<(), Error> {
        let session = reqwest::Client::new();
        let job = match self.get_job(session).await {
            Ok(job) => job,
            Err(e) if e.status() == Some(reqwest::StatusCode::NOT_FOUND) => {
                return Err(Error::NotFound)
            }
            Err(_) => return Err(Error::PingTimeout),
        };
        // If Jenkins has listed the job as having failed, then we can't expect anything to be
        // uploaded
        if job["result"] == "FAILURE" {
            return Err(Error::FatalFailure(format!(
                "Jenkins lists job {} for run {} as failed",
                job["id"], expected_log_id
            )));
        }
        Ok(())
    }

    async fn get_health_status(&self, _expected_log_id: &str) -> Result<HealthStatus, Error> {
        let session = reqwest::Client::new();
        let job = match self.get_job(session).await {
            Ok(job) => job,
            Err(e) if e.status() == Some(reqwest::StatusCode::NOT_FOUND) => {
                return Ok(HealthStatus {
                    alive: false,
                    current_run_id: None,
                    status: "not-found".to_string(),
                    last_ping: Some(Utc::now()),
                    uptime: None,
                })
            }
            Err(_) => return Err(Error::PingTimeout),
        };

        let building = job
            .get("lastBuild")
            .and_then(|b| b.get("building"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);

        let current_run_id = if building {
            job.get("lastBuild")
                .and_then(|b| b.get("number"))
                .and_then(|n| n.as_u64())
                .map(|n| n.to_string())
        } else {
            None
        };

        let status = if let Some(result) = job.get("result").and_then(|r| r.as_str()) {
            match result {
                "SUCCESS" => "completed".to_string(),
                "FAILURE" => "failed".to_string(),
                "ABORTED" => "aborted".to_string(),
                _ => "unknown".to_string(),
            }
        } else if building {
            "building".to_string()
        } else {
            "idle".to_string()
        };

        Ok(HealthStatus {
            alive: true,
            current_run_id,
            status,
            last_ping: Some(Utc::now()),
            uptime: None, // Jenkins doesn't easily provide uptime
        })
    }
}

/// Backchannel implementation that polls a worker via HTTP.
pub struct PollingBackchannel {
    my_url: url::Url,
}

impl PollingBackchannel {
    /// Create a new polling backchannel with the specified URL.
    pub fn new(my_url: url::Url) -> Self {
        Self { my_url }
    }

    async fn get_log_id(&self, session: reqwest::Client) -> Result<String, reqwest::Error> {
        let url = self
            .my_url
            .join("log-id")
            .expect("static relative \"log-id\" always joins onto a valid base URL");
        log::info!("Fetching log ID from URL {}", url);
        let resp = session.get(url).send().await?;
        resp.text().await
    }

    async fn get_status_info(
        &self,
        session: reqwest::Client,
    ) -> Result<serde_json::Value, reqwest::Error> {
        let url = self
            .my_url
            .join("status")
            .expect("static relative \"status\" always joins onto a valid base URL");
        log::info!("Fetching status from URL {}", url);
        let resp = session.get(url).send().await?;
        resp.json().await
    }
}

#[async_trait]
impl Backchannel for PollingBackchannel {
    async fn kill(&self) -> Result<(), Error> {
        let session = reqwest::Client::new();
        let url = self
            .my_url
            .join("kill")
            .map_err(|e| Error::FatalFailure(format!("Invalid URL join: {}", e)))?;

        log::info!("Killing worker at URL {}", url);

        session
            .post(url)
            .send()
            .await
            .map_err(Error::IntermediaryFailure)?;
        Ok(())
    }

    async fn terminate(&self, log_id: &str) -> Result<(), Error> {
        let session = reqwest::Client::new();
        let url = self
            .my_url
            .join("terminate")
            .map_err(|e| Error::FatalFailure(format!("Invalid URL join: {}", e)))?;

        log::info!("Terminating worker at URL {} for log {}", url, log_id);

        let response = session
            .post(url)
            .json(&serde_json::json!({ "log_id": log_id }))
            .send()
            .await
            .map_err(Error::IntermediaryFailure)?;

        if response.status().is_success() {
            Ok(())
        } else {
            // Fall back to kill if terminate is not supported
            self.kill().await
        }
    }

    async fn list_log_files(&self) -> Result<Vec<String>, Error> {
        let session = reqwest::Client::new();
        let url = self
            .my_url
            .join("logs")
            .map_err(|e| Error::FatalFailure(format!("Invalid URL join: {}", e)))?;

        log::info!("Listing log files at URL {}", url);

        // Worker's /logs handler does content negotiation: HTML by
        // default, JSON when `Accept: application/json` is set
        // (`worker/src/web.rs::get_logs`). Without this header the
        // body comes back as HTML and `resp.json()` fails to parse,
        // surfacing here as the empty-log-files state on the
        // active-run page.
        let resp = session
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(Error::IntermediaryFailure)?;
        Ok(resp.json().await.map_err(Error::IntermediaryFailure)?)
    }

    async fn get_log_file(&self, name: &str) -> Result<Vec<u8>, Error> {
        let session = reqwest::Client::new();
        // `Url::join` replaces the last path segment when the base
        // has no trailing slash, so chained joins like
        // `my_url.join("logs")?.join("worker.log")` resolve to
        // `<my_url>/worker.log` (the `logs` segment is overwritten).
        // Use the trailing-slash form so the second join appends.
        let logs_url = self
            .my_url
            .join("logs/")
            .map_err(|e| Error::FatalFailure(format!("Invalid URL join: {}", e)))?;
        let url = logs_url
            .join(name)
            .map_err(|e| Error::FatalFailure(format!("Invalid log file URL join: {}", e)))?;

        log::info!("Fetching log file at URL {}", url);

        let resp = session
            .get(url)
            .send()
            .await
            .map_err(Error::IntermediaryFailure)?;
        Ok(resp
            .bytes()
            .await
            .map_err(Error::IntermediaryFailure)?
            .to_vec())
    }

    async fn ping(&self, expected_log_id: &str) -> Result<(), Error> {
        let session = reqwest::Client::new();

        let log_id = match self.get_log_id(session).await {
            Ok(log_id) => log_id,
            Err(e) => {
                // Connection errors should be IntermediaryFailure, other errors are timeouts
                if e.is_connect() || e.is_timeout() || e.is_request() {
                    return Err(Error::IntermediaryFailure(e));
                } else {
                    return Err(Error::PingTimeout);
                }
            }
        };

        if log_id != expected_log_id {
            return Err(Error::FatalFailure(format!(
                "Worker started processing new run {} rather than {}",
                log_id, expected_log_id
            )));
        }

        Ok(())
    }

    async fn get_health_status(&self, expected_log_id: &str) -> Result<HealthStatus, Error> {
        let session = reqwest::Client::new();

        // Try to get detailed status first
        if let Ok(status_info) = self.get_status_info(session.clone()).await {
            let current_log_id = status_info
                .get("current_run_id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_string());

            let alive = status_info
                .get("alive")
                .and_then(|a| a.as_bool())
                .unwrap_or(false);

            let status = status_info
                .get("status")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());

            let uptime = status_info
                .get("uptime_seconds")
                .and_then(|u| u.as_u64())
                .map(Duration::from_secs);

            let final_status = match status.as_deref() {
                Some("processing") | Some("building") | Some("running") => {
                    if current_log_id.as_ref() == Some(&expected_log_id.to_string()) {
                        "running".to_string()
                    } else {
                        "different-run".to_string()
                    }
                }
                Some(s) => s.to_string(),
                None => {
                    if alive && current_log_id.as_ref() == Some(&expected_log_id.to_string()) {
                        "running".to_string()
                    } else if alive {
                        "different-run".to_string()
                    } else {
                        "unknown".to_string()
                    }
                }
            };

            return Ok(HealthStatus {
                alive,
                current_run_id: current_log_id,
                status: final_status,
                last_ping: Some(Utc::now()),
                uptime,
            });
        }

        // Fall back to basic ping-based health check
        match self.get_log_id(session).await {
            Ok(log_id) => {
                let alive = log_id == expected_log_id;
                Ok(HealthStatus {
                    alive,
                    current_run_id: Some(log_id),
                    status: if alive { "running" } else { "different-run" }.to_string(),
                    last_ping: Some(Utc::now()),
                    uptime: None,
                })
            }
            Err(_) => Ok(HealthStatus {
                alive: false,
                current_run_id: None,
                status: "unreachable".to_string(),
                last_ping: Some(Utc::now()),
                uptime: None,
            }),
        }
    }
}
