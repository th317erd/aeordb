use aeordb::engine::{
    VirtualClock, SystemClock, MockClock,
    PeerClockTracker, PeerClockStats,
};

// ---------------------------------------------------------------------------
// SystemClock tests
// ---------------------------------------------------------------------------

#[test]
fn test_system_clock_returns_reasonable_time() {
    let clock = SystemClock::new(42);
    let now = clock.now_ms();

    // Should be a positive value well past the Unix epoch.
    // 1_700_000_000_000 ms ≈ Nov 2023 — any run after that is fine.
    assert!(now > 1_700_000_000_000, "now_ms should be after Nov 2023");

    // And reasonably close to chrono's view of "now" (within 50 ms).
    let chrono_now = chrono::Utc::now().timestamp_millis() as u64;
    let drift = chrono_now.abs_diff(now);
    assert!(drift < 50, "SystemClock should be within 50ms of chrono::Utc::now()");
}

#[test]
fn test_system_clock_node_id() {
    let clock = SystemClock::new(99);
    assert_eq!(clock.node_id(), 99);
}

// ---------------------------------------------------------------------------
// MockClock tests
// ---------------------------------------------------------------------------

#[test]
fn test_mock_clock_set_time() {
    let clock = MockClock::new(1, 5000);
    assert_eq!(clock.now_ms(), 5000);

    clock.set_time(12345);
    assert_eq!(clock.now_ms(), 12345);
}

#[test]
fn test_mock_clock_advance() {
    let clock = MockClock::new(1, 1000);
    clock.advance(100);
    assert_eq!(clock.now_ms(), 1100);

    clock.advance(50);
    assert_eq!(clock.now_ms(), 1150);
}

#[test]
fn test_mock_clock_node_id() {
    let clock = MockClock::new(77, 0);
    assert_eq!(clock.node_id(), 77);
}

