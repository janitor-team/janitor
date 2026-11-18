use clap::Parser;

#[derive(Parser)]
struct Args {
    /// Path to configuration (JSON)
    #[clap(short, long)]
    config: Option<std::path::PathBuf>,
    /// Output directory
    #[clap(short, long)]
    output_directory: std::path::PathBuf,
}

fn main() {
    let args = Args::parse();

    breezyshim::init();

    let (wt, subpath) =
        breezyshim::workingtree::open_containing(std::path::Path::new(".")).unwrap();

    let config: janitor::api::worker::GenericBuildConfig = if let Some(config) = args.config {
        let config = std::fs::read_to_string(config).unwrap();
        serde_json::from_str(&config).unwrap()
    } else {
        Default::default()
    };

    match janitor_worker::generic::build_from_config(
        &wt,
        &subpath,
        &args.output_directory,
        &config,
        &std::env::vars().collect::<std::collections::HashMap<_, _>>(),
    ) {
        Ok(result) => serde_json::to_writer(std::io::stdout(), &result).unwrap(),
        Err(e) => {
            serde_json::to_writer(std::io::stdout(), &serde_json::to_value(e).unwrap()).unwrap();
            std::process::exit(1);
        }
    }
}
