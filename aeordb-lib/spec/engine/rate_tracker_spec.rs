use aeordb::engine::{
    RateTracker, RateSnapshot, RateTrackerSet, CountersSnapshot,
};

/// Helper: build a CountersSnapshot with only the throughput fields set.
fn make_counters(writes: u64, reads: u64, bytes_w: u64, bytes_r: u64) -> CountersSnapshot {
    CountersSnapshot {
        files: 0,
        directories: 0,
        symlinks: 0,
        chunks: 0,
        snapshots: 0,
        forks: 0,
        logical_data_size: 0,
        chunk_data_size: 0,
        void_space: 0,
        writes_total: writes,
        reads_total: reads,
        bytes_written_total: bytes_w,
        bytes_read_total: bytes_r,
        chunks_deduped_total: 0,
        write_buffer_depth: 0,
    }
}

// ---------------------------------------------------------------------------
// 1. Empty tracker -- all rates zero
// ---------------------------------------------------------------------------

#[test]
fn test_empty_tracker_returns_zero() {
    let tracker = RateTracker::new();
    assert_eq!(tracker.rate_1m(), 0.0);
    assert_eq!(tracker.rate_5m(), 0.0);
    assert_eq!(tracker.rate_15m(), 0.0);
    assert_eq!(tracker.peak_1m(), 0.0);

    let snapshot = tracker.snapshot();
    assert_eq!(snapshot.rate_1m, 0.0);
    assert_eq!(snapshot.rate_5m, 0.0);
    assert_eq!(snapshot.rate_15m, 0.0);
    assert_eq!(snapshot.peak_1m, 0.0);
}

// ---------------------------------------------------------------------------
// 2. Single sample -- need at least 2 for a delta
// ---------------------------------------------------------------------------

#[test]
fn test_single_sample_returns_zero() {
    let tracker = RateTracker::new();
    tracker.record(1000, 42);

    assert_eq!(tracker.rate_1m(), 0.0);
    assert_eq!(tracker.rate_5m(), 0.0);
    assert_eq!(tracker.rate_15m(), 0.0);
    assert_eq!(tracker.peak_1m(), 0.0);
}

// ---------------------------------------------------------------------------
// 3. Basic 1-minute rate: 0 -> 60 over 60 seconds = 1.0 ops/sec
// ---------------------------------------------------------------------------

#[test]
fn test_rate_1m_basic() {
    let tracker = RateTracker::new();
    tracker.record(0, 0);
    tracker.record(60_000, 60);

    let rate = tracker.rate_1m();
    assert!(
        (rate - 1.0).abs() < 1e-9,
        "expected 1.0 ops/sec, got {rate}"
    );
}

// ---------------------------------------------------------------------------
// 4. Known input: 10 ops/sec for 60 seconds
// ---------------------------------------------------------------------------

#[test]
fn test_rate_with_known_input() {
    let tracker = RateTracker::new();
    for second in 0..=60 {
        let timestamp_ms = second * 1000;
        let counter_value = second * 10;
        tracker.record(timestamp_ms, counter_value);
    }

    let rate = tracker.rate_1m();
    assert!(
        (rate - 10.0).abs() < 1e-9,
        "expected ~10.0 ops/sec, got {rate}"
    );
}

// ---------------------------------------------------------------------------
// 5. peak_1m finds the maximum single-second burst
// ---------------------------------------------------------------------------

#[test]
fn test_peak_1m_finds_maximum() {
    let tracker = RateTracker::new();

    // Steady 5 ops/sec for 50 seconds.
    for second in 0..=50 {
        tracker.record(second * 1000, second * 5);
    }

    // Then a burst of 100 ops in the 51st second.
    let counter_at_50 = 50 * 5; // 250
    tracker.record(51_000, counter_at_50 + 100); // 350

    let peak = tracker.peak_1m();
    assert!(
        (peak - 100.0).abs() < 1e-9,
        "expected peak=100.0, got {peak}"
    );

    // Average rate should be much lower than the peak.
    let avg = tracker.rate_1m();
    assert!(
        avg < peak,
        "average rate ({avg}) should be less than peak ({peak})"
    );
}

// ---------------------------------------------------------------------------
// 6. Old samples are evicted past max_samples (900)
// ---------------------------------------------------------------------------

#[test]
fn test_old_samples_evicted() {
    let tracker = RateTracker::new();

    // Record 1000 samples -- only 900 should survive.
    for i in 0..1000_u64 {
        tracker.record(i * 1000, i * 10);
    }

    assert_eq!(
        tracker.sample_count(),
        900,
        "should retain exactly 900 samples"
    );

    // The oldest surviving sample is from t=100_000 (sample #100).
    // rate_15m spans the entire deque:
    //   newest: (999_000, 9990)
    //   oldest: (100_000, 1000)
    //   rate = (9990 - 1000) / (999_000 - 100_000) * 1000 = 8990/899 ~ 9.9889
    let rate = tracker.rate_15m();
    assert!(
        (rate - 10.0).abs() < 0.02,
        "expected ~10.0, got {rate}"
    );
}

