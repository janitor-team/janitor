//! Repository management for the BZR Store service.
//!
//! Mirrors the shape of `py/janitor/bzr_store.py`: one shared bzr
//! repository per codebase at `<base>/<codebase>`, with campaign/role
//! as transport sub-paths inside that shared repo (handled by the
//! smart protocol module). The admin endpoints `diff` and
//! `revision-info` route through [`RepositoryManager`]; the Python
//! `bzr_diff_helper` and `bzr_revision_info_request` are the reference.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::database::DatabaseManager;
use crate::error::{BzrError, Result};
use crate::pyo3_bridge::BreezyOperations;

/// Repository path identifying a codebase with an optional
/// campaign/role transport sub-path within the codebase's bzr shared
/// repository.
///
/// Matches the Python `bzr_store` layout: a codebase is a single bzr
/// shared repo on disk at `<base>/<codebase>`, and campaign/role are
/// bzr transport sub-paths (branches) inside it, obtained via
/// `transport.clone(campaign_name)` / `transport.clone(role_name)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryPath {
    /// Codebase identifier — the on-disk shared repo directory name.
    pub codebase: String,
    /// Optional campaign name (transport sub-path).
    pub campaign: Option<String>,
    /// Optional role name (transport sub-path below campaign).
    pub role: Option<String>,
}

impl RepositoryPath {
    /// Create a new repository path for a codebase, with optional
    /// campaign/role.
    pub fn new(codebase: String, campaign: Option<String>, role: Option<String>) -> Self {
        Self {
            codebase,
            campaign,
            role,
        }
    }

    /// Create a repository path that refers only to the codebase repo.
    pub fn codebase_only(codebase: String) -> Self {
        Self {
            codebase,
            campaign: None,
            role: None,
        }
    }

