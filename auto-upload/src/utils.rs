//! Utility functions for the auto-upload service

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::debug;

use crate::error::{Result, UploadError};

fn is_target_changes(path: &Path, source_only: bool) -> bool {
    if path.extension() != Some(OsStr::new("changes")) {
        return false;
    }
    if !source_only {
        return true;
    }
    path.file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.ends_with("_source"))
}

/// Find all `.changes` files in `dir`, optionally restricting to source-only.
pub async fn find_changes_files(dir: &Path, source_only: bool) -> Result<Vec<PathBuf>> {
    let mut changes_files = Vec::new();
    let mut entries = fs::read_dir(dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if is_target_changes(&path, source_only) {
            changes_files.push(path);
        }
    }

    if changes_files.is_empty() {
        return Err(UploadError::NoChangesFiles);
    }

    Ok(changes_files)
}

/// Relax entry permissions in `dir` to `0o644 & !umask`.
///
/// Works around https://bugs.debian.org/389908: gpg-agent-invoked signing
/// tools refuse to touch files that aren't group/world-readable.
pub async fn fix_file_permissions(dir: &Path) -> Result<()> {
    let umask = current_umask();
    let new_mode = 0o644 & !umask;

    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let mut permissions = entry.metadata().await?.permissions();
        permissions.set_mode(new_mode);
        fs::set_permissions(&path, permissions).await?;
        debug!("set permissions on {} to {:o}", path.display(), new_mode);
    }

    Ok(())
}

/// Read the process umask without altering it (by round-tripping through `umask(2)`).
fn current_umask() -> u32 {
    use nix::sys::stat::{umask, Mode};
    let prev = umask(Mode::empty());
    umask(prev);
    prev.bits() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn find_changes_files_lists_all_or_source_only() {
        let temp_dir = TempDir::new().unwrap();
        let dir_path = temp_dir.path();

        // Create test files
        fs::write(dir_path.join("test_1.0-1_amd64.changes"), "")
            .await
            .unwrap();
        fs::write(dir_path.join("test_1.0-1_source.changes"), "")
            .await
            .unwrap();
        fs::write(dir_path.join("test_1.0-1.dsc"), "")
            .await
            .unwrap();

        // Test finding all changes files
        let changes_files = find_changes_files(dir_path, false).await.unwrap();
        assert_eq!(changes_files.len(), 2);

        // Test finding only source changes
        let source_changes = find_changes_files(dir_path, true).await.unwrap();
        assert_eq!(source_changes.len(), 1);
        assert!(source_changes[0]
            .to_string_lossy()
            .ends_with("_source.changes"));
    }

    #[tokio::test]
    async fn find_changes_files_returns_no_changes_when_dir_has_no_changes() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("foo.dsc"), "")
            .await
            .unwrap();

        let err = find_changes_files(temp_dir.path(), false)
            .await
            .unwrap_err();
        assert!(matches!(err, UploadError::NoChangesFiles));
    }

    #[tokio::test]
    async fn find_changes_files_source_only_skips_binary_changes() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("foo_1.0-1_amd64.changes"), "")
            .await
            .unwrap();

        let err = find_changes_files(temp_dir.path(), true).await.unwrap_err();
        assert!(matches!(err, UploadError::NoChangesFiles));
    }

    #[tokio::test]
    async fn find_changes_files_missing_dir_is_io_error() {
        let temp_dir = TempDir::new().unwrap();
        let missing = temp_dir.path().join("does-not-exist");

        let err = find_changes_files(&missing, false).await.unwrap_err();
        assert!(matches!(err, UploadError::Io(_)));
    }

    #[tokio::test]
    async fn fix_file_permissions_relaxes_group_and_world_read() {
        let temp_dir = TempDir::new().unwrap();
        let file = temp_dir.path().join("foo.changes");
        fs::write(&file, b"body").await.unwrap();

        // Start with owner-only 0o600, and observe what fix_file_permissions
        // computes given the current process umask.
        fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();

        let expected = 0o644 & !current_umask();

        fix_file_permissions(temp_dir.path()).await.unwrap();

        let mode = fs::metadata(&file).await.unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, expected);
    }
}
