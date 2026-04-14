use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::version_access::resolve_file_at_version;
use crate::engine::version_manager::VersionManager;

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
) -> Response {
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

    // Sort by created_at ascending for comparison
    let mut sorted_snapshots = snapshots;
    sorted_snapshots.sort_by(|a, b| {
        a.created_at.cmp(&b.created_at).then_with(|| a.name.cmp(&b.name))
    });

    // Resolve file at each snapshot
    struct FileAtVersion {
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
                    snapshot_name: snapshot.name.clone(),
                    timestamp: snapshot.created_at,
                    content_hash: file_hash,
                    size: file_record.total_size,
                    content_type: file_record.content_type.clone(),
                    found: true,
                });
            }
            Err(_) => {
                entries.push(FileAtVersion {
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
            Some("unchanged")
        } else if !entry.found && previous_found {
            Some("deleted")
        } else {
            // !entry.found && !previous_found -> omit
            None
        };

        if let Some(change) = change_type {
            let mut obj = serde_json::json!({
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
