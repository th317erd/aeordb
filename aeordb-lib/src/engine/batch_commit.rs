use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::engine::btree;
use crate::engine::content_type::detect_content_type;
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries, serialize_child_entries};
use crate::engine::directory_ops::{
  chunk_content_hash, directory_content_hash, directory_path_hash, is_system_path, publish_file_record_entries, whole_file_content_hash,
  DirectoryOps, FileRecordPublishInput, DEFAULT_CHUNK_SIZE,
};
use crate::engine::engine_event::{EntryEventData, EVENT_ENTRIES_CREATED};
use crate::engine::entry_header::FLAG_SYSTEM;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::indexing_pipeline::IndexingPipeline;
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// A file to commit as part of a batch, with pre-uploaded chunk hashes.
#[derive(Debug, Clone, Deserialize)]
pub struct CommitFile {
  pub path: String,
  /// Hex-encoded chunk hashes (matching hashes already in the KV store).
  #[serde(alias = "chunk_hashes")]
  pub chunks: Vec<String>,
  #[serde(default)]
  pub content_type: Option<String>,
  /// Optional caller-asserted raw whole-file hash (`BLAKE3(file bytes)`).
  ///
  /// When present with `size` and all referenced chunks are stored raw, commit
  /// can avoid a full chunk body read pass. If it must read chunk bodies
  /// anyway, the supplied hash is verified against the computed value.
  #[serde(default)]
  pub content_hash: Option<String>,
  /// Optional caller-asserted total file size in bytes.
  #[serde(default)]
  pub size: Option<u64>,
}

