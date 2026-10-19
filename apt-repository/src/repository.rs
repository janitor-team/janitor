//! Repository configuration.

use crate::{
    AptRepositoryError, Compression, HashAlgorithm, ReleaseBuilder, Result, DEFAULT_COMPRESSIONS,
    DEFAULT_HASH_ALGORITHMS,
};

/// An APT repository that can generate metadata files.
#[derive(Debug, Clone)]
pub struct Repository {
    /// Origin of the repository.
    pub origin: Option<String>,
    /// Label for the repository.
    pub label: Option<String>,
    /// Suite name.
    pub suite: String,
    /// Codename.
    pub codename: Option<String>,
    /// Version.
    pub version: Option<String>,
    /// Supported architectures.
    pub architectures: Vec<String>,
    /// Repository components.
    pub components: Vec<String>,
    /// Description.
    pub description: Option<String>,
    /// Whether packages require authentication.
    pub not_automatic: bool,
    /// Whether automatic upgrades are allowed.
    pub but_automatic_upgrades: bool,
    /// Whether by-hash is supported.
    pub acquire_by_hash: bool,
    /// Compression formats to use.
    pub compressions: Vec<Compression>,
    /// Hash algorithms to use.
    pub hash_algorithms: Vec<HashAlgorithm>,
    /// Number of files to keep in each by-hash directory when pruning
    /// it after writing new indices, or `None` to never prune.
    pub by_hash_keep: Option<usize>,
}

impl Repository {
    /// A Release builder populated with this repository's fields.
    pub fn release_builder(&self) -> ReleaseBuilder {
        let mut builder = ReleaseBuilder::new()
            .suite(&self.suite)
            .architectures(self.architectures.clone())
            .components(self.components.clone())
            .not_automatic(self.not_automatic)
            .but_automatic_upgrades(self.but_automatic_upgrades)
            .acquire_by_hash(self.acquire_by_hash);
        if let Some(origin) = &self.origin {
            builder = builder.origin(origin);
        }
        if let Some(label) = &self.label {
            builder = builder.label(label);
        }
        if let Some(codename) = &self.codename {
            builder = builder.codename(codename);
        }
        if let Some(version) = &self.version {
            builder = builder.version(version);
        }
        if let Some(description) = &self.description {
            builder = builder.description(description);
        }
        builder
    }
}

/// Builder for creating Repository instances.
#[derive(Debug, Clone)]
pub struct RepositoryBuilder {
    repository: Repository,
}

impl RepositoryBuilder {
    /// Create a new Repository builder.
    pub fn new() -> Self {
        Self {
            repository: Repository {
                origin: None,
                label: None,
                suite: "stable".to_string(),
                codename: None,
                version: None,
                architectures: vec!["amd64".to_string()],
                components: vec!["main".to_string()],
                description: None,
                not_automatic: true,
                but_automatic_upgrades: true,
                acquire_by_hash: true,
                compressions: DEFAULT_COMPRESSIONS.to_vec(),
                hash_algorithms: DEFAULT_HASH_ALGORITHMS.to_vec(),
                by_hash_keep: None,
            },
        }
    }

    /// Set the origin.
    pub fn origin<S: Into<String>>(mut self, origin: S) -> Self {
        self.repository.origin = Some(origin.into());
        self
    }

    /// Set the label.
    pub fn label<S: Into<String>>(mut self, label: S) -> Self {
        self.repository.label = Some(label.into());
        self
    }

    /// Set the suite.
    pub fn suite<S: Into<String>>(mut self, suite: S) -> Self {
        self.repository.suite = suite.into();
        self
    }

    /// Set the codename.
    pub fn codename<S: Into<String>>(mut self, codename: S) -> Self {
        self.repository.codename = Some(codename.into());
        self
    }

    /// Set the version.
    pub fn version<S: Into<String>>(mut self, version: S) -> Self {
        self.repository.version = Some(version.into());
        self
    }

    /// Set the architectures.
    pub fn architectures(mut self, architectures: Vec<String>) -> Self {
        self.repository.architectures = architectures;
        self
    }

    /// Set the components.
    pub fn components(mut self, components: Vec<String>) -> Self {
        self.repository.components = components;
        self
    }

    /// Set the description.
    pub fn description<S: Into<String>>(mut self, description: S) -> Self {
        self.repository.description = Some(description.into());
        self
    }

    /// Set not automatic flag.
    pub fn not_automatic(mut self, not_automatic: bool) -> Self {
        self.repository.not_automatic = not_automatic;
        self
    }

    /// Set but automatic upgrades flag.
    pub fn but_automatic_upgrades(mut self, but_automatic_upgrades: bool) -> Self {
        self.repository.but_automatic_upgrades = but_automatic_upgrades;
        self
    }

    /// Set acquire by hash flag.
    pub fn acquire_by_hash(mut self, acquire_by_hash: bool) -> Self {
        self.repository.acquire_by_hash = acquire_by_hash;
        self
    }

    /// Set compression formats.
    pub fn compressions(mut self, compressions: Vec<Compression>) -> Self {
        self.repository.compressions = compressions;
        self
    }

    /// Set hash algorithms.
    pub fn hash_algorithms(mut self, hash_algorithms: Vec<HashAlgorithm>) -> Self {
        self.repository.hash_algorithms = hash_algorithms;
        self
    }

    /// Prune each by-hash directory down to the `keep_count` newest
    /// files after writing new indices into it.
    pub fn by_hash_keep(mut self, keep_count: usize) -> Self {
        self.repository.by_hash_keep = Some(keep_count);
        self
    }

    /// Build the Repository.
    pub fn build(self) -> Result<Repository> {
        if self.repository.suite.is_empty() {
            return Err(AptRepositoryError::invalid_config("Suite cannot be empty"));
        }
        if self.repository.architectures.is_empty() {
            return Err(AptRepositoryError::invalid_config(
                "At least one architecture must be specified",
            ));
        }
        if self.repository.components.is_empty() {
            return Err(AptRepositoryError::invalid_config(
                "At least one component must be specified",
            ));
        }

        Ok(self.repository)
    }
}

impl Default for RepositoryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repository_builder_validation() {
        // Empty suite should fail
        let result = RepositoryBuilder::new().suite("").build();
        assert!(result.is_err());

        // Empty architectures should fail
        let result = RepositoryBuilder::new().architectures(vec![]).build();
        assert!(result.is_err());

        // Empty components should fail
        let result = RepositoryBuilder::new().components(vec![]).build();
        assert!(result.is_err());
    }
}
