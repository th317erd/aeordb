use std::sync::Arc;
use tokio::time::Duration;
use chrono::Timelike;
use crate::engine::engine_event::{EngineEvent, HeartbeatData, EVENT_HEARTBEAT};
use crate::engine::event_bus::EventBus;
use crate::engine::storage_engine::StorageEngine;

const HEARTBEAT_INTERVAL_SECS: u64 = 15;

/// Spawn a heartbeat task that emits DatabaseStats every 15 seconds,
/// aligned to wall clock boundaries (XX:00, XX:15, XX:30, XX:45).
///
/// Returns a JoinHandle that can be aborted to stop the heartbeat.
pub fn spawn_heartbeat(
    bus: Arc<EventBus>,
    engine: Arc<StorageEngine>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Calculate delay to next 15-second boundary
        let initial_delay = delay_to_next_boundary();
        tokio::time::sleep(initial_delay).await;

        let mut interval = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            let stats = engine.stats();
            let heartbeat_data = HeartbeatData {
                entry_count: stats.entry_count,
                kv_entries: stats.kv_entries,
                chunk_count: stats.chunk_count,
                file_count: stats.file_count,
                directory_count: stats.directory_count,
                snapshot_count: stats.snapshot_count,
                fork_count: stats.fork_count,
                void_count: stats.void_count,
                void_space_bytes: stats.void_space_bytes,
                db_file_size_bytes: stats.db_file_size_bytes,
                kv_size_bytes: stats.kv_size_bytes,
                nvt_buckets: stats.nvt_buckets,
            };

            let event = EngineEvent::new(
                EVENT_HEARTBEAT,
                "system",
                serde_json::json!({"stats": heartbeat_data}),
            );
            bus.emit(event);
        }
    })
}

/// Calculate the duration until the next 15-second wall clock boundary.
pub fn delay_to_next_boundary() -> Duration {
    let now = chrono::Utc::now();
    let current_second = now.second();
    let current_millis = now.timestamp_subsec_millis();

    // Next boundary: ceil to next multiple of 15
    let next_boundary_second = ((current_second / HEARTBEAT_INTERVAL_SECS as u32) + 1) * HEARTBEAT_INTERVAL_SECS as u32;

    let delay_seconds = if next_boundary_second >= 60 {
        // Wrap to next minute
        60 - current_second
    } else {
        next_boundary_second - current_second
    };

    let delay_ms = (delay_seconds as u64 * 1000).saturating_sub(current_millis as u64);
    Duration::from_millis(delay_ms.max(1)) // at least 1ms to avoid zero-duration
}
