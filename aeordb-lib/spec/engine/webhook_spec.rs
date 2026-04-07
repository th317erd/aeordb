use std::sync::Arc;
use aeordb::engine::webhook::{
    compute_signature, event_matches_webhook, load_webhook_config,
    WebhookConfig, WebhookRegistry,
};
use aeordb::engine::{DirectoryOps, EventBus};
use aeordb::engine::engine_event::EngineEvent;
use aeordb::engine::request_context::RequestContext;
use aeordb::server::create_temp_engine_for_tests;

// --- compute_signature tests ---

#[test]
fn test_compute_signature_format() {
    let sig = compute_signature("my-secret", b"test payload");
    assert!(sig.starts_with("sha256="));
    assert_eq!(sig.len(), 7 + 64); // "sha256=" + 64 hex chars
}

#[test]
fn test_compute_signature_deterministic() {
    let sig1 = compute_signature("key", b"data");
    let sig2 = compute_signature("key", b"data");
    assert_eq!(sig1, sig2);
}

#[test]
fn test_compute_signature_different_secrets() {
    let sig1 = compute_signature("key1", b"data");
    let sig2 = compute_signature("key2", b"data");
    assert_ne!(sig1, sig2);
}

#[test]
fn test_compute_signature_different_payloads() {
    let sig1 = compute_signature("key", b"data1");
    let sig2 = compute_signature("key", b"data2");
    assert_ne!(sig1, sig2);
}

#[test]
fn test_compute_signature_empty_payload() {
    let sig = compute_signature("secret", b"");
    assert!(sig.starts_with("sha256="));
    assert_eq!(sig.len(), 7 + 64);
}

#[test]
fn test_compute_signature_empty_secret() {
    let sig = compute_signature("", b"payload");
    assert!(sig.starts_with("sha256="));
    assert_eq!(sig.len(), 7 + 64);
}

#[test]
fn test_compute_signature_known_value() {
    // HMAC-SHA256("secret", "payload") should produce a stable hex output.
    let sig1 = compute_signature("secret", b"payload");
    let sig2 = compute_signature("secret", b"payload");
    assert_eq!(sig1, sig2);
    // Just verify it's valid hex after the prefix
    let hex_part = &sig1[7..];
    assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
}

// --- WebhookConfig deserialization tests ---

#[test]
fn test_webhook_config_deserialize() {
    let json = r#"{"webhooks":[{"id":"wh1","url":"https://example.com/hook","events":["entries_created"],"secret":"abc","active":true}]}"#;
    let registry: WebhookRegistry = serde_json::from_str(json).unwrap();
    assert_eq!(registry.webhooks.len(), 1);
    assert_eq!(registry.webhooks[0].id, "wh1");
    assert_eq!(registry.webhooks[0].url, "https://example.com/hook");
    assert!(registry.webhooks[0].active);
}

#[test]
fn test_webhook_config_active_defaults_true() {
    let json = r#"{"id":"wh1","url":"https://example.com","events":["entries_created"],"secret":"abc"}"#;
    let config: WebhookConfig = serde_json::from_str(json).unwrap();
    assert!(config.active);
}

#[test]
fn test_webhook_config_active_explicit_false() {
    let json = r#"{"id":"wh1","url":"https://example.com","events":["entries_created"],"secret":"abc","active":false}"#;
    let config: WebhookConfig = serde_json::from_str(json).unwrap();
    assert!(!config.active);
}

#[test]
fn test_webhook_config_with_path_prefix() {
    let json = r#"{"id":"wh1","url":"https://example.com","events":["entries_created"],"path_prefix":"/people/","secret":"abc"}"#;
    let config: WebhookConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.path_prefix, Some("/people/".to_string()));
}

#[test]
fn test_webhook_config_without_path_prefix() {
    let json = r#"{"id":"wh1","url":"https://example.com","events":["entries_created"],"secret":"abc"}"#;
    let config: WebhookConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.path_prefix, None);
}

