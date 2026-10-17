#[cfg(test)]
mod tests {
    use crate::tee::CopyOutput;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    #[serial_test::serial]
    fn test_copy_output_new_invalid_parent() {
        let nonexistent_path = std::path::Path::new("/nonexistent/directory/output.log");
        let result = CopyOutput::new(nonexistent_path, false);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("Parent directory does not exist"));
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_new_valid_parent() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("test_output.log");

        // Test without tee
        let result = CopyOutput::new(&output_path, false);
        assert!(result.is_ok());

        // The file should be created
        assert!(output_path.exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_new_root_directory() {
        // Test with a file in the root directory (which always exists)
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("output.log");

        let result = CopyOutput::new(&output_path, false);
        assert!(result.is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_file_creation() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("test.log");

        // Create CopyOutput which should create the file
        let _copy_output = CopyOutput::new(&output_path, false).unwrap();

        // Verify file was created
        assert!(output_path.exists());
        assert!(output_path.is_file());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_manual_restore() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("restore_test.log");

        let mut copy_output = CopyOutput::new(&output_path, false).unwrap();

        // Should be able to restore manually
        let result = copy_output.restore();
        assert!(result.is_ok());

        // Second restore should be a no-op (old_stdout/stderr already taken)
        let result = copy_output.restore();
        assert!(result.is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_drop_cleanup() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("drop_test.log");

        {
            let _copy_output = CopyOutput::new(&output_path, false).unwrap();
            // CopyOutput should be dropped here and restore file descriptors
        }

        // If we get here without panic, Drop succeeded
        assert!(output_path.exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_fields() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("fields_test.log");

        // Test tee=false
        let copy_output = CopyOutput::new(&output_path, false).unwrap();
        assert!(!copy_output.tee);
        assert!(copy_output.old_stdout.is_some());
        assert!(copy_output.old_stderr.is_some());
        assert!(copy_output.process.is_none());
        assert!(copy_output.newfd.is_some());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_tee_mode_process_spawn_failure() {
        // This test attempts to use tee mode, but may fail if 'tee' command is not available
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("tee_test.log");

        let result = CopyOutput::new(&output_path, true);

        // This might succeed or fail depending on system availability of 'tee'
        match result {
            Ok(copy_output) => {
                assert!(copy_output.tee);
                assert!(copy_output.old_stdout.is_some());
                assert!(copy_output.old_stderr.is_some());
                assert!(copy_output.process.is_some());
                assert!(copy_output.newfd.is_none());
            }
            Err(e) => {
                // Expected on systems without 'tee' command
                assert!(e.to_string().contains("Failed to spawn tee process"));
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_refuses_to_nest() {
        let temp_dir = TempDir::new().unwrap();
        let first = CopyOutput::new(&temp_dir.path().join("first.log"), false).unwrap();

        let second = CopyOutput::new(&temp_dir.path().join("second.log"), false);
        assert!(
            second.is_err(),
            "a second redirection was allowed while the first was held"
        );

        drop(first);
        // The claim is given up again, so a later one succeeds.
        CopyOutput::new(&temp_dir.path().join("third.log"), false).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_file_mode_vs_tee_mode() {
        let temp_dir = TempDir::new().unwrap();

        // Test file mode
        let file_path = temp_dir.path().join("file_mode.log");
        {
            let file_copy = CopyOutput::new(&file_path, false).unwrap();
            assert!(!file_copy.tee);
            assert!(file_copy.newfd.is_some());
            assert!(file_copy.process.is_none());
        }

        // Test tee mode (if available). Only one redirection may be held at a
        // time, so the file one above has to be dropped first.
        let tee_path = temp_dir.path().join("tee_mode.log");
        if let Ok(tee_copy) = CopyOutput::new(&tee_path, true) {
            assert!(tee_copy.tee);
            assert!(tee_copy.newfd.is_none());
            assert!(tee_copy.process.is_some());
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_with_different_extensions() {
        let temp_dir = TempDir::new().unwrap();

        // Test various file extensions
        for ext in &["log", "txt", "out", ""] {
            let filename = if ext.is_empty() {
                "output".to_string()
            } else {
                format!("output.{}", ext)
            };
            let output_path = temp_dir.path().join(filename);

            let result = CopyOutput::new(&output_path, false);
            assert!(result.is_ok(), "Failed for extension: {}", ext);
            assert!(output_path.exists());
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_nested_directories() {
        let temp_dir = TempDir::new().unwrap();

        // Create nested directory structure
        let nested_path = temp_dir.path().join("level1").join("level2");
        fs::create_dir_all(&nested_path).unwrap();

        let output_path = nested_path.join("nested_output.log");
        let result = CopyOutput::new(&output_path, false);

        assert!(result.is_ok());
        assert!(output_path.exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_existing_file_overwrite() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("existing.log");

        // Create existing file with content
        fs::write(&output_path, "existing content").unwrap();
        assert_eq!(
            fs::read_to_string(&output_path).unwrap(),
            "existing content"
        );

        // CopyOutput should overwrite it (File::create truncates)
        let _copy_output = CopyOutput::new(&output_path, false).unwrap();

        // File should still exist but may be truncated
        assert!(output_path.exists());
    }

    // Test the internal state management
    #[test]
    #[serial_test::serial]
    fn test_copy_output_state_transitions() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("state_test.log");

        let mut copy_output = CopyOutput::new(&output_path, false).unwrap();

        // Initial state
        assert!(copy_output.old_stdout.is_some());
        assert!(copy_output.old_stderr.is_some());

        // After restore
        copy_output.restore().unwrap();
        assert!(copy_output.old_stdout.is_none());
        assert!(copy_output.old_stderr.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_error_messages() {
        // Test error message formatting for invalid paths
        let result = CopyOutput::new(std::path::Path::new("/invalid/path/file.log"), false);

        assert!(result.is_err());
        let error_message = result.unwrap_err().to_string();
        assert!(error_message.contains("Parent directory does not exist"));
        assert!(error_message.contains("/invalid/path"));
    }

    // This test ensures the CopyOutput properly implements Send + Sync if needed
    #[test]
    #[serial_test::serial]
    fn test_copy_output_thread_safety() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}

        // CopyOutput contains raw file descriptors, so it may not be Send/Sync
        // This documents the current behavior
        // assert_send::<CopyOutput>();
        // assert_sync::<CopyOutput>();
    }

    #[test]
    #[serial_test::serial]
    fn test_copy_output_memory_safety() {
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("memory_test.log");

        // Test that creating and dropping multiple CopyOutput instances works
        for i in 0..10 {
            let path = temp_dir.path().join(format!("test_{}.log", i));
            let _copy_output = CopyOutput::new(&path, false).unwrap();
            // Each one should clean up properly when dropped
        }
    }
}
