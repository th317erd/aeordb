use std::collections::HashSet;
use std::fmt;
use std::ops::{Deref, DerefMut};

use crate::engine::btree::{BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::engine_event::{EVENT_GC_COMPLETED, EVENT_GC_STARTED};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::engine_counters::{estimated_chunk_payload_bytes, CountersSnapshot};
use crate::engine::file_record::FileRecord;
use crate::engine::kv_store::{
  KVEntry, KV_TYPE_DELETION, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_CHUNK, KV_TYPE_SNAPSHOT, KV_TYPE_FORK, KV_TYPE_SYMLINK,
};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::request_context::RequestContext;
use crate::engine::run_configuration::GcRunConfiguration;
use crate::engine::rss_sampler::PhaseSampler;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::{symlink_path_hash, symlink_content_hash};
use crate::engine::system_family_policy::GcPathSelection;
use crate::engine::version_manager::VersionManager;
use crate::engine::version_manager::SnapshotInfo;
use crate::engine::SystemFamilyPolicyResolver;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// Fixed coordinator workspace required before GC can inspect version roots.
/// Retained mark/sweep collections add their own reservations as they grow.
const GC_ADMISSION_BYTES: u64 = 512 * 1024;
/// Conservative retained v3 mark/sweep footprint per live KV row. This covers
/// cloned KV rows, hash-set buckets, candidate/reverify hashes, and void tuples.
const GC_RETAINED_BYTES_PER_KV_ENTRY: u64 = 512;
const MAXIMUM_GC_CLEANUP_WARNINGS: usize = 32;

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
  /// Bounded optional-cleanup incidents observed during or after the primary
  /// GC operation. The primary mark/sweep result remains authoritative.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub cleanup_warnings: Vec<String>,
}

struct GcRecheckGuard<'a> {
  engine: &'a StorageEngine,
  armed: bool,
}

impl<'a> GcRecheckGuard<'a> {
  fn active(engine: &'a StorageEngine) -> Self {
    Self { engine, armed: true }
  }

  fn finish(mut self, primary_result: EngineResult<GcResult>) -> EngineResult<GcResult> {
    self.armed = false;
    let cleanup_result = self.engine.end_gc_recheck();
    match (primary_result, cleanup_result) {
      (result, Ok(())) => result,
      (Ok(mut result), Err(cleanup_error)) => {
        metrics::counter!("aeordb_gc_recheck_teardown_failures_total").increment(1);
        let warning = bounded_gc_cleanup_warning("GC recheck teardown recovered", &cleanup_error.to_string());
        tracing::warn!(%cleanup_error, "GC recheck teardown recovered after primary success");
        result.cleanup_warnings.push(warning);
        Ok(result)
      }
      (Err(primary_error), Err(cleanup_error)) => {
        metrics::counter!("aeordb_gc_recheck_teardown_failures_total").increment(1);
        tracing::error!(%primary_error, %cleanup_error, "GC recheck teardown also failed; preserving the primary GC error");
        Err(primary_error)
      }
    }
  }
}

impl Drop for GcRecheckGuard<'_> {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }
    if let Err(error) = self.engine.end_gc_recheck() {
      metrics::counter!("aeordb_gc_recheck_teardown_failures_total").increment(1);
      tracing::error!(%error, "GC recheck teardown recovered during unwind");
    }
  }
}

fn bounded_gc_cleanup_warning(context: &str, error: &str) -> String {
  const MAXIMUM_BYTES: usize = 512;
  let prefix = format!("{context}: ");
  let available = MAXIMUM_BYTES.saturating_sub(prefix.len());
  if error.len() <= available {
    return format!("{prefix}{error}");
  }
  let mut boundary = available.saturating_sub(3);
  while boundary > 0 && !error.is_char_boundary(boundary) {
    boundary -= 1;
  }
  format!("{prefix}{}...", &error[..boundary])
}

fn record_gc_cleanup_failure(cleanup_warnings: &mut Vec<String>, stage: &'static str, context: &str, error: &str) {
  metrics::counter!("aeordb_gc_optional_cleanup_failures_total", "stage" => stage).increment(1);
  tracing::warn!(stage, error, "Optional GC cleanup failed; preserving the primary GC operation");
  if cleanup_warnings.len() < MAXIMUM_GC_CLEANUP_WARNINGS.saturating_sub(1) {
    cleanup_warnings.push(bounded_gc_cleanup_warning(context, error));
  } else if cleanup_warnings.len() == MAXIMUM_GC_CLEANUP_WARNINGS.saturating_sub(1) {
    cleanup_warnings.push("Additional GC optional-cleanup incidents were omitted from this bounded result".to_string());
  }
}

fn cleanup_old_pre_gc_snapshots<F>(snapshots: EngineResult<Vec<SnapshotInfo>>, mut delete_snapshot: F, cleanup_warnings: &mut Vec<String>)
where
  F: FnMut(&str) -> EngineResult<()>,
{
  let mut pre_gc_snapshots: Vec<String> = match snapshots {
    Ok(snapshots) => {
      snapshots.iter().filter(|snapshot| snapshot.name.starts_with("_aeordb_pre_gc_")).map(|snapshot| snapshot.name.clone()).collect()
    }
    Err(error) => {
      record_gc_cleanup_failure(cleanup_warnings, "pre_gc_snapshot_list", "Failed to list old pre-GC snapshots", &error.to_string());
      return;
    }
  };
  pre_gc_snapshots.sort();
  pre_gc_snapshots.reverse();

  for old_name in pre_gc_snapshots.iter().skip(3) {
    if let Err(error) = delete_snapshot(old_name) {
      record_gc_cleanup_failure(
        cleanup_warnings,
        "pre_gc_snapshot_delete",
        &format!("Failed to delete old pre-GC snapshot {old_name}"),
        &error.to_string(),
      );
    }
  }
}

/// Reachability set returned by the low-level mark API. Its coordinator
/// reservation remains live until sweep consumption ends or the caller drops
/// the set, preventing embedded callers from bypassing GC memory accounting.
pub struct GcLiveSet {
  hashes: HashSet<Vec<u8>>,
  _reservation: MemoryReservation,
}

impl fmt::Debug for GcLiveSet {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("GcLiveSet").field("hashes", &self.hashes).finish_non_exhaustive()
  }
}

impl Deref for GcLiveSet {
  type Target = HashSet<Vec<u8>>;

  fn deref(&self) -> &Self::Target {
    &self.hashes
  }
}

impl DerefMut for GcLiveSet {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.hashes
  }
}

fn is_directory_hard_link(entry_type: EntryType, value: &[u8], hash_length: usize) -> bool {
  entry_type == EntryType::DirectoryIndex && value.len() == hash_length
}

