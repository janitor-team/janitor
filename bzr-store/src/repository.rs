//! Repository management for the BZR Store service.
//!
//! Mirrors the shape of `py/janitor/bzr_store.py`: one shared bzr
//! repository per codebase at `<base>/<codebase>`, with campaign/role
//! as transport sub-paths inside that shared repo (handled by the
//! smart protocol module). The admin endpoints `diff` and
//! `revision-info` route through [`RepositoryManager`]; the Python
//! `bzr_diff_helper` and `bzr_revision_info_request` are the reference.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use breezyshim::branch::Branch;
use breezyshim::repository::Repository;
use breezyshim::revisionid::RevisionId;
use janitor_types::CodebaseName;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, info, warn};

use crate::api_types::RemoteInfo;
use crate::database::DatabaseManager;
use crate::error::{BzrError, Result};

/// Repository path identifying a codebase with an optional
/// campaign/role transport sub-path within the codebase's bzr shared
/// repository.
///
/// Matches the Python `bzr_store` layout: a codebase is a single bzr
/// shared repo on disk at `<base>/<codebase>`, and campaign/role are
/// bzr transport sub-paths (branches) inside it, obtained via
/// `transport.clone(campaign_name)` / `transport.clone(role_name)`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RepositoryPath {
    /// Codebase identifier — the on-disk shared repo directory name.
    pub codebase: CodebaseName,
    /// Optional campaign name (transport sub-path).
    pub campaign: Option<String>,
    /// Optional role name (transport sub-path below campaign).
    pub role: Option<String>,
}

impl RepositoryPath {
    /// Create a new repository path for a codebase, with optional
    /// campaign/role.
    pub fn new(codebase: CodebaseName, campaign: Option<String>, role: Option<String>) -> Self {
        Self {
            codebase,
            campaign,
            role,
        }
    }

    /// Create a repository path that refers only to the codebase repo.
    pub fn codebase_only(codebase: CodebaseName) -> Self {
        Self {
            codebase,
            campaign: None,
            role: None,
        }
    }

    /// Human-readable relative path, e.g. `codebase/campaign/role`.
    pub fn relative_path(&self) -> String {
        let mut s = self.codebase.to_string();
        if let Some(c) = &self.campaign {
            s.push('/');
            s.push_str(c);
        }
        if let Some(r) = &self.role {
            s.push('/');
            s.push_str(r);
        }
        s
    }
}

/// Repository information for the admin UI.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RepositoryInfo {
    /// Path structure identifying the repository.
    pub path: RepositoryPath,
    /// Whether the repository exists on disk.
    pub exists: bool,
    /// The most recent revision identifier, if available.
    pub last_revision: Option<String>,
    /// Number of branches in the repository (best-effort; 1 if the
    /// repo exists at all, 0 otherwise).
    pub branch_count: u32,
}

/// Repository manager trait. Lets integration tests swap in a stub
/// without pulling in `breezyshim` + a real bzr install.
#[async_trait]
pub trait RepositoryManager: Send + Sync {
    /// Ensure repository exists on disk, creating a shared bzr repo
    /// via breezy if it doesn't.
    async fn ensure_repository(&self, path: &RepositoryPath) -> Result<PathBuf>;

    /// Metadata view used by `/{codebase}/info`.
    async fn get_repository_info(&self, path: &RepositoryPath) -> Result<RepositoryInfo>;

    /// List one `RepositoryInfo` per top-level codebase directory.
    async fn list_repositories(&self) -> Result<Vec<RepositoryInfo>>;

    /// Produce a unified diff between two revids. Matches Python's
    /// `bzr_diff_helper`.
    async fn get_diff(
        &self,
        path: &RepositoryPath,
        old_revid: &str,
        new_revid: &str,
    ) -> Result<Vec<u8>>;

    /// Walk the revision ancestry between two revids. Matches
    /// Python's `bzr_revision_info_request`.
    async fn get_revision_info(
        &self,
        path: &RepositoryPath,
        old_revid: &str,
        new_revid: &str,
    ) -> Result<Vec<RevisionInfo>>;

