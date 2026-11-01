//! CLI for janitor instance administration.
//!
//! Talks to a running janitor site instance over its public HTTP API.

use clap::{Args, Parser, Subcommand};
use janitor::{Campaign, RunId};
use reqwest::{Client, RequestBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about = "Administer a janitor instance over its HTTP API")]
struct Cli {
    /// Base URL of the janitor site (e.g. https://janitor.debian.net).
    #[arg(long, env = "JANITOR_URL")]
    url: Url,

    /// Username for HTTP basic auth (admin).
    #[arg(long, env = "JANITOR_USER")]
    user: Option<String>,

    /// Password for HTTP basic auth (admin).
    #[arg(long, env = "JANITOR_PASSWORD")]
    password: Option<String>,

    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    logging: janitor::logging::LoggingArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Manage workers.
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Reschedule runs.
    Reschedule(RescheduleArgs),
    /// Reprocess the logs of finished runs.
    ReprocessLogs(ReprocessLogsArgs),
    /// Inspect the queue.
    Queue {
        #[command(subcommand)]
        cmd: QueueCmd,
    },
    /// Operate on runs.
    Run {
        #[command(subcommand)]
        cmd: RunCmd,
    },
    /// Print a log file of a run.
    Logs {
        /// Run ID.
        run_id: RunId,
        /// Log file to print (e.g. worker.log).
        name: String,
    },
}

#[derive(Subcommand)]
enum WorkerCmd {
    /// List registered workers.
    List,
    /// Register a new worker. Prints the generated password.
    Add {
        /// Name of the worker.
        name: String,
        /// Optional link to a status page for the worker.
        #[arg(long)]
        link: Option<String>,
    },
    /// Delete a worker.
    Delete {
        /// Name of the worker.
        name: String,
    },
}

#[derive(Args)]
struct RescheduleArgs {
    /// A single run ID to reschedule. If not set, --result-code triggers a mass reschedule.
    #[arg(long, conflicts_with = "result_code")]
    run_id: Option<RunId>,

    /// Mass-reschedule: rerun everything with this result code.
    #[arg(long)]
    result_code: Option<String>,

    /// Optional description regex filter (mass reschedule only).
    #[arg(long)]
    description_re: Option<String>,

    /// Restrict to this campaign.
    #[arg(long)]
    campaign: Option<Campaign>,

    /// Only reschedule rejected runs (mass reschedule only).
    #[arg(long)]
    rejected: bool,

    /// Only reschedule runs older than N days (mass reschedule only).
    #[arg(long)]
    min_age: Option<u32>,

    /// Schedule offset.
    #[arg(long)]
    offset: Option<f64>,

    /// Force a fresh run.
    #[arg(long)]
    refresh: bool,

    /// Include transient failures (mass reschedule only).
    #[arg(long)]
    include_transient: bool,
}

#[derive(Args)]
struct ReprocessLogsArgs {
    /// Run ID to reprocess, may be repeated. Without it, the filters below select runs.
    #[arg(long = "run-id")]
    run_ids: Vec<RunId>,

    /// Restrict to this campaign.
    #[arg(long)]
    campaign: Option<Campaign>,

    /// Only runs whose description matches this regex.
    #[arg(long)]
    description_re: Option<String>,

    /// Only runs older than N days.
    #[arg(long)]
    min_age: Option<u32>,

    /// Show what would change without storing it.
    #[arg(long)]
    dry_run: bool,

    /// Reschedule runs whose result code changes.
    #[arg(long)]
    reschedule: bool,
}

