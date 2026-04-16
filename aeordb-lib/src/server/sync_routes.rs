use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use base64::Engine as _;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::engine::compression::{decompress, CompressionAlgorithm};
use crate::engine::file_record::FileRecord;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::system_tables::SystemTables;
use crate::engine::tree_walker::{diff_trees, walk_version_tree, TreeDiff, VersionTree};
use crate::engine::version_manager::VersionManager;
use crate::engine::storage_engine::StorageEngine;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SyncDiffRequest {
    pub since_root_hash: Option<String>,
    pub current_root_hash: Option<String>,
    pub paths: Option<Vec<String>>,
    pub node_id: Option<u64>,
    pub virtual_time: Option<u64>,
}

#[derive(Serialize)]
pub struct SyncDiffResponse {
    pub root_hash: String,
    pub changes: SyncChanges,
    pub chunk_hashes_needed: Vec<String>,
}

#[derive(Serialize)]
pub struct SyncChanges {
    pub files_added: Vec<SyncFileEntry>,
    pub files_modified: Vec<SyncFileEntry>,
    pub files_deleted: Vec<SyncDeletedEntry>,
    pub symlinks_added: Vec<SyncSymlinkEntry>,
    pub symlinks_modified: Vec<SyncSymlinkEntry>,
    pub symlinks_deleted: Vec<SyncDeletedEntry>,
}

#[derive(Serialize)]
pub struct SyncFileEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub chunk_hashes: Vec<String>,
}

#[derive(Serialize)]
pub struct SyncSymlinkEntry {
    pub path: String,
    pub hash: String,
    pub target: String,
}

#[derive(Serialize)]
pub struct SyncDeletedEntry {
    pub path: String,
}

