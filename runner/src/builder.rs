//! Build configuration for campaigns.

/// Campaign settings needed to assemble a worker assignment.
#[derive(Debug, Clone)]
pub struct CampaignConfig {
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

impl CampaignConfig {
    /// The kind of build the worker runs for this campaign.
    pub fn builder_kind(&self) -> &'static str {
        if self.debian_build.is_some() {
            "debian"
        } else {
            "generic"
        }
    }

    /// Colocated branches the worker should fetch alongside `main_branch`,
    /// sorted so the assignment JSON is stable.
    pub fn additional_colocated_branches(&self, main_branch: &str) -> Vec<String> {
        if self.debian_build.is_none() {
            return vec![];
        }
        let mut branches = vec![];
        if main_branch.contains("debian") || main_branch == "master" || main_branch == "main" {
            branches.push("upstream".to_string());
        }
        branches.extend(["pristine-tar", "vendor", "experimental"].map(String::from));
        branches.sort();
        branches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn debian_config() -> CampaignConfig {
        CampaignConfig {
            debian_build: Some(DebianBuildConfig {
                base_distribution: "bullseye".to_string(),
                build_distribution: None,
                build_suffix: None,
                build_command: None,
                chroot: None,
                extra_build_distribution: vec![],
            }),
            force_build: false,
            default_empty: false,
        }
    }

    #[test]
    fn test_builder_kind() {
        let mut config = debian_config();
        assert_eq!(config.builder_kind(), "debian");
        config.debian_build = None;
        assert_eq!(config.builder_kind(), "generic");
    }

    #[test]
    fn test_additional_colocated_branches() {
        let config = debian_config();
        assert_eq!(
            config.additional_colocated_branches("debian/master"),
            vec!["experimental", "pristine-tar", "upstream", "vendor"]
        );
        assert_eq!(
            config.additional_colocated_branches("feature/some-feature"),
            vec!["experimental", "pristine-tar", "vendor"]
        );
        let generic = CampaignConfig {
            debian_build: None,
            ..config
        };
        assert_eq!(
            generic.additional_colocated_branches("main"),
            Vec::<String>::new()
        );
    }
}
