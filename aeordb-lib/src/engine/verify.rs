//! Database integrity verification.
//!
//! Scans the append log, verifies entry hashes, checks directory consistency,
//! validates KV index, and produces a structured report.

use std::collections::{HashMap, HashSet};
use std::fs::File;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::entry_type::EntryType;
use crate::engine::file_header::read_active_header;
use crate::engine::hot_tail;
use crate::engine::kv_store::{KV_TYPE_DIRECTORY, KV_TYPE_VOID};
use crate::engine::storage_engine::StorageEngine;

#[derive(Debug, Clone, Default)]
struct ExpectedKvEntry {
  offset: u64,
  total_length: u32,
  value_length: u32,
  kv_type: u8,
  timestamp: i64,
}

#[derive(Debug, Clone, Default)]
struct ExpectedKvIndex {
  entries: HashMap<Vec<u8>, ExpectedKvEntry>,
  deletion_records: Vec<(String, i64, u64)>,
}

/// Result of a full database integrity check.
#[derive(Debug, Clone)]
pub struct VerifyReport {
  // Database info
  pub db_path: String,
  pub file_size: u64,
  pub hash_algorithm: String,

  // Entry counts by type
  pub total_entries: u64,
  pub chunks: u64,
  pub file_records: u64,
  pub directory_indexes: u64,
  pub symlinks: u64,
  pub snapshots: u64,
  pub deletion_records: u64,
  pub forks: u64,
  pub voids: u64,
  pub void_bytes: u64,

  // Storage metrics
  pub logical_data_size: u64,
  pub chunk_data_size: u64,
  pub dedup_savings: u64,

  // Integrity
  pub valid_entries: u64,
  pub corrupt_hash: u64,
  pub corrupt_header: u64,
  pub skipped_regions: Vec<(u64, u64)>, // (offset, length)

  // Directory consistency
  pub directories_checked: u64,
  pub missing_children: Vec<String>, // paths where child doesn't exist
  pub unlisted_files: Vec<String>,   // files that exist but aren't in parent dir

  // KV index
  pub kv_entries: u64,
  pub stale_kv_entries: u64,
  pub missing_kv_entries: u64,
  pub stale_kv_details: Vec<String>,
  pub missing_kv_details: Vec<String>,
  pub invalid_kv_offsets: Vec<String>,
  pub invalid_hot_tail_voids: Vec<String>,

  /// Directories whose `dir:{path}` entry hard-links to a content hash
  /// that's been swept by GC. The directory is reachable through its
  /// parent's ChildEntry but a direct `list_directory` would fail
  /// without the runtime recovery fallback in `read_directory_data`.
  /// Repair rewrites the path-key to point at the merkle-canonical
  /// content hash. Known root cause: `snapshot_restore` and
  /// `fork_promote` move HEAD without rewriting dir_key entries.
  pub stale_dir_path_keys: Vec<String>,

  // Snapshot integrity
  pub snapshots_checked: u64,
  pub broken_snapshots: Vec<String>, // snapshot names with broken tree references

  // Issues found during repair (if --repair was used)
  pub repairs: Vec<String>,
}

impl VerifyReport {
  pub fn new(db_path: &str) -> Self {
    VerifyReport {
      db_path: db_path.to_string(),
      file_size: 0,
      hash_algorithm: String::new(),
      total_entries: 0,
      chunks: 0,
      file_records: 0,
      directory_indexes: 0,
      symlinks: 0,
      snapshots: 0,
      deletion_records: 0,
      forks: 0,
      voids: 0,
      void_bytes: 0,
      logical_data_size: 0,
      chunk_data_size: 0,
      dedup_savings: 0,
      valid_entries: 0,
      corrupt_hash: 0,
      corrupt_header: 0,
      skipped_regions: Vec::new(),
      directories_checked: 0,
      missing_children: Vec::new(),
      unlisted_files: Vec::new(),
      kv_entries: 0,
      stale_kv_entries: 0,
      missing_kv_entries: 0,
      stale_kv_details: Vec::new(),
      missing_kv_details: Vec::new(),
      invalid_kv_offsets: Vec::new(),
      invalid_hot_tail_voids: Vec::new(),
      stale_dir_path_keys: Vec::new(),
      snapshots_checked: 0,
      broken_snapshots: Vec::new(),
      repairs: Vec::new(),
    }
  }

