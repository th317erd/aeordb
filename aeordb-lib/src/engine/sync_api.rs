//! Library-level sync API — pure synchronous functions mirroring the HTTP sync protocol.
//!
//! These functions expose the same functionality as the HTTP sync endpoints in
//! `sync_routes.rs`, but as direct library calls with typed structs instead of JSON.
//! This allows embedded clients to replicate without HTTP overhead.

use crate::engine::compression::{decompress, CompressionAlgorithm};
use crate::engine::conflict_store;
use crate::engine::directory_ops;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::file_record::FileRecord;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::tree_walker::{diff_trees, walk_version_tree, TreeDiff, VersionTree};
use crate::engine::version_manager::VersionManager;

// ---------------------------------------------------------------------------
// Re-exports from conflict_store
// ---------------------------------------------------------------------------

pub use crate::engine::conflict_store::{dismiss_conflict, get_conflict, resolve_conflict};

// ---------------------------------------------------------------------------
// Sync diff types
// ---------------------------------------------------------------------------

/// A file entry in a sync diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncFileEntry {
    pub path: String,
    pub hash: Vec<u8>,
    pub size: u64,
    pub content_type: Option<String>,
    pub chunk_hashes: Vec<Vec<u8>>,
}

/// A symlink entry in a sync diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncSymlinkEntry {
    pub path: String,
    pub hash: Vec<u8>,
    pub target: String,
}

/// A deleted entry in a sync diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncDeletedEntry {
    pub path: String,
}

/// The result of computing a sync diff.
#[derive(Debug, Clone)]
pub struct SyncDiff {
    pub root_hash: Vec<u8>,
    pub files_added: Vec<SyncFileEntry>,
    pub files_modified: Vec<SyncFileEntry>,
    pub files_deleted: Vec<SyncDeletedEntry>,
    pub symlinks_added: Vec<SyncSymlinkEntry>,
    pub symlinks_modified: Vec<SyncSymlinkEntry>,
    pub symlinks_deleted: Vec<SyncDeletedEntry>,
    pub chunk_hashes_needed: Vec<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Chunk types
// ---------------------------------------------------------------------------

/// A chunk of data identified by its hash.
#[derive(Debug, Clone)]
pub struct ChunkData {
    pub hash: Vec<u8>,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Conflict types
// ---------------------------------------------------------------------------

/// A conflict record with structured data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConflictRecord {
    pub path: String,
    pub conflict_type: String,
    pub auto_winner: String,
    pub created_at: i64,
    pub winner: ConflictVersionInfo,
    pub loser: ConflictVersionInfo,
}

/// Version info for one side of a conflict.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConflictVersionInfo {
    pub hash: String,
    pub virtual_time: u64,
    pub node_id: u64,
    pub size: u64,
    pub content_type: Option<String>,
}

// ---------------------------------------------------------------------------
// 1. compute_sync_diff
// ---------------------------------------------------------------------------

/// Compute the diff between the local database state and a reference point.
///
/// If `since_root_hash` is None, returns the entire tree as "added".
/// If `paths_filter` is Some, only entries matching the glob patterns are included.
/// If `include_system` is false, entries under `/.system/` are excluded.
///
/// NOTE: If `since_root_hash` refers to a hash that does not exist in the engine,
/// `walk_version_tree` returns an empty tree, causing the diff to treat all current
/// entries as "added" -- effectively a full re-sync. This is a safe degradation but
/// may cause unexpected bandwidth usage. Callers should validate the base hash
/// if they want to detect this case.
pub fn compute_sync_diff(
    engine: &StorageEngine,
    since_root_hash: Option<&[u8]>,
    paths_filter: Option<&[String]>,
    include_system: bool,
) -> EngineResult<SyncDiff> {
    let vm = VersionManager::new(engine);
    let head_hash = vm.get_head_hash()?;

    let current_tree = walk_version_tree(engine, &head_hash)?;

    let (mut diff_result, chunk_hashes) = if let Some(since) = since_root_hash {
        let base_tree = walk_version_tree(engine, since)?;
        let diff = diff_trees(&base_tree, &current_tree);
        build_diff_from_tree_diff(&diff, &current_tree)
    } else {
        build_full_diff(&current_tree)
    };

    // Apply path filtering
    if let Some(paths) = paths_filter {
        filter_diff_by_paths(&mut diff_result, paths);
    }

    // Apply system filtering
    if !include_system {
        filter_diff_system(&mut diff_result);
    }

    diff_result.root_hash = head_hash;
    diff_result.chunk_hashes_needed = chunk_hashes;

    Ok(diff_result)
}