    /// Set the configured `parent_location` for the branch at
    /// `<base>/<codebase>/<campaign>` (campaign slot). Matches
    /// Python's `handle_set_bzr_remote`.
    async fn configure_remote(&self, path: &RepositoryPath, remote_url: &str) -> Result<()>;

    /// List the parent locations of the branches directly below
    /// `<base>/<codebase>`, named after their directory.
    async fn list_remotes(&self, path: &RepositoryPath) -> Result<Vec<RemoteInfo>>;
}

/// One row of revision metadata returned by
/// [`RepositoryManager::get_revision_info`]. Serialized as the
/// Python response shape expects.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub struct RevisionInfo {
    /// Unique identifier for the revision.
    pub revision_id: String,
    /// Commit message associated with the revision.
    pub message: String,
    /// Committer identity (email + name).
    pub committer: String,
    /// Commit timestamp as a free-form string.
    pub timestamp: String,
}

/// Default repository manager, backed by `breezyshim`.
///
/// Every operation runs in `spawn_blocking`, since breezyshim holds
/// the GIL for the duration of each call.
#[derive(Clone)]
pub struct BreezyRepositoryManager {
    base_path: PathBuf,
    #[allow(dead_code)] // reserved for future codebase-exists checks
    database: DatabaseManager,
}

impl BreezyRepositoryManager {
    /// Create a new repository manager rooted at `base_path`.
    pub fn new(base_path: PathBuf, database: DatabaseManager) -> Self {
        Self {
            base_path,
            database,
        }
    }

    /// On-disk path for a codebase's shared bzr repository.
    pub fn get_repository_path(&self, path: &RepositoryPath) -> PathBuf {
        self.base_path.join(&path.codebase)
    }

    /// On-disk path of the branch at `<codebase>/<campaign>/<role>`.
    pub fn get_branch_path(&self, path: &RepositoryPath) -> Result<PathBuf> {
        branch_path(&self.base_path, path)
    }

    /// Create `<base>/<codebase>` and initialize it as a shared bzr
    /// repository if it doesn't already exist.
    async fn ensure_codebase_structure(&self, codebase: &CodebaseName) -> Result<()> {
        let codebase_path = self.base_path.join(codebase);
        if codebase_path.join(".bzr").exists() {
            return Ok(());
        }
        info!(
            "Creating codebase shared repo at: {}",
            codebase_path.display()
        );
        fs::create_dir_all(&codebase_path).await?;

        run_blocking(move || {
            let controldir = breezyshim::controldir::create(codebase_path.as_path(), "2a", None)
                .map_err(|e| breezy_error(e, "ControlDir.create"))?;
            controldir
                .create_repository(Some(true))
                .map_err(|e| breezy_error(e, "ControlDir.create_repository"))?;
            Ok(())
        })
        .await
    }
}

fn branch_path(base_path: &Path, path: &RepositoryPath) -> Result<PathBuf> {
    let mut p = base_path.join(&path.codebase);
    for component in [&path.campaign, &path.role].into_iter().flatten() {
        if component.is_empty() || component == "." || component == ".." || component.contains('/')
        {
            return Err(BzrError::invalid_request(format!(
                "invalid branch name: {:?}",
                component
            )));
        }
        p.push(component);
    }
    Ok(p)
}

/// Convert a breezy error, mapping "not a branch" to a 404.
fn breezy_error(e: breezyshim::error::Error, context: &str) -> BzrError {
    match e {
        breezyshim::error::Error::NotBranchError(path, _) => BzrError::PathNotFound { path },
        other => BzrError::Python(format!("{}: {}", context, other)),
    }
}

fn file_url(path: &Path) -> Result<url::Url> {
    url::Url::from_file_path(path).map_err(|_| {
        BzrError::internal(format!(
            "expected absolute path for file:// URL, got {}",
            path.display()
        ))
    })
}

