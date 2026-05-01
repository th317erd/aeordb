use axum::{
    Extension,
    extract::{Path, Query as AxumQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::version_access::resolve_file_at_version;
use crate::engine::version_manager::VersionManager;
use crate::engine::directory_ops::{DirectoryOps, is_system_path};
use crate::engine::request_context::RequestContext;
use crate::engine::user::is_root;

#[derive(Deserialize)]
pub struct RestoreRequest {
    /// Snapshot ID (hex root hash) — preferred.
    pub id: Option<String>,
    /// Snapshot name — fallback.
    pub snapshot: Option<String>,
    /// Direct version hash — for advanced use.
    pub version: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

/// GET /version/file-history/{*path}
///
/// Returns the change history of a single file across all snapshots.
/// Each entry includes the snapshot name, timestamp, change_type
/// ("added", "modified", "unchanged", or "deleted"), and — when the
/// file exists in that snapshot — its size and content hash.
///
/// The response is ordered newest-first.
pub async fn file_history(
    State(state): State<AppState>,
    Extension(_claims): Extension<TokenClaims>,
    Path(path): Path<String>,
    AxumQuery(query): AxumQuery<HistoryQuery>,
) -> Response {
    let max_snapshots = query.limit.unwrap_or(200).min(1000);
    // Block ALL access to /.aeordb-system/ via API — system data is only accessible
    // through the internal system_store module, never through HTTP endpoints.
    if is_system_path(&path) {
        return ErrorResponse::new(format!("Not found: {}", path))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

    let vm = VersionManager::new(&state.engine);

    // List all snapshots
    let snapshots = match vm.list_snapshots() {
        Ok(snaps) => snaps,
        Err(error) => {
            tracing::error!("Failed to list snapshots: {}", error);
            return ErrorResponse::new(format!("Failed to list snapshots: {}", error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    // Sort by created_at ascending for comparison, cap to limit
    let mut sorted_snapshots = snapshots;
    sorted_snapshots.sort_by(|a, b| {
        a.created_at.cmp(&b.created_at).then_with(|| a.name.cmp(&b.name))
    });
    // Keep only the most recent snapshots (sorted ascending, so take from the end)
    if sorted_snapshots.len() > max_snapshots {
        sorted_snapshots = sorted_snapshots.split_off(sorted_snapshots.len() - max_snapshots);
    }

    // Resolve file at each snapshot
    struct FileAtVersion {
        snapshot_id: String,
        snapshot_name: String,
        timestamp: i64,
        content_hash: Vec<u8>,
        size: u64,
        content_type: Option<String>,
        found: bool,
    }

    let mut entries: Vec<FileAtVersion> = Vec::new();

    for snapshot in &sorted_snapshots {
        match resolve_file_at_version(&state.engine, &snapshot.root_hash, &path) {
            Ok((file_hash, file_record)) => {
                entries.push(FileAtVersion {
                    snapshot_id: snapshot.id(),
                    snapshot_name: snapshot.name.clone(),
                    timestamp: snapshot.created_at,
                    content_hash: file_hash,
                    size: file_record.total_size,
                    content_type: file_record.content_type.clone(),
                    found: true,
                });
            }
            Err(e) => {
                tracing::warn!(
                    snapshot = %snapshot.name,
                    root_hash = %hex::encode(&snapshot.root_hash),
                    path = %path,
                    error = %e,
                    "file_history: resolve failed for snapshot"
                );
                entries.push(FileAtVersion {
                    snapshot_id: snapshot.id(),
                    snapshot_name: snapshot.name.clone(),
                    timestamp: snapshot.created_at,
                    content_hash: Vec::new(),
                    size: 0,
                    content_type: None,
                    found: false,
                });
            }
        }
    }

    // Compute change_type by comparing to previous entry
    let mut history: Vec<serde_json::Value> = Vec::new();
    let mut previous_found = false;
    let mut previous_hash: Vec<u8> = Vec::new();

    for entry in &entries {
        let change_type = if entry.found && !previous_found {
            Some("added")
        } else if entry.found && previous_found && entry.content_hash != previous_hash {
            Some("modified")
        } else if entry.found && previous_found && entry.content_hash == previous_hash {
            None // skip unchanged — only show snapshots where the file changed
        } else if !entry.found && previous_found {
            Some("deleted")
        } else {
            // !entry.found && !previous_found -> omit
            None
        };

        if let Some(change) = change_type {
            let mut obj = serde_json::json!({
                "id": entry.snapshot_id,
                "snapshot": entry.snapshot_name,
                "timestamp": entry.timestamp,
                "change_type": change,
            });

            if entry.found {
                obj["size"] = serde_json::json!(entry.size);
                obj["content_hash"] = serde_json::json!(hex::encode(&entry.content_hash));
                if let Some(ref ct) = entry.content_type {
                    obj["content_type"] = serde_json::json!(ct);
                }
            }

            history.push(obj);
        }

        previous_found = entry.found;
        if entry.found {
            previous_hash = entry.content_hash.clone();
        }
    }

    // Reverse for newest-first output
    history.reverse();

    let response = serde_json::json!({
        "path": path,
        "history": history,
    });

    (StatusCode::OK, Json(response)).into_response()
}

/// POST /version/file-restore/{*path}
///
/// Restores a file from a historical snapshot/version to the current HEAD.
/// Creates an automatic safety snapshot before the restore.
///
/// Requires both write permission on the path AND snapshot permission (root only).
/// If the safety snapshot cannot be created, the restore is rejected (403).
pub async fn file_restore(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(path): Path<String>,
    Json(payload): Json<RestoreRequest>,
) -> Response {
    // Block ALL access to /.aeordb-system/ via API — system data is only accessible
    // through the internal system_store module, never through HTTP endpoints.
    if is_system_path(&path) {
        return ErrorResponse::new(format!("Not found: {}", path))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

    // Auth: Restore requires root (snapshot permission)
    let user_id = match uuid::Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return ErrorResponse::new("Invalid user ID: token 'sub' claim is not a valid UUID")
                .with_status(StatusCode::FORBIDDEN)
                .into_response();
        }
    };
    if !is_root(&user_id) {
        return ErrorResponse::new("root access required. File restore requires root permissions (snapshot + write access)")
            .with_status(StatusCode::FORBIDDEN)
            .into_response();
    }

    let vm = VersionManager::new(&state.engine);

    // Resolve root hash: id takes precedence, then snapshot name, then version hash
    let (root_hash, source_label) = if let Some(ref snapshot_id) = payload.id {
        match vm.resolve_snapshot(snapshot_id) {
            Ok(snap) => {
                let label = format!("snapshot '{}' ({})", snap.name, snap.id());
                (snap.root_hash, label)
            }
            Err(_) => {
                return ErrorResponse::new(format!("Snapshot not found: '{}'", snapshot_id))
                    .with_status(StatusCode::NOT_FOUND)
                    .into_response();
            }
        }
    } else if let Some(ref snapshot_name) = payload.snapshot {
        match vm.resolve_root_hash(Some(snapshot_name)) {
            Ok(hash) => (hash, format!("snapshot '{}'", snapshot_name)),
            Err(_) => {
                return ErrorResponse::new(format!("Snapshot '{}' not found. Use GET /versions/snapshots to list available snapshots", snapshot_name))
                    .with_status(StatusCode::NOT_FOUND)
                    .into_response();
            }
        }
    } else if let Some(ref version_hex) = payload.version {
        match hex::decode(version_hex) {
            Ok(hash) => (hash, format!("version '{}'", version_hex)),
            Err(_) => {
                return ErrorResponse::new("Invalid version hash: value is not valid hex. Use the root_hash from a snapshot or version response")
                    .with_status(StatusCode::BAD_REQUEST)
                    .into_response();
            }
        }
    } else {
        return ErrorResponse::new("Request must include 'snapshot' or 'version' field. Provide {\"snapshot\": \"<name>\"} or {\"version\": \"<hex_hash>\"}")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    };

    // Resolve the historical file to verify it exists
    let (_, file_record) = match resolve_file_at_version(
        &state.engine, &root_hash, &path,
    ) {
        Ok(result) => result,
        Err(crate::engine::errors::EngineError::NotFound(msg)) => {
            return ErrorResponse::new(msg)
                .with_status(StatusCode::NOT_FOUND)
                .into_response();
        }
        Err(error) => {
            return ErrorResponse::new(format!("Failed to resolve file at {}: {}", source_label, error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    // Create auto-snapshot BEFORE restore (mandatory safety net).
    // Uses the restore lane so it doesn't interfere with delete auto-snapshots.
    let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
    state.engine.last_auto_snapshot_restore.store(
      chrono::Utc::now().timestamp_millis(),
      std::sync::atomic::Ordering::Relaxed,
    );
    let now = chrono::Utc::now();
    let base_name = now.format("auto-pre-restore %Y-%m-%d %H:%M:%S%.3f").to_string();

    let auto_snapshot_name = {
        let mut name = base_name.clone();
        let mut attempt = 1;
        loop {
            match vm.create_snapshot(&ctx, &name, {
                let mut metadata = std::collections::HashMap::new();
                metadata.insert("reason".to_string(), "auto-snapshot before file restore".to_string());
                metadata.insert("restored_path".to_string(), path.clone());
                metadata
            }) {
                Ok(_) => break name,
                Err(_) if attempt < 10 => {
                    attempt += 1;
                    name = format!("{}-{}", base_name, attempt);
                }
                Err(error) => {
                    return ErrorResponse::new(format!(
                        "Failed to create safety snapshot: {}. Restore aborted.", error
                    ))
                        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                        .into_response();
                }
            }
        }
    };

    // Restore the file by re-using the existing chunk hashes from the
    // historical FileRecord. This avoids loading the entire file into memory.
    let directory_ops = DirectoryOps::new(&state.engine);
    let size = file_record.total_size;

    match directory_ops.restore_file_from_record(&ctx, &path, &file_record) {
        Ok(_) => {}
        Err(error) => {
            return ErrorResponse::new(format!("Failed to write restored file: {}", error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    }

    // Build response
    let mut response = serde_json::json!({
        "restored": true,
        "path": path,
        "auto_snapshot": auto_snapshot_name,
        "size": size,
    });

    if let Some(ref snapshot_name) = payload.snapshot {
        response["from_snapshot"] = serde_json::json!(snapshot_name);
    }
    if let Some(ref version_hex) = payload.version {
        response["from_version"] = serde_json::json!(version_hex);
    }

    (StatusCode::OK, Json(response)).into_response()
}
