use std::sync::Arc;
use aeordb::engine::{
    EventBus, EngineEvent, RequestContext,
    EntryEventData, VersionEventData, HeartbeatData,
    EVENT_ENTRIES_CREATED, EVENT_ENTRIES_UPDATED, EVENT_ENTRIES_DELETED,
    EVENT_VERSIONS_CREATED, EVENT_VERSIONS_DELETED, EVENT_VERSIONS_PROMOTED, EVENT_VERSIONS_RESTORED,
    EVENT_USERS_CREATED, EVENT_USERS_ACTIVATED, EVENT_USERS_DEACTIVATED,
    EVENT_PERMISSIONS_CHANGED, EVENT_IMPORTS_COMPLETED, EVENT_INDEXES_UPDATED, EVENT_ERRORS,
    EVENT_TOKENS_EXCHANGED, EVENT_API_KEYS_CREATED, EVENT_API_KEYS_REVOKED,
    EVENT_PLUGINS_DEPLOYED, EVENT_PLUGINS_REMOVED, EVENT_HEARTBEAT,
};
use tokio::sync::broadcast::error::TryRecvError;

// --- EventBus tests ---

#[tokio::test]
async fn test_event_bus_emit_receive() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe();

    let event = EngineEvent::new("test_event", "user1", serde_json::json!({"key": "value"}));
    bus.emit(event);

    let received = rx.recv().await.unwrap();
    assert_eq!(received.event_type, "test_event");
    assert_eq!(received.user_id, "user1");
    assert_eq!(received.payload["key"], "value");
}

#[tokio::test]
async fn test_event_bus_no_subscribers() {
    // Emit with no subscribers should not panic
    let bus = EventBus::new();
    let event = EngineEvent::new("test_event", "user1", serde_json::json!({}));
    bus.emit(event); // should silently drop
    assert_eq!(bus.subscriber_count(), 0);
}

#[tokio::test]
async fn test_event_bus_multiple_subscribers() {
    let bus = EventBus::new();
    let mut rx1 = bus.subscribe();
    let mut rx2 = bus.subscribe();

    let event = EngineEvent::new("multi_test", "user1", serde_json::json!({"n": 42}));
    bus.emit(event);

    let r1 = rx1.recv().await.unwrap();
    let r2 = rx2.recv().await.unwrap();
    assert_eq!(r1.event_type, "multi_test");
    assert_eq!(r2.event_type, "multi_test");
    assert_eq!(r1.event_id, r2.event_id); // same event
}

#[tokio::test]
async fn test_event_bus_subscriber_dropped() {
    let bus = EventBus::new();
    {
        let _rx = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
    }
    // subscriber dropped
    assert_eq!(bus.subscriber_count(), 0);
    // emit after drop should not panic
    let event = EngineEvent::new("test", "sys", serde_json::json!({}));
    bus.emit(event);
}

#[tokio::test]
async fn test_event_bus_capacity() {
    // Create a bus with capacity 2
    let bus = EventBus::with_capacity(2);
    let mut rx = bus.subscribe();

    // Emit 4 events — subscriber should lag
    for i in 0..4 {
        bus.emit(EngineEvent::new("cap_test", "sys", serde_json::json!({"i": i})));
    }

    // First recv should get a Lagged error since the buffer overflowed
    let result = rx.recv().await;
    match result {
        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
            assert!(n > 0, "should have lagged by at least 1");
        }
        Ok(evt) => {
            // Depending on timing, we might get the latest events
            // The important thing is we don't panic
            assert_eq!(evt.event_type, "cap_test");
        }
        Err(other) => panic!("unexpected error: {:?}", other),
    }
}

#[tokio::test]
async fn test_event_bus_subscriber_count() {
    let bus = EventBus::new();
    assert_eq!(bus.subscriber_count(), 0);

    let rx1 = bus.subscribe();
    assert_eq!(bus.subscriber_count(), 1);

    let rx2 = bus.subscribe();
    assert_eq!(bus.subscriber_count(), 2);

    drop(rx1);
    assert_eq!(bus.subscriber_count(), 1);

    drop(rx2);
    assert_eq!(bus.subscriber_count(), 0);
}