/// A small, fully-buffered file to commit through the embedded SDK batch path.
///
/// This intentionally accepts raw bytes, not HTTP pre-uploaded chunk hashes.
/// It is meant for trusted in-process callers that already hold file contents
/// in memory, such as sync dirty-bucket flushes and small JSON/config writes.
#[derive(Debug, Clone)]
pub struct BufferedFile {
  pub path: String,
  pub data: Vec<u8>,
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

struct BatchFileInfo {
  normalized_path: String,
  file_record: FileRecord,
  child_entry: ChildEntry,
}

struct PreparedCommitFile {
  chunks: Vec<(Vec<u8>, u64)>,
  content_hash: Vec<u8>,
  fast_path_status: &'static str,
  chunk_metadata_lookup_us: u128,
  chunk_body_read_us: u128,
  chunk_body_read_bytes: u64,
}

#[derive(Default)]
struct FinishBatchCommitTimings {
  directories_updated: usize,
  directory_update_ms: u128,
  head_update_ms: u128,
  event_emit_ms: u128,
  metadata_index_ms: u128,
  metadata_indexed_files: usize,
}

/// Atomically commit multiple files from pre-uploaded chunks.
///
/// 1. Validates all chunk hashes exist in the KV store
/// 2. Creates FileRecords from chunk hash lists (preserving created_at on overwrite)
/// 3. Updates directories in a single pass (each directory updated once)
/// 4. Updates HEAD once
/// 5. Emits a single `entries_created` event
pub fn commit_files(engine: &StorageEngine, ctx: &RequestContext, files: Vec<CommitFile>) -> EngineResult<CommitResult> {
  let total_start = std::time::Instant::now();
  if files.is_empty() {
    return Err(EngineError::InvalidInput("No files provided for commit".to_string()));
  }

  let file_count = files.len();
  let total_logical_file_bytes: u64 = files.iter().filter_map(|file| file.size).sum();
  let supplied_content_hash_files = files.iter().filter(|file| file.content_hash.is_some()).count();
  let supplied_size_files = files.iter().filter(|file| file.size.is_some()).count();

  // Reject any path under /.aeordb-system/ or /.aeordb-config/. System data
  // is written exclusively through dedicated APIs (system_store, directory_ops
  // with FLAG_SYSTEM) — never through user-facing batch commit. Without this
  // check, an authenticated user could overwrite /.aeordb-system/api-keys/<uuid>
  // and mint themselves a root key.
  for file in &files {
    let normalized = normalize_path(&file.path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }
    if is_system_path(&normalized) {
      return Err(EngineError::InvalidInput(format!(
        "Path '{}' is reserved for internal system data and cannot be written through this endpoint",
        file.path
      )));
    }
  }

  let algo = engine.hash_algo();

  // --- Phase 1: Validate all chunk hashes exist ---
  let validation_start = std::time::Instant::now();
  let mut missing_chunks: Vec<String> = Vec::new();
  // Decode all hex chunk hashes upfront and validate existence.
  // file_chunks[i] = Vec of (raw_hash_bytes, chunk_byte_size) for files[i].
  let mut file_chunks: Vec<Vec<(Vec<u8>, u64)>> = Vec::with_capacity(files.len());
  let mut file_content_hashes: Vec<Vec<u8>> = Vec::with_capacity(files.len());
  let mut asserted_hash_fast_path_files = 0usize;
  let mut fast_path_missing_content_hash_files = 0usize;
  let mut fast_path_missing_size_files = 0usize;
  let mut fast_path_metadata_incomplete_files = 0usize;
  let mut chunk_metadata_lookup_us = 0u128;
  let mut chunk_body_read_us = 0u128;
  let mut chunk_body_read_bytes = 0u64;
  let mut total_chunk_refs = 0usize;

  for file in &files {
    total_chunk_refs += file.chunks.len();
    match prepare_commit_file(engine, file, algo.hash_length())? {
      Ok(prepared) => {
        match prepared.fast_path_status {
          "used" => asserted_hash_fast_path_files += 1,
          "missing_content_hash" => fast_path_missing_content_hash_files += 1,
          "missing_size" => fast_path_missing_size_files += 1,
          "chunk_metadata_incomplete" => fast_path_metadata_incomplete_files += 1,
          _ => {}
        }
        chunk_metadata_lookup_us += prepared.chunk_metadata_lookup_us;
        chunk_body_read_us += prepared.chunk_body_read_us;
        chunk_body_read_bytes = chunk_body_read_bytes.saturating_add(prepared.chunk_body_read_bytes);
        file_content_hashes.push(prepared.content_hash);
        file_chunks.push(prepared.chunks);
      }
      Err(missing) => {
        missing_chunks.extend(missing);
      }
    }
  }

  if !missing_chunks.is_empty() {
    return Err(EngineError::InvalidInput(format!("Missing {} chunk(s): {}", missing_chunks.len(), missing_chunks.join(", "))));
  }

  let validation_elapsed = validation_start.elapsed();

  // Serialize the publish phase so mutable path keys, directory entries, and
  // HEAD are advanced as one namespace operation relative to other writers.
  let namespace_wait_start = std::time::Instant::now();
  let _namespace = engine.namespace_write_guard()?;
  let namespace_wait_ms = namespace_wait_start.elapsed().as_millis();
  let txn = crate::engine::storage_engine::TransactionGuard::new(engine);

  // --- Phase 2: Create FileRecords ---
  let publish_start = std::time::Instant::now();
  let mut file_infos: Vec<BatchFileInfo> = Vec::with_capacity(files.len());
  let mut event_entries: Vec<EntryEventData> = Vec::with_capacity(files.len());
  let mut first_chunk_sniff_reads = 0usize;
  let mut first_chunk_sniff_bytes = 0u64;
  let mut first_chunk_sniff_us = 0u128;

  for (i, file) in files.iter().enumerate() {
    let normalized = normalize_path(&file.path);
    let chunk_hashes: Vec<Vec<u8>> = file_chunks[i].iter().map(|(h, _)| h.clone()).collect();

    // Compute total size from chunk data sizes
    let total_size: u64 = file_chunks[i].iter().map(|(_, sz)| *sz).sum();

    // Match DirectoryOps' MIME contract: trust specific caller-provided
    // types, but treat empty/octet-stream as unknown and sniff bytes.
    let first_chunk_bytes = if content_type_needs_sniffing(file.content_type.as_deref()) {
      if let Some(first_hash) = chunk_hashes.first() {
        let sniff_start = std::time::Instant::now();
        let bytes = read_chunk_data(engine, first_hash)?.unwrap_or_default();
        first_chunk_sniff_us += sniff_start.elapsed().as_micros();
        first_chunk_sniff_reads += 1;
        first_chunk_sniff_bytes = first_chunk_sniff_bytes.saturating_add(bytes.len() as u64);
        bytes
      } else {
        Vec::new()
      }
    } else {
      Vec::new()
    };
    let detected_content_type = detect_content_type(&first_chunk_bytes, file.content_type.as_deref());

    let published = publish_file_record_entries(
      engine,
      FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        chunk_hashes,
        content_hash: file_content_hashes[i].clone(),
        flags: 0,
        created_at_override: None,
        prefer_existing_created_at: true,
      },
    )?;

    event_entries.push(published.event_entry.clone());
    engine.counters().record_file_write(published.existing_total_size, total_size, 0);
    file_infos.push(BatchFileInfo {
      normalized_path: published.normalized_path,
      file_record: published.file_record,
      child_entry: published.child_entry,
    });
  }
  let publish_file_records_ms = publish_start.elapsed().as_millis();