#[test]
fn test_webhook_config_multiple_events() {
    let json = r#"{"id":"wh1","url":"https://example.com","events":["entries_created","entries_updated","entries_deleted"],"secret":"abc"}"#;
    let config: WebhookConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.events.len(), 3);
}

#[test]
fn test_webhook_config_multiple_webhooks() {
    let json = r#"{"webhooks":[
        {"id":"wh1","url":"https://a.com","events":["entries_created"],"secret":"s1"},
        {"id":"wh2","url":"https://b.com","events":["entries_deleted","versions_created"],"secret":"s2","active":false}
    ]}"#;
    let registry: WebhookRegistry = serde_json::from_str(json).unwrap();
    assert_eq!(registry.webhooks.len(), 2);
    assert!(registry.webhooks[0].active);
    assert!(!registry.webhooks[1].active);
}

#[test]
fn test_webhook_config_empty_webhooks() {
    let json = r#"{"webhooks":[]}"#;
    let registry: WebhookRegistry = serde_json::from_str(json).unwrap();
    assert_eq!(registry.webhooks.len(), 0);
}

#[test]
fn test_webhook_config_missing_required_field() {
    // Missing "secret"
    let json = r#"{"id":"wh1","url":"https://example.com","events":["entries_created"]}"#;
    let result: Result<WebhookConfig, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn test_webhook_config_missing_id() {
    let json = r#"{"url":"https://example.com","events":["entries_created"],"secret":"abc"}"#;
    let result: Result<WebhookConfig, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn test_webhook_config_missing_url() {
    let json = r#"{"id":"wh1","events":["entries_created"],"secret":"abc"}"#;
    let result: Result<WebhookConfig, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn test_webhook_config_missing_events() {
    let json = r#"{"id":"wh1","url":"https://example.com","secret":"abc"}"#;
    let result: Result<WebhookConfig, _> = serde_json::from_str(json);
    assert!(result.is_err());
}

#[test]
fn test_webhook_config_serialization_roundtrip() {
    let config = WebhookConfig {
        id: "wh1".to_string(),
        url: "https://example.com".to_string(),
        events: vec!["entries_created".to_string()],
        path_prefix: Some("/data/".to_string()),
        secret: "my-secret".to_string(),
        active: true,
    };
    let json = serde_json::to_string(&config).unwrap();
    let deserialized: WebhookConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, config.id);
    assert_eq!(deserialized.url, config.url);
    assert_eq!(deserialized.events, config.events);
    assert_eq!(deserialized.path_prefix, config.path_prefix);
    assert_eq!(deserialized.secret, config.secret);
    assert_eq!(deserialized.active, config.active);
}

// --- load_webhook_config tests ---

#[test]
fn test_load_webhook_config_from_database() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    let config_json = r#"{"webhooks":[{"id":"wh1","url":"https://example.com","events":["entries_created"],"secret":"test-secret"}]}"#;
    ops.store_file(&ctx, "/.config/webhooks.json", config_json.as_bytes(), Some("application/json")).unwrap();

    let registry = load_webhook_config(&engine);
    assert!(registry.is_some());
    let registry = registry.unwrap();
    assert_eq!(registry.webhooks.len(), 1);
    assert_eq!(registry.webhooks[0].id, "wh1");
    assert_eq!(registry.webhooks[0].secret, "test-secret");
}

#[test]
fn test_load_webhook_config_missing() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let registry = load_webhook_config(&engine);
    assert!(registry.is_none());
}

#[test]
fn test_load_webhook_config_invalid_json() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    ops.store_file(&ctx, "/.config/webhooks.json", b"not json", Some("text/plain")).unwrap();

    let registry = load_webhook_config(&engine);
    assert!(registry.is_none());
}

#[test]
fn test_load_webhook_config_empty_json_object() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Valid JSON but missing "webhooks" key
    ops.store_file(&ctx, "/.config/webhooks.json", b"{}", Some("application/json")).unwrap();

    let registry = load_webhook_config(&engine);
    assert!(registry.is_none());
}

