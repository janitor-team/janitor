//! Builder system for the Janitor runner.
//!
//! This module provides the builder trait and implementations for different build types.

use crate::{BuilderResult, QueueItem};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Lintian input file structure.
#[derive(Deserialize, Serialize, Debug, Clone)]
struct LintianInputFile {
    /// Lintian hints for this file.
    pub hints: Vec<String>,
    /// Path to the file.
    pub path: PathBuf,
}

/// Lintian group structure.
#[derive(Deserialize, Serialize, Debug, Clone)]
struct LintianGroup {
    /// Group identifier.
    pub group_id: String,
    /// Input files in this group.
    pub input_files: Vec<LintianInputFile>,
    /// Source package name.
    pub source_name: String,
    /// Source version.
    pub source_version: String,
}

/// Lintian result structure.
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
struct LintianResult {
    /// Lintian groups.
    pub groups: Vec<LintianGroup>,
    /// Lintian version used.
    pub lintian_version: Option<String>,
}

/// Campaign configuration for builds.
/// This is a simplified version - in a full implementation this would be loaded from config
#[derive(Debug, Clone)]
pub struct CampaignConfig {
    /// Generic build configuration.
    pub generic_build: Option<GenericBuildConfig>,
    /// Debian build configuration.
    pub debian_build: Option<DebianBuildConfig>,
    /// Force-build flag from the campaign (proto `force_build`).
    /// Sent to the worker as `force-build` so a no-op codemod still
    /// produces a build artifact (used by the `control` campaign).
    pub force_build: bool,
    /// Whether the campaign is allowed to start from an empty tree
    /// when the codebase has no branch URL (proto `default_empty`).
    /// Emitted to the worker as `branch.default-empty` and used by
    /// assignment validation to decide whether a missing branch_url
    /// is fatal.
    pub default_empty: bool,
}

/// Generic build configuration.
#[derive(Debug, Clone)]
pub struct GenericBuildConfig {
    /// Chroot to use for builds.
    pub chroot: Option<String>,
}

/// Debian build configuration.
///
/// Mirrors the protobuf `DebianBuild` message at the level of detail
/// the runner needs when assembling worker assignments. The earlier
/// version only carried `base_distribution` + `extra_build_distribution`;
/// without `build_command` (and the related fields) the worker's
/// `DebianBuildConfig.build_command` deserialised as `None` and the
/// build step short-circuited to a no-op -- meaning no `debian_build`
/// row, no .deb produced, and no debdiff for any per-run page.
#[derive(Debug, Clone)]
pub struct DebianBuildConfig {
    /// Base distribution for builds.
    pub base_distribution: String,
    /// Distribution to label the build with (Python convention:
    /// fall back to the campaign name when empty).
    pub build_distribution: Option<String>,
    /// Suffix to append to the version on build.
    pub build_suffix: Option<String>,
    /// sbuild-style invocation; `None` means defer to the
    /// distribution's `build_command`.
    pub build_command: Option<String>,
    /// Chroot to use; `None` means defer to the distribution's
    /// `chroot`.
    pub chroot: Option<String>,
    /// Extra build distributions.
    pub extra_build_distribution: Vec<String>,
}

/// Abstract builder trait for different build systems.
#[async_trait]
pub trait Builder: Send + Sync {
    /// Get the kind of builder.
    fn kind(&self) -> &'static str;

