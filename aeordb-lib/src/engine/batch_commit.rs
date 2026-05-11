use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::engine::btree;
use crate::engine::content_type::detect_content_type;
use crate::engine::directory_entry::{
    ChildEntry, deserialize_child_entries, serialize_child_entries,
};
use crate::engine::directory_ops::{
    directory_content_hash, directory_path_hash, file_content_hash, file_identity_hash, file_path_hash,
    is_system_path,
};
use crate::engine::engine_event::{EntryEventData, EVENT_ENTRIES_CREATED};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// A file to commit as part of a batch, with pre-uploaded chunk hashes.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitFile {
    pub path: String,
    /// Hex-encoded chunk hashes (matching hashes already in the KV store).
    pub chunks: Vec<String>,
    #[serde(default)]
    pub content_type: Option<String>,
}

/// Result of a successful batch commit.
#[derive(Debug, Clone, Serialize)]
pub struct CommitResult {
    pub committed: usize,
    pub files: Vec<CommittedFile>,
}

/// Metadata for a single committed file.
#[derive(Debug, Clone, Serialize)]
pub struct CommittedFile {
    pub path: String,
    pub size: u64,
}

/// Atomically commit multiple files from pre-uploaded chunks.
///
/// 1. Validates all chunk hashes exist in the KV store
/// 2. Creates FileRecords from chunk hash lists (preserving created_at on overwrite)
/// 3. Updates directories in a single pass (each directory updated once)
/// 4. Updates HEAD once
/// 5. Emits a single `entries_created` event
pub fn commit_files(
    engine: &StorageEngine,
    ctx: &RequestContext,
    files: Vec<CommitFile>,
) -> EngineResult<CommitResult> {
    if files.is_empty() {
        return Err(EngineError::InvalidInput(
            "No files provided for commit".to_string(),
        ));
    }

    // Reject any path under /.aeordb-system/ or /.aeordb-config/. System data
    // is written exclusively through dedicated APIs (system_store, directory_ops
    // with FLAG_SYSTEM) — never through user-facing batch commit. Without this
    // check, an authenticated user could overwrite /.aeordb-system/api-keys/<uuid>
    // and mint themselves a root key.
    for file in &files {
        if is_system_path(&file.path) {
            return Err(EngineError::InvalidInput(format!(
                "Path '{}' is reserved for internal system data and cannot be written through this endpoint",
                file.path
            )));
        }
    }

    let algo = engine.hash_algo();
    let hash_length = algo.hash_length();

    // --- Phase 1: Validate all chunk hashes exist ---
    let mut missing_chunks: Vec<String> = Vec::new();
    // Decode all hex chunk hashes upfront and validate existence.
    // file_chunks[i] = Vec of (raw_hash_bytes, chunk_byte_size) for files[i].
    let mut file_chunks: Vec<Vec<(Vec<u8>, u64)>> = Vec::with_capacity(files.len());

    for file in &files {
        let mut chunks_for_file: Vec<(Vec<u8>, u64)> = Vec::with_capacity(file.chunks.len());
        for hex_hash in &file.chunks {
            let raw_hash = hex::decode(hex_hash).map_err(|e| {
                EngineError::InvalidInput(format!(
                    "Invalid hex chunk hash '{}': {}",
                    hex_hash, e
                ))
            })?;

            // Verify chunk exists in KV store
            match engine.get_entry(&raw_hash)? {
                Some((_header, _key, value)) => {
                    chunks_for_file.push((raw_hash, value.len() as u64));
                }
                None => {
                    missing_chunks.push(hex_hash.clone());
                }
            }
        }
        file_chunks.push(chunks_for_file);
    }

    if !missing_chunks.is_empty() {
        return Err(EngineError::InvalidInput(format!(
            "Missing {} chunk(s): {}",
            missing_chunks.len(),
            missing_chunks.join(", ")
        )));
    }

    // --- Phase 2: Create FileRecords ---
    // Collect (normalized_path, file_key, file_record, ChildEntry) for each file.
    struct FileInfo {
        normalized_path: String,
        file_record: FileRecord,
        child_entry: ChildEntry,
    }

    let mut file_infos: Vec<FileInfo> = Vec::with_capacity(files.len());
    let mut event_entries: Vec<EntryEventData> = Vec::with_capacity(files.len());

    for (i, file) in files.iter().enumerate() {
        let normalized = normalize_path(&file.path);
        let chunk_hashes: Vec<Vec<u8>> = file_chunks[i]
            .iter()
            .map(|(h, _)| h.clone())
            .collect();

        // Compute total size from chunk data sizes
        let total_size: u64 = file_chunks[i].iter().map(|(_, sz)| *sz).sum();

        // Detect content type: read first chunk data for magic byte detection
        let detected_content_type = if let Some(ref ct) = file.content_type {
            ct.clone()
        } else if !chunk_hashes.is_empty() {
            // Read first chunk to detect content type from magic bytes
            match engine.get_entry(&chunk_hashes[0])? {
                Some((_h, _k, v)) => detect_content_type(&v, None),
                None => "application/octet-stream".to_string(),
            }
        } else {
            "application/octet-stream".to_string()
        };

        // Check if file already exists (preserve created_at on overwrite)
        let file_key = file_path_hash(&normalized, &algo)?;
        let existing_created_at = match engine.get_entry(&file_key)? {
            Some((header, _key, value)) => {
                let existing = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
                Some(existing.created_at)
            }
            None => None,
        };

        let mut file_record = FileRecord::new(
            normalized.clone(),
            Some(detected_content_type.clone()),
            total_size,
            chunk_hashes,
        );

        if let Some(original_created_at) = existing_created_at {
            file_record.created_at = original_created_at;
        }

        // Store the FileRecord
        let file_value = file_record.serialize(hash_length)?;

        // Content-addressed key (immutable — for versioning via ChildEntry.hash)
        let file_content_key = file_content_hash(&file_value, &algo)?;
        engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;

        // Path-based key (mutable — for reads, indexing, deletion)
        engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

        let identity_key = file_identity_hash(&normalized, Some(detected_content_type.as_str()), &file_record.chunk_hashes, &algo)?;
        // Store at identity key so tree walker can look up entries by ChildEntry.hash
        engine.store_entry(EntryType::FileRecord, &identity_key, &file_value)?;

        let child = ChildEntry {
            entry_type: EntryType::FileRecord.to_u8(),
            hash: identity_key,
            total_size,
            created_at: file_record.created_at,
            updated_at: file_record.updated_at,
            name: file_name(&normalized).unwrap_or("").to_string(),
            content_type: Some(detected_content_type.clone()),
            virtual_time: chrono::Utc::now().timestamp_millis() as u64,
            node_id: 0,
        };

        event_entries.push(EntryEventData {
            path: normalized.clone(),
            entry_type: "file".to_string(),
            content_type: Some(detected_content_type),
            size: total_size,
            hash: hex::encode(
                file_record
                    .chunk_hashes
                    .first()
                    .unwrap_or(&vec![]),
            ),
            created_at: file_record.created_at,
            updated_at: file_record.updated_at,
            previous_hash: None,
        });

        file_infos.push(FileInfo {
            normalized_path: normalized,
            file_record,
            child_entry: child,
        });
    }

    // --- Phase 3: Single-pass directory propagation ---
    // Group files by their immediate parent directory.
    // Key = parent dir path, Value = Vec of ChildEntry for files in that dir.
    let mut dir_children: HashMap<String, Vec<ChildEntry>> = HashMap::new();

    for info in &file_infos {
        if let Some(parent) = parent_path(&info.normalized_path) {
            dir_children
                .entry(parent)
                .or_default()
                .push(info.child_entry.clone());
        }
    }

    // Process directories from deepest to shallowest.
    // After updating a directory, add it as a child of its parent.
    // We use a work queue: start with leaf directories, propagate up.
    let mut pending: Vec<(String, Vec<ChildEntry>)> = dir_children.into_iter().collect();

    // Sort by depth descending (deepest first).
    pending.sort_by(|a, b| {
        let depth_a = a.0.matches('/').count();
        let depth_b = b.0.matches('/').count();
        depth_b.cmp(&depth_a)
    });

    // Track directories we've already processed so we merge children
    // from multiple depths into a single update per directory.
    // Map: dir_path -> (content_key of the updated directory, serialized data length)
    let mut updated_dirs: HashMap<String, (Vec<u8>, u64)> = HashMap::new();

    // Also accumulate children for parent dirs that result from propagation.
    // We'll process level by level.
    let mut propagated: HashMap<String, Vec<ChildEntry>> = HashMap::new();

    for (dir_path, new_children) in &pending {
        // Merge with any propagated children from deeper directories
        let mut all_new_children = new_children.clone();
        if let Some(extra) = propagated.remove(dir_path) {
            all_new_children.extend(extra);
        }

        let (content_key, dir_data_len) =
            update_directory(engine, dir_path, all_new_children, hash_length, &algo)?;

        updated_dirs.insert(dir_path.clone(), (content_key.clone(), dir_data_len));

        // If not root, propagate this directory as a child of its parent
        if dir_path != "/" {
            let bc_now = chrono::Utc::now().timestamp_millis();
            let dir_child = ChildEntry {
                entry_type: EntryType::DirectoryIndex.to_u8(),
                hash: content_key.clone(),
                total_size: dir_data_len,
                created_at: bc_now,
                updated_at: bc_now,
                name: file_name(dir_path).unwrap_or("").to_string(),
                content_type: None,
                virtual_time: bc_now as u64,
                node_id: 0,
            };

            let grandparent = parent_path(dir_path).unwrap_or_else(|| "/".to_string());

            // Check if grandparent is already in our pending list
            if updated_dirs.contains_key(&grandparent) {
                // Already processed — re-update it
                let (new_content_key, new_len) = update_directory(
                    engine,
                    &grandparent,
                    vec![dir_child],
                    hash_length,
                    &algo,
                )?;
                updated_dirs.insert(grandparent.clone(), (new_content_key.clone(), new_len));

                // Continue propagating up from grandparent
                propagate_up(
                    engine,
                    &grandparent,
                    &new_content_key,
                    new_len,
                    hash_length,
                    &algo,
                    &mut updated_dirs,
                )?;
            } else {
                // Grandparent not yet processed — queue it
                propagated
                    .entry(grandparent)
                    .or_default()
                    .push(dir_child);
            }
        }
    }

    // Process any remaining propagated directories that weren't in the original set
    // (parent dirs that had no direct file children).
    // Sort deepest first again.
    let mut remaining: Vec<(String, Vec<ChildEntry>)> = propagated.into_iter().collect();
    remaining.sort_by(|a, b| {
        let depth_a = a.0.matches('/').count();
        let depth_b = b.0.matches('/').count();
        depth_b.cmp(&depth_a)
    });

    for (dir_path, children) in remaining {
        let (content_key, dir_data_len) =
            update_directory(engine, &dir_path, children, hash_length, &algo)?;
        updated_dirs.insert(dir_path.clone(), (content_key.clone(), dir_data_len));

        // Propagate up
        if dir_path != "/" {
            propagate_up(
                engine,
                &dir_path,
                &content_key,
                dir_data_len,
                hash_length,
                &algo,
                &mut updated_dirs,
            )?;
        }
    }

    // --- Phase 4: Update HEAD ---
    // The root "/" should have been updated. Use its content hash.
    if let Some((root_content_key, _)) = updated_dirs.get("/") {
        engine.update_head(root_content_key)?;
    }

    // --- Phase 5: Emit event ---
    let committed = file_infos.len();
    let result_files: Vec<CommittedFile> = file_infos
        .iter()
        .map(|info| CommittedFile {
            path: info.normalized_path.clone(),
            size: info.file_record.total_size,
        })
        .collect();

    ctx.emit(
        EVENT_ENTRIES_CREATED,
        serde_json::json!({ "entries": event_entries }),
    );

    Ok(CommitResult {
        committed,
        files: result_files,
    })
}