  pub fn has_issues(&self) -> bool {
    self.corrupt_hash > 0
      || self.corrupt_header > 0
      || !self.missing_children.is_empty()
      || !self.unlisted_files.is_empty()
      || self.stale_kv_entries > 0
      || self.missing_kv_entries > 0
      || !self.invalid_kv_offsets.is_empty()
      || !self.invalid_hot_tail_voids.is_empty()
      || !self.broken_snapshots.is_empty()
      || !self.stale_dir_path_keys.is_empty()
  }
}

/// Run a full integrity check on the database.
pub fn verify(engine: &StorageEngine, db_path: &str) -> VerifyReport {
  let mut report = VerifyReport::new(db_path);

  // File size
  report.file_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
  report.hash_algorithm = format!("{:?}", engine.hash_algo());

  // Phase 1: Scan all entries from the append log. Also collects the
  // expected live KV hashes so check_kv_index doesn't need a second WAL pass.
  let mut expected = scan_entries(engine, &mut report);

  // Current hot-tail void records are durable tombstones for GC-swept WAL
  // ranges. The old bytes may still contain valid-looking entry headers, but
  // they are intentionally absent from the live KV index.
  let hot_tail_voids = check_hot_tail_voids(db_path, &mut report);
  remove_expected_entries_in_voids(&mut expected, &hot_tail_voids);

  // Phase 2: Check KV index consistency — uses the unique_hashes count
  // from Phase 1 (no WAL re-scan).
  check_kv_index(engine, &mut report, expected);

  // Phase 3: Check directory consistency
  check_directories(engine, &mut report);

  // Phase 4: Check snapshot tree integrity (detects GC damage)
  check_snapshot_integrity(engine, &mut report);

  report
}

/// Run verify with auto-repair (KV rebuild + directory tree rebuild).
///
/// For KV block expansion, use the CLI's verify command which handles
/// the engine drop/reopen cycle needed for WAL relocation.
pub fn verify_and_repair(engine: &StorageEngine, db_path: &str) -> VerifyReport {
  let mut report = verify(engine, db_path);

  // Repair 1: Rebuild KV if there are missing or stale entries
  if report.missing_kv_entries > 0 || report.stale_kv_entries > 0 {
    match engine.rebuild_kv() {
      Ok(()) => {
        report
          .repairs
          .push(format!("KV index rebuilt ({} missing + {} stale entries recovered)", report.missing_kv_entries, report.stale_kv_entries,));
      }
      Err(e) => {
        report.repairs.push(format!("KV rebuild failed: {}", e));
      }
    }
  }

  // Repair 2: Note corrupt entries
  if report.corrupt_hash > 0 || report.corrupt_header > 0 {
    report.repairs.push(format!(
      "Found {} corrupt entries ({} hash failures + {} header failures)",
      report.corrupt_hash + report.corrupt_header,
      report.corrupt_hash,
      report.corrupt_header,
    ));
  }

  // Repair 3: Rebuild directory tree
  if report.missing_kv_entries > 0 && report.file_records > 0 {
    let ops = DirectoryOps::new(engine);
    let ctx = crate::engine::request_context::RequestContext::system();
    match ops.rebuild_directory_tree(&ctx) {
      Ok(count) => {
        report.repairs.push(format!("Directory tree rebuilt ({} paths re-propagated)", count));
      }
      Err(e) => {
        report.repairs.push(format!("Directory tree rebuild failed: {}", e));
      }
    }
  }

  // Repair 4: Rewrite stale dir_key entries to point at the canonical
  // merkle-reachable content. Known cause: `snapshot_restore` and
  // `fork_promote` move HEAD without rewriting dir_keys; subsequent GC
  // sweeps the orphan content. Files under these dirs are unaffected;
  // only `list_directory` is broken. The runtime fallback in
  // `read_directory_data` masks the symptom, but this repair removes
  // it permanently and removes the warning-log churn.
  if !report.stale_dir_path_keys.is_empty() {
    let ops = DirectoryOps::new(engine);
    let mut repaired = 0usize;
    let mut failed = 0usize;
    // Clone to avoid borrow conflicts when we mutate report.repairs.
    let paths: Vec<String> = report.stale_dir_path_keys.clone();
    for path in &paths {
      match ops.repair_stale_dir_key(path) {
        Ok(true) => repaired += 1,
        Ok(false) => {}
        Err(_) => failed += 1,
      }
    }
    report.repairs.push(format!("Stale dir_keys rewritten: {} fixed, {} unfixable", repaired, failed,));
  }

  // Persist repairs to disk
  if !report.repairs.is_empty() {
    match engine.shutdown() {
      Ok(()) => {
        report.repairs.push("Repairs persisted to disk.".to_string());
      }
      Err(e) => {
        report.repairs.push(format!("Warning: failed to persist repairs: {}", e));
      }
    }
  }

  report
}

