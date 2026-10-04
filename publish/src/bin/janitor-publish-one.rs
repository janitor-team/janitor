use clap::Parser;
use janitor_publish::publish_one::load_template_env;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Path to templates
    #[clap(short, long)]
    template_env_path: Option<PathBuf>,

    #[clap(flatten)]
    logs: janitor::logging::LoggingArgs,
}

fn main() {
    let args = Args::parse();

    let templates_dir = args.template_env_path.unwrap_or_else(|| {
        let mut path = std::env::current_exe().expect("Failed to get current executable path");
        path.pop();
        path.push("proposal-templates");
        path
    });

    args.logs.init();

    // Mirror janitor-publish: init breezy (registers RemoteGitProber so
    // `Branch.open(<git url>)` works) and load plugins (registers the
    // gitlab/github/launchpad forge plugins so `get_forge(branch)`
    // resolves to a real forge instead of returning UnsupportedForge).
    // Without this the subprocess opens the target branch fine via
    // silver_platter's prober list but then fails forge lookup with
    // `Failed(hoster-unsupported): Forge unsupported: salsa.debian.org.`
    breezyshim::init();
    let _ = breezyshim::plugin::load_plugins();

    let request: janitor_publish::PublishOneRequest = serde_json::from_reader(std::io::stdin())
        .unwrap_or_else(|e| {
            eprintln!("Failed to parse JSON request from stdin: {}", e);
            std::process::exit(1);
        });

    let mut template_env = load_template_env(&templates_dir);
    template_env.add_global(
        "external_url",
        request
            .external_url
            .as_ref()
            .map(|external_url| external_url.to_string().trim_end_matches('/').to_string()),
    );

    let publish_result: janitor_publish::PublishOneResult =
        match janitor_publish::publish_one::publish_one(
            template_env,
            &request,
            &mut Some(Vec::new()),
        ) {
            Ok(result) => result,
            Err(e) => {
                if let Err(json_err) = serde_json::to_writer(std::io::stdout(), &e) {
                    eprintln!("Failed to write error response: {}", json_err);
                }
                std::process::exit(1);
            }
        }
        .into();

    if let Err(e) = serde_json::to_writer(std::io::stdout(), &publish_result) {
        eprintln!("Failed to write result to stdout: {}", e);
        std::process::exit(1);
    }
}
