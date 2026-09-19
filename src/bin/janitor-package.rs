//! CLI for the actions a logged-in, non-admin janitor user can take.
//!
//! Talks to a running janitor site instance over its public HTTP API, and
//! covers the maintainer/QA-reviewer side of the web UI: triggering
//! publishes, submitting reviews, rescheduling and inspecting runs, and
//! browsing merge proposals. Admin-gated actions live in `janitor-admin`
//! instead.

use clap::{Parser, Subcommand, ValueEnum};
use janitor::site_client::{expect_success, ApiClient};
use serde_json::Value;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Act as a logged-in janitor user over the site's HTTP API"
)]
struct Cli {
    /// Base URL of the janitor site (e.g. https://janitor.debian.net).
    #[arg(
        long,
        env = "JANITOR_URL",
        default_value = "https://janitor.debian.net"
    )]
    url: reqwest::Url,

    /// Username for HTTP basic auth.
    #[arg(long, env = "JANITOR_USER")]
    user: Option<String>,

    /// Password for HTTP basic auth.
    #[arg(long, env = "JANITOR_PASSWORD")]
    password: Option<String>,

    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    logging: janitor::logging::LoggingArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Trigger and inspect publish attempts.
    Publish {
        #[command(subcommand)]
        cmd: PublishCmd,
    },
    /// Submit QA review verdicts and browse the review queue.
    Review {
        #[command(subcommand)]
        cmd: ReviewCmd,
    },
    /// Reschedule and inspect runs.
    Run {
        #[command(subcommand)]
        cmd: RunCmd,
    },
    /// Browse merge proposals and refresh their status.
    MergeProposals {
        #[command(subcommand)]
        cmd: MergeProposalsCmd,
    },
    /// Show the runner's current status.
    Status,
}