/// Update a single directory by merging new children into its existing entries.
/// Returns (content_hash, data_length) of the updated directory.
fn update_directory(
    engine: &StorageEngine,
    dir_path: &str,
    new_children: Vec<ChildEntry>,
    hash_length: usize,
    algo: &crate::engine::hash_algorithm::HashAlgorithm,
) -> EngineResult<(Vec<u8>, u64)> {
    let dir_key = directory_path_hash(dir_path, algo)?;

    // Follow hard links: dir_key may contain a 32-byte content hash pointer
    let existing = {
        let raw = engine.get_entry(&dir_key)?;
        match raw {
            Some((_header, _key, value)) if value.len() == hash_length => {
                // Hard link — follow to actual content
                engine.get_entry(&value)?
            }
            other => other,
        }
    };

    let (dir_value, content_key) = match existing {
        Some((_header, _key, value))
            if !value.is_empty() && btree::is_btree_format(&value) =>
        {
            // B-tree format: insert each new child into the tree
            let mut current_data = value;
            let mut current_hash = Vec::new();

            for child in new_children {
                let (new_hash, new_data) = btree::btree_insert_batched(
                    engine,
                    &current_data,
                    child,
                    hash_length,
                    algo,
                )?;
                current_hash = new_hash;
                current_data = new_data;
            }

            (current_data, current_hash)
        }
        Some((header, _key, value)) => {
            // Flat format
            let mut children = if value.is_empty() {
                Vec::new()
            } else {
                deserialize_child_entries(&value, hash_length, header.entry_version)?
            };

            // Merge new children: update existing by name or append
            for new_child in new_children {
                if let Some(existing) =
                    children.iter_mut().find(|c| c.name == new_child.name)
                {
                    *existing = new_child;
                } else {
                    children.push(new_child);
                }
            }

            // Check if we should convert to B-tree
            if children.len() >= btree::BTREE_CONVERSION_THRESHOLD {
                let root_hash =
                    btree::btree_from_entries(engine, children, hash_length, algo)?;
                let root_entry = engine.get_entry(&root_hash)?.ok_or_else(|| {
                    EngineError::NotFound(
                        "B-tree root not found after conversion".to_string(),
                    )
                })?;
                (root_entry.2, root_hash)
            } else {
                let dir_value = serialize_child_entries(&children, hash_length)?;
                let content_key = directory_content_hash(&dir_value, algo)?;
                engine.store_entry(
                    EntryType::DirectoryIndex,
                    &content_key,
                    &dir_value,
                )?;
                (dir_value, content_key)
            }
        }
        None => {
            // New directory
            let dir_value = serialize_child_entries(&new_children, hash_length)?;
            let content_key = directory_content_hash(&dir_value, algo)?;
            engine.store_entry(
                EntryType::DirectoryIndex,
                &content_key,
                &dir_value,
            )?;
            (dir_value, content_key)
        }
    };

    // Store at path-based key
    engine.store_entry(EntryType::DirectoryIndex, &dir_key, &dir_value)?;

    Ok((content_key, dir_value.len() as u64))
}

