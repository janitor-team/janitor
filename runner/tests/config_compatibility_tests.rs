//! Config-loading parity tests. These round-trip a real TOML config
//! file through `RunnerConfig` and confirm the produced structure
//! matches what the runner service actually reads at startup.

use janitor::shared_config::{DatabaseConfig, WebConfig};
use janitor_runner::config::RunnerConfig;

/// A TOML document with `database`, `web`, and `application` sections
/// parses cleanly into `RunnerConfig` and every value round-trips to
/// the expected accessor.
#[test]
fn toml_config_round_trips_into_runner_config() {
    let toml_config = r#"
[database]
url = "postgresql://user:pass@localhost/janitor"
max_connections = 20
connection_timeout_seconds = 30
query_timeout_seconds = 60

[web]
listen_address = "0.0.0.0"
port = 9911
public_port = 9919

[application]
name = "janitor-runner"
version = "1.0.0"
environment = "development"
debug = true
"#;

    let config: RunnerConfig =
        toml::from_str(toml_config).expect("well-formed TOML must parse into RunnerConfig");

    let db = config.database().expect("database section present");
    assert_eq!(db.url, "postgresql://user:pass@localhost/janitor");
    assert_eq!(db.max_connections, 20);
    assert_eq!(db.connection_timeout_seconds, 30);
    assert_eq!(db.query_timeout_seconds, 60);

    let web = config.web().expect("web section present");
    assert_eq!(web.listen_address, "0.0.0.0");
    assert_eq!(web.port, 9911);
    assert_eq!(web.public_port, Some(9919));

    assert_eq!(config.application.name, "janitor-runner");
}

/// `RunnerConfig::default()` should leave database + web unset so
/// the operator has to opt in with a config file or env vars.
/// Individual sub-config defaults are what the shared_config crate
/// documents.
#[test]
fn defaults_are_opt_in() {
    let config = RunnerConfig::default();
    assert!(config.database().is_none());
    assert!(config.web().is_none());

    let db = DatabaseConfig::default();
    assert_eq!(db.max_connections, 10);
    assert_eq!(db.connection_timeout_seconds, 30);
    assert_eq!(db.query_timeout_seconds, 60);

    let web = WebConfig::default();
    assert_eq!(web.listen_address, "localhost");
    assert_eq!(web.port, 8080);
    assert_eq!(web.public_port, None);
    assert_eq!(web.request_timeout_seconds, 30);

    assert_eq!(config.application.environment, "development");
}
