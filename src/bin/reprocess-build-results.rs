//! Trigger reprocessing of build results via the cupboard
//! `mass-reschedule` API.
//!
//! Port of the former `reprocess-build-results.py` admin script.
//! Mirrors the Python behaviour: it POSTs to the same endpoint as
//! reschedule.rs but with the `dry_run`, `reschedule`, and
//! `run_ids` parameters used by the reprocess flow.

use clap::Parser;
use reqwest::Url;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Reprocess build results via the cupboard mass-reschedule API"
)]
struct Args {
    /// Default timeout when retrieving log files (seconds).
    /// Currently unused — kept for CLI compatibility with the
    /// previous Python script.
    #[arg(long, default_value_t = 60)]
    log_timeout: u64,

    /// Run id(s) to process. May be repeated.
    #[arg(short = 'r', long = "run-id")]
    run_id: Vec<String>,

    /// Schedule rebuilds for runs whose result code has changed.
    #[arg(long)]
    reschedule: bool,

    #[arg(long)]
    dry_run: bool,

    /// Instance URL.
    #[arg(long, default_value = "https://janitor.debian.net")]
    base_url: String,
}

/// Build the endpoint URL + query pairs for a reprocess-build POST.
/// Broken out of `main` so it is testable.
fn build_request(args: &Args) -> Result<(Url, Vec<(&'static str, String)>), String> {
    let mut url = Url::parse(&args.base_url).map_err(|e| format!("invalid base-url: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "base-url cannot be a base URL".to_string())?
        .push("cupboard")
        .push("api")
        .push("mass-reschedule");

    let mut query: Vec<(&'static str, String)> = Vec::new();
    if args.dry_run {
        query.push(("dry_run", "1".into()));
    }
    if args.reschedule {
        query.push(("reschedule", "1".into()));
    }
    for id in &args.run_id {
        query.push(("run_ids", id.clone()));
    }
    Ok((url, query))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let (url, query) = build_request(&args)?;
    let resp = reqwest::Client::new().post(url).query(&query).send().await?;
    let status = resp.status();
    if !status.is_success() {
        log::error!("rescheduling failed: {}", status);
        std::process::exit(1);
    }
    let entries: serde_json::Value = resp.json().await?;
    if let Some(arr) = entries.as_array() {
        for entry in arr {
            log::info!("{:?}", entry);
        }
    } else {
        log::info!("{:?}", entries);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_args() -> Args {
        Args {
            log_timeout: 60,
            run_id: Vec::new(),
            reschedule: false,
            dry_run: false,
            base_url: "https://janitor.example.com".to_string(),
        }
    }

    #[test]
    fn build_request_no_flags_yields_empty_query() {
        let (url, query) = build_request(&minimal_args()).unwrap();
        assert_eq!(
            url.as_str(),
            "https://janitor.example.com/cupboard/api/mass-reschedule"
        );
        assert_eq!(query, Vec::<(&'static str, String)>::new());
    }

    #[test]
    fn build_request_emits_one_run_ids_pair_per_id() {
        // The Python script passed run_id as a list, and aiohttp
        // serialised it as repeated `run_ids=` pairs — preserve that
        // so the server-side parsing doesn't change.
        let mut args = minimal_args();
        args.run_id = vec!["run-a".into(), "run-b".into(), "run-c".into()];
        let (_, query) = build_request(&args).unwrap();
        assert_eq!(
            query,
            vec![
                ("run_ids", "run-a".to_string()),
                ("run_ids", "run-b".to_string()),
                ("run_ids", "run-c".to_string()),
            ]
        );
    }

    #[test]
    fn build_request_sets_dry_run_and_reschedule_flags() {
        let mut args = minimal_args();
        args.dry_run = true;
        args.reschedule = true;
        args.run_id = vec!["id-1".into()];
        let (_, query) = build_request(&args).unwrap();
        assert_eq!(
            query,
            vec![
                ("dry_run", "1".into()),
                ("reschedule", "1".into()),
                ("run_ids", "id-1".into()),
            ]
        );
    }

    #[test]
    fn build_request_rejects_malformed_base_url() {
        let mut args = minimal_args();
        args.base_url = "not a url".into();
        assert!(build_request(&args).is_err());
    }
}