#[derive(Subcommand)]
enum PublishCmd {
    /// Trigger a publish attempt for a codebase.
    Trigger {
        /// Campaign name (e.g. lintian-fixes).
        campaign: String,
        /// Codebase name.
        codebase: String,
        /// Publish mode. Leave unset to use the codebase's configured mode.
        #[arg(long)]
        mode: Option<PublishMode>,
    },
    /// Show details of a single publish attempt.
    Show {
        /// Publish ID.
        publish_id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum PublishMode {
    PushDerived,
    Push,
    Propose,
    AttemptPush,
}

impl PublishMode {
    fn as_str(&self) -> &'static str {
        match self {
            PublishMode::PushDerived => "push-derived",
            PublishMode::Push => "push",
            PublishMode::Propose => "propose",
            PublishMode::AttemptPush => "attempt-push",
        }
    }
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Submit a review verdict for a run.
    Submit {
        /// Run ID being reviewed.
        run_id: String,
        /// Review verdict.
        verdict: Verdict,
        /// Optional free-text comment to attach to the review.
        #[arg(long)]
        comment: Option<String>,
        /// Restrict the regenerated review queue to publishable runs only
        /// (defaults to true, matching the web UI).
        #[arg(long)]
        publishable_only: Option<bool>,
        /// Restrict the regenerated review queue to these campaigns. May be
        /// given more than once. Only affects the (discarded) HTML the
        /// server renders after storing the review, not the verdict itself.
        #[arg(long = "suite")]
        suites: Vec<String>,
    },
    /// List runs waiting for review.
    NeedsReview {
        /// Restrict to a single campaign.
        #[arg(long)]
        campaign: Option<String>,
        /// Reviewer email to check per-reviewer review state for. Defaults
        /// to the authenticated user.
        #[arg(long)]
        reviewer: Option<String>,
        /// Only include runs that are ready to be published (default true).
        #[arg(long)]
        publishable_only: Option<bool>,
        /// Only include runs where a review is required.
        #[arg(long)]
        required_only: Option<bool>,
        /// Maximum number of runs to list.
        #[arg(long, default_value_t = 200)]
        limit: u32,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Verdict {
    Approve,
    Reject,
    Reschedule,
    Abstain,
}

impl Verdict {
    fn as_str(&self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::Reject => "reject",
            Verdict::Reschedule => "reschedule",
            Verdict::Abstain => "abstain",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct NeedsReviewEntry {
    id: String,
    codebase: String,
    campaign: String,
}

#[derive(Subcommand)]
enum RunCmd {
    /// Reschedule a single run, keeping its existing queue bucket.
    Reschedule(RunScheduleArgs),
    /// Reschedule a run via the QA-reviewer queue-jump control endpoint.
    ScheduleControl(RunScheduleArgs),
    /// List currently active (in-progress) runs.
    Active,
    /// Peek at the next run that would be assigned to a worker.
    Peek,
    /// Show a single active run's detail.
    Show {
        /// Run ID.
        run_id: String,
    },
    /// List, or fetch the content of, a run's log files.
    Log {
        /// Run ID.
        run_id: String,
        /// Log filename to fetch (e.g. build.log). If omitted, list the
        /// available log filenames instead.
        filename: Option<String>,
    },
    /// Show the VCS diff produced by a run.
    Diff {
        /// Run ID.
        run_id: String,
        /// Result branch role to diff (defaults to "main").
        #[arg(long)]
        role: Option<String>,
    },
    /// Show the debdiff between a run's build and the last successful,
    /// unchanged build.
    Debdiff {
        /// Run ID.
        run_id: String,
        /// Filter out boring differences (e.g. changelog-only diffs).
        #[arg(long)]
        filter_boring: bool,
    },
    /// Show the diffoscope output between a run's build and the last
    /// successful, unchanged build.
    Diffoscope {
        /// Run ID.
        run_id: String,
        /// Filter out boring differences.
        #[arg(long)]
        filter_boring: bool,
    },
}

#[derive(clap::Args)]
struct RunScheduleArgs {
    /// Run ID.
    run_id: String,
    /// Schedule offset. May be negative to move a run closer to the top of
    /// the queue.
    #[arg(long, allow_hyphen_values = true)]
    offset: Option<f64>,
    /// Force a fresh run instead of reusing cached artifacts.
    #[arg(long)]
    refresh: bool,
}

#[derive(Subcommand)]
enum MergeProposalsCmd {
    /// List merge proposals, optionally restricted to a codebase or campaign.
    List {
        /// Restrict to this codebase.
        #[arg(long, conflicts_with = "campaign")]
        codebase: Option<String>,
        /// Restrict to this campaign.
        #[arg(long)]
        campaign: Option<String>,
    },
    /// Ask the publisher to re-check a merge proposal's status (e.g. after
    /// it was merged or closed out-of-band).
    RefreshStatus {
        /// Merge proposal URL.
        url: String,
    },
}

async fn cmd_publish_trigger(
    client: &ApiClient,
    campaign: &str,
    codebase: &str,
    mode: Option<PublishMode>,
) -> Result<(), String> {
    let path = format!("api/{}/c/{}/publish", campaign, codebase);
    let mut form: Vec<(&str, &str)> = Vec::new();
    if let Some(mode) = mode {
        form.push(("mode", mode.as_str()));
    }
    let resp = client
        .request(reqwest::Method::POST, &path)?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    // The site forwards the publisher's own success/failure JSON as-is, so a
    // non-2xx status here still carries a useful JSON body rather than plain
    // text; print it either way instead of using expect_success.
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    if !status.is_success() {
        return Err(format!("publish request failed: HTTP {}", status));
    }
    Ok(())
}

async fn cmd_publish_show(client: &ApiClient, publish_id: &str) -> Result<(), String> {
    let path = format!("api/publish/{}", publish_id);
    let resp = client
        .request(reqwest::Method::GET, &path)?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_review_submit(
    client: &ApiClient,
    run_id: &str,
    verdict: Verdict,
    comment: Option<&str>,
    publishable_only: Option<bool>,
    suites: &[String],
) -> Result<(), String> {
    let mut form: Vec<(&str, String)> = Vec::new();
    form.push(("run_id", run_id.to_string()));
    form.push(("verdict", verdict.as_str().to_string()));
    if let Some(comment) = comment {
        form.push(("review_comment", comment.to_string()));
    }
    if let Some(publishable_only) = publishable_only {
        form.push(("publishable_only", publishable_only.to_string()));
    }
    for suite in suites {
        form.push(("suite", suite.clone()));
    }
    let resp = client
        .request(reqwest::Method::POST, "cupboard/review")?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    // The endpoint re-renders the (now updated) review queue as HTML rather
    // than returning JSON, so there's nothing structured to print back.
    expect_success(resp).await?;
    println!("Submitted verdict {} for run {}", verdict.as_str(), run_id);
    Ok(())
}

async fn cmd_review_needs_review(
    client: &ApiClient,
    campaign: Option<&str>,
    reviewer: Option<&str>,
    publishable_only: Option<bool>,
    required_only: Option<bool>,
    limit: u32,
) -> Result<(), String> {
    let path = match campaign {
        Some(campaign) => format!("cupboard/api/{}/needs-review", campaign),
        None => "cupboard/api/needs-review".to_string(),
    };
    let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
    if let Some(reviewer) = reviewer {
        query.push(("reviewer", reviewer.to_string()));
    }
    if let Some(publishable_only) = publishable_only {
        query.push(("publishable_only", publishable_only.to_string()));
    }
    if let Some(required_only) = required_only {
        query.push(("required_only", required_only.to_string()));
    }
    let resp = client
        .request(reqwest::Method::GET, &path)?
        .query(&query)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let entries: Vec<NeedsReviewEntry> = resp.json().await.map_err(|e| e.to_string())?;
    if entries.is_empty() {
        println!("No runs waiting for review.");
        return Ok(());
    }
    println!("{:<38} {:<24} RUN ID", "CODEBASE", "CAMPAIGN");
    for entry in entries {
        println!("{:<38} {:<24} {}", entry.codebase, entry.campaign, entry.id);
    }
    Ok(())
}

/// Shared implementation of `run reschedule` and `run schedule-control`:
/// both post the same `offset`/`refresh` form to a run-scoped path and get
/// back a scheduling result.
async fn cmd_run_schedule(
    client: &ApiClient,
    path: &str,
    offset: Option<f64>,
    refresh: bool,
) -> Result<(), String> {
    let mut form: Vec<(&str, String)> = vec![("refresh", if refresh { "1" } else { "0" }.to_string())];
    if let Some(offset) = offset {
        form.push(("offset", offset.to_string()));
    }
    let resp = client
        .request(reqwest::Method::POST, path)?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_run_active(client: &ApiClient) -> Result<(), String> {
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

async fn cmd_run_peek(client: &ApiClient) -> Result<(), String> {
    let resp = client
        .request(reqwest::Method::GET, "api/active-runs/+peek")?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_run_show(client: &ApiClient, run_id: &str) -> Result<(), String> {
    let path = format!("api/active-runs/{}", run_id);
    let resp = client
        .request(reqwest::Method::GET, &path)?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_run_log(
    client: &ApiClient,
    run_id: &str,
    filename: Option<&str>,
) -> Result<(), String> {
    match filename {
        None => {
            let path = format!("api/active-runs/{}/log/", run_id);
            let resp = client
                .request(reqwest::Method::GET, &path)?
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let resp = expect_success(resp).await?;
            let filenames: Vec<String> = resp.json().await.map_err(|e| e.to_string())?;
            if filenames.is_empty() {
                println!("No log files for run {}.", run_id);
            }
            for filename in filenames {
                println!("{}", filename);
            }
            Ok(())
        }
        Some(filename) => {
            let path = format!("api/active-runs/{}/log/{}", run_id, filename);
            let resp = client
                .request(reqwest::Method::GET, &path)?
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let resp = expect_success(resp).await?;
            let body = resp.text().await.map_err(|e| e.to_string())?;
            print!("{}", body);
            Ok(())
        }
    }
}

async fn cmd_run_diff(client: &ApiClient, run_id: &str, role: Option<&str>) -> Result<(), String> {
    let path = match role {
        Some(role) => format!("api/run/{}/diff/{}", run_id, role),
        None => format!("api/run/{}/diff", run_id),
    };
    let resp = client
        .request(reqwest::Method::GET, &path)?
        .header(reqwest::header::ACCEPT, "text/x-diff")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body = resp.text().await.map_err(|e| e.to_string())?;
    print!("{}", body);
    Ok(())
}

async fn cmd_run_archive_diff(
    client: &ApiClient,
    run_id: &str,
    kind: &str,
    filter_boring: bool,
) -> Result<(), String> {
    let path = format!("api/run/{}/{}", run_id, kind);
    let mut req = client.request(reqwest::Method::GET, &path)?;
    if filter_boring {
        req = req.query(&[("filter_boring", "1")]);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body = resp.text().await.map_err(|e| e.to_string())?;
    print!("{}", body);
    Ok(())
}

async fn cmd_merge_proposals_list(
    client: &ApiClient,
    codebase: Option<&str>,
    campaign: Option<&str>,
) -> Result<(), String> {
    let path = match (codebase, campaign) {
        (Some(codebase), _) => format!("api/c/{}/merge-proposals", codebase),
        (None, Some(campaign)) => format!("api/{}/merge-proposals", campaign),
        (None, None) => "api/merge-proposals".to_string(),
    };
    let resp = client
        .request(reqwest::Method::GET, &path)?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
}

async fn cmd_merge_proposals_refresh_status(client: &ApiClient, url: &str) -> Result<(), String> {
    let form = [("url", url)];
    let resp = client
        .request(reqwest::Method::POST, "api/refresh-proposal-status")?
        .form(&form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body = resp.text().await.map_err(|e| e.to_string())?;
    println!("{}", body.trim());
    Ok(())
}

async fn cmd_status(client: &ApiClient) -> Result<(), String> {
    let resp = client
        .request(reqwest::Method::GET, "api/runner/status")?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let resp = expect_success(resp).await?;
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
    Ok(())
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
        Command::Publish { cmd } => match cmd {
            PublishCmd::Trigger {
                campaign,
                codebase,
                mode,
            } => cmd_publish_trigger(&client, &campaign, &codebase, mode).await,
            PublishCmd::Show { publish_id } => cmd_publish_show(&client, &publish_id).await,
        },
        Command::Review { cmd } => match cmd {
            ReviewCmd::Submit {
                run_id,
                verdict,
                comment,
                publishable_only,
                suites,
            } => {
                cmd_review_submit(
                    &client,
                    &run_id,
                    verdict,
                    comment.as_deref(),
                    publishable_only,
                    &suites,
                )
                .await
            }
            ReviewCmd::NeedsReview {
                campaign,
                reviewer,
                publishable_only,
                required_only,
                limit,
            } => {
                cmd_review_needs_review(
                    &client,
                    campaign.as_deref(),
                    reviewer.as_deref(),
                    publishable_only,
                    required_only,
                    limit,
                )
                .await
            }
        },
        Command::Run { cmd } => match cmd {
            RunCmd::Reschedule(RunScheduleArgs {
                run_id,
                offset,
                refresh,
            }) => {
                let path = format!("api/run/{}/reschedule", run_id);
                cmd_run_schedule(&client, &path, offset, refresh).await
            }
            RunCmd::ScheduleControl(RunScheduleArgs {
                run_id,
                offset,
                refresh,
            }) => {
                let path = format!("api/run/{}/schedule-control", run_id);
                cmd_run_schedule(&client, &path, offset, refresh).await
            }
            RunCmd::Active => cmd_run_active(&client).await,
            RunCmd::Peek => cmd_run_peek(&client).await,
            RunCmd::Show { run_id } => cmd_run_show(&client, &run_id).await,
            RunCmd::Log { run_id, filename } => {
                cmd_run_log(&client, &run_id, filename.as_deref()).await
            }
            RunCmd::Diff { run_id, role } => {
                cmd_run_diff(&client, &run_id, role.as_deref()).await
            }
            RunCmd::Debdiff {
                run_id,
                filter_boring,
            } => cmd_run_archive_diff(&client, &run_id, "debdiff", filter_boring).await,
            RunCmd::Diffoscope {
                run_id,
                filter_boring,
            } => cmd_run_archive_diff(&client, &run_id, "diffoscope", filter_boring).await,
        },
        Command::MergeProposals { cmd } => match cmd {
            MergeProposalsCmd::List { codebase, campaign } => {
                cmd_merge_proposals_list(&client, codebase.as_deref(), campaign.as_deref()).await
            }
            MergeProposalsCmd::RefreshStatus { url } => {
                cmd_merge_proposals_refresh_status(&client, &url).await
            }
        },
        Command::Status => cmd_status(&client).await,
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

    #[test]
    fn cli_parses_publish_trigger_with_mode() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "--url",
            "https://example.com",
            "publish",
            "trigger",
            "lintian-fixes",
            "mypkg",
            "--mode",
            "propose",
        ])
        .unwrap();
        match cli.command {
            Command::Publish {
                cmd: PublishCmd::Trigger {
                    campaign,
                    codebase,
                    mode,
                },
            } => {
                assert_eq!(campaign, "lintian-fixes");
                assert_eq!(codebase, "mypkg");
                assert_eq!(mode.unwrap().as_str(), "propose");
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_publish_trigger_with_multiword_mode() {
        // clap's derived ValueEnum parsing is kebab-case, so the two-word
        // enum variant must be spelled with a hyphen on the command line.
        let cli = Cli::try_parse_from([
            "janitor-package",
            "publish",
            "trigger",
            "lintian-fixes",
            "mypkg",
            "--mode",
            "attempt-push",
        ])
        .unwrap();
        match cli.command {
            Command::Publish {
                cmd: PublishCmd::Trigger { mode, .. },
            } => {
                assert_eq!(mode.unwrap().as_str(), "attempt-push");
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_publish_trigger_without_mode() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "publish",
            "trigger",
            "lintian-fixes",
            "mypkg",
        ])
        .unwrap();
        match cli.command {
            Command::Publish {
                cmd: PublishCmd::Trigger { mode, .. },
            } => {
                assert!(mode.is_none());
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_publish_show() {
        let cli = Cli::try_parse_from(["janitor-package", "publish", "show", "1234"]).unwrap();
        match cli.command {
            Command::Publish {
                cmd: PublishCmd::Show { publish_id },
            } => {
                assert_eq!(publish_id, "1234");
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_review_submit_with_comment_and_suites() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "review",
            "submit",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
            "reschedule",
            "--comment",
            "flaky build, please retry",
            "--suite",
            "lintian-fixes",
            "--suite",
            "multiarch-hints",
        ])
        .unwrap();
        match cli.command {
            Command::Review {
                cmd:
                    ReviewCmd::Submit {
                        run_id,
                        verdict,
                        comment,
                        publishable_only,
                        suites,
                    },
            } => {
                assert_eq!(run_id, "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab");
                assert_eq!(verdict.as_str(), "reschedule");
                assert_eq!(comment.as_deref(), Some("flaky build, please retry"));
                assert!(publishable_only.is_none());
                assert_eq!(suites, vec!["lintian-fixes", "multiarch-hints"]);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_review_needs_review_defaults() {
        let cli = Cli::try_parse_from(["janitor-package", "review", "needs-review"]).unwrap();
        match cli.command {
            Command::Review {
                cmd:
                    ReviewCmd::NeedsReview {
                        campaign,
                        reviewer,
                        publishable_only,
                        required_only,
                        limit,
                    },
            } => {
                assert!(campaign.is_none());
                assert!(reviewer.is_none());
                assert!(publishable_only.is_none());
                assert!(required_only.is_none());
                assert_eq!(limit, 200);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_review_needs_review_with_filters() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "review",
            "needs-review",
            "--campaign",
            "lintian-fixes",
            "--publishable-only",
            "false",
            "--required-only",
            "true",
            "--limit",
            "10",
        ])
        .unwrap();
        match cli.command {
            Command::Review {
                cmd:
                    ReviewCmd::NeedsReview {
                        campaign,
                        publishable_only,
                        required_only,
                        limit,
                        ..
                    },
            } => {
                assert_eq!(campaign.as_deref(), Some("lintian-fixes"));
                assert_eq!(publishable_only, Some(false));
                assert_eq!(required_only, Some(true));
                assert_eq!(limit, 10);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_run_reschedule_with_offset_and_refresh() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "reschedule",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
            "--offset",
            "-5",
            "--refresh",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd:
                    RunCmd::Reschedule(RunScheduleArgs {
                        run_id,
                        offset,
                        refresh,
                    }),
            } => {
                assert_eq!(run_id, "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab");
                assert_eq!(offset, Some(-5.0));
                assert!(refresh);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_run_schedule_control() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "schedule-control",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd: RunCmd::ScheduleControl(args),
            } => {
                assert_eq!(args.run_id, "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab");
                assert!(args.offset.is_none());
                assert!(!args.refresh);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_run_log_with_and_without_filename() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "log",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd: RunCmd::Log { run_id, filename },
            } => {
                assert_eq!(run_id, "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab");
                assert!(filename.is_none());
            }
            _ => panic!("wrong subcommand"),
        }

        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "log",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
            "build.log",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd: RunCmd::Log { filename, .. },
            } => {
                assert_eq!(filename.as_deref(), Some("build.log"));
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_run_diff_with_role() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "diff",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
            "--role",
            "upstream",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd: RunCmd::Diff { run_id, role },
            } => {
                assert_eq!(run_id, "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab");
                assert_eq!(role.as_deref(), Some("upstream"));
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_run_debdiff_and_diffoscope_with_filter_boring() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "debdiff",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
            "--filter-boring",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd: RunCmd::Debdiff {
                    run_id,
                    filter_boring,
                },
            } => {
                assert_eq!(run_id, "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab");
                assert!(filter_boring);
            }
            _ => panic!("wrong subcommand"),
        }

        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "diffoscope",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd: RunCmd::Diffoscope { filter_boring, .. },
            } => {
                assert!(!filter_boring);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_run_active_peek_and_show() {
        let cli = Cli::try_parse_from(["janitor-package", "run", "active"]).unwrap();
        assert!(matches!(cli.command, Command::Run { cmd: RunCmd::Active }));

        let cli = Cli::try_parse_from(["janitor-package", "run", "peek"]).unwrap();
        assert!(matches!(cli.command, Command::Run { cmd: RunCmd::Peek }));

        let cli = Cli::try_parse_from([
            "janitor-package",
            "run",
            "show",
            "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab",
        ])
        .unwrap();
        match cli.command {
            Command::Run {
                cmd: RunCmd::Show { run_id },
            } => {
                assert_eq!(run_id, "a1b2c3d4-5f6a-4b3c-9d8e-1234567890ab");
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_merge_proposals_list_with_codebase() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "merge-proposals",
            "list",
            "--codebase",
            "mypkg",
        ])
        .unwrap();
        match cli.command {
            Command::MergeProposals {
                cmd: MergeProposalsCmd::List { codebase, campaign },
            } => {
                assert_eq!(codebase.as_deref(), Some("mypkg"));
                assert!(campaign.is_none());
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_rejects_merge_proposals_list_with_both_codebase_and_campaign() {
        let result = Cli::try_parse_from([
            "janitor-package",
            "merge-proposals",
            "list",
            "--codebase",
            "mypkg",
            "--campaign",
            "lintian-fixes",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_parses_merge_proposals_refresh_status() {
        let cli = Cli::try_parse_from([
            "janitor-package",
            "merge-proposals",
            "refresh-status",
            "https://github.com/example/mypkg/pull/1",
        ])
        .unwrap();
        match cli.command {
            Command::MergeProposals {
                cmd: MergeProposalsCmd::RefreshStatus { url },
            } => {
                assert_eq!(url, "https://github.com/example/mypkg/pull/1");
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn cli_parses_status() {
        let cli = Cli::try_parse_from(["janitor-package", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status));
    }
}
