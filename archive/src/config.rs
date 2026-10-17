//! Archive configuration, derived from the textproto `janitor.conf`.

use crate::error::ArchiveError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Top-level archive config: one [`AptRepositoryConfig`] per
/// `apt_repository` block in `janitor.conf`, plus the settings that
/// come from the command line.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct ArchiveConfig {
    pub repositories: HashMap<String, AptRepositoryConfig>,
    pub gpg: Option<GpgConfig>,
    pub archive_path: PathBuf,
    pub default_architectures: Vec<String>,
    /// The parsed `janitor.conf`. Always set outside of tests.
    pub runtime_config: Option<Arc<janitor::config::Config>>,
}

impl ArchiveConfig {
    /// Build the archive config from `janitor.conf`, writing suites
    /// under `dists_directory` and signing with `gpg` if set.
    pub fn from_janitor_config(
        cfg: janitor::config::Config,
        dists_directory: &Path,
        gpg: Option<GpgConfig>,
    ) -> Result<Self, ArchiveError> {
        let default_architectures = default_architectures();
        let repositories = cfg
            .apt_repository
            .iter()
            .map(|proto| {
                let components = components_for_apt_repo(&cfg, proto)?;
                let repo = apt_repository_config_from_proto(
                    proto,
                    dists_directory,
                    &default_architectures,
                    &components,
                    cfg.origin(),
                );
                Ok((repo.name.clone(), repo))
            })
            .collect::<Result<_, ArchiveError>>()?;
        Ok(Self {
            repositories,
            gpg,
            archive_path: dists_directory.to_path_buf(),
            default_architectures,
            runtime_config: Some(Arc::new(cfg)),
        })
    }

    /// Get a repository configuration by name
    pub fn get_repository(&self, name: &str) -> Option<&AptRepositoryConfig> {
        self.repositories.get(name)
    }
}

// TODO(jelmer): Don't hardcode this
fn default_architectures() -> Vec<String> {
    vec!["amd64".to_string()]
}

/// The components an apt_repository serves: those of the
/// distribution named by its `base`.
fn components_for_apt_repo(
    cfg: &janitor::config::Config,
    proto: &janitor::config::AptRepository,
) -> Result<Vec<String>, ArchiveError> {
    let base = proto.base.as_deref().ok_or_else(|| {
        ArchiveError::InvalidConfiguration(format!(
            "apt_repository {} has no base distribution",
            proto.name()
        ))
    })?;
    let dist = cfg.get_distribution(base).ok_or_else(|| {
        ArchiveError::InvalidConfiguration(format!(
            "apt_repository {} has unknown base distribution {}",
            proto.name(),
            base
        ))
    })?;
    Ok(dist.component.to_vec())
}

/// Build an `AptRepositoryConfig` from a textproto `apt_repository`
/// block (`janitor.config::AptRepository`).
pub(crate) fn apt_repository_config_from_proto(
    proto: &janitor::config::AptRepository,
    archive_path: &Path,
    architectures: &[String],
    components: &[String],
    origin: &str,
) -> AptRepositoryConfig {
    let name = proto.name().to_string();
    let description = proto.description().to_string();
    AptRepositoryConfig {
        name: name.clone(),
        label: description.clone(),
        description,
        origin: origin.to_string(),
        suite: name.clone(),
        codename: name.clone(),
        architectures: architectures.to_vec(),
        components: components.to_vec(),
        base_path: archive_path.join(&name),
        by_hash: true,
    }
}

/// One APT repository: identity fields for Release generation
/// (origin/label/suite/codename), the on-disk `base_path` where
/// files are written.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct GpgConfig {
    /// Key selector accepted by `gpg --local-user` (fingerprint or
    /// short ID). When unset, gpg's default secret key is used.
    pub key_id: Option<String>,
    /// Overrides `$GNUPGHOME`.
    pub gpg_home: Option<PathBuf>,
    /// Emit `Release.gpg` (detached).
    pub detached_signature: bool,
    /// Emit `InRelease` (clear-signed).
    pub clearsign: bool,
}

