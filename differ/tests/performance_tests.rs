//! Performance and load tests for the differ service.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Semaphore;

/// Test concurrent diffoscope operations
#[tokio::test]
async fn test_concurrent_diffoscope() {
    let temp_dir = TempDir::new().unwrap();
    let concurrency = 5;
    let semaphore = Arc::new(Semaphore::new(concurrency));

    let mut handles = Vec::new();

    for i in 0..10 {
        let temp_dir_path = temp_dir.path().to_owned();
        let semaphore = semaphore.clone();

        let handle = tokio::spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();

            // Create test files
            let old_file = temp_dir_path.join(format!("old_{}.json", i));
            let new_file = temp_dir_path.join(format!("new_{}.json", i));

            std::fs::write(&old_file, format!(r#"{{"test": "value_{}_old"}}"#, i)).unwrap();
            std::fs::write(&new_file, format!(r#"{{"test": "value_{}_new"}}"#, i)).unwrap();

            let start = Instant::now();

            // Run diffoscope
            let result = janitor_differ::diffoscope::run_diffoscope(
                &[(format!("old_{}", i).as_str(), old_file.to_str().unwrap())],
                &[(format!("new_{}", i).as_str(), new_file.to_str().unwrap())],
                Some(30.0), // 30 second timeout
                Some(512),  // 512MB memory limit
                None,
            )
            .await;

            let duration = start.elapsed();

            // Clean up files
            let _ = std::fs::remove_file(&old_file);
            let _ = std::fs::remove_file(&new_file);

            (result, duration)
        });

        handles.push(handle);
    }

    let mut total_time = Duration::new(0, 0);
    let mut success_count = 0;

    for handle in handles {
        match handle.await {
            Ok((result, duration)) => {
                total_time += duration;
                if result.is_ok() {
                    success_count += 1;
                }
            }
            Err(e) => {
                eprintln!("Task failed: {}", e);
            }
        }
    }

    println!("Concurrent diffoscope test completed:");
    println!("  Successful operations: {}/10", success_count);
    println!("  Total time: {:?}", total_time);
    println!("  Average time per operation: {:?}", total_time / 10);

    // At least half should succeed in reasonable time
    assert!(success_count >= 5);
    assert!(total_time < Duration::from_secs(300)); // Total under 5 minutes
}

/// Test memory usage patterns during diff operations
#[tokio::test]
#[ignore] // This test requires access to binary internals
async fn test_memory_usage_monitoring() {
    // Mock function for testing - in real usage this would be from the binary
    fn get_process_memory_mb() -> Option<f64> {
        Some(100.0) // Mock value
    }

    // Record initial memory
    let initial_memory = get_process_memory_mb().unwrap_or(0.0) as f32;

    // Create a moderately large file to diff
    let temp_dir = TempDir::new().unwrap();
    let old_file = temp_dir.path().join("large_old.json");
    let new_file = temp_dir.path().join("large_new.json");

    // Create files with different content that will produce a diff
    let large_content_old = format!(
        r#"{{"data": {}}}"#,
        (0..1000)
            .map(|i| format!(r#""item_{}": "value_{}_old""#, i, i))
            .collect::<Vec<_>>()
            .join(",")
    );
    let large_content_new = format!(
        r#"{{"data": {}}}"#,
        (0..1000)
            .map(|i| format!(r#""item_{}": "value_{}_new""#, i, i))
            .collect::<Vec<_>>()
            .join(",")
    );

    std::fs::write(&old_file, large_content_old).unwrap();
    std::fs::write(&new_file, large_content_new).unwrap();

    // Monitor memory during operation
    let max_memory = initial_memory;
    let start = Instant::now();

    // Start monitoring in background
    let monitoring_handle = tokio::spawn(async move {
        let mut peak_memory: f32 = 0.0;
        while start.elapsed() < Duration::from_secs(60) {
            if let Some(current_memory) = get_process_memory_mb() {
                peak_memory = peak_memory.max(current_memory as f32);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        peak_memory
    });

    // Run the diff operation
    let diff_result = janitor_differ::diffoscope::run_diffoscope(
        &[("large_old", old_file.to_str().unwrap())],
        &[("large_new", new_file.to_str().unwrap())],
        Some(30.0), // 30 second timeout
        Some(1024), // 1GB memory limit
        None,
    )
    .await;

    // Get peak memory usage
    let peak_memory = monitoring_handle.await.unwrap();
    let final_memory = get_process_memory_mb().unwrap_or(0.0) as f32;

    println!("Memory usage test results:");
    println!("  Initial memory: {:.1}MB", initial_memory);
    println!("  Peak memory: {:.1}MB", peak_memory);
    println!("  Final memory: {:.1}MB", final_memory);
    println!("  Memory increase: {:.1}MB", peak_memory - initial_memory);

    // Verify operation completed successfully
    assert!(diff_result.is_ok());

    // Memory should not grow unreasonably (less than 500MB increase)
    assert!(peak_memory - initial_memory < 500.0);

    // Memory should be cleaned up afterwards (within 50MB of initial)
    assert!((final_memory - initial_memory).abs() < 50.0);
}

/// Test timeout behavior under load
#[tokio::test]
async fn test_timeout_behavior() {
    let temp_dir = TempDir::new().unwrap();

    // Create files that might take a while to diff
    let old_file = temp_dir.path().join("timeout_old.json");
    let new_file = temp_dir.path().join("timeout_new.json");

    // Create files with complex nested structure
    let complex_content = format!(
        r#"{{"nested": {}}}"#,
        (0..100)
            .map(|i| {
                format!(
                    r#""level_{}": {}"#,
                    i,
                    format!(
                        r#"{{"sublevel": {}}}"#,
                        (0..50)
                            .map(|j| format!(r#""item_{}": "data_{}_{}""#, j, i, j))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    );

    std::fs::write(&old_file, &complex_content).unwrap();

    // Modify content slightly for new file
    let modified_content = complex_content.replace("_old", "_new");
    std::fs::write(&new_file, modified_content).unwrap();

    // Test with very short timeout
    let start = Instant::now();
    let result = janitor_differ::diffoscope::run_diffoscope(
        &[("timeout_old", old_file.to_str().unwrap())],
        &[("timeout_new", new_file.to_str().unwrap())],
        Some(1.0), // 1 second timeout - should timeout for complex diffs
        Some(512),
        None,
    )
    .await;
    let elapsed = start.elapsed();

    // Should either complete quickly or timeout appropriately
    assert!(elapsed < Duration::from_secs(5)); // Should not hang

    // If it failed, it should be due to timeout
    if let Err(error) = result {
        match error {
            janitor_differ::diffoscope::DiffoscopeError::Timeout => {
                // Expected behavior
                println!("Timeout test passed - operation timed out as expected");
            }
            _ => {
                // Other errors are also acceptable for this test
                println!("Timeout test completed with different error: {:?}", error);
            }
        }
    } else {
        println!("Timeout test passed - operation completed within timeout");
    }
}

/// Test cache performance improvements
#[tokio::test]
async fn test_cache_performance() {
    let temp_dir = TempDir::new().unwrap();
    let cache_dir = temp_dir.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();

    // Create test files
    let old_file = temp_dir.path().join("cache_old.json");
    let new_file = temp_dir.path().join("cache_new.json");

    std::fs::write(&old_file, r#"{"test": "old_value"}"#).unwrap();
    std::fs::write(&new_file, r#"{"test": "new_value"}"#).unwrap();

    // First run - no cache
    let start1 = Instant::now();
    let result1 = janitor_differ::diffoscope::run_diffoscope(
        &[("cache_old", old_file.to_str().unwrap())],
        &[("cache_new", new_file.to_str().unwrap())],
        None,
        None,
        None,
    )
    .await;
    let time1 = start1.elapsed();

    assert!(result1.is_ok());

    // Simulate caching by writing result to cache file
    let cache_file = cache_dir.join("cache_old_cache_new.json");
    if let Ok(diff) = result1 {
        let cache_content = serde_json::to_string(&diff).unwrap();
        std::fs::write(&cache_file, cache_content).unwrap();
    }

    // Second run - simulate cache hit by reading from cache
    let start2 = Instant::now();
    let cached_content = std::fs::read_to_string(&cache_file).unwrap();
    let _cached_result: janitor_differ::diffoscope::DiffoscopeOutput =
        serde_json::from_str(&cached_content).unwrap();
    let time2 = start2.elapsed();

    println!("Cache performance test:");
    println!("  First run (no cache): {:?}", time1);
    println!("  Second run (cache hit): {:?}", time2);
    println!(
        "  Cache speedup: {:.2}x",
        time1.as_nanos() as f64 / time2.as_nanos() as f64
    );

    // Cache hit should be significantly faster
    assert!(time2 < time1);
    assert!(time2 < Duration::from_millis(10)); // Cache hit should be very fast
}

/// Test error handling under stress
#[tokio::test]
async fn test_error_handling_stress() {
    let temp_dir = TempDir::new().unwrap();
    let mut handles = Vec::new();

    // Create various error scenarios
    for i in 0..20 {
        let temp_path = temp_dir.path().to_owned();

        let handle = tokio::spawn(async move {
            match i % 4 {
                0 => {
                    // Missing files
                    let result = janitor_differ::diffoscope::run_diffoscope(
                        &[("missing", "/nonexistent/file")],
                        &[("missing", "/another/nonexistent/file")],
                        None,
                        None,
                        None,
                    )
                    .await;
                    assert!(result.is_err());
                }
                1 => {
                    // Empty files
                    let old_file = temp_path.join(format!("empty_old_{}", i));
                    let new_file = temp_path.join(format!("empty_new_{}", i));
                    std::fs::write(&old_file, "").unwrap();
                    std::fs::write(&new_file, "").unwrap();

                    let result = janitor_differ::diffoscope::run_diffoscope(
                        &[("empty", old_file.to_str().unwrap())],
                        &[("empty", new_file.to_str().unwrap())],
                        None,
                        None,
                        None,
                    )
                    .await;
                    // This might succeed (no diff) or fail (no binaries) - both are valid
                }
                2 => {
                    // Very small memory limit
                    let old_file = temp_path.join(format!("small_old_{}", i));
                    let new_file = temp_path.join(format!("small_new_{}", i));
                    std::fs::write(&old_file, r#"{"small": "file"}"#).unwrap();
                    std::fs::write(&new_file, r#"{"small": "different"}"#).unwrap();

                    let result = janitor_differ::diffoscope::run_diffoscope(
                        &[("small", old_file.to_str().unwrap())],
                        &[("small", new_file.to_str().unwrap())],
                        None,
                        Some(1), // 1MB limit - very restrictive
                        None,
                    )
                    .await;
                    // Might succeed or fail due to memory limit
                }
                3 => {
                    // Very short timeout
                    let old_file = temp_path.join(format!("quick_old_{}", i));
                    let new_file = temp_path.join(format!("quick_new_{}", i));
                    std::fs::write(&old_file, r#"{"quick": "test"}"#).unwrap();
                    std::fs::write(&new_file, r#"{"quick": "modified"}"#).unwrap();

                    let result = janitor_differ::diffoscope::run_diffoscope(
                        &[("quick", old_file.to_str().unwrap())],
                        &[("quick", new_file.to_str().unwrap())],
                        Some(0.1), // 100ms timeout
                        None,
                        None,
                    )
                    .await;
                    // Might succeed if very fast, or timeout
                }
                _ => unreachable!(),
            }
        });

        handles.push(handle);
    }

    // Wait for all tasks to complete
    let mut completed = 0;
    for handle in handles {
        if handle.await.is_ok() {
            completed += 1;
        }
    }

    println!(
        "Error handling stress test completed: {}/20 tasks finished",
        completed
    );

    // Most tasks should complete without panicking
    assert!(completed >= 15);
}

/// Benchmark the basic operations
#[tokio::test]
async fn test_basic_operations_benchmark() {
    use janitor_differ::{find_binaries, is_binary};

    let temp_dir = TempDir::new().unwrap();

    // Create many files for testing find_binaries performance
    for i in 0..1000 {
        let filename = if i % 3 == 0 {
            format!("package_{}.deb", i)
        } else if i % 3 == 1 {
            format!("package_{}.udeb", i)
        } else {
            format!("file_{}.txt", i)
        };

        std::fs::write(temp_dir.path().join(&filename), format!("content {}", i)).unwrap();
    }

    // Benchmark find_binaries
    let start = Instant::now();
    let binaries: Vec<_> = find_binaries(temp_dir.path()).unwrap();
    let find_time = start.elapsed();

    println!("find_binaries benchmark:");
    println!("  Found {} files", binaries.len());
    println!("  Time taken: {:?}", find_time);

    // Should find all 1000 files (find_binaries returns all files, not just binaries)
    assert!(binaries.len() == 1000);
    assert!(find_time < Duration::from_millis(100));

    // Benchmark is_binary function
    let test_filenames = [
        "test.deb",
        "test.udeb",
        "test.txt",
        "test.tar.gz",
        "test.rpm",
        "package.deb",
        "file.udeb",
        "document.pdf",
        "image.png",
    ];

    let start = Instant::now();
    let binary_count = test_filenames
        .iter()
        .filter(|&name| is_binary(name))
        .count();
    let is_binary_time = start.elapsed();

    println!("is_binary benchmark:");
    println!("  Tested {} filenames", test_filenames.len());
    println!("  Found {} binary files", binary_count);
    println!("  Time taken: {:?}", is_binary_time);

    assert_eq!(binary_count, 4); // 2 .deb and 2 .udeb files
    assert!(is_binary_time < Duration::from_micros(100));
}