/// Collect all reachable hashes from HEAD + all snapshots + all forks.
pub fn gc_mark(engine: &StorageEngine) -> EngineResult<GcLiveSet> {
  let run_configuration = engine.capture_gc_run_configuration()?;
  let reservation = reserve_gc_workspace(engine, &run_configuration)?;
  let hashes = gc_mark_internal(engine, None)?;
  Ok(GcLiveSet { hashes, _reservation: reservation })
}

fn gc_mark_internal(engine: &StorageEngine, cancellation: Option<&CancellationToken>) -> EngineResult<HashSet<Vec<u8>>> {
  check_gc_cancellation(engine, cancellation)?;
  let mut live: HashSet<Vec<u8>> = HashSet::new();
  let hash_length = engine.hash_algo().hash_length();
  let family_policy = SystemFamilyPolicyResolver::new(engine.hash_algo())?;
  let timing = std::env::var("AEORDB_GC_TIMING").is_ok();
  let mark_start = std::time::Instant::now();

  // Gather all merkle roots and walk them BFS with offset-sorted I/O.
  // The walk visits each unique hash once across all roots (visited-set
  // short-circuit), so structural sharing between snapshots is free.
  let mut roots: Vec<(Vec<u8>, String)> = Vec::new();
  let head_hash = engine.head_hash()?;
  if !head_hash.is_empty() && head_hash.iter().any(|&b| b != 0) {
    roots.push((head_hash, "/".to_string()));
  }

  let vm = VersionManager::new(engine);
  let snapshots = vm.list_snapshots()?;
  for (index, snapshot) in snapshots.iter().enumerate() {
    check_gc_quantum(engine, cancellation, index)?;
    roots.push((snapshot.root_hash.clone(), "/".to_string()));
  }
  let forks = vm.list_forks()?;
  for (index, fork) in forks.iter().enumerate() {
    check_gc_quantum(engine, cancellation, index)?;
    roots.push((fork.root_hash.clone(), "/".to_string()));
  }

  if timing {
    eprintln!("[gc-timing] mark: {} roots ({} snapshots + {} forks + HEAD)", roots.len(), snapshots.len(), forks.len());
  }

  let bfs_start = std::time::Instant::now();
  let bfs_mem = PhaseSampler::start("mark.bfs", std::time::Duration::from_millis(50));
  walk_versions_bfs(engine, roots, hash_length, family_policy, &mut live, cancellation)?;
  bfs_mem.finish();
  if timing {
    eprintln!("[gc-timing] mark.bfs: {:?} (live={})", bfs_start.elapsed(), live.len());
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

  // Mark detached registry path families as live.
  let sys_start = std::time::Instant::now();
  mark_registry_gc_entries(engine, hash_length, family_policy, &mut live, cancellation)?;
  if timing {
    eprintln!("[gc-timing] mark.system: {:?}", sys_start.elapsed());
  }

  let conflict_start = std::time::Instant::now();
  mark_conflict_gc_entries(engine, hash_length, family_policy, &mut live, cancellation)?;
  if timing {
    eprintln!("[gc-timing] mark.conflicts: {:?}", conflict_start.elapsed());
  }

  // Mark task queue entries as live -- task records use deterministic hashes
  // ("::aeordb:task:{id}") that are NOT in the directory tree, so
  // Registry path traversal does not cover them.
  let task_start = std::time::Instant::now();
  mark_task_entries(engine, &mut live, cancellation)?;
  if timing {
    eprintln!("[gc-timing] mark.tasks: {:?}", task_start.elapsed());
  }

  let all_entries = engine.iter_kv_entries()?;

  // Mark current path-key FileRecords as live even if HEAD temporarily
  // diverged from the path index. User-facing reads resolve `file:{path}`
  // directly, so sweeping chunks referenced by a live path-key record creates
  // a dangling file that still appears readable until chunk lookup fails.
  let path_file_start = std::time::Instant::now();
  let path_file_count = mark_live_path_file_records(engine, hash_length, &mut live, &all_entries, cancellation)?;
  if timing {
    eprintln!("[gc-timing] mark.path-files: {:?} (path_records={})", path_file_start.elapsed(), path_file_count);
  }

  // Mark DeletionRecord entries as live — they are needed for KV rebuild
  // from a full .aeordb scan (deletion replay) and must not be swept.
  let del_start = std::time::Instant::now();
  let del_mem = PhaseSampler::start("mark.deletion-pass", std::time::Duration::from_millis(50));
  let mut deletion_count = 0usize;
  for (index, entry) in all_entries.iter().enumerate() {
    check_gc_quantum(engine, cancellation, index)?;
    if entry.entry_type() == KV_TYPE_DELETION {
      live.insert(entry.hash.clone());
      deletion_count += 1;
    }
  }
  del_mem.finish();
  if timing {
    eprintln!("[gc-timing] mark.deletion-pass: {:?} (kv_entries={}, deletions={})", del_start.elapsed(), all_entries.len(), deletion_count);
    eprintln!("[gc-timing] mark TOTAL: {:?} (live={})", mark_start.elapsed(), live.len());
  }

  Ok(live)
}

fn mark_live_path_file_records(
  engine: &StorageEngine,
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
  all_entries: &[KVEntry],
  cancellation: Option<&CancellationToken>,
) -> EngineResult<usize> {
  let algo = engine.hash_algo();
  let mut marked = 0usize;

  for (index, entry) in all_entries.iter().enumerate() {
    check_gc_quantum(engine, cancellation, index)?;
    if entry.entry_type() != KV_TYPE_FILE_RECORD {
      continue;
    }
    if live.contains(&entry.hash) {
      continue;
    }

    let Some((header, _key, value)) = engine.get_entry(&entry.hash)? else {
      continue;
    };
    if let Some(task_validation) = crate::engine::task_queue::validate_task_storage_record(&entry.hash, &value) {
      task_validation?;
      continue;
    }
    let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version).map_err(|error| EngineError::CorruptEntry {
      offset: entry.offset,
      reason: format!("ambiguous FileRecord-tag row cannot be safely classified during GC mark: {error}"),
    })?;
    let path_key = crate::engine::directory_ops::file_path_hash(&file_record.path, &algo)?;
    if entry.hash != path_key {
      continue;
    }

    live.insert(entry.hash.clone());
    for chunk_hash in &file_record.chunk_hashes {
      live.insert(chunk_hash.clone());
    }

    let content_key = crate::engine::directory_ops::file_content_hash(&value, &algo)?;
    live.insert(content_key);
    let identity_key = crate::engine::directory_ops::file_identity_hash(
      &file_record.path,
      file_record.content_type.as_deref(),
      &file_record.chunk_hashes,
      &algo,
    )?;
    live.insert(identity_key);
    marked += 1;
  }

  Ok(marked)
}

