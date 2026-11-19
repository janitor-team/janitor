//! Redis integration tests for the differ service.

use redis::AsyncCommands;
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;

/// Test Redis connection and basic operations
#[tokio::test]
#[ignore] // Requires Redis server
async fn test_redis_connection() {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    let client = redis::Client::open(redis_url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();

    // Test basic operations
    conn.set::<_, _, ()>("differ_test_key", "test_value")
        .await
        .unwrap();
    let result: String = conn.get("differ_test_key").await.unwrap();
    assert_eq!(result, "test_value");

    // Clean up
    conn.del::<_, ()>("differ_test_key").await.unwrap();
}

/// Test Redis pub/sub functionality for run completion events
#[tokio::test]
#[ignore] // Requires Redis server
async fn test_redis_pubsub_run_completion() {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    let publisher_client = redis::Client::open(redis_url.as_str()).unwrap();
    let subscriber_client = redis::Client::open(redis_url.as_str()).unwrap();

    let mut publisher_conn = publisher_client
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let mut pubsub = subscriber_client.get_async_pubsub().await.unwrap();

    // Set up subscriber
    pubsub.subscribe("run-finished").await.unwrap();

    // Publish a test run completion event
    let run_event = json!({
        "run_id": "test_run_123",
        "result_code": "success",
        "codebase": "test/package",
        "suite": "lintian-fixes"
    });

    publisher_conn
        .publish::<_, _, ()>("run-finished", run_event.to_string())
        .await
        .unwrap();

    // Wait for the message with timeout
    use futures_util::StreamExt;
    let mut stream = pubsub.on_message();
    let message_result = timeout(Duration::from_secs(5), stream.next()).await;

    assert!(message_result.is_ok());
    let message = message_result.unwrap().unwrap();

    let payload: String = message.get_payload().unwrap();
    let parsed_event: serde_json::Value = serde_json::from_str(&payload).unwrap();

    assert_eq!(parsed_event["run_id"], "test_run_123");
    assert_eq!(parsed_event["result_code"], "success");
}

/// Test Redis connection resilience
#[tokio::test]
#[ignore] // Requires Redis server
async fn test_redis_connection_resilience() {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    let client = redis::Client::open(redis_url).unwrap();

    // Test connection manager for automatic reconnection
    let conn_manager = redis::aio::ConnectionManager::new(client).await.unwrap();
    let mut conn = conn_manager.clone();

    // Test basic operation
    conn.set::<_, _, ()>("resilience_test", "value1")
        .await
        .unwrap();
    let result: String = conn.get("resilience_test").await.unwrap();
    assert_eq!(result, "value1");

    // Test that connection manager handles multiple operations
    for i in 0..10 {
        let key = format!("test_key_{}", i);
        let value = format!("test_value_{}", i);
        conn.set::<_, _, ()>(&key, &value).await.unwrap();
        let retrieved: String = conn.get(&key).await.unwrap();
        assert_eq!(retrieved, value);
        conn.del::<_, ()>(&key).await.unwrap();
    }

    // Clean up
    conn.del::<_, ()>("resilience_test").await.unwrap();
}

/// Test event filtering for automatic precaching
#[tokio::test]
#[ignore] // Requires Redis server and database
async fn test_precaching_event_filtering() {
    // This test simulates the differ service's Redis event handling logic

    let test_events = vec![
        // Should trigger precaching (successful run)
        json!({
            "run_id": "success_run_1",
            "result_code": "success",
            "codebase": "test/package1",
            "suite": "lintian-fixes"
        }),
        // Should not trigger precaching (failed run)
        json!({
            "run_id": "failed_run_1",
            "result_code": "failed",
            "codebase": "test/package2",
            "suite": "lintian-fixes"
        }),
        // Should not trigger precaching (control suite)
        json!({
            "run_id": "control_run_1",
            "result_code": "success",
            "codebase": "test/package3",
            "suite": "control"
        }),
        // Should trigger precaching (another successful run)
        json!({
            "run_id": "success_run_2",
            "result_code": "success",
            "codebase": "test/package4",
            "suite": "upstream-ontologist"
        }),
    ];

    let mut should_precache_count = 0;

    for event in test_events {
        let result_code = event
            .get("result_code")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let suite = event.get("suite").and_then(|v| v.as_str()).unwrap_or("");

        // Apply the same filtering logic as the differ service
        let should_precache = result_code == "success" && !matches!(suite, "control" | "unchanged");

        if should_precache {
            should_precache_count += 1;
            println!("Would trigger precaching for run: {}", event["run_id"]);
        } else {
            println!(
                "Would skip precaching for run: {} (reason: result_code={}, suite={})",
                event["run_id"], result_code, suite
            );
        }
    }

    assert_eq!(should_precache_count, 2); // Only 2 events should trigger precaching
}

/// Test Redis event message format validation
#[tokio::test]
async fn test_event_message_validation() {
    // Test various event message formats that the differ service might receive

    let valid_events = vec![
        json!({
            "run_id": "valid_run_1",
            "result_code": "success",
            "codebase": "test/package",
            "suite": "lintian-fixes",
            "timestamp": "2024-01-01T00:00:00Z"
        }),
        json!({
            "run_id": "valid_run_2",
            "result_code": "failed",
            "codebase": "test/package2"
        }),
    ];

    let invalid_events = vec![
        json!({}),                                // Empty event
        json!({"result_code": "success"}),        // Missing run_id
        serde_json::json!("invalid json string"), // Not JSON
        json!({"run_id": null}),                  // Null run_id
    ];

    // Test valid events
    for event in valid_events {
        let event_str = serde_json::to_string(&event).unwrap();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&event_str);
        assert!(parsed.is_ok());

        let parsed_event = parsed.unwrap();
        let run_id = parsed_event.get("run_id").and_then(|v| v.as_str());
        assert!(run_id.is_some());
        assert!(!run_id.unwrap().is_empty());
    }

    // Test invalid events
    for event in invalid_events {
        let event_str = match event {
            serde_json::Value::String(s) => s,
            _ => serde_json::to_string(&event).unwrap(),
        };

        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&event_str);
        if let Ok(parsed_event) = parsed {
            let run_id = parsed_event.get("run_id").and_then(|v| v.as_str());
            // Should either fail to parse or have missing/invalid run_id
            assert!(run_id.is_none() || run_id.unwrap().is_empty());
        }
        // If parsing fails entirely, that's also acceptable for invalid events
    }
}

