use std::fs::File;

use log::debug;
use std::process;

use clap::Parser;
use log::{error, info};
use reqwest::blocking::Client;
use reqwest::Url;

#[derive(Parser)]
struct Args {
    /// URL of the site's refresh route. A user:password in it is sent as HTTP basic auth.
    #[clap(
        short,
        long,
        default_value = "https://janitor.debian.net/api/refresh-proposal-status"
    )]
    refresh_url: String,
    #[clap(short, long, default_value = "/dev/stdin")]
    input: String,
}

/// `api_url` without its userinfo and query, to keep credentials out of the log.
fn redact(api_url: &str) -> String {
    match Url::parse(api_url) {
        Ok(mut url) if url.has_host() => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        _ => "<invalid URL>".to_string(),
    }
}

fn refresh_merge_proposal(api_url: &str, merge_proposal_url: &str) -> Result<(), String> {
    let client = Client::new();
    let res = client
        .post(api_url)
        .form(&[("url", merge_proposal_url)])
        .send()
        .map_err(|e| {
            format!(
                "error triggering refresh for {}: {}",
                redact(api_url),
                e.without_url()
            )
        })?;

    match res.status().as_u16() {
        200 | 202 => Ok(()),
        status => Err(format!(
            "error {} triggering refresh for {}",
            status,
            redact(api_url)
        )),
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    let f = File::open(args.input).unwrap();

    match janitor_mail_filter::parse_email(f) {
        Some(merge_proposal_url) => {
            info!("Found merge proposal URL: {}", merge_proposal_url);
            match refresh_merge_proposal(&args.refresh_url, &merge_proposal_url) {
                Ok(()) => process::exit(0),
                Err(e) => {
                    error!("Error: {}", e);
                    process::exit(1);
                }
            }
        }
        None => {
            debug!("No merge proposal URL found.");
            process::exit(0);
        }
    }
}
