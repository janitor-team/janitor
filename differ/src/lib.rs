//! Differ crate for the Janitor project.

pub mod diffoscope;
pub mod error;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use error::{Error, Result};

/// Return `(filename, path)` for every entry directly under `path`.
pub fn find_binaries(path: &Path) -> std::io::Result<Vec<(OsString, PathBuf)>> {
    std::fs::read_dir(path)?
        .map(|entry| {
            let entry = entry?;
            Ok((entry.file_name(), entry.path()))
        })
        .collect()
}

/// True for Debian binary package filenames (`.deb`, `.udeb`).
pub fn is_binary(name: &str) -> bool {
    name.ends_with(".deb") || name.ends_with(".udeb")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn is_binary_matches_deb_and_udeb() {
        assert!(is_binary("pkg_1.0_amd64.deb"));
        assert!(is_binary("pkg_1.0_amd64.udeb"));
        assert!(!is_binary("pkg_1.0.dsc"));
        assert!(!is_binary("pkg_1.0.tar.gz"));
        assert!(!is_binary("pkg_1.0.changes"));
    }

    #[test]
    fn find_binaries_lists_directory() {
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join("pkg.deb"), b"").unwrap();
        std::fs::write(td.path().join("pkg.dsc"), b"").unwrap();
        std::fs::write(td.path().join("pkg.udeb"), b"").unwrap();

        let mut names: Vec<String> = find_binaries(td.path())
            .unwrap()
            .into_iter()
            .map(|(n, _)| n.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["pkg.deb", "pkg.dsc", "pkg.udeb"]);
    }

    #[test]
    fn find_binaries_errors_on_missing_dir() {
        assert!(find_binaries(Path::new("/no/such/path/should/exist")).is_err());
    }
}
