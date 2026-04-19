use std::sync::Arc;

use aeordb::engine::engine_counters::EngineCounters;
use aeordb::engine::event_bus::EventBus;
use aeordb::engine::metrics_pulse::{spawn_metrics_pulse, spawn_rate_sampler};
use aeordb::engine::rate_tracker::RateTrackerSet;
use aeordb::server::create_temp_engine_for_tests;
use tokio_util::sync::CancellationToken;

// ===========================================================================
// spawn_metrics_pulse
// ===========================================================================

#[tokio::test]
async fn test_metrics_pulse_emits_event() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    // Wait for first metrics event (interval is 15s, timeout at 20s)
    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .expect("should receive metrics within 20s")
    .expect("recv should succeed");

    assert_eq!(event.event_type, "metrics");
    assert_eq!(event.user_id, "system");

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_payload_structure() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    let payload = &event.payload;

    // Verify counts section
    let counts = &payload["counts"];
    assert!(counts["files"].is_number());
    assert!(counts["directories"].is_number());
    assert!(counts["symlinks"].is_number());
    assert!(counts["chunks"].is_number());
    assert!(counts["snapshots"].is_number());
    assert!(counts["forks"].is_number());

    // Verify sizes section
    let sizes = &payload["sizes"];
    assert!(sizes["logical_data"].is_number());
    assert!(sizes["chunk_data"].is_number());
    assert!(sizes["void_space"].is_number());
    assert!(sizes["dedup_savings"].is_number());
    assert!(sizes["db_file_size"].is_number());

    // Verify throughput section
    let throughput = &payload["throughput"];
    for key in &["writes_per_sec", "reads_per_sec", "bytes_written_per_sec", "bytes_read_per_sec"] {
        let rate = &throughput[key];
        assert!(rate["1m"].is_number(), "throughput.{}.1m should be number", key);
        assert!(rate["5m"].is_number(), "throughput.{}.5m should be number", key);
        assert!(rate["15m"].is_number(), "throughput.{}.15m should be number", key);
        assert!(rate["peak_1m"].is_number(), "throughput.{}.peak_1m should be number", key);
    }

    // Verify health section
    let health = &payload["health"];
    assert!(health["write_buffer_depth"].is_number());
    assert!(health["dedup_hit_rate"].is_number());

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_reflects_counter_values() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    // Set some counter values before starting the pulse.
    counters.increment_files();
    counters.increment_files();
    counters.increment_files();
    counters.increment_directories();
    counters.increment_chunks();
    counters.increment_chunks();
    counters.add_logical_data_size(1024);
    counters.add_chunk_data_size(512);
    counters.set_write_buffer_depth(7);

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    let payload = &event.payload;
    assert_eq!(payload["counts"]["files"].as_u64().unwrap(), 3);
    assert_eq!(payload["counts"]["directories"].as_u64().unwrap(), 1);
    assert_eq!(payload["counts"]["chunks"].as_u64().unwrap(), 2);
    assert_eq!(payload["sizes"]["logical_data"].as_u64().unwrap(), 1024);
    assert_eq!(payload["sizes"]["chunk_data"].as_u64().unwrap(), 512);
    assert_eq!(payload["sizes"]["dedup_savings"].as_u64().unwrap(), 512); // 1024 - 512
    assert_eq!(payload["health"]["write_buffer_depth"].as_u64().unwrap(), 7);

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_dedup_savings_clamped_to_zero() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    // chunk_data > logical_data => dedup_savings should clamp to 0 (saturating sub)
    counters.add_logical_data_size(100);
    counters.add_chunk_data_size(500);

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(event.payload["sizes"]["dedup_savings"].as_u64().unwrap(), 0);

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_dedup_hit_rate_zero_when_no_chunks() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    // No chunks at all => dedup_hit_rate should be 0.0
    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    let hit_rate = event.payload["health"]["dedup_hit_rate"].as_f64().unwrap();
    assert!((hit_rate - 0.0).abs() < f64::EPSILON, "dedup_hit_rate should be 0.0 with no chunks");

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_dedup_hit_rate_with_deduplication() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    // 3 unique chunks + 2 deduplicated = 40% hit rate
    counters.increment_chunks();
    counters.increment_chunks();
    counters.increment_chunks();
    counters.increment_chunks_deduped();
    counters.increment_chunks_deduped();

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    let hit_rate = event.payload["health"]["dedup_hit_rate"].as_f64().unwrap();
    // 2 / (3 + 2) = 0.4
    assert!((hit_rate - 0.4).abs() < 0.001, "dedup_hit_rate should be 0.4, got {}", hit_rate);

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_cancellation_stops_task() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());
    let cancel = CancellationToken::new();

    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    // Cancel immediately
    cancel.cancel();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle,
    )
    .await;

    assert!(result.is_ok(), "metrics pulse should exit after cancellation");
}