/// Test Redis connection pool behavior under load
#[tokio::test]
#[ignore] // Requires Redis server
async fn test_redis_connection_pool_load() {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    let client = redis::Client::open(redis_url).unwrap();
    let conn_manager = redis::aio::ConnectionManager::new(client).await.unwrap();

    // Spawn multiple concurrent tasks that use Redis
    let mut handles = Vec::new();

    for i in 0..20 {
        let conn_manager = conn_manager.clone();

        let handle = tokio::spawn(async move {
            let mut conn = conn_manager;

            // Perform multiple operations per task
            for j in 0..10 {
                let key = format!("load_test_{}_{}", i, j);
                let value = format!("value_{}_{}", i, j);

                conn.set::<_, _, ()>(&key, &value).await.unwrap();
                let retrieved: String = conn.get(&key).await.unwrap();
                assert_eq!(retrieved, value);
                conn.del::<_, ()>(&key).await.unwrap();

                // Small delay to simulate realistic usage
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }

    println!("Redis connection pool load test completed successfully");
}

/// Test Redis error handling and recovery
#[tokio::test]
#[ignore] // Requires Redis server
async fn test_redis_error_handling() {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    let client = redis::Client::open(redis_url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();

    // Test operations that should succeed
    conn.set::<_, _, ()>("error_test_key", "value")
        .await
        .unwrap();
    let result: String = conn.get("error_test_key").await.unwrap();
    assert_eq!(result, "value");

    // Test operations that should fail gracefully
    let result: redis::RedisResult<String> =
        conn.get("nonexistent_key_that_should_not_exist").await;
    match result {
        Ok(value) => {
            // If key exists for some reason, that's unexpected but not an error
            println!("Unexpected key found: {}", value);
        }
        Err(e) => {
            // Expected behavior for missing keys
            match e.kind() {
                redis::ErrorKind::UnexpectedReturnType => {
                    // Redis returns nil for missing keys, which becomes a type error when
                    // trying to convert to String
                    println!("Got expected type error for missing key");
                }
                _ => {
                    println!("Got unexpected error type: {:?}", e);
                }
            }
        }
    }

    // Test invalid operations
    let invalid_result: redis::RedisResult<()> =
        conn.hset("error_test_key", "field", "value").await;
    match invalid_result {
        Ok(_) => {
            // This might succeed if the key was deleted
            println!("HSET operation succeeded unexpectedly");
        }
        Err(e) => {
            // Expected if trying to use hash operations on a string value
            println!("Got expected error for invalid operation: {:?}", e);
        }
    }

    // Clean up
    conn.del::<_, ()>("error_test_key").await.unwrap();
}

/// Test Redis memory usage and cleanup
#[tokio::test]
#[ignore] // Requires Redis server
async fn test_redis_memory_cleanup() {
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

    let client = redis::Client::open(redis_url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();

    // Store many keys to test memory usage
    let key_count = 1000;
    let keys: Vec<String> = (0..key_count)
        .map(|i| format!("memory_test_key_{}", i))
        .collect();

    // Set all keys
    for (i, key) in keys.iter().enumerate() {
        let value = format!("test_value_{}", i);
        conn.set::<_, _, ()>(key, value).await.unwrap();
    }

    // Verify all keys exist
    let mut found_count = 0;
    for key in &keys {
        let exists: bool = conn.exists(key).await.unwrap();
        if exists {
            found_count += 1;
        }
    }
    assert_eq!(found_count, key_count);

    // Clean up all keys
    for key in &keys {
        conn.del::<_, ()>(key).await.unwrap();
    }

    // Verify all keys are deleted
    let mut remaining_count = 0;
    for key in &keys {
        let exists: bool = conn.exists(key).await.unwrap();
        if exists {
            remaining_count += 1;
        }
    }
    assert_eq!(remaining_count, 0);

    println!(
        "Redis memory cleanup test completed: {} keys created and cleaned up",
        key_count
    );
}