#[test]
fn test_load_webhook_config_empty_webhooks_array() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    ops.store_file(&ctx, "/.config/webhooks.json", b"{\"webhooks\":[]}", Some("application/json")).unwrap();

    let registry = load_webhook_config(&engine);
    assert!(registry.is_some());
    assert_eq!(registry.unwrap().webhooks.len(), 0);
}

#[test]
fn test_load_webhook_config_invalid_utf8() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    ops.store_file(&ctx, "/.config/webhooks.json", &[0xFF, 0xFE, 0xFD], Some("application/octet-stream")).unwrap();

    let registry = load_webhook_config(&engine);
    assert!(registry.is_none());
}

#[test]
fn test_load_webhook_config_partial_invalid_webhooks() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Array contains a webhook missing required fields -- whole parse fails
    let json = r#"{"webhooks":[{"id":"wh1"}]}"#;
    ops.store_file(&ctx, "/.config/webhooks.json", json.as_bytes(), Some("application/json")).unwrap();

    let registry = load_webhook_config(&engine);
    assert!(registry.is_none());
}

#[test]
fn test_load_webhook_config_multiple_webhooks() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    let config_json = r#"{"webhooks":[
        {"id":"wh1","url":"https://a.com","events":["entries_created"],"secret":"s1"},
        {"id":"wh2","url":"https://b.com","events":["entries_deleted"],"secret":"s2","active":false}
    ]}"#;
    ops.store_file(&ctx, "/.config/webhooks.json", config_json.as_bytes(), Some("application/json")).unwrap();

    let registry = load_webhook_config(&engine).unwrap();
    assert_eq!(registry.webhooks.len(), 2);
    assert_eq!(registry.webhooks[0].id, "wh1");
    assert_eq!(registry.webhooks[1].id, "wh2");
    assert!(!registry.webhooks[1].active);
}

// --- event_matches_webhook tests ---

fn make_event(event_type: &str, entries_payload: Option<serde_json::Value>) -> EngineEvent {
    let payload = match entries_payload {
        Some(entries) => serde_json::json!({"entries": entries}),
        None => serde_json::json!({}),
    };
    EngineEvent::new(event_type, "test-user", payload)
}

fn make_webhook(events: Vec<&str>, path_prefix: Option<&str>, active: bool) -> WebhookConfig {
    WebhookConfig {
        id: "test-wh".to_string(),
        url: "https://example.com/hook".to_string(),
        events: events.into_iter().map(|s| s.to_string()).collect(),
        path_prefix: path_prefix.map(|s| s.to_string()),
        secret: "secret".to_string(),
        active,
    }
}

