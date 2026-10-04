//! Repository management functionality

use crate::error::{GitStoreError, Result};
use git2::{Repository, RepositoryInitOptions};
use std::path::{Path, PathBuf};
use tracing::info;

/// Repository manager handles Git repository operations
pub struct RepositoryManager {
    base_path: PathBuf,
}

impl RepositoryManager {
    /// Create a new repository manager
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    /// Get the path for a repository
    pub fn repo_path(&self, codebase: &str) -> PathBuf {
        self.base_path.join(codebase)
    }

    /// Open a repository, creating it if it doesn't exist
    pub fn open_or_create(&self, codebase: &str) -> Result<Repository> {
        let repo_path = self.repo_path(codebase);

        match Repository::open(&repo_path) {
            Ok(repo) => Ok(repo),
            Err(_) => {
                info!("Creating new bare repository: {}", codebase);
                self.create_bare_repository(&repo_path)
            }
        }
    }

    /// Create a new bare repository
    fn create_bare_repository(&self, path: &Path) -> Result<Repository> {
        // Create parent directories if they don't exist
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut opts = RepositoryInitOptions::new();
        opts.bare(true);
        opts.mkdir(true);

        Repository::init_opts(path, &opts).map_err(GitStoreError::from)
    }

    /// Check if a repository exists
    pub fn exists(&self, codebase: &str) -> bool {
        let repo_path = self.repo_path(codebase);
        Repository::open(&repo_path).is_ok()
    }

    /// Validate a SHA
    pub fn validate_sha(sha: &str) -> Result<()> {
        if sha.len() != 40 {
            return Err(GitStoreError::InvalidSha(
                "SHA must be 40 characters".to_string(),
            ));
        }

        if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(GitStoreError::InvalidSha(
                "SHA must contain only hexadecimal characters".to_string(),
            ));
        }

        Ok(())
    }

    /// Write remote URL + default fetch refspec.
    pub fn set_remote(&self, codebase: &str, name: &str, url: &str) -> Result<()> {
        let repo = self.open_or_create(codebase)?;

        if repo.find_remote(name).is_ok() {
            repo.remote_delete(name)?;
        }

        let fetch_refspec = format!("+refs/heads/*:refs/remotes/{}/*", name);
        repo.remote_with_fetch(name, url, &fetch_refspec)?;
        info!("Set remote '{}' to '{}' for {}", name, url, codebase);

        Ok(())
    }

    /// List every entry under `base_path`, sorted by name. Matches
    /// Python's `os.scandir(local_path)` in `handle_repo_list` -
    /// no filtering, so empty auto-created placeholders show up too.
    pub fn list_repositories(&self) -> Result<Vec<String>> {
        let mut repos = Vec::new();
        if !self.base_path.exists() {
            return Ok(repos);
        }
        for entry in std::fs::read_dir(&self.base_path)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                repos.push(name.to_string());
            }
        }
        repos.sort();
        Ok(repos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_validate_sha() {
        // Valid SHA
        assert!(
            RepositoryManager::validate_sha("1234567890abcdef1234567890abcdef12345678").is_ok()
        );

        // Too short
        assert!(RepositoryManager::validate_sha("123456").is_err());

        // Too long
        assert!(
            RepositoryManager::validate_sha("1234567890abcdef1234567890abcdef123456789").is_err()
        );

        // Invalid characters
        assert!(
            RepositoryManager::validate_sha("1234567890abcdef1234567890abcdef1234567g").is_err()
        );
    }

    #[test]
    fn test_repository_creation() {
        let temp_dir = TempDir::new().unwrap();
        let manager = RepositoryManager::new(temp_dir.path().to_path_buf());

        assert!(!manager.exists("test-repo"));

        let repo = manager.open_or_create("test-repo").unwrap();
        assert!(repo.is_bare());
        assert!(manager.exists("test-repo"));

        let _repo2 = manager.open_or_create("test-repo").unwrap();
    }

    #[test]
    fn test_list_repositories_lists_all_entries_sorted() {
        // Matches Python `os.scandir(local_path)` in handle_repo_list:
        // no filtering, every top-level entry appears, name-sorted.
        let temp_dir = TempDir::new().unwrap();
        let manager = RepositoryManager::new(temp_dir.path().to_path_buf());

        assert_eq!(manager.list_repositories().unwrap(), Vec::<String>::new());

        manager.open_or_create("gamma").unwrap();
        manager.open_or_create("alpha").unwrap();
        std::fs::create_dir(temp_dir.path().join("beta")).unwrap();

        assert_eq!(
            manager.list_repositories().unwrap(),
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
        );
    }
}