/// Propagate a directory update upward to root.
/// Called when we need to update ancestors that were already processed.
fn propagate_up(
    engine: &StorageEngine,
    dir_path: &str,
    content_key: &[u8],
    data_len: u64,
    hash_length: usize,
    algo: &crate::engine::hash_algorithm::HashAlgorithm,
    updated_dirs: &mut HashMap<String, (Vec<u8>, u64)>,
) -> EngineResult<()> {
    if dir_path == "/" {
        // Already at root, nothing to propagate
        return Ok(());
    }

    let grandparent = parent_path(dir_path).unwrap_or_else(|| "/".to_string());

    let prop_now = chrono::Utc::now().timestamp_millis();
    let dir_child = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key.to_vec(),
        total_size: data_len,
        created_at: prop_now,
        updated_at: prop_now,
        name: file_name(dir_path).unwrap_or("").to_string(),
        content_type: None,
        virtual_time: prop_now as u64,
        node_id: 0,
    };

    let (new_content_key, new_len) =
        update_directory(engine, &grandparent, vec![dir_child], hash_length, algo)?;

    updated_dirs.insert(grandparent.clone(), (new_content_key.clone(), new_len));

    if grandparent != "/" {
        propagate_up(
            engine,
            &grandparent,
            &new_content_key,
            new_len,
            hash_length,
            algo,
            updated_dirs,
        )?;
    }

    Ok(())
}