/// Scan the WAL, accumulating per-type counts, integrity counts, and
/// the set of KV hash keys expected to be live. Void entries are storage
/// bookkeeping, not user/content records, so they are counted in the storage
/// summary but excluded from live-KV completeness checks.
fn scan_entries(engine: &StorageEngine, report: &mut VerifyReport) -> ExpectedKvIndex {
  use crate::engine::errors::EngineError;

  // Use the writer to scan entries — reporting mode yields errors
  // for corrupt entries instead of silently skipping them.
  let writer = match engine.writer_read_lock() {
    Ok(w) => w,
    Err(_) => return ExpectedKvIndex::default(),
  };

  let mut expected = ExpectedKvIndex::default();

  if let Ok(mut scanner) = writer.scan_entries_reporting() {
    for result in scanner.by_ref() {
      match result {
        Ok(scanned) => {
          report.total_entries += 1;
          report.valid_entries += 1;

          match scanned.header.entry_type {
            EntryType::Chunk => {
              report.chunks += 1;
              report.chunk_data_size += scanned.value.len() as u64;
            }
            EntryType::FileRecord => {
              report.file_records += 1;
              report.logical_data_size += scanned.header.value_length as u64;
            }
            EntryType::DirectoryIndex => report.directory_indexes += 1,
            EntryType::Symlink => report.symlinks += 1,
            EntryType::Snapshot => report.snapshots += 1,
            EntryType::DeletionRecord => report.deletion_records += 1,
            EntryType::Fork => report.forks += 1,
            // Void WAL records are historical bookkeeping. The current
            // reusable-space state lives in the hot-tail void snapshot and is
            // counted in check_hot_tail_voids().
            EntryType::Void => {}
          }

          if scanned.header.entry_type == EntryType::DeletionRecord {
            if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(&scanned.value, scanned.header.entry_version) {
              expected.deletion_records.push((record.path, scanned.header.timestamp, scanned.offset));
            }
          }

          if scanned.header.entry_type != EntryType::Void {
            let candidate = ExpectedKvEntry {
              offset: scanned.offset,
              total_length: scanned.header.total_length,
              value_length: scanned.header.value_length,
              kv_type: scanned.header.entry_type.to_kv_type(),
              timestamp: scanned.header.timestamp,
            };
            let replace =
              expected.entries.get(&scanned.key).map(|existing| should_replace_expected_entry(existing, &candidate)).unwrap_or(true);
            if replace {
              expected.entries.insert(scanned.key, candidate);
            }
          }
        }
        Err(EngineError::CorruptEntry { ref reason, .. }) => {
          report.total_entries += 1;
          if reason.contains("Hash verification") {
            report.corrupt_hash += 1;
          } else {
            report.corrupt_header += 1;
          }
        }
        Err(_) => {
          report.total_entries += 1;
          report.corrupt_header += 1;
        }
      }
    }

    // Collect skipped regions from the scanner
    for (offset, length) in &scanner.skipped_regions {
      report.skipped_regions.push((*offset, *length as u64));
    }
  }

  report.dedup_savings = report.logical_data_size.saturating_sub(report.chunk_data_size);
  expected
}

