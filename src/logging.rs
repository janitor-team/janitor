#[derive(clap::Args, Debug, Clone)]
#[group()]
pub struct LoggingArgs {
    /// Enable debug mode.
    #[arg(long, default_value_t = false)]
    pub debug: bool,

    /// Use Google cloud logging.
    #[cfg(feature = "gcp")]
    #[arg(long, default_value_t = false)]
    pub gcp_logging: bool,
}

impl LoggingArgs {
    pub fn init(&self) {
        #[cfg(feature = "gcp")]
        let gcp_logging = self.gcp_logging;

        #[cfg(not(feature = "gcp"))]
        let gcp_logging = false;

        init_logging(gcp_logging, self.debug);
    }
}

pub fn init_logging(gcp_logging: bool, debug_mode: bool) {
    #[cfg(feature = "gcp")]
    if gcp_logging {
        stackdriver_logger::init_with_cargo!("../Cargo.toml");
        return;
    }

    #[cfg(not(feature = "gcp"))]
    assert!(!gcp_logging, "GCP logging is not enabled");

    use log::LevelFilter;
    let level = if debug_mode {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    env_logger::builder().filter(None, level).init();
}

#[cfg(all(test, not(feature = "gcp")))]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "GCP logging is not enabled")]
    fn test_init_logging_gcp_without_feature() {
        init_logging(true, false);
    }
}