impl GpgConfig {
    /// Sign with `key_id`, or gpg's default key, emitting both
    /// `Release.gpg` and `InRelease`.
    pub fn new(key_id: Option<String>) -> Self {
        Self {
            key_id,
            gpg_home: None,
            detached_signature: true,
            clearsign: true,
        }
    }
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            repositories: HashMap::new(),
            gpg: None,
            archive_path: PathBuf::from("/var/lib/janitor/archive"),
            default_architectures: default_architectures(),
            runtime_config: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loading apt_repository blocks from the protobuf janitor.conf:
    /// each block becomes an AptRepositoryConfig with name/suite/
    /// codename set to `proto.name`, base_path=archive_path/name,
    /// and the description used as label.
    #[test]
    fn test_apt_repository_config_from_proto_basic() {
        let mut proto = janitor::config::AptRepository::new();
        proto.set_name("lintian-fixes".to_string());
        proto.set_description("Builds of lintian fixes".to_string());
        let archive_path = std::path::PathBuf::from("/var/lib/janitor/archive");
        let cfg = apt_repository_config_from_proto(
            &proto,
            &archive_path,
            &["amd64".to_string(), "source".to_string()],
            &["main".to_string()],
            "janitor.debian.net",
        );
        assert_eq!(cfg.name, "lintian-fixes");
        assert_eq!(cfg.suite, "lintian-fixes");
        assert_eq!(cfg.codename, "lintian-fixes");
        assert_eq!(cfg.description, "Builds of lintian fixes");
        assert_eq!(cfg.label, "Builds of lintian fixes");
        assert_eq!(cfg.origin, "janitor.debian.net");
        assert_eq!(cfg.base_path, archive_path.join("lintian-fixes"));
        assert_eq!(cfg.architectures, vec!["amd64", "source"]);
        assert_eq!(cfg.components, vec!["main"]);
        assert!(cfg.by_hash);
    }

    /// Description left empty in the textproto: the Release Label is
    /// left empty too.
    #[test]
    fn test_apt_repository_config_from_proto_empty_description() {
        let mut proto = janitor::config::AptRepository::new();
        proto.set_name("unchanged".to_string());
        let cfg = apt_repository_config_from_proto(
            &proto,
            Path::new("/x"),
            &["amd64".to_string()],
            &["main".to_string()],
            "Janitor",
        );
        assert_eq!(cfg.description, "");
        assert_eq!(cfg.label, "");
    }

    #[test]
    fn test_from_janitor_config() {
        let cfg = janitor::config::read_string(
            r#"
origin: "janitor.debian.net"
distribution {
  name: "unstable"
  component: "main"
  component: "contrib"
}
distribution {
  name: "bookworm"
  component: "main"
}
campaign {
  name: "lintian-fixes"
  debian_build {
    base_distribution: "bookworm"
    build_distribution: "lintian-fixes"
  }
}
apt_repository {
  name: "lintian-fixes"
  base: "unstable"
  description: "Builds of lintian fixes"
  select { campaign: "lintian-fixes" }
}
"#,
        )
        .unwrap();
        let dists = Path::new("/srv/dists");
        let config = ArchiveConfig::from_janitor_config(cfg, dists, None).unwrap();

        assert_eq!(config.archive_path, dists);
        assert!(config.gpg.is_none());
        assert!(config.runtime_config.is_some());
        assert_eq!(config.repositories.len(), 1);
        let repo = config.get_repository("lintian-fixes").unwrap();
        assert_eq!(repo.origin, "janitor.debian.net");
        assert_eq!(repo.description, "Builds of lintian fixes");
        assert_eq!(repo.base_path, dists.join("lintian-fixes"));
        assert_eq!(repo.components, vec!["main", "contrib"]);
        assert_eq!(repo.architectures, vec!["amd64"]);
        assert!(repo.by_hash);
    }

    #[test]
    fn test_from_janitor_config_rejects_unknown_base() {
        let cfg = janitor::config::read_string(
            r#"
apt_repository {
  name: "lintian-fixes"
  base: "unstable"
}
"#,
        )
        .unwrap();
        assert!(matches!(
            ArchiveConfig::from_janitor_config(cfg, Path::new("/srv/dists"), None),
            Err(ArchiveError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn test_from_janitor_config_rejects_missing_base() {
        let cfg =
            janitor::config::read_string("apt_repository { name: \"lintian-fixes\" }").unwrap();
        assert!(matches!(
            ArchiveConfig::from_janitor_config(cfg, Path::new("/srv/dists"), None),
            Err(ArchiveError::InvalidConfiguration(_))
        ));
    }
}
