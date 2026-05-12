use std::collections::HashSet;

use crate::engine::btree::{BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::engine_event::{EVENT_GC_COMPLETED, EVENT_GC_STARTED};
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::file_record::FileRecord;
use crate::engine::engine_counters::CountersSnapshot;
use crate::engine::kv_store::{
    KV_TYPE_DELETION, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY,
    KV_TYPE_CHUNK, KV_TYPE_SNAPSHOT, KV_TYPE_FORK, KV_TYPE_SYMLINK,
};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::{symlink_path_hash, symlink_content_hash};
use crate::engine::version_manager::VersionManager;

use serde::Serialize;

/// Result of a garbage collection run, returned by [`run_gc`].
#[derive(Debug, Clone, Serialize)]
pub struct GcResult {
  /// Number of version roots scanned (HEAD + snapshots + forks).
  pub versions_scanned: usize,
  /// Number of entries reachable from at least one version root.
  pub live_entries: usize,
  /// Number of unreachable entries identified as garbage.
  pub garbage_entries: usize,
  /// Total bytes freed (or that would be freed in a dry run).
  pub reclaimed_bytes: u64,
  /// Wall-clock time of the GC cycle in milliseconds.
  pub duration_ms: u64,
  /// True if this was a dry run (no entries were actually swept).
  pub dry_run: bool,
}

/// Collect all reachable hashes from HEAD + all snapshots + all forks.
pub fn gc_mark(engine: &StorageEngine) -> EngineResult<HashSet<Vec<u8>>> {
  let mut live: HashSet<Vec<u8>> = HashSet::new();
  let hash_length = engine.hash_algo().hash_length();

  // Walk HEAD
  let head_hash = engine.head_hash()?;
  if !head_hash.is_empty() && head_hash.iter().any(|&b| b != 0) {
    // HEAD points to a content-addressed directory hash for "/"
    // Mark both the content hash and the path-based hash
    walk_directory_tree(engine, &head_hash, "/", hash_length, &mut live)?;
  }

  // Walk every snapshot
  let vm = VersionManager::new(engine);
  let snapshots = vm.list_snapshots()?;
  for snapshot in &snapshots {
    walk_directory_tree(engine, &snapshot.root_hash, "/", hash_length, &mut live)?;
  }

  // Walk every fork
  let forks = vm.list_forks()?;
  for fork in &forks {
    walk_directory_tree(engine, &fork.root_hash, "/", hash_length, &mut live)?;
  }

  // Mark snapshot and fork KV key hashes as live
  for snapshot in &snapshots {
    let key = engine.compute_hash(format!("snap:{}", snapshot.name).as_bytes())?;
    live.insert(key);
  }
  for fork in &forks {
    let key = engine.compute_hash(format!("::aeordb:fork:{}", fork.name).as_bytes())?;
    live.insert(key);
  }

  // Mark system table entries as live
  mark_system_entries(engine, hash_length, &mut live)?;

  // Mark task queue entries as live -- task records use deterministic hashes
  // ("::aeordb:task:{id}") that are NOT in the directory tree, so
  // mark_system_entries does not cover them.
  mark_task_entries(engine, &mut live)?;

  // Mark DeletionRecord entries as live — they are needed for KV rebuild
  // from a full .aeordb scan (deletion replay) and must not be swept.
  let all_entries = engine.iter_kv_entries()?;
  for entry in &all_entries {
    if entry.entry_type() == KV_TYPE_DELETION {
      live.insert(entry.hash.clone());
    }
  }

  Ok(live)
}

/// Walk a directory tree from a root hash, knowing the directory path.
/// Marks both content-addressed and path-based hashes for directories.
fn walk_directory_tree(
  engine: &StorageEngine,
  root_hash: &[u8],
  dir_path: &str,
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  // Mark the content-addressed root hash
  if !live.insert(root_hash.to_vec()) {
    // Already visited this exact content hash — but we still need to mark
    // the path-based hash for this directory path if it differs.
    let path_hash = engine.compute_hash(format!("dir:{}", dir_path).as_bytes())?;
    live.insert(path_hash);
    return Ok(());
  }

  // Also mark the path-based directory hash
  let path_hash = engine.compute_hash(format!("dir:{}", dir_path).as_bytes())?;
  live.insert(path_hash);

  // Use get_entry_including_deleted: content-addressed entries may be marked
  // deleted at HEAD but still reachable from historical snapshot roots.
  let entry = match engine.get_entry_including_deleted(root_hash)? {
    Some(entry) => entry,
    None => return Ok(()),
  };

  let (header, _key, value) = entry;

  // Follow hard links: if value is exactly hash_length bytes, it's a content hash pointer
  let value = if value.len() == hash_length {
    live.insert(value.clone()); // Mark the content hash as live
    match engine.get_entry_including_deleted(&value)? {
      Some((_h, _k, v)) => v,
      None => return Ok(()),
    }
  } else {
    value
  };

  if header.entry_type == EntryType::DirectoryIndex {
    if value.is_empty() {
      return Ok(());
    }
    let children = if is_btree_format(&value) {
      collect_btree_children(engine, &value, hash_length, live)?
    } else {
      deserialize_child_entries(&value, hash_length, header.entry_version)?
    };

    for child in &children {
      let child_path = if dir_path == "/" {
        format!("/{}", child.name)
      } else {
        format!("{}/{}", dir_path, child.name)
      };

      let child_type = EntryType::from_u8(child.entry_type)?;
      match child_type {
        EntryType::DirectoryIndex => {
          // Recurse into subdirectory — child.hash is the content-addressed hash
          walk_directory_tree(engine, &child.hash, &child_path, hash_length, live)?;
        }
        EntryType::FileRecord => {
          mark_file_entry(engine, &child.hash, hash_length, live)?;
        }
        EntryType::Symlink => {
          mark_symlink_entry(engine, &child.hash, &child_path, live)?;
        }
        _ => {
          live.insert(child.hash.clone());
        }
      }
    }
  }

  Ok(())
}

/// Mark a file entry and its chunks as live.
fn mark_file_entry(
  engine: &StorageEngine,
  file_hash: &[u8],
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  if !live.insert(file_hash.to_vec()) {
    return Ok(());
  }

  // Use get_entry_including_deleted: file entries may be deleted at HEAD
  // but still referenced by historical snapshots.
  if let Some((header, _key, value)) = engine.get_entry_including_deleted(file_hash)? {
    let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
    for chunk_hash in &file_record.chunk_hashes {
      live.insert(chunk_hash.clone());
    }

    let algo = engine.hash_algo();

    // Also mark the path-based key as live (mutable index for reads/indexing)
    let path_key = crate::engine::directory_ops::file_path_hash(&file_record.path, &algo)?;
    live.insert(path_key);

    // Also mark the content-addressed key as live (immutable KV store entry)
    let content_key = crate::engine::directory_ops::file_content_hash(&value, &algo)?;
    live.insert(content_key);
  }

  Ok(())
}

/// Mark a symlink entry and its path-based key as live.
fn mark_symlink_entry(
  engine: &StorageEngine,
  symlink_hash: &[u8],
  symlink_path: &str,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  if !live.insert(symlink_hash.to_vec()) {
    return Ok(());
  }

  let algo = engine.hash_algo();

  // Also mark the path-based key as live (mutable index for reads)
  let path_key = symlink_path_hash(symlink_path, &algo)?;
  live.insert(path_key);

  // Also mark the content-addressed key as live (immutable KV store entry)
  // Use _including_deleted: symlink may be deleted at HEAD but snapshot-referenced.
  if let Some((_header, _key, value)) = engine.get_entry_including_deleted(symlink_hash)? {
    let content_key = symlink_content_hash(&value, &algo)?;
    live.insert(content_key);
  }

  Ok(())
}

/// Collect children from a B-tree node, marking all intermediate node hashes.
fn collect_btree_children(
  engine: &StorageEngine,
  node_data: &[u8],
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<Vec<ChildEntry>> {
  let node = BTreeNode::deserialize(node_data, hash_length)?;
  let mut all_children = Vec::new();

  match node {
    BTreeNode::Leaf(leaf) => {
      all_children.extend(leaf.entries);
    }
    BTreeNode::Internal(internal) => {
      for child_hash in &internal.children {
        live.insert(child_hash.clone());
        // B-tree internal nodes may be deleted at HEAD but snapshot-referenced.
        if let Some((_header, _key, child_data)) = engine.get_entry_including_deleted(child_hash)? {
          let sub_children = collect_btree_children(engine, &child_data, hash_length, live)?;
          all_children.extend(sub_children);
        }
      }
    }
  }

  Ok(all_children)
}

/// Mark system table entries as live.
fn mark_system_entries(
  engine: &StorageEngine,
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  let system_prefixes = ["/.aeordb-system", "/.aeordb-config"];

  for prefix in &system_prefixes {
    let dir_hash = engine.compute_hash(format!("dir:{}", prefix).as_bytes())?;
    // System dirs may be deleted at HEAD but snapshot-referenced.
    if let Some((header, _key, value)) = engine.get_entry_including_deleted(&dir_hash)? {
      live.insert(dir_hash);
      // Follow hard link if value is a content-hash pointer.
      let dir_value = if value.len() == hash_length {
        live.insert(value.clone());
        match engine.get_entry_including_deleted(&value)? {
          Some((_h, _k, v)) => v,
          None => continue,
        }
      } else {
        value
      };
      if !dir_value.is_empty() {
        if is_btree_format(&dir_value) {
          let children = collect_btree_children(engine, &dir_value, hash_length, live)?;
          for child in &children {
            mark_entry_recursive(engine, &child.hash, hash_length, live)?;
          }
        } else {
          let children = deserialize_child_entries(&dir_value, hash_length, header.entry_version)?;
          for child in &children {
            mark_entry_recursive(engine, &child.hash, hash_length, live)?;
          }
        }
      }
    }
  }

  Ok(())
}

/// Generic recursive mark for entries reachable from system tables.
fn mark_entry_recursive(
  engine: &StorageEngine,
  hash: &[u8],
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  if !live.insert(hash.to_vec()) {
    return Ok(());
  }

  // Use _including_deleted: system entries may reference content-addressed
  // entries that are deleted at HEAD but still needed.
  let entry = match engine.get_entry_including_deleted(hash)? {
    Some(entry) => entry,
    None => return Ok(()),
  };

  let (header, _key, value) = entry;
  match header.entry_type {
    EntryType::DirectoryIndex => {
      if !value.is_empty() {
        if is_btree_format(&value) {
          let children = collect_btree_children(engine, &value, hash_length, live)?;
          for child in &children {
            mark_entry_recursive(engine, &child.hash, hash_length, live)?;
          }
        } else {
          let children = deserialize_child_entries(&value, hash_length, header.entry_version)?;
          for child in &children {
            mark_entry_recursive(engine, &child.hash, hash_length, live)?;
          }
        }
      }
    }
    EntryType::FileRecord => {
      let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
      for chunk_hash in &file_record.chunk_hashes {
        live.insert(chunk_hash.clone());
      }
    }
    EntryType::Symlink => {
      // Symlinks are leaf entries — already marked by the insert above
    }
    _ => {}
  }

  Ok(())
}

/// Mark task queue entries (registry + individual task records) as live.
/// Task records use deterministic blake3 hashes on "::aeordb:task:{id}" keys
/// and are stored as EntryType::FileRecord, so they would be swept by GC
/// unless explicitly marked.
fn mark_task_entries(
  engine: &StorageEngine,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();
  live.insert(registry_key.clone());

  // Load the registry to find all task IDs
  if let Some((_header, _key, value)) = engine.get_entry(&registry_key)? {
    if let Ok(ids) = serde_json::from_slice::<Vec<String>>(&value) {
      for id in &ids {
        let task_key = blake3::hash(format!("::aeordb:task:{}", id).as_bytes()).as_bytes().to_vec();
        live.insert(task_key);
      }
    }
  }

  Ok(())
}

/// Minimum DeletionRecord entry size for the given engine's hash algorithm.
fn min_deletion_size(engine: &StorageEngine) -> u32 {
  // DeletionRecord with path="gc", reason="gc":
  // value = u16(2) + "gc"(2) + i64(8) + u16(2) + "gc"(2) = 16 bytes
  // key = hash_length bytes (computed hash)
  let hash_length = engine.hash_algo().hash_length();
  EntryHeader::compute_total_length(engine.hash_algo(), hash_length, 16)
    .expect("small fixed sizes cannot exceed length bounds")
}

/// Minimum void entry size.
fn min_void_size(engine: &StorageEngine) -> u32 {
  EntryHeader::compute_total_length(engine.hash_algo(), 0, 0)
    .expect("zero lengths cannot exceed bounds")
}

/// Sweep phase: iterate all KV entries, overwrite non-live entries in-place.
/// Uses nosync writes for batch performance — one sync at the end.
///
/// **Concurrency note**: GC should not be run concurrently with writes.
/// The HTTP endpoint runs GC in `spawn_blocking`, which does NOT prevent
/// concurrent writes from other requests. A concurrent write during the
/// sweep phase could create an entry that the mark phase missed, causing
/// it to be incorrectly swept. To mitigate this, each entry is re-verified
/// against the current KV state before being overwritten — if a concurrent
/// write has made an entry live since the mark phase, it is skipped.
/// For full safety, callers should ensure exclusive access during GC.
///
/// **Crash safety (M8)**: If the process crashes mid-sweep, the `.aeordb`
/// file may contain partially overwritten entries (some garbage entries
/// replaced with DeletionRecord/Void, others not yet swept), while the
/// `.kv` index still references the old offsets. On restart the `.kv` file
/// will be stale and must be deleted to trigger a full rebuild from the
/// `.aeordb` file scan. The rebuild replays deletion records and
/// reconstructs the index from the on-disk entry headers, so no committed
/// data is lost — only the sweep progress is discarded and garbage entries
/// that were not yet overwritten will persist until the next GC run.
pub fn gc_sweep(
  engine: &StorageEngine,
  live: &HashSet<Vec<u8>>,
  dry_run: bool,
) -> EngineResult<(usize, u64)> {
  let min_del = min_deletion_size(engine);
  let min_void = min_void_size(engine);

  let all_entries = engine.iter_kv_entries()?;

  // First pass: identify garbage entries and compute sizes.
  let mut garbage_candidates: Vec<(Vec<u8>, u64, u32)> = Vec::new(); // (hash, offset, entry_size)
  let mut garbage_count: usize = 0;
  let mut reclaimed_bytes: u64 = 0;

  for entry in &all_entries {
    if live.contains(&entry.hash) {
      continue;
    }
    // Spare entries that landed during mark/sweep — they're in the recheck
    // set the engine maintains while GC is active. Without this, concurrent
    // writes would be eligible for sweep just because they're not in `live`.
    if !dry_run && engine.gc_recheck_contains(&entry.hash) {
      continue;
    }

    let header = engine.read_entry_header_at(entry.offset)?;
    let entry_size = header.total_length;

    garbage_count += 1;
    reclaimed_bytes += entry_size as u64;

    if !dry_run {
      garbage_candidates.push((entry.hash.clone(), entry.offset, entry_size));
    }
  }

  // Free the full entry list before the sweep loop
  drop(all_entries);

  // Second pass (non-dry-run): re-verify each candidate against the current KV
  // state before overwriting. A concurrent write between mark and sweep could
  // have made an entry live (new offset for the same hash = re-created entry).
  // Uses per-entry get_kv_entry() lookups instead of loading all entries into
  // a HashMap to avoid doubling memory usage.
  let mut garbage_hashes: Vec<Vec<u8>> = Vec::new();

  if !dry_run && !garbage_candidates.is_empty() {
    for (hash, offset, entry_size) in &garbage_candidates {
      // Re-verify: if the entry no longer exists or now points to a different
      // offset, a concurrent write occurred — skip this entry.
      match engine.get_kv_entry(hash) {
        Some(fresh_entry) if fresh_entry.offset == *offset => {
          // Still garbage at the same offset — safe to sweep
        }
        _ => {
          // Entry was re-written or deleted since mark — skip
          garbage_count -= 1;
          reclaimed_bytes -= *entry_size as u64;
          continue;
        }
      }

      // Best-effort in-place overwrite (nosync — batch all writes)
      if *entry_size >= min_del {
        let written = engine.write_deletion_at_nosync(*offset, "gc")?;
        let remaining = *entry_size - written;
        if remaining >= min_void {
          let void_offset = *offset + written as u64;
          engine.write_void_at_nosync(void_offset, remaining)?;
        }
      }

      garbage_hashes.push(hash.clone());
    }
  }

  if !dry_run && !garbage_hashes.is_empty() {
    // One sync for all in-place overwrites
    engine.sync_writer()?;

    // Batch remove from KV
    engine.remove_kv_entries_batch(&garbage_hashes)?;
  }

  Ok((garbage_count, reclaimed_bytes))
}

/// Run a complete garbage collection cycle (mark + sweep).
///
/// The **mark** phase walks all version roots (HEAD, snapshots, forks)
/// and collects the set of reachable entry hashes. The **sweep** phase
/// overwrites unreachable entries in-place with deletion records and voids.
///
/// Pass `dry_run = true` to compute what would be collected without
/// modifying the database.
///
/// GC should not be run concurrently with writes -- see [`gc_sweep`] for details.
pub fn run_gc(
  engine: &StorageEngine,
  ctx: &RequestContext,
  dry_run: bool,
) -> EngineResult<GcResult> {
  let start = std::time::Instant::now();

  // Emit GC started event
  ctx.emit(EVENT_GC_STARTED, serde_json::json!({
    "dry_run": dry_run,
  }));

  // Begin GC recheck tracking before any version-forest reads. From this
  // point on, every successful write hash is recorded so the sweep phase can
  // spare entries that arrived after the mark snapshot was captured. See
  // bot-docs/plan/gc-mark-sweep.md. The RAII guard ensures we always call
  // end_gc_recheck on exit, even on `?`-propagated errors.
  struct RecheckGuard<'a>(&'a StorageEngine, bool);
  impl<'a> Drop for RecheckGuard<'a> {
    fn drop(&mut self) { if self.1 { self.0.end_gc_recheck(); } }
  }
  if !dry_run {
    engine.begin_gc_recheck();
  }
  let _recheck_guard = RecheckGuard(engine, !dry_run);

  let vm = VersionManager::new(engine);

  // Auto-snapshot before GC — safety net in case sweep removes something needed
  if !dry_run {
    let snapshot_name = format!("_aeordb_pre_gc_{}", chrono::Utc::now().timestamp());

    match vm.create_snapshot(ctx, &snapshot_name, std::collections::HashMap::new()) {
      Ok(_) => {
        tracing::info!("Created pre-GC snapshot: {}", snapshot_name);
      }
      Err(e) => {
        tracing::warn!("Failed to create pre-GC snapshot: {}. Proceeding with GC anyway.", e);
      }
    }

    // Clean up old pre-GC snapshots — keep last 3
    if let Ok(snapshots) = vm.list_snapshots() {
      let mut pre_gc_snapshots: Vec<String> = snapshots
        .iter()
        .filter(|s| s.name.starts_with("_aeordb_pre_gc_"))
        .map(|s| s.name.clone())
        .collect();
      pre_gc_snapshots.sort();
      pre_gc_snapshots.reverse(); // newest first (timestamp suffix sorts lexicographically)

      for old_name in pre_gc_snapshots.iter().skip(3) {
        if let Err(e) = vm.delete_snapshot(ctx, old_name) {
          tracing::warn!("Failed to delete old pre-GC snapshot {}: {}", old_name, e);
        }
      }
    }
  }

  let snapshot_count = vm.list_snapshots()?.len();
  let fork_count = vm.list_forks()?.len();
  let versions_scanned = 1 + snapshot_count + fork_count;

  let mut live = gc_mark(engine)?;

  // Re-check drain: any entry that was written during the mark phase is now in
  // the recheck set. Walk each one and union into `live` so the sweep doesn't
  // clobber freshly-written data. Loop until the queue is empty for one pass.
  if !dry_run {
    loop {
      let pending = engine.take_gc_recheck();
      if pending.is_empty() {
        break;
      }
      let hash_length = engine.hash_algo().hash_length();
      for hash in pending {
        mark_entry_recursive(engine, &hash, hash_length, &mut live)?;
      }
    }
  }

  let live_entries = live.len();

  // The RAII guard above calls end_gc_recheck on scope exit so failure paths
  // don't leave recheck recording on indefinitely.
  let (garbage_entries, reclaimed_bytes) = gc_sweep(engine, &live, dry_run)?;

  // Reconcile counters from authoritative KV state after sweep
  if !dry_run {
    let authoritative = build_authoritative_snapshot(engine)?;
    engine.counters().reconcile(&authoritative);
  }

  let duration_ms = start.elapsed().as_millis() as u64;

  let result = GcResult {
    versions_scanned,
    live_entries,
    garbage_entries,
    reclaimed_bytes,
    duration_ms,
    dry_run,
  };

  // Emit GC event
  ctx.emit(EVENT_GC_COMPLETED, serde_json::json!({
    "versions_scanned": result.versions_scanned,
    "live_entries": result.live_entries,
    "garbage_entries": result.garbage_entries,
    "reclaimed_bytes": result.reclaimed_bytes,
    "duration_ms": result.duration_ms,
    "dry_run": result.dry_run,
  }));

  Ok(result)
}

/// Build an authoritative CountersSnapshot by scanning the current KV state.
/// Used by GC to reconcile counters after sweep.
fn build_authoritative_snapshot(engine: &StorageEngine) -> EngineResult<CountersSnapshot> {
  let all_entries = engine.iter_kv_entries()?;
  let hash_length = engine.hash_algo().hash_length();

  let mut files: u64 = 0;
  let mut directories: u64 = 0;
  let mut symlinks: u64 = 0;
  let mut chunks: u64 = 0;
  let mut snapshots: u64 = 0;
  let mut forks: u64 = 0;
  let mut logical_data_size: u64 = 0;
  let mut chunk_data_size: u64 = 0;

  for entry in &all_entries {
    match entry.entry_type() {
      KV_TYPE_FILE_RECORD => {
        files += 1;
        if let Ok(Some((_header, _key, value))) = engine.get_entry(&entry.hash) {
          if let Ok(record) = FileRecord::deserialize(&value, hash_length, 0) {
            logical_data_size += record.total_size;
          }
        }
      }
      KV_TYPE_DIRECTORY => { directories += 1; }
      KV_TYPE_SYMLINK => { symlinks += 1; }
      KV_TYPE_CHUNK => {
        chunks += 1;
        if let Ok(Some((_header, _key, value))) = engine.get_entry(&entry.hash) {
          chunk_data_size += value.len() as u64;
        }
      }
      KV_TYPE_SNAPSHOT => { snapshots += 1; }
      KV_TYPE_FORK => { forks += 1; }
      _ => {}
    }
  }

  let void_space = if let Ok(vm) = engine.void_manager.read() {
    vm.total_void_space()
  } else {
    0
  };

  // Preserve current throughput counters (they are monotonic, not reconciled)
  let current = engine.counters().snapshot();

  Ok(CountersSnapshot {
    files,
    directories,
    symlinks,
    chunks,
    snapshots,
    forks,
    logical_data_size,
    chunk_data_size,
    void_space,
    writes_total: current.writes_total,
    reads_total: current.reads_total,
    bytes_written_total: current.bytes_written_total,
    bytes_read_total: current.bytes_read_total,
    chunks_deduped_total: current.chunks_deduped_total,
    write_buffer_depth: current.write_buffer_depth,
  })
}
