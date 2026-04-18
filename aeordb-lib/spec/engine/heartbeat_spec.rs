use std::sync::Arc;
use aeordb::engine::EventBus;
use aeordb::engine::heartbeat::spawn_heartbeat;
use aeordb::server::create_temp_engine_for_tests;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_heartbeat_emits_event() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let handle = spawn_heartbeat(bus.clone(), engine, 1, CancellationToken::new());

    // Wait for first heartbeat (max ~20 seconds for alignment + one interval)
    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    ).await.expect("should receive heartbeat within 20s")
     .expect("recv should succeed");

    assert_eq!(event.event_type, "heartbeat");
    assert_eq!(event.user_id, "system");
    assert!(event.payload["stats"]["entry_count"].is_number());
    assert!(event.payload["stats"]["file_count"].is_number());
    assert!(event.payload["stats"]["db_file_size_bytes"].is_number());

    handle.abort();
}

#[tokio::test]
async fn test_heartbeat_contains_all_stats_fields() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let handle = spawn_heartbeat(bus.clone(), engine, 1, CancellationToken::new());

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    ).await.unwrap().unwrap();

    let stats = &event.payload["stats"];
    // Verify all fields present
    assert!(stats.get("entry_count").is_some());
    assert!(stats.get("kv_entries").is_some());
    assert!(stats.get("chunk_count").is_some());
    assert!(stats.get("file_count").is_some());
    assert!(stats.get("directory_count").is_some());
    assert!(stats.get("snapshot_count").is_some());
    assert!(stats.get("fork_count").is_some());
    assert!(stats.get("void_count").is_some());
    assert!(stats.get("void_space_bytes").is_some());
    assert!(stats.get("db_file_size_bytes").is_some());
    assert!(stats.get("kv_size_bytes").is_some());
    assert!(stats.get("nvt_buckets").is_some());

    handle.abort();
}

#[tokio::test]
async fn test_heartbeat_abort_stops_emission() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());

    let handle = spawn_heartbeat(bus.clone(), engine, 1, CancellationToken::new());
    handle.abort();

    // Should not panic or cause issues
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[test]
fn test_delay_to_next_boundary_positive() {
    // Just verify it returns a positive duration
    let delay = aeordb::engine::heartbeat::delay_to_next_boundary();
    assert!(delay.as_millis() > 0);
    assert!(delay.as_secs() <= 15);
}

#[test]
fn test_delay_to_next_boundary_within_range() {
    // The delay should always be between 1ms and 15 seconds
    for _ in 0..10 {
        let delay = aeordb::engine::heartbeat::delay_to_next_boundary();
        assert!(delay.as_millis() >= 1, "delay should be at least 1ms");
        assert!(delay.as_secs() <= 15, "delay should be at most 15s");
    }
}

#[tokio::test]
async fn test_heartbeat_no_subscribers_does_not_panic() {
    // Spawn heartbeat with no subscribers -- events should be silently dropped.
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());

    let handle = spawn_heartbeat(bus.clone(), engine, 1, CancellationToken::new());

    // Let it run briefly without any subscriber
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    handle.abort();
    // If we get here without panic, the test passes.
}

#[tokio::test]
async fn test_heartbeat_event_has_valid_envelope() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let handle = spawn_heartbeat(bus.clone(), engine, 1, CancellationToken::new());

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    ).await.unwrap().unwrap();

    // Verify envelope metadata
    assert!(!event.event_id.is_empty(), "event_id should not be empty");
    assert!(event.timestamp > 0, "timestamp should be positive");
    assert_eq!(event.event_type, "heartbeat");
    assert_eq!(event.user_id, "system");

    // Verify event_id looks like a UUID (36 chars with hyphens)
    assert_eq!(event.event_id.len(), 36);
    assert_eq!(event.event_id.chars().filter(|c| *c == '-').count(), 4);

    handle.abort();
}
