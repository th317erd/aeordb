use serde::Serialize;
use std::path::Path;

use crate::engine::cluster_join::{get_cluster_mode, has_signing_key};
use crate::engine::peer_connection::{ConnectionState, PeerManager};
use crate::engine::storage_engine::StorageEngine;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub status: HealthStatus,
    pub checks: HealthChecks,
    pub uptime_seconds: u64,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthChecks {
    pub engine: EngineHealth,
    pub disk: DiskHealth,
    pub sync: SyncHealth,
    pub auth: AuthHealth,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineHealth {
    pub status: HealthStatus,
    pub entry_count: u64,
    pub db_file_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskHealth {
    pub status: HealthStatus,
    pub available_bytes: u64,
    pub total_bytes: u64,
    pub usage_percent: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncHealth {
    pub status: HealthStatus,
    pub active_peers: usize,
    pub failing_peers: usize,
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthHealth {
    pub status: HealthStatus,
    pub mode: String,
    pub signing_key_present: bool,
}

/// Check engine health by querying atomic counters (O(1), no KV scan).
///
/// `db_path` is used to read the WAL file size from disk metadata.
pub fn check_engine(engine: &StorageEngine, db_path: &str) -> EngineHealth {
    let snapshot = engine.counters().snapshot();
    // Total entry count: files + directories + symlinks + chunks + snapshots + forks
    let entry_count = snapshot.files
        + snapshot.directories
        + snapshot.symlinks
        + snapshot.chunks
        + snapshot.snapshots
        + snapshot.forks;
    let db_file_size_bytes = std::fs::metadata(db_path)
        .map(|m| m.len())
        .unwrap_or(0);
    EngineHealth {
        status: HealthStatus::Healthy,
        entry_count,
        db_file_size_bytes,
    }
}

/// Check disk health for the partition containing the database file.
///
/// On Linux, uses `libc::statvfs` to determine available and total space.
/// Returns a fallback with zero values and Healthy status on non-Linux
/// platforms or if the stat call fails.
pub fn check_disk(db_path: &str) -> DiskHealth {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        let parent = Path::new(db_path)
            .parent()
            .unwrap_or(Path::new("/"));
        if let Ok(c_path) = CString::new(parent.to_str().unwrap_or("/")) {
            unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                    let total = stat.f_blocks as u64 * stat.f_frsize as u64;
                    let available = stat.f_bavail as u64 * stat.f_frsize as u64;
                    let used = total.saturating_sub(available);
                    let usage = if total > 0 {
                        (used as f64 / total as f64) * 100.0
                    } else {
                        0.0
                    };

                    let status = if usage > 98.0 {
                        HealthStatus::Unhealthy
                    } else if usage > 90.0 {
                        HealthStatus::Degraded
                    } else {
                        HealthStatus::Healthy
                    };

                    return DiskHealth {
                        status,
                        available_bytes: available,
                        total_bytes: total,
                        usage_percent: usage,
                    };
                }
            }
        }
    }

    // Fallback: cannot determine disk stats — report healthy with zeros.
    DiskHealth {
        status: HealthStatus::Healthy,
        available_bytes: 0,
        total_bytes: 0,
        usage_percent: 0.0,
    }
}

/// Check sync health by inspecting peer connections.
///
/// A peer with more than 10 consecutive sync failures is considered "failing",
/// which degrades the overall sync health status.
pub fn check_sync(peer_manager: &PeerManager) -> SyncHealth {
    let peers = peer_manager.all_peers();
    let active = peers
        .iter()
        .filter(|p| p.state == ConnectionState::Active)
        .count();
    let failing = peers
        .iter()
        .filter(|p| p.sync_status.consecutive_failures > 10)
        .count();

    let status = if failing > 0 {
        HealthStatus::Degraded
    } else {
        HealthStatus::Healthy
    };

    let details = if failing > 0 {
        Some(format!(
            "{} peer(s) with >10 consecutive sync failures",
            failing
        ))
    } else {
        None
    };

    SyncHealth {
        status,
        active_peers: active,
        failing_peers: failing,
        details,
    }
}

/// Check auth health based on cluster mode and signing key presence.
///
/// In cluster mode, the signing key MUST be present (synced from the leader).
/// If it is missing, the node cannot verify JWTs and auth is unhealthy.
/// In standalone mode, auth is always healthy.
pub fn check_auth(engine: &StorageEngine) -> AuthHealth {
    let mode = get_cluster_mode(engine);
    let key_present = has_signing_key(engine);
    let is_cluster = mode == "cluster";

    let status = if is_cluster && !key_present {
        HealthStatus::Unhealthy
    } else {
        HealthStatus::Healthy
    };

    AuthHealth {
        status,
        mode,
        signing_key_present: key_present,
    }
}

/// Compute overall health status from individual checks.
///
/// If any check is Unhealthy, overall is Unhealthy.
/// If any check is Degraded (and none Unhealthy), overall is Degraded.
/// Otherwise, overall is Healthy.
pub fn compute_overall_status(checks: &HealthChecks) -> HealthStatus {
    let statuses = [
        &checks.engine.status,
        &checks.disk.status,
        &checks.sync.status,
        &checks.auth.status,
    ];

    if statuses.iter().any(|s| **s == HealthStatus::Unhealthy) {
        HealthStatus::Unhealthy
    } else if statuses.iter().any(|s| **s == HealthStatus::Degraded) {
        HealthStatus::Degraded
    } else {
        HealthStatus::Healthy
    }
}

/// Run a full health check across all subsystems.
///
/// `startup_time` is the server start time in milliseconds since epoch.
pub fn full_health_check(
    engine: &StorageEngine,
    db_path: &str,
    peer_manager: &PeerManager,
    startup_time: u64,
) -> HealthReport {
    let engine_check = check_engine(engine, db_path);
    let disk_check = check_disk(db_path);
    let sync_check = check_sync(peer_manager);
    let auth_check = check_auth(engine);

    let checks = HealthChecks {
        engine: engine_check,
        disk: disk_check,
        sync: sync_check,
        auth: auth_check,
    };

    let status = compute_overall_status(&checks);
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let uptime = now.saturating_sub(startup_time) / 1000;

    HealthReport {
        status,
        checks,
        uptime_seconds: uptime,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}
