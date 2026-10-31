use crate::{find_changes, FindChangesError};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_find_changes_error_handling_nonexistent_directory() {
    let nonexistent_path = std::path::Path::new("/nonexistent/directory");
    let result = find_changes(nonexistent_path);

    assert!(result.is_err());
    match result.unwrap_err() {
        FindChangesError::IoError(path, err) => {
            assert_eq!(path, nonexistent_path);
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        }
        other => panic!("Expected IoError, got {:?}", other),
    }
}

#[test]
fn test_find_changes_error_handling_empty_directory() {
    let temp_dir = TempDir::new().unwrap();
    let result = find_changes(temp_dir.path());

    assert!(result.is_err());
    match result.unwrap_err() {
        FindChangesError::NoChangesFile(path) => {
            assert_eq!(path, temp_dir.path());
        }
        other => panic!("Expected NoChangesFile, got {:?}", other),
    }
}

#[test]
fn test_find_changes_error_handling_invalid_changes_file() {
    let temp_dir = TempDir::new().unwrap();
    let changes_file = temp_dir.path().join("invalid.changes");

    fs::write(&changes_file, "invalid content").unwrap();

    let result = find_changes(temp_dir.path());

    assert!(result.is_err());
    match result.unwrap_err() {
        FindChangesError::ParseError(path, _) => {
            assert_eq!(path, changes_file);
        }
        other => panic!("Expected ParseError, got {:?}", other),
    }
}

#[test]
fn test_find_changes_error_handling_permission_denied() {
    let temp_dir = TempDir::new().unwrap();
    let changes_file = temp_dir.path().join("test.changes");

    let valid_changes = r#"Format: 1.8
Date: Mon, 01 Jan 2024 00:00:00 +0000
Source: test-package
Binary: test-package
Architecture: amd64
Version: 1.0-1
Distribution: unstable
Urgency: medium
Maintainer: Test <test@example.com>
Changed-By: Test <test@example.com>
Description:
 test-package - Test package
Changes:
 test-package (1.0-1) unstable; urgency=medium
 .
   * Initial release
Checksums-Sha1:
 da39a3ee5e6b4b0d3255bfef95601890afd80709 1234 test-package_1.0-1.deb
Files:
 d41d8cd98f00b204e9800998ecf8427e 1234 deb optional test-package_1.0-1.deb
"#;
    fs::write(&changes_file, valid_changes).unwrap();

    // On Unix systems, we can test permission denied by changing file permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&changes_file).unwrap().permissions();
        perms.set_mode(0o000); // Remove all permissions
        fs::set_permissions(&changes_file, perms).unwrap();

        let result = find_changes(temp_dir.path());

        // Reset permissions for cleanup
        let mut perms = fs::metadata(&changes_file).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&changes_file, perms).unwrap();

        assert!(result.is_err());
        match result.unwrap_err() {
            FindChangesError::IoError(path, err) => {
                assert_eq!(path, changes_file);
                assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("Expected IoError with PermissionDenied, got {:?}", other),
        }
    }
}

#[test]
fn test_find_changes_success_case() {
    let temp_dir = TempDir::new().unwrap();
    let changes_file = temp_dir.path().join("test_1.0-1_amd64.changes");

    let valid_changes = r#"Format: 1.8
Date: Mon, 01 Jan 2024 00:00:00 +0000
Source: test-package
Binary: test-package
Architecture: amd64
Version: 1.0-1
Distribution: unstable
Urgency: medium
Maintainer: Test <test@example.com>
Changed-By: Test <test@example.com>
Description:
 test-package - Test package
Changes:
 test-package (1.0-1) unstable; urgency=medium
 .
   * Initial release
Checksums-Sha1:
 da39a3ee5e6b4b0d3255bfef95601890afd80709 1234 test-package_1.0-1_amd64.deb
Files:
 d41d8cd98f00b204e9800998ecf8427e 1234 deb optional test-package_1.0-1_amd64.deb
"#;
    fs::write(&changes_file, valid_changes).unwrap();

    let result = find_changes(temp_dir.path());

    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(summary.names, vec!["test_1.0-1_amd64.changes"]);
    assert_eq!(summary.source, "test-package");
    assert_eq!(summary.distribution, "unstable");
    assert_eq!(summary.binary_packages, vec!["test-package"]);
}

#[test]
fn test_find_changes_inconsistent_versions() {
    let temp_dir = TempDir::new().unwrap();

    let changes1 = temp_dir.path().join("test_1.0-1_amd64.changes");
    let valid_changes1 = r#"Format: 1.8
Source: test-package
Version: 1.0-1
Distribution: unstable
Checksums-Sha1:
Files:
"#;
    fs::write(&changes1, valid_changes1).unwrap();

    let changes2 = temp_dir.path().join("test_1.0-2_amd64.changes");
    let valid_changes2 = r#"Format: 1.8
Source: test-package
Version: 1.0-2
Distribution: unstable
Checksums-Sha1:
Files:
"#;
    fs::write(&changes2, valid_changes2).unwrap();

    let result = find_changes(temp_dir.path());

    assert!(result.is_err());
    match result.unwrap_err() {
        FindChangesError::InconsistentVersion(names, found, expected) => {
            assert_eq!(names.len(), 2);
            assert!(found.to_string() == "1.0-2" || found.to_string() == "1.0-1");
            assert!(expected.to_string() == "1.0-1" || expected.to_string() == "1.0-2");
            assert_ne!(found, expected);
        }
        other => panic!("Expected InconsistentVersion, got {:?}", other),
    }
}

#[test]
fn test_find_changes_missing_required_fields() {
    let temp_dir = TempDir::new().unwrap();
    let changes_file = temp_dir.path().join("test.changes");

    let incomplete_changes = r#"Format: 1.8
Distribution: unstable
Checksums-Sha1:
Files:
"#;
    fs::write(&changes_file, incomplete_changes).unwrap();

    let result = find_changes(temp_dir.path());

    assert!(result.is_err());
    match result.unwrap_err() {
        FindChangesError::MissingChangesFileFields(field) => {
            assert_eq!(field, "Source");
        }
        other => panic!("Expected MissingChangesFileFields, got {:?}", other),
    }
}

#[test]
fn test_find_changes_binary_package_extraction() {
    let temp_dir = TempDir::new().unwrap();
    let changes_file = temp_dir.path().join("test_1.0-1_amd64.changes");

    let valid_changes = r#"Format: 1.8
Source: test-package
Version: 1.0-1
Distribution: unstable
Checksums-Sha1:
 da39a3ee5e6b4b0d3255bfef95601890afd80709 1234 test-package_1.0-1_amd64.deb
 da39a3ee5e6b4b0d3255bfef95601890afd80709 5678 test-package-dev_1.0-1_amd64.deb
 da39a3ee5e6b4b0d3255bfef95601890afd80709 9012 test-package.tar.gz
Files:
 d41d8cd98f00b204e9800998ecf8427e 1234 deb optional test-package_1.0-1_amd64.deb
 d41d8cd98f00b204e9800998ecf8427e 5678 deb optional test-package-dev_1.0-1_amd64.deb
 d41d8cd98f00b204e9800998ecf8427e 9012 tar optional test-package.tar.gz
"#;
    fs::write(&changes_file, valid_changes).unwrap();

    let result = find_changes(temp_dir.path());

    assert!(result.is_ok());
    let summary = result.unwrap();

    // Should extract package names from .deb files only
    let mut expected_packages = vec!["test-package", "test-package-dev"];
    expected_packages.sort();
    let mut actual_packages = summary.binary_packages;
    actual_packages.sort();

    assert_eq!(actual_packages, expected_packages);
}
