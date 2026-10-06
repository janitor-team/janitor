//! Release file generation for APT repositories.

use crate::{HashAlgorithm, HashedFile, Result};
use chrono::{DateTime, Utc};
use debian_control::fields::{Md5Checksum, Sha1Checksum, Sha256Checksum, Sha512Checksum};
use std::fmt;

/// A Release file for an APT repository.
#[derive(Clone)]
pub struct Release {
    inner: debian_control::lossy::apt::Release,
    files: Vec<HashedFile>,
}

impl Release {
    /// Create a new Release.
    pub fn new() -> Self {
        Self {
            inner: debian_control::lossy::apt::Release {
                codename: None,
                components: vec![],
                architectures: vec![],
                description: None,
                origin: None,
                label: None,
                suite: None,
                version: None,
                date: None,
                not_automatic: None,
                but_automatic_upgrades: None,
                acquire_by_hash: None,
                checksums_md5: None,
                checksums_sha1: None,
                checksums_sha256: None,
                checksums_sha512: None,
            },
            files: Vec::new(),
        }
    }

    /// Add a file to the release.
    pub fn add_file(&mut self, file: HashedFile) {
        self.files.push(file);
    }

    /// Get all files.
    pub fn files(&self) -> &[HashedFile] {
        &self.files
    }

    /// Get the origin.
    pub fn origin(&self) -> Option<String> {
        self.inner.origin.clone()
    }

    /// Get the label.
    pub fn label(&self) -> Option<String> {
        self.inner.label.clone()
    }

    /// Get the suite.
    pub fn suite(&self) -> Option<String> {
        self.inner.suite.clone()
    }

    /// Get the codename.
    pub fn codename(&self) -> Option<String> {
        self.inner.codename.clone()
    }

    /// Get the version.
    pub fn version(&self) -> Option<String> {
        self.inner.version.clone()
    }

    /// Get the architectures.
    pub fn architectures(&self) -> Option<Vec<String>> {
        if self.inner.architectures.is_empty() {
            None
        } else {
            Some(self.inner.architectures.clone())
        }
    }

    /// Get the components.
    pub fn components(&self) -> Option<Vec<String>> {
        if self.inner.components.is_empty() {
            None
        } else {
            Some(self.inner.components.clone())
        }
    }

    /// Get the description.
    pub fn description(&self) -> Option<String> {
        self.inner.description.clone()
    }

    /// Get the not_automatic flag.
    pub fn not_automatic(&self) -> Option<bool> {
        self.inner.not_automatic
    }

    /// Get the but_automatic_upgrades flag.
    pub fn but_automatic_upgrades(&self) -> Option<bool> {
        self.inner.but_automatic_upgrades
    }

    /// Get the acquire_by_hash flag.
    pub fn acquire_by_hash(&self) -> bool {
        self.inner.acquire_by_hash.unwrap_or(false)
    }

    /// Parse a Release file from a string.
    pub fn from_str(content: &str) -> Result<Self> {
        let inner = content
            .parse::<debian_control::lossy::apt::Release>()
            .map_err(|e| crate::AptRepositoryError::invalid_config(e))?;

        let mut files: Vec<HashedFile> = Vec::new();

        // Extract md5 checksums
        for c in inner.checksums_md5.iter().flatten() {
            merge_hash(
                &mut files,
                &c.filename,
                c.size as u64,
                HashAlgorithm::Md5,
                c.md5sum.clone(),
            );
        }

        // Extract sha1 checksums
        for c in inner.checksums_sha1.iter().flatten() {
            merge_hash(
                &mut files,
                &c.filename,
                c.size as u64,
                HashAlgorithm::Sha1,
                c.sha1.clone(),
            );
        }

        // Extract sha256 checksums
        for c in inner.checksums_sha256.iter().flatten() {
            merge_hash(
                &mut files,
                &c.filename,
                c.size as u64,
                HashAlgorithm::Sha256,
                c.sha256.clone(),
            );
        }

        // Extract sha512 checksums
        for c in inner.checksums_sha512.iter().flatten() {
            merge_hash(
                &mut files,
                &c.filename,
                c.size as u64,
                HashAlgorithm::Sha512,
                c.sha512.clone(),
            );
        }

        Ok(Self { inner, files })
    }

