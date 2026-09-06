//! GPG signing for APT Release files.
//!
//! Wraps the `gpg` binary via `tokio::process::Command` to produce the
//! `Release.gpg` (detached) and `InRelease` (clear-signed) files that
//! apt requires to accept the repository. Shelling out matches the
//! `debsign`/`reprepro` approach and avoids a native gpgme dependency.

use crate::config::GpgConfig;
use crate::error::{ArchiveError, ArchiveResult};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Sign `release_bytes` and write `Release.gpg` (detached) and/or
/// `InRelease` (clear-signed) next to the existing `Release` file in
/// `base_path`, according to the flags on `cfg`. The caller must have
/// already written `Release`.
pub async fn sign_release(
    base_path: &Path,
    release_bytes: &[u8],
    cfg: &GpgConfig,
) -> ArchiveResult<()> {
    if cfg.detached_signature {
        let detached = run_gpg(release_bytes, cfg, SignMode::Detach).await?;
        tokio::fs::write(base_path.join("Release.gpg"), &detached)
            .await
            .map_err(ArchiveError::Io)?;
    }

    if cfg.clearsign {
        let clearsigned = run_gpg(release_bytes, cfg, SignMode::Clear).await?;
        tokio::fs::write(base_path.join("InRelease"), &clearsigned)
            .await
            .map_err(ArchiveError::Io)?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum SignMode {
    Detach,
    Clear,
}

async fn run_gpg(input: &[u8], cfg: &GpgConfig, mode: SignMode) -> ArchiveResult<Vec<u8>> {
    let mut cmd = Command::new("gpg");
    cmd.arg("--batch").arg("--yes").arg("--armor");
    cmd.arg("--local-user").arg(&cfg.key_id);
    if let Some(home) = &cfg.gpg_home {
        cmd.arg("--homedir").arg(home);
    }
    if let Some(passphrase) = &cfg.passphrase {
        cmd.arg("--pinentry-mode").arg("loopback");
        cmd.arg("--passphrase").arg(passphrase);
    }
    match mode {
        SignMode::Detach => cmd.arg("--detach-sign"),
        SignMode::Clear => cmd.arg("--clearsign"),
    };
    cmd.arg("--output").arg("-");

    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| ArchiveError::RepositoryGeneration(format!("spawn gpg: {}", e)))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .await
            .map_err(|e| ArchiveError::RepositoryGeneration(format!("write gpg stdin: {}", e)))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| ArchiveError::RepositoryGeneration(format!("close gpg stdin: {}", e)))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| ArchiveError::RepositoryGeneration(format!("wait gpg: {}", e)))?;

    if !output.status.success() {
        return Err(ArchiveError::RepositoryGeneration(format!(
            "gpg exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The sign_release path is exercised by an integration test in
    // tests/sign_release_tests.rs, which provisions a throwaway GPG
    // home. Here we only check that gpg with an unknown key
    // surfaces as an ArchiveError rather than a panic.
    //
    // Earlier revisions manipulated `PATH` to hide `gpg`, but
    // std::env::set_var is process-global and clobbers concurrent
    // tests that shell out to `dpkg-scanpackages` (they end up on
    // a stripped PATH). Passing a bogus GNUPGHOME and a nonexistent
    // key achieves the same "gpg returns non-zero" outcome without
    // touching process state.
    #[tokio::test]
    async fn missing_gpg_key_returns_error() {
        let empty_home = tempfile::tempdir().unwrap();
        let cfg = GpgConfig {
            key_id: "0000000000000000".to_string(),
            gpg_home: Some(empty_home.path().to_path_buf()),
            passphrase: None,
            detached_signature: true,
            clearsign: true,
        };
        let result = run_gpg(b"Origin: test\n", &cfg, SignMode::Detach).await;
        assert!(result.is_err(), "expected gpg to fail with no such key");
    }
}
