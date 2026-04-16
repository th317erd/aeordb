use std::sync::Arc;
use tokio::time::Duration;
use chrono::Timelike;
use crate::engine::engine_event::{EngineEvent, HeartbeatData, EVENT_HEARTBEAT};
use crate::engine::event_bus::EventBus;
use crate::engine::storage_engine::StorageEngine;

const HEARTBEAT_INTERVAL_SECS: u64 = 15;
const HEARTBEAT_INTERVAL_MS: u64 = HEARTBEAT_INTERVAL_SECS * 1000;

/// Spawn a heartbeat task that emits DatabaseStats every 15 seconds,
/// aligned to wall clock boundaries (XX:00, XX:15, XX:30, XX:45).
///
/// Each heartbeat carries clock-sync fields (`intent_time`, `construct_time`,
/// `node_id`) so that peers can compute clock offsets.  After each tick, the
/// next sleep duration is adaptively adjusted to compensate for any drift
/// between the target boundary and the actual fire time.
///
/// Returns a JoinHandle that can be aborted to stop the heartbeat.
pub fn spawn_heartbeat(
    bus: Arc<EventBus>,
    engine: Arc<StorageEngine>,
    node_id: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Align to the next 15-second wall-clock boundary.
        let initial_delay = delay_to_next_boundary();
        tokio::time::sleep(initial_delay).await;

        loop {
            // The boundary we intended to fire on (ms since epoch, rounded
            // down to the nearest 15-second mark).
            let intent_time = aligned_now_ms();

            // Build the heartbeat payload at the actual wall-clock instant.
            let construct_time = chrono::Utc::now().timestamp_millis() as u64;

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
                intent_time,
                construct_time,
                node_id,
            };

            let event = EngineEvent::new(
                EVENT_HEARTBEAT,
                "system",
                serde_json::json!({"stats": heartbeat_data}),
            );
            bus.emit(event);

            // --- Adaptive timing ---
            // Measure how far past the target boundary we actually fired,
            // then subtract that overshoot from the next sleep so we stay
            // aligned.
            let after_emit_ms = chrono::Utc::now().timestamp_millis() as u64;
            let overshoot_ms = after_emit_ms.saturating_sub(intent_time) % HEARTBEAT_INTERVAL_MS;
            let next_sleep_ms = HEARTBEAT_INTERVAL_MS.saturating_sub(overshoot_ms).max(1);

            tokio::time::sleep(Duration::from_millis(next_sleep_ms)).await;
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

/// Current wall-clock time (ms) floored to the nearest 15-second boundary.
fn aligned_now_ms() -> u64 {
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    now_ms - (now_ms % HEARTBEAT_INTERVAL_MS)
}
