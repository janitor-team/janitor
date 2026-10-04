//! Breezyshim-backed helpers for admin-side bzr operations.
//!
//! The smart protocol (`smart_protocol.rs`) talks to Breezy directly;
//! this module covers the rest: creating a shared repository, reading
//! `last_revision`, configuring parent URLs, and running range
//! queries against the revision graph. All work runs in a
//! `tokio::task::spawn_blocking` block because `breezyshim` holds the
//! Python GIL on every call.
//!
//! Prefer the typed [`breezyshim`] crate over raw PyO3 everywhere
//! that's possible: the crate wraps error conversion, revision
//! parsing, and lock scopes so this file stays small.

use breezyshim::branch::Branch;
use breezyshim::graph::Graph;
use breezyshim::repository::Repository;
use breezyshim::revisionid::RevisionId;
use std::path::Path;
use tracing::{debug, info};

use crate::error::{BzrError, Result};

/// `breezyshim::branch::open` wants `&url::Url`, but our callers hand
/// us filesystem paths. Convert via `url::Url::from_file_path`; this
/// is fallible when `path` isn't absolute, which is what the error
/// surface here signals.
fn path_to_file_url(path: &Path) -> Result<url::Url> {
    url::Url::from_file_path(path).map_err(|_| {
        BzrError::invalid_request(format!(
            "expected absolute path for file:// URL, got {}",
            path.display()
        ))
    })
}

/// Convert a `breezyshim::error::Error` into our `BzrError`.
///
/// Specifically preserves "not a branch" as a `PathNotFound` so the
/// web layer can turn it into a 404.
fn map_err(e: breezyshim::error::Error, context: &str) -> BzrError {
    match e {
        breezyshim::error::Error::NotBranchError(path, _) => BzrError::PathNotFound { path },
        other => BzrError::Python(format!("{}: {}", context, other)),
    }
}

/// Basic branch metadata used by the admin UI.
#[derive(Debug, Clone)]
pub struct BreezyBranchInfo {
    /// Last revision identifier as returned by `Branch.last_revision()`.
    /// `None` for an empty branch (`null:` revid).
    pub last_revision: Option<String>,
}

/// One row of revision ancestry, matching the Python
/// `bzr_revision_info_request` output shape.
#[derive(Debug, Clone)]
pub struct BreezyRevisionInfo {
    /// Revision identifier as a UTF-8 string.
    pub revision_id: String,
    /// Commit message.
    pub message: String,
    /// Committer identity.
    pub committer: String,
    /// Commit timestamp as a free-form string. We format the breezyshim
    /// `f64` seconds-since-epoch as its `Debug` output rather than
    /// imposing a specific calendar format; callers that want a
    /// structured timestamp can parse it as a float.
    pub timestamp: String,
}

/// Entry points for the admin-side bzr operations.
pub struct BreezyOperations;

