use aeordb::engine::peer_connection::{PeerConfig, PeerManager, SyncStatus};

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

// ===========================================================================
// SyncStatus::new -- initial state is zeros/None
// ===========================================================================

#[test]
fn test_sync_status_new() {
    let status = SyncStatus::new();

    assert!(status.last_success_at.is_none());
    assert!(status.last_attempt_at.is_none());
    assert!(status.last_error.is_none());
    assert_eq!(status.consecutive_failures, 0);
    assert_eq!(status.total_syncs, 0);
    assert_eq!(status.total_failures, 0);
}

#[test]
fn test_sync_status_default_matches_new() {
    let from_new = SyncStatus::new();
    let from_default = SyncStatus::default();

    assert_eq!(from_new.consecutive_failures, from_default.consecutive_failures);
    assert_eq!(from_new.total_syncs, from_default.total_syncs);
    assert_eq!(from_new.total_failures, from_default.total_failures);
    assert!(from_default.last_success_at.is_none());
    assert!(from_default.last_attempt_at.is_none());
    assert!(from_default.last_error.is_none());
}

// ===========================================================================
// record_success -- resets failures, increments total
// ===========================================================================

#[test]
fn test_record_success() {
    let mut status = SyncStatus::new();

    status.record_success();

    assert!(status.last_success_at.is_some());
    assert!(status.last_attempt_at.is_some());
    assert!(status.last_error.is_none());
    assert_eq!(status.consecutive_failures, 0);
    assert_eq!(status.total_syncs, 1);
    assert_eq!(status.total_failures, 0);
}

#[test]
fn test_record_success_resets_consecutive_failures() {
    let mut status = SyncStatus::new();

    // Accumulate some failures first
    status.record_failure("error 1".to_string());
    status.record_failure("error 2".to_string());
    status.record_failure("error 3".to_string());
    assert_eq!(status.consecutive_failures, 3);
    assert_eq!(status.total_failures, 3);

    // Now succeed
    status.record_success();

    assert_eq!(status.consecutive_failures, 0);
    assert!(status.last_error.is_none());
    assert!(status.last_success_at.is_some());
    // total_syncs = 3 failures + 1 success = 4
    assert_eq!(status.total_syncs, 4);
    // total_failures stays at 3
    assert_eq!(status.total_failures, 3);
}

#[test]
fn test_record_multiple_successes() {
    let mut status = SyncStatus::new();

    status.record_success();
    let first_success = status.last_success_at;
    status.record_success();
    status.record_success();

    assert_eq!(status.total_syncs, 3);
    assert_eq!(status.total_failures, 0);
    assert_eq!(status.consecutive_failures, 0);
    // last_success_at should be >= the first one
    assert!(status.last_success_at.unwrap() >= first_success.unwrap());
}

// ===========================================================================
// record_failure -- increments failures, stores error
// ===========================================================================

#[test]
fn test_record_failure() {
    let mut status = SyncStatus::new();

    status.record_failure("connection refused".to_string());

    assert!(status.last_success_at.is_none());
    assert!(status.last_attempt_at.is_some());
    assert_eq!(status.last_error, Some("connection refused".to_string()));
    assert_eq!(status.consecutive_failures, 1);
    assert_eq!(status.total_syncs, 1);
    assert_eq!(status.total_failures, 1);
}

#[test]
fn test_record_multiple_failures() {
    let mut status = SyncStatus::new();

    status.record_failure("error 1".to_string());
    status.record_failure("error 2".to_string());
    status.record_failure("error 3".to_string());

    assert_eq!(status.consecutive_failures, 3);
    assert_eq!(status.total_failures, 3);
    assert_eq!(status.total_syncs, 3);
    // Last error should be the most recent
    assert_eq!(status.last_error, Some("error 3".to_string()));
}

#[test]
fn test_failure_then_success_then_failure_resets() {
    let mut status = SyncStatus::new();

    status.record_failure("fail 1".to_string());
    status.record_failure("fail 2".to_string());
    assert_eq!(status.consecutive_failures, 2);

    status.record_success();
    assert_eq!(status.consecutive_failures, 0);

    status.record_failure("fail 3".to_string());
    assert_eq!(status.consecutive_failures, 1);
    assert_eq!(status.total_failures, 3);
    assert_eq!(status.total_syncs, 4);
}

// ===========================================================================
// next_retry_interval_secs -- base interval with no failures
// ===========================================================================

#[test]
fn test_next_retry_base_no_failures() {
    let status = SyncStatus::new();
    let interval = status.next_retry_interval_secs(30, 300);
    assert_eq!(interval, 30);
}

// ===========================================================================
// next_retry_interval_secs -- exponential backoff
// ===========================================================================