    /// Generate build configuration.
    async fn config(
        &self,
        conn: &mut PgConnection,
        campaign_config: &CampaignConfig,
        queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, BuilderError>;

    /// Generate build environment variables.
    async fn build_env(
        &self,
        conn: &mut PgConnection,
        campaign_config: &CampaignConfig,
        queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, BuilderError>;

    /// Get additional colocated branches for this build type.
    fn additional_colocated_branches(&self, main_branch: &str) -> HashMap<String, String>;

    /// Process build results from a directory.
    fn process_result(&self, output_dir: &Path) -> Result<BuilderResult, BuilderError>;
}

/// Errors that can occur during building.
#[derive(Debug, thiserror::Error)]
pub enum BuilderError {
    /// Database error.
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Configuration error.
    #[error("Configuration error: {0}")]
    Config(String),
    /// Build processing error.
    #[error("Build processing error: {0}")]
    Processing(String),
    /// Lintian processing error.
    #[error("Lintian error: {0}")]
    Lintian(String),
}

/// Parse lintian output from JSON.
fn parse_lintian_output(text: &str) -> Result<LintianResult, BuilderError> {
    let lines: Vec<&str> = text.trim().split('\n').collect();
    let mut joined_lines: Vec<&str> = Vec::new();
    for line in lines {
        joined_lines.push(line);
        if line == "}" {
            break;
        }
    }

    let joined_str = joined_lines.join("\n");
    let mut result: LintianResult = serde_json::from_str(&joined_str)
        .map_err(|e| BuilderError::Lintian(format!("Failed to parse lintian JSON: {}", e)))?;

    // Strip irrelevant directory information
    for group in &mut result.groups {
        for input_file in &mut group.input_files {
            if let Some(file_name) = input_file.path.file_name() {
                input_file.path = PathBuf::from(file_name);
            }
        }
    }

    Ok(result)
}

/// Run lintian on changes files and return the result.
fn run_lintian(
    output_directory: &Path,
    changes_names: &[String],
    profile: Option<&str>,
    suppress_tags: Option<&[String]>,
) -> Result<LintianResult, BuilderError> {
    let mut args: Vec<String> = vec![
        "--exp-output=format=json".to_owned(),
        "--allow-root".to_owned(),
    ];

    if let Some(tags) = suppress_tags {
        if !tags.is_empty() {
            args.push(format!("--suppress-tags={}", tags.join(",")));
        }
    }

    if let Some(profile_str) = profile {
        args.push(format!("--profile={}", profile_str));
    }

    // Add changes file paths
    for changes_name in changes_names {
        args.push(changes_name.clone());
    }

    let mut cmd = Command::new("lintian");
    cmd.args(args);
    cmd.current_dir(output_directory);

    let lintian_output = cmd
        .output()
        .map_err(|e| BuilderError::Lintian(format!("Failed to run lintian: {}", e)))?;

    let output_str = std::str::from_utf8(&lintian_output.stdout)
        .map_err(|e| BuilderError::Lintian(format!("Invalid lintian output: {}", e)))?;

    if output_str.trim().is_empty() {
        // Empty output means no issues found
        return Ok(LintianResult::default());
    }

    parse_lintian_output(output_str)
}

/// Generic builder implementation.
#[derive(Debug, Clone)]
pub struct GenericBuilder {
    /// Dependency server URL.
    pub dep_server_url: Option<String>,
}

impl GenericBuilder {
    /// Create a new generic builder.
    pub fn new(dep_server_url: Option<String>) -> Self {
        Self { dep_server_url }
    }
}

#[async_trait]
impl Builder for GenericBuilder {
    fn kind(&self) -> &'static str {
        "generic"
    }

    async fn config(
        &self,
        _conn: &mut PgConnection,
        campaign_config: &CampaignConfig,
        _queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, BuilderError> {
        let mut config = HashMap::new();

        if let Some(generic_config) = &campaign_config.generic_build {
            if let Some(chroot) = &generic_config.chroot {
                config.insert("chroot".to_string(), chroot.clone());
            }
        }

        if let Some(dep_server_url) = &self.dep_server_url {
            config.insert("dep_server_url".to_string(), dep_server_url.clone());
        }

        Ok(config)
    }

    async fn build_env(
        &self,
        _conn: &mut PgConnection,
        _campaign_config: &CampaignConfig,
        _queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, BuilderError> {
        // Generic builds don't need special environment variables
        Ok(HashMap::new())
    }

    fn additional_colocated_branches(&self, _main_branch: &str) -> HashMap<String, String> {
        // Generic builds don't have additional branches
        HashMap::new()
    }

    fn process_result(&self, _output_dir: &Path) -> Result<BuilderResult, BuilderError> {
        // Generic builds produce a simple result
        Ok(BuilderResult::Generic)
    }
}

/// Debian builder implementation.
#[derive(Debug, Clone)]
pub struct DebianBuilder {
    /// Distribution configuration.
    pub distro_config: DistroConfig,
    /// APT archive location.
    pub apt_location: Option<String>,
    /// Dependency server URL.
    pub dep_server_url: Option<String>,
}

/// Distribution configuration for Debian builds.
#[derive(Debug, Clone)]
pub struct DistroConfig {
    /// Lintian profile to use.
    pub lintian_profile: String,
    /// Lintian tags to suppress.
    pub lintian_suppress_tag: Vec<String>,
}

impl DebianBuilder {
    /// Create a new Debian builder.
    pub fn new(
        distro_config: DistroConfig,
        apt_location: Option<String>,
        dep_server_url: Option<String>,
    ) -> Self {
        Self {
            distro_config,
            apt_location,
            dep_server_url,
        }
    }
}

#[async_trait]
impl Builder for DebianBuilder {
    fn kind(&self) -> &'static str {
        "debian"
    }

    async fn config(
        &self,
        _conn: &mut PgConnection,
        campaign_config: &CampaignConfig,
        queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, BuilderError> {
        let mut config = HashMap::new();

        // Lintian configuration
        config.insert(
            "lintian_profile".to_string(),
            self.distro_config.lintian_profile.clone(),
        );

        if !self.distro_config.lintian_suppress_tag.is_empty() {
            config.insert(
                "lintian_suppress_tags".to_string(),
                self.distro_config.lintian_suppress_tag.join(","),
            );
        }

        // Extra repositories
        let mut extra_repositories = Vec::new();

        if let Some(debian_config) = &campaign_config.debian_build {
            for dist in &debian_config.extra_build_distribution {
                if let Some(apt_location) = &self.apt_location {
                    extra_repositories
                        .push(format!("deb [trusted=yes] {} {} main", apt_location, dist));
                }
            }
        }

        // Add change set specific repository
        if let Some(change_set) = &queue_item.change_set {
            if let Some(apt_location) = &self.apt_location {
                extra_repositories.push(format!(
                    "deb [trusted=yes] {} cs/{} main",
                    apt_location, change_set
                ));
            }
        }

        if !extra_repositories.is_empty() {
            config.insert(
                "build_extra_repositories".to_string(),
                extra_repositories.join("\n"),
            );
        }

        if let Some(dep_server_url) = &self.dep_server_url {
            config.insert("dep_server_url".to_string(), dep_server_url.clone());
        }

        Ok(config)
    }

    async fn build_env(
        &self,
        _conn: &mut PgConnection,
        _campaign_config: &CampaignConfig,
        _queue_item: &QueueItem,
    ) -> Result<HashMap<String, String>, BuilderError> {
        let mut env = HashMap::new();

        env.insert("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string());
        env.insert("DEB_BUILD_OPTIONS".to_string(), "nocheck".to_string());

        Ok(env)
    }

    fn additional_colocated_branches(&self, main_branch: &str) -> HashMap<String, String> {
        // Implement common Debian branch patterns
        let mut branches = HashMap::new();

        // Add upstream branch if this is a packaging branch
        if main_branch.contains("debian") || main_branch == "master" || main_branch == "main" {
            branches.insert("upstream".to_string(), "upstream".to_string());
        }

        // Add pristine-tar branch for Debian packaging
        branches.insert("pristine-tar".to_string(), "pristine-tar".to_string());

        // Add vendor branches if they exist
        branches.insert("vendor".to_string(), "vendor".to_string());

        // Add experimental branches
        branches.insert("experimental".to_string(), "experimental".to_string());

        branches
    }

    fn process_result(&self, output_dir: &Path) -> Result<BuilderResult, BuilderError> {
        // Look for Debian build artifacts
        let changes_summary = match crate::find_changes(output_dir) {
            Ok(summary) => summary,
            Err(_) => {
                // No changes files found, return basic Debian result
                return Ok(BuilderResult::Debian {
                    source: None,
                    build_version: None,
                    build_distribution: None,
                    changes_filenames: None,
                    lintian_result: None,
                    binary_packages: None,
                });
            }
        };

        // Run lintian on the changes files
        let lintian_result = if !changes_summary.names.is_empty() {
            match run_lintian(
                output_dir,
                &changes_summary.names,
                Some(&self.distro_config.lintian_profile),
                if self.distro_config.lintian_suppress_tag.is_empty() {
                    None
                } else {
                    Some(&self.distro_config.lintian_suppress_tag)
                },
            ) {
                Ok(result) => {
                    // Convert to JSON value for storage
                    match serde_json::to_value(&result) {
                        Ok(json_value) => Some(json_value),
                        Err(e) => {
                            log::warn!("Failed to serialize lintian result: {}", e);
                            None
                        }
                    }
                }
                Err(e) => {
                    log::warn!("Lintian failed: {}", e);
                    None
                }
            }
        } else {
            None
        };

        Ok(BuilderResult::Debian {
            source: Some(changes_summary.source),
            build_version: Some(changes_summary.version.to_string()),
            build_distribution: Some(changes_summary.distribution),
            changes_filenames: Some(changes_summary.names),
            lintian_result,
            binary_packages: Some(changes_summary.binary_packages),
        })
    }
}

/// Get the appropriate builder for a campaign configuration.
pub fn get_builder(
    campaign_config: &CampaignConfig,
    apt_archive_url: Option<String>,
    dep_server_url: Option<String>,
) -> Result<Box<dyn Builder>, BuilderError> {
    if campaign_config.debian_build.is_some() {
        let distro_config = DistroConfig {
            lintian_profile: "debian".to_string(),
            lintian_suppress_tag: vec![],
        };

        Ok(Box::new(DebianBuilder::new(
            distro_config,
            apt_archive_url,
            dep_server_url,
        )))
    } else {
        // Default to generic builder
        Ok(Box::new(GenericBuilder::new(dep_server_url)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generic_builder_creation() {
        let builder = GenericBuilder::new(Some("http://dep.server".to_string()));
        assert_eq!(builder.kind(), "generic");
        assert_eq!(
            builder.dep_server_url,
            Some("http://dep.server".to_string())
        );
    }

    #[test]
    fn test_debian_builder_creation() {
        let distro_config = DistroConfig {
            lintian_profile: "debian".to_string(),
            lintian_suppress_tag: vec!["tag1".to_string(), "tag2".to_string()],
        };

        let builder = DebianBuilder::new(
            distro_config,
            Some("http://apt.archive".to_string()),
            Some("http://dep.server".to_string()),
        );

        assert_eq!(builder.kind(), "debian");
        assert_eq!(builder.apt_location, Some("http://apt.archive".to_string()));
        assert_eq!(
            builder.dep_server_url,
            Some("http://dep.server".to_string())
        );
    }

    #[test]
    fn test_builder_factory() {
        let mut config = CampaignConfig {
            generic_build: None,
            debian_build: None,
            force_build: false,
            default_empty: false,
        };

        // Test generic builder selection
        let builder = get_builder(&config, None, None).unwrap();
        assert_eq!(builder.kind(), "generic");

        // Test Debian builder selection
        config.debian_build = Some(DebianBuildConfig {
            base_distribution: "bullseye".to_string(),
            build_distribution: None,
            build_suffix: None,
            build_command: None,
            chroot: None,
            extra_build_distribution: vec![],
        });

        let builder = get_builder(&config, None, None).unwrap();
        assert_eq!(builder.kind(), "debian");
    }

    #[test]
    fn test_debian_builder_additional_branches() {
        let distro_config = DistroConfig {
            lintian_profile: "debian".to_string(),
            lintian_suppress_tag: vec![],
        };

        let builder = DebianBuilder::new(distro_config, None, None);

        // Test with debian branch
        let branches = builder.additional_colocated_branches("debian/master");
        assert!(branches.contains_key("upstream"));
        assert!(branches.contains_key("pristine-tar"));
        assert!(branches.contains_key("vendor"));
        assert!(branches.contains_key("experimental"));

        // Test with main branch
        let branches = builder.additional_colocated_branches("main");
        assert!(branches.contains_key("upstream"));
        assert!(branches.contains_key("pristine-tar"));

        // Test with feature branch
        let branches = builder.additional_colocated_branches("feature/some-feature");
        assert!(!branches.contains_key("upstream"));
        assert!(branches.contains_key("pristine-tar"));
    }

    #[test]
    fn test_parse_lintian_output() {
        let output_str = r#"{
   "groups" : [
      {
         "group_id" : "test-package_1.0",
         "input_files" : [
            {
               "hints" : [],
               "path" : "/tmp/test-package_1.0.dsc"
            },
            {
               "hints" : [],
               "path" : "/tmp/test-package_1.0_source.changes"
            }
         ],
         "source_name" : "test-package",
         "source_version" : "1.0"
      }
   ],
   "lintian_version" : "2.116.3"
}
OTHER BOGUS DATA
"#;
        let result = parse_lintian_output(output_str).unwrap();
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].source_name, "test-package");
        assert_eq!(result.groups[0].source_version, "1.0");
        assert_eq!(result.lintian_version, Some("2.116.3".to_string()));

        // Paths should be stripped to just filenames
        assert_eq!(
            result.groups[0].input_files[0].path,
            PathBuf::from("test-package_1.0.dsc")
        );
        assert_eq!(
            result.groups[0].input_files[1].path,
            PathBuf::from("test-package_1.0_source.changes")
        );
    }
}
