//! Repository management functionality

use crate::error::{GitStoreError, Result};
use git2::{Repository, RepositoryInitOptions};
use janitor_types::CodebaseName;
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
    pub fn repo_path(&self, codebase: &CodebaseName) -> PathBuf {
        self.base_path.join(codebase)
    }

    /// Open a repository, creating it if it doesn't exist
    pub fn open_or_create(&self, codebase: &CodebaseName) -> Result<Repository> {
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
        // The control campaign's unchanged upstream branch.
        opts.initial_head("refs/heads/control/main");

        Repository::init_opts(path, &opts).map_err(GitStoreError::from)
    }

    /// Check if a repository exists
    pub fn exists(&self, codebase: &CodebaseName) -> bool {
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
    pub fn set_remote(&self, codebase: &CodebaseName, name: &str, url: &str) -> Result<()> {
        let repo = self.open_or_create(codebase)?;

        if repo.find_remote(name).is_ok() {
            repo.remote_delete(name)?;
        }

        let fetch_refspec = format!("+refs/heads/*:refs/remotes/{}/*", name);
        repo.remote_with_fetch(name, url, &fetch_refspec)?;
        info!("Set remote '{}' to '{}' for {}", name, url, codebase);

        Ok(())
    }

    /// List codebases that have at least one ref.
    ///
    /// Filtering matters: a bare repo is auto-created the first time
    /// anything hits `/info/refs` for a codebase, so listing every
    /// directory entry reports placeholders that were never pushed to,
    /// alongside any plain directory that happens to sit in the VCS
    /// path. Both pollute the index (salsa #114).
    pub fn list_repositories(&self) -> Result<Vec<String>> {
        let mut repos = Vec::new();
        if !self.base_path.exists() {
            return Ok(repos);
        }
        for entry in std::fs::read_dir(&self.base_path)? {
            let entry = entry?;
            let name = match entry.file_name().to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            let path = entry.path();
            let repo = match Repository::open_bare(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let has_ref = match repo.references() {
                Ok(mut refs) => refs.next().is_some(),
                Err(e) => {
                    tracing::warn!("failed to enumerate refs for {}: {}", name, e);
                    continue;
                }
            };
            if has_ref {
                repos.push(name);
            } else {
                tracing::debug!("skipping empty repo {}: no refs", name);
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

        let codebase = CodebaseName::new("test-repo").unwrap();
        assert!(!manager.exists(&codebase));

        let repo = manager.open_or_create(&codebase).unwrap();
        assert!(repo.is_bare());
        let head = repo.find_reference("HEAD").unwrap();
        assert_eq!(head.symbolic_target(), Some("refs/heads/control/main"));
        assert!(manager.exists(&codebase));

        let _repo2 = manager.open_or_create(&codebase).unwrap();
    }

    /// Only repositories with refs are listed: a bare repo is
    /// auto-created on the first `/info/refs` hit, so an unfiltered
    /// listing would include placeholders nobody ever pushed to (salsa
    /// #114). Sorting is asserted here; the filtering itself is
    /// covered by the two tests below.
    #[test]
    fn test_list_repositories_sorts_populated_repos() {
        let temp_dir = TempDir::new().unwrap();
        let manager = RepositoryManager::new(temp_dir.path().to_path_buf());

        assert_eq!(manager.list_repositories().unwrap(), Vec::<String>::new());

        let gamma = manager
            .open_or_create(&CodebaseName::new("gamma").unwrap())
            .unwrap();
        let alpha = manager
            .open_or_create(&CodebaseName::new("alpha").unwrap())
            .unwrap();
        populate_bare_repo(&gamma);
        populate_bare_repo(&alpha);
        // A plain directory, which is not a repository.
        std::fs::create_dir(temp_dir.path().join("beta")).unwrap();

        assert_eq!(
            manager.list_repositories().unwrap(),
            vec!["alpha".to_string(), "gamma".to_string()]
        );
    }

    fn populate_bare_repo(repo: &git2::Repository) {
        let sig = git2::Signature::now("test", "test@example.com").unwrap();
        let tree_oid = {
            let tb = repo.treebuilder(None).unwrap();
            tb.write().unwrap()
        };
        let tree = repo.find_tree(tree_oid).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    #[test]
    fn test_list_repositories_only_includes_initialised_repos() {
        let temp_dir = TempDir::new().unwrap();
        let manager = RepositoryManager::new(temp_dir.path().to_path_buf());

        assert_eq!(manager.list_repositories().unwrap(), Vec::<String>::new());

        // Placeholders are auto-created on the first /info/refs hit;
        // only the repo that has actually been pushed to should list.
        manager
            .open_or_create(&CodebaseName::new("placeholder-1").unwrap())
            .unwrap();
        manager
            .open_or_create(&CodebaseName::new("placeholder-2").unwrap())
            .unwrap();
        let real = manager
            .open_or_create(&CodebaseName::new("real-repo").unwrap())
            .unwrap();
        populate_bare_repo(&real);

        assert_eq!(
            manager.list_repositories().unwrap(),
            vec!["real-repo".to_string()]
        );
    }

    #[test]
    fn test_list_repositories_skips_non_git_dirs() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::create_dir(temp_dir.path().join("not-a-repo")).unwrap();

        let manager = RepositoryManager::new(temp_dir.path().to_path_buf());
        assert_eq!(manager.list_repositories().unwrap(), Vec::<String>::new());
    }
}
