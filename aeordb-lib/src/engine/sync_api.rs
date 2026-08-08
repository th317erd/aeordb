//! Library-level sync API — pure synchronous functions mirroring the HTTP sync protocol.
//!
//! These functions expose the same functionality as the HTTP sync endpoints in
//! `sync_routes.rs`, but as direct library calls with typed structs instead of JSON.
//! This allows embedded clients to replicate without HTTP overhead.

use crate::engine::conflict_store;
use crate::engine::directory_ops::{chunk_content_hash, read_chunk_reserved, validate_existing_chunk_locator};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::tree_walker::{diff_trees_with_budget, walk_version_tree_for_transfer_with_budget, TreeDiff, VersionTree};
use crate::engine::v4::system_family::SystemFamilyTransferOperationV1;
use crate::engine::version_manager::VersionManager;
use tokio_util::sync::CancellationToken;

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
  /// Whole-file content hash when the source FileRecord has been migrated to
  /// the current format. Legacy v0 records expose `None` and receivers may
  /// derive it from the validated chunk closure once.
  pub content_hash: Option<Vec<u8>>,
  pub size: u64,
  pub content_type: Option<String>,
  pub created_at: i64,
  pub updated_at: i64,
  pub chunk_hashes: Vec<Vec<u8>>,
}

/// A symlink entry in a sync diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncSymlinkEntry {
  pub path: String,
  pub hash: Vec<u8>,
  pub target: String,
  pub created_at: i64,
  pub updated_at: i64,
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

pub(crate) struct AccountedSyncDiff {
  diff: SyncDiff,
  memory: OperationMemoryBudget,
}

