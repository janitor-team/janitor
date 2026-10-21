use std::fs::File;

use log::debug;
use std::process;

use clap::Parser;
use log::{error, info};
use reqwest::blocking::Client;

#[derive(Parser)]
struct Args {
    #[clap(
        short,
        long,
        default_value = "https://janitor.debian.net/api/refresh-proposal-status"
    )]
    refresh_url: String,
    #[clap(short, long, default_value = "/dev/stdin")]
    input: String,
}

fn refresh_merge_proposal(api_url: &str, merge_proposal_url: &str) -> Result<(), String> {
    let client = Client::new();
    let res = client
        .post(api_url)
        .json(&serde_json::json!({"url": merge_proposal_url}))
        .send()
        .map_err(|e| e.to_string())?;

    match res.status().as_u16() {
        200 | 202 => Ok(()),
        status => Err(format!(
            "error {} triggering refresh for {}",
            status, api_url
        )),
    }
}

fn main() {
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
