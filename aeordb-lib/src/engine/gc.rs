use std::collections::HashSet;

use crate::engine::btree::{BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::engine_event::{EVENT_GC_COMPLETED, EVENT_GC_STARTED};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::file_record::FileRecord;
use crate::engine::engine_counters::CountersSnapshot;
use crate::engine::kv_store::{
  KVEntry, KV_TYPE_DELETION, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_CHUNK, KV_TYPE_SNAPSHOT, KV_TYPE_FORK, KV_TYPE_SYMLINK,
};
use crate::engine::request_context::RequestContext;
use crate::engine::rss_sampler::PhaseSampler;
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
  for snapshot in &snapshots {
    roots.push((snapshot.root_hash.clone(), "/".to_string()));
  }
  let forks = vm.list_forks()?;
  for fork in &forks {
    roots.push((fork.root_hash.clone(), "/".to_string()));
  }

  if timing {
    eprintln!("[gc-timing] mark: {} roots ({} snapshots + {} forks + HEAD)", roots.len(), snapshots.len(), forks.len());
  }

  let bfs_start = std::time::Instant::now();
  let bfs_mem = PhaseSampler::start("mark.bfs", std::time::Duration::from_millis(50));
  walk_versions_bfs(engine, roots, hash_length, &mut live)?;
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

  // Mark system table entries as live
  let sys_start = std::time::Instant::now();
  mark_system_entries(engine, hash_length, &mut live)?;
  if timing {
    eprintln!("[gc-timing] mark.system: {:?}", sys_start.elapsed());
  }

  // Mark task queue entries as live -- task records use deterministic hashes
  // ("::aeordb:task:{id}") that are NOT in the directory tree, so
  // mark_system_entries does not cover them.
  let task_start = std::time::Instant::now();
  mark_task_entries(engine, &mut live)?;
  if timing {
    eprintln!("[gc-timing] mark.tasks: {:?}", task_start.elapsed());
  }

  let all_entries = engine.iter_kv_entries()?;

  // Mark current path-key FileRecords as live even if HEAD temporarily
  // diverged from the path index. User-facing reads resolve `file:{path}`
  // directly, so sweeping chunks referenced by a live path-key record creates
  // a dangling file that still appears readable until chunk lookup fails.
  let path_file_start = std::time::Instant::now();
  let path_file_count = mark_live_path_file_records(engine, hash_length, &mut live, &all_entries)?;
  if timing {
    eprintln!("[gc-timing] mark.path-files: {:?} (path_records={})", path_file_start.elapsed(), path_file_count);
  }

  // Mark DeletionRecord entries as live — they are needed for KV rebuild
  // from a full .aeordb scan (deletion replay) and must not be swept.
  let del_start = std::time::Instant::now();
  let del_mem = PhaseSampler::start("mark.deletion-pass", std::time::Duration::from_millis(50));
  let mut deletion_count = 0usize;
  for entry in &all_entries {
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
) -> EngineResult<usize> {
  let algo = engine.hash_algo();
  let mut marked = 0usize;

  for entry in all_entries {
    if entry.entry_type() != KV_TYPE_FILE_RECORD {
      continue;
    }

    let Some((header, _key, value)) = engine.get_entry(&entry.hash)? else {
      continue;
    };
    let Ok(file_record) = FileRecord::deserialize(&value, hash_length, header.entry_version) else {
      continue;
    };
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
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  let algo = engine.hash_algo();
  let timing = std::env::var("AEORDB_GC_TIMING").is_ok();
  let mut frontier = roots;
  let mut level = 0u32;
  let mut total_reads = 0u64;
  let mut total_leaves_skipped = 0u64;

  while !frontier.is_empty() {
    let frontier_size = frontier.len();
    // Stage 1: dedup, mark path-keys for directories, fold in leaf-only entries.
    // Survivors need a disk read; collect them with their KV offset.
    let mut to_read: Vec<(Vec<u8>, String, u64)> = Vec::with_capacity(frontier.len());
    let mut visited_dups = 0u64;
    let mut leaves_skipped = 0u64;
    let mut not_in_kv = 0u64;
    let dedup_start = std::time::Instant::now();
    for (hash, path) in frontier.drain(..) {
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
      match engine.get_kv_entry(&hash) {
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
          not_in_kv += 1;
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
    for (hash, path, _offset) in to_read {
      let entry = match engine.get_entry_including_deleted(&hash)? {
        Some(e) => e,
        None => continue,
      };
      let (header, _key, value) = entry;

      // Follow hard-link: if value is exactly a content hash, dereference.
      let value = if value.len() == hash_length {
        live.insert(value.clone());
        match engine.get_entry_including_deleted(&value)? {
          Some((_h, _k, v)) => v,
          None => continue,
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
            collect_btree_children(engine, &value, hash_length, live)?
          } else {
            deserialize_child_entries(&value, hash_length, header.entry_version)?
          };

          for child in &children {
            let child_path = if path == "/" { format!("/{}", child.name) } else { format!("{}/{}", path, child.name) };
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
) -> EngineResult<Vec<ChildEntry>> {
  // TODO: thread the surrounding EntryHeader's entry_version through collect_btree_children
  // when a v1 BTreeNode format ships. Today everything on disk is v0.
  let node = BTreeNode::deserialize(node_data, hash_length, 0)?;
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
///
/// System data (`/.aeordb-system/...`, `/.aeordb-config/...`) lives outside
/// the user-visible tree, so `walk_versions_bfs` never sees it. We walk it
/// here with the same path-aware logic so we mark every key the engine
/// might use to reach an entry — identity, path, and content — not just
/// the merkle hash. Missing any of those silently breaks `JsonStore.get`
/// after the next sweep, which is how every api-key / user / group
/// disappeared on the prior GC run.
fn mark_system_entries(engine: &StorageEngine, hash_length: usize, live: &mut HashSet<Vec<u8>>) -> EngineResult<()> {
  let system_prefixes = ["/.aeordb-system", "/.aeordb-config"];

  for prefix in &system_prefixes {
    let dir_hash = engine.compute_hash(format!("dir:{}", prefix).as_bytes())?;
    if engine.get_entry_including_deleted(&dir_hash)?.is_some() {
      mark_entry_recursive(engine, &dir_hash, prefix, hash_length, live)?;
    }
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
/// `path` is the absolute path of the entry being marked (e.g.
/// `"/.aeordb-system/api-keys/abc"`). We need it because directories and
/// symlinks don't carry their own path in the stored value, and files use
/// `file_path_hash(path)` rather than the identity/content hash for path
/// lookups.
fn mark_entry_recursive(
  engine: &StorageEngine,
  hash: &[u8],
  path: &str,
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
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
      return Ok(());
    }
  };

  let (header, _key, value) = entry;
  let algo = engine.hash_algo();

  // Follow hard link: if value is exactly a content hash, dereference and
  // mark the content entry too, then use its payload as the working value.
  let value = if value.len() == hash_length {
    live.insert(value.clone());
    match engine.get_entry_including_deleted(&value)? {
      Some((_h, _k, v)) => v,
      None => return Ok(()),
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
        collect_btree_children(engine, &value, hash_length, live)?
      } else {
        deserialize_child_entries(&value, hash_length, header.entry_version)?
      };
      for child in &children {
        let child_path = if path == "/" { format!("/{}", child.name) } else { format!("{}/{}", path, child.name) };
        mark_entry_recursive(engine, &child.hash, &child_path, hash_length, live)?;
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
fn mark_task_entries(engine: &StorageEngine, live: &mut HashSet<Vec<u8>>) -> EngineResult<()> {
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
pub fn gc_sweep(engine: &StorageEngine, live: &HashSet<Vec<u8>>, dry_run: bool) -> EngineResult<(usize, u64)> {
  let timing = std::env::var("AEORDB_GC_TIMING").is_ok();
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
  for (hash, offset, entry_size) in &garbage_candidates {
    match engine.get_kv_entry(hash) {
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

  // Drop the verified hashes from the live KV index. All in-memory; no WAL
  // writes from sweep itself — the durability of these deletions comes from
  // the hot tail flush that follows (which carries the void snapshot, and
  // by the void offsets implies the entries at those offsets are gone).
  let kv_remove_start = std::time::Instant::now();
  if !verified_hashes.is_empty() {
    engine.remove_kv_entries_batch(&verified_hashes)?;
  }
  let kv_remove_elapsed = kv_remove_start.elapsed();

  // Register the freed regions with VoidManager (in-memory). On the next
  // hot tail flush these get mirrored to disk as VoidRecords.
  let void_register_start = std::time::Instant::now();
  if !freed_regions.is_empty() {
    if let Ok(mut vm) = engine.void_manager.write() {
      for (offset, size) in &freed_regions {
        vm.register_void(*offset, *size);
      }
    }
  }
  let void_register_elapsed = void_register_start.elapsed();

  // Sync void state into the kv_writer's pending_voids and force a hot tail
  // flush so the new void set is durable. One sequential write at the WAL
  // tail; one fsync. Fast on slow disks.
  let flush_start = std::time::Instant::now();
  engine.sync_voids_to_kv_writer();
  if let Err(e) = engine.force_hot_tail_flush() {
    tracing::warn!("Hot tail flush after GC sweep failed: {}", e);
  }
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
  let start = std::time::Instant::now();

  // Emit GC started event
  ctx.emit(
    EVENT_GC_STARTED,
    serde_json::json!({
      "dry_run": dry_run,
    }),
  );

  // Begin GC recheck tracking before any version-forest reads. From this
  // point on, every successful write hash is recorded so the sweep phase can
  // spare entries that arrived after the mark snapshot was captured. See
  // bot-docs/plan/gc-mark-sweep.md. The RAII guard ensures we always call
  // end_gc_recheck on exit, even on `?`-propagated errors.
  struct RecheckGuard<'a>(&'a StorageEngine, bool);
  impl<'a> Drop for RecheckGuard<'a> {
    fn drop(&mut self) {
      if self.1 {
        self.0.end_gc_recheck();
      }
    }
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
      let mut pre_gc_snapshots: Vec<String> =
        snapshots.iter().filter(|s| s.name.starts_with("_aeordb_pre_gc_")).map(|s| s.name.clone()).collect();
      pre_gc_snapshots.sort();
      pre_gc_snapshots.reverse(); // newest first (timestamp suffix sorts lexicographically)

      for old_name in pre_gc_snapshots.iter().skip(3) {
        if let Err(e) = vm.delete_snapshot(ctx, old_name) {
          tracing::warn!("Failed to delete old pre-GC snapshot {}: {}", old_name, e);
        }
      }
    }

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
      Err(e) => tracing::warn!("Lifecycle retention pruning failed: {}", e),
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
  let mut live = gc_mark(engine)?;
  mark_mem.finish();

  // Re-check drain: any entry that was written during the mark phase is now in
  // the recheck set. Walk each one and union into `live` so the sweep doesn't
  // clobber freshly-written data. Loop until the queue is empty for one pass.
  if !dry_run {
    let drain_mem = PhaseSampler::start("recheck-drain", std::time::Duration::from_millis(50));
    loop {
      let pending = engine.take_gc_recheck();
      if pending.is_empty() {
        break;
      }
      let hash_length = engine.hash_algo().hash_length();
      for hash in pending {
        // Path is unknown for recheck entries — the writer recorded raw hashes
        // only. Every key it wrote (identity, file-path, content) is in the
        // recheck set independently, so they each get marked when their hash
        // shows up in this loop. The empty path means path-derived keys
        // (dir:{path}, file:{path}) computed inside the recursion are wrong,
        // but harmless: the live set is "do not sweep" — extra hashes in it
        // never match a real entry and are simply ignored.
        mark_entry_recursive(engine, &hash, "", hash_length, &mut live)?;
      }
    }
    drain_mem.finish();
  }

  let live_entries = live.len();

  // The RAII guard above calls end_gc_recheck on scope exit so failure paths
  // don't leave recheck recording on indefinitely.
  let sweep_start = std::time::Instant::now();
  let sweep_mem = PhaseSampler::start("sweep", std::time::Duration::from_millis(50));
  let (garbage_entries, reclaimed_bytes) = gc_sweep(engine, &live, dry_run)?;
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

  let result = GcResult { versions_scanned, live_entries, garbage_entries, reclaimed_bytes, duration_ms, dry_run };

  // Emit GC event
  ctx.emit(
    EVENT_GC_COMPLETED,
    serde_json::json!({
      "versions_scanned": result.versions_scanned,
      "live_entries": result.live_entries,
      "garbage_entries": result.garbage_entries,
      "reclaimed_bytes": result.reclaimed_bytes,
      "duration_ms": result.duration_ms,
      "dry_run": result.dry_run,
    }),
  );

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
        if let Ok(Some((header, _key, value))) = engine.get_entry(&entry.hash) {
          if let Ok(record) = FileRecord::deserialize(&value, hash_length, header.entry_version) {
            logical_data_size += record.total_size;
          }
        }
      }
      KV_TYPE_DIRECTORY => {
        directories += 1;
      }
      KV_TYPE_SYMLINK => {
        symlinks += 1;
      }
      KV_TYPE_CHUNK => {
        chunks += 1;
        if let Ok(Some((_header, _key, value))) = engine.get_entry(&entry.hash) {
          chunk_data_size += value.len() as u64;
        }
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

  let void_space = if let Ok(vm) = engine.void_manager.read() { vm.total_void_space() } else { 0 };

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
    void_count: current.void_count,
  })
}
