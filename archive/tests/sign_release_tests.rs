//! Integration test for `janitor_archive::sign::sign_release`.
//!
//! Creates a throwaway GPG home with an unprotected RSA key, signs a
//! minimal Release body, and asserts both `Release.gpg` and
//! `InRelease` verify against that key.

use std::path::Path;
use std::process::Command;

use janitor_archive::config::GpgConfig;
use janitor_archive::sign::sign_release;
use tempfile::TempDir;

const RELEASE_BODY: &[u8] = b"Origin: test\nLabel: test\nSuite: test\nCodename: test\n";

fn generate_test_key(gpg_home: &Path) -> String {
    let batch = "\
        Key-Type: RSA\n\
        Key-Length: 1024\n\
        Name-Real: janitor-sign-test\n\
        Name-Email: sign-test@example.com\n\
        Expire-Date: 0\n\
        %no-protection\n\
        %commit\n";

    let status = Command::new("gpg")
        .env("GNUPGHOME", gpg_home)
        .args(["--batch", "--generate-key"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("gpg stdin")
                .write_all(batch.as_bytes())?;
            child.wait()
        })
        .expect("gpg generate-key");
    assert!(status.success(), "gpg --generate-key failed");

    // Grab the long keyid of the only secret key we just created.
    let output = Command::new("gpg")
        .env("GNUPGHOME", gpg_home)
        .args(["--batch", "--list-secret-keys", "--with-colons"])
        .output()
        .expect("gpg list-secret-keys");
    assert!(output.status.success());
    let listing = String::from_utf8(output.stdout).unwrap();
    listing
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            (fields.next()? == "fpr")
                .then(|| fields.nth(8))
                .flatten()
                .map(str::to_string)
        })
        .expect("secret key fingerprint")
}

fn verify(gpg_home: &Path, extra_arg: &str, path: &Path) -> bool {
    Command::new("gpg")
        .env("GNUPGHOME", gpg_home)
        .args(["--batch", "--verify"])
        .arg(extra_arg)
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Shared fixture: throwaway GPG home + key + repo dir containing
/// the RELEASE_BODY. Returns the ambient state each test needs to
/// call `sign_release` and verify the output.
async fn make_signing_fixture() -> (TempDir, TempDir, String) {
    let gpg_home = TempDir::new().unwrap();
    let key_id = generate_test_key(gpg_home.path());
    let repo_dir = TempDir::new().unwrap();
    tokio::fs::write(repo_dir.path().join("Release"), RELEASE_BODY)
        .await
        .unwrap();
    (gpg_home, repo_dir, key_id)
}

#[tokio::test]
async fn sign_release_produces_verifiable_signatures() {
    let (gpg_home, repo_dir, key_id) = make_signing_fixture().await;

    let cfg = GpgConfig {
        key_id,
        gpg_home: Some(gpg_home.path().to_path_buf()),
        passphrase: None,
        detached_signature: true,
        clearsign: true,
    };

    sign_release(repo_dir.path(), RELEASE_BODY, &cfg)
        .await
        .expect("sign_release");

    let release_gpg = repo_dir.path().join("Release.gpg");
    let inrelease = repo_dir.path().join("InRelease");
    let release = repo_dir.path().join("Release");

    assert!(release_gpg.exists(), "Release.gpg was not created");
    assert!(inrelease.exists(), "InRelease was not created");

    // Detached: gpg --verify Release.gpg Release
    let release_str = release.to_string_lossy().to_string();
    assert!(
        Command::new("gpg")
            .env("GNUPGHOME", gpg_home.path())
            .args(["--batch", "--verify"])
            .arg(&release_gpg)
            .arg(&release)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "Release.gpg did not verify against Release ({})",
        release_str
    );

    // Clearsigned: gpg --verify InRelease
    assert!(
        verify(gpg_home.path(), "--", &inrelease),
        "InRelease did not verify"
    );
}

/// `detached_signature = true, clearsign = false` -- must produce
/// `Release.gpg` but *not* `InRelease`. Guards against a future
/// refactor that unconditionally emits both files.
#[tokio::test]
async fn sign_release_detached_only_skips_inrelease() {
    let (gpg_home, repo_dir, key_id) = make_signing_fixture().await;

    let cfg = GpgConfig {
        key_id,
        gpg_home: Some(gpg_home.path().to_path_buf()),
        passphrase: None,
        detached_signature: true,
        clearsign: false,
    };

    sign_release(repo_dir.path(), RELEASE_BODY, &cfg)
        .await
        .expect("sign_release");

    let release_gpg = repo_dir.path().join("Release.gpg");
    let inrelease = repo_dir.path().join("InRelease");
    assert!(release_gpg.exists(), "Release.gpg must be created");
    assert!(
        !inrelease.exists(),
        "InRelease must NOT be created when clearsign=false"
    );
    // The detached signature must still verify.
    assert!(
        Command::new("gpg")
            .env("GNUPGHOME", gpg_home.path())
            .args(["--batch", "--verify"])
            .arg(&release_gpg)
            .arg(repo_dir.path().join("Release"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "detached Release.gpg failed to verify"
    );
}

/// `detached_signature = false, clearsign = true` -- must produce
/// `InRelease` but *not* `Release.gpg`. Opposite of the previous
/// test.
#[tokio::test]
async fn sign_release_clearsign_only_skips_detached() {
    let (gpg_home, repo_dir, key_id) = make_signing_fixture().await;

    let cfg = GpgConfig {
        key_id,
        gpg_home: Some(gpg_home.path().to_path_buf()),
        passphrase: None,
        detached_signature: false,
        clearsign: true,
    };

    sign_release(repo_dir.path(), RELEASE_BODY, &cfg)
        .await
        .expect("sign_release");

    let release_gpg = repo_dir.path().join("Release.gpg");
    let inrelease = repo_dir.path().join("InRelease");
    assert!(!release_gpg.exists(), "Release.gpg must NOT be created");
    assert!(inrelease.exists(), "InRelease must be created");
    assert!(
        verify(gpg_home.path(), "--", &inrelease),
        "InRelease did not verify"
    );
}

/// Both flags off -- sign_release must be a no-op. Neither file
/// gets written. Guards against silently emitting empty
/// signatures.
#[tokio::test]
async fn sign_release_both_flags_off_is_noop() {
    let (gpg_home, repo_dir, key_id) = make_signing_fixture().await;

    let cfg = GpgConfig {
        key_id,
        gpg_home: Some(gpg_home.path().to_path_buf()),
        passphrase: None,
        detached_signature: false,
        clearsign: false,
    };

    sign_release(repo_dir.path(), RELEASE_BODY, &cfg)
        .await
        .expect("sign_release");

    assert!(!repo_dir.path().join("Release.gpg").exists());
    assert!(!repo_dir.path().join("InRelease").exists());
}
