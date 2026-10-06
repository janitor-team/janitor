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
    let mut cmd = gpg_command(cfg);
    cmd.arg("--batch").arg("--yes").arg("--armor");
    if let Some(key_id) = &cfg.key_id {
        cmd.arg("--local-user").arg(key_id);
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

fn gpg_command(cfg: &GpgConfig) -> Command {
    let mut cmd = Command::new("gpg");
    if let Some(home) = &cfg.gpg_home {
        cmd.arg("--homedir").arg(home);
    }
    cmd
}

async fn gpg_output(mut cmd: Command) -> ArchiveResult<Vec<u8>> {
    let output = cmd
        .output()
        .await
        .map_err(|e| ArchiveError::RepositoryGeneration(format!("spawn gpg: {}", e)))?;
    if !output.status.success() {
        return Err(ArchiveError::RepositoryGeneration(format!(
            "gpg exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Export the armored public key(s) used for signing: `key_id` if set,
/// otherwise every key that has a secret key in the keyring.
pub async fn export_public_keys(cfg: &GpgConfig) -> ArchiveResult<String> {
    let key_ids = match &cfg.key_id {
        Some(key_id) => vec![key_id.clone()],
        None => {
            let mut cmd = gpg_command(cfg);
            cmd.args(["--batch", "--with-colons", "--list-secret-keys"]);
            let listing = gpg_output(cmd).await?;
            secret_key_fingerprints(&String::from_utf8_lossy(&listing))
        }
    };
    if key_ids.is_empty() {
        return Err(ArchiveError::RepositoryGeneration(
            "no secret keys in gpg keyring".to_string(),
        ));
    }
    let mut cmd = gpg_command(cfg);
    cmd.args(["--batch", "--armor", "--export"]).args(&key_ids);
    let exported = gpg_output(cmd).await?;
    String::from_utf8(exported)
        .map_err(|e| ArchiveError::RepositoryGeneration(format!("gpg export output: {}", e)))
}

/// Export every key that has a secret key in the keyring, each as a
/// separate armored, minimal export.
pub async fn export_minimal_public_keys(cfg: &GpgConfig) -> ArchiveResult<Vec<String>> {
    let mut cmd = gpg_command(cfg);
    cmd.args(["--batch", "--with-colons", "--list-secret-keys"]);
    let listing = gpg_output(cmd).await?;
    let mut keys = Vec::new();
    for fpr in secret_key_fingerprints(&String::from_utf8_lossy(&listing)) {
        let mut cmd = gpg_command(cfg);
        cmd.args([
            "--batch",
            "--armor",
            "--export-options",
            "export-minimal",
            "--export",
            &fpr,
        ]);
        let exported = gpg_output(cmd).await?;
        keys.push(String::from_utf8(exported).map_err(|e| {
            ArchiveError::RepositoryGeneration(format!("gpg export output: {}", e))
        })?);
    }
    Ok(keys)
}

/// Fingerprints of the primary keys in `gpg --with-colons
/// --list-secret-keys` output.
fn secret_key_fingerprints(listing: &str) -> Vec<String> {
    let mut fingerprints = Vec::new();
    let mut in_primary = false;
    for line in listing.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields[0] {
            "sec" => in_primary = true,
            "fpr" if in_primary => {
                if let Some(fpr) = fields.get(9) {
                    fingerprints.push(fpr.to_string());
                }
                in_primary = false;
            }
            "ssb" => in_primary = false,
            _ => {}
        }
    }
    fingerprints
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
            key_id: Some("0000000000000000".to_string()),
            gpg_home: Some(empty_home.path().to_path_buf()),
            detached_signature: true,
            clearsign: true,
        };
        let result = run_gpg(b"Origin: test\n", &cfg, SignMode::Detach).await;
        assert!(result.is_err(), "expected gpg to fail with no such key");
    }

    #[test]
    fn secret_key_fingerprints_skips_subkeys() {
        let listing = "\
sec:u:255:22:AAAAAAAAAAAAAAAA:1700000000:::u:::scESC:::+:::ed25519:::0:
fpr:::::::::0123456789ABCDEF0123456789ABCDEF01234567:
grp:::::::::1111111111111111111111111111111111111111:
uid:u::::1700000000::HASH::Test <test@example.com>::::::::::0:
ssb:u:255:18:BBBBBBBBBBBBBBBB:1700000000::::::e:::+:::cv25519::
fpr:::::::::FEDCBA9876543210FEDCBA9876543210FEDCBA98:
";
        assert_eq!(
            secret_key_fingerprints(listing),
            vec!["0123456789ABCDEF0123456789ABCDEF01234567".to_string()]
        );
    }
}