    /// Human-readable relative path, e.g. `codebase/campaign/role`.
    pub fn relative_path(&self) -> String {
        let mut s = self.codebase.clone();
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// One row of revision metadata returned by
/// [`RepositoryManager::get_revision_info`]. Serialized as the
/// Python response shape expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Default repository manager: prefers PyO3 (via `breezyshim`) for
/// repo creation and metadata, falls back to the `brz` CLI when PyO3
/// isn't available or raises.
#[derive(Clone)]
pub struct PyO3RepositoryManager {
    base_path: PathBuf,
    #[allow(dead_code)] // reserved for future codebase-exists checks
    database: DatabaseManager,
    prefer_pyo3: bool,
}

impl PyO3RepositoryManager {
    /// Create a new repository manager rooted at `base_path`.
    pub fn new(base_path: PathBuf, database: DatabaseManager, prefer_pyo3: bool) -> Self {
        Self {
            base_path,
            database,
            prefer_pyo3,
        }
    }

    /// On-disk path for a codebase's shared bzr repository.
    pub fn get_repository_path(&self, path: &RepositoryPath) -> PathBuf {
        self.base_path.join(&path.codebase)
    }

    /// Create `<base>/<codebase>` and initialize it as a shared bzr
    /// repository if it doesn't already exist.
    async fn ensure_codebase_structure(&self, codebase: &str) -> Result<()> {
        let codebase_path = self.base_path.join(codebase);
        if codebase_path.join(".bzr").exists() {
            return Ok(());
        }
        info!(
            "Creating codebase shared repo at: {}",
            codebase_path.display()
        );
        fs::create_dir_all(&codebase_path).await?;

        if self.prefer_pyo3 {
            if let Err(e) = BreezyOperations::init_shared_repository(&codebase_path).await {
                warn!("PyO3 shared-repo init failed, falling back to brz: {}", e);
            } else {
                return Ok(());
            }
        }

        create_shared_repository_via_brz(&codebase_path).await
    }
}

async fn create_shared_repository_via_brz(path: &Path) -> Result<()> {
    let output = Command::new("brz")
        .args(["init-shared-repository", "--format=2a"])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        return Err(BzrError::subprocess(format!(
            "brz init-shared-repository failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

#[async_trait]
impl RepositoryManager for PyO3RepositoryManager {
    async fn ensure_repository(&self, path: &RepositoryPath) -> Result<PathBuf> {
        self.ensure_codebase_structure(&path.codebase).await?;
        Ok(self.get_repository_path(path))
    }

    async fn get_repository_info(&self, path: &RepositoryPath) -> Result<RepositoryInfo> {
        let repo_path = self.get_repository_path(path);
        let exists = repo_path.join(".bzr").exists();

        let last_revision = if exists && self.prefer_pyo3 {
            BreezyOperations::get_branch_info(&repo_path)
                .await
                .ok()
                .and_then(|b| b.last_revision)
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
            let repo_path = RepositoryPath::codebase_only(name);
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

        // Prefer the breezyshim-backed path: typed, no subprocess.
        if self.prefer_pyo3 {
            match BreezyOperations::get_diff(&repo_path, old_revid, new_revid).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => warn!("PyO3 diff failed, falling back to brz subprocess: {}", e),
            }
        }

        // Fallback matches Python's `bzr_diff_helper` shape: shell
        // out to brz. `brz diff` exits non-zero when differences
        // exist, so we accept exit codes 0 (no diff) and 1 (diffs).
        let output = Command::new("brz")
            .args([
                "diff",
                "-r",
                &format!("revid:{}..revid:{}", old_revid, new_revid),
            ])
            .current_dir(&repo_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let code = output.status.code();
        // bzr's `diff` exit codes: 0 = no diffs, 1 = diffs present,
        // 3 = error. Treat 0 and 1 as success.
        if matches!(code, Some(0) | Some(1)) {
            Ok(output.stdout)
        } else {
            Err(BzrError::subprocess(format!(
                "brz diff failed ({:?}): {}",
                code,
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    async fn get_revision_info(
        &self,
        path: &RepositoryPath,
        old_revid: &str,
        new_revid: &str,
    ) -> Result<Vec<RevisionInfo>> {
        let repo_path = self.get_repository_path(path);
        if self.prefer_pyo3 {
            match BreezyOperations::get_revision_info(&repo_path, old_revid, new_revid).await {
                Ok(revisions) => {
                    return Ok(revisions
                        .into_iter()
                        .map(|br| RevisionInfo {
                            revision_id: br.revision_id,
                            message: br.message,
                            committer: br.committer,
                            timestamp: br.timestamp,
                        })
                        .collect())
                }
                Err(e) => warn!("PyO3 revision-info failed, falling back to brz: {}", e),
            }
        }

        let output = Command::new("brz")
            .args([
                "log",
                "-r",
                &format!("revid:{}..revid:{}", old_revid, new_revid),
                "--show-ids",
            ])
            .current_dir(&repo_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        if !output.status.success() {
            return Err(BzrError::subprocess(format!(
                "brz log failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(parse_bzr_log(&String::from_utf8_lossy(&output.stdout)))
    }

    async fn configure_remote(&self, path: &RepositoryPath, remote_url: &str) -> Result<()> {
        let repo_path = self.get_repository_path(path);
        if !repo_path.exists() {
            return Err(BzrError::PathNotFound {
                path: path.relative_path(),
            });
        }

        if self.prefer_pyo3 {
            if let Err(e) = BreezyOperations::configure_remote(&repo_path, remote_url).await {
                warn!(
                    "PyO3 configure_remote failed, falling back to brz config: {}",
                    e
                );
            } else {
                info!(
                    "Configured remote {} = {}",
                    path.relative_path(),
                    remote_url
                );
                return Ok(());
            }
        }

        let output = Command::new("brz")
            .args(["config", &format!("parent_location={}", remote_url)])
            .current_dir(&repo_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if !output.status.success() {
            return Err(BzrError::subprocess(format!(
                "brz config failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        info!(
            "Configured remote {} = {}",
            path.relative_path(),
            remote_url
        );
        Ok(())
    }
}

/// Minimal parser for `brz log --show-ids` output. Extracts the
/// `revision-id:` and `message:` fields and drops the rest. Robust
/// enough for the admin API — the primary code path is the PyO3
/// implementation.
fn parse_bzr_log(text: &str) -> Vec<RevisionInfo> {
    let mut out = Vec::new();
    let mut current: Option<RevisionInfo> = None;
    for line in text.lines() {
        if let Some(revid) = line.strip_prefix("revision-id:") {
            if let Some(prev) = current.take() {
                out.push(prev);
            }
            current = Some(RevisionInfo {
                revision_id: revid.trim().to_string(),
                message: String::new(),
                committer: String::new(),
                timestamp: String::new(),
            });
        } else if let Some(committer) = line.strip_prefix("committer:") {
            if let Some(c) = current.as_mut() {
                c.committer = committer.trim().to_string();
            }
        } else if let Some(ts) = line.strip_prefix("timestamp:") {
            if let Some(c) = current.as_mut() {
                c.timestamp = ts.trim().to_string();
            }
        } else if let Some(msg) = line.strip_prefix("  ") {
            if let Some(c) = current.as_mut() {
                if !c.message.is_empty() {
                    c.message.push('\n');
                }
                c.message.push_str(msg);
            }
        }
    }
    if let Some(c) = current {
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_codebase_only() {
        let p = RepositoryPath::codebase_only("foo".into());
        assert_eq!(p.relative_path(), "foo");
    }

    #[test]
    fn relative_path_codebase_campaign() {
        let p = RepositoryPath::new("foo".into(), Some("c".into()), None);
        assert_eq!(p.relative_path(), "foo/c");
    }

    #[test]
    fn relative_path_full() {
        let p = RepositoryPath::new("foo".into(), Some("c".into()), Some("r".into()));
        assert_eq!(p.relative_path(), "foo/c/r");
    }

    #[test]
    fn parse_bzr_log_single_revision() {
        let text = "\
revno: 1
revision-id: alice@example.com-20240101000000-aaaaaaaaaaaaaaaa
committer: Alice <alice@example.com>
timestamp: Mon 2024-01-01 00:00:00 +0000
message:
  initial import
";
        let revs = parse_bzr_log(text);
        assert_eq!(revs.len(), 1);
        assert_eq!(
            revs[0].revision_id,
            "alice@example.com-20240101000000-aaaaaaaaaaaaaaaa"
        );
        assert_eq!(revs[0].committer, "Alice <alice@example.com>");
        assert_eq!(revs[0].timestamp, "Mon 2024-01-01 00:00:00 +0000");
        assert_eq!(revs[0].message, "initial import");
    }

    #[test]
    fn parse_bzr_log_multiple_revisions() {
        let text = "\
revno: 2
revision-id: r2
committer: X <x@example.com>
timestamp: ts2
message:
  second

revno: 1
revision-id: r1
committer: Y <y@example.com>
timestamp: ts1
message:
  first
";
        let revs = parse_bzr_log(text);
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[0].revision_id, "r2");
        assert_eq!(revs[1].revision_id, "r1");
    }
}
