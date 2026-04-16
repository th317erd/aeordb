use aeordb::engine::peer_connection::{ConnectionState, PeerConfig, PeerManager};
use aeordb::engine::virtual_clock::PeerClockStats;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_peer_config(node_id: u64, address: &str) -> PeerConfig {
    PeerConfig {
        node_id,
        address: address.to_string(),
        label: None,
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    }
}

fn make_peer_config_with_label(node_id: u64, address: &str, label: &str) -> PeerConfig {
    PeerConfig {
        node_id,
        address: address.to_string(),
        label: Some(label.to_string()),
        sync_paths: None,
        last_clock_offset_ms: None,
        last_wire_time_ms: None,
        last_jitter_ms: None,
        clock_state_at: None,
    }
}

// ===========================================================================
// PeerManager: add / get / remove
// ===========================================================================

#[test]
fn test_add_peer_starts_disconnected() {
    let manager = PeerManager::new();
    let config = make_peer_config(1, "127.0.0.1:9000");
    let connection = manager.add_peer(&config);

    assert_eq!(connection.node_id, 1);
    assert_eq!(connection.address, "127.0.0.1:9000");
    assert_eq!(connection.state, ConnectionState::Disconnected);
    assert!(connection.clock_stats.is_none());
    assert!(connection.last_synced_root_hash.is_none());
    assert!(connection.last_sync_at.is_none());
}

#[test]
fn test_add_peer_with_label() {
    let manager = PeerManager::new();
    let config = make_peer_config_with_label(42, "10.0.0.5:9000", "us-west-replica");
    let connection = manager.add_peer(&config);

    assert_eq!(connection.label, Some("us-west-replica".to_string()));
}

#[test]
fn test_get_peer_returns_none_for_missing() {
    let manager = PeerManager::new();
    assert!(manager.get_peer(999).is_none());
}

#[test]
fn test_get_peer_returns_connection() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    let peer = manager.get_peer(1).expect("peer should exist");
    assert_eq!(peer.node_id, 1);
    assert_eq!(peer.address, "127.0.0.1:9000");
}

#[test]
fn test_remove_peer_returns_true_when_found() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    assert!(manager.remove_peer(1));
    assert!(manager.get_peer(1).is_none());
}

#[test]
fn test_remove_peer_returns_false_when_missing() {
    let manager = PeerManager::new();
    assert!(!manager.remove_peer(999));
}

#[test]
fn test_add_peer_overwrites_existing() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "old:9000"));

    // Start honeymoon to change state
    manager.start_honeymoon(1, 100);
    let peer = manager.get_peer(1).unwrap();
    assert!(matches!(peer.state, ConnectionState::Honeymoon { .. }));

    // Re-adding should reset to Disconnected
    manager.add_peer(&make_peer_config(1, "new:9000"));
    let peer = manager.get_peer(1).unwrap();
    assert_eq!(peer.state, ConnectionState::Disconnected);
    assert_eq!(peer.address, "new:9000");
}

// ===========================================================================
// all_peers
// ===========================================================================

#[test]
fn test_all_peers_empty() {
    let manager = PeerManager::new();
    assert!(manager.all_peers().is_empty());
}

#[test]
fn test_all_peers_returns_all() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "a:9000"));
    manager.add_peer(&make_peer_config(2, "b:9000"));
    manager.add_peer(&make_peer_config(3, "c:9000"));

    let peers = manager.all_peers();
    assert_eq!(peers.len(), 3);

    let mut node_ids: Vec<u64> = peers.iter().map(|peer| peer.node_id).collect();
    node_ids.sort();
    assert_eq!(node_ids, vec![1, 2, 3]);
}

// ===========================================================================
// Honeymoon lifecycle: Disconnected -> Honeymoon -> Active
// ===========================================================================