  let (result, finish_timings) = finish_batch_commit(engine, ctx, file_infos, event_entries)?;
  let transaction_commit_start = std::time::Instant::now();
  drop(txn);
  let transaction_commit_ms = transaction_commit_start.elapsed().as_millis();

  tracing::info!(
    files = file_count,
    total_chunk_refs,
    total_logical_file_bytes,
    supplied_content_hash_files,
    supplied_size_files,
    asserted_hash_fast_path_files,
    fast_path_missing_content_hash_files,
    fast_path_missing_size_files,
    fast_path_metadata_incomplete_files,
    chunk_metadata_lookup_us,
    chunk_body_read_us,
    chunk_body_read_bytes,
    chunk_validation_ms = validation_elapsed.as_millis(),
    namespace_wait_ms,
    publish_file_records_ms,
    first_chunk_sniff_reads,
    first_chunk_sniff_bytes,
    first_chunk_sniff_us,
    directories_updated = finish_timings.directories_updated,
    directory_update_ms = finish_timings.directory_update_ms,
    head_update_ms = finish_timings.head_update_ms,
    metadata_indexed_files = finish_timings.metadata_indexed_files,
    metadata_index_ms = finish_timings.metadata_index_ms,
    event_emit_ms = finish_timings.event_emit_ms,
    transaction_commit_ms,
    total_ms = total_start.elapsed().as_millis(),
    "blob commit completed"
  );

  Ok(result)
}