// ---------------------------------------------------------------------------
// 2. get_needed_chunks
// ---------------------------------------------------------------------------

/// Retrieve chunks by their hashes from the local engine.
///
/// Returns only chunks that exist locally. Missing hashes are silently skipped.
/// Chunks are automatically decompressed if stored compressed.
pub fn get_needed_chunks(
    engine: &StorageEngine,
    chunk_hashes: &[Vec<u8>],
) -> EngineResult<Vec<ChunkData>> {
    let mut result = Vec::new();

    for hash in chunk_hashes {
        if let Some((header, _key, value)) = engine.get_entry(hash)? {
            let data = if header.compression_algo != CompressionAlgorithm::None {
                decompress(&value, header.compression_algo)?
            } else {
                value
            };
            result.push(ChunkData {
                hash: hash.clone(),
                data,
            });
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// 3. apply_sync_chunks
// ---------------------------------------------------------------------------

/// Store chunks received from a remote peer into the local engine.
///
/// Skips chunks that already exist locally (dedup).
/// Returns the number of new chunks stored.
pub fn apply_sync_chunks(engine: &StorageEngine, chunks: &[ChunkData]) -> EngineResult<usize> {
    let mut stored = 0;

    for chunk in chunks {
        if !engine.has_entry(&chunk.hash)? {
            engine.store_entry(EntryType::Chunk, &chunk.hash, &chunk.data)?;
            stored += 1;
        }
    }

    Ok(stored)
}

// ---------------------------------------------------------------------------
// 4. list_conflicts_typed
// ---------------------------------------------------------------------------

/// List all unresolved conflicts with typed data.
///
/// Malformed conflict records that fail deserialization are logged and skipped
/// rather than causing the entire listing to fail.
pub fn list_conflicts_typed(engine: &StorageEngine) -> EngineResult<Vec<ConflictRecord>> {
    let raw = conflict_store::list_conflicts(engine)?;
    let mut conflicts = Vec::new();
    for value in raw {
        match serde_json::from_value::<ConflictRecord>(value.clone()) {
            Ok(record) => conflicts.push(record),
            Err(e) => tracing::warn!("Skipping malformed conflict record: {}", e),
        }
    }
    Ok(conflicts)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a FileRecord into a SyncFileEntry.
fn file_record_to_entry(path: &str, hash: &[u8], record: &FileRecord) -> SyncFileEntry {
    SyncFileEntry {
        path: path.to_string(),
        hash: hash.to_vec(),
        size: record.total_size,
        content_type: record.content_type.clone(),
        chunk_hashes: record.chunk_hashes.clone(),
    }
}

/// Convert a SymlinkRecord into a SyncSymlinkEntry.
fn symlink_record_to_entry(path: &str, hash: &[u8], record: &SymlinkRecord) -> SyncSymlinkEntry {
    SyncSymlinkEntry {
        path: path.to_string(),
        hash: hash.to_vec(),
        target: record.target.clone(),
    }
}

/// Build a full sync diff (no base hash) — everything in the tree is "added".
fn build_full_diff(tree: &VersionTree) -> (SyncDiff, Vec<Vec<u8>>) {
    let mut files_added = Vec::new();
    let mut symlinks_added = Vec::new();
    let mut chunk_hashes: Vec<Vec<u8>> = Vec::new();

    for (path, (hash, record)) in &tree.files {
        let entry = file_record_to_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_added.push(entry);
    }

    for (path, (hash, record)) in &tree.symlinks {
        symlinks_added.push(symlink_record_to_entry(path, hash, record));
    }

    // Sort for deterministic output
    files_added.sort_by(|a, b| a.path.cmp(&b.path));
    symlinks_added.sort_by(|a, b| a.path.cmp(&b.path));
    chunk_hashes.sort();
    chunk_hashes.dedup();

    let diff = SyncDiff {
        root_hash: Vec::new(), // filled in by caller
        files_added,
        files_modified: Vec::new(),
        files_deleted: Vec::new(),
        symlinks_added,
        symlinks_modified: Vec::new(),
        symlinks_deleted: Vec::new(),
        chunk_hashes_needed: Vec::new(), // filled in by caller
    };

    (diff, chunk_hashes)
}

/// Build a sync diff from a TreeDiff (incremental sync).
fn build_diff_from_tree_diff(
    diff: &TreeDiff,
    _current_tree: &VersionTree,
) -> (SyncDiff, Vec<Vec<u8>>) {
    let mut files_added = Vec::new();
    let mut files_modified = Vec::new();
    let mut files_deleted = Vec::new();
    let mut symlinks_added = Vec::new();
    let mut symlinks_modified = Vec::new();
    let mut symlinks_deleted = Vec::new();
    let mut chunk_hashes: Vec<Vec<u8>> = Vec::new();

    for (path, (hash, record)) in &diff.added {
        let entry = file_record_to_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_added.push(entry);
    }

    for (path, (hash, record)) in &diff.modified {
        let entry = file_record_to_entry(path, hash, record);
        chunk_hashes.extend(entry.chunk_hashes.iter().cloned());
        files_modified.push(entry);
    }

    for path in &diff.deleted {
        files_deleted.push(SyncDeletedEntry {
            path: path.clone(),
        });
    }

    for (path, (hash, record)) in &diff.symlinks_added {
        symlinks_added.push(symlink_record_to_entry(path, hash, record));
    }

    for (path, (hash, record)) in &diff.symlinks_modified {
        symlinks_modified.push(symlink_record_to_entry(path, hash, record));
    }

    for path in &diff.symlinks_deleted {
        symlinks_deleted.push(SyncDeletedEntry {
            path: path.clone(),
        });
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

    let result = SyncDiff {
        root_hash: Vec::new(),
        files_added,
        files_modified,
        files_deleted,
        symlinks_added,
        symlinks_modified,
        symlinks_deleted,
        chunk_hashes_needed: Vec::new(),
    };

    (result, chunk_hashes)
}

/// Filter diff entries to only include those matching at least one glob pattern.
fn filter_diff_by_paths(diff: &mut SyncDiff, patterns: &[String]) {
    if patterns.is_empty() {
        return;
    }

    let matches = |path: &str| -> bool {
        patterns
            .iter()
            .any(|pattern| glob_match::glob_match(pattern, path))
    };

    diff.files_added.retain(|e| matches(&e.path));
    diff.files_modified.retain(|e| matches(&e.path));
    diff.files_deleted.retain(|e| matches(&e.path));
    diff.symlinks_added.retain(|e| matches(&e.path));
    diff.symlinks_modified.retain(|e| matches(&e.path));
    diff.symlinks_deleted.retain(|e| matches(&e.path));
}

/// Remove entries whose path starts with `/.system`.
fn filter_diff_system(diff: &mut SyncDiff) {
    let is_system = |path: &str| -> bool { directory_ops::is_system_path(path) };

    diff.files_added.retain(|e| !is_system(&e.path));
    diff.files_modified.retain(|e| !is_system(&e.path));
    diff.files_deleted.retain(|e| !is_system(&e.path));
    diff.symlinks_added.retain(|e| !is_system(&e.path));
    diff.symlinks_modified.retain(|e| !is_system(&e.path));
    diff.symlinks_deleted.retain(|e| !is_system(&e.path));
}

// ---------------------------------------------------------------------------
// File History + Restore (library equivalents of HTTP-only handlers)
// ---------------------------------------------------------------------------

/// A single entry in a file's version history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileHistoryEntry {
    pub snapshot: String,
    pub timestamp: i64,
    pub change_type: String, // "added", "modified", "unchanged", "deleted"
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub content_hash: Option<String>, // hex
}

/// Get the version history of a single file across all snapshots.
///
/// Returns entries ordered newest-first, each with a change_type indicating
/// what happened to the file at that snapshot (added/modified/unchanged/deleted).
pub fn file_history(
    engine: &StorageEngine,
    path: &str,
) -> EngineResult<Vec<FileHistoryEntry>> {
    use crate::engine::version_access::resolve_file_at_version;

    let vm = crate::engine::version_manager::VersionManager::new(engine);
    let mut snapshots = vm.list_snapshots()?;
    snapshots.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.name.cmp(&b.name)));

    let mut history: Vec<FileHistoryEntry> = Vec::new();
    let mut previous_found = false;
    let mut previous_hash: Vec<u8> = Vec::new();

    for snapshot in &snapshots {
        let (found, file_hash, size, content_type) =
            match resolve_file_at_version(engine, &snapshot.root_hash, path) {
                Ok((hash, record)) => (true, hash, record.total_size, record.content_type.clone()),
                Err(_) => (false, Vec::new(), 0, None),
            };

        let change_type = if found && !previous_found {
            Some("added")
        } else if found && previous_found && file_hash != previous_hash {
            Some("modified")
        } else if found && previous_found && file_hash == previous_hash {
            Some("unchanged")
        } else if !found && previous_found {
            Some("deleted")
        } else {
            None
        };

        if let Some(change) = change_type {
            let mut entry = FileHistoryEntry {
                snapshot: snapshot.name.clone(),
                timestamp: snapshot.created_at,
                change_type: change.to_string(),
                size: None,
                content_type: None,
                content_hash: None,
            };

            if found {
                entry.size = Some(size);
                entry.content_hash = Some(hex::encode(&file_hash));
                entry.content_type = content_type;
            }

            history.push(entry);
        }

        previous_found = found;
        if found {
            previous_hash = file_hash;
        }
    }

    history.reverse(); // newest first
    Ok(history)
}

/// Restore a file from a historical snapshot/version to the current HEAD.
///
/// Creates an automatic safety snapshot before restoring.
/// Returns the auto-snapshot name and the restored file size.
pub fn file_restore_from_version(
    engine: &StorageEngine,
    ctx: &crate::engine::request_context::RequestContext,
    path: &str,
    snapshot_name: Option<&str>,
    version_hash: Option<&[u8]>,
) -> EngineResult<(String, u64)> {
    use crate::engine::version_access::read_file_at_version;
    use std::collections::HashMap;

    let vm = crate::engine::version_manager::VersionManager::new(engine);

    // Resolve root hash
    let root_hash = if let Some(name) = snapshot_name {
        vm.resolve_root_hash(Some(name))?
    } else if let Some(hash) = version_hash {
        hash.to_vec()
    } else {
        return Err(crate::engine::errors::EngineError::InvalidInput(
            "Must provide snapshot_name or version_hash".to_string(),
        ));
    };

    // Resolve the file at the version
    let (_, file_record) = crate::engine::version_access::resolve_file_at_version(
        engine, &root_hash, path,
    )?;

    // Create auto-snapshot
    let now = chrono::Utc::now();
    let base_name = now.format("pre-restore-%Y-%m-%dT%H-%M-%SZ").to_string();
    let auto_snapshot_name = {
        let mut name = base_name.clone();
        let mut attempt = 1;
        loop {
            let mut metadata = HashMap::new();
            metadata.insert("reason".to_string(), "auto-snapshot before file restore".to_string());
            metadata.insert("restored_path".to_string(), path.to_string());
            match vm.create_snapshot(ctx, &name, metadata) {
                Ok(_) => break name,
                Err(_) if attempt < 10 => {
                    attempt += 1;
                    name = format!("{}-{}", base_name, attempt);
                }
                Err(error) => return Err(error),
            }
        }
    };

    // Read historical file content
    let content = read_file_at_version(engine, &root_hash, path)?;
    let size = content.len() as u64;

    // Write to HEAD
    let ops = crate::engine::directory_ops::DirectoryOps::new(engine);
    ops.store_file(ctx, path, &content, file_record.content_type.as_deref())?;

    Ok((auto_snapshot_name, size))
}
