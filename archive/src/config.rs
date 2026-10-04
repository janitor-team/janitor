//! Configuration types loaded from env vars and the legacy textproto `janitor.conf`.

use janitor::shared_config::{
    ConfigError, ConfigLoader, ConfigSource, FromEnv, Mergeable, ServiceConfig, ValidationError,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level archive config. Loaded from env vars, and (when
/// `legacy_config_path` is set) merged with an `apt_repository`
/// block per repo from the textproto `janitor.conf`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct ArchiveConfig {
    #[serde(flatten)]
    pub base: ServiceConfig,
    pub repositories: HashMap<String, AptRepositoryConfig>,
    pub gpg: Option<GpgConfig>,
    pub archive_path: PathBuf,
    pub artifact_manager_url: Option<String>,
    #[serde(default)]
    pub default_architectures: Vec<String>,
    pub max_artifact_size: u64,
    pub cache_ttl_seconds: u64,
    pub cache_size_mb: u64,
    pub legacy_config_path: Option<PathBuf>,
    /// Loaded protobuf janitor.conf, when `legacy_config_path`
    /// resolves. Not serialized (the protobuf `Config` type
    /// doesn't derive serde).
    #[serde(skip)]
    pub runtime_config: Option<std::sync::Arc<janitor::config::Config>>,
}

/// Derive the components an apt_repository serves from the base
/// distribution declared on its first `select`ed campaign. Returns
/// None when the config doesn't wire enough of `select -> campaign
/// -> debian_build -> distribution` for us to make the call.
fn components_for_apt_repo(
    cfg: &janitor::config::Config,
    proto: &janitor::config::AptRepository,
) -> Option<Vec<String>> {
    let campaign_name = proto.select.first().and_then(|s| s.campaign.as_deref())?;
    let campaign = cfg.get_campaign(campaign_name)?;
    if !campaign.has_debian_build() {
        return None;
    }
    let base_distribution = campaign.debian_build().base_distribution.as_deref()?;
    let dist = cfg.get_distribution(base_distribution)?;
    if dist.component.is_empty() {
        return None;
    }
    Some(dist.component.to_vec())
}

/// Build an `AptRepositoryConfig` from a textproto `apt_repository`
/// block (`janitor.config::AptRepository`).
pub(crate) fn apt_repository_config_from_proto(
    proto: &janitor::config::AptRepository,
    archive_path: &std::path::Path,
    base_url_root: &str,
    architectures: &[String],
    components: &[String],
    origin: &str,
    by_hash: bool,
) -> AptRepositoryConfig {
    let name = proto.name().to_string();
    AptRepositoryConfig {
        name: name.clone(),
        description: if proto.description().is_empty() {
            format!("{} APT repository", name)
        } else {
            proto.description().to_string()
        },
        origin: origin.to_string(),
        label: name.clone(),
        suite: name.clone(),
        codename: name.clone(),
        architectures: architectures.to_vec(),
        components: components.to_vec(),
        base_url: format!("{}/{}", base_url_root.trim_end_matches('/'), name),
        base_path: archive_path.join(&name),
        by_hash,
    }
}

/// One APT repository: identity fields for Release generation
/// (origin/label/suite/codename), the on-disk `base_path` where
/// files are written, and the `base_url` clients fetch them from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct AptRepositoryConfig {
    pub name: String,
    pub description: String,
    pub origin: String,
    pub label: String,
    pub suite: String,
    pub codename: String,
    pub architectures: Vec<String>,
    pub components: Vec<String>,
    pub base_url: String,
    pub base_path: PathBuf,
    pub by_hash: bool,
}

impl AptRepositoryConfig {
    /// Create a new APT repository configuration.
    pub fn new(
        name: String,
        suite: String,
        architectures: Vec<String>,
        base_path: PathBuf,
    ) -> Self {
        Self {
            name: name.clone(),
            description: String::new(),
            origin: name.clone(),
            label: name,
            suite: suite.clone(),
            codename: suite,
            architectures,
            components: vec!["main".to_string()],
            base_url: String::new(),
            base_path,
            by_hash: false,
        }
    }
    /// Directory that contains the generated repository files for
    /// this suite. Equivalent to `<dists_directory>/<name>`.
    ///
    /// `base_path` is already the per-suite directory (set by
    /// `apt_repository_config_from_proto` to `archive_path/name`,
    /// which is `<dists_directory>/<name>` when `archive_path`
    /// points at the dists tree). Callers use this method rather
    /// than `base_path` directly so the layout stays a single
    /// source of truth.
    pub fn suite_path(&self) -> PathBuf {
        self.base_path.clone()
    }