fn should_replace_expected_entry(existing: &ExpectedKvEntry, candidate: &ExpectedKvEntry) -> bool {
  if existing.kv_type == KV_TYPE_DIRECTORY && candidate.kv_type == KV_TYPE_DIRECTORY {
    if candidate.value_length == 0 && existing.value_length > 0 {
      return false;
    }
    if candidate.value_length > 0 && existing.value_length == 0 {
      return true;
    }
  }

  (candidate.timestamp, candidate.offset) > (existing.timestamp, existing.offset)
}

fn check_kv_index(engine: &StorageEngine, report: &mut VerifyReport, mut expected: ExpectedKvIndex) {
  apply_deletion_records(engine, &mut expected);

  // Count KV entries from snapshot
  let snapshot = engine.kv_snapshot.load();
  if let Ok(entries) = snapshot.iter_all() {
    let live_entries: Vec<_> = entries.into_iter().filter(|entry| entry.entry_type() != KV_TYPE_VOID).collect();
    report.kv_entries = live_entries.len() as u64;
    if let Ok(writer) = engine.writer_read_lock() {
      let header = writer.file_header();
      let wal_start = header.kv_block_offset.saturating_add(header.kv_block_length);
      let wal_end = writer.current_offset();
      for entry in &live_entries {
        if !StorageEngine::valid_reusable_range(entry.offset, entry.total_length, wal_start, wal_end) {
          report.invalid_kv_offsets.push(format!(
            "hash {} offset {} length {} outside WAL region {}..{}",
            hex::encode(&entry.hash[..8.min(entry.hash.len())]),
            entry.offset,
            entry.total_length,
            wal_start,
            wal_end
          ));
        }
      }
    }

    let expected_hashes: HashSet<Vec<u8>> = expected.entries.keys().cloned().collect();
    let mut actual_by_hash = HashMap::with_capacity(live_entries.len());
    for entry in live_entries {
      actual_by_hash.insert(entry.hash.clone(), entry);
    }
    let actual_hashes: HashSet<Vec<u8>> = actual_by_hash.keys().cloned().collect();

    report.missing_kv_entries = expected_hashes.difference(&actual_hashes).count() as u64;
    report.stale_kv_entries = actual_hashes.difference(&expected_hashes).count() as u64;
    for hash in expected_hashes.difference(&actual_hashes).take(20) {
      if let Some(entry) = expected.entries.get(hash) {
        report.missing_kv_details.push(format!(
          "hash {} offset {} length {}",
          hex::encode(&hash[..8.min(hash.len())]),
          entry.offset,
          entry.total_length
        ));
      }
    }
    for hash in actual_hashes.difference(&expected_hashes).take(20) {
      if let Some(entry) = actual_by_hash.get(hash) {
        report.stale_kv_details.push(format!(
          "hash {} type_flags=0x{:02x} offset {} length {}",
          hex::encode(&hash[..8.min(hash.len())]),
          entry.type_flags,
          entry.offset,
          entry.total_length
        ));
      }
    }
  }
}

fn apply_deletion_records(engine: &StorageEngine, expected: &mut ExpectedKvIndex) {
  let hash_algo = engine.hash_algo();
  for (path, deletion_timestamp, deletion_offset) in expected.deletion_records.clone() {
    let normalized = crate::engine::path_utils::normalize_path(&path);

    if let Ok(file_key) = crate::engine::directory_ops::file_path_hash(&normalized, &hash_algo) {
      remove_expected_if_older(expected, &file_key, deletion_timestamp, deletion_offset);
    }
    if let Ok(dir_key) = crate::engine::directory_ops::directory_path_hash(&normalized, &hash_algo) {
      remove_expected_if_older(expected, &dir_key, deletion_timestamp, deletion_offset);
    }
    if let Ok(symlink_key) = crate::engine::symlink_record::symlink_path_hash(&normalized, &hash_algo) {
      remove_expected_if_older(expected, &symlink_key, deletion_timestamp, deletion_offset);
    }
    if let Ok(raw_key) = hash_algo.compute_hash(path.as_bytes()) {
      remove_expected_if_older(expected, &raw_key, deletion_timestamp, deletion_offset);
    }
  }
}

