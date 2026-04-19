use std::sync::Arc;
use aeordb::engine::EventBus;
use aeordb::engine::heartbeat::spawn_heartbeat;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_heartbeat_emits_event() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let handle = spawn_heartbeat(bus.clone(), 1, CancellationToken::new());

    // Wait for first heartbeat (max ~20 seconds for alignment + one interval)
    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    ).await.expect("should receive heartbeat within 20s")
     .expect("recv should succeed");

    assert_eq!(event.event_type, "heartbeat");
    assert_eq!(event.user_id, "system");
    // Clock-sync fields should be present under "clock"
    assert!(event.payload["clock"]["intent_time"].is_number());
    assert!(event.payload["clock"]["construct_time"].is_number());
    assert!(event.payload["clock"]["node_id"].is_number());

    handle.abort();
}

#[tokio::test]
async fn test_heartbeat_contains_only_clock_fields() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let handle = spawn_heartbeat(bus.clone(), 42, CancellationToken::new());

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    ).await.unwrap().unwrap();

    let clock = &event.payload["clock"];
    // Verify only clock-sync fields are present
    assert!(clock.get("intent_time").is_some());
    assert!(clock.get("construct_time").is_some());
    assert!(clock.get("node_id").is_some());
    // Verify stats fields are NOT present (stripped in Phase 3)
    assert!(clock.get("entry_count").is_none());
    assert!(clock.get("kv_entries").is_none());
    assert!(clock.get("chunk_count").is_none());
    assert!(clock.get("file_count").is_none());
    assert!(clock.get("directory_count").is_none());
    assert!(clock.get("snapshot_count").is_none());
    assert!(clock.get("fork_count").is_none());
    assert!(clock.get("void_count").is_none());
    assert!(clock.get("void_space_bytes").is_none());
    assert!(clock.get("db_file_size_bytes").is_none());
    assert!(clock.get("kv_size_bytes").is_none());
    assert!(clock.get("nvt_buckets").is_none());

    // Verify node_id is correctly propagated
    assert_eq!(clock["node_id"].as_u64().unwrap(), 42);

    handle.abort();
}

#[tokio::test]
async fn test_heartbeat_abort_stops_emission() {
    let bus = Arc::new(EventBus::new());

    let handle = spawn_heartbeat(bus.clone(), 1, CancellationToken::new());
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
    let bus = Arc::new(EventBus::new());

    let handle = spawn_heartbeat(bus.clone(), 1, CancellationToken::new());

    // Let it run briefly without any subscriber
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    handle.abort();
    // If we get here without panic, the test passes.
}

#[tokio::test]
async fn test_heartbeat_event_has_valid_envelope() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let handle = spawn_heartbeat(bus.clone(), 1, CancellationToken::new());

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

#[tokio::test]
async fn test_heartbeat_intent_time_aligned_to_15s() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let handle = spawn_heartbeat(bus.clone(), 1, CancellationToken::new());

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    ).await.unwrap().unwrap();

    let intent_time = event.payload["clock"]["intent_time"].as_u64().unwrap();
    // intent_time should be aligned to a 15-second boundary (divisible by 15000ms)
    assert_eq!(intent_time % 15_000, 0, "intent_time should be aligned to 15s boundary");

    let construct_time = event.payload["clock"]["construct_time"].as_u64().unwrap();
    // construct_time should be >= intent_time (actual time is at or after the boundary)
    assert!(construct_time >= intent_time, "construct_time should be >= intent_time");
    // Drift should be small (less than 1 second)
    assert!(construct_time - intent_time < 1_000, "drift between intent and construct should be < 1s");

    handle.abort();
}
