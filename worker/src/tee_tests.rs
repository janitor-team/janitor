#[cfg(test)]
mod tests {
    use crate::tee::CopyOutput;
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
}