/// Atomically commit multiple small files from raw in-memory buffers.
///
/// This is the embedded-library companion to [`commit_files`]. It avoids the
/// HTTP chunk pre-upload contract, validates all paths before writing any
/// entries, supports trusted/system paths the same way `DirectoryOps` does,
/// and performs directory propagation in one batch.
pub fn commit_buffered_files(engine: &StorageEngine, ctx: &RequestContext, files: Vec<BufferedFile>) -> EngineResult<CommitResult> {
  if files.is_empty() {
    return Err(EngineError::InvalidInput("No files provided for buffered batch commit".to_string()));
  }

  let mut seen_paths = HashSet::with_capacity(files.len());
  let mut normalized_paths = Vec::with_capacity(files.len());
  for file in &files {
    let normalized = normalize_path(&file.path);
    if normalized == "/" {
      return Err(EngineError::InvalidInput("Cannot store at root path".to_string()));
    }
    if !seen_paths.insert(normalized.clone()) {
      return Err(EngineError::InvalidInput(format!("Duplicate batch path: {}", normalized)));
    }
    normalized_paths.push(normalized);
  }

  let _namespace = engine.namespace_write_guard()?;
  let _txn = crate::engine::storage_engine::TransactionGuard::new(engine);

  let algo = engine.hash_algo();
  let mut file_infos: Vec<BatchFileInfo> = Vec::with_capacity(files.len());
  let mut event_entries: Vec<EntryEventData> = Vec::with_capacity(files.len());

  for (file, normalized) in files.iter().zip(normalized_paths.into_iter()) {
    let sys_flags = if is_system_path(&normalized) { FLAG_SYSTEM } else { 0 };
    let detected_content_type = detect_content_type(&file.data, file.content_type.as_deref());
    let total_size = file.data.len() as u64;
    let mut chunk_hashes = Vec::new();

    let mut offset = 0usize;
    while offset < file.data.len() {
      let end = (offset + DEFAULT_CHUNK_SIZE).min(file.data.len());
      let chunk_data = &file.data[offset..end];
      let chunk_key = store_buffered_chunk(engine, chunk_data, sys_flags)?;
      chunk_hashes.push(chunk_key);
      offset = end;
    }

    let published = publish_file_record_entries(
      engine,
      FileRecordPublishInput {
        normalized_path: normalized,
        content_type: Some(detected_content_type),
        total_size,
        chunk_hashes,
        content_hash: whole_file_content_hash(&file.data, &algo)?,
        flags: sys_flags,
        created_at_override: None,
        prefer_existing_created_at: true,
      },
    )?;

    event_entries.push(published.event_entry.clone());
    engine.counters().record_file_write(published.existing_total_size, total_size, total_size);
    file_infos.push(BatchFileInfo {
      normalized_path: published.normalized_path,
      file_record: published.file_record,
      child_entry: published.child_entry,
    });
  }

  finish_batch_commit(engine, ctx, file_infos, event_entries).map(|(result, _timings)| result)
}