#[test]
fn test_mock_clock_thread_safety() {
    use std::sync::Arc;
    use std::thread;

    let clock = Arc::new(MockClock::new(1, 0));
    let mut handles = vec![];

    for _ in 0..10 {
        let clock_clone = clock.clone();
        handles.push(thread::spawn(move || {
            clock_clone.advance(1);
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(clock.now_ms(), 10, "10 threads each advancing by 1 should yield 10");
}

// ---------------------------------------------------------------------------
// PeerClockTracker tests
// ---------------------------------------------------------------------------

#[test]
fn test_peer_tracker_record_heartbeat() {
    let tracker = PeerClockTracker::new(30_000);

    let accepted = tracker.record_heartbeat(
        /* peer_node_id */ 2,
        /* intent_time */ 1000,
        /* construct_time */ 1005,
        /* receive_time */ 1010,
    );
    assert!(accepted, "heartbeat within threshold should be accepted");

    let stats = tracker.get_peer_stats(2).expect("stats should exist after recording");
    assert!(stats.samples >= 1);
    assert!(stats.last_updated_ms == 1010);
}

#[test]
fn test_peer_tracker_moving_average() {
    let tracker = PeerClockTracker::new(30_000);

    // Send several heartbeats where the peer is consistently 10ms ahead.
    for i in 0..10u64 {
        let base = 1_000_000 + i * 15_000;
        tracker.record_heartbeat(2, base, base + 10, base);
    }

    let stats = tracker.get_peer_stats(2).unwrap();
    // With peer consistently 10ms ahead (construct - receive = +10),
    // the EMA should converge close to 10.0.
    assert!(
        (stats.clock_offset_ms - 10.0).abs() < 2.0,
        "offset should converge to ~10.0, got {}",
        stats.clock_offset_ms,
    );
}

#[test]
fn test_peer_tracker_bounds_rejection() {
    let tracker = PeerClockTracker::new(30_000); // 30-second threshold

    // Peer claims to be 60 seconds ahead — way beyond threshold.
    let accepted = tracker.record_heartbeat(
        3,
        1_000_000,
        1_060_000, // construct 60s ahead
        1_000_000, // receive now
    );
    assert!(!accepted, "heartbeat exceeding threshold should be rejected");

    // No stats should have been recorded.
    assert!(tracker.get_peer_stats(3).is_none());
}

#[test]
fn test_peer_tracker_bounds_rejection_negative_offset() {
    let tracker = PeerClockTracker::new(30_000);

    // Peer claims to be 60 seconds behind.
    let accepted = tracker.record_heartbeat(
        4,
        1_000_000,
        940_000,   // construct 60s behind
        1_000_000,
    );
    assert!(!accepted, "large negative offset should also be rejected");
    assert!(tracker.get_peer_stats(4).is_none());
}

#[test]
fn test_peer_tracker_is_settled() {
    let tracker = PeerClockTracker::new(30_000);

    // Not settled with zero samples.
    assert!(!tracker.is_settled(5, 5, 5.0));

    // Record 1 sample — not enough.
    tracker.record_heartbeat(5, 1000, 1005, 1010);
    assert!(!tracker.is_settled(5, 5, 5.0));

    // Record enough samples with consistent wire time so jitter stays low.
    for i in 1..10u64 {
        let base = 1000 + i * 15_000;
        tracker.record_heartbeat(5, base, base + 5, base + 10);
    }

    let stats = tracker.get_peer_stats(5).unwrap();
    assert!(stats.samples >= 5);
    // With consistent timings, jitter should be very low.
    assert!(
        tracker.is_settled(5, 5, 5.0),
        "should be settled after enough consistent samples (jitter={})",
        stats.jitter_ms,
    );
}

#[test]
fn test_peer_tracker_is_settled_high_jitter_not_settled() {
    let tracker = PeerClockTracker::new(30_000);

    // Alternate wire times to create jitter: 5ms then 25ms.
    for i in 0..10u64 {
        let base = 1_000_000 + i * 15_000;
        let wire = if i % 2 == 0 { 5 } else { 25 };
        tracker.record_heartbeat(6, base, base, base + wire);
    }

    let stats = tracker.get_peer_stats(6).unwrap();
    // With alternating wire times, jitter should be meaningful.
    assert!(stats.jitter_ms > 0.0, "jitter should be > 0 with varying wire times");
    // Should NOT be settled if max_jitter threshold is very tight (e.g. 0.1ms).
    assert!(
        !tracker.is_settled(6, 5, 0.1),
        "should not be settled with tight jitter threshold (jitter={})",
        stats.jitter_ms,
    );
}

#[test]
fn test_peer_tracker_seed() {
    let tracker = PeerClockTracker::new(30_000);

    let seeded = PeerClockStats {
        clock_offset_ms: 3.5,
        wire_time_ms: 2.0,
        jitter_ms: 0.5,
        samples: 100,
        last_updated_ms: 999_999,
    };
    tracker.seed_peer(7, seeded.clone());

    let stats = tracker.get_peer_stats(7).expect("seeded peer should be present");
    assert_eq!(stats.samples, 100);
    assert!((stats.clock_offset_ms - 3.5).abs() < f64::EPSILON);
    assert!((stats.wire_time_ms - 2.0).abs() < f64::EPSILON);
    assert!((stats.jitter_ms - 0.5).abs() < f64::EPSILON);
    assert_eq!(stats.last_updated_ms, 999_999);
}

#[test]
fn test_peer_tracker_jitter_calculation() {
    let tracker = PeerClockTracker::new(30_000);

    // Record heartbeats with varying wire times.
    let wire_times = [5u64, 10, 3, 15, 8, 20, 2, 12, 6, 18];
    for (i, &wire) in wire_times.iter().enumerate() {
        let base = 1_000_000 + (i as u64) * 15_000;
        tracker.record_heartbeat(8, base, base, base + wire);
    }

    let stats = tracker.get_peer_stats(8).unwrap();
    assert!(
        stats.jitter_ms > 0.0,
        "jitter should be positive with varying wire times, got {}",
        stats.jitter_ms,
    );
    assert_eq!(stats.samples, wire_times.len() as u32);
}

#[test]
fn test_peer_tracker_all_peer_stats() {
    let tracker = PeerClockTracker::new(30_000);

    // Record heartbeats from multiple peers.
    tracker.record_heartbeat(10, 1000, 1005, 1010);
    tracker.record_heartbeat(20, 2000, 2003, 2008);
    tracker.record_heartbeat(30, 3000, 3001, 3005);

    let all = tracker.all_peer_stats();
    assert_eq!(all.len(), 3);
    assert!(all.contains_key(&10));
    assert!(all.contains_key(&20));
    assert!(all.contains_key(&30));
}

#[test]
fn test_peer_tracker_unknown_peer_returns_none() {
    let tracker = PeerClockTracker::new(30_000);
    assert!(tracker.get_peer_stats(999).is_none());
}

#[test]
fn test_peer_tracker_is_settled_unknown_peer() {
    let tracker = PeerClockTracker::new(30_000);
    assert!(!tracker.is_settled(999, 1, 100.0), "unknown peer should not be settled");
}

#[test]
fn test_peer_tracker_seed_then_record() {
    let tracker = PeerClockTracker::new(30_000);

    // Seed with historical data.
    tracker.seed_peer(11, PeerClockStats {
        clock_offset_ms: 5.0,
        wire_time_ms: 3.0,
        jitter_ms: 1.0,
        samples: 50,
        last_updated_ms: 500_000,
    });

    // Record a new heartbeat — should update the seeded stats.
    tracker.record_heartbeat(11, 600_000, 600_005, 600_010);

    let stats = tracker.get_peer_stats(11).unwrap();
    assert_eq!(stats.samples, 51);
    assert!(stats.last_updated_ms == 600_010);
}

#[test]
fn test_peer_tracker_exact_threshold_boundary() {
    let tracker = PeerClockTracker::new(30_000);

    // Offset of exactly 30_000ms — accepted because the check is strict > (not >=).
    // raw_offset = construct - receive = 30000
    let accepted = tracker.record_heartbeat(12, 1000, 31_000, 1_000);
    assert!(accepted, "offset exactly at threshold should be accepted (strict > comparison)");

    // Offset of 30_001ms — should be rejected.
    let accepted = tracker.record_heartbeat(13, 1000, 31_001, 1_000);
    assert!(!accepted, "offset exceeding threshold should be rejected");

    // Offset of 29_999ms — should be accepted.
    let accepted = tracker.record_heartbeat(14, 1000, 30_999, 1_000);
    assert!(accepted, "offset just below threshold should be accepted");
}

#[test]
fn test_peer_tracker_wire_time_clamped_non_negative() {
    let tracker = PeerClockTracker::new(30_000);

    // construct_time > receive_time means negative wire time, which should
    // be clamped to 0.
    tracker.record_heartbeat(14, 1000, 1010, 1005);

    let stats = tracker.get_peer_stats(14).unwrap();
    assert!(
        stats.wire_time_ms >= 0.0,
        "wire_time should never be negative, got {}",
        stats.wire_time_ms,
    );
}

// ---------------------------------------------------------------------------
// Object safety — ensure VirtualClock can be used as dyn trait
// ---------------------------------------------------------------------------

#[test]
fn test_virtual_clock_object_safety() {
    use std::sync::Arc;

    let mock: Arc<dyn VirtualClock> = Arc::new(MockClock::new(1, 42_000));
    assert_eq!(mock.now_ms(), 42_000);
    assert_eq!(mock.node_id(), 1);

    let system: Arc<dyn VirtualClock> = Arc::new(SystemClock::new(2));
    assert!(system.now_ms() > 0);
    assert_eq!(system.node_id(), 2);
}