    /// The underlying Release with the checksum lists filled in from
    /// `self.files`.
    fn with_checksums(&self) -> debian_control::lossy::apt::Release {
        let mut inner = self.inner.clone();

        // Build checksum lists from self.files
        let md5: Vec<Md5Checksum> = self
            .files
            .iter()
            .filter_map(|f| {
                f.get_hash(&HashAlgorithm::Md5).map(|h| Md5Checksum {
                    md5sum: h.to_string(),
                    size: f.size as usize,
                    filename: f.path.clone(),
                })
            })
            .collect();
        if !md5.is_empty() {
            inner.checksums_md5 = Some(md5);
        }

        let sha1: Vec<Sha1Checksum> = self
            .files
            .iter()
            .filter_map(|f| {
                f.get_hash(&HashAlgorithm::Sha1).map(|h| Sha1Checksum {
                    sha1: h.to_string(),
                    size: f.size as usize,
                    filename: f.path.clone(),
                })
            })
            .collect();
        if !sha1.is_empty() {
            inner.checksums_sha1 = Some(sha1);
        }

        let sha256: Vec<Sha256Checksum> = self
            .files
            .iter()
            .filter_map(|f| {
                f.get_hash(&HashAlgorithm::Sha256).map(|h| Sha256Checksum {
                    sha256: h.to_string(),
                    size: f.size as usize,
                    filename: f.path.clone(),
                })
            })
            .collect();
        if !sha256.is_empty() {
            inner.checksums_sha256 = Some(sha256);
        }

        let sha512: Vec<Sha512Checksum> = self
            .files
            .iter()
            .filter_map(|f| {
                f.get_hash(&HashAlgorithm::Sha512).map(|h| Sha512Checksum {
                    sha512: h.to_string(),
                    size: f.size as usize,
                    filename: f.path.clone(),
                })
            })
            .collect();
        if !sha512.is_empty() {
            inner.checksums_sha512 = Some(sha512);
        }

        inner
    }
}

fn merge_hash(
    files: &mut Vec<HashedFile>,
    path: &str,
    size: u64,
    algorithm: HashAlgorithm,
    hash: String,
) {
    if let Some(f) = files.iter_mut().find(|f| f.path == path) {
        f.add_hash(algorithm, hash);
    } else {
        let mut f = HashedFile::new(path.to_string(), size);
        f.add_hash(algorithm, hash);
        files.push(f);
    }
}

impl Default for Release {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Release")
            .field("suite", &self.suite())
            .field("codename", &self.codename())
            .field("files", &self.files)
            .finish()
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.with_checksums())
    }
}

/// Builder for creating Release files.
#[derive(Clone)]
pub struct ReleaseBuilder {
    inner: debian_control::lossy::apt::Release,
    date: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
    files: Vec<HashedFile>,
}

impl ReleaseBuilder {
    /// Create a new Release builder.
    pub fn new() -> Self {
        Self {
            inner: debian_control::lossy::apt::Release {
                codename: None,
                components: vec![],
                architectures: vec![],
                description: None,
                origin: None,
                label: None,
                suite: None,
                version: None,
                date: None,
                not_automatic: None,
                but_automatic_upgrades: None,
                acquire_by_hash: None,
                checksums_md5: None,
                checksums_sha1: None,
                checksums_sha256: None,
                checksums_sha512: None,
            },
            date: None,
            valid_until: None,
            files: Vec::new(),
        }
    }

    /// Set the origin.
    pub fn origin<S: AsRef<str>>(mut self, origin: S) -> Self {
        self.inner.origin = Some(origin.as_ref().to_string());
        self
    }

    /// Set the label.
    pub fn label<S: AsRef<str>>(mut self, label: S) -> Self {
        self.inner.label = Some(label.as_ref().to_string());
        self
    }

    /// Set the suite.
    pub fn suite<S: AsRef<str>>(mut self, suite: S) -> Self {
        self.inner.suite = Some(suite.as_ref().to_string());
        self
    }

    /// Set the codename.
    pub fn codename<S: AsRef<str>>(mut self, codename: S) -> Self {
        self.inner.codename = Some(codename.as_ref().to_string());
        self
    }

    /// Set the version.
    pub fn version<S: AsRef<str>>(mut self, version: S) -> Self {
        self.inner.version = Some(version.as_ref().to_string());
        self
    }

    /// Set the date.
    pub fn date(mut self, date: DateTime<Utc>) -> Self {
        self.date = Some(date);
        self
    }

    /// Set the valid until date.
    pub fn valid_until(mut self, valid_until: DateTime<Utc>) -> Self {
        self.valid_until = Some(valid_until);
        self
    }

    /// Set the architectures.
    pub fn architectures(mut self, architectures: Vec<String>) -> Self {
        self.inner.architectures = architectures;
        self
    }