/// Walk all version roots level-by-level, sorting each level by KV offset
/// for sequential WAL I/O instead of random reads in tree-walk order.
///
/// The KV is in-memory, so type lookups and offset lookups are free. The
/// expensive part — reading entry payloads from the WAL — happens in
/// offset-ascending order, which lets the page cache and disk scheduler do
/// large sequential reads instead of seeking on every entry.
///
/// **Type-aware leaf skip**: entries whose KV type is `KV_TYPE_CHUNK` are
/// leaves — they have no children to follow. We mark them live without
/// reading their payload from disk.
fn walk_versions_bfs(
  engine: &StorageEngine,
  roots: Vec<(Vec<u8>, String)>,
  hash_length: usize,
  family_policy: SystemFamilyPolicyResolver,
  live: &mut HashSet<Vec<u8>>,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<()> {
  let algo = engine.hash_algo();
  let timing = std::env::var("AEORDB_GC_TIMING").is_ok();
  let mut frontier = roots;
  let mut level = 0u32;
  let mut total_reads = 0u64;
  let mut total_leaves_skipped = 0u64;

  while !frontier.is_empty() {
    check_gc_cancellation(engine, cancellation)?;
    let frontier_size = frontier.len();
    // Stage 1: dedup, mark path-keys for directories, fold in leaf-only entries.
    // Survivors need a disk read; collect them with their KV offset.
    let mut to_read: Vec<(Vec<u8>, String, u64)> = Vec::with_capacity(frontier.len());
    let mut visited_dups = 0u64;
    let mut leaves_skipped = 0u64;
    let not_in_kv = 0u64;
    let dedup_start = std::time::Instant::now();
    for (index, (hash, path)) in frontier.drain(..).enumerate() {
      check_gc_quantum(engine, cancellation, index)?;
      if !live.insert(hash.clone()) {
        // Already visited content hash — still mark the path-key for this
        // appearance because the same content can be referenced under
        // multiple paths.
        let path_key = engine.compute_hash(format!("dir:{}", path).as_bytes())?;
        live.insert(path_key);
        visited_dups += 1;
        continue;
      }
      // In-memory KV lookup tells us the type and offset without disk I/O.
      match engine.get_kv_entry(&hash)? {
        Some(kv) => {
          let t = kv.entry_type();
          if t == KV_TYPE_CHUNK {
            // Leaf — already in `live`, nothing more to do.
            leaves_skipped += 1;
            continue;
          }
          to_read.push((hash, path, kv.offset));
        }
        None => {
          return Err(EngineError::NotFound(format!(
            "Reachable entry not found during GC mark: path={} hash={}",
            path,
            hex::encode(&hash)
          )));
        }
      }
    }
    let dedup_elapsed = dedup_start.elapsed();
    total_leaves_skipped += leaves_skipped;

    // Stage 2: sort by WAL offset so reads are sequential.
    let sort_start = std::time::Instant::now();
    to_read.sort_by_key(|(_, _, offset)| *offset);
    let sort_elapsed = sort_start.elapsed();

    // Stage 3: read each entry in offset order; emit children to next frontier.
    let read_start = std::time::Instant::now();
    let read_count = to_read.len();
    total_reads += read_count as u64;
    let mut next_frontier: Vec<(Vec<u8>, String)> = Vec::new();
    for (index, (hash, path, _offset)) in to_read.into_iter().enumerate() {
      check_gc_quantum(engine, cancellation, index)?;
      let entry = match engine.get_entry_including_deleted(&hash)? {
        Some(e) => e,
        None => {
          return Err(EngineError::NotFound(format!(
            "Reachable entry disappeared during GC mark: path={} hash={}",
            path,
            hex::encode(&hash)
          )));
        }
      };
      let (header, _key, value) = entry;

      // Only directory indexes use hash-sized payloads as hard links. Other
      // typed records can legitimately serialize to exactly hash_length bytes.
      let value = if is_directory_hard_link(header.entry_type, &value, hash_length) {
        live.insert(value.clone());
        match engine.get_entry_including_deleted(&value)? {
          Some((_h, _k, v)) => v,
          None => {
            return Err(EngineError::NotFound(format!(
              "Hard-link target not found during GC mark: path={} target={}",
              path,
              hex::encode(&value)
            )));
          }
        }
      } else {
        value
      };

      match header.entry_type {
        EntryType::DirectoryIndex => {
          // Mark the path-keyed lookup for this directory.
          let path_key = engine.compute_hash(format!("dir:{}", path).as_bytes())?;
          live.insert(path_key);

          if value.is_empty() {
            continue;
          }
          let children = if is_btree_format(&value) {
            collect_btree_children(engine, &value, hash_length, live, cancellation)?
          } else {
            deserialize_child_entries(&value, hash_length, header.entry_version)?
          };

          for child in &children {
            let child_path = if path == "/" { format!("/{}", child.name) } else { format!("{}/{}", path, child.name) };
            // V3 keeps every currently reachable family, including derived
            // state. The typed decision still rejects unknown protected paths
            // and identifies rebuildable state for the v4 GC owner.
            match family_policy.gc_path_selection(&child_path)? {
              GcPathSelection::Retain | GcPathSelection::Rebuildable | GcPathSelection::StructuralContainer => {}
            }
            let child_type = EntryType::from_u8(child.entry_type)?;
            match child_type {
              EntryType::DirectoryIndex | EntryType::FileRecord | EntryType::Symlink => {
                next_frontier.push((child.hash.clone(), child_path));
              }
              _ => {
                live.insert(child.hash.clone());
              }
            }
          }
        }
        EntryType::FileRecord => {
          let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
          // Chunks are leaves — mark them as live without disk reads.
          for chunk_hash in &file_record.chunk_hashes {
            live.insert(chunk_hash.clone());
          }
          // Mark path-key (mutable index used for reads) and content-key
          // (immutable content-addressed entry).
          let file_path_key = crate::engine::directory_ops::file_path_hash(&file_record.path, &algo)?;
          live.insert(file_path_key);
          let content_key = crate::engine::directory_ops::file_content_hash(&value, &algo)?;
          live.insert(content_key);
        }
        EntryType::Symlink => {
          let path_key = symlink_path_hash(&path, &algo)?;
          live.insert(path_key);
          let content_key = symlink_content_hash(&value, &algo)?;
          live.insert(content_key);
        }
        _ => {
          // Unhandled types are simply present in `live` already.
        }
      }
    }

    let read_elapsed = read_start.elapsed();

    if timing {
      eprintln!(
        "[gc-timing]   level {}: frontier={} → dedup {:?} (dups={} leaves_skip={} miss={}) → sort {:?} → read {} entries in {:?} → next={}",
        level,
        frontier_size,
        dedup_elapsed,
        visited_dups,
        leaves_skipped,
        not_in_kv,
        sort_elapsed,
        read_count,
        read_elapsed,
        next_frontier.len(),
      );
    }

    frontier = next_frontier;
    level += 1;
  }

  if timing {
    eprintln!(
      "[gc-timing]   bfs summary: {} levels, {} entries read from disk, {} leaves skipped",
      level, total_reads, total_leaves_skipped,
    );
  }

  Ok(())
}

/// Collect children from a B-tree node, marking all intermediate node hashes.
fn collect_btree_children(
  engine: &StorageEngine,
  node_data: &[u8],
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<Vec<ChildEntry>> {
  check_gc_cancellation(engine, cancellation)?;
  // TODO: thread the surrounding EntryHeader's entry_version through collect_btree_children
  // when a v1 BTreeNode format ships. Today everything on disk is v0.
  let node = BTreeNode::deserialize(node_data, hash_length, 0)?;
  let mut all_children = Vec::new();

  match node {
    BTreeNode::Leaf(leaf) => {
      all_children.extend(leaf.entries);
    }
    BTreeNode::Internal(internal) => {
      for (index, child_hash) in internal.children.iter().enumerate() {
        check_gc_quantum(engine, cancellation, index)?;
        live.insert(child_hash.clone());
        // B-tree internal nodes may be deleted at HEAD but snapshot-referenced.
        let Some((header, _key, child_data)) = engine.get_entry_including_deleted(child_hash)? else {
          return Err(crate::engine::errors::EngineError::NotFound(format!(
            "B-tree child not found during GC mark: {}",
            hex::encode(child_hash)
          )));
        };
        if header.entry_type != EntryType::DirectoryIndex {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("B-tree child hash resolved to {:?} during GC mark: {}", header.entry_type, hex::encode(child_hash)),
          });
        }
        let sub_children = collect_btree_children(engine, &child_data, hash_length, live, cancellation)?;
        all_children.extend(sub_children);
      }
    }
  }

  Ok(all_children)
}