// ---------------------------------------------------------------------------
// 7. Partial window -- only 30 seconds of data, rate_1m still works
// ---------------------------------------------------------------------------

#[test]
fn test_partial_window() {
    let tracker = RateTracker::new();

    // 30 seconds of data at 5 ops/sec.
    for second in 0..=30 {
        tracker.record(second * 1000, second * 5);
    }

    // rate_1m asks for 60s but we only have 30s -- should use all available.
    let rate = tracker.rate_1m();
    assert!(
        (rate - 5.0).abs() < 1e-9,
        "expected 5.0 ops/sec with partial window, got {rate}"
    );
}

// ---------------------------------------------------------------------------
// 8. rate_5m and rate_15m compute over longer windows
// ---------------------------------------------------------------------------

#[test]
fn test_rate_5m_and_15m() {
    let tracker = RateTracker::new();

    // 300 seconds at 8 ops/sec.
    for second in 0..=300 {
        tracker.record(second * 1000, second * 8);
    }

    let rate_5m = tracker.rate_5m();
    assert!(
        (rate_5m - 8.0).abs() < 1e-9,
        "expected rate_5m=8.0, got {rate_5m}"
    );

    let rate_1m = tracker.rate_1m();
    assert!(
        (rate_1m - 8.0).abs() < 1e-9,
        "expected rate_1m=8.0, got {rate_1m}"
    );

    // Extend to 900 seconds.
    for second in 301..=900 {
        tracker.record(second * 1000, second * 8);
    }

    let rate_15m = tracker.rate_15m();
    assert!(
        (rate_15m - 8.0).abs() < 1e-9,
        "expected rate_15m=8.0, got {rate_15m}"
    );
}

// ---------------------------------------------------------------------------
// 9. snapshot() captures all 4 fields populated
// ---------------------------------------------------------------------------

#[test]
fn test_rate_snapshot_captures_all() {
    let tracker = RateTracker::new();

    // 120 seconds at 3 ops/sec.
    for second in 0..=120 {
        tracker.record(second * 1000, second * 3);
    }

    let snapshot = tracker.snapshot();

    assert!(
        (snapshot.rate_1m - 3.0).abs() < 1e-9,
        "rate_1m should be 3.0, got {}",
        snapshot.rate_1m
    );
    assert!(
        (snapshot.rate_5m - 3.0).abs() < 1e-9,
        "rate_5m should be 3.0, got {}",
        snapshot.rate_5m
    );
    assert!(
        (snapshot.rate_15m - 3.0).abs() < 1e-9,
        "rate_15m should be 3.0, got {}",
        snapshot.rate_15m
    );
    // Steady 3/sec means peak should also be 3.0.
    assert!(
        (snapshot.peak_1m - 3.0).abs() < 1e-9,
        "peak_1m should be 3.0, got {}",
        snapshot.peak_1m
    );
}

// ---------------------------------------------------------------------------
// 10. RateTrackerSet -- record_all and snapshot
// ---------------------------------------------------------------------------

#[test]
fn test_rate_tracker_set_record_and_snapshot() {
    let set = RateTrackerSet::new();

    for second in 0..=60 {
        let timestamp_ms = second * 1000;
        let counters = make_counters(second * 10, second * 20, second * 1024, second * 2048);
        set.record_all(timestamp_ms, &counters);
    }

    let snapshot = set.snapshot();

    assert!(
        (snapshot.writes.rate_1m - 10.0).abs() < 1e-9,
        "writes rate_1m should be 10.0, got {}",
        snapshot.writes.rate_1m
    );
    assert!(
        (snapshot.reads.rate_1m - 20.0).abs() < 1e-9,
        "reads rate_1m should be 20.0, got {}",
        snapshot.reads.rate_1m
    );
    assert!(
        (snapshot.bytes_written.rate_1m - 1024.0).abs() < 1e-9,
        "bytes_written rate_1m should be 1024.0, got {}",
        snapshot.bytes_written.rate_1m
    );
    assert!(
        (snapshot.bytes_read.rate_1m - 2048.0).abs() < 1e-9,
        "bytes_read rate_1m should be 2048.0, got {}",
        snapshot.bytes_read.rate_1m
    );
}

// ---------------------------------------------------------------------------
// 11. Edge case: all samples have the same timestamp -- zero rate
// ---------------------------------------------------------------------------

#[test]
fn test_zero_time_delta_returns_zero() {
    let tracker = RateTracker::new();
    tracker.record(5000, 0);
    tracker.record(5000, 100); // same timestamp

    assert_eq!(
        tracker.rate_1m(),
        0.0,
        "zero time delta should return 0.0"
    );
    assert_eq!(
        tracker.peak_1m(),
        0.0,
        "zero time delta in peak should return 0.0"
    );
}

