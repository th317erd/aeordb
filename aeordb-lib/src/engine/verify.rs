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

    report
}

/// Run verify with auto-repair.
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

    // Repair 2: Note corrupt entries for quarantine
    if report.corrupt_hash > 0 || report.corrupt_header > 0 {
        report.repairs.push(format!(
            "Found {} corrupt entries ({} hash failures + {} header failures)",
            report.corrupt_hash + report.corrupt_header,
            report.corrupt_hash,
            report.corrupt_header,
        ));
    }

    // Repair 3: Rebuild directory tree after KV rebuild to ensure
    // directory entries reflect the current file set (not stale empty entries
    // from a previous broken session).
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
    // Use the writer to scan entries
    let writer = match engine.writer_read_lock() {
        Ok(w) => w,
        Err(_) => return,
    };

    match writer.scan_entries() {
        Ok(scanner) => {
            for result in scanner {
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
                    Err(e) => {
                        report.total_entries += 1;
                        let msg = format!("{}", e);
                        if msg.contains("Hash verification") {
                            report.corrupt_hash += 1;
                        } else {
                            report.corrupt_header += 1;
                        }
                    }
                }
            }
        }
        Err(_) => {}
    }

    report.dedup_savings = report.logical_data_size.saturating_sub(report.chunk_data_size);
}

fn check_kv_index(engine: &StorageEngine, report: &mut VerifyReport) {
    // Count KV entries from snapshot
    let snapshot = engine.kv_snapshot.load();
    match snapshot.iter_all() {
        Ok(entries) => {
            report.kv_entries = entries.len() as u64;
        }
        Err(_) => {}
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
                for result in scanner {
                    if let Ok(scanned) = result {
                        seen.insert(scanned.key);
                    }
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
                    match ops.read_file(&child_path) {
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
