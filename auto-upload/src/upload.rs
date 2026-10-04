//! Sign and upload Debian `.changes` files.
//!
//! Thin wrappers around `silver_platter::debian::uploader::{debsign,
//! dput_changes}`. Those are blocking, so we call them via
//! [`tokio::task::spawn_blocking`].

use std::path::Path;

use silver_platter::debian::uploader as sp_uploader;
use tracing::info;

use crate::error::{Result, UploadError};
use crate::{DEBSIGN_FAILED_COUNT, UPLOAD_FAILED_COUNT};

/// Sign the `.changes` file at `changes_path` with `debsign`, optionally
/// under GPG key `keyid`.
pub async fn sign_package(changes_path: &Path, keyid: Option<&str>) -> Result<()> {
    info!("Signing package: {}", changes_path.display());

    let path = changes_path.to_path_buf();
    let keyid = keyid.map(str::to_string);
    tokio::task::spawn_blocking(move || sp_uploader::debsign(&path, keyid.as_deref()))
        .await
        .map_err(|e| UploadError::DebsignFailure(format!("debsign task panicked: {}", e)))?
        .map_err(|e| {
            DEBSIGN_FAILED_COUNT.inc();
            match e {
                sp_uploader::SignError::Failed(msg) => UploadError::DebsignFailure(msg),
                sp_uploader::SignError::IOError(e) => UploadError::Io(e),
            }
        })
}

/// Upload the `.changes` file at `changes_path` via `dput`.
///
/// When `dput_host` is `None`, dput picks its default host from `dput.cf`.
pub async fn upload_package(changes_path: &Path, dput_host: Option<&str>) -> Result<()> {
    info!(
        "Uploading package: {} to {}",
        changes_path.display(),
        dput_host.unwrap_or("<default>")
    );

    let path = changes_path.to_path_buf();
    let host = dput_host.map(str::to_string);
    tokio::task::spawn_blocking(move || sp_uploader::dput_changes(&path, host.as_deref()))
        .await
        .map_err(|e| UploadError::DputFailure(format!("dput task panicked: {}", e)))?
        .map_err(|e| {
            UPLOAD_FAILED_COUNT.inc();
            match e {
                sp_uploader::UploadError::Failed(msg) => UploadError::DputFailure(msg),
                sp_uploader::UploadError::IOError(e) => UploadError::Io(e),
            }
        })
}

/// Which packages to upload and how.
#[derive(Debug, Clone)]
pub struct UploadConfig {
    /// GPG key ID for signing (passed to debsign as `-k`).
    pub debsign_keyid: Option<String>,
    /// dput target host, matching an entry in `dput.cf`. `None` lets dput
    /// pick the default host.
    pub dput_host: Option<String>,
    /// Only upload `_source.changes` files.
    pub source_only: bool,
    /// If non-empty, only upload builds whose distribution appears here.
    pub distributions: Vec<String>,
}

impl UploadConfig {
    /// True if `distribution` is in the allow-list (or the list is empty).
    pub fn should_upload_distribution(&self, distribution: &str) -> bool {
        self.distributions.is_empty() || self.distributions.iter().any(|d| d == distribution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use tempfile::TempDir;

    // sign_package / upload_package tests mutate the process-wide PATH to
    // point at a fake debsign / dput, so they cannot run in parallel.
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    /// Write an executable shim at `dir/name` that silently exits with
    /// `exit_code`.
    fn write_shim(dir: &Path, name: &str, exit_code: i32) {
        let path = dir.join(name);
        let script = format!("#!/bin/sh\nexit {}\n", exit_code);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    struct PathGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
        original: std::ffi::OsString,
    }

    impl PathGuard<'_> {
        fn prepend<'a>(shim_dir: &Path) -> PathGuard<'a> {
            let lock = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let original = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![shim_dir.to_path_buf()];
            paths.extend(std::env::split_paths(&original));
            let joined = std::env::join_paths(paths).unwrap();
            // SAFETY: `_lock` serialises all PATH writes in this module and
            // the guard restores the original on drop.
            unsafe { std::env::set_var("PATH", &joined) };
            PathGuard {
                _lock: lock,
                original,
            }
        }
    }

    impl Drop for PathGuard<'_> {
        fn drop(&mut self) {
            // SAFETY: still holding `_lock`.
            unsafe { std::env::set_var("PATH", &self.original) };
        }
    }

    fn write_changes(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("foo_1.0-1_amd64.changes");
        std::fs::write(&path, b"Format: 1.8\n").unwrap();
        path
    }

    fn cfg(distributions: Vec<String>) -> UploadConfig {
        UploadConfig {
            dput_host: Some("test-host".into()),
            debsign_keyid: None,
            source_only: false,
            distributions,
        }
    }

    #[test]
    fn distribution_allow_list_matches_configured_entries() {
        let config = cfg(vec!["unstable".to_string(), "experimental".to_string()]);
        assert!(config.should_upload_distribution("unstable"));
        assert!(config.should_upload_distribution("experimental"));
        assert!(!config.should_upload_distribution("stable"));
    }

    #[test]
    fn empty_allow_list_accepts_any_distribution() {
        let config = cfg(vec![]);
        assert!(config.should_upload_distribution("unstable"));
        assert!(config.should_upload_distribution("stable"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sign_package_succeeds_when_debsign_exits_zero() {
        let shims = TempDir::new().unwrap();
        write_shim(shims.path(), "debsign", 0);
        let _guard = PathGuard::prepend(shims.path());

        let workdir = TempDir::new().unwrap();
        let changes = write_changes(workdir.path());
        sign_package(&changes, None)
            .await
            .expect("sign should succeed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sign_package_reports_failure_and_bumps_counter() {
        let shims = TempDir::new().unwrap();
        write_shim(shims.path(), "debsign", 1);
        let _guard = PathGuard::prepend(shims.path());

        let before = DEBSIGN_FAILED_COUNT.get();
        let workdir = TempDir::new().unwrap();
        let changes = write_changes(workdir.path());
        match sign_package(&changes, None).await {
            Err(UploadError::DebsignFailure(_)) => {}
            other => panic!("expected DebsignFailure, got {:?}", other),
        }
        assert_eq!(DEBSIGN_FAILED_COUNT.get(), before + 1.0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upload_package_succeeds_when_dput_exits_zero() {
        let shims = TempDir::new().unwrap();
        write_shim(shims.path(), "dput", 0);
        let _guard = PathGuard::prepend(shims.path());

        let workdir = TempDir::new().unwrap();
        let changes = write_changes(workdir.path());
        upload_package(&changes, Some("some-host"))
            .await
            .expect("upload should succeed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upload_package_reports_failure_and_bumps_counter() {
        let shims = TempDir::new().unwrap();
        write_shim(shims.path(), "dput", 1);
        let _guard = PathGuard::prepend(shims.path());

        let before = UPLOAD_FAILED_COUNT.get();
        let workdir = TempDir::new().unwrap();
        let changes = write_changes(workdir.path());
        match upload_package(&changes, Some("some-host")).await {
            Err(UploadError::DputFailure(_)) => {}
            other => panic!("expected DputFailure, got {:?}", other),
        }
        assert_eq!(UPLOAD_FAILED_COUNT.get(), before + 1.0);
    }
}