#[tokio::test]
async fn test_event_bus_default() {
    let bus = EventBus::default();
    assert_eq!(bus.subscriber_count(), 0);
    // Should work identically to ::new()
    let mut rx = bus.subscribe();
    bus.emit(EngineEvent::new("default_test", "sys", serde_json::json!({})));
    let received = rx.recv().await.unwrap();
    assert_eq!(received.event_type, "default_test");
}

#[tokio::test]
async fn test_event_bus_debug() {
    let bus = EventBus::new();
    let _rx = bus.subscribe();
    let debug_str = format!("{:?}", bus);
    assert!(debug_str.contains("EventBus"));
    assert!(debug_str.contains("subscriber_count"));
}

#[tokio::test]
async fn test_event_bus_receiver_empty() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    // No events emitted — try_recv should return Empty
    match rx.try_recv() {
        Err(TryRecvError::Empty) => {} // expected
        other => panic!("expected Empty, got {:?}", other),
    }
}

// --- EngineEvent tests ---

#[tokio::test]
async fn test_engine_event_new() {
    let event = EngineEvent::new("test_type", "user42", serde_json::json!({"a": 1}));
    assert_eq!(event.event_type, "test_type");
    assert_eq!(event.user_id, "user42");
    assert!(!event.event_id.is_empty());
    // event_id should be a valid UUID (36 chars with hyphens)
    assert_eq!(event.event_id.len(), 36);
    assert!(event.timestamp > 0);
    assert_eq!(event.payload["a"], 1);
}

#[tokio::test]
async fn test_engine_event_unique_ids() {
    let e1 = EngineEvent::new("t", "u", serde_json::json!({}));
    let e2 = EngineEvent::new("t", "u", serde_json::json!({}));
    assert_ne!(e1.event_id, e2.event_id);
}

#[tokio::test]
async fn test_engine_event_serialize() {
    let event = EngineEvent::new("test_type", "user1", serde_json::json!({"key": "val"}));
    let json_str = serde_json::to_string(&event).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed["event_type"], "test_type");
    assert_eq!(parsed["user_id"], "user1");
    assert_eq!(parsed["payload"]["key"], "val");
    assert!(parsed["event_id"].is_string());
    assert!(parsed["timestamp"].is_i64());
}

#[tokio::test]
async fn test_engine_event_clone() {
    let event = EngineEvent::new("clone_test", "u", serde_json::json!({"x": 1}));
    let cloned = event.clone();
    assert_eq!(event.event_id, cloned.event_id);
    assert_eq!(event.event_type, cloned.event_type);
    assert_eq!(event.timestamp, cloned.timestamp);
}

// --- Payload data struct serialization tests ---

#[tokio::test]
async fn test_entry_event_data_serialize() {
    let data = EntryEventData {
        path: "/docs/readme.md".to_string(),
        entry_type: "file".to_string(),
        content_type: Some("text/markdown".to_string()),
        size: 1024,
        hash: "abc123".to_string(),
        created_at: 1000,
        updated_at: 2000,
        previous_hash: None,
    };
    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["path"], "/docs/readme.md");
    assert_eq!(json["entry_type"], "file");
    assert_eq!(json["content_type"], "text/markdown");
    assert_eq!(json["size"], 1024);
    assert_eq!(json["hash"], "abc123");
    // previous_hash should be skipped when None
    assert!(json.get("previous_hash").is_none());
}

#[tokio::test]
async fn test_entry_event_data_with_previous_hash() {
    let data = EntryEventData {
        path: "/a".to_string(),
        entry_type: "file".to_string(),
        content_type: None,
        size: 0,
        hash: "new".to_string(),
        created_at: 0,
        updated_at: 0,
        previous_hash: Some("old".to_string()),
    };
    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["previous_hash"], "old");
}

#[tokio::test]
async fn test_version_event_data_serialize() {
    let data = VersionEventData {
        name: "v1.0".to_string(),
        version_type: Some("snapshot".to_string()),
        root_hash: "roothash".to_string(),
        created_at: Some(12345),
    };
    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["name"], "v1.0");
    assert_eq!(json["version_type"], "snapshot");
    assert_eq!(json["root_hash"], "roothash");
    assert_eq!(json["created_at"], 12345);
}