    /// Get the component path for a specific architecture. Path is
    /// `<suite_path>/<component>/binary-<arch>`.
    pub fn component_arch_path(&self, component: &str, arch: &str) -> PathBuf {
        self.suite_path()
            .join(component)
            .join(format!("binary-{}", arch))
    }

    /// Get the source path for a component:
    /// `<suite_path>/<component>/source`.
    pub fn source_path(&self, component: &str) -> PathBuf {
        self.suite_path().join(component).join("source")
    }

    /// Validate the repository configuration
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("Repository name cannot be empty".to_string());
        }
        if self.architectures.is_empty() {
            return Err("At least one architecture must be specified".to_string());
        }
        if self.components.is_empty() {
            return Err("At least one component must be specified".to_string());
        }
        Ok(())
    }
}

/// Inputs passed to [`crate::sign::sign_release`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct GpgConfig {
    /// Key selector accepted by `gpg --local-user` (fingerprint or
    /// short ID).
    pub key_id: String,
    pub gpg_home: Option<PathBuf>,
    pub passphrase: Option<String>,
    /// Emit `Release.gpg` (detached).
    pub detached_signature: bool,
    /// Emit `InRelease` (clear-signed).
    pub clearsign: bool,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            base: ServiceConfig::default(),
            repositories: HashMap::new(),
            gpg: None,
            archive_path: PathBuf::from("/var/lib/janitor/archive"),
            artifact_manager_url: None,
            default_architectures: vec!["amd64".to_string(), "source".to_string()],
            max_artifact_size: 1024 * 1024 * 1024, // 1GB
            cache_ttl_seconds: 3600,               // 1 hour
            cache_size_mb: 100,
            legacy_config_path: None,
            runtime_config: None,
        }
    }
}

impl FromEnv for ArchiveConfig {
    fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_prefix("")
    }

    fn from_env_with_prefix(prefix: &str) -> Result<Self, ConfigError> {
        use janitor::shared_config::EnvParser;
        let parser = EnvParser::with_prefix(prefix);

        // Load base configuration
        let base = ServiceConfig::from_env_with_prefix(prefix)?;

        // Archive-specific configuration
        let archive_path = parser
            .get_string("ARCHIVE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/janitor/archive"));

        let artifact_manager_url = parser.get_string("ARTIFACT_MANAGER_URL");

        let max_artifact_size = parser
            .get_u64("MAX_ARTIFACT_SIZE")?
            .unwrap_or(1024 * 1024 * 1024);

        let cache_ttl_seconds = parser.get_u64("CACHE_TTL_SECONDS")?.unwrap_or(3600);
        let cache_size_mb = parser.get_u64("CACHE_SIZE_MB")?.unwrap_or(100);

        // GPG configuration
        let gpg = if let Some(key_id) = parser.get_string("GPG_KEY_ID") {
            Some(GpgConfig {
                key_id,
                gpg_home: parser.get_string("GPG_HOME").map(PathBuf::from),
                passphrase: parser.get_string("GPG_PASSPHRASE"),
                detached_signature: parser.get_bool("GPG_DETACHED_SIGNATURE")?.unwrap_or(true),
                clearsign: parser.get_bool("GPG_CLEARSIGN")?.unwrap_or(true),
            })
        } else {
            None
        };

        // Default architectures
        let default_architectures = parser
            .get_string("DEFAULT_ARCHITECTURES")
            .map(|s| s.split(',').map(|arch| arch.trim().to_string()).collect())
            .unwrap_or_else(|| vec!["amd64".to_string(), "source".to_string()]);

        // Load repositories from environment or config file
        let mut repositories = Self::load_repositories_from_env(&parser)?;

        // Layer in apt_repository blocks from the legacy textproto
        // janitor.conf. Env-config repos win on overlap, so a
        // deployment can still override a single repo's settings
        // without knocking out the rest.
        let legacy_config_path = parser.get_string("LEGACY_CONFIG_PATH").map(PathBuf::from);
        let mut runtime_config: Option<std::sync::Arc<janitor::config::Config>> = None;
        if let Some(ref path) = legacy_config_path {
            match janitor::config::read_file(path) {
                Ok(cfg) => {
                    let origin = parser
                        .get_string("REPOSITORY_ORIGIN")
                        .unwrap_or_else(|| "Janitor".to_string());
                    let base_url_root = parser
                        .get_string("REPOSITORY_BASE_URL")
                        .unwrap_or_else(|| "http://localhost:9914/".to_string());
                    let by_hash = parser.get_bool("REPOSITORY_BY_HASH")?.unwrap_or(true);
                    for proto in &cfg.apt_repository {
                        let name = proto.name().to_string();
                        if name.is_empty() || repositories.contains_key(&name) {
                            continue;
                        }
                        // Pull components from the campaign's target
                        // distribution when we can, so periodic and
                        // /publish emit exactly the configured
                        // components. Falls back to `main`.
                        let components = components_for_apt_repo(&cfg, proto)
                            .unwrap_or_else(|| vec!["main".to_string()]);
                        repositories.insert(
                            name.clone(),
                            apt_repository_config_from_proto(
                                proto,
                                &archive_path,
                                &base_url_root,
                                &default_architectures,
                                &components,
                                &origin,
                                by_hash,
                            ),
                        );
                    }
                    runtime_config = Some(std::sync::Arc::new(cfg));
                }
                Err(e) => {
                    tracing::warn!("Failed to load legacy config at {}: {}", path.display(), e)
                }
            }
        }

        Ok(Self {
            base,
            repositories,
            gpg,
            archive_path,
            artifact_manager_url,
            default_architectures,
            max_artifact_size,
            cache_ttl_seconds,
            cache_size_mb,
            legacy_config_path,
            runtime_config,
        })
    }
}