impl AccountedSyncDiff {
  pub(crate) fn into_parts(self) -> (SyncDiff, OperationMemoryBudget) {
    (self.diff, self.memory)
  }
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
/// `include_system=true` selects peer-replication policy;
/// `include_system=false` selects client-sync policy. Unknown protected state,
/// malformed required state, and missing historical roots fail the operation
/// rather than being converted into an empty or partial diff.
pub fn compute_sync_diff(
  engine: &StorageEngine,
  since_root_hash: Option<&[u8]>,
  paths_filter: Option<&[String]>,
  include_system: bool,
) -> EngineResult<SyncDiff> {
  let (diff, _memory) = compute_sync_diff_accounted(engine, since_root_hash, paths_filter, include_system)?.into_parts();
  Ok(diff)
}

pub(crate) fn compute_sync_diff_accounted(
  engine: &StorageEngine,
  since_root_hash: Option<&[u8]>,
  paths_filter: Option<&[String]>,
  include_system: bool,
) -> EngineResult<AccountedSyncDiff> {
  compute_sync_diff_accounted_with_cancellation(engine, since_root_hash, paths_filter, include_system, None)
}

pub(crate) fn compute_sync_diff_accounted_with_cancellation(
  engine: &StorageEngine,
  since_root_hash: Option<&[u8]>,
  paths_filter: Option<&[String]>,
  include_system: bool,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<AccountedSyncDiff> {
  let vm = VersionManager::new(engine);
  let head_hash = vm.get_head_hash()?;
  let operation =
    if include_system { SystemFamilyTransferOperationV1::PeerReplication } else { SystemFamilyTransferOperationV1::ClientSync };
  let mut memory = OperationMemoryBudget::new(engine, "sync diff", MemoryOwner::StreamingRead, AdmissionClass::Workload, 0, cancellation)?;
  let current_tree = walk_version_tree_for_transfer_with_budget(engine, &head_hash, operation, true, &mut memory)?;

  let mut diff_result = if let Some(since) = since_root_hash {
    // Detached system families are current-state authorities rather than part
    // of historical HEAD roots. Excluding them from the base makes current
    // portable peer state appear as idempotent additions.
    let base_tree = walk_version_tree_for_transfer_with_budget(engine, since, operation, false, &mut memory)?;
    let diff = diff_trees_with_budget(&base_tree, &current_tree, &mut memory)?;
    build_diff_from_tree_diff(diff, &mut memory)?
  } else {
    build_full_diff(current_tree, &mut memory)?
  };

  // Apply path filtering
  if let Some(paths) = paths_filter {
    filter_diff_by_paths(&mut diff_result, paths);
  }

  diff_result.root_hash = head_hash;
  refresh_needed_chunk_hashes(&mut diff_result, &mut memory)?;

  Ok(AccountedSyncDiff { diff: diff_result, memory })
}

// ---------------------------------------------------------------------------
// 2. get_needed_chunks
// ---------------------------------------------------------------------------

/// Retrieve chunks by their hashes from the local engine.
///
/// Returns only chunks that exist locally. Missing hashes are silently skipped.
/// Chunks are automatically decompressed if stored compressed.
pub fn get_needed_chunks(engine: &StorageEngine, chunk_hashes: &[Vec<u8>]) -> EngineResult<Vec<ChunkData>> {
  let mut result = Vec::new();

  for hash in chunk_hashes {
    if let Some(data) = engine.read_chunk(hash)? {
      engine.counters().record_read(data.len() as u64);
      result.push(ChunkData { hash: hash.clone(), data });
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
  let hash_length = engine.hash_algo().hash_length();
  let mut memory = OperationMemoryBudget::new(engine, "sync chunk staging", MemoryOwner::StreamingRead, AdmissionClass::Workload, 0, None)?;
  let mut unique = std::collections::BTreeMap::<Vec<u8>, usize>::new();
  for (index, chunk) in chunks.iter().enumerate() {
    memory.record_work(1)?;
    if chunk.hash.len() != hash_length {
      return Err(EngineError::InvalidInput(format!(
        "sync chunk hash length {} does not match expected length {hash_length}",
        chunk.hash.len()
      )));
    }
    let computed = chunk_content_hash(&chunk.data, &engine.hash_algo())?;
    if computed != chunk.hash {
      return Err(EngineError::InvalidInput(format!(
        "sync chunk payload hash {} does not match claimed hash {}",
        hex::encode(computed),
        hex::encode(&chunk.hash)
      )));
    }
    if let Some(previous_index) = unique.get(&chunk.hash) {
      if chunks[*previous_index].data != chunk.data {
        return Err(EngineError::InvalidInput(format!("sync chunk {} is repeated with different bytes", hex::encode(&chunk.hash))));
      }
    } else {
      let retained_bytes = chunk
        .hash
        .len()
        .checked_add(128)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or_else(|| EngineError::ResourceExhausted("sync chunk deduplication estimate overflow".to_string()))?;
      memory.reserve(retained_bytes, "sync chunk deduplication admission failed")?;
      unique.insert(chunk.hash.clone(), index);
    }
  }

  let mut batch = crate::engine::storage_engine::WriteBatch::new();
  let mut stored_sizes = Vec::new();
  let mut deduplicated = chunks.len().saturating_sub(unique.len());
  for (hash, index) in unique {
    let data = &chunks[index].data;
    if validate_existing_chunk_locator(engine, "sync chunk apply", &hash)? {
      let existing = read_chunk_reserved(engine, &hash, false)?;
      if chunk_content_hash(existing.as_ref(), &engine.hash_algo())? != hash {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("existing sync chunk {} does not match its content-addressed key", hex::encode(&hash)),
        });
      }
      deduplicated = deduplicated.saturating_add(1);
      continue;
    }
    let batch_bytes = data
      .len()
      .checked_add(hash.len())
      .and_then(|bytes| bytes.checked_add(128))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("sync chunk write-batch estimate overflow".to_string()))?;
    memory.reserve(batch_bytes, "sync chunk write-batch admission failed")?;
    stored_sizes.push(data.len() as u64);
    batch.add(EntryType::Chunk, hash, data.clone());
  }

  if !batch.is_empty() {
    engine.flush_batch(batch)?;
  }
  for size in &stored_sizes {
    engine.counters().record_chunk_stored(*size);
    engine.counters().record_write(*size);
  }
  for _ in 0..deduplicated {
    engine.counters().record_chunk_deduped();
  }
  Ok(stored_sizes.len())
}

// ---------------------------------------------------------------------------
// 4. list_conflicts_typed
// ---------------------------------------------------------------------------

/// List all unresolved conflicts with typed data.
///
/// Malformed authoritative conflict evidence fails the listing rather than
/// presenting an incomplete view to callers.
pub fn list_conflicts_typed(engine: &StorageEngine) -> EngineResult<Vec<ConflictRecord>> {
  let raw = conflict_store::list_conflicts(engine)?;
  raw
    .into_iter()
    .map(|value| serde_json::from_value::<ConflictRecord>(value).map_err(|error| EngineError::JsonParseError(error.to_string())))
    .collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a FileRecord into a SyncFileEntry without retaining a second copy.
fn file_record_into_entry(path: String, hash: Vec<u8>, record: FileRecord) -> SyncFileEntry {
  let FileRecord { content_type, total_size, created_at, updated_at, content_hash, chunk_hashes, .. } = record;
  SyncFileEntry {
    path,
    hash,
    content_hash: (!content_hash.is_empty()).then_some(content_hash),
    size: total_size,
    content_type,
    created_at,
    updated_at,
    chunk_hashes,
  }
}

/// Convert a SymlinkRecord into a SyncSymlinkEntry without retaining a second copy.
fn symlink_record_into_entry(path: String, hash: Vec<u8>, record: SymlinkRecord) -> SyncSymlinkEntry {
  SyncSymlinkEntry { path, hash, target: record.target, created_at: record.created_at, updated_at: record.updated_at }
}

/// Build a full sync diff (no base hash) — everything in the tree is "added".
fn build_full_diff(tree: VersionTree, memory: &mut OperationMemoryBudget) -> EngineResult<SyncDiff> {
  let vector_bytes = sync_diff_vector_bytes(tree.files.len(), 0, tree.symlinks.len())?;
  memory.reserve(vector_bytes, "full sync diff vector admission failed")?;
  let mut files_added = Vec::with_capacity(tree.files.len());
  let mut symlinks_added = Vec::with_capacity(tree.symlinks.len());

  for (path, (hash, record)) in tree.files {
    files_added.push(file_record_into_entry(path, hash, record));
  }

  for (path, (hash, record)) in tree.symlinks {
    symlinks_added.push(symlink_record_into_entry(path, hash, record));
  }

  // Sort for deterministic output
  files_added.sort_by(|a, b| a.path.cmp(&b.path));
  symlinks_added.sort_by(|a, b| a.path.cmp(&b.path));

  Ok(SyncDiff {
    root_hash: Vec::new(), // filled in by caller
    files_added,
    files_modified: Vec::new(),
    files_deleted: Vec::new(),
    symlinks_added,
    symlinks_modified: Vec::new(),
    symlinks_deleted: Vec::new(),
    chunk_hashes_needed: Vec::new(), // filled in by caller
  })
}

/// Build a sync diff from a TreeDiff (incremental sync).
fn build_diff_from_tree_diff(diff: TreeDiff, memory: &mut OperationMemoryBudget) -> EngineResult<SyncDiff> {
  let file_count = diff.added.len().saturating_add(diff.modified.len()).saturating_add(diff.deleted.len());
  let symlink_count = diff.symlinks_added.len().saturating_add(diff.symlinks_modified.len()).saturating_add(diff.symlinks_deleted.len());
  memory
    .reserve(sync_diff_vector_bytes(file_count, diff.deleted.len(), symlink_count)?, "incremental sync diff vector admission failed")?;
  let mut files_added = Vec::with_capacity(diff.added.len());
  let mut files_modified = Vec::with_capacity(diff.modified.len());
  let mut files_deleted = Vec::with_capacity(diff.deleted.len());
  let mut symlinks_added = Vec::with_capacity(diff.symlinks_added.len());
  let mut symlinks_modified = Vec::with_capacity(diff.symlinks_modified.len());
  let mut symlinks_deleted = Vec::with_capacity(diff.symlinks_deleted.len());

  for (path, (hash, record)) in diff.added {
    files_added.push(file_record_into_entry(path, hash, record));
  }

  for (path, (hash, record)) in diff.modified {
    files_modified.push(file_record_into_entry(path, hash, record));
  }

  for path in diff.deleted {
    files_deleted.push(SyncDeletedEntry { path });
  }

  for (path, (hash, record)) in diff.symlinks_added {
    symlinks_added.push(symlink_record_into_entry(path, hash, record));
  }

  for (path, (hash, record)) in diff.symlinks_modified {
    symlinks_modified.push(symlink_record_into_entry(path, hash, record));
  }

  for path in diff.symlinks_deleted {
    symlinks_deleted.push(SyncDeletedEntry { path });
  }

  // Sort for deterministic output
  files_added.sort_by(|a, b| a.path.cmp(&b.path));
  files_modified.sort_by(|a, b| a.path.cmp(&b.path));
  files_deleted.sort_by(|a, b| a.path.cmp(&b.path));
  symlinks_added.sort_by(|a, b| a.path.cmp(&b.path));
  symlinks_modified.sort_by(|a, b| a.path.cmp(&b.path));
  symlinks_deleted.sort_by(|a, b| a.path.cmp(&b.path));

  Ok(SyncDiff {
    root_hash: Vec::new(),
    files_added,
    files_modified,
    files_deleted,
    symlinks_added,
    symlinks_modified,
    symlinks_deleted,
    chunk_hashes_needed: Vec::new(),
  })
}

/// Filter diff entries to only include those matching at least one glob pattern.
fn filter_diff_by_paths(diff: &mut SyncDiff, patterns: &[String]) {
  if patterns.is_empty() {
    return;
  }

  let matches = |path: &str| -> bool { patterns.iter().any(|pattern| glob_match::glob_match(pattern, path)) };

  diff.files_added.retain(|e| matches(&e.path));
  diff.files_modified.retain(|e| matches(&e.path));
  diff.files_deleted.retain(|e| matches(&e.path));
  diff.symlinks_added.retain(|e| matches(&e.path));
  diff.symlinks_modified.retain(|e| matches(&e.path));
  diff.symlinks_deleted.retain(|e| matches(&e.path));
}

fn refresh_needed_chunk_hashes(diff: &mut SyncDiff, memory: &mut OperationMemoryBudget) -> EngineResult<()> {
  let hashes_count = diff
    .files_added
    .iter()
    .chain(diff.files_modified.iter())
    .try_fold(0usize, |count, entry| count.checked_add(entry.chunk_hashes.len()))
    .ok_or_else(|| EngineError::ResourceExhausted("sync chunk inventory count overflow".to_string()))?;
  let hashes_bytes = diff
    .files_added
    .iter()
    .chain(diff.files_modified.iter())
    .flat_map(|entry| entry.chunk_hashes.iter())
    .try_fold(0usize, |bytes, hash| bytes.checked_add(hash.len()).and_then(|total| total.checked_add(std::mem::size_of::<Vec<u8>>())))
    .ok_or_else(|| EngineError::ResourceExhausted("sync chunk inventory estimate overflow".to_string()))?;
  memory.reserve(
    u64::try_from(hashes_bytes).map_err(|_| EngineError::ResourceExhausted("sync chunk inventory estimate overflow".to_string()))?,
    "sync chunk inventory admission failed",
  )?;
  let mut hashes = Vec::with_capacity(hashes_count);
  hashes.extend(diff.files_added.iter().chain(diff.files_modified.iter()).flat_map(|entry| entry.chunk_hashes.iter().cloned()));
  hashes.sort();
  hashes.dedup();
  diff.chunk_hashes_needed = hashes;
  Ok(())
}

fn sync_diff_vector_bytes(file_count: usize, deleted_file_count: usize, symlink_count: usize) -> EngineResult<u64> {
  let file_bytes = file_count
    .checked_sub(deleted_file_count)
    .and_then(|count| count.checked_mul(std::mem::size_of::<SyncFileEntry>()))
    .and_then(|bytes| {
      deleted_file_count.checked_mul(std::mem::size_of::<SyncDeletedEntry>()).and_then(|deleted| bytes.checked_add(deleted))
    })
    .ok_or_else(|| EngineError::ResourceExhausted("sync diff file vector estimate overflow".to_string()))?;
  let symlink_bytes = symlink_count
    .checked_mul(std::mem::size_of::<SyncSymlinkEntry>().max(std::mem::size_of::<SyncDeletedEntry>()))
    .ok_or_else(|| EngineError::ResourceExhausted("sync diff symlink vector estimate overflow".to_string()))?;
  file_bytes
    .checked_add(symlink_bytes)
    .and_then(|bytes| bytes.checked_add(512))
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| EngineError::ResourceExhausted("sync diff vector estimate overflow".to_string()))
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
pub fn file_history(engine: &StorageEngine, path: &str) -> EngineResult<Vec<FileHistoryEntry>> {
  use crate::engine::version_access::resolve_file_at_version;

  let vm = crate::engine::version_manager::VersionManager::new(engine);
  let mut snapshots = vm.list_snapshots()?;
  snapshots.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.name.cmp(&b.name)));