fn remove_expected_if_older(expected: &mut ExpectedKvIndex, key: &[u8], deletion_timestamp: i64, deletion_offset: u64) {
  if expected.entries.get(key).map(|entry| (deletion_timestamp, deletion_offset) > (entry.timestamp, entry.offset)).unwrap_or(false) {
    expected.entries.remove(key);
  }
}

fn remove_expected_entries_in_voids(expected: &mut ExpectedKvIndex, voids: &[hot_tail::VoidRecord]) {
  if voids.is_empty() {
    return;
  }

  expected.entries.retain(|_hash, entry| {
    let entry_end = entry.offset.saturating_add(entry.total_length as u64);
    !voids.iter().any(|void| {
      let void_end = void.offset.saturating_add(void.size as u64);
      entry.offset < void_end && void.offset < entry_end
    })
  });
}

fn check_hot_tail_voids(db_path: &str, report: &mut VerifyReport) -> Vec<hot_tail::VoidRecord> {
  let mut file = match File::open(db_path) {
    Ok(file) => file,
    Err(_) => return Vec::new(),
  };
  let (header, _) = match read_active_header(&mut file) {
    Ok(header) => header,
    Err(_) => return Vec::new(),
  };
  if header.hot_tail_offset == 0 {
    return Vec::new();
  }

  let hash_length = header.hash_algo.hash_length();
  let wal_start = header.kv_block_offset.saturating_add(header.kv_block_length);
  let hot_tail_offset = header.hot_tail_offset;
  let payload = match hot_tail::read_hot_tail(&mut file, hot_tail_offset, hash_length) {
    Some(payload) => payload,
    None => return Vec::new(),
  };

  let mut valid_voids = Vec::new();
  for (index, void) in payload.voids.iter().enumerate() {
    report.voids += 1;
    report.void_bytes += void.size as u64;
    if !StorageEngine::valid_reusable_range(void.offset, void.size, wal_start, hot_tail_offset) {
      report
        .invalid_hot_tail_voids
        .push(format!("void #{} offset {} length {} outside WAL region {}..{}", index, void.offset, void.size, wal_start, hot_tail_offset));
    } else {
      valid_voids.push(*void);
    }
  }
  valid_voids
}

fn check_directories(engine: &StorageEngine, report: &mut VerifyReport) {
  let ops = DirectoryOps::new(engine);

  // List root directory and recursively check all children
  check_directory_recursive(&ops, engine, "/", report, 0);
}

fn check_directory_recursive(ops: &DirectoryOps, engine: &StorageEngine, path: &str, report: &mut VerifyReport, depth: usize) {
  // Limit recursion depth to prevent infinite loops on corrupt directory cycles
  if depth > 100 {
    return;
  }

  report.directories_checked += 1;

  // Detect stale dir_key (hard-link target dead, recoverable via parent
  // walk). `recover_directory_data_if_stale` returns Some only when the
  // path-key entry is a hard-link AND its target is missing from LIVE
  // AND we can reach the canonical content via HEAD's merkle walk.
  {
    let algo = engine.hash_algo();
    if let Ok(dir_key) = crate::engine::directory_ops::directory_path_hash(path, &algo) {
      if let Ok(Some(_)) = ops.recover_directory_data_if_stale(path, &dir_key) {
        report.stale_dir_path_keys.push(path.to_string());
      }
    }
  }

  match ops.list_directory(path) {
    Ok(children) => {
      for child in &children {
        let child_path = if path == "/" { format!("/{}", child.name) } else { format!("{}/{}", path.trim_end_matches('/'), child.name) };

        if child.entry_type == EntryType::DirectoryIndex.to_u8() {
          // Recurse into subdirectories
          check_directory_recursive(ops, engine, &child_path, report, depth + 1);
        } else if child.entry_type == EntryType::FileRecord.to_u8() {
          // Verify file record is readable
          match ops.read_file_buffered(&child_path) {
            Ok(_) => {}
            Err(e) => {
              report.missing_children.push(format!("{} ({})", child_path, e));
            }
          }
        }
      }
    }
    Err(_) => {
      // Directory itself doesn't exist or is corrupt
    }
  }
}

