//! POST a `mass-reschedule` request to a janitor cupboard API.
//!
//! Port of the former `reschedule.py` admin script.

use clap::Parser;
use reqwest::Url;

#[derive(Parser, Debug)]
#[command(author, version, about = "Mass-reschedule runs by result code")]
struct Args {
    /// Result code to match (required, positional).
    result_code: String,

    /// Optional regex over run.description.
    description_re: Option<String>,

    /// Force run from scratch.
    #[arg(long)]
    refresh: bool,

    /// Schedule offset.
    #[arg(long, default_value_t = 0)]
    offset: i64,

    /// Process rejected runs only.
    #[arg(long)]
    rejected: bool,

    /// Campaign to process (sent as `suite=…` to the API).
    #[arg(long)]
    campaign: Option<String>,

    /// Only reschedule runs older than N days.
    #[arg(long, default_value_t = 0)]
    min_age: u64,

    /// Instance URL.
    #[arg(long, default_value = "https://janitor.debian.net")]
    base_url: String,
}

/// Build the endpoint URL + query pairs for a mass-reschedule POST.
/// Returns (url, query_pairs). Broken out of `main` so it is testable.
fn build_request(args: &Args) -> Result<(Url, Vec<(&'static str, String)>), String> {
    let mut url = Url::parse(&args.base_url).map_err(|e| format!("invalid base-url: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "base-url cannot be a base URL".to_string())?
        .push("cupboard")
        .push("api")
        .push("mass-reschedule");

    let mut query: Vec<(&'static str, String)> = vec![("result_code", args.result_code.clone())];
    if let Some(c) = &args.campaign {
        query.push(("suite", c.clone()));
    }
    if let Some(re) = &args.description_re {
        query.push(("description_re", re.clone()));
    }
    if args.rejected {
        query.push(("rejected", "1".into()));
    }
    if args.min_age > 0 {
        query.push(("min_age", args.min_age.to_string()));
    }
    if args.refresh {
        query.push(("refresh", "1".into()));
    }
    if args.offset != 0 {
        query.push(("offset", args.offset.to_string()));
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
            result_code: "build-failed".to_string(),
            description_re: None,
            refresh: false,
            offset: 0,
            rejected: false,
            campaign: None,
            min_age: 0,
            base_url: "https://janitor.example.com".to_string(),
        }
    }

    #[test]
    fn build_request_points_at_cupboard_mass_reschedule() {
        let (url, query) = build_request(&minimal_args()).unwrap();
        assert_eq!(
            url.as_str(),
            "https://janitor.example.com/cupboard/api/mass-reschedule"
        );
        // Only the required result_code pair when every other flag is default.
        assert_eq!(query, vec![("result_code", "build-failed".to_string())]);
    }

    #[test]
    fn build_request_maps_campaign_to_suite() {
        let mut args = minimal_args();
        args.campaign = Some("lintian-fixes".to_string());
        let (_, query) = build_request(&args).unwrap();
        // Python sent --campaign as `suite=` so the server-side API
        // didn't need to change during the port.
        assert_eq!(
            query,
            vec![
                ("result_code", "build-failed".to_string()),
                ("suite", "lintian-fixes".to_string()),
            ]
        );
    }

    #[test]
    fn build_request_includes_every_optional_flag_when_set() {
        let args = Args {
            result_code: "rc".into(),
            description_re: Some("timeout.*".into()),
            refresh: true,
            offset: 42,
            rejected: true,
            campaign: Some("c".into()),
            min_age: 7,
            base_url: "https://janitor.example.com".into(),
        };
        let (_, query) = build_request(&args).unwrap();
        assert_eq!(
            query,
            vec![
                ("result_code", "rc".into()),
                ("suite", "c".into()),
                ("description_re", "timeout.*".into()),
                ("rejected", "1".into()),
                ("min_age", "7".into()),
                ("refresh", "1".into()),
                ("offset", "42".into()),
            ]
        );
    }

    #[test]
    fn build_request_skips_zero_min_age_and_offset() {
        // Mirrors the Python guard: `if min_age:` / `if args.offset`.
        // Passing `--min-age 0 --offset 0` (the CLI defaults) should
        // not add those params at all.
        let mut args = minimal_args();
        args.min_age = 0;
        args.offset = 0;
        let (_, query) = build_request(&args).unwrap();
        assert!(query.iter().all(|(k, _)| *k != "min_age" && *k != "offset"));
    }

    #[test]
    fn build_request_rejects_malformed_base_url() {
        let mut args = minimal_args();
        args.base_url = "not a url".into();
        assert!(build_request(&args).is_err());
    }

    #[test]
    fn build_request_rejects_cannot_be_a_base_url() {
        // mailto: parses as a Url but isn't hierarchical, so
        // path_segments_mut() fails — exercise that branch.
        let mut args = minimal_args();
        args.base_url = "mailto:ops@example.com".into();
        assert!(build_request(&args).is_err());
    }
}