impl ArchiveConfig {
    /// Load repository configurations from environment variables
    fn load_repositories_from_env(
        parser: &janitor::shared_config::EnvParser,
    ) -> Result<HashMap<String, AptRepositoryConfig>, ConfigError> {
        let mut repos = HashMap::new();

        // Support REPOSITORY_{NAME}_{FIELD} pattern
        // For now, support a single default repository
        if let Some(name) = parser.get_string("REPOSITORY_NAME") {
            let repo = AptRepositoryConfig {
                name: name.clone(),
                description: parser
                    .get_string("REPOSITORY_DESCRIPTION")
                    .unwrap_or_else(|| format!("{} APT repository", name)),
                origin: parser
                    .get_string("REPOSITORY_ORIGIN")
                    .unwrap_or_else(|| "Janitor".to_string()),
                label: parser
                    .get_string("REPOSITORY_LABEL")
                    .unwrap_or_else(|| name.clone()),
                suite: parser
                    .get_string("REPOSITORY_SUITE")
                    .unwrap_or_else(|| "unstable".to_string()),
                codename: parser
                    .get_string("REPOSITORY_CODENAME")
                    .unwrap_or_else(|| "unstable".to_string()),
                architectures: parser
                    .get_string("REPOSITORY_ARCHITECTURES")
                    .map(|s| s.split(',').map(|a| a.trim().to_string()).collect())
                    .unwrap_or_else(|| vec!["amd64".to_string(), "source".to_string()]),
                components: parser
                    .get_string("REPOSITORY_COMPONENTS")
                    .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
                    .unwrap_or_else(|| vec!["main".to_string()]),
                base_url: parser
                    .get_string("REPOSITORY_BASE_URL")
                    .unwrap_or_else(|| "http://localhost:9913/".to_string()),
                base_path: parser
                    .get_string("REPOSITORY_BASE_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/var/lib/janitor/archive/repo")),
                by_hash: parser.get_bool("REPOSITORY_BY_HASH")?.unwrap_or(true),
            };
            repos.insert(name, repo);
        }

        Ok(repos)
    }

    /// Get the effective artifact manager URL
    pub fn artifact_manager_url(&self) -> Option<&str> {
        self.artifact_manager_url.as_deref().or(self
            .base
            .external_services
            .artifact_service_url
            .as_deref())
    }

    /// Check if GPG signing is configured
    pub fn has_gpg_signing(&self) -> bool {
        self.gpg.is_some()
    }

    /// Get a repository configuration by name
    pub fn get_repository(&self, name: &str) -> Option<&AptRepositoryConfig> {
        self.repositories.get(name)
    }
}