// ---------------------------------------------------------------------------
// 12. Peak with single pair in window
// ---------------------------------------------------------------------------

#[test]
fn test_peak_with_two_samples() {
    let tracker = RateTracker::new();
    tracker.record(0, 0);
    tracker.record(1000, 50);

    let peak = tracker.peak_1m();
    assert!(
        (peak - 50.0).abs() < 1e-9,
        "expected peak=50.0, got {peak}"
    );
}

// ---------------------------------------------------------------------------
// 13. Monotonic counter -- saturating_sub prevents underflow
// ---------------------------------------------------------------------------

#[test]
fn test_counter_wrapping_handled_gracefully() {
    let tracker = RateTracker::new();
    tracker.record(0, 100);
    tracker.record(1000, 50); // "decrease" -- should not panic

    let rate = tracker.rate_1m();
    assert_eq!(rate, 0.0, "should not panic on counter decrease");
}

// ---------------------------------------------------------------------------
// 14. rate_1m only considers the last 60 seconds of data
// ---------------------------------------------------------------------------

#[test]
fn test_rate_1m_uses_correct_window() {
    let tracker = RateTracker::new();

    // First 60 seconds: 2 ops/sec (counter goes 0..120).
    for second in 0..=60 {
        tracker.record(second * 1000, second * 2);
    }
    // Next 60 seconds: 20 ops/sec.
    for second in 61..=120 {
        let counter_value = 120 + (second - 60) * 20;
        tracker.record(second * 1000, counter_value);
    }

    // rate_1m should reflect the last 60 seconds (the fast period).
    let rate = tracker.rate_1m();
    assert!(
        (rate - 20.0).abs() < 1e-9,
        "expected rate_1m=20.0 (fast period), got {rate}"
    );

    // rate_5m spans the full 120 seconds.
    // newest counter = 120 + 60*20 = 1320, oldest = 0
    // rate = 1320/120 = 11.0
    let rate_5m = tracker.rate_5m();
    assert!(
        (rate_5m - 11.0).abs() < 1e-9,
        "expected rate_5m=11.0, got {rate_5m}"
    );
}

// ---------------------------------------------------------------------------
// 15. RateSnapshot serializes with correct JSON field names
// ---------------------------------------------------------------------------

#[test]
fn test_rate_snapshot_json_field_names() {
    let snapshot = RateSnapshot {
        rate_1m: 1.5,
        rate_5m: 2.5,
        rate_15m: 3.5,
        peak_1m: 10.0,
    };

    let json = serde_json::to_value(&snapshot).unwrap();
    assert!(json.get("1m").is_some(), "should serialize as '1m'");
    assert!(json.get("5m").is_some(), "should serialize as '5m'");
    assert!(json.get("15m").is_some(), "should serialize as '15m'");
    assert!(json.get("peak_1m").is_some(), "should serialize as 'peak_1m'");

    // Struct field names should NOT appear in JSON.
    assert!(json.get("rate_1m").is_none(), "should NOT serialize as 'rate_1m'");
    assert!(json.get("rate_5m").is_none(), "should NOT serialize as 'rate_5m'");
    assert!(json.get("rate_15m").is_none(), "should NOT serialize as 'rate_15m'");
}

// ---------------------------------------------------------------------------
// 16. RateSetSnapshot serializes correctly
// ---------------------------------------------------------------------------

#[test]
fn test_rate_set_snapshot_json() {
    let set = RateTrackerSet::new();
    set.record_all(0, &make_counters(0, 0, 0, 0));
    set.record_all(1000, &make_counters(10, 20, 100, 200));

    let snapshot = set.snapshot();
    let json = serde_json::to_value(&snapshot).unwrap();
    assert!(json.get("writes").is_some());
    assert!(json.get("reads").is_some());
    assert!(json.get("bytes_written").is_some());
    assert!(json.get("bytes_read").is_some());
}

// ---------------------------------------------------------------------------
// 17. RateTrackerSet with empty counters
// ---------------------------------------------------------------------------

#[test]
fn test_rate_tracker_set_empty() {
    let set = RateTrackerSet::new();
    let snapshot = set.snapshot();

    assert_eq!(snapshot.writes.rate_1m, 0.0);
    assert_eq!(snapshot.reads.rate_1m, 0.0);
    assert_eq!(snapshot.bytes_written.rate_1m, 0.0);
    assert_eq!(snapshot.bytes_read.rate_1m, 0.0);
}

// ---------------------------------------------------------------------------
// 18. Peak is zero when all deltas are zero (flat counter)
// ---------------------------------------------------------------------------

#[test]
fn test_peak_with_flat_counter() {
    let tracker = RateTracker::new();
    for second in 0..=30 {
        tracker.record(second * 1000, 100); // counter never changes
    }

    assert_eq!(tracker.peak_1m(), 0.0, "flat counter should have zero peak");
    assert_eq!(tracker.rate_1m(), 0.0, "flat counter should have zero rate");
}