    /// Set the components.
    pub fn components(mut self, components: Vec<String>) -> Self {
        self.inner.components = components;
        self
    }

    /// Set the description.
    pub fn description<S: AsRef<str>>(mut self, description: S) -> Self {
        self.inner.description = Some(description.as_ref().to_string());
        self
    }

    /// Set not automatic flag.
    pub fn not_automatic(mut self, not_automatic: bool) -> Self {
        self.inner.not_automatic = Some(not_automatic);
        self
    }

    /// Set but automatic upgrades flag.
    pub fn but_automatic_upgrades(mut self, but_automatic_upgrades: bool) -> Self {
        self.inner.but_automatic_upgrades = Some(but_automatic_upgrades);
        self
    }

    /// Set acquire by hash flag.
    pub fn acquire_by_hash(mut self, acquire_by_hash: bool) -> Self {
        self.inner.acquire_by_hash = Some(acquire_by_hash);
        self
    }

    /// Add a file.
    pub fn add_file(mut self, file: HashedFile) -> Self {
        self.files.push(file);
        self
    }

    /// Build the Release.
    pub fn build(mut self) -> Result<Release> {
        let date = self.date.unwrap_or_else(Utc::now);
        self.inner.date = Some(date.format("%a, %d %b %Y %H:%M:%S GMT").to_string());
        if let Some(valid_until) = self.valid_until {
            // TODO: store valid_until in lossy Release when that field is added
            let _ = valid_until;
        }
        Ok(Release {
            inner: self.inner,
            files: self.files,
        })
    }
}

impl Default for ReleaseBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HashAlgorithm;

    #[test]
    fn test_release_builder() {
        let release = ReleaseBuilder::new()
            .origin("Test Origin")
            .label("Test Repository")
            .suite("stable")
            .codename("stable")
            .architectures(vec!["amd64".to_string(), "i386".to_string()])
            .components(vec!["main".to_string()])
            .description("Test repository")
            .not_automatic(true)
            .acquire_by_hash(true)
            .build()
            .unwrap();

        assert_eq!(release.origin(), Some("Test Origin".to_string()));
        assert_eq!(release.label(), Some("Test Repository".to_string()));
        assert_eq!(release.suite(), Some("stable".to_string()));
        assert_eq!(release.codename(), Some("stable".to_string()));
        assert_eq!(
            release.architectures(),
            Some(vec!["amd64".to_string(), "i386".to_string()])
        );
        assert_eq!(release.components(), Some(vec!["main".to_string()]));
        assert_eq!(release.not_automatic(), Some(true));
        assert_eq!(release.acquire_by_hash(), true);
    }

    #[test]
    fn test_release_date_uses_gmt() {
        let date = DateTime::parse_from_rfc3339("2024-03-05T07:08:09Z")
            .unwrap()
            .with_timezone(&Utc);
        let release = ReleaseBuilder::new()
            .suite("stable")
            .date(date)
            .build()
            .unwrap();
        assert_eq!(
            release.inner.date.as_deref(),
            Some("Tue, 05 Mar 2024 07:08:09 GMT")
        );
    }

    #[test]
    fn test_release_roundtrip() {
        let mut release = Release::new();
        release.inner.origin = Some("Test Origin".to_string());
        release.inner.suite = Some("stable".to_string());
        release.inner.architectures = vec!["amd64".to_string()];
        release.inner.components = vec!["main".to_string()];
        release.inner.date = Some(Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string());

        let mut file = HashedFile::new("main/binary-amd64/Packages", 1024);
        file.add_hash(HashAlgorithm::Md5, "abc123".to_string());
        file.add_hash(HashAlgorithm::Sha256, "def456".to_string());
        release.add_file(file);

        let content = release.to_string();
        let parsed = Release::from_str(&content).unwrap();

        assert_eq!(release.origin(), parsed.origin());
        assert_eq!(release.suite(), parsed.suite());
        assert_eq!(release.architectures(), parsed.architectures());
        assert_eq!(release.components(), parsed.components());
        assert_eq!(release.files().len(), parsed.files().len());

        if !release.files().is_empty() {
            let orig_file = &release.files()[0];
            let parsed_file = &parsed.files()[0];
            assert_eq!(orig_file.path, parsed_file.path);
            assert_eq!(orig_file.size, parsed_file.size);
            assert_eq!(
                orig_file.get_hash(&HashAlgorithm::Md5),
                parsed_file.get_hash(&HashAlgorithm::Md5)
            );
        }
    }
}