#[test]
fn test_next_retry_backoff_exponential() {
    let base = 30u64;
    let max = 600u64; // high enough to not cap

    // 1 failure: base * 2^0 = 30
    let mut status = SyncStatus::new();
    status.consecutive_failures = 1;
    let interval = status.next_retry_interval_secs(base, max);
    // With jitter of +/-10%, range is [27, 33]
    assert!(interval >= 27 && interval <= 33,
        "1 failure: expected ~30, got {}", interval);

    // 2 failures: base * 2^1 = 60
    status.consecutive_failures = 2;
    let interval = status.next_retry_interval_secs(base, max);
    assert!(interval >= 54 && interval <= 66,
        "2 failures: expected ~60, got {}", interval);

    // 3 failures: base * 2^2 = 120
    status.consecutive_failures = 3;
    let interval = status.next_retry_interval_secs(base, max);
    assert!(interval >= 108 && interval <= 132,
        "3 failures: expected ~120, got {}", interval);

    // 4 failures: base * 2^3 = 240
    status.consecutive_failures = 4;
    let interval = status.next_retry_interval_secs(base, max);
    assert!(interval >= 216 && interval <= 264,
        "4 failures: expected ~240, got {}", interval);
}

// ===========================================================================
// next_retry_interval_secs -- capped at max
// ===========================================================================

#[test]
fn test_next_retry_capped() {
    let base = 30u64;
    let max = 300u64;

    // At 5 failures: base * 2^4 = 480, should be capped to 300
    let mut status = SyncStatus::new();
    status.consecutive_failures = 5;
    let interval = status.next_retry_interval_secs(base, max);
    // With jitter of +/-10% of 300: range [270, 330]
    assert!(interval >= 270 && interval <= 330,
        "5 failures: expected ~300 (capped), got {}", interval);

    // At 10 failures: still capped to max
    status.consecutive_failures = 10;
    let interval = status.next_retry_interval_secs(base, max);
    assert!(interval >= 270 && interval <= 330,
        "10 failures: expected ~300 (capped), got {}", interval);
}

#[test]
fn test_next_retry_exponent_capped_at_8() {
    // Exponent is capped at 8 (consecutive_failures - 1).min(8)
    // So 9 failures => exponent = 8, 10 failures => still exponent 8
    let base = 1u64;
    let max = u64::MAX; // no artificial max cap

    let mut status = SyncStatus::new();
    status.consecutive_failures = 9;
    let interval_at_9 = status.next_retry_interval_secs(base, max);
    // 1 * 2^8 = 256, with jitter [230, 282]

    status.consecutive_failures = 20;
    let interval_at_20 = status.next_retry_interval_secs(base, max);
    // Still 1 * 2^8 = 256

    // Both should be approximately equal (within jitter bounds)
    assert!(interval_at_9 >= 230 && interval_at_9 <= 282,
        "9 failures: expected ~256, got {}", interval_at_9);
    assert!(interval_at_20 >= 230 && interval_at_20 <= 282,
        "20 failures: expected ~256, got {}", interval_at_20);
}

// ===========================================================================
// should_retry -- immediate retry with no failures
// ===========================================================================

#[test]
fn test_should_retry_immediate_no_failures() {
    let status = SyncStatus::new();
    assert!(status.should_retry(30, 300));
}

#[test]
fn test_should_retry_after_success() {
    let mut status = SyncStatus::new();
    status.record_success();
    // After success, consecutive_failures is 0, so should always retry
    assert!(status.should_retry(30, 300));
}

// ===========================================================================
// should_retry -- recently failed means backoff applies
// ===========================================================================

#[test]
fn test_should_retry_backoff_recently_failed() {
    let mut status = SyncStatus::new();
    status.record_failure("timeout".to_string());

    // Just failed -- should_retry should return false because
    // last_attempt_at is "now" and the backoff interval hasn't elapsed
    assert!(!status.should_retry(30, 300),
        "Should not retry immediately after failure");
}

#[test]
fn test_should_retry_backoff_elapsed() {
    let mut status = SyncStatus::new();
    status.consecutive_failures = 1;
    // Simulate that last attempt was a long time ago
    status.last_attempt_at = Some(1000); // ancient timestamp

    assert!(status.should_retry(30, 300),
        "Should retry after backoff has elapsed");
}

#[test]
fn test_should_retry_no_last_attempt() {
    let mut status = SyncStatus::new();
    status.consecutive_failures = 5;
    status.last_attempt_at = None;

    // No last_attempt_at means we should retry
    assert!(status.should_retry(30, 300));
}

// ===========================================================================
// SyncStatus serialization (for admin API JSON)
// ===========================================================================

#[test]
fn test_sync_status_serializes_to_json() {
    let mut status = SyncStatus::new();
    status.record_success();
    status.record_failure("test error".to_string());

    let json = serde_json::to_value(&status).expect("should serialize");

    assert!(json["last_success_at"].is_number());
    assert!(json["last_attempt_at"].is_number());
    assert_eq!(json["last_error"], "test error");
    assert_eq!(json["consecutive_failures"], 1);
    assert_eq!(json["total_syncs"], 2);
    assert_eq!(json["total_failures"], 1);
}

