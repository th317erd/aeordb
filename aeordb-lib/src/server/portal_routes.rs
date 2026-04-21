use std::sync::Arc;
use axum::extract::State;
use axum::Extension;
use axum::response::{Html, IntoResponse, Json};
use axum::http::{header, StatusCode};
use serde::Serialize;
use super::state::AppState;
use crate::engine::directory_ops::DEFAULT_CHUNK_SIZE;
use crate::engine::health::check_disk;
use crate::engine::rate_tracker::RateSnapshot;

// Embed portal assets at compile time
const PORTAL_HTML: &str = include_str!("../portal/index.html");
const PORTAL_APP_MJS: &str = include_str!("../portal/app.mjs");
const PORTAL_DASHBOARD_MJS: &str = include_str!("../portal/dashboard.mjs");
const PORTAL_USERS_MJS: &str = include_str!("../portal/users.mjs");
const PORTAL_GROUPS_MJS: &str = include_str!("../portal/groups.mjs");
const PORTAL_SHARED_UTILS_JS: &str = include_str!("../portal/shared/utils.js");

/// Serve the main portal HTML page.
pub async fn portal_index() -> Html<&'static str> {
    Html(PORTAL_HTML)
}

/// Serve portal JS assets with correct content type.
pub async fn portal_asset(
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> impl IntoResponse {
    let (content, content_type) = match filename.as_str() {
        "app.mjs" => (PORTAL_APP_MJS, "application/javascript; charset=utf-8"),
        "dashboard.mjs" => (PORTAL_DASHBOARD_MJS, "application/javascript; charset=utf-8"),
        "users.mjs" => (PORTAL_USERS_MJS, "application/javascript; charset=utf-8"),
        "groups.mjs" => (PORTAL_GROUPS_MJS, "application/javascript; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not found").into_response(),
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], content).into_response()
}

/// Serve shared web-component assets (symlinked into portal/shared/ at build time).
pub async fn portal_shared_asset(
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> impl IntoResponse {
    let (content, content_type) = match filename.as_str() {
        "utils.js" => (PORTAL_SHARED_UTILS_JS, "application/javascript; charset=utf-8"),
        _ => return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "Not found").into_response(),
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], content).into_response()
}

// ── Enhanced stats response types ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct EnhancedStats {
    pub identity: StatsIdentity,
    pub counts: StatsCounts,
    pub sizes: StatsSizes,
    pub throughput: StatsThroughput,
    pub health: StatsHealth,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsIdentity {
    pub version: String,
    pub database_path: String,
    pub hash_algorithm: String,
    pub chunk_size: usize,
    pub node_id: u64,
    pub uptime_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsCounts {
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub chunks: u64,
    pub snapshots: u64,
    pub forks: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsSizes {
    pub disk_total: u64,
    pub kv_file: u64,
    pub logical_data: u64,
    pub chunk_data: u64,
    pub void_space: u64,
    pub dedup_savings: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsThroughput {
    pub writes_per_sec: RateSnapshot,
    pub reads_per_sec: RateSnapshot,
    pub bytes_written_per_sec: RateSnapshot,
    pub bytes_read_per_sec: RateSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsHealth {
    pub disk_usage_percent: f64,
    pub dedup_hit_rate: f64,
    pub write_buffer_depth: u64,
}

/// Return O(1) database stats as JSON using atomic counters and rate trackers.
///
/// Replaces the old `engine.stats()` call which performed an O(n) KV scan.
/// All data now comes from:
/// - `EngineCounters::snapshot()` — O(1) atomic reads
/// - `RateTrackerSet::snapshot()` — O(window_size) but bounded at 900 samples
/// - `check_disk()` — single `statvfs` syscall
/// - `std::fs::metadata()` — single `stat` syscall
pub async fn get_stats(
    State(state): State<AppState>,
    rate_ext: Option<Extension<Arc<crate::engine::rate_tracker::RateTrackerSet>>>,
    db_path_ext: Option<Extension<String>>,
) -> Json<EnhancedStats> {
    // O(1) counter snapshot from atomics
    let counters = state.engine.counters().snapshot();

    // Uptime from monotonic Instant (immune to wall-clock drift)
    let uptime_seconds = state.startup_instant.elapsed().as_secs();

    // Hash algorithm name from engine
    let hash_algorithm = format!("{:?}", state.engine.hash_algo());

    // Database file size: single stat() call
    let db_path_string = db_path_ext.map(|Extension(p)| p).unwrap_or_else(|| state.db_path.clone());
    let db_path = &db_path_string;
    let disk_total = std::fs::metadata(db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // KV file size: single stat() call
    let kv_path = format!("{}.kv", db_path);
    let kv_file = std::fs::metadata(&kv_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Dedup savings
    let dedup_savings = counters.logical_data_size
        .saturating_sub(counters.chunk_data_size);

    // Throughput from rate trackers (zero-rate fallback if not wired up)
    // Try AppState first, then Extension layer, then zero fallback
    let ext_trackers = rate_ext.map(|Extension(t)| t);
    let tracker_ref = state.rate_trackers.as_ref().or(ext_trackers.as_ref());
    let throughput = match tracker_ref {
        Some(trackers) => {
            let rate_snapshot = trackers.snapshot();
            StatsThroughput {
                writes_per_sec: rate_snapshot.writes,
                reads_per_sec: rate_snapshot.reads,
                bytes_written_per_sec: rate_snapshot.bytes_written,
                bytes_read_per_sec: rate_snapshot.bytes_read,
            }
        }
        None => {
            let zero = RateSnapshot {
                rate_1m: 0.0,
                rate_5m: 0.0,
                rate_15m: 0.0,
                peak_1m: 0.0,
            };
            StatsThroughput {
                writes_per_sec: zero.clone(),
                reads_per_sec: zero.clone(),
                bytes_written_per_sec: zero.clone(),
                bytes_read_per_sec: zero,
            }
        }
    };

    // Disk health: single statvfs call
    let disk_health = check_disk(db_path);

    // Dedup hit rate: chunks_deduped / (chunks + chunks_deduped)
    let total_chunk_operations = counters.chunks + counters.chunks_deduped_total;
    let dedup_hit_rate = if total_chunk_operations > 0 {
        counters.chunks_deduped_total as f64 / total_chunk_operations as f64
    } else {
        0.0
    };

    // SECURITY: Only expose the database filename, not the full absolute path.
    // The full path leaks server filesystem layout to authenticated users.
    let db_filename = std::path::Path::new(db_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("unknown")
        .to_string();

    let stats = EnhancedStats {
        identity: StatsIdentity {
            version: env!("CARGO_PKG_VERSION").to_string(),
            database_path: db_filename,
            hash_algorithm,
            chunk_size: DEFAULT_CHUNK_SIZE,
            node_id: 1,
            uptime_seconds,
        },
        counts: StatsCounts {
            files: counters.files,
            directories: counters.directories,
            symlinks: counters.symlinks,
            chunks: counters.chunks,
            snapshots: counters.snapshots,
            forks: counters.forks,
        },
        sizes: StatsSizes {
            disk_total,
            kv_file,
            logical_data: counters.logical_data_size,
            chunk_data: counters.chunk_data_size,
            void_space: counters.void_space,
            dedup_savings,
        },
        throughput,
        health: StatsHealth {
            disk_usage_percent: disk_health.usage_percent,
            dedup_hit_rate,
            write_buffer_depth: counters.write_buffer_depth,
        },
    };

    Json(stats)
}