#[tokio::test]
async fn test_version_event_data_optional_fields_skipped() {
    let data = VersionEventData {
        name: "promoted".to_string(),
        version_type: None,
        root_hash: "hash".to_string(),
        created_at: None,
    };
    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["name"], "promoted");
    // version_type is not skip_serializing_if, so it will be null
    assert!(json["version_type"].is_null());
    // created_at uses skip_serializing_if
    assert!(json.get("created_at").is_none());
}

#[tokio::test]
async fn test_heartbeat_data_serialize() {
    let data = HeartbeatData {
        entry_count: 100,
        kv_entries: 50,
        chunk_count: 30,
        file_count: 20,
        directory_count: 10,
        snapshot_count: 5,
        fork_count: 2,
        void_count: 1,
        void_space_bytes: 4096,
        db_file_size_bytes: 1_000_000,
        kv_size_bytes: 50_000,
        nvt_buckets: 1024,
        intent_time: 1700000000000,
        construct_time: 1700000000005,
        node_id: 1,
    };
    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["entry_count"], 100);
    assert_eq!(json["kv_entries"], 50);
    assert_eq!(json["chunk_count"], 30);
    assert_eq!(json["file_count"], 20);
    assert_eq!(json["directory_count"], 10);
    assert_eq!(json["snapshot_count"], 5);
    assert_eq!(json["fork_count"], 2);
    assert_eq!(json["void_count"], 1);
    assert_eq!(json["void_space_bytes"], 4096);
    assert_eq!(json["db_file_size_bytes"], 1_000_000);
    assert_eq!(json["kv_size_bytes"], 50_000);
    assert_eq!(json["nvt_buckets"], 1024);
}

// --- Event type constant tests ---

#[test]
fn test_event_type_constants() {
    // Verify all 20 event type constants are distinct non-empty strings
    let all_types: Vec<&str> = vec![
        EVENT_ENTRIES_CREATED, EVENT_ENTRIES_UPDATED, EVENT_ENTRIES_DELETED,
        EVENT_VERSIONS_CREATED, EVENT_VERSIONS_DELETED, EVENT_VERSIONS_PROMOTED, EVENT_VERSIONS_RESTORED,
        EVENT_USERS_CREATED, EVENT_USERS_ACTIVATED, EVENT_USERS_DEACTIVATED,
        EVENT_PERMISSIONS_CHANGED, EVENT_IMPORTS_COMPLETED, EVENT_INDEXES_UPDATED, EVENT_ERRORS,
        EVENT_TOKENS_EXCHANGED, EVENT_API_KEYS_CREATED, EVENT_API_KEYS_REVOKED,
        EVENT_PLUGINS_DEPLOYED, EVENT_PLUGINS_REMOVED, EVENT_HEARTBEAT,
    ];
    assert_eq!(all_types.len(), 20);
    for t in &all_types {
        assert!(!t.is_empty());
    }
    // All unique
    let mut deduped = all_types.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), all_types.len(), "event type constants must be unique");
}

// --- RequestContext tests ---

#[tokio::test]
async fn test_request_context_system() {
    let ctx = RequestContext::system();
    assert_eq!(ctx.user_id, "system");
    assert!(!ctx.events_enabled());
    assert!(ctx.event_bus().is_none());
}

#[tokio::test]
async fn test_request_context_with_bus() {
    let bus = Arc::new(EventBus::new());
    let ctx = RequestContext::with_bus(bus);
    assert_eq!(ctx.user_id, "system");
    assert!(ctx.events_enabled());
    assert!(ctx.event_bus().is_some());
}

#[tokio::test]
async fn test_request_context_from_claims() {
    let bus = Arc::new(EventBus::new());
    let ctx = RequestContext::from_claims("user-abc-123", bus);
    assert_eq!(ctx.user_id, "user-abc-123");
    assert!(ctx.events_enabled());
}

#[tokio::test]
async fn test_request_context_emit_no_bus() {
    // Emit on system context should be a silent no-op
    let ctx = RequestContext::system();
    ctx.emit("test_event", serde_json::json!({"data": 1}));
    // No panic, no way to observe the event — that's the point
}