async fn run_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| BzrError::internal(format!("breezy task failed: {}", e)))?
}

#[async_trait]
impl RepositoryManager for BreezyRepositoryManager {
    async fn ensure_repository(&self, path: &RepositoryPath) -> Result<PathBuf> {
        self.ensure_codebase_structure(&path.codebase).await?;
        Ok(self.get_repository_path(path))
    }

    async fn get_repository_info(&self, path: &RepositoryPath) -> Result<RepositoryInfo> {
        let repo_path = self.get_repository_path(path);
        let exists = repo_path.join(".bzr").exists();

        let last_revision = if exists {
            run_blocking(move || {
                let branch = match breezyshim::branch::open_as_generic(&file_url(&repo_path)?) {
                    Ok(branch) => branch,
                    // A shared repository without a branch of its own.
                    Err(breezyshim::error::Error::NotBranchError(..)) => return Ok(None),
                    Err(e) => return Err(breezy_error(e, "Branch.open")),
                };
                let last = branch.last_revision();
                Ok((!last.is_null()).then(|| last.as_str().to_string()))
            })
            .await?
        } else {
            None
        };

        Ok(RepositoryInfo {
            path: path.clone(),
            exists,
            last_revision,
            branch_count: if exists { 1 } else { 0 },
        })
    }

    async fn list_repositories(&self) -> Result<Vec<RepositoryInfo>> {
        let mut out = Vec::new();
        if !self.base_path.exists() {
            return Ok(out);
        }

        let mut entries = fs::read_dir(&self.base_path).await?;
        while let Some(e) = entries.next_entry().await? {
            if !e.file_type().await?.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let codebase = match CodebaseName::new(name) {
                Ok(c) => c,
                Err(e) => {
                    warn!("skipping repository directory: {}", e);
                    continue;
                }
            };
            let repo_path = RepositoryPath::codebase_only(codebase);
            let info = self.get_repository_info(&repo_path).await?;
            out.push(info);
        }
        Ok(out)
    }

    async fn get_diff(
        &self,
        path: &RepositoryPath,
        old_revid: &str,
        new_revid: &str,
    ) -> Result<Vec<u8>> {
        let repo_path = self.get_repository_path(path);
        debug!(
            "get_diff {} {}..{}",
            repo_path.display(),
            old_revid,
            new_revid
        );
        let old_rev = RevisionId::from(old_revid.as_bytes());
        let new_rev = RevisionId::from(new_revid.as_bytes());
        run_blocking(move || {
            let repo = breezyshim::repository::open(repo_path.as_path())
                .map_err(|e| breezy_error(e, "Repository.open"))?;
            let old_tree = repo
                .revision_tree(&old_rev)
                .map_err(|e| breezy_error(e, "Repository.revision_tree"))?;
            let new_tree = repo
                .revision_tree(&new_rev)
                .map_err(|e| breezy_error(e, "Repository.revision_tree"))?;
            let mut buf = Vec::new();
            breezyshim::diff::show_diff_trees(&old_tree, &new_tree, &mut buf, None, None)
                .map_err(|e| breezy_error(e, "show_diff_trees"))?;
            Ok(buf)
        })
        .await
    }