impl ConfigLoader for ArchiveConfig {
    fn from_env() -> Result<Self, ConfigError> {
        <Self as FromEnv>::from_env()
    }

    fn from_file<P: AsRef<std::path::Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::IoError {
            path: path.as_ref().display().to_string(),
            message: e.to_string(),
        })?;

        // Auto-detect format based on extension
        let extension = path
            .as_ref()
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("");

        match extension {
            // Only JSON is supported for the archive-specific
            // config file today. The archive's textproto
            // `janitor.conf` is loaded separately via
            // `LEGACY_CONFIG_PATH` (see FromEnv), not through this
            // path. Callers who need YAML/TOML for archive-specific
            // settings should either convert to JSON or point
            // LEGACY_CONFIG_PATH at their textproto and use env
            // vars for the remaining overrides. Rejecting yaml/toml
            // explicitly is deliberate -- the earlier code fell back
            // to serde_json::from_str which silently mangled real
            // yaml/toml files.
            "json" => serde_json::from_str(&content).map_err(|e| ConfigError::ParseError {
                field: "root".to_string(),
                message: e.to_string(),
            }),
            "yaml" | "yml" | "toml" => Err(ConfigError::ParseError {
                field: "file".to_string(),
                message: format!(
                    "{} config format is not supported by the archive service; \
                     use JSON, or load via LEGACY_CONFIG_PATH + env vars",
                    extension
                ),
            }),
            _ => Err(ConfigError::ParseError {
                field: "file".to_string(),
                message: format!("Unsupported file extension: {}", extension),
            }),
        }
    }

    fn from_sources(sources: &[ConfigSource]) -> Result<Self, ConfigError> {
        let mut config = Self::default();

        for source in sources {
            match source {
                ConfigSource::File(path) => {
                    let file_config = Self::from_file(path)?;
                    config = config.merge_with(file_config);
                }
                ConfigSource::Environment => {
                    let env_config = <Self as FromEnv>::from_env()?;
                    config = config.merge_with(env_config);
                }
                ConfigSource::Defaults => {
                    // Already using defaults as starting point
                }
            }
        }

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ValidationError> {
        // Validate base configuration
        if let Some(ref database) = self.base.database {
            database.validate()?;
        }
        if let Some(ref redis) = self.base.redis {
            redis.validate()?;
        }
        if let Some(ref web) = self.base.web {
            web.validate()?;
        }
        self.base.logging.validate()?;

        // Validate archive-specific fields
        if !self.archive_path.exists() {
            return Err(ValidationError::InvalidValue {
                field: "archive_path".to_string(),
                message: format!("Archive path does not exist: {:?}", self.archive_path),
            });
        }

        if self.repositories.is_empty() {
            return Err(ValidationError::MissingField {
                field: "repositories".to_string(),
            });
        }

        // Validate each repository
        for (name, repo) in &self.repositories {
            if repo.architectures.is_empty() {
                return Err(ValidationError::InvalidValue {
                    field: format!("repositories.{}.architectures", name),
                    message: "Repository must support at least one architecture".to_string(),
                });
            }
            if repo.components.is_empty() {
                return Err(ValidationError::InvalidValue {
                    field: format!("repositories.{}.components", name),
                    message: "Repository must have at least one component".to_string(),
                });
            }
        }

        // Validate GPG configuration if present
        if let Some(ref gpg) = self.gpg {
            if gpg.key_id.is_empty() {
                return Err(ValidationError::InvalidValue {
                    field: "gpg.key_id".to_string(),
                    message: "GPG key ID cannot be empty".to_string(),
                });
            }
        }

        Ok(())
    }
}