#[test]
fn test_sync_status_serializes_none_fields_as_null() {
    let status = SyncStatus::new();
    let json = serde_json::to_value(&status).expect("should serialize");

    assert!(json["last_success_at"].is_null());
    assert!(json["last_attempt_at"].is_null());
    assert!(json["last_error"].is_null());
    assert_eq!(json["consecutive_failures"], 0);
    assert_eq!(json["total_syncs"], 0);
    assert_eq!(json["total_failures"], 0);
}

// ===========================================================================
// PeerManager sync status integration
// ===========================================================================

#[test]
fn test_peer_manager_record_sync_success() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    manager.record_sync_success(1);

    let status = manager.get_sync_status(1).expect("should have status");
    assert!(status.last_success_at.is_some());
    assert_eq!(status.consecutive_failures, 0);
    assert_eq!(status.total_syncs, 1);
    assert_eq!(status.total_failures, 0);
}

#[test]
fn test_peer_manager_record_sync_failure() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    manager.record_sync_failure(1, "connection refused".to_string());

    let status = manager.get_sync_status(1).expect("should have status");
    assert_eq!(status.consecutive_failures, 1);
    assert_eq!(status.last_error, Some("connection refused".to_string()));
    assert_eq!(status.total_failures, 1);
}

#[test]
fn test_peer_manager_sync_status_lifecycle() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    // Initial state
    let status = manager.get_sync_status(1).unwrap();
    assert_eq!(status.consecutive_failures, 0);
    assert_eq!(status.total_syncs, 0);

    // Fail a few times
    manager.record_sync_failure(1, "error 1".to_string());
    manager.record_sync_failure(1, "error 2".to_string());

    let status = manager.get_sync_status(1).unwrap();
    assert_eq!(status.consecutive_failures, 2);
    assert_eq!(status.total_failures, 2);
    assert_eq!(status.total_syncs, 2);

    // Succeed -- resets consecutive_failures
    manager.record_sync_success(1);

    let status = manager.get_sync_status(1).unwrap();
    assert_eq!(status.consecutive_failures, 0);
    assert!(status.last_error.is_none());
    assert!(status.last_success_at.is_some());
    assert_eq!(status.total_syncs, 3);
    // total_failures is cumulative, not reset
    assert_eq!(status.total_failures, 2);
}

#[test]
fn test_peer_manager_get_sync_status_missing_peer() {
    let manager = PeerManager::new();
    assert!(manager.get_sync_status(999).is_none());
}

#[test]
fn test_peer_manager_record_on_missing_peer_is_noop() {
    let manager = PeerManager::new();
    // These should not panic
    manager.record_sync_success(999);
    manager.record_sync_failure(999, "error".to_string());
    assert!(manager.get_sync_status(999).is_none());
}

#[test]
fn test_peer_connection_has_sync_status_field() {
    let manager = PeerManager::new();
    let connection = manager.add_peer(&make_peer_config(1, "127.0.0.1:9000"));

    // The connection returned from add_peer should have a fresh SyncStatus
    assert_eq!(connection.sync_status.consecutive_failures, 0);
    assert_eq!(connection.sync_status.total_syncs, 0);
    assert!(connection.sync_status.last_success_at.is_none());
}

#[test]
fn test_peer_manager_sync_status_independent_per_peer() {
    let manager = PeerManager::new();
    manager.add_peer(&make_peer_config(1, "peer1:9000"));
    manager.add_peer(&make_peer_config(2, "peer2:9000"));

    // Fail peer 1, succeed peer 2
    manager.record_sync_failure(1, "peer1 error".to_string());
    manager.record_sync_success(2);

    let status_1 = manager.get_sync_status(1).unwrap();
    let status_2 = manager.get_sync_status(2).unwrap();

    assert_eq!(status_1.consecutive_failures, 1);
    assert_eq!(status_1.total_failures, 1);
    assert_eq!(status_2.consecutive_failures, 0);
    assert_eq!(status_2.total_failures, 0);
    assert!(status_2.last_success_at.is_some());
    assert!(status_1.last_success_at.is_none());
}

// ===========================================================================
// Edge cases for next_retry_interval_secs
// ===========================================================================

#[test]
fn test_next_retry_base_zero() {
    let mut status = SyncStatus::new();
    status.consecutive_failures = 1;
    // base_secs = 0: 0 * 2^0 = 0, jitter_range = 0
    let interval = status.next_retry_interval_secs(0, 300);
    assert_eq!(interval, 0);
}

#[test]
fn test_next_retry_max_zero() {
    let mut status = SyncStatus::new();
    status.consecutive_failures = 1;
    // max_secs = 0: capped to 0, jitter_range = 0
    let interval = status.next_retry_interval_secs(30, 0);
    assert_eq!(interval, 0);
}

#[test]
fn test_next_retry_saturating_arithmetic() {
    let mut status = SyncStatus::new();
    status.consecutive_failures = 1;
    // base near u64::MAX should not overflow thanks to saturating_mul
    let interval = status.next_retry_interval_secs(u64::MAX, u64::MAX);
    // Should be capped and not panic
    assert!(interval > 0);
}