#[derive(Deserialize)]
pub struct SyncChunksRequest {
    pub hashes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Cluster secret validation
// ---------------------------------------------------------------------------

fn validate_cluster_secret(headers: &HeaderMap, engine: &StorageEngine) -> bool {
    let secret = match headers.get("X-Cluster-Secret") {
        Some(v) => match v.to_str() {
            Ok(s) => s,
            Err(_) => return false,
        },
        None => return false,
    };

    let provided_hash = blake3::hash(secret.as_bytes());
    let system_tables = SystemTables::new(engine);

    match system_tables.get_cluster_secret_hash() {
        Ok(Some(stored_hash)) => provided_hash.as_bytes().to_vec() == stored_hash,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Helpers: convert tree structures to sync response types
// ---------------------------------------------------------------------------

fn file_record_to_sync_entry(path: &str, hash: &[u8], record: &FileRecord) -> SyncFileEntry {
    SyncFileEntry {
        path: path.to_string(),
        hash: hex::encode(hash),
        size: record.total_size,
        content_type: record.content_type.clone(),
        chunk_hashes: record.chunk_hashes.iter().map(|h| hex::encode(h)).collect(),
    }
}

fn symlink_record_to_sync_entry(path: &str, hash: &[u8], record: &SymlinkRecord) -> SyncSymlinkEntry {
    SyncSymlinkEntry {
        path: path.to_string(),
        hash: hex::encode(hash),
        target: record.target.clone(),
    }
}

fn path_matches_filter(path: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if glob_match::glob_match(pattern, path) {
            return true;
        }
    }
    false
}

/// Build a full sync response (no since_root_hash -- everything is "added").
fn build_full_sync_response(
    tree: &VersionTree,
    path_filter: &Option<Vec<String>>,
) -> (SyncChanges, Vec<String>) {
    let mut files_added = Vec::new();
    let mut symlinks_added = Vec::new();
    let mut chunk_hashes: Vec<String> = Vec::new();

    for (path, (hash, record)) in &tree.files {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        let entry = file_record_to_sync_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_added.push(entry);
    }

    for (path, (hash, record)) in &tree.symlinks {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_added.push(symlink_record_to_sync_entry(path, hash, record));
    }

    // Sort for deterministic output
    files_added.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_added.sort_by(|a, b| a.path.cmp(&b.path));
    chunk_hashes.sort();
    chunk_hashes.dedup();

    let changes = SyncChanges {
        files_added,
        files_modified: Vec::new(),
        files_deleted: Vec::new(),
        symlinks_added,
        symlinks_modified: Vec::new(),
        symlinks_deleted: Vec::new(),
    };

    (changes, chunk_hashes)
}

/// Build a diff-based sync response from a TreeDiff.
fn build_sync_response_from_diff(
    diff: &TreeDiff,
    _current_tree: &VersionTree,
    path_filter: &Option<Vec<String>>,
) -> (SyncChanges, Vec<String>) {
    let mut files_added = Vec::new();
    let mut files_modified = Vec::new();
    let mut files_deleted = Vec::new();
    let mut symlinks_added = Vec::new();
    let mut symlinks_modified = Vec::new();
    let mut symlinks_deleted = Vec::new();
    let mut chunk_hashes: Vec<String> = Vec::new();

    for (path, (hash, record)) in &diff.added {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        let entry = file_record_to_sync_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_added.push(entry);
    }

    for (path, (hash, record)) in &diff.modified {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        let entry = file_record_to_sync_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_modified.push(entry);
    }

    for path in &diff.deleted {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        files_deleted.push(SyncDeletedEntry { path: path.clone() });
    }

    for (path, (hash, record)) in &diff.symlinks_added {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_added.push(symlink_record_to_sync_entry(path, hash, record));
    }

    for (path, (hash, record)) in &diff.symlinks_modified {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_modified.push(symlink_record_to_sync_entry(path, hash, record));
    }

    for path in &diff.symlinks_deleted {
        if let Some(ref patterns) = path_filter {
            if !path_matches_filter(path, patterns) {
                continue;
            }
        }
        symlinks_deleted.push(SyncDeletedEntry { path: path.clone() });
    }

    // Sort for deterministic output
    files_added.sort_by(|a, b| a.path.cmp(&b.path));
    files_modified.sort_by(|a, b| a.path.cmp(&b.path));
    files_deleted.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_added.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_modified.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_deleted.sort_by(|a, b| a.path.cmp(&b.path));
    chunk_hashes.sort();
    chunk_hashes.dedup();

    let changes = SyncChanges {
        files_added,
        files_modified,
        files_deleted,
        symlinks_added,
        symlinks_modified,
        symlinks_deleted,
    };

    (changes, chunk_hashes)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /sync/diff -- compute and return tree differences.
pub async fn sync_diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncDiffRequest>,
) -> Response {
    if !validate_cluster_secret(&headers, &state.engine) {
        return ErrorResponse::new("Invalid or missing cluster secret")
            .with_status(StatusCode::UNAUTHORIZED)
            .into_response();
    }

    let vm = VersionManager::new(&state.engine);

    let head_hash = match vm.get_head_hash() {
        Ok(hash) => hash,
        Err(e) => {
            return ErrorResponse::new(format!("Failed to get HEAD: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    };

    let (changes, chunk_hashes) = if let Some(ref since_hex) = payload.since_root_hash {
        let since_hash = match hex::decode(since_hex) {
            Ok(h) => h,
            Err(_) => {
                return ErrorResponse::new("Invalid since_root_hash hex")
                    .with_status(StatusCode::BAD_REQUEST)
                    .into_response()
            }
        };

        let base_tree = match walk_version_tree(&state.engine, &since_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk base tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };

        let current_tree = match walk_version_tree(&state.engine, &head_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk current tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };

        let diff = diff_trees(&base_tree, &current_tree);
        build_sync_response_from_diff(&diff, &current_tree, &payload.paths)
    } else {
        let tree = match walk_version_tree(&state.engine, &head_hash) {
            Ok(t) => t,
            Err(e) => {
                return ErrorResponse::new(format!("Failed to walk tree: {}", e))
                    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                    .into_response()
            }
        };
        build_full_sync_response(&tree, &payload.paths)
    };

    let response = SyncDiffResponse {
        root_hash: hex::encode(&head_hash),
        changes,
        chunk_hashes_needed: chunk_hashes,
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// POST /sync/chunks -- batch chunk transfer.
pub async fn sync_chunks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<SyncChunksRequest>,
) -> Response {
    if !validate_cluster_secret(&headers, &state.engine) {
        return ErrorResponse::new("Invalid or missing cluster secret")
            .with_status(StatusCode::UNAUTHORIZED)
            .into_response();
    }

    let mut chunks: Vec<serde_json::Value> = Vec::new();

    for hex_hash in &payload.hashes {
        let hash = match hex::decode(hex_hash) {
            Ok(h) => h,
            Err(_) => continue,
        };

        if let Ok(Some((header, _key, value))) = state.engine.get_entry(&hash) {
            let data = if header.compression_algo != CompressionAlgorithm::None {
                match decompress(&value, header.compression_algo) {
                    Ok(d) => d,
                    Err(_) => continue,
                }
            } else {
                value
            };

            chunks.push(serde_json::json!({
                "hash": hex_hash,
                "data": base64::engine::general_purpose::STANDARD.encode(&data),
                "size": data.len(),
            }));
        }
    }

    (StatusCode::OK, Json(serde_json::json!({ "chunks": chunks }))).into_response()
}
