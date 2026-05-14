//! Database integrity verification.
//!
//! Scans the append log, verifies entry hashes, checks directory consistency,
//! validates KV index, and produces a structured report.

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::entry_type::EntryType;
use crate::engine::storage_engine::StorageEngine;

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
    pub missing_children: Vec<String>,  // paths where child doesn't exist
    pub unlisted_files: Vec<String>,    // files that exist but aren't in parent dir

    // KV index
    pub kv_entries: u64,
    pub stale_kv_entries: u64,
    pub missing_kv_entries: u64,

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
            || !self.broken_snapshots.is_empty()
    }
}

/// Run a full integrity check on the database.
pub fn verify(engine: &StorageEngine, db_path: &str) -> VerifyReport {
    let mut report = VerifyReport::new(db_path);

    // File size
    report.file_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    report.hash_algorithm = format!("{:?}", engine.hash_algo());

    // Phase 1: Scan all entries from the append log
    scan_entries(engine, &mut report);

    // Phase 2: Check KV index consistency
    check_kv_index(engine, &mut report);

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
                report.repairs.push(format!(
                    "KV index rebuilt ({} missing + {} stale entries recovered)",
                    report.missing_kv_entries, report.stale_kv_entries,
                ));
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
                report.repairs.push(format!(
                    "Directory tree rebuilt ({} paths re-propagated)", count
                ));
            }
            Err(e) => {
                report.repairs.push(format!("Directory tree rebuild failed: {}", e));
            }
        }
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

fn scan_entries(engine: &StorageEngine, report: &mut VerifyReport) {
    use crate::engine::errors::EngineError;

    // Use the writer to scan entries — reporting mode yields errors
    // for corrupt entries instead of silently skipping them.
    let writer = match engine.writer_read_lock() {
        Ok(w) => w,
        Err(_) => return,
    };

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
                        EntryType::Void => {
                            report.voids += 1;
                            report.void_bytes += scanned.header.total_length as u64;
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
}

fn check_kv_index(engine: &StorageEngine, report: &mut VerifyReport) {
    // Count KV entries from snapshot
    let snapshot = engine.kv_snapshot.load();
    if let Ok(entries) = snapshot.iter_all() {
        report.kv_entries = entries.len() as u64;
    }

    // Count unique hashes from the WAL scan (not total entries, since
    // duplicate hashes are expected — e.g., file records stored at both
    // content hash and identity hash, or entries updated in place).
    let unique_hashes = {
        let writer = match engine.writer_read_lock() {
            Ok(w) => w,
            Err(_) => {
                report.missing_kv_entries = report.valid_entries.saturating_sub(report.kv_entries);
                return;
            }
        };
        match writer.scan_entries() {
            Ok(scanner) => {
                let mut seen = std::collections::HashSet::new();
                for scanned in scanner.flatten() {
                    seen.insert(scanned.key);
                }
                seen.len() as u64
            }
            Err(_) => report.valid_entries,
        }
    };

    if report.kv_entries < unique_hashes {
        report.missing_kv_entries = unique_hashes - report.kv_entries;
    } else if report.kv_entries > unique_hashes {
        report.stale_kv_entries = report.kv_entries - unique_hashes;
    }
}

fn check_directories(engine: &StorageEngine, report: &mut VerifyReport) {
    let ops = DirectoryOps::new(engine);

    // List root directory and recursively check all children
    check_directory_recursive(&ops, engine, "/", report, 0);
}

fn check_directory_recursive(
    ops: &DirectoryOps,
    engine: &StorageEngine,
    path: &str,
    report: &mut VerifyReport,
    depth: usize,
) {
    // Limit recursion depth to prevent infinite loops on corrupt directory cycles
    if depth > 100 {
        return;
    }

    report.directories_checked += 1;

    match ops.list_directory(path) {
        Ok(children) => {
            for child in &children {
                let child_path = if path == "/" {
                    format!("/{}", child.name)
                } else {
                    format!("{}/{}", path.trim_end_matches('/'), child.name)
                };

                if child.entry_type == EntryType::DirectoryIndex.to_u8() {
                    // Recurse into subdirectories
                    check_directory_recursive(ops, engine, &child_path, report, depth + 1);
                } else if child.entry_type == EntryType::FileRecord.to_u8() {
                    // Verify file record is readable
                    match ops.read_file_buffered(&child_path) {
                        Ok(_) => {}
                        Err(e) => {
                            report.missing_children.push(format!(
                                "{} ({})", child_path, e
                            ));
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
    if depth > 100 { return; }

    let value = match engine.get_entry_including_deleted(root_hash) {
        Ok(Some((_header, _key, value))) => value,
        Ok(None) => {
            missing.push(format!("{} (dir entry missing)", dir_path));
            return;
        }
        Err(_) => return,
    };

    if value.is_empty() { return; }

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
        let child_path = if dir_path == "/" {
            format!("/{}", child.name)
        } else {
            format!("{}/{}", dir_path, child.name)
        };

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
