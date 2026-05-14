//! Quarantine module for corrupt or unrecoverable data.
//!
//! When corruption is detected anywhere in the engine, the affected data
//! is written to a sibling `lost+found/` directory rather than being
//! silently dropped. This preserves the raw bytes for manual recovery.
//!
//! **IMPORTANT:** Quarantine operations must NEVER fail the parent operation.
//! If writing to lost+found fails (disk full, etc.), log a warning and return.

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// Quarantine raw bytes from a corrupt region.
///
/// Writes `data` to `{parent_path}/lost+found/{filename}`.
/// If `parent_path` is empty or "/", writes to `/lost+found/`.
pub fn quarantine_bytes(
    engine: &StorageEngine,
    parent_path: &str,
    filename: &str,
    reason: &str,
    data: &[u8],
) {
    let lf_path = lost_found_path(parent_path, filename);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);

    tracing::warn!(
        "Quarantining {} bytes to {}: {}",
        data.len(),
        lf_path,
        reason,
    );

    if let Err(e) = ops.store_file_buffered(&ctx, &lf_path, data, Some("application/octet-stream")) {
        tracing::warn!(
            "Failed to write quarantine file {}: {}. Data may be lost.",
            lf_path,
            e,
        );
    }
}

/// Quarantine metadata (JSON) about a corrupt entry.
///
/// Writes a JSON document with offset, reason, timestamp, and any extra fields.
pub fn quarantine_metadata(
    engine: &StorageEngine,
    parent_path: &str,
    filename: &str,
    reason: &str,
    offset: u64,
    extra: Option<&serde_json::Value>,
) {
    let mut meta = serde_json::json!({
        "reason": reason,
        "offset": offset,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    if let Some(extra_data) = extra {
        if let Some(obj) = extra_data.as_object() {
            for (k, v) in obj {
                meta[k.clone()] = v.clone();
            }
        }
    }

    let data = serde_json::to_vec_pretty(&meta).unwrap_or_default();
    let lf_path = lost_found_path(parent_path, filename);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);

    tracing::warn!(
        "Quarantining metadata to {}: {}",
        lf_path,
        reason,
    );

    if let Err(e) = ops.store_file_buffered(&ctx, &lf_path, &data, Some("application/json")) {
        tracing::warn!(
            "Failed to write quarantine metadata {}: {}. Metadata may be lost.",
            lf_path,
            e,
        );
    }
}

/// Build the lost+found path for a given parent and filename.
fn lost_found_path(parent_path: &str, filename: &str) -> String {
    let parent = if parent_path.is_empty() || parent_path == "/" {
        "".to_string()
    } else {
        let trimmed = parent_path.trim_end_matches('/');
        trimmed.to_string()
    };

    format!("{}/lost+found/{}", parent, filename)
}