fn finish_batch_commit(
  engine: &StorageEngine,
  ctx: &RequestContext,
  file_infos: Vec<BatchFileInfo>,
  event_entries: Vec<EntryEventData>,
) -> EngineResult<(CommitResult, FinishBatchCommitTimings)> {
  let mut timings = FinishBatchCommitTimings::default();
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();

  // --- Phase 3: Single-pass directory propagation ---
  // Group files by their immediate parent directory.
  // Key = parent dir path, Value = Vec of ChildEntry for files in that dir.
  let mut dir_children: HashMap<String, Vec<ChildEntry>> = HashMap::new();

  for info in &file_infos {
    if let Some(parent) = parent_path(&info.normalized_path) {
      dir_children.entry(parent).or_default().push(info.child_entry.clone());
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

    let directory_update_start = std::time::Instant::now();
    let (content_key, dir_data_len) = update_directory(engine, dir_path, all_new_children, hash_length, &algo)?;
    timings.directory_update_ms += directory_update_start.elapsed().as_millis();
    timings.directories_updated += 1;

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
      if should_skip_root_propagation(dir_path, &grandparent) {
        continue;
      }

      // Check if grandparent is already in our pending list
      if updated_dirs.contains_key(&grandparent) {
        // Already processed — re-update it
        let directory_update_start = std::time::Instant::now();
        let (new_content_key, new_len) = update_directory(engine, &grandparent, vec![dir_child], hash_length, &algo)?;
        timings.directory_update_ms += directory_update_start.elapsed().as_millis();
        timings.directories_updated += 1;
        updated_dirs.insert(grandparent.clone(), (new_content_key.clone(), new_len));

        // Continue propagating up from grandparent
        propagate_up(engine, &grandparent, &new_content_key, new_len, hash_length, &algo, &mut updated_dirs, &mut timings)?;
      } else {
        // Grandparent not yet processed — queue it
        propagated.entry(grandparent).or_default().push(dir_child);
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
    let directory_update_start = std::time::Instant::now();
    let (content_key, dir_data_len) = update_directory(engine, &dir_path, children, hash_length, &algo)?;
    timings.directory_update_ms += directory_update_start.elapsed().as_millis();
    timings.directories_updated += 1;
    updated_dirs.insert(dir_path.clone(), (content_key.clone(), dir_data_len));

    // Propagate up
    if dir_path != "/" {
      propagate_up(engine, &dir_path, &content_key, dir_data_len, hash_length, &algo, &mut updated_dirs, &mut timings)?;
    }
  }

  // --- Phase 4: Update HEAD ---
  // The root "/" should have been updated. Use its content hash.
  if let Some((root_content_key, _)) = updated_dirs.get("/") {
    let head_update_start = std::time::Instant::now();
    engine.update_head(root_content_key)?;
    timings.head_update_ms += head_update_start.elapsed().as_millis();
  }

  // --- Phase 5: Emit event ---
  let committed = file_infos.len();
  let result_files: Vec<CommittedFile> =
    file_infos.iter().map(|info| CommittedFile { path: info.normalized_path.clone(), size: info.file_record.total_size }).collect();

  let event_emit_start = std::time::Instant::now();
  ctx.emit(EVENT_ENTRIES_CREATED, serde_json::json!({ "entries": event_entries }));
  timings.event_emit_ms = event_emit_start.elapsed().as_millis();

  let metadata_index_start = std::time::Instant::now();
  let pipeline = IndexingPipeline::new(engine);
  for info in &file_infos {
    if !is_system_path(&info.normalized_path) {
      timings.metadata_indexed_files += 1;
      if let Err(error) = pipeline.run_metadata_only(ctx, &info.normalized_path) {
        tracing::warn!("Metadata indexing failed for '{}': {}", info.normalized_path, error);
      }
    }
  }
  timings.metadata_index_ms = metadata_index_start.elapsed().as_millis();

  Ok((CommitResult { committed, files: result_files }, timings))
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
    let ops = DirectoryOps::new(engine);
    if let Some((header, value)) = ops.recover_directory_data_if_stale(dir_path, &dir_key)? {
      Some((header, dir_key.clone(), value))
    } else {
      let raw = engine.get_entry(&dir_key)?;
      match raw {
        Some((_header, _key, value)) if value.len() == hash_length => {
          // Hard link — follow to actual content
          engine.get_entry(&value)?
        }
        other => other,
      }
    }
  };

  let (dir_value, content_key) = match existing {
    Some((_header, _key, value)) if !value.is_empty() && btree::is_btree_format(&value) => {
      // B-tree format: insert each new child into the tree
      let mut current_data = value;
      let mut current_hash = Vec::new();

      for child in new_children {
        let (new_hash, new_data) = btree::btree_insert_batched(engine, &current_data, child, hash_length, algo)?;
        current_hash = new_hash;
        current_data = new_data;
      }

      (current_data, current_hash)
    }
    Some((header, _key, value)) => {
      // Flat format
      let mut children = if value.is_empty() { Vec::new() } else { deserialize_child_entries(&value, hash_length, header.entry_version)? };

      // Merge new children: update existing by name or append
      for new_child in new_children {
        if let Some(existing) = children.iter_mut().find(|c| c.name == new_child.name) {
          *existing = new_child;
        } else {
          children.push(new_child);
        }
      }

      // Check if we should convert to B-tree
      if children.len() >= btree::BTREE_CONVERSION_THRESHOLD {
        let root_hash = btree::btree_from_entries(engine, children, hash_length, algo)?;
        let root_entry =
          engine.get_entry(&root_hash)?.ok_or_else(|| EngineError::NotFound("B-tree root not found after conversion".to_string()))?;
        (root_entry.2, root_hash)
      } else {
        let dir_value = serialize_child_entries(&children, hash_length)?;
        let content_key = directory_content_hash(&dir_value, algo)?;
        engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
        (dir_value, content_key)
      }
    }
    None => {
      // New directory
      let dir_value = serialize_child_entries(&new_children, hash_length)?;
      let content_key = directory_content_hash(&dir_value, algo)?;
      engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
      (dir_value, content_key)
    }
  };

  // Store a path-key hard link to the content entry, matching
  // DirectoryOps::update_parent_directories. If a process dies after this
  // path key lands but before HEAD advances, list_directory can detect the
  // divergence and serve the canonical HEAD tree instead of stale directory
  // bytes.
  engine.store_entry(EntryType::DirectoryIndex, &dir_key, &content_key)?;

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
  timings: &mut FinishBatchCommitTimings,
) -> EngineResult<()> {
  if dir_path == "/" {
    // Already at root, nothing to propagate
    return Ok(());
  }

  let grandparent = parent_path(dir_path).unwrap_or_else(|| "/".to_string());
  if should_skip_root_propagation(dir_path, &grandparent) {
    return Ok(());
  }

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

  let directory_update_start = std::time::Instant::now();
  let (new_content_key, new_len) = update_directory(engine, &grandparent, vec![dir_child], hash_length, algo)?;
  timings.directory_update_ms += directory_update_start.elapsed().as_millis();
  timings.directories_updated += 1;

  updated_dirs.insert(grandparent.clone(), (new_content_key.clone(), new_len));

  if grandparent != "/" {
    propagate_up(engine, &grandparent, &new_content_key, new_len, hash_length, algo, updated_dirs, timings)?;
  }

  Ok(())
}

fn should_skip_root_propagation(dir_path: &str, grandparent: &str) -> bool {
  grandparent == "/" && is_system_path(dir_path)
}

fn content_type_needs_sniffing(content_type: Option<&str>) -> bool {
  match content_type {
    Some(content_type) => content_type.is_empty() || content_type == "application/octet-stream",
    None => true,
  }
}

fn prepare_commit_file(
  engine: &StorageEngine,
  file: &CommitFile,
  hash_length: usize,
) -> EngineResult<Result<PreparedCommitFile, Vec<String>>> {
  let decoded_hashes = decode_commit_chunk_hashes(file)?;
  let supplied_content_hash = decode_commit_content_hash(file.content_hash.as_deref(), hash_length)?;
  let mut chunk_metadata_lookup_us = 0u128;
  let mut chunk_body_read_us = 0u128;
  let mut chunk_body_read_bytes = 0u64;

  if let Some(asserted_hash) = supplied_content_hash.as_ref().filter(|_| file.size.is_some()) {
    let mut chunks_for_file: Vec<(Vec<u8>, u64)> = Vec::with_capacity(decoded_hashes.len());
    let mut missing_chunks = Vec::new();
    let mut total_size = 0u64;
    let mut metadata_complete = true;

    for (hex_hash, raw_hash) in &decoded_hashes {
      let metadata_start = std::time::Instant::now();
      match engine.get_chunk_metadata(raw_hash)? {
        Some(metadata) => match metadata.raw_value_length {
          Some(raw_len) => {
            chunk_metadata_lookup_us += metadata_start.elapsed().as_micros();
            total_size = total_size
              .checked_add(raw_len)
              .ok_or_else(|| EngineError::InvalidInput(format!("Commit size overflow while preparing '{}'", file.path)))?;
            chunks_for_file.push((raw_hash.clone(), raw_len));
          }
          None => {
            chunk_metadata_lookup_us += metadata_start.elapsed().as_micros();
            metadata_complete = false;
            break;
          }
        },
        None => {
          chunk_metadata_lookup_us += metadata_start.elapsed().as_micros();
          missing_chunks.push(hex_hash.clone());
        }
      }
    }

    if !missing_chunks.is_empty() {
      return Ok(Err(missing_chunks));
    }

    if metadata_complete {
      validate_commit_size(file, total_size)?;
      return Ok(Ok(PreparedCommitFile {
        chunks: chunks_for_file,
        content_hash: asserted_hash.clone(),
        fast_path_status: "used",
        chunk_metadata_lookup_us,
        chunk_body_read_us,
        chunk_body_read_bytes,
      }));
    }
  }

  let fast_path_status = if supplied_content_hash.is_none() {
    "missing_content_hash"
  } else if file.size.is_none() {
    "missing_size"
  } else {
    "chunk_metadata_incomplete"
  };

  let algo = engine.hash_algo();
  let mut chunks_for_file: Vec<(Vec<u8>, u64)> = Vec::with_capacity(decoded_hashes.len());
  let mut missing_chunks = Vec::new();
  let mut content_hasher = algo.incremental_hasher()?;
  let mut total_size = 0u64;

  for (hex_hash, raw_hash) in decoded_hashes {
    let read_start = std::time::Instant::now();
    match read_chunk_data(engine, &raw_hash)? {
      Some(value) => {
        chunk_body_read_us += read_start.elapsed().as_micros();
        chunk_body_read_bytes = chunk_body_read_bytes.saturating_add(value.len() as u64);
        content_hasher.update(&value);
        let chunk_len = value.len() as u64;
        total_size = total_size
          .checked_add(chunk_len)
          .ok_or_else(|| EngineError::InvalidInput(format!("Commit size overflow while preparing '{}'", file.path)))?;
        chunks_for_file.push((raw_hash, chunk_len));
      }
      None => {
        chunk_body_read_us += read_start.elapsed().as_micros();
        missing_chunks.push(hex_hash);
      }
    }
  }

  if !missing_chunks.is_empty() {
    return Ok(Err(missing_chunks));
  }

  validate_commit_size(file, total_size)?;
  let computed_hash = content_hasher.finalize();
  if let Some(asserted_hash) = supplied_content_hash {
    if asserted_hash != computed_hash {
      return Err(EngineError::InvalidInput(format!(
        "Content hash mismatch for '{}': expected {}, computed {}",
        file.path,
        hex::encode(asserted_hash),
        hex::encode(&computed_hash),
      )));
    }
  }

  Ok(Ok(PreparedCommitFile {
    chunks: chunks_for_file,
    content_hash: computed_hash,
    fast_path_status,
    chunk_metadata_lookup_us,
    chunk_body_read_us,
    chunk_body_read_bytes,
  }))
}

fn decode_commit_chunk_hashes(file: &CommitFile) -> EngineResult<Vec<(String, Vec<u8>)>> {
  let mut decoded = Vec::with_capacity(file.chunks.len());
  for hex_hash in &file.chunks {
    let raw_hash = hex::decode(hex_hash)
      .map_err(|error| EngineError::InvalidInput(format!("Invalid hex chunk hash '{}' for '{}': {}", hex_hash, file.path, error)))?;
    decoded.push((hex_hash.clone(), raw_hash));
  }
  Ok(decoded)
}

fn decode_commit_content_hash(content_hash: Option<&str>, hash_length: usize) -> EngineResult<Option<Vec<u8>>> {
  let Some(content_hash) = content_hash else {
    return Ok(None);
  };

  let decoded =
    hex::decode(content_hash).map_err(|error| EngineError::InvalidInput(format!("Invalid content_hash '{}': {}", content_hash, error)))?;
  if decoded.len() != hash_length {
    return Err(EngineError::InvalidInput(format!("Invalid content_hash length {} bytes; expected {} bytes", decoded.len(), hash_length,)));
  }
  Ok(Some(decoded))
}

fn validate_commit_size(file: &CommitFile, actual_size: u64) -> EngineResult<()> {
  if let Some(expected_size) = file.size {
    if expected_size != actual_size {
      return Err(EngineError::InvalidInput(format!(
        "Size mismatch for '{}': expected {}, computed {}",
        file.path, expected_size, actual_size,
      )));
    }
  }
  Ok(())
}

fn read_chunk_data(engine: &StorageEngine, hash: &[u8]) -> EngineResult<Option<Vec<u8>>> {
  engine.read_chunk(hash)
}

fn store_buffered_chunk(engine: &StorageEngine, data: &[u8], flags: u8) -> EngineResult<Vec<u8>> {
  let algo = engine.hash_algo();
  let chunk_key = chunk_content_hash(data, &algo)?;

  if engine.has_entry(&chunk_key)? {
    engine.counters().record_chunk_deduped();
    return Ok(chunk_key);
  }

  if flags != 0 {
    engine.store_entry_with_flags(EntryType::Chunk, &chunk_key, data, flags)?;
  } else {
    engine.store_entry(EntryType::Chunk, &chunk_key, data)?;
  }
  engine.counters().record_chunk_stored(data.len() as u64);

  Ok(chunk_key)
}