impl BreezyOperations {
    /// Initialize a shared repository at `path`, equivalent to
    /// `ControlDir.create(path).create_repository(shared=True)` in
    /// Python.
    pub async fn init_shared_repository(path: &Path) -> Result<()> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let controldir = breezyshim::controldir::create(path.as_path(), "2a", None)
                .map_err(|e| map_err(e, "ControlDir.create"))?;
            controldir
                .create_repository(Some(true))
                .map_err(|e| map_err(e, "create_repository(shared=True)"))?;
            info!("init_shared_repository at {}", path.display());
            Ok(())
        })
        .await
        .map_err(|e| BzrError::internal(format!("init_shared_repository join: {}", e)))?
    }

    /// Open the branch at `path` and return its `last_revision`.
    /// Returns `None` for an empty branch (`null:` revid).
    pub async fn get_branch_info(path: &Path) -> Result<BreezyBranchInfo> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<BreezyBranchInfo> {
            let url = path_to_file_url(path.as_path())?;
            let branch =
                breezyshim::branch::open_as_generic(&url).map_err(|e| map_err(e, "Branch.open"))?;
            let last = branch.last_revision();
            let last_revision = if last.is_null() {
                None
            } else {
                Some(last.as_str().to_string())
            };
            Ok(BreezyBranchInfo { last_revision })
        })
        .await
        .map_err(|e| BzrError::internal(format!("get_branch_info join: {}", e)))?
    }

    /// Walk the left-hand ancestry between `old_revid` (exclusive)
    /// and `new_revid` (inclusive). Matches the Python
    /// `bzr_revision_info_request` loop:
    ///
    /// ```python
    /// graph = repo.get_graph()
    /// for rev in repo.iter_revisions(
    ///     graph.iter_lefthand_ancestry(new_revid, [old_revid])
    /// ):
    ///     ...
    /// ```
    pub async fn get_revision_info(
        repo_path: &Path,
        old_revid: &str,
        new_revid: &str,
    ) -> Result<Vec<BreezyRevisionInfo>> {
        let repo_path = repo_path.to_path_buf();
        let old = old_revid.to_string();
        let new = new_revid.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<BreezyRevisionInfo>> {
            let repo = breezyshim::repository::open(repo_path.as_path())
                .map_err(|e| map_err(e, "Repository.open"))?;

            let new_rev = RevisionId::from(new.into_bytes());
            let old_rev = RevisionId::from(old.into_bytes());

            // Scope: lock_read keeps the ancestry walk consistent.
            // The returned Lock releases on drop.
            let _lock = repo
                .lock_read()
                .map_err(|e| map_err(e, "Repository.lock_read"))?;

            let graph: Graph = repo.get_graph();
            let ancestry_iter = graph
                .iter_lefthand_ancestry(&new_rev, Some(&[old_rev]))
                .map_err(|e| map_err(e, "Graph.iter_lefthand_ancestry"))?;

            // Collect the ancestry walk eagerly so the GIL isn't held
            // across the `iter_revisions` call that follows.
            let mut ancestry: Vec<RevisionId> = Vec::new();
            for item in ancestry_iter {
                let r = item.map_err(|e| map_err(e, "iter_lefthand_ancestry"))?;
                ancestry.push(r);
            }

            let mut out = Vec::new();
            for (_rid, maybe_rev) in repo.iter_revisions(ancestry) {
                let Some(rev) = maybe_rev else { continue };
                out.push(BreezyRevisionInfo {
                    revision_id: rev.revision_id.as_str().to_string(),
                    message: rev.message,
                    committer: rev.committer,
                    timestamp: format!("{}", rev.timestamp),
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| BzrError::internal(format!("get_revision_info join: {}", e)))?
    }

    /// Set `parent_location` on the branch at `path` via
    /// `Branch.open(...).set_parent(remote_url)`.
    pub async fn configure_remote(path: &Path, remote_url: &str) -> Result<()> {
        let path = path.to_path_buf();
        let url = remote_url.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let file_url = path_to_file_url(path.as_path())?;
            let mut branch = breezyshim::branch::open_as_generic(&file_url)
                .map_err(|e| map_err(e, "Branch.open"))?;
            branch.set_parent(&url);
            debug!("set_parent {} on {}", url, path.display());
            Ok(())
        })
        .await
        .map_err(|e| BzrError::internal(format!("configure_remote join: {}", e)))?
    }

    /// Produce a unified diff between two revisions by diffing their
    /// `RevisionTree`s. Equivalent to Python's `show_diff_trees`.
    pub async fn get_diff(path: &Path, old_revid: &str, new_revid: &str) -> Result<Vec<u8>> {
        let path = path.to_path_buf();
        let old = old_revid.to_string();
        let new = new_revid.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let repo = breezyshim::repository::open(path.as_path())
                .map_err(|e| map_err(e, "Repository.open"))?;
            let old_rev = RevisionId::from(old.into_bytes());
            let new_rev = RevisionId::from(new.into_bytes());
            let old_tree = repo
                .revision_tree(&old_rev)
                .map_err(|e| map_err(e, "Repository.revision_tree(old)"))?;
            let new_tree = repo
                .revision_tree(&new_rev)
                .map_err(|e| map_err(e, "Repository.revision_tree(new)"))?;
            let mut buf = Vec::new();
            breezyshim::diff::show_diff_trees(&old_tree, &new_tree, &mut buf, None, None)
                .map_err(|e| map_err(e, "show_diff_trees"))?;
            Ok(buf)
        })
        .await
        .map_err(|e| BzrError::internal(format!("get_diff join: {}", e)))?
    }
}
