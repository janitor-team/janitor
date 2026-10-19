include!(concat!(env!("OUT_DIR"), "/generated/mod.rs"));

use protobuf::text_format;
use std::fs::File;
use std::io::Read;

pub use config::{
    AptRepository, BugTracker, BugTrackerKind, Campaign, Config, DebianBuild, Distribution,
    GenericBuild, MergeProposalConfig, OAuth2Provider, Select,
};

pub fn read_file(file_path: &std::path::Path) -> Result<Config, Box<dyn std::error::Error>> {
    let mut file = File::open(file_path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    read_string(&contents)
}

pub fn read_readable<R: Read>(mut readable: R) -> Result<Config, Box<dyn std::error::Error>> {
    let mut contents = String::new();
    readable.read_to_string(&mut contents)?;

    read_string(&contents)
}

pub fn read_string(contents: &str) -> Result<Config, Box<dyn std::error::Error>> {
    Ok(text_format::parse_from_str(contents)?)
}

impl Config {
    pub fn get_distribution(&self, name: &str) -> Option<&Distribution> {
        self.distribution
            .iter()
            .find(|d| d.name.as_deref() == Some(name))
    }

    pub fn get_campaign(&self, name: &str) -> Option<&Campaign> {
        self.campaign
            .iter()
            .find(|c| c.name.as_deref() == Some(name))
    }

    pub fn find_campaign_by_branch_name(&self, branch_name: &str) -> Option<(&str, &str)> {
        for campaign in &self.campaign {
            if let Some(campaign_branch_name) = &campaign.branch_name {
                if branch_name == campaign_branch_name {
                    if let Some(name) = campaign.name.as_deref() {
                        return Some((name, "main"));
                    }
                }
            }
        }
        None
    }

    pub async fn pg_pool(&self) -> std::result::Result<sqlx::PgPool, sqlx::Error> {
        if let Some(db_location) = self.database_location.as_ref() {
            sqlx::postgres::PgPool::connect(db_location.as_str()).await
        } else {
            sqlx::postgres::PgPool::connect_with(sqlx::postgres::PgConnectOptions::new()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_read_file() {
        let config = read_file(std::path::Path::new("janitor.conf.example")).unwrap();
        assert_eq!(config.distribution.len(), 1);
        assert_eq!(config.campaign.len(), 8);
        assert_eq!(config.apt_repository.len(), 1);
    }

    #[test]
    fn test_read_string() {
        let contents = r#"distribution { name: "test" }"#;
        let config = read_string(contents).unwrap();
        assert_eq!(config.distribution.len(), 1);
        assert_eq!(config.distribution[0].name, Some("test".to_string()));
    }

    #[test]
    fn test_read_readable() {
        let contents = r#"distribution { name: "test" }"#;
        let cursor = Cursor::new(contents.as_bytes());
        let config = read_readable(cursor).unwrap();
        assert_eq!(config.distribution.len(), 1);
        assert_eq!(config.distribution[0].name, Some("test".to_string()));
    }

    #[test]
    fn test_empty_config() {
        let config = read_string("").unwrap();
        assert_eq!(config.distribution.len(), 0);
        assert_eq!(config.campaign.len(), 0);
        assert_eq!(config.apt_repository.len(), 0);
    }

    #[test]
    fn test_get_distribution() {
        let config = read_string(r#"distribution { name: "test" }"#).unwrap();
        assert_eq!(
            config.get_distribution("test").unwrap().name,
            Some("test".to_string())
        );
        assert!(config.get_distribution("test2").is_none());
    }

    #[test]
    fn test_distribution_fields() {
        let config = read_string(
            r#"
            distribution {
                name: "unstable"
                archive_mirror_uri: "http://deb.debian.org/debian"
                chroot: "unstable-amd64-sbuild"
                lintian_profile: "debian"
                build_command: "sbuild -Asv"
                vendor: "debian"
            }
            "#,
        )
        .unwrap();
        let dist = config.get_distribution("unstable").unwrap();
        assert_eq!(
            dist.archive_mirror_uri,
            Some("http://deb.debian.org/debian".to_string())
        );
        assert_eq!(dist.chroot, Some("unstable-amd64-sbuild".to_string()));
        assert_eq!(dist.lintian_profile, Some("debian".to_string()));
        assert_eq!(dist.build_command, Some("sbuild -Asv".to_string()));
        assert_eq!(dist.vendor, Some("debian".to_string()));
    }

    #[test]
    fn test_get_campaign() {
        let config = read_string(
            r#"
            campaign {
                name: "lintian-fixes"
                command: "lintian-brush"
                branch_name: "lintian-fixes"
                debian_build {
                    build_distribution: "lintian-fixes"
                    build_suffix: "jan+lint"
                    base_distribution: "unstable"
                }
            }
            "#,
        )
        .unwrap();
        let campaign = config.get_campaign("lintian-fixes").unwrap();
        assert_eq!(campaign.name, Some("lintian-fixes".to_string()));
        assert_eq!(campaign.command, Some("lintian-brush".to_string()));
        assert_eq!(campaign.branch_name, Some("lintian-fixes".to_string()));
        assert!(campaign.has_debian_build());
        let debian_build = campaign.debian_build();
        assert_eq!(debian_build.build_distribution(), "lintian-fixes");
        assert_eq!(debian_build.build_suffix(), "jan+lint");
        assert_eq!(debian_build.base_distribution(), "unstable");
    }

    #[test]
    fn test_get_campaign_not_found() {
        let config = read_string(r#"campaign { name: "foo" }"#).unwrap();
        assert!(config.get_campaign("bar").is_none());
    }

    #[test]
    fn test_find_campaign_by_branch_name() {
        let config = read_string(
            r#"
            campaign { name: "lintian-fixes" branch_name: "lintian-fixes" }
            campaign { name: "fresh-releases" branch_name: "new-upstream" }
            "#,
        )
        .unwrap();
        assert_eq!(
            config.find_campaign_by_branch_name("lintian-fixes"),
            Some(("lintian-fixes", "main"))
        );
        assert_eq!(
            config.find_campaign_by_branch_name("new-upstream"),
            Some(("fresh-releases", "main"))
        );
        assert_eq!(config.find_campaign_by_branch_name("nonexistent"), None);
    }

    #[test]
    fn test_campaign_with_bugtracker() {
        let config = read_string(
            r#"
            campaign {
                name: "lintian-fixes"
                bugtracker {
                    kind: debian
                    url: "https://bugs.debian.org/lintian-brush"
                    name: "lintian-brush"
                }
            }
            "#,
        )
        .unwrap();
        let campaign = config.get_campaign("lintian-fixes").unwrap();
        assert_eq!(campaign.bugtracker.len(), 1);
        let bt = &campaign.bugtracker[0];
        assert_eq!(
            bt.url,
            Some("https://bugs.debian.org/lintian-brush".to_string())
        );
        assert_eq!(bt.name, Some("lintian-brush".to_string()));
    }

    #[test]
    fn test_read_string_invalid() {
        assert!(read_string("this is not valid protobuf {{{").is_err());
    }

    #[test]
    fn test_example_config_campaigns() {
        let config = read_file(std::path::Path::new("janitor.conf.example")).unwrap();

        // Verify all expected campaigns exist
        for name in &[
            "lintian-fixes",
            "control",
            "fresh-releases",
            "fresh-snapshots",
            "multiarch-fixes",
            "uncommitted",
            "debianize",
            "upstream-unchanged",
        ] {
            assert!(
                config.get_campaign(name).is_some(),
                "Campaign '{}' not found",
                name
            );
        }

        // Verify find_campaign_by_branch_name works with real config
        assert_eq!(
            config.find_campaign_by_branch_name("lintian-fixes"),
            Some(("lintian-fixes", "main"))
        );
        assert_eq!(
            config.find_campaign_by_branch_name("new-upstream"),
            Some(("fresh-releases", "main"))
        );
    }
}