    async fn get_revision_info(
        &self,
        path: &RepositoryPath,
        old_revid: &str,
        new_revid: &str,
    ) -> Result<Vec<RevisionInfo>> {
        let repo_path = self.get_repository_path(path);
        let old_rev = RevisionId::from(old_revid.as_bytes());
        let new_rev = RevisionId::from(new_revid.as_bytes());
        run_blocking(move || {
            let repo = breezyshim::repository::open(repo_path.as_path())
                .map_err(|e| breezy_error(e, "Repository.open"))?;
            let _lock = repo
                .lock_read()
                .map_err(|e| breezy_error(e, "Repository.lock_read"))?;
            let ancestry = repo
                .get_graph()
                .iter_lefthand_ancestry(&new_rev, Some(&[old_rev]))
                .map_err(|e| breezy_error(e, "Graph.iter_lefthand_ancestry"))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| breezy_error(e, "Graph.iter_lefthand_ancestry"))?;
            Ok(repo
                .iter_revisions(ancestry)
                .filter_map(|(_, rev)| rev)
                .map(|rev| RevisionInfo {
                    revision_id: rev.revision_id.as_str().to_string(),
                    message: rev.message,
                    committer: rev.committer,
                    timestamp: rev.timestamp.to_string(),
                })
                .collect())
        })
        .await
    }

    async fn configure_remote(&self, path: &RepositoryPath, remote_url: &str) -> Result<()> {
        let branch_path = self.get_branch_path(path)?;
        if !branch_path.exists() {
            return Err(BzrError::PathNotFound {
                path: path.relative_path(),
            });
        }

        let url = remote_url.to_string();
        run_blocking(move || {
            let mut branch = breezyshim::branch::open_as_generic(&file_url(&branch_path)?)
                .map_err(|e| breezy_error(e, "Branch.open"))?;
            branch
                .set_parent(&url)
                .map_err(|e| breezy_error(e, "set parent location"))?;
            Ok(())
        })
        .await?;
        info!(
            "Configured remote {} = {}",
            path.relative_path(),
            remote_url
        );
        Ok(())
    }

    async fn list_remotes(&self, path: &RepositoryPath) -> Result<Vec<RemoteInfo>> {
        let repo_path = self.get_repository_path(path);
        if !repo_path.exists() {
            return Err(BzrError::PathNotFound {
                path: path.relative_path(),
            });
        }

        run_blocking(move || {
            let mut remotes = Vec::new();
            for entry in std::fs::read_dir(&repo_path)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() || entry.file_name() == ".bzr" {
                    continue;
                }
                let Ok(name) = entry.file_name().into_string() else {
                    warn!("skipping non-UTF-8 branch name {:?}", entry.file_name());
                    continue;
                };
                let branch = match breezyshim::branch::open_as_generic(&file_url(&entry.path())?) {
                    Ok(branch) => branch,
                    Err(breezyshim::error::Error::NotBranchError(..)) => continue,
                    Err(e) => return Err(breezy_error(e, "Branch.open")),
                };
                if let Some(url) = branch.get_parent() {
                    remotes.push(RemoteInfo { name, url });
                }
            }
            remotes.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(remotes)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_codebase_only() {
        let p = RepositoryPath::codebase_only(CodebaseName::new("foo").unwrap());
        assert_eq!(p.relative_path(), "foo");
    }

    #[test]
    fn relative_path_codebase_campaign() {
        let p = RepositoryPath::new(CodebaseName::new("foo").unwrap(), Some("c".into()), None);
        assert_eq!(p.relative_path(), "foo/c");
    }

    #[test]
    fn relative_path_full() {
        let p = RepositoryPath::new(
            CodebaseName::new("foo").unwrap(),
            Some("c".into()),
            Some("r".into()),
        );
        assert_eq!(p.relative_path(), "foo/c/r");
    }

    #[test]
    fn branch_path_full() {
        let p = RepositoryPath::new(
            CodebaseName::new("foo").unwrap(),
            Some("c".into()),
            Some("r".into()),
        );
        assert_eq!(
            branch_path(Path::new("/base"), &p).unwrap(),
            PathBuf::from("/base/foo/c/r")
        );
    }

    #[test]
    fn branch_path_rejects_traversal() {
        for name in ["", ".", "..", "../bar", "a/b"] {
            let p = RepositoryPath::new(CodebaseName::new("foo").unwrap(), Some(name.into()), None);
            assert!(
                matches!(
                    branch_path(Path::new("/base"), &p),
                    Err(BzrError::InvalidRequest { .. })
                ),
                "{:?} accepted",
                name
            );
        }
    }
}
