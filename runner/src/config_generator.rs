use crate::QueueItem;
use async_trait::async_trait;
use breezyshim::branch::GenericBranch;
use debversion::Version;
use janitor::config::{Campaign, Distribution};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;

/// Python treats unset and empty proto strings alike; so do we.
fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

#[async_trait]
/// Result type for configuration generators.
pub trait ConfigGeneratorResult: Serialize + Deserialize<'static> {
    /// Load artifacts from the specified path.
    fn load_artifacts(&mut self, path: &Path) -> Result<(), Error>;

    /// Store the results in the database for the specified run.
    async fn store(&self, conn: &PgPool, run_id: &str) -> Result<(), sqlx::Error>;

    /// Get the list of artifact filenames produced.
    fn artifact_filenames(&self) -> Vec<String>;
}

#[derive(Debug)]
/// Errors that can occur during configuration generation.
pub enum Error {
    /// Database error.
    Sqlx(sqlx::Error),
    /// Required artifacts are missing.
    ArtifactsMissing,
    /// Error in the configuration.
    ConfigError(String),
    /// Other errors.
    Other(String),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Sqlx(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Sqlx(e) => write!(f, "SQLx error: {}", e),
            Error::ArtifactsMissing => write!(f, "Artifacts missing"),
            Error::ConfigError(e) => write!(f, "Configuration error: {}", e),
            Error::Other(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for Error {}

#[async_trait]
/// Interface for generating configurations for worker runs.
pub trait ConfigGenerator: Send + Sync {
    /// Kind of build, sent to the worker as `build.target`.
    fn kind(&self) -> &'static str;

    /// Generate a configuration for a worker run.
    async fn config(
        &self,
        conn: &PgPool,
        campaign_config: &Campaign,
        queue_item: &QueueItem,
    ) -> Result<serde_json::Value, Error>;

    /// Generate environment variables for a worker run.
    async fn build_env(
        &self,
        conn: &PgPool,
        campaign_config: &Campaign,
        queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, Error>;

    /// Get additional branches that should be colocated with the main branch.
    fn additional_colocated_branches(&self, main_branch: &GenericBranch)
        -> HashMap<String, String>;
}

#[derive(Debug, Serialize, Deserialize)]
/// Result type for generic build configurations.
pub struct GenericResult;

#[async_trait]
impl ConfigGeneratorResult for GenericResult {
    fn load_artifacts(&mut self, _path: &Path) -> Result<(), Error> {
        Ok(())
    }

    async fn store(&self, _conn: &PgPool, _run_id: &str) -> Result<(), sqlx::Error> {
        Ok(())
    }

    fn artifact_filenames(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Configuration generator for generic builds.
pub struct GenericConfigGenerator {
    dep_server_url: Option<String>,
}

impl GenericConfigGenerator {
    /// Create a new generic configuration generator.
    pub fn new(dep_server_url: Option<String>) -> Self {
        Self { dep_server_url }
    }
}

#[async_trait]
impl ConfigGenerator for GenericConfigGenerator {
    fn kind(&self) -> &'static str {
        "generic"
    }

    async fn config(
        &self,
        _conn: &PgPool,
        campaign_config: &Campaign,
        _queue_item: &QueueItem,
    ) -> Result<Value, Error> {
        let mut config = Map::new();
        if let Some(chroot) = non_empty(&campaign_config.generic_build().chroot) {
            config.insert("chroot".to_string(), json!(chroot));
        }
        config.insert("dep_server_url".to_string(), json!(self.dep_server_url));
        Ok(Value::Object(config))
    }

    async fn build_env(
        &self,
        _conn: &PgPool,
        _campaign_config: &Campaign,
        _queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, Error> {
        Ok(HashMap::new())
    }

    fn additional_colocated_branches(
        &self,
        _main_branch: &GenericBranch,
    ) -> HashMap<String, String> {
        HashMap::new()
    }
}

#[derive(Debug, Serialize, Deserialize)]
/// Result type for Debian build configurations.
pub struct DebianResult {
    source: String,
    build_version: Version,
    build_distribution: String,
    changes_filenames: Vec<String>,
    lintian_result: Option<String>,
    binary_packages: Vec<String>,
    output_directory: Option<std::path::PathBuf>,
}

#[async_trait]
impl ConfigGeneratorResult for DebianResult {
    fn load_artifacts(&mut self, path: &Path) -> Result<(), Error> {
        let summary = match crate::find_changes(path) {
            Ok(summary) => {
                log::info!(
                        "Found changes files {:?}, source {}, build version {}, distribution: {}, binary packages: {:?}",
                        summary.names,
                        summary.source,
                        summary.version,
                        summary.distribution,
                        summary.binary_packages,
                    );

                summary
            }
            Err(e) => {
                log::info!("No changes file found: {}", e);
                return Err(Error::ArtifactsMissing);
            }
        };
        self.source = summary.source;
        self.build_version = summary.version;
        self.build_distribution = summary.distribution;
        self.changes_filenames = summary.names;
        self.binary_packages = summary.binary_packages;
        self.lintian_result = None;
        self.output_directory = Some(path.to_owned());
        Ok(())
    }

    async fn store(&self, conn: &PgPool, run_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO debian_build (run_id, source, version, distribution, lintian_result, binary_packages) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(run_id)
            .bind(&self.source)
            .bind(&self.build_version)
            .bind(&self.build_distribution)
            .bind(&self.lintian_result)
            .bind(&self.binary_packages)
            .execute(conn)
            .await?;
        Ok(())
    }

    fn artifact_filenames(&self) -> Vec<String> {
        let mut ret = Vec::new();
        for changes_filename in &self.changes_filenames {
            let Some(output_dir) = self.output_directory.as_ref() else {
                log::warn!(
                    "No output directory set for changes file: {}",
                    changes_filename
                );
                continue;
            };
            let changes_path = output_dir.join(changes_filename);
            if let Ok(filenames) = crate::changes_filenames(&changes_path) {
                ret.extend(filenames);
            } else {
                log::warn!("Failed to read changes file: {}", changes_path.display());
            }
            ret.push(changes_filename.to_string());
        }
        ret
    }
}

/// Configuration generator for Debian builds.
pub struct DebianConfigGenerator {
    distro_config: Distribution,
    apt_location: Option<String>,
    dep_server_url: Option<String>,
}

impl DebianConfigGenerator {
    /// Create a new Debian configuration generator.
    pub fn new(
        distro_config: Distribution,
        apt_location: Option<String>,
        dep_server_url: Option<String>,
    ) -> Self {
        Self {
            distro_config,
            apt_location,
            dep_server_url,
        }
    }

    /// The distribution's base apt repository line, if it has a mirror
    /// and components.
    fn base_apt_repository(&self) -> Option<String> {
        let mirror = non_empty(&self.distro_config.archive_mirror_uri)?;
        if self.distro_config.component.is_empty() {
            return None;
        }
        Some(format!(
            "{} {} {}",
            mirror,
            self.distro_config.name(),
            self.distro_config.component.join(" ")
        ))
    }

    fn chroot<'a>(&'a self, campaign_config: &'a Campaign) -> Option<&'a str> {
        non_empty(&campaign_config.debian_build().chroot)
            .or_else(|| non_empty(&self.distro_config.chroot))
    }
}

#[async_trait]
impl ConfigGenerator for DebianConfigGenerator {
    fn kind(&self) -> &'static str {
        "debian"
    }

    async fn config(
        &self,
        conn: &PgPool,
        campaign_config: &Campaign,
        queue_item: &QueueItem,
    ) -> Result<Value, Error> {
        let debian_build = campaign_config.debian_build();
        let mut config = Map::new();

        let mut lintian = Map::new();
        lintian.insert(
            "profile".to_string(),
            json!(self.distro_config.lintian_profile()),
        );
        if !self.distro_config.lintian_suppress_tag.is_empty() {
            lintian.insert(
                "suppress-tags".to_string(),
                json!(self.distro_config.lintian_suppress_tag),
            );
        }
        config.insert("lintian".to_string(), Value::Object(lintian));

        let mut extra_janitor_distributions = debian_build.extra_build_distribution.clone();
        if let Some(change_set) = &queue_item.change_set {
            extra_janitor_distributions.push(format!("cs/{}", change_set));
        }

        // TODO(jelmer): Ship build-extra-repositories-keys, and specify [signed-by] here
        let extra_repositories: Vec<String> = match &self.apt_location {
            Some(apt_location) => extra_janitor_distributions
                .iter()
                .map(|suite| format!("deb [trusted=yes] {} {} main", apt_location, suite))
                .collect(),
            None => Vec::new(),
        };
        config.insert(
            "build-extra-repositories".to_string(),
            json!(extra_repositories),
        );

        let build_distribution =
            non_empty(&debian_build.build_distribution).unwrap_or(campaign_config.name());
        config.insert("build-distribution".to_string(), json!(build_distribution));

        config.insert(
            "build-suffix".to_string(),
            json!(non_empty(&debian_build.build_suffix).unwrap_or("")),
        );

        if let Some(build_command) = non_empty(&debian_build.build_command)
            .or_else(|| non_empty(&self.distro_config.build_command))
        {
            config.insert("build-command".to_string(), json!(build_command));
        }

        let last_build_version: Option<String> = sqlx::query_scalar(
            "SELECT MAX(debian_build.version)::text FROM run \
             LEFT JOIN debian_build ON debian_build.run_id = run.id \
             WHERE debian_build.version IS NOT NULL AND run.codebase = $1 AND \
             debian_build.distribution = $2",
        )
        .bind(&queue_item.codebase)
        .bind(build_distribution)
        .fetch_one(conn)
        .await?;
        if let Some(last_build_version) = last_build_version {
            config.insert("last-build-version".to_string(), json!(last_build_version));
        }

        if let Some(chroot) = self.chroot(campaign_config) {
            config.insert("chroot".to_string(), json!(chroot));
        }

        if let Some(base_apt_repository) = self.base_apt_repository() {
            config.insert(
                "base-apt-repository".to_string(),
                json!(base_apt_repository),
            );
            config.insert(
                "base-apt-repository-signed-by".to_string(),
                json!(non_empty(&self.distro_config.signed_by)),
            );
        }

        config.insert("dep_server_url".to_string(), json!(self.dep_server_url));

        Ok(Value::Object(config))
    }

    async fn build_env(
        &self,
        _conn: &PgPool,
        campaign_config: &Campaign,
        _queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, Error> {
        let mut env = HashMap::new();
        if let Some(distro_name) = non_empty(&self.distro_config.name) {
            env.insert("DISTRIBUTION".to_string(), distro_name.to_string());
        }

        let vendor = match non_empty(&self.distro_config.vendor) {
            Some(vendor) => vendor.to_string(),
            None => crate::dpkg_vendor()
                .ok_or_else(|| Error::Other("Unable to determine dpkg vendor".to_string()))?,
        };
        env.insert("DEB_VENDOR".to_owned(), vendor);

        if let Some(chroot) = self.chroot(campaign_config) {
            env.insert("CHROOT".to_owned(), chroot.to_string());
        }

        if let Some(base_apt_repository) = self.base_apt_repository() {
            env.insert("APT_REPOSITORY".to_owned(), base_apt_repository);
        }
        // TODO(jelmer): Set APT_REPOSITORY_KEY

        Ok(env)
    }

    fn additional_colocated_branches(
        &self,
        main_branch: &GenericBranch,
    ) -> HashMap<String, String> {
        silver_platter::debian::pick_additional_colocated_branches(main_branch)
    }
}

/// Get the appropriate configuration generator based on the campaign configuration.
pub fn get_config_generator(
    config: &janitor::config::Config,
    campaign_config: &Campaign,
    apt_archive_url: Option<&str>,
    dep_server_url: Option<&str>,
) -> Result<Box<dyn ConfigGenerator>, Error> {
    if campaign_config.has_debian_build() {
        let base_distribution = campaign_config.debian_build().base_distribution();
        let distribution = config.get_distribution(base_distribution).ok_or_else(|| {
            Error::ConfigError(format!("Unsupported distribution: {}", base_distribution))
        })?;
        Ok(Box::new(DebianConfigGenerator::new(
            distribution.clone(),
            apt_archive_url.map(str::to_string),
            dep_server_url.map(str::to_string),
        )))
    } else if campaign_config.has_generic_build() {
        Ok(Box::new(GenericConfigGenerator::new(
            dep_server_url.map(str::to_string),
        )))
    } else {
        Err(Error::ConfigError("no supported build type".to_string()))
    }
}