#[test]
fn test_honeymoon_lifecycle() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    // Disconnected -> Honeymoon
    manager.start_honeymoon(1, 1000);
    let peer = manager.get_peer(1).unwrap();
    assert_eq!(
        peer.state,
        ConnectionState::Honeymoon {
            started_at: 1000,
            heartbeats_received: 0,
        }
    );

    // Record heartbeats
    assert_eq!(manager.record_honeymoon_heartbeat(1), Some(1));
    assert_eq!(manager.record_honeymoon_heartbeat(1), Some(2));
    assert_eq!(manager.record_honeymoon_heartbeat(1), Some(3));

    let peer = manager.get_peer(1).unwrap();
    assert_eq!(
        peer.state,
        ConnectionState::Honeymoon {
            started_at: 1000,
            heartbeats_received: 3,
        }
    );

    // Honeymoon -> Active
    manager.activate_peer(1);
    let peer = manager.get_peer(1).unwrap();
    assert_eq!(peer.state, ConnectionState::Active);
}

// ===========================================================================
// Disconnect resets state
// ===========================================================================

#[test]
fn test_disconnect_from_active() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));
    manager.start_honeymoon(1, 1000);
    manager.activate_peer(1);

    assert_eq!(
        manager.get_peer(1).unwrap().state,
        ConnectionState::Active
    );

    manager.disconnect_peer(1);
    assert_eq!(
        manager.get_peer(1).unwrap().state,
        ConnectionState::Disconnected
    );
}

#[test]
fn test_disconnect_from_honeymoon() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));
    manager.start_honeymoon(1, 1000);

    manager.disconnect_peer(1);
    assert_eq!(
        manager.get_peer(1).unwrap().state,
        ConnectionState::Disconnected
    );
}

// ===========================================================================
// Heartbeat edge cases
// ===========================================================================

#[test]
fn test_record_honeymoon_heartbeat_increments() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));
    manager.start_honeymoon(1, 500);

    for expected in 1..=10 {
        assert_eq!(manager.record_honeymoon_heartbeat(1), Some(expected));
    }
}

#[test]
fn test_record_honeymoon_heartbeat_returns_none_when_not_in_honeymoon() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    // Disconnected state -- should return None
    assert_eq!(manager.record_honeymoon_heartbeat(1), None);

    // Active state -- should also return None
    manager.start_honeymoon(1, 1000);
    manager.activate_peer(1);
    assert_eq!(manager.record_honeymoon_heartbeat(1), None);
}

#[test]
fn test_record_honeymoon_heartbeat_returns_none_for_missing_peer() {
    let manager = PeerManager::new();
    assert_eq!(manager.record_honeymoon_heartbeat(999), None);
}

// ===========================================================================
// State transitions on missing peers are no-ops
// ===========================================================================

#[test]
fn test_start_honeymoon_on_missing_peer_is_noop() {
    let manager = PeerManager::new();
    manager.start_honeymoon(999, 1000);
    // No panic, no peer created
    assert!(manager.get_peer(999).is_none());
}

#[test]
fn test_activate_missing_peer_is_noop() {
    let manager = PeerManager::new();
    manager.activate_peer(999);
    assert!(manager.get_peer(999).is_none());
}

#[test]
fn test_disconnect_missing_peer_is_noop() {
    let manager = PeerManager::new();
    manager.disconnect_peer(999);
    assert!(manager.get_peer(999).is_none());
}

// ===========================================================================
// Clock stats
// ===========================================================================

#[test]
fn test_update_clock_stats() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    let stats = PeerClockStats {
        clock_offset_ms: 5.0,
        wire_time_ms: 12.5,
        jitter_ms: 2.3,
        samples: 10,
        last_updated_ms: 99999,
    };
    manager.update_clock_stats(1, stats.clone());

    let peer = manager.get_peer(1).unwrap();
    let peer_stats = peer.clock_stats.expect("should have stats");
    assert!((peer_stats.clock_offset_ms - 5.0).abs() < f64::EPSILON);
    assert!((peer_stats.wire_time_ms - 12.5).abs() < f64::EPSILON);
    assert!((peer_stats.jitter_ms - 2.3).abs() < f64::EPSILON);
    assert_eq!(peer_stats.samples, 10);
    assert_eq!(peer_stats.last_updated_ms, 99999);
}

#[test]
fn test_update_clock_stats_on_missing_peer_is_noop() {
    let manager = PeerManager::new();
    let stats = PeerClockStats {
        clock_offset_ms: 0.0,
        wire_time_ms: 0.0,
        jitter_ms: 0.0,
        samples: 0,
        last_updated_ms: 0,
    };
    manager.update_clock_stats(999, stats);
    assert!(manager.get_peer(999).is_none());
}