/// Mark registry-selected detached path families as live.
fn mark_registry_gc_entries(
  engine: &StorageEngine,
  hash_length: usize,
  family_policy: SystemFamilyPolicyResolver,
  live: &mut HashSet<Vec<u8>>,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<()> {
  let algo = engine.hash_algo();
  for path in family_policy.retained_absolute_gc_paths()? {
    check_gc_cancellation(engine, cancellation)?;
    let candidate_keys = [
      crate::engine::directory_ops::directory_path_hash(&path, &algo)?,
      crate::engine::directory_ops::file_path_hash(&path, &algo)?,
      symlink_path_hash(&path, &algo)?,
    ];
    for key in candidate_keys {
      if engine.get_entry_including_deleted(&key)?.is_some() {
        mark_entry_recursive(engine, &key, &path, hash_length, family_policy, live, cancellation)?;
      }
    }
  }

  Ok(())
}

/// Mark immutable file/symlink versions referenced from unresolved conflict
/// metadata. The registry walk retains the metadata file itself; this pass
/// follows its typed JSON edges to the versions a later resolution may select.
fn mark_conflict_gc_entries(
  engine: &StorageEngine,
  hash_length: usize,
  family_policy: SystemFamilyPolicyResolver,
  live: &mut HashSet<Vec<u8>>,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<()> {
  let references = crate::engine::conflict_store::retained_conflict_version_references(engine)?;
  for (index, reference) in references.iter().enumerate() {
    check_gc_quantum(engine, cancellation, index)?;
    mark_entry_recursive(engine, &reference.hash, &reference.path, hash_length, family_policy, live, cancellation)?;
  }
  Ok(())
}

/// Generic recursive mark for entries reachable from system tables.
///
/// Mirrors the per-type handling in [`walk_versions_bfs`]:
/// - DirectoryIndex: mark `dir:{path}` path-key + follow content-hash hard
///   link + recurse children with the child path.
/// - FileRecord: mark `file:{path}` path-key + content-key + chunk hashes.
/// - Symlink: mark `symlink:{path}` path-key + content-key.
///
/// `path` is the absolute path of the entry being marked. We need it because directories and
/// symlinks don't carry their own path in the stored value, and files use
/// `file_path_hash(path)` rather than the identity/content hash for path
/// lookups.
fn mark_entry_recursive(
  engine: &StorageEngine,
  hash: &[u8],
  path: &str,
  hash_length: usize,
  family_policy: SystemFamilyPolicyResolver,
  live: &mut HashSet<Vec<u8>>,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<()> {
  check_gc_cancellation(engine, cancellation)?;
  let debug = std::env::var("AEORDB_GC_DEBUG_SYSTEM").is_ok();

  if !live.insert(hash.to_vec()) {
    if debug {
      eprintln!("[gc-rec]   hash={} path={:?} already-live, skip", hex::encode(&hash[..8.min(hash.len())]), path);
    }
    return Ok(());
  }

  // Use _including_deleted: system entries may reference content-addressed
  // entries that are deleted at HEAD but still needed.
  let entry = match engine.get_entry_including_deleted(hash)? {
    Some(entry) => entry,
    None => {
      if debug {
        eprintln!("[gc-rec]   hash={} path={:?} NOT-FOUND", hex::encode(&hash[..8.min(hash.len())]), path);
      }
      return Err(EngineError::NotFound(format!("System entry not found during GC mark: path={} hash={}", path, hex::encode(hash))));
    }
  };

  let (header, _key, value) = entry;
  let algo = engine.hash_algo();

  // Only directory indexes use hash-sized payloads as hard links. Other
  // typed records can legitimately serialize to exactly hash_length bytes.
  let value = if is_directory_hard_link(header.entry_type, &value, hash_length) {
    live.insert(value.clone());
    match engine.get_entry_including_deleted(&value)? {
      Some((_h, _k, v)) => v,
      None => {
        return Err(EngineError::NotFound(format!(
          "System hard-link target not found during GC mark: path={} target={}",
          path,
          hex::encode(&value)
        )));
      }
    }
  } else {
    value
  };

  if debug {
    eprintln!(
      "[gc-rec]   hash={} path={:?} type={:?} value_len={}",
      hex::encode(&hash[..8.min(hash.len())]),
      path,
      header.entry_type,
      value.len()
    );
  }

  match header.entry_type {
    EntryType::DirectoryIndex => {
      // Path-key the engine uses for `list_directory` / `read_file` lookups.
      let path_key = engine.compute_hash(format!("dir:{}", path).as_bytes())?;
      live.insert(path_key);

      if value.is_empty() {
        return Ok(());
      }

      let children = if is_btree_format(&value) {
        collect_btree_children(engine, &value, hash_length, live, cancellation)?
      } else {
        deserialize_child_entries(&value, hash_length, header.entry_version)?
      };
      for (index, child) in children.iter().enumerate() {
        check_gc_quantum(engine, cancellation, index)?;
        let child_path = if path == "/" { format!("/{}", child.name) } else { format!("{}/{}", path, child.name) };
        match family_policy.gc_path_selection(&child_path)? {
          GcPathSelection::Retain | GcPathSelection::Rebuildable | GcPathSelection::StructuralContainer => {}
        }
        mark_entry_recursive(engine, &child.hash, &child_path, hash_length, family_policy, live, cancellation)?;
      }
    }
    EntryType::FileRecord => {
      let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
      for chunk_hash in &file_record.chunk_hashes {
        live.insert(chunk_hash.clone());
      }
      // Match walk_versions_bfs: mark the path-key (mutable index used for
      // reads) and the content-key (immutable content-addressed entry).
      let file_path_key = crate::engine::directory_ops::file_path_hash(&file_record.path, &algo)?;
      live.insert(file_path_key);
      let content_key = crate::engine::directory_ops::file_content_hash(&value, &algo)?;
      live.insert(content_key);
    }
    EntryType::Symlink => {
      let path_key = symlink_path_hash(path, &algo)?;
      live.insert(path_key);
      let content_key = symlink_content_hash(&value, &algo)?;
      live.insert(content_key);
    }
    _ => {}
  }

  Ok(())
}

/// Mark task queue entries (registry + individual task records) as live.
/// Task records use deterministic blake3 hashes on "::aeordb:task:{id}" keys
/// and are stored as EntryType::FileRecord, so they would be swept by GC
/// unless explicitly marked.
fn mark_task_entries(engine: &StorageEngine, live: &mut HashSet<Vec<u8>>, cancellation: Option<&CancellationToken>) -> EngineResult<()> {
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();
  live.insert(registry_key.clone());

  // Load the registry to find all task IDs
  if let Some((_header, _key, value)) = engine.get_entry(&registry_key)? {
    let ids = serde_json::from_slice::<Vec<String>>(&value)
      .map_err(|error| EngineError::InvalidInput(format!("task registry is malformed during GC mark: {error}")))?;
    for (index, id) in ids.iter().enumerate() {
      check_gc_quantum(engine, cancellation, index)?;
      let task_key = blake3::hash(format!("::aeordb:task:{}", id).as_bytes()).as_bytes().to_vec();
      live.insert(task_key);
    }
  }

  Ok(())
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
pub fn gc_sweep(engine: &StorageEngine, live: &GcLiveSet, dry_run: bool) -> EngineResult<(usize, u64)> {
  gc_sweep_internal(engine, &live.hashes, dry_run, None)
}

fn gc_sweep_internal(
  engine: &StorageEngine,
  live: &HashSet<Vec<u8>>,
  dry_run: bool,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<(usize, u64)> {
  check_gc_cancellation(engine, cancellation)?;
  let timing = std::env::var("AEORDB_GC_TIMING").is_ok();
  let all_entries = engine.iter_kv_entries()?;

  // First pass: identify garbage entries and compute sizes.
  let mut garbage_candidates: Vec<(Vec<u8>, u64, u32)> = Vec::new(); // (hash, offset, entry_size)
  let mut garbage_count: usize = 0;
  let mut reclaimed_bytes: u64 = 0;

  for (index, entry) in all_entries.iter().enumerate() {
    check_gc_quantum(engine, cancellation, index)?;
    if live.contains(&entry.hash) {
      continue;
    }
    // Spare entries that landed during mark/sweep — they're in the recheck
    // set the engine maintains while GC is active. Without this, concurrent
    // writes would be eligible for sweep just because they're not in `live`.
    if !dry_run && engine.gc_recheck_contains(&entry.hash)? {
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

  drop(all_entries);

  if dry_run || garbage_candidates.is_empty() {
    return Ok((garbage_count, reclaimed_bytes));
  }

  // Re-verify each candidate against the current KV state. A concurrent
  // write between mark and sweep could have re-created an entry at a new
  // offset (same hash, different WAL position); we must NOT delete those.
  let reverify_start = std::time::Instant::now();
  let mut verified_hashes: Vec<Vec<u8>> = Vec::with_capacity(garbage_candidates.len());
  let mut freed_regions: Vec<(u64, u32)> = Vec::with_capacity(garbage_candidates.len());
  for (index, (hash, offset, entry_size)) in garbage_candidates.iter().enumerate() {
    check_gc_quantum(engine, cancellation, index)?;
    match engine.get_kv_entry(hash)? {
      Some(fresh) if fresh.offset == *offset => {
        if engine.is_current_reusable_range(*offset, *entry_size)? {
          verified_hashes.push(hash.clone());
          freed_regions.push((*offset, *entry_size));
        } else {
          tracing::warn!(
            offset = *offset,
            entry_size = *entry_size,
            "GC candidate points outside current WAL region; skipping void registration"
          );
          garbage_count -= 1;
          reclaimed_bytes -= *entry_size as u64;
        }
      }
      _ => {
        // Re-created since mark — skip and rollback the size accounting.
        garbage_count -= 1;
        reclaimed_bytes -= *entry_size as u64;
      }
    }
  }
  let reverify_elapsed = reverify_start.elapsed();

  // Refuse before the first destructive KV mutation if reclaimed-space
  // authority is unavailable. Once KV removal begins, every verified region
  // must be registered and durably flushed rather than silently leaked.
  if !freed_regions.is_empty() {
    let void_manager = engine
      .void_manager
      .read()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("GC void-manager preflight failed: {error}"))))?;
    drop(void_manager);
  }

  // Drop the verified hashes from the live KV index. All in-memory; no WAL
  // writes from sweep itself — the durability of these deletions comes from
  // the hot tail flush that follows (which carries the void snapshot, and
  // by the void offsets implies the entries at those offsets are gone).
  let kv_remove_start = std::time::Instant::now();
  check_gc_cancellation(engine, cancellation)?;
  if !verified_hashes.is_empty() {
    engine.remove_kv_entries_batch(&verified_hashes)?;
  }
  let kv_remove_elapsed = kv_remove_start.elapsed();

  // Register the freed regions with VoidManager (in-memory). On the next
  // hot tail flush these get mirrored to disk as VoidRecords.
  let void_register_start = std::time::Instant::now();
  if !freed_regions.is_empty() {
    let mut vm = engine
      .void_manager
      .write()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("GC void registration failed: {error}"))))?;
    for (offset, size) in &freed_regions {
      vm.register_void(*offset, *size);
    }
  }
  let void_register_elapsed = void_register_start.elapsed();

  // Sync void state into the kv_writer's pending_voids and force a hot tail
  // flush so the new void set is durable. One sequential write at the WAL
  // tail; one fsync. Fast on slow disks.
  let flush_start = std::time::Instant::now();
  engine.sync_voids_to_kv_writer()?;
  engine.force_hot_tail_flush()?;
  let flush_elapsed = flush_start.elapsed();

  if timing {
    eprintln!("[gc-timing]   sweep.reverify: {:?} (kept {} of {})", reverify_elapsed, verified_hashes.len(), garbage_candidates.len());
    eprintln!("[gc-timing]   sweep.kv_remove: {:?}", kv_remove_elapsed);
    eprintln!("[gc-timing]   sweep.void_register: {:?} ({} voids)", void_register_elapsed, freed_regions.len());
    eprintln!("[gc-timing]   sweep.hot_tail_flush: {:?}", flush_elapsed);
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
pub fn run_gc(engine: &StorageEngine, ctx: &RequestContext, dry_run: bool) -> EngineResult<GcResult> {
  run_gc_internal(engine, ctx, dry_run, None, || {}, || {})
}

/// Run a complete GC cycle while honoring cooperative cancellation before
/// side effects and at bounded safe points during mark/sweep preparation.
pub fn run_gc_with_cancellation(
  engine: &StorageEngine,
  ctx: &RequestContext,
  dry_run: bool,
  cancellation: &CancellationToken,
) -> EngineResult<GcResult> {
  run_gc_internal(engine, ctx, dry_run, Some(cancellation), || {}, || {})
}

/// Deterministic phase-boundary hook used to prove pressure changes that occur
/// after initial GC admission but before mark work begins.
#[doc(hidden)]
pub fn run_gc_with_post_start_hook<F>(
  engine: &StorageEngine,
  ctx: &RequestContext,
  dry_run: bool,
  post_start_hook: F,
) -> EngineResult<GcResult>
where
  F: FnOnce(),
{
  run_gc_internal(engine, ctx, dry_run, None, post_start_hook, || {})
}

fn run_gc_internal<F, G>(
  engine: &StorageEngine,
  ctx: &RequestContext,
  dry_run: bool,
  cancellation: Option<&CancellationToken>,
  post_start_hook: F,
  pre_teardown_hook: G,
) -> EngineResult<GcResult>
where
  F: FnOnce(),
  G: FnOnce(),
{
  check_gc_token(cancellation)?;
  let run_configuration = engine.capture_gc_run_configuration()?;
  let gc_admission = reserve_gc_workspace(engine, &run_configuration)?;
  check_gc_cancellation(engine, cancellation)?;

  let start = std::time::Instant::now();

  // Mutating GC must not observe a half-published namespace update. Directory
  // B-tree writes are intentionally multi-step (write new nodes, then publish
  // path keys/HEAD), so a concurrent sweep can otherwise collect nodes that
  // are about to become reachable. Dry-run GC does not sweep and can remain
  // non-blocking.
  let _namespace_guard = if dry_run { None } else { Some(engine.direct_hard_authority_guard()?) };

  // Emit GC started event
  ctx.emit(
    EVENT_GC_STARTED,
    serde_json::json!({
      "dry_run": dry_run,
      "configuration_generation": run_configuration.generation,
    }),
  );
  post_start_hook();
  check_gc_cancellation(engine, cancellation)?;

  // Begin GC recheck tracking before any version-forest reads. From this
  // point on, every successful write hash is recorded so the sweep phase can
  // spare entries that arrived after the mark snapshot was captured. See
  // bot-docs/plan/gc-mark-sweep.md. Normal completion combines the primary
  // result with explicit teardown; Drop remains only as unwind protection.
  if !dry_run {
    engine.begin_gc_recheck()?;
  }
  let recheck_guard = if dry_run { None } else { Some(GcRecheckGuard::active(engine)) };

  let primary_result = (|| -> EngineResult<GcResult> {
    let vm = VersionManager::new(engine);
    let mut cleanup_warnings = Vec::new();

    // Auto-snapshot before GC — safety net in case sweep removes something needed
    if !dry_run {
      if crate::engine::lifecycle_config::snapshot_writes_enabled(engine) {
        let snapshot_name = format!("_aeordb_pre_gc_{}_{}", chrono::Utc::now().timestamp_millis(), uuid::Uuid::new_v4().simple());

        match vm.create_snapshot(ctx, &snapshot_name, std::collections::HashMap::new()) {
          Ok(_) => {
            tracing::info!("Created pre-GC snapshot: {}", snapshot_name);
          }
          Err(e) => {
            return Err(e);
          }
        }
      } else {
        tracing::info!("Skipping pre-GC snapshot because snapshot writes are disabled");
      }

      // Clean up old pre-GC snapshots — keep last 3. This is optional data
      // retention, so failures remain visible without invalidating sweep.
      cleanup_old_pre_gc_snapshots(vm.list_snapshots(), |old_name| vm.delete_snapshot(ctx, old_name), &mut cleanup_warnings);

      // Apply user-configured retention to non-engine snapshots before the
      // mark phase. Snapshots deleted here have their orphaned data swept in
      // this same GC cycle.
      let prune_start = std::time::Instant::now();
      let _gc_timing = std::env::var("AEORDB_GC_TIMING").is_ok();
      match crate::engine::lifecycle_config::prune_expired_snapshots(engine, ctx) {
        Ok(result) if result.pruned_count > 0 => {
          tracing::info!(
            pruned = result.pruned_count,
            names = ?result.pruned_names,
            "Lifecycle retention pruned snapshots",
          );
        }
        Ok(_) => {}
        Err(error) => record_gc_cleanup_failure(
          &mut cleanup_warnings,
          "lifecycle_retention_prune",
          "Lifecycle retention pruning failed",
          &error.to_string(),
        ),
      }
      if _gc_timing {
        eprintln!("[gc-timing] prune: {:?}", prune_start.elapsed());
      }
    }

    let snapshot_count = vm.list_snapshots()?.len();
    let fork_count = vm.list_forks()?.len();
    let versions_scanned = 1 + snapshot_count + fork_count;

    // RSS sampling: bracket mark, recheck-drain, and sweep separately so we can
    // attribute the multi-GB transient to a specific phase. No-op unless
    // AEORDB_GC_MEM_PROFILE is set.
    let mark_mem = PhaseSampler::start("mark", std::time::Duration::from_millis(50));
    check_gc_cancellation(engine, cancellation)?;
    let hashes = gc_mark_internal(engine, cancellation)?;
    let mut live = GcLiveSet { hashes, _reservation: gc_admission };
    mark_mem.finish();
    check_gc_cancellation(engine, cancellation)?;

    // Re-check drain: any entry that was written during the mark phase is now in
    // the recheck set. Walk each one and union into `live` so the sweep doesn't
    // clobber freshly-written data. Loop until the queue is empty for one pass.
    if !dry_run {
      let drain_mem = PhaseSampler::start("recheck-drain", std::time::Duration::from_millis(50));
      let family_policy = SystemFamilyPolicyResolver::new(engine.hash_algo())?;
      loop {
        check_gc_cancellation(engine, cancellation)?;
        let pending = engine.take_gc_recheck()?;
        if pending.is_empty() {
          break;
        }
        let hash_length = engine.hash_algo().hash_length();
        for (index, hash) in pending.into_iter().enumerate() {
          check_gc_quantum(engine, cancellation, index)?;
          // Path is unknown for recheck entries — the writer recorded raw hashes
          // only. Every key it wrote (identity, file-path, content) is in the
          // recheck set independently, so they each get marked when their hash
          // shows up in this loop. The empty path means path-derived keys
          // (dir:{path}, file:{path}) computed inside the recursion are wrong,
          // but harmless: the live set is "do not sweep" — extra hashes in it
          // never match a real entry and are simply ignored.
          mark_entry_recursive(engine, &hash, "", hash_length, family_policy, &mut live, cancellation)?;
        }
      }
      drain_mem.finish();
    }

    let live_entries = live.len();

    let sweep_start = std::time::Instant::now();
    let sweep_mem = PhaseSampler::start("sweep", std::time::Duration::from_millis(50));
    check_gc_cancellation(engine, cancellation)?;
    let (garbage_entries, reclaimed_bytes) = gc_sweep_internal(engine, &live, dry_run, cancellation)?;
    sweep_mem.finish();
    if std::env::var("AEORDB_GC_TIMING").is_ok() {
      eprintln!("[gc-timing] sweep: {:?} (garbage={}, reclaimed_bytes={})", sweep_start.elapsed(), garbage_entries, reclaimed_bytes);
    }

    // Reconcile counters from authoritative KV state after sweep
    if !dry_run {
      let authoritative = build_authoritative_snapshot(engine)?;
      engine.counters().reconcile(&authoritative);
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(GcResult { versions_scanned, live_entries, garbage_entries, reclaimed_bytes, duration_ms, dry_run, cleanup_warnings })
  })();

  pre_teardown_hook();
  let result = match recheck_guard {
    Some(guard) => guard.finish(primary_result),
    None => primary_result,
  }?;

  // Emit GC event
  let mut completion_payload = serde_json::Map::new();
  completion_payload.insert("versions_scanned".to_string(), serde_json::json!(result.versions_scanned));
  completion_payload.insert("live_entries".to_string(), serde_json::json!(result.live_entries));
  completion_payload.insert("garbage_entries".to_string(), serde_json::json!(result.garbage_entries));
  completion_payload.insert("reclaimed_bytes".to_string(), serde_json::json!(result.reclaimed_bytes));
  completion_payload.insert("duration_ms".to_string(), serde_json::json!(result.duration_ms));
  completion_payload.insert("dry_run".to_string(), serde_json::json!(result.dry_run));
  if !result.cleanup_warnings.is_empty() {
    completion_payload.insert("cleanup_warnings".to_string(), serde_json::json!(&result.cleanup_warnings));
  }
  ctx.emit(EVENT_GC_COMPLETED, serde_json::Value::Object(completion_payload));

  Ok(result)
}

fn check_gc_cancellation(engine: &StorageEngine, cancellation: Option<&CancellationToken>) -> EngineResult<()> {
  check_gc_token(cancellation)?;
  engine.memory_coordinator().check_admission(MemoryOwner::GarbageCollection, AdmissionClass::Maintenance).map_err(gc_memory_error)?;
  Ok(())
}

fn check_gc_token(cancellation: Option<&CancellationToken>) -> EngineResult<()> {
  if cancellation.is_some_and(CancellationToken::is_cancelled) {
    return Err(EngineError::Cancelled("garbage collection".to_string()));
  }
  Ok(())
}

#[cfg(test)]
mod recheck_teardown_tests {
  use super::*;
  use crate::engine::directory_ops::DirectoryOps;
  use crate::engine::event_bus::EventBus;
  use std::sync::Arc;

  fn create_test_engine() -> (StorageEngine, tempfile::TempDir) {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("gc-recheck-teardown.aeordb");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    (engine, temporary)
  }

  fn successful_result() -> GcResult {
    GcResult {
      versions_scanned: 1,
      live_entries: 2,
      garbage_entries: 3,
      reclaimed_bytes: 4,
      duration_ms: 5,
      dry_run: false,
      cleanup_warnings: Vec::new(),
    }
  }

  fn snapshot_fixture(name: impl Into<String>) -> SnapshotInfo {
    SnapshotInfo { name: name.into(), root_hash: vec![1; 32], created_at: 0, metadata: std::collections::HashMap::new() }
  }

  #[test]
  fn optional_pre_gc_cleanup_preserves_list_failure_as_bounded_evidence() {
    let mut cleanup_warnings = Vec::new();

    cleanup_old_pre_gc_snapshots(
      Err(EngineError::InvalidInput("injected snapshot listing failure".to_string())),
      |_| panic!("delete must not run when listing failed"),
      &mut cleanup_warnings,
    );

    assert_eq!(cleanup_warnings.len(), 1);
    assert!(cleanup_warnings[0].contains("list old pre-GC snapshots"));
    assert!(cleanup_warnings[0].contains("injected snapshot listing failure"));
  }

  #[test]
  fn optional_pre_gc_cleanup_keeps_newest_three_and_bounds_delete_failures() {
    let snapshots = (0..40).map(|index| snapshot_fixture(format!("_aeordb_pre_gc_{index:03}"))).collect();
    let mut attempted = Vec::new();
    let mut cleanup_warnings = Vec::new();

    cleanup_old_pre_gc_snapshots(
      Ok(snapshots),
      |name| {
        attempted.push(name.to_string());
        Err(EngineError::InvalidInput(format!("injected deletion failure for {name}")))
      },
      &mut cleanup_warnings,
    );

    assert_eq!(attempted.len(), 37);
    assert_eq!(&attempted[..3], &["_aeordb_pre_gc_036", "_aeordb_pre_gc_035", "_aeordb_pre_gc_034"]);
    assert!(!attempted.iter().any(|name| ["_aeordb_pre_gc_039", "_aeordb_pre_gc_038", "_aeordb_pre_gc_037"].contains(&name.as_str())));
    assert_eq!(cleanup_warnings.len(), MAXIMUM_GC_CLEANUP_WARNINGS);
    assert!(cleanup_warnings.last().unwrap().contains("omitted"));
  }

  #[test]
  fn optional_gc_cleanup_warning_is_utf8_safe_and_byte_bounded() {
    let warning = bounded_gc_cleanup_warning("Lifecycle retention pruning failed", &"é".repeat(600));

    assert!(warning.len() <= 512);
    assert!(warning.ends_with("..."));
  }

  #[test]
  fn explicit_recheck_teardown_preserves_success_and_surfaces_recovered_cleanup_failure() {
    let (engine, _temporary) = create_test_engine();
    engine.begin_gc_recheck().unwrap();
    engine.poison_gc_recheck_for_test();
    let guard = GcRecheckGuard::active(&engine);

    let result = guard.finish(Ok(successful_result())).expect("optional teardown failure must preserve primary success");

    assert_eq!(result.cleanup_warnings.len(), 1);
    assert!(result.cleanup_warnings[0].contains("recheck"));
    engine.begin_gc_recheck().expect("recovered teardown must leave later GC usable");
    engine.end_gc_recheck().unwrap();
  }

  #[test]
  fn explicit_recheck_teardown_preserves_the_primary_error() {
    let (engine, _temporary) = create_test_engine();
    engine.begin_gc_recheck().unwrap();
    engine.poison_gc_recheck_for_test();
    let guard = GcRecheckGuard::active(&engine);

    let error =
      guard.finish(Err(EngineError::Cancelled("primary gc failure".to_string()))).expect_err("the primary GC error must remain terminal");

    assert!(matches!(error, EngineError::Cancelled(message) if message == "primary gc failure"));
    engine.begin_gc_recheck().expect("failed primary plus recovered teardown must leave later GC usable");
    engine.end_gc_recheck().unwrap();
  }

  #[test]
  fn complete_gc_run_returns_and_emits_a_recovered_teardown_warning() {
    let (engine, _temporary) = create_test_engine();
    let event_bus = Arc::new(EventBus::new());
    let mut events = event_bus.subscribe();
    let context = RequestContext::with_bus(event_bus);
    DirectoryOps::new(&engine).store_file_buffered(&context, "/gc/live.txt", b"live", Some("text/plain")).unwrap();

    let result = run_gc_internal(&engine, &context, false, None, || {}, || engine.poison_gc_recheck_for_test())
      .expect("the primary GC result must survive recovered teardown failure");

    assert_eq!(result.cleanup_warnings.len(), 1);
    let completed = std::iter::from_fn(|| events.try_recv().ok())
      .find(|event| event.event_type == EVENT_GC_COMPLETED)
      .expect("GC completion event must follow explicit teardown");
    assert_eq!(completed.payload["cleanup_warnings"].as_array().map(Vec::len), Some(1));
    engine.begin_gc_recheck().expect("the completed run must leave later GC usable");
    engine.end_gc_recheck().unwrap();
  }
}

fn check_gc_quantum(engine: &StorageEngine, cancellation: Option<&CancellationToken>, index: usize) -> EngineResult<()> {
  if index % 256 == 0 {
    check_gc_cancellation(engine, cancellation)?;
  }
  Ok(())
}

fn reserve_gc_workspace(engine: &StorageEngine, run_configuration: &GcRunConfiguration) -> EngineResult<MemoryReservation> {
  let mut reservation = engine
    .memory_coordinator()
    .reserve(MemoryOwner::GarbageCollection, GC_ADMISSION_BYTES, AdmissionClass::Maintenance)
    .map_err(gc_memory_error)?;
  let kv_entries = u64::try_from(engine.kv_entry_count()?)
    .map_err(|_| EngineError::ResourceExhausted("garbage collection KV population does not fit memory accounting".to_string()))?;
  let retained_bytes = kv_entries
    .checked_mul(GC_RETAINED_BYTES_PER_KV_ENTRY)
    .ok_or_else(|| EngineError::ResourceExhausted("garbage collection retained workspace estimate overflow".to_string()))?;
  if retained_bytes > run_configuration.mark_memory_preferred_bytes {
    tracing::warn!(
      required_bytes = retained_bytes,
      preferred_bytes = run_configuration.mark_memory_preferred_bytes,
      configuration_generation = run_configuration.generation,
      "Legacy in-memory GC mark exceeds the captured preferred memory budget; the v4 bounded mark pipeline will replace this path"
    );
  }
  reservation.grow(retained_bytes).map_err(gc_memory_error)?;
  Ok(reservation)
}

fn gc_memory_error(error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => {
      EngineError::ResourceExhausted(format!("garbage collection memory admission failed: {error}"))
    }
    _ => EngineError::IoError(std::io::Error::other(format!("garbage collection memory admission failed: {error}"))),
  }
}

/// Build an authoritative CountersSnapshot by scanning the current KV state.
/// Used by GC to reconcile counters after sweep.
fn build_authoritative_snapshot(engine: &StorageEngine) -> EngineResult<CountersSnapshot> {
  let all_entries = engine.iter_kv_entries()?;
  let hash_length = engine.hash_algo().hash_length();

  let mut symlinks: u64 = 0;
  let mut chunks: u64 = 0;
  let mut snapshots: u64 = 0;
  let mut forks: u64 = 0;
  let mut chunk_data_size: u64 = 0;

  for entry in &all_entries {
    match entry.entry_type() {
      KV_TYPE_FILE_RECORD => {}
      KV_TYPE_DIRECTORY => {}
      KV_TYPE_SYMLINK => {
        symlinks += 1;
      }
      KV_TYPE_CHUNK => {
        chunks += 1;
        chunk_data_size = chunk_data_size.saturating_add(estimated_chunk_payload_bytes(entry, hash_length));
      }
      KV_TYPE_SNAPSHOT => {
        snapshots += 1;
      }
      KV_TYPE_FORK => {
        forks += 1;
      }
      _ => {}
    }
  }

  let live_tree = crate::engine::directory_listing::measure_live_tree(engine)?;
  let void_space = engine
    .void_manager
    .read()
    .map_err(|error| EngineError::IoError(std::io::Error::other(format!("GC counter reconciliation could not read void state: {error}"))))?
    .total_void_space();

  // Preserve current throughput counters (they are monotonic, not reconciled)
  let current = engine.counters().snapshot();

  Ok(CountersSnapshot {
    files: live_tree.files,
    directories: live_tree.directories,
    symlinks,
    chunks,
    snapshots,
    forks,
    logical_data_size: live_tree.logical_data_size,
    chunk_data_size,
    void_space,
    writes_total: current.writes_total,
    reads_total: current.reads_total,
    bytes_written_total: current.bytes_written_total,
    bytes_read_total: current.bytes_read_total,
    chunks_deduped_total: current.chunks_deduped_total,
    write_buffer_depth: current.write_buffer_depth,
    void_count: current.void_count,
  })
}
