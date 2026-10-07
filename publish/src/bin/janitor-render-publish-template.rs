use clap::Parser;
use janitor_publish::publish_one::{load_template_env, render_proposal_description};
use silver_platter::publish::DescriptionFormat;
use std::path::PathBuf;
use url::Url;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Md,
    Txt,
}

impl From<Format> for DescriptionFormat {
    fn from(format: Format) -> Self {
        match format {
            Format::Md => DescriptionFormat::Markdown,
            Format::Txt => DescriptionFormat::Plain,
        }
    }
}

/// Render the merge proposal description for a run.
#[derive(Parser, Debug)]
struct Args {
    /// Path to load configuration from.
    #[clap(long, default_value = "janitor.conf")]
    config: PathBuf,

    /// Run id to process
    #[clap(short, long)]
    run_id: String,

    /// Role
    #[clap(long, default_value = "main")]
    role: String,

    /// Description format
    #[clap(long, value_enum, default_value = "md")]
    format: Format,

    /// Path to templates
    #[clap(long)]
    template_env_path: Option<PathBuf>,

    /// External URL
    #[clap(long)]
    external_url: Option<Url>,

    #[clap(flatten)]
    logs: janitor::logging::LoggingArgs,
}

async fn render(args: &Args) -> Result<String, String> {
    let templates_dir = match args.template_env_path.clone() {
        Some(path) => path,
        None => {
            let mut path = std::env::current_exe()
                .map_err(|e| format!("Failed to get current executable path: {}", e))?;
            path.pop();
            path.push("proposal-templates");
            path
        }
    };

    let config = janitor::config::read_file(&args.config)
        .map_err(|e| format!("Failed to read config {}: {}", args.config.display(), e))?;
    let db = janitor::state::create_pool(&config)
        .await
        .map_err(|e| format!("Failed to connect to database: {}", e))?;
    let run = janitor_publish::state::get_run(&db, &args.run_id)
        .await
        .map_err(|e| format!("Failed to load run {}: {}", args.run_id, e))?
        .ok_or_else(|| format!("No such run: {}", args.run_id))?;

    let mut template_env = load_template_env(&templates_dir);
    template_env.add_global(
        "external_url",
        args.external_url
            .as_ref()
            .map(|external_url| external_url.to_string().trim_end_matches('/').to_string()),
    );

    let codemod_result = run.result.unwrap_or(serde_json::Value::Null);
    render_proposal_description(
        &template_env,
        &run.suite,
        &run.id,
        &args.role,
        &codemod_result,
        None,
        None,
        args.format.into(),
    )
    .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();

    args.logs.init();

    match render(&args).await {
        Ok(description) => {
            println!("{}", description);
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_args_defaults() {
        let args =
            Args::try_parse_from(["janitor-render-publish-template", "-r", "some-run"]).unwrap();
        assert_eq!(args.config, PathBuf::from("janitor.conf"));
        assert_eq!(args.run_id, "some-run");
        assert_eq!(args.role, "main");
        assert_eq!(args.format, Format::Md);
        assert_eq!(args.template_env_path, None);
        assert_eq!(args.external_url, None);
    }

    #[test]
    fn test_args_all() {
        let args = Args::try_parse_from([
            "janitor-render-publish-template",
            "--config",
            "/etc/janitor.conf",
            "--run-id",
            "some-run",
            "--role",
            "pristine-tar",
            "--format",
            "txt",
            "--template-env-path",
            "/srv/templates",
            "--external-url",
            "https://janitor.example.com/",
        ])
        .unwrap();
        assert_eq!(args.config, PathBuf::from("/etc/janitor.conf"));
        assert_eq!(args.run_id, "some-run");
        assert_eq!(args.role, "pristine-tar");
        assert_eq!(args.format, Format::Txt);
        assert_eq!(
            args.template_env_path,
            Some(PathBuf::from("/srv/templates"))
        );
        assert_eq!(
            args.external_url,
            Some(Url::parse("https://janitor.example.com/").unwrap())
        );
    }

    #[test]
    fn test_args_run_id_required() {
        assert!(Args::try_parse_from(["janitor-render-publish-template"]).is_err());
    }

    #[test]
    fn test_args_invalid_format() {
        assert!(Args::try_parse_from([
            "janitor-render-publish-template",
            "-r",
            "some-run",
            "--format",
            "html"
        ])
        .is_err());
    }

    #[test]
    fn test_format_conversion() {
        assert_eq!(
            DescriptionFormat::from(Format::Md),
            DescriptionFormat::Markdown
        );
        assert_eq!(
            DescriptionFormat::from(Format::Txt),
            DescriptionFormat::Plain
        );
    }
}
