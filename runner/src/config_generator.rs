use async_trait::async_trait;
use breezyshim::branch::GenericBranch;
use debversion::Version;
use janitor::api::worker::LintianConfig;
use janitor::config::{Campaign, Distribution};
use janitor::queue::QueueItem;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;
use url::Url;

static DEFAULT_DEP_SERVER_URL: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("http://dep-server:8080").expect("hardcoded default dep-server URL parses")
});

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
pub trait ConfigGenerator {
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
    dep_server_url: url::Url,
}

impl GenericConfigGenerator {
    /// Create a new generic configuration generator.
    pub fn new(dep_server_url: Option<url::Url>) -> Self {
        Self {
            dep_server_url: dep_server_url.unwrap_or_else(|| DEFAULT_DEP_SERVER_URL.clone()),
        }
    }
}

#[async_trait]
impl ConfigGenerator for GenericConfigGenerator {
    async fn config(
        &self,
        _conn: &PgPool,
        campaign_config: &Campaign,
        _queue_item: &QueueItem,
    ) -> Result<serde_json::Value, Error> {
        let mut config = janitor::api::worker::GenericBuildConfig::default();
        if let Some(chroot) = &campaign_config.generic_build().chroot {
            config.chroot = Some(chroot.to_string());
        }
        config.dep_server_url = Some(self.dep_server_url.clone());
        serde_json::to_value(config)
            .map_err(|e| Error::Other(format!("Failed to serialize config: {}", e)))
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
    dep_server_url: Option<url::Url>,
}

impl DebianConfigGenerator {
    /// Create a new Debian configuration generator.
    pub fn new(
        distro_config: Distribution,
        apt_location: Option<String>,
        dep_server_url: Option<url::Url>,
    ) -> Self {
        Self {
            distro_config,
            apt_location,
            dep_server_url,
        }
    }
}

#[async_trait]
impl ConfigGenerator for DebianConfigGenerator {
    async fn config(
        &self,
        conn: &PgPool,
        campaign_config: &Campaign,
        queue_item: &QueueItem,
    ) -> Result<serde_json::Value, Error> {
        let mut config = janitor::api::worker::DebianBuildConfig {
            lintian: LintianConfig {
                profile: self.distro_config.lintian_profile.clone(),
                suppress_tags: Some(self.distro_config.lintian_suppress_tag.clone()),
            },
            ..Default::default()
        };

        let mut extra_janitor_distributions = Vec::new();
        extra_janitor_distributions.extend(
            campaign_config
                .debian_build()
                .extra_build_distribution
                .iter()
                .cloned(),
        );
        if let Some(change_set) = &queue_item.change_set {
            extra_janitor_distributions.push(format!("cs/{}", change_set));
        }

        // Use signed repositories instead of trusted=yes
        let build_extra_repositories = extra_janitor_distributions
            .iter()
            .filter_map(|suite| {
                // Use the Debian Janitor signing key for extra repositories
                match self.apt_location.as_ref() {
                    Some(apt_location) => Some(format!(
                        "deb [arch=amd64 signed-by=/etc/apt/keyrings/debian-janitor.gpg] {} {} main",
                        apt_location,
                        suite
                    )),
                    None => {
                        log::warn!("APT location not configured, skipping extra repository for suite: {}", suite);
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        config.extra_repositories = Some(build_extra_repositories);

        // Add the Debian Janitor repository key for extra repositories
        if !extra_janitor_distributions.is_empty() {
            config.apt_repository_key = Some("/etc/apt/keyrings/debian-janitor.gpg".to_string());
        }

        let build_distribution = campaign_config
            .debian_build()
            .build_distribution
            .as_deref()
            .unwrap_or(campaign_config.name());
        config.build_distribution = Some(build_distribution.to_string());

        let build_suffix = campaign_config
            .debian_build()
            .build_suffix
            .as_deref()
            .unwrap_or("");
        config.build_suffix = Some(build_suffix.to_string());

        config.build_command =
            if let Some(build_command) = &campaign_config.debian_build().build_command {
                Some(build_command.to_string())
            } else {
                self.distro_config
                    .build_command
                    .as_ref()
                    .map(|build_command| build_command.to_string())
            };

        let last_build_version: Option<(debversion::Version, )> = sqlx::query_as("SELECT MAX(debian_build.version) FROM run LEFT JOIN debian_build ON debian_build.run_id = run.id WHERE debian_build.version IS NOT NULL AND run.codebase = $1 AND debian_build.distribution = $2")
            .bind(&queue_item.codebase)
            .bind(&config.build_distribution)
            .fetch_optional(conn)
            .await?;

        if let Some((last_build_version,)) = last_build_version {
            config.last_build_version = Some(last_build_version);
        }

        config.chroot = if let Some(chroot) = &campaign_config.debian_build().chroot {
            Some(chroot.to_string())
        } else {
            self.distro_config
                .chroot
                .as_ref()
                .map(|chroot| chroot.to_string())
        };

        if let (Some(archive_mirror_uri), Some(distro_name)) = (
            &self.distro_config.archive_mirror_uri,
            &self.distro_config.name,
        ) {
            config.apt_repository = Some(format!(
                "{} {} {}",
                archive_mirror_uri,
                distro_name,
                self.distro_config.component.join(" ")
            ));
            config.apt_repository_key = self.distro_config.signed_by.clone();
        }

        config.dep_server_url = self.dep_server_url.as_ref().map(|u| u.to_string());

        serde_json::to_value(config)
            .map_err(|e| Error::Other(format!("Failed to serialize config: {}", e)))
    }

    async fn build_env(
        &self,
        _conn: &PgPool,
        campaign_config: &Campaign,
        _queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, Error> {
        let mut env = HashMap::new();
        if let Some(distro_name) = &self.distro_config.name {
            env.insert("DISTRIBUTION".to_string(), distro_name.to_string());
        }

        if let Some(vendor) = self
            .distro_config
            .vendor
            .clone()
            .or_else(crate::dpkg_vendor)
        {
            env.insert("DEB_VENDOR".to_owned(), vendor);
        }

        if let Some(chroot) = &campaign_config.debian_build().chroot {
            env.insert("CHROOT".to_owned(), chroot.to_string());
        } else if let Some(chroot) = &self.distro_config.chroot {
            env.insert("CHROOT".to_owned(), chroot.to_string());
        }

        if let (Some(archive_mirror_uri), Some(distro_name)) = (
            &self.distro_config.archive_mirror_uri,
            &self.distro_config.name,
        ) {
            env.insert(
                "APT_REPOSITORY".to_owned(),
                format!(
                    "{} {} {}",
                    archive_mirror_uri,
                    distro_name,
                    self.distro_config.component.join(" ")
                ),
            );
        }

        if let Some(signed_by) = &self.distro_config.signed_by {
            env.insert("APT_REPOSITORY_KEY".to_owned(), signed_by.clone());
        }

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
    apt_archive_url: Option<&Url>,
    dep_server_url: Option<&Url>,
) -> Result<Box<dyn ConfigGenerator>, Error> {
    if campaign_config.has_debian_build() {
        let base_distribution =
            if let Some(d) = campaign_config.debian_build().base_distribution.as_deref() {
                d
            } else {
                return Err(Error::ConfigError(
                    "No base distribution specified".to_string(),
                ));
            };
        match config.get_distribution(base_distribution) {
            Some(distribution) => Ok(Box::new(DebianConfigGenerator::new(
                distribution.clone(),
                apt_archive_url.map(|u| u.to_string()),
                dep_server_url.cloned(),
            )) as Box<dyn ConfigGenerator>),
            None => Err(Error::ConfigError(format!(
                "Unsupported distribution: {}",
                base_distribution
            ))),
        }
    } else if campaign_config.has_generic_build() {
        Ok(
            Box::new(GenericConfigGenerator::new(dep_server_url.cloned()))
                as Box<dyn ConfigGenerator>,
        )
    } else {
        Err(Error::ConfigError("no supported build type".to_string()))
    }
}
