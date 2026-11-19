/// DEPRECATED: Use janitor::shared_config::LoggingConfig instead
/// This struct is maintained for backward compatibility only.
/// New services should use the comprehensive logging configuration in shared_config.
#[deprecated(
    since = "0.1.0",
    note = "Use janitor::shared_config::LoggingConfig and init_logging instead"
)]
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
    /// DEPRECATED: Use janitor::shared_config::init_logging_from_simple_args instead
    #[deprecated(
        since = "0.1.0",
        note = "Use janitor::shared_config::init_logging_from_simple_args or init_logging instead"
    )]
    pub fn init(&self) {
        #[cfg(feature = "gcp")]
        let gcp_logging = self.gcp_logging;

        #[cfg(not(feature = "gcp"))]
        let gcp_logging = false;

        init_logging(gcp_logging, self.debug);
    }
}

/// DEPRECATED: Use janitor::shared_config::init_logging_from_simple_args instead
#[deprecated(
    since = "0.1.0",
    note = "Use janitor::shared_config::init_logging_from_simple_args or init_logging instead"
)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logging_args_default() {
        let args = LoggingArgs {
            debug: false,
            #[cfg(feature = "gcp")]
            gcp_logging: false,
        };
        assert!(!args.debug);
        #[cfg(feature = "gcp")]
        assert!(!args.gcp_logging);
    }

    #[test]
    fn test_logging_args_debug() {
        let args = LoggingArgs {
            debug: true,
            #[cfg(feature = "gcp")]
            gcp_logging: false,
        };
        assert!(args.debug);
    }

    #[test]
    #[cfg(feature = "gcp")]
    fn test_logging_args_gcp() {
        let args = LoggingArgs {
            debug: false,
            gcp_logging: true,
        };
        assert!(args.gcp_logging);
    }

    #[test]
    #[should_panic(expected = "GCP logging is not enabled")]
    #[cfg(not(feature = "gcp"))]
    fn test_init_logging_gcp_without_feature() {
        init_logging(true, false);
    }

    #[test]
    fn test_logging_args_clone() {
        let args = LoggingArgs {
            debug: true,
            #[cfg(feature = "gcp")]
            gcp_logging: false,
        };
        let cloned = args.clone();
        assert_eq!(args.debug, cloned.debug);
        #[cfg(feature = "gcp")]
        assert_eq!(args.gcp_logging, cloned.gcp_logging);
    }
}