/// Phase 4: Walk each snapshot's directory tree and verify all entries
/// are reachable. Detects damage from GC sweeping snapshot-referenced data.
fn check_snapshot_integrity(engine: &StorageEngine, report: &mut VerifyReport) {
  use crate::engine::version_manager::VersionManager;

  let vm = VersionManager::new(engine);
  let snapshots = match vm.list_snapshots() {
    Ok(s) => s,
    Err(_) => return,
  };

  let hash_length = engine.hash_algo().hash_length();

  for snapshot in &snapshots {
    report.snapshots_checked += 1;

    let mut missing = Vec::new();
    walk_snapshot_tree(engine, &snapshot.root_hash, "/", hash_length, &mut missing, 0);

    if !missing.is_empty() {
      report.broken_snapshots.push(format!(
        "{} (id: {}): {} broken references — {}",
        snapshot.name,
        hex::encode(&snapshot.root_hash),
        missing.len(),
        missing.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
      ));
    }
  }
}

/// Recursively walk a snapshot's directory tree, collecting paths where
/// entries are missing (GC damage or corruption).
fn walk_snapshot_tree(
  engine: &StorageEngine,
  root_hash: &[u8],
  dir_path: &str,
  hash_length: usize,
  missing: &mut Vec<String>,
  depth: usize,
) {
  if depth > 100 {
    return;
  }

  let value = match engine.get_entry_including_deleted(root_hash) {
    Ok(Some((_header, _key, value))) => value,
    Ok(None) => {
      missing.push(format!("{} (dir entry missing)", dir_path));
      return;
    }
    Err(_) => return,
  };

  if value.is_empty() {
    return;
  }

  let children = if crate::engine::btree::is_btree_format(&value) {
    match crate::engine::btree::btree_list_from_node(&value, engine, hash_length, true) {
      Ok(c) => c,
      Err(_) => {
        missing.push(format!("{} (corrupt btree)", dir_path));
        return;
      }
    }
  } else {
    match crate::engine::directory_entry::deserialize_child_entries(&value, hash_length, 0) {
      Ok(c) => c,
      Err(_) => {
        missing.push(format!("{} (corrupt flat index)", dir_path));
        return;
      }
    }
  };

  for child in &children {
    let child_path = if dir_path == "/" { format!("/{}", child.name) } else { format!("{}/{}", dir_path, child.name) };

    let child_type = crate::engine::entry_type::EntryType::from_u8(child.entry_type);
    match child_type {
      Ok(crate::engine::entry_type::EntryType::DirectoryIndex) => {
        walk_snapshot_tree(engine, &child.hash, &child_path, hash_length, missing, depth + 1);
      }
      Ok(crate::engine::entry_type::EntryType::FileRecord) => {
        // Verify file record and its chunks are readable
        match engine.get_entry_including_deleted(&child.hash) {
          Ok(Some((header, _key, value))) => {
            match crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version) {
              Ok(record) => {
                for chunk_hash in &record.chunk_hashes {
                  match engine.get_entry_including_deleted(chunk_hash) {
                    Ok(Some(_)) => {}
                    _ => {
                      missing.push(format!("{} (chunk {} missing)", child_path, hex::encode(chunk_hash)));
                    }
                  }
                }
              }
              Err(_) => {
                missing.push(format!("{} (corrupt file record)", child_path));
              }
            }
          }
          Ok(None) => {
            missing.push(format!("{} (file record missing)", child_path));
          }
          Err(_) => {
            missing.push(format!("{} (read error)", child_path));
          }
        }
      }
      _ => {
        // Symlinks and other types — just verify entry exists
        if let Ok(None) = engine.get_entry_including_deleted(&child.hash) {
          missing.push(format!("{} (entry missing)", child_path));
        }
      }
    }
  }
}