impl Mergeable for ArchiveConfig {
    fn merge_with(mut self, other: Self) -> Self {
        // Merge base configuration
        self.base = self.base.merge(other.base);

        // Merge Archive-specific fields (other takes precedence)
        if !other.repositories.is_empty() {
            self.repositories = other.repositories;
        }
        if other.gpg.is_some() {
            self.gpg = other.gpg;
        }
        if other.archive_path != *"/var/lib/janitor/archive" {
            self.archive_path = other.archive_path;
        }
        if other.artifact_manager_url.is_some() {
            self.artifact_manager_url = other.artifact_manager_url;
        }
        if !other.default_architectures.is_empty() {
            self.default_architectures = other.default_architectures;
        }
        self.max_artifact_size = other.max_artifact_size;
        self.cache_ttl_seconds = other.cache_ttl_seconds;
        self.cache_size_mb = other.cache_size_mb;

        if other.runtime_config.is_some() {
            self.runtime_config = other.runtime_config;
        }
        if other.legacy_config_path.is_some() {
            self.legacy_config_path = other.legacy_config_path;
        }

        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_config_from_env() {
        // Set minimal required env vars
        std::env::set_var("DATABASE_URL", "postgresql://test/db");
        std::env::set_var("REPOSITORY_NAME", "test-repo");
        std::env::set_var("ARCHIVE_PATH", "/tmp/archive");

        let config = <ArchiveConfig as FromEnv>::from_env().unwrap();
        assert!(config.base.database.is_some());
        assert_eq!(config.repositories.len(), 1);
        assert!(config.repositories.contains_key("test-repo"));
    }

    #[test]
    fn test_archive_config_validation() {
        let mut config = ArchiveConfig::default();

        // Should fail with no repositories
        assert!(config.validate().is_err());

        // Add a repository
        config.repositories.insert(
            "test".to_string(),
            AptRepositoryConfig {
                name: "test".to_string(),
                description: "Test repo".to_string(),
                origin: "Test".to_string(),
                label: "Test".to_string(),
                suite: "unstable".to_string(),
                codename: "unstable".to_string(),
                architectures: vec!["amd64".to_string()],
                components: vec!["main".to_string()],
                base_url: "http://localhost/".to_string(),
                base_path: PathBuf::from("/tmp/test"),
                by_hash: true,
            },
        );

        // Set a valid archive path
        config.archive_path = PathBuf::from("/tmp");

        // Should pass with valid repository
        assert!(config.validate().is_ok());
    }

    /// Loading apt_repository blocks from the protobuf janitor.conf:
    /// each block becomes an AptRepositoryConfig with name/suite/
    /// codename set to `proto.name`, base_path=archive_path/name,
    /// base_url=base_url_root/name, and the description carried
    /// through. Missing description falls back to a default.
    #[test]
    fn test_apt_repository_config_from_proto_basic() {
        let mut proto = janitor::config::AptRepository::new();
        proto.set_name("lintian-fixes".to_string());
        proto.set_description("Builds of lintian fixes".to_string());
        let archive_path = std::path::PathBuf::from("/var/lib/janitor/archive");
        let cfg = apt_repository_config_from_proto(
            &proto,
            &archive_path,
            "http://janitor.local/",
            &["amd64".to_string(), "source".to_string()],
            &["main".to_string()],
            "janitor.debian.net",
            true,
        );
        assert_eq!(cfg.name, "lintian-fixes");
        assert_eq!(cfg.suite, "lintian-fixes");
        assert_eq!(cfg.codename, "lintian-fixes");
        assert_eq!(cfg.description, "Builds of lintian fixes");
        assert_eq!(cfg.origin, "janitor.debian.net");
        // Trailing slash in base_url_root must not double up.
        assert_eq!(cfg.base_url, "http://janitor.local/lintian-fixes");
        assert_eq!(cfg.base_path, archive_path.join("lintian-fixes"));
        assert_eq!(cfg.architectures, vec!["amd64", "source"]);
        assert_eq!(cfg.components, vec!["main"]);
        assert!(cfg.by_hash);
    }

    /// Description left empty in the textproto: synthesise one
    /// from the suite name so the Release file isn't broken.
    #[test]
    fn test_apt_repository_config_from_proto_default_description() {
        let mut proto = janitor::config::AptRepository::new();
        proto.set_name("unchanged".to_string());
        // No set_description.
        let cfg = apt_repository_config_from_proto(
            &proto,
            std::path::Path::new("/x"),
            "http://x",
            &["amd64".to_string()],
            &["main".to_string()],
            "Janitor",
            false,
        );
        assert_eq!(cfg.description, "unchanged APT repository");
        assert!(!cfg.by_hash);
    }

    #[test]
    fn test_gpg_config() {
        let config = ArchiveConfig {
            gpg: Some(GpgConfig {
                key_id: "ABCD1234".to_string(),
                gpg_home: Some(PathBuf::from("/home/user/.gnupg")),
                passphrase: None,
                detached_signature: true,
                clearsign: true,
            }),
            ..Default::default()
        };

        assert!(config.has_gpg_signing());
        assert_eq!(config.gpg.as_ref().unwrap().key_id, "ABCD1234");
    }
}