#[tokio::test]
async fn test_metrics_pulse_no_subscribers_does_not_panic() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());
    let cancel = CancellationToken::new();

    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    // Let it run briefly without any subscriber
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_db_file_size_from_disk() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str.clone(),
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    // The db file was created by create_temp_engine_for_tests, so it should have a non-zero size.
    let db_file_size = event.payload["sizes"]["db_file_size"].as_u64().unwrap();
    let actual_size = std::fs::metadata(&db_path_str).unwrap().len();
    assert_eq!(db_file_size, actual_size, "db_file_size should match actual file metadata");

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_nonexistent_db_path() {
    // When the db_path doesn't exist, db_file_size should be 0 (not panic).
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        "/tmp/nonexistent_aeordb_test_file.aeordb".to_string(),
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(event.payload["sizes"]["db_file_size"].as_u64().unwrap(), 0);

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

// ===========================================================================
// spawn_rate_sampler
// ===========================================================================

#[tokio::test]
async fn test_rate_sampler_records_samples() {
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());
    let cancel = CancellationToken::new();

    // Seed some counter values
    counters.increment_writes();
    counters.increment_writes();
    counters.increment_reads();
    counters.add_bytes_written(1024);

    let handle = spawn_rate_sampler(
        counters.clone(),
        rate_trackers.clone(),
        cancel.clone(),
    );

    // Let the sampler run for ~3 seconds to collect a few samples
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Check that samples were recorded
    assert!(rate_trackers.writes.sample_count() >= 2, "should have at least 2 write samples");
    assert!(rate_trackers.reads.sample_count() >= 2, "should have at least 2 read samples");
    assert!(rate_trackers.bytes_written.sample_count() >= 2, "should have at least 2 bytes_written samples");

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_rate_sampler_cancellation_stops_task() {
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());
    let cancel = CancellationToken::new();

    let handle = spawn_rate_sampler(
        counters.clone(),
        rate_trackers.clone(),
        cancel.clone(),
    );

    cancel.cancel();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle,
    )
    .await;

    assert!(result.is_ok(), "rate sampler should exit after cancellation");
}

#[tokio::test]
async fn test_rate_sampler_detects_throughput_changes() {
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());
    let cancel = CancellationToken::new();

    let handle = spawn_rate_sampler(
        counters.clone(),
        rate_trackers.clone(),
        cancel.clone(),
    );

    // Let sampler collect a baseline
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Now generate some writes
    for _ in 0..100 {
        counters.increment_writes();
        counters.add_bytes_written(4096);
    }

    // Let sampler pick up the change
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let snapshot = rate_trackers.snapshot();
    // Rate should be non-zero since we added writes between samples
    assert!(snapshot.writes.rate_1m > 0.0, "write rate should be > 0 after writes");
    assert!(snapshot.bytes_written.rate_1m > 0.0, "bytes_written rate should be > 0 after writes");

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}

#[tokio::test]
async fn test_metrics_pulse_event_has_valid_envelope() {
    let (_engine, temp) = create_temp_engine_for_tests();
    let db_path = temp.path().join("test.aeordb");
    let db_path_str = db_path.to_str().unwrap().to_string();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let counters = Arc::new(EngineCounters::new());
    let rate_trackers = Arc::new(RateTrackerSet::new());

    let cancel = CancellationToken::new();
    let handle = spawn_metrics_pulse(
        bus.clone(),
        counters.clone(),
        rate_trackers.clone(),
        db_path_str,
        cancel.clone(),
    );

    let event = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        rx.recv(),
    )
    .await
    .unwrap()
    .unwrap();

    // Verify envelope metadata
    assert!(!event.event_id.is_empty(), "event_id should not be empty");
    assert!(event.timestamp > 0, "timestamp should be positive");
    assert_eq!(event.event_type, "metrics");
    assert_eq!(event.user_id, "system");

    // Verify event_id looks like a UUID (36 chars with hyphens)
    assert_eq!(event.event_id.len(), 36);
    assert_eq!(event.event_id.chars().filter(|c| *c == '-').count(), 4);

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
}
