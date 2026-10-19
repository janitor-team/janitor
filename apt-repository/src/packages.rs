//! Package file parsing and generation for APT repositories.

use crate::Result;
use std::fmt;

pub use debian_control::lossy::apt::Package;

/// Create a new package with required fields.
pub fn new_package(
    name: &str,
    version: &str,
    architecture: &str,
    filename: &str,
    size: u64,
) -> Package {
    Package {
        name: name.to_string(),
        version: version.parse().expect("valid version"),
        architecture: architecture.to_string(),
        filename: Some(filename.to_string()),
        size: Some(size as usize),
        source: None,
        maintainer: None,
        installed_size: None,
        depends: None,
        pre_depends: None,
        recommends: None,
        suggests: None,
        enhances: None,
        breaks: None,
        conflicts: None,
        provides: None,
        replaces: None,
        built_using: None,
        static_built_using: None,
        description: None,
        homepage: None,
        origin: None,
        priority: None,
        section: None,
        essential: None,
        multi_arch: None,
        tag: None,
        task: None,
        md5sum: None,
        sha1: None,
        sha256: None,
        sha512: None,
        description_md5: None,
    }
}

/// A collection of packages that can be written to a Packages file.
#[derive(Debug, Clone)]
pub struct PackageFile {
    packages: Vec<Package>,
}

impl PackageFile {
    /// Create a new empty package file.
    pub fn new() -> Self {
        Self {
            packages: Vec::new(),
        }
    }

    /// Add a package to the file.
    pub fn add_package(&mut self, package: Package) {
        self.packages.push(package);
    }

    /// Get all packages.
    pub fn packages(&self) -> &[Package] {
        &self.packages
    }

    /// Parse a Packages file from a string.
    pub fn from_str(content: &str) -> Result<Self> {
        use deb822_fast::{Deb822, FromDeb822Paragraph};
        let deb822: Deb822 = content.parse().map_err(|e: deb822_fast::Error| {
            crate::AptRepositoryError::invalid_package(e.to_string())
        })?;
        let packages = deb822
            .iter()
            .map(|p| {
                FromDeb822Paragraph::from_paragraph(p)
                    .map_err(crate::AptRepositoryError::invalid_package)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { packages })
    }

    /// Get the number of packages.
    pub fn len(&self) -> usize {
        self.packages.len()
    }

    /// Check if the package file is empty.
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    /// Sort packages by name and version.
    pub fn sort(&mut self) {
        self.packages
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    }
}

impl Default for PackageFile {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PackageFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, package) in self.packages.iter().enumerate() {
            if i > 0 {
                write!(f, "\n")?;
            }
            write!(f, "{}", package)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_creation() {
        let package = new_package(
            "test-package",
            "1.0.0",
            "amd64",
            "pool/main/t/test_1.0.0_amd64.deb",
            1024,
        );

        assert_eq!(package.name, "test-package");
        assert_eq!(package.version.to_string(), "1.0.0");
        assert_eq!(package.architecture, "amd64");
        assert_eq!(
            package.filename,
            Some("pool/main/t/test_1.0.0_amd64.deb".to_string())
        );
        assert_eq!(package.size, Some(1024));
    }

    #[test]
    fn test_package_file() {
        let mut package_file = PackageFile::new();
        assert!(package_file.is_empty());
        assert_eq!(package_file.len(), 0);

        let package1 = new_package("package-a", "1.0.0", "amd64", "a.deb", 1024);
        let package2 = new_package("package-b", "2.0.0", "amd64", "b.deb", 2048);

        package_file.add_package(package1);
        package_file.add_package(package2);

        assert!(!package_file.is_empty());
        assert_eq!(package_file.len(), 2);

        let content = package_file.to_string();
        let parsed = PackageFile::from_str(&content).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.packages()[0].name, "package-a");
        assert_eq!(parsed.packages()[1].name, "package-b");
    }

    #[test]
    fn test_package_sorting() {
        let mut package_file = PackageFile::new();

        let package1 = new_package("zpackage", "1.0.0", "amd64", "z.deb", 1024);
        let package2 = new_package("apackage", "2.0.0", "amd64", "a.deb", 2048);
        let package3 = new_package("apackage", "1.0.0", "amd64", "a1.deb", 1024);

        package_file.add_package(package1);
        package_file.add_package(package2);
        package_file.add_package(package3);

        package_file.sort();

        let packages = package_file.packages();
        assert_eq!(packages[0].name, "apackage");
        assert_eq!(packages[0].version.to_string(), "1.0.0");
        assert_eq!(packages[1].name, "apackage");
        assert_eq!(packages[1].version.to_string(), "2.0.0");
        assert_eq!(packages[2].name, "zpackage");
    }
}
