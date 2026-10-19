//! Source package file parsing and generation for APT repositories.

use crate::Result;
use std::fmt;

pub use debian_control::lossy::apt::Source;

/// Create a new source package with required fields.
pub fn new_source(package: &str, version: &str, architecture: &str, directory: &str) -> Source {
    Source {
        package: package.to_string(),
        version: version.parse().expect("valid version"),
        architecture: Some(architecture.to_string()),
        directory: directory.to_string(),
        description: None,
        binaries: None,
        maintainer: None,
        uploaders: None,
        build_depends: None,
        build_depends_indep: None,
        build_depends_arch: None,
        build_conflicts: None,
        build_conflicts_indep: None,
        build_conflicts_arch: None,
        standards_version: None,
        homepage: None,
        autobuild: None,
        extra_source_only: None,
        testsuite: None,
        testsuite_triggers: None,
        vcs_browser: None,
        vcs_git: None,
        vcs_bzr: None,
        vcs_hg: None,
        vcs_svn: None,
        vcs_darcs: None,
        vcs_cvs: None,
        vcs_arch: None,
        vcs_mtn: None,
        dgit: None,
        priority: None,
        section: None,
        format: None,
        package_list: None,
        files: None,
        checksums_sha1: None,
        checksums_sha256: None,
        checksums_sha512: None,
    }
}

/// A collection of source packages that can be written to a Sources file.
#[derive(Debug, Clone)]
pub struct SourceFile {
    sources: Vec<Source>,
}

impl SourceFile {
    /// Create a new empty source file.
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    /// Add a source package to the file.
    pub fn add_source(&mut self, source: Source) {
        self.sources.push(source);
    }

    /// Get all source packages.
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// Parse a Sources file from a string.
    pub fn from_str(content: &str) -> Result<Self> {
        use deb822_fast::{Deb822, FromDeb822Paragraph};
        let deb822: Deb822 = content.parse().map_err(|e: deb822_fast::Error| {
            crate::AptRepositoryError::invalid_source(e.to_string())
        })?;
        let sources = deb822
            .iter()
            .map(|p| {
                FromDeb822Paragraph::from_paragraph(p)
                    .map_err(crate::AptRepositoryError::invalid_source)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { sources })
    }

    /// Get the number of source packages.
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Check if the source file is empty.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Sort source packages by name and version.
    pub fn sort(&mut self) {
        self.sources.sort_by(|a, b| {
            a.package
                .cmp(&b.package)
                .then_with(|| a.version.cmp(&b.version))
        });
    }
}

impl Default for SourceFile {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SourceFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, source) in self.sources.iter().enumerate() {
            if i > 0 {
                write!(f, "\n")?;
            }
            write!(f, "{}", source)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_creation() {
        let source = new_source("test-package", "1.0.0", "any", "pool/main/t/test");

        assert_eq!(source.package, "test-package");
        assert_eq!(source.version.to_string(), "1.0.0");
        assert_eq!(source.architecture, Some("any".to_string()));
        assert_eq!(source.directory, "pool/main/t/test");
    }

    #[test]
    fn test_source_file() {
        let mut source_file = SourceFile::new();
        assert!(source_file.is_empty());
        assert_eq!(source_file.len(), 0);

        let source1 = new_source("package-a", "1.0.0", "any", "pool/main/a/package-a");
        let source2 = new_source("package-b", "2.0.0", "any", "pool/main/b/package-b");

        source_file.add_source(source1);
        source_file.add_source(source2);

        assert!(!source_file.is_empty());
        assert_eq!(source_file.len(), 2);

        let content = source_file.to_string();
        let parsed = SourceFile::from_str(&content).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.sources()[0].package, "package-a");
        assert_eq!(parsed.sources()[1].package, "package-b");
    }
}
