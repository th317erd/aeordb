use std::collections::HashSet;

use crate::engine::btree::{BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::engine_event::EVENT_GC_COMPLETED;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::file_record::FileRecord;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::version_manager::VersionManager;

use serde::Serialize;

/// Result of a GC run.
#[derive(Debug, Clone, Serialize)]
pub struct GcResult {
  pub versions_scanned: usize,
  pub live_entries: usize,
  pub garbage_entries: usize,
  pub reclaimed_bytes: u64,
  pub duration_ms: u64,
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

  let entry = match engine.get_entry(root_hash)? {
    Some(entry) => entry,
    None => return Ok(()),
  };

  let (header, _key, value) = entry;

  match header.entry_type {
    EntryType::DirectoryIndex => {
      if value.is_empty() {
        return Ok(());
      }
      let children = if is_btree_format(&value) {
        collect_btree_children(engine, &value, hash_length, live)?
      } else {
        deserialize_child_entries(&value, hash_length)?
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
          _ => {
            live.insert(child.hash.clone());
          }
        }
      }
    }
    _ => {}
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

  if let Some((_header, _key, value)) = engine.get_entry(file_hash)? {
    let file_record = FileRecord::deserialize(&value, hash_length)?;
    for chunk_hash in &file_record.chunk_hashes {
      live.insert(chunk_hash.clone());
    }

    // Also mark the path-based key as live (mutable index for reads/indexing)
    let algo = engine.hash_algo();
    let path_key = crate::engine::directory_ops::file_path_hash(&file_record.path, &algo)?;
    live.insert(path_key);
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
        if let Some((_header, _key, child_data)) = engine.get_entry(child_hash)? {
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
  let system_prefixes = ["/.system", "/.config"];

  for prefix in &system_prefixes {
    let dir_hash = engine.compute_hash(format!("dir:{}", prefix).as_bytes())?;
    if let Some((_header, _key, value)) = engine.get_entry(&dir_hash)? {
      live.insert(dir_hash);
      if !value.is_empty() {
        if is_btree_format(&value) {
          let children = collect_btree_children(engine, &value, hash_length, live)?;
          for child in &children {
            mark_entry_recursive(engine, &child.hash, hash_length, live)?;
          }
        } else {
          let children = deserialize_child_entries(&value, hash_length)?;
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

  let entry = match engine.get_entry(hash)? {
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
          let children = deserialize_child_entries(&value, hash_length)?;
          for child in &children {
            mark_entry_recursive(engine, &child.hash, hash_length, live)?;
          }
        }
      }
    }
    EntryType::FileRecord => {
      let file_record = FileRecord::deserialize(&value, hash_length)?;
      for chunk_hash in &file_record.chunk_hashes {
        live.insert(chunk_hash.clone());
      }
    }
    _ => {}
  }

  Ok(())
}

/// Minimum DeletionRecord entry size for the given engine's hash algorithm.
fn min_deletion_size(engine: &StorageEngine) -> u32 {
  // DeletionRecord with path="gc", reason="gc":
  // value = u16(2) + "gc"(2) + i64(8) + u16(2) + "gc"(2) = 16 bytes
  // key = hash_length bytes (computed hash)
  let hash_length = engine.hash_algo().hash_length();
  EntryHeader::compute_total_length(engine.hash_algo(), hash_length as u32, 16)
}

/// Minimum void entry size.
fn min_void_size(engine: &StorageEngine) -> u32 {
  EntryHeader::compute_total_length(engine.hash_algo(), 0, 0)
}

/// Sweep phase: iterate all KV entries, overwrite non-live entries in-place.
pub fn gc_sweep(
  engine: &StorageEngine,
  live: &HashSet<Vec<u8>>,
  dry_run: bool,
) -> EngineResult<(usize, u64)> {
  let min_del = min_deletion_size(engine);
  let min_void = min_void_size(engine);

  let all_entries = engine.iter_kv_entries()?;

  let mut garbage_count: usize = 0;
  let mut reclaimed_bytes: u64 = 0;

  for entry in &all_entries {
    if live.contains(&entry.hash) {
      continue;
    }

    garbage_count += 1;

    let header = engine.read_entry_header_at(entry.offset)?;
    let entry_size = header.total_length;
    reclaimed_bytes += entry_size as u64;

    if dry_run {
      continue;
    }

    // Best-effort in-place overwrite
    if entry_size >= min_del {
      let written = engine.write_deletion_at(entry.offset, "gc")?;
      let remaining = entry_size - written;
      if remaining >= min_void {
        let void_offset = entry.offset + written as u64;
        engine.write_void_at(void_offset, remaining)?;
      }
    }

    engine.remove_kv_entry(&entry.hash)?;
  }

  Ok((garbage_count, reclaimed_bytes))
}

/// Run a complete GC cycle: mark + sweep.
pub fn run_gc(
  engine: &StorageEngine,
  ctx: &RequestContext,
  dry_run: bool,
) -> EngineResult<GcResult> {
  let start = std::time::Instant::now();

  let vm = VersionManager::new(engine);
  let snapshot_count = vm.list_snapshots()?.len();
  let fork_count = vm.list_forks()?.len();
  let versions_scanned = 1 + snapshot_count + fork_count;

  let live = gc_mark(engine)?;
  let live_entries = live.len();

  let (garbage_entries, reclaimed_bytes) = gc_sweep(engine, &live, dry_run)?;

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
