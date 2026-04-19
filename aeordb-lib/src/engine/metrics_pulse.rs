use std::sync::Arc;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::engine::engine_counters::EngineCounters;
use crate::engine::engine_event::{EngineEvent, EVENT_METRICS};
use crate::engine::event_bus::EventBus;
use crate::engine::rate_tracker::RateTrackerSet;

const METRICS_INTERVAL_SECS: u64 = 15;
const RATE_SAMPLE_INTERVAL_SECS: u64 = 1;

/// Spawn a metrics pulse task that emits detailed engine statistics every 15 seconds.
///
/// Unlike the heartbeat (which is stripped to clock-sync only), the metrics pulse
/// provides counts, sizes, throughput rates, and health indicators. These are
/// consumed by the dashboard, SSE subscribers, and the portal.
///
/// Accepts a [`CancellationToken`] for graceful shutdown.
///
/// Returns a JoinHandle that resolves when the task exits.
pub fn spawn_metrics_pulse(
    bus: Arc<EventBus>,
    counters: Arc<EngineCounters>,
    rate_trackers: Arc<RateTrackerSet>,
    db_path: String,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Metrics pulse shutting down");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(METRICS_INTERVAL_SECS)) => {}
            }

            let snapshot = counters.snapshot();
            let rates = rate_trackers.snapshot();

            // Get the db file size from disk metadata.
            let db_file_size = std::fs::metadata(&db_path)
                .map(|m| m.len())
                .unwrap_or(0);

            // Compute derived metrics.
            let dedup_savings = snapshot.logical_data_size.saturating_sub(snapshot.chunk_data_size);
            let total_chunk_ops = snapshot.chunks + snapshot.chunks_deduped_total;
            let dedup_hit_rate = if total_chunk_ops > 0 {
                snapshot.chunks_deduped_total as f64 / total_chunk_ops as f64
            } else {
                0.0
            };

            let payload = serde_json::json!({
                "counts": {
                    "files": snapshot.files,
                    "directories": snapshot.directories,
                    "symlinks": snapshot.symlinks,
                    "chunks": snapshot.chunks,
                    "snapshots": snapshot.snapshots,
                    "forks": snapshot.forks,
                },
                "sizes": {
                    "logical_data": snapshot.logical_data_size,
                    "chunk_data": snapshot.chunk_data_size,
                    "void_space": snapshot.void_space,
                    "dedup_savings": dedup_savings,
                    "db_file_size": db_file_size,
                },
                "throughput": {
                    "writes_per_sec": {
                        "1m": rates.writes.rate_1m,
                        "5m": rates.writes.rate_5m,
                        "15m": rates.writes.rate_15m,
                        "peak_1m": rates.writes.peak_1m,
                    },
                    "reads_per_sec": {
                        "1m": rates.reads.rate_1m,
                        "5m": rates.reads.rate_5m,
                        "15m": rates.reads.rate_15m,
                        "peak_1m": rates.reads.peak_1m,
                    },
                    "bytes_written_per_sec": {
                        "1m": rates.bytes_written.rate_1m,
                        "5m": rates.bytes_written.rate_5m,
                        "15m": rates.bytes_written.rate_15m,
                        "peak_1m": rates.bytes_written.peak_1m,
                    },
                    "bytes_read_per_sec": {
                        "1m": rates.bytes_read.rate_1m,
                        "5m": rates.bytes_read.rate_5m,
                        "15m": rates.bytes_read.rate_15m,
                        "peak_1m": rates.bytes_read.peak_1m,
                    },
                },
                "health": {
                    "write_buffer_depth": snapshot.write_buffer_depth,
                    "dedup_hit_rate": dedup_hit_rate,
                },
            });

            let event = EngineEvent::new(EVENT_METRICS, "system", payload);
            bus.emit(event);
        }
    })
}

/// Spawn a background rate sampler that feeds the rate trackers every second.
///
/// This task runs at 1 Hz, takes a counters snapshot, and records the
/// monotonic counter values into the rate tracker set so that rolling
/// throughput rates (1m, 5m, 15m) can be computed.
///
/// Accepts a [`CancellationToken`] for graceful shutdown.
///
/// Returns a JoinHandle that resolves when the task exits.
pub fn spawn_rate_sampler(
    counters: Arc<EngineCounters>,
    rate_trackers: Arc<RateTrackerSet>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Rate sampler shutting down");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(RATE_SAMPLE_INTERVAL_SECS)) => {}
            }

            let snapshot = counters.snapshot();
            let timestamp_ms = chrono::Utc::now().timestamp_millis() as u64;
            rate_trackers.record_all(timestamp_ms, &snapshot);
        }
    })
}