// ===========================================================================
// Sync state
// ===========================================================================

#[test]
fn test_update_sync_state() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    let root_hash = vec![0xDE, 0xAD, 0xBE, 0xEF];
    manager.update_sync_state(1, root_hash.clone(), 50000);

    let peer = manager.get_peer(1).unwrap();
    assert_eq!(peer.last_synced_root_hash, Some(root_hash));
    assert_eq!(peer.last_sync_at, Some(50000));
}

#[test]
fn test_update_sync_state_on_missing_peer_is_noop() {
    let manager = PeerManager::new();
    manager.update_sync_state(999, vec![1, 2, 3], 1000);
    assert!(manager.get_peer(999).is_none());
}

// ===========================================================================
// PeerConfig serialization round-trip
// ===========================================================================

#[test]
fn test_peer_config_serialization_round_trip() {
    let config = PeerConfig {
        node_id: 12345,
        address: "10.0.0.1:9000".to_string(),
        label: Some("primary-replica".to_string()),
        sync_paths: Some(vec!["/data".to_string(), "/config".to_string()]),
        last_clock_offset_ms: Some(3.14),
        last_wire_time_ms: Some(7.5),
        last_jitter_ms: Some(1.2),
        clock_state_at: Some(99999),
    };

    let serialized = serde_json::to_vec(&config).expect("serialize");
    let deserialized: PeerConfig = serde_json::from_slice(&serialized).expect("deserialize");

    assert_eq!(deserialized.node_id, 12345);
    assert_eq!(deserialized.address, "10.0.0.1:9000");
    assert_eq!(deserialized.label, Some("primary-replica".to_string()));
    assert_eq!(
        deserialized.sync_paths,
        Some(vec!["/data".to_string(), "/config".to_string()])
    );
    assert!((deserialized.last_clock_offset_ms.unwrap() - 3.14).abs() < f64::EPSILON);
}

#[test]
fn test_peer_config_serialization_with_none_fields() {
    let config = make_peer_config(1, "host:9000");
    let serialized = serde_json::to_vec(&config).expect("serialize");
    let deserialized: PeerConfig = serde_json::from_slice(&serialized).expect("deserialize");

    assert_eq!(deserialized.node_id, 1);
    assert!(deserialized.label.is_none());
    assert!(deserialized.sync_paths.is_none());
    assert!(deserialized.last_clock_offset_ms.is_none());
}

// ===========================================================================
// Multiple configs serialization (as stored in system tables)
// ===========================================================================

#[test]
fn test_peer_configs_vec_serialization() {
    let configs = vec![
        make_peer_config(1, "a:9000"),
        make_peer_config(2, "b:9000"),
        make_peer_config(3, "c:9000"),
    ];

    let serialized = serde_json::to_vec(&configs).expect("serialize");
    let deserialized: Vec<PeerConfig> = serde_json::from_slice(&serialized).expect("deserialize");

    assert_eq!(deserialized.len(), 3);
    assert_eq!(deserialized[0].node_id, 1);
    assert_eq!(deserialized[1].node_id, 2);
    assert_eq!(deserialized[2].node_id, 3);
}

// ===========================================================================
// ConnectionState equality
// ===========================================================================

#[test]
fn test_connection_state_equality() {
    assert_eq!(ConnectionState::Disconnected, ConnectionState::Disconnected);
    assert_eq!(ConnectionState::Active, ConnectionState::Active);
    assert_eq!(
        ConnectionState::Honeymoon {
            started_at: 100,
            heartbeats_received: 5
        },
        ConnectionState::Honeymoon {
            started_at: 100,
            heartbeats_received: 5
        }
    );
    assert_ne!(ConnectionState::Disconnected, ConnectionState::Active);
    assert_ne!(
        ConnectionState::Honeymoon {
            started_at: 100,
            heartbeats_received: 5
        },
        ConnectionState::Honeymoon {
            started_at: 200,
            heartbeats_received: 5
        }
    );
}