  let mut history: Vec<FileHistoryEntry> = Vec::new();
  let mut previous_found = false;
  let mut previous_hash: Vec<u8> = Vec::new();

  for snapshot in &snapshots {
    let (found, file_hash, size, content_type) = match resolve_file_at_version(engine, &snapshot.root_hash, path) {
      Ok((hash, record)) => (true, hash, record.total_size, record.content_type.clone()),
      Err(EngineError::NotFound(_)) => (false, Vec::new(), 0, None),
      Err(error) => return Err(error),
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
    return Err(crate::engine::errors::EngineError::InvalidInput("Must provide snapshot_name or version_hash".to_string()));
  };

  // Resolve the file at the version
  let (_, file_record) = crate::engine::version_access::resolve_file_at_version(engine, &root_hash, path)?;

  // Create auto-snapshot when snapshot writes are enabled. Preserve the
  // existing return type by returning an empty name when lifecycle policy has
  // disabled snapshot writes.
  let auto_snapshot_name = if crate::engine::lifecycle_config::snapshot_writes_enabled(engine) {
    let now = chrono::Utc::now();
    let base_name = now.format("pre-restore-%Y-%m-%dT%H-%M-%SZ").to_string();
    let mut name = base_name.clone();
    let mut attempt = 1;
    loop {
      let mut metadata = HashMap::new();
      metadata.insert("reason".to_string(), "auto-snapshot before file restore".to_string());
      metadata.insert("restored_path".to_string(), path.to_string());
      metadata.insert(
        crate::engine::lifecycle_config::SNAPSHOT_TYPE_KEY.to_string(),
        crate::engine::lifecycle_config::SNAPSHOT_TYPE_AUTO.to_string(),
      );
      match vm.create_snapshot(ctx, &name, metadata) {
        Ok(_) => break name,
        Err(_) if attempt < 10 => {
          attempt += 1;
          name = format!("{}-{}", base_name, attempt);
        }
        Err(error) => return Err(error),
      }
    }
  } else {
    tracing::info!(path, "Skipping pre-restore snapshot because snapshot writes are disabled");
    String::new()
  };

  // Read historical file content
  let content = read_file_at_version(engine, &root_hash, path)?;
  let size = content.len() as u64;

  // Write to HEAD
  let ops = crate::engine::directory_ops::DirectoryOps::new(engine);
  ops.store_file_buffered(ctx, path, &content, file_record.content_type.as_deref())?;

  Ok((auto_snapshot_name, size))
}