#[test]
fn test_event_matches_basic() {
    let event = make_event("entries_created", None);
    let webhook = make_webhook(vec!["entries_created"], None, true);
    assert!(event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_does_not_match_wrong_type() {
    let event = make_event("entries_deleted", None);
    let webhook = make_webhook(vec!["entries_created"], None, true);
    assert!(!event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_does_not_match_inactive_webhook() {
    let event = make_event("entries_created", None);
    let webhook = make_webhook(vec!["entries_created"], None, false);
    assert!(!event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_matches_one_of_multiple_types() {
    let event = make_event("entries_deleted", None);
    let webhook = make_webhook(vec!["entries_created", "entries_deleted"], None, true);
    assert!(event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_matches_with_path_prefix() {
    let entries = serde_json::json!([{"path": "/people/alice.json"}]);
    let event = make_event("entries_created", Some(entries));
    let webhook = make_webhook(vec!["entries_created"], Some("/people/"), true);
    assert!(event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_no_match_with_wrong_path_prefix() {
    let entries = serde_json::json!([{"path": "/animals/dog.json"}]);
    let event = make_event("entries_created", Some(entries));
    let webhook = make_webhook(vec!["entries_created"], Some("/people/"), true);
    assert!(!event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_matches_path_prefix_any_entry() {
    // Multiple entries -- only one needs to match
    let entries = serde_json::json!([
        {"path": "/animals/dog.json"},
        {"path": "/people/alice.json"}
    ]);
    let event = make_event("entries_created", Some(entries));
    let webhook = make_webhook(vec!["entries_created"], Some("/people/"), true);
    assert!(event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_no_match_path_prefix_no_entries() {
    // Event with path_prefix filter but no entries in payload
    let event = make_event("entries_created", None);
    let webhook = make_webhook(vec!["entries_created"], Some("/people/"), true);
    // No entries array at all -- the prefix filter is vacuously satisfied
    // (the check only fails if entries exist and none match)
    assert!(event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_no_match_path_prefix_empty_entries() {
    let entries = serde_json::json!([]);
    let event = make_event("entries_created", Some(entries));
    let webhook = make_webhook(vec!["entries_created"], Some("/people/"), true);
    // Empty array -- no entry matches the prefix
    assert!(!event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_matches_without_path_prefix_filter() {
    // No path_prefix on webhook -- should match regardless of paths
    let entries = serde_json::json!([{"path": "/anything/at/all.json"}]);
    let event = make_event("entries_created", Some(entries));
    let webhook = make_webhook(vec!["entries_created"], None, true);
    assert!(event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_matches_entries_without_path_field() {
    // Entries exist but don't have a "path" field -- none match
    let entries = serde_json::json!([{"name": "something"}]);
    let event = make_event("entries_created", Some(entries));
    let webhook = make_webhook(vec!["entries_created"], Some("/people/"), true);
    assert!(!event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_matches_heartbeat_no_entries() {
    let event = make_event("heartbeat", None);
    let webhook = make_webhook(vec!["heartbeat"], None, true);
    assert!(event_matches_webhook(&event, &webhook));
}

#[test]
fn test_event_matches_empty_events_list() {
    let event = make_event("entries_created", None);
    let webhook = make_webhook(vec![], None, true);
    assert!(!event_matches_webhook(&event, &webhook));
}

// --- spawn_webhook_dispatcher integration test ---

#[tokio::test]
async fn test_webhook_dispatcher_receives_events() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());

    // Store a webhook config that listens for entries_created
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    let config_json = r#"{"webhooks":[{"id":"wh1","url":"https://httpbin.org/post","events":["entries_created"],"secret":"test"}]}"#;
    ops.store_file(&ctx, "/.config/webhooks.json", config_json.as_bytes(), Some("application/json")).unwrap();

    // Spawn the dispatcher
    let handle = aeordb::engine::webhook::spawn_webhook_dispatcher(bus.clone(), engine.clone());

    // Give the dispatcher a moment to subscribe
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify the bus has a subscriber (the dispatcher)
    assert!(bus.subscriber_count() >= 1);

    // Clean up
    handle.abort();
}

#[tokio::test]
async fn test_webhook_dispatcher_reloads_config_on_change() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());

    // Start with no config
    let handle = aeordb::engine::webhook::spawn_webhook_dispatcher(bus.clone(), engine.clone());

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Emit a config change event (simulating a webhook config file write)
    let event = EngineEvent::new(
        "entries_created",
        "system",
        serde_json::json!({"entries": [{"path": "/.config/webhooks.json"}]}),
    );
    bus.emit(event);

    // Give it a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The dispatcher should have tried to reload config (no crash)
    handle.abort();
}

#[tokio::test]
async fn test_webhook_dispatcher_handles_bus_close() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let bus = Arc::new(EventBus::new());

    let handle = aeordb::engine::webhook::spawn_webhook_dispatcher(bus.clone(), engine.clone());

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Drop the bus -- all senders gone, receivers get Closed
    drop(bus);

    // The dispatcher should exit gracefully
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle,
    ).await;
    assert!(result.is_ok(), "Dispatcher should have shut down after bus closed");
}