#[derive(Subcommand)]
enum QueueCmd {
    /// Show items in the queue.
    List {
        /// Maximum items to list.
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum RunCmd {
    /// Kill an active run.
    Kill {
        /// Run ID.
        run_id: RunId,
    },
    /// List active runs.
    List,
    /// Reprocess the build log of a run.
    ReprocessLogs {
        /// Run ID.
        run_id: RunId,
        /// Show what would change without storing it.
        #[arg(long)]
        dry_run: bool,
        /// Reschedule the run if its result code changes.
        #[arg(long)]
        reschedule: bool,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkerInfo {
    name: String,
    link: Option<String>,
    run_count: i64,
}

#[derive(Debug, Deserialize)]
struct AddWorkerResponse {
    name: String,
    link: Option<String>,
    password: String,
}

struct ApiClient {
    http: Client,
    base: Url,
    user: Option<String>,
    password: Option<String>,
}

impl ApiClient {
    fn new(base: Url, user: Option<String>, password: Option<String>) -> reqwest::Result<Self> {
        Ok(Self {
            http: Client::builder().build()?,
            base,
            user,
            password,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Result<RequestBuilder, String> {
        let url = api_url(&self.base, path).map_err(|e| e.to_string())?;
        let mut req = self.http.request(method, url);
        if let Some(user) = &self.user {
            req = req.basic_auth(user, self.password.as_deref());
        }
        Ok(req)
    }
}

fn api_url(base: &Url, path: &str) -> Result<Url, url::ParseError> {
    // Ensure the base ends with '/' so join() treats it as a directory.
    let base = if base.path().ends_with('/') {
        base.clone()
    } else {
        let mut b = base.clone();
        b.set_path(&format!("{}/", base.path()));
        b
    };
    base.join(path.trim_start_matches('/'))
}

async fn expect_success(resp: reqwest::Response) -> Result<reqwest::Response, String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(format!("HTTP {}: {}", status, body.trim()))
}

async fn cmd_worker_list(client: &ApiClient) -> Result<(), String> {
    let resp = client
        .request(reqwest::Method::GET, "cupboard/api/workers")?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let workers: Vec<WorkerInfo> = resp.json().await.map_err(|e| e.to_string())?;
    if workers.is_empty() {
        println!("No workers registered.");
        return Ok(());
    }
    println!("{:<30} {:>10}  LINK", "NAME", "RUNS");
    for w in workers {
        println!(
            "{:<30} {:>10}  {}",
            w.name,
            w.run_count,
            w.link.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

async fn cmd_worker_add(client: &ApiClient, name: &str, link: Option<&str>) -> Result<(), String> {
    let mut form: HashMap<&str, &str> = HashMap::new();
    form.insert("name", name);
    if let Some(link) = link {
        form.insert("link", link);
    }
    let resp = client
        .request(reqwest::Method::POST, "cupboard/api/workers")?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let created: AddWorkerResponse = resp.json().await.map_err(|e| e.to_string())?;
    println!("Created worker {}", created.name);
    if let Some(link) = created.link {
        println!("Link: {}", link);
    }
    println!("Password: {}", created.password);
    println!("(Store this password now; it is not recoverable.)");
    Ok(())
}

async fn cmd_worker_delete(client: &ApiClient, name: &str) -> Result<(), String> {
    let path = format!("cupboard/api/workers/{}", name);
    let resp = client
        .request(reqwest::Method::DELETE, &path)?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Err(format!("no such worker: {}", name));
    }
    expect_success(resp).await?;
    println!("Deleted worker {}", name);
    Ok(())
}

async fn cmd_reschedule(client: &ApiClient, args: &RescheduleArgs) -> Result<(), String> {
    if let Some(run_id) = &args.run_id {
        let path = format!("api/run/{}/reschedule", run_id);
        let mut form: Vec<(&str, String)> = Vec::new();
        form.push(("refresh", if args.refresh { "1" } else { "0" }.to_string()));
        if let Some(offset) = args.offset {
            form.push(("offset", offset.to_string()));
        }
        let resp = client
            .request(reqwest::Method::POST, &path)?
            .form(&form)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let resp = expect_success(resp).await?;
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        println!("{}", serde_json::to_string_pretty(&body).unwrap());
        return Ok(());
    }

    let result_code = args
        .result_code
        .as_ref()
        .ok_or_else(|| "either --run-id or --result-code is required".to_string())?;
    let mut form: Vec<(&str, String)> = Vec::new();
    form.push(("result_code", result_code.clone()));
    if let Some(desc) = &args.description_re {
        form.push(("description_re", desc.clone()));
    }
    if let Some(campaign) = &args.campaign {
        form.push(("campaign", campaign.to_string()));
    }
    if args.rejected {
        form.push(("rejected", "1".to_string()));
    }
    if let Some(min_age) = args.min_age {
        form.push(("min_age", min_age.to_string()));
    }
    if let Some(offset) = args.offset {
        form.push(("offset", offset.to_string()));
    }
    if args.refresh {
        form.push(("refresh", "1".to_string()));
    }
    if args.include_transient {
        form.push(("include_transient", "on".to_string()));
    }
    let resp = client
        .request(reqwest::Method::POST, "cupboard/api/mass-reschedule")?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let scheduled: Vec<Value> = resp.json().await.map_err(|e| e.to_string())?;
    println!("Queued {} run(s) for reschedule.", scheduled.len());
    for entry in scheduled {
        let codebase = entry.get("codebase").and_then(Value::as_str).unwrap_or("?");
        let campaign = entry.get("campaign").and_then(Value::as_str).unwrap_or("?");
        println!("  {} / {}", codebase, campaign);
    }
    Ok(())
}

async fn cmd_reprocess_logs(client: &ApiClient, args: &ReprocessLogsArgs) -> Result<(), String> {
    let mut form: Vec<(&str, String)> = Vec::new();
    for run_id in &args.run_ids {
        form.push(("run_id", run_id.to_string()));
    }
    if let Some(campaign) = &args.campaign {
        form.push(("campaign", campaign.to_string()));
    }
    if let Some(desc) = &args.description_re {
        form.push(("description_re", desc.clone()));
    }
    if let Some(min_age) = args.min_age {
        form.push(("min_age", min_age.to_string()));
    }
    if args.dry_run {
        form.push(("dry_run", "1".to_string()));
    }
    if args.reschedule {
        form.push(("reschedule", "1".to_string()));
    }
    let resp = client
        .request(reqwest::Method::POST, "cupboard/api/reprocess-logs")?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let runs: Vec<Value> = resp.json().await.map_err(|e| e.to_string())?;
    println!("Reprocessing the logs of {} run(s).", runs.len());
    for entry in runs {
        let codebase = entry.get("codebase").and_then(Value::as_str).unwrap_or("?");
        let campaign = entry.get("campaign").and_then(Value::as_str).unwrap_or("?");
        let log_id = entry.get("log_id").and_then(Value::as_str).unwrap_or("?");
        println!("  {} / {} ({})", codebase, campaign, log_id);
    }
    Ok(())
}

async fn cmd_queue_list(client: &ApiClient, limit: u32) -> Result<(), String> {
    let resp = client
        .request(reqwest::Method::GET, "api/queue")?
        .query(&[("limit", limit.to_string())])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_run_kill(client: &ApiClient, run_id: &RunId) -> Result<(), String> {
    let path = format!("api/active-runs/{}/kill", run_id);
    let resp = client
        .request(reqwest::Method::POST, &path)?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status, body.trim()));
    }
    println!("{}", body);
    Ok(())
}

async fn cmd_run_list(client: &ApiClient) -> Result<(), String> {
    let resp = client
        .request(reqwest::Method::GET, "api/active-runs")?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_run_reprocess_logs(
    client: &ApiClient,
    run_id: &RunId,
    dry_run: bool,
    reschedule: bool,
) -> Result<(), String> {
    let path = format!("cupboard/api/run/{}/reprocess-logs", run_id);
    let mut form: Vec<(&str, String)> = Vec::new();
    if dry_run {
        form.push(("dry_run", "1".to_string()));
    }
    if reschedule {
        form.push(("reschedule", "1".to_string()));
    }
    let resp = client
        .request(reqwest::Method::POST, &path)?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_logs(client: &ApiClient, run_id: &RunId, name: &str) -> Result<(), String> {
    let path = format!("api/run/{}/logs/{}", run_id, name);
    let resp = client
        .request(reqwest::Method::GET, &path)?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body = expect_success(resp)
        .await?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    std::io::stdout()
        .write_all(&body)
        .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    cli.logging.init();

    let client = match ApiClient::new(cli.url, cli.user, cli.password) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to build HTTP client: {}", e);
            return ExitCode::from(2);
        }
    };

    let result = match cli.command {
        Command::Worker { cmd } => match cmd {
            WorkerCmd::List => cmd_worker_list(&client).await,
            WorkerCmd::Add { name, link } => cmd_worker_add(&client, &name, link.as_deref()).await,
            WorkerCmd::Delete { name } => cmd_worker_delete(&client, &name).await,
        },
        Command::Reschedule(args) => cmd_reschedule(&client, &args).await,
        Command::ReprocessLogs(args) => cmd_reprocess_logs(&client, &args).await,
        Command::Queue { cmd } => match cmd {
            QueueCmd::List { limit } => cmd_queue_list(&client, limit).await,
        },
        Command::Run { cmd } => match cmd {
            RunCmd::Kill { run_id } => cmd_run_kill(&client, &run_id).await,
            RunCmd::List => cmd_run_list(&client).await,
            RunCmd::ReprocessLogs {
                run_id,
                dry_run,
                reschedule,
            } => cmd_run_reprocess_logs(&client, &run_id, dry_run, reschedule).await,
        },
        Command::Logs { run_id, name } => cmd_logs(&client, &run_id, &name).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", e);
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Posted = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

    async fn form_server() -> (ApiClient, Posted) {
        let posted: Posted = Arc::default();
        let record = posted.clone();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: String| {
            let record = record.clone();
            async move {
                let fields = url::form_urlencoded::parse(body.as_bytes())
                    .into_owned()
                    .collect();
                record
                    .lock()
                    .unwrap()
                    .push((uri.path().to_string(), fields));
                axum::Json(Vec::<Value>::new())
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (ApiClient::new(base, None, None).unwrap(), posted)
    }

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn test_reprocess_logs() {
        let (client, posted) = form_server().await;
        for argv in [
            &[
                "reprocess-logs",
                "--run-id",
                "run-1",
                "--run-id",
                "run-2",
                "--dry-run",
            ][..],
            &[
                "reprocess-logs",
                "--campaign",
                "lintian-fixes",
                "--description-re",
                "^Failed",
                "--min-age",
                "3",
                "--reschedule",
            ][..],
        ] {
            let cli = Cli::try_parse_from(
                ["janitor-admin", "--url", "https://example.com"]
                    .into_iter()
                    .chain(argv.iter().copied()),
            )
            .unwrap();
            let Command::ReprocessLogs(args) = cli.command else {
                panic!("wrong subcommand");
            };
            cmd_reprocess_logs(&client, &args).await.unwrap();
        }
        assert_eq!(
            *posted.lock().unwrap(),
            vec![
                (
                    "/cupboard/api/reprocess-logs".to_string(),
                    fields(&[("run_id", "run-1"), ("run_id", "run-2"), ("dry_run", "1")])
                ),
                (
                    "/cupboard/api/reprocess-logs".to_string(),
                    fields(&[
                        ("campaign", "lintian-fixes"),
                        ("description_re", "^Failed"),
                        ("min_age", "3"),
                        ("reschedule", "1"),
                    ])
                ),
            ]
        );
    }

    #[tokio::test]
    async fn test_reschedule_mass() {
        let (client, posted) = form_server().await;
        let cli = Cli::try_parse_from([
            "janitor-admin",
            "--url",
            "https://example.com",
            "reschedule",
            "--result-code",
            "never-processed",
            "--campaign",
            "lintian-fixes",
            "--refresh",
            "--offset",
            "10",
        ])
        .unwrap();
        let Command::Reschedule(args) = cli.command else {
            panic!("wrong subcommand");
        };
        cmd_reschedule(&client, &args).await.unwrap();
        assert_eq!(
            *posted.lock().unwrap(),
            vec![(
                "/cupboard/api/mass-reschedule".to_string(),
                fields(&[
                    ("result_code", "never-processed"),
                    ("campaign", "lintian-fixes"),
                    ("offset", "10"),
                    ("refresh", "1"),
                ])
            )]
        );
    }

    #[test]
    fn api_url_appends_when_base_has_no_trailing_slash() {
        let base = Url::parse("https://janitor.example/").unwrap();
        assert_eq!(
            api_url(&base, "cupboard/api/workers").unwrap().as_str(),
            "https://janitor.example/cupboard/api/workers"
        );
        let base = Url::parse("https://janitor.example").unwrap();
        assert_eq!(
            api_url(&base, "cupboard/api/workers").unwrap().as_str(),
            "https://janitor.example/cupboard/api/workers"
        );
    }

    #[test]
    fn api_url_preserves_subpath() {
        let base = Url::parse("https://example.com/janitor").unwrap();
        assert_eq!(
            api_url(&base, "api/queue").unwrap().as_str(),
            "https://example.com/janitor/api/queue"
        );
    }

    #[test]
    fn cli_parses_worker_add_with_link() {
        let cli = Cli::try_parse_from([
            "janitor-admin",
            "--url",
            "https://example.com",
            "worker",
            "add",
            "worker-1",
            "--link",
            "https://worker-1.example",
        ])
        .unwrap();
        match cli.command {
            Command::Worker {
                cmd: WorkerCmd::Add { name, link },
            } => {
                assert_eq!(name, "worker-1");
                assert_eq!(link.as_deref(), Some("https://worker-1.example"));
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_url_has_no_default() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        let url = cmd.get_arguments().find(|a| a.get_id() == "url").unwrap();
        assert!(url.is_required_set());
        assert!(url.get_default_values().is_empty());
    }

    #[test]
    fn cli_reschedule_requires_run_or_result_code() {
        // Neither run_id nor result_code is required at parse time; the check happens
        // at runtime. Parsing with no filter should still succeed.
        let cli = Cli::try_parse_from([
            "janitor-admin",
            "--url",
            "https://example.com",
            "reschedule",
        ])
        .unwrap();
        match cli.command {
            Command::Reschedule(args) => {
                assert!(args.run_id.is_none());
                assert!(args.result_code.is_none());
            }
            _ => panic!("wrong subcommand"),
        }
    }
}