#[tokio::test]
async fn test_request_context_emit_with_bus() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let ctx = RequestContext::from_claims("alice", bus);

    ctx.emit("entries_created", serde_json::json!({"path": "/test"}));

    let received = rx.recv().await.unwrap();
    assert_eq!(received.event_type, "entries_created");
    assert_eq!(received.user_id, "alice");
    assert_eq!(received.payload["path"], "/test");
}

#[tokio::test]
async fn test_request_context_emit_multiple_events() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let ctx = RequestContext::from_claims("bob", bus);

    ctx.emit("event_a", serde_json::json!({"n": 1}));
    ctx.emit("event_b", serde_json::json!({"n": 2}));

    let r1 = rx.recv().await.unwrap();
    let r2 = rx.recv().await.unwrap();
    assert_eq!(r1.event_type, "event_a");
    assert_eq!(r2.event_type, "event_b");
    assert_ne!(r1.event_id, r2.event_id);
}

#[tokio::test]
async fn test_request_context_debug() {
    let ctx = RequestContext::system();
    let debug_str = format!("{:?}", ctx);
    assert!(debug_str.contains("RequestContext"));
    assert!(debug_str.contains("user_id"));
    assert!(debug_str.contains("system"));
    assert!(debug_str.contains("events_enabled"));

    let bus = Arc::new(EventBus::new());
    let ctx2 = RequestContext::from_claims("alice", bus);
    let debug_str2 = format!("{:?}", ctx2);
    assert!(debug_str2.contains("alice"));
    assert!(debug_str2.contains("true"));
}

#[tokio::test]
async fn test_request_context_with_bus_system_user() {
    // with_bus should set user_id to "system"
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let ctx = RequestContext::with_bus(bus);

    ctx.emit("sys_event", serde_json::json!({}));

    let received = rx.recv().await.unwrap();
    assert_eq!(received.user_id, "system");
}

// --- Edge case / failure path tests ---

#[tokio::test]
async fn test_event_bus_emit_empty_payload() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    bus.emit(EngineEvent::new("empty", "u", serde_json::json!(null)));
    let received = rx.recv().await.unwrap();
    assert!(received.payload.is_null());
}

#[tokio::test]
async fn test_event_bus_emit_large_payload() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    let big_payload = serde_json::json!({
        "data": "x".repeat(100_000),
    });
    bus.emit(EngineEvent::new("big", "u", big_payload));
    let received = rx.recv().await.unwrap();
    assert_eq!(received.payload["data"].as_str().unwrap().len(), 100_000);
}

#[tokio::test]
async fn test_event_bus_emit_special_chars() {
    let bus = EventBus::new();
    let mut rx = bus.subscribe();
    bus.emit(EngineEvent::new(
        "special/type",
        "user with spaces",
        serde_json::json!({"emoji": "\u{1f680}", "newline": "a\nb"}),
    ));
    let received = rx.recv().await.unwrap();
    assert_eq!(received.event_type, "special/type");
    assert_eq!(received.user_id, "user with spaces");
}

#[tokio::test]
async fn test_event_bus_many_subscribers() {
    let bus = EventBus::new();
    let mut receivers: Vec<_> = (0..100).map(|_| bus.subscribe()).collect();
    assert_eq!(bus.subscriber_count(), 100);

    bus.emit(EngineEvent::new("broadcast", "sys", serde_json::json!({})));

    for rx in &mut receivers {
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.event_type, "broadcast");
    }
}

#[tokio::test]
async fn test_request_context_bus_shared_across_contexts() {
    // Multiple contexts can share the same bus
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let ctx1 = RequestContext::from_claims("alice", Arc::clone(&bus));
    let ctx2 = RequestContext::from_claims("bob", Arc::clone(&bus));

    ctx1.emit("from_alice", serde_json::json!({}));
    ctx2.emit("from_bob", serde_json::json!({}));

    let r1 = rx.recv().await.unwrap();
    let r2 = rx.recv().await.unwrap();
    assert_eq!(r1.user_id, "alice");
    assert_eq!(r2.user_id, "bob");
}
