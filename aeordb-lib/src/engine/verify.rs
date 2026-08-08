//! Database integrity verification.
//!
//! Scans the append log, verifies entry hashes, checks directory consistency,
//! validates KV index, and produces a structured report.

use std::fs::File;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::entry_type::EntryType;
use crate::engine::file_header::read_active_header;
use crate::engine::hot_tail;
use crate::engine::kv_rebuild_workspace::{KvRebuildWorkspace, RebuildOrder, ResolvedKvRecord};
use crate::engine::kv_store::{KV_FLAG_DELETED, KV_TYPE_VOID};
use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::VerifyPathSelection;
use crate::engine::v4::durability_recovery::DurabilityRepairVerification;
use crate::engine::v4::hash::digest_parts;
use crate::engine::SystemFamilyPolicyResolver;

/// Structured B-tree directory issue used by repair logic. The CLI still
/// renders `btree_directory_issues` as strings for human-readable output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BTreeDirectoryIssue {
  pub path: String,
  pub node_hash: Option<String>,
  pub reason: String,
}

const MAX_VERIFY_DETAILS: usize = 20;
const MAX_VERIFY_DIAGNOSTICS: usize = 1024;

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
  /// Logical file bytes reachable from the current HEAD namespace.
  pub logical_data_size: u64,
  /// Logical bytes represented by unique retained FileRecord versions in the
  /// live KV index. Canonical `fileid:` keys win; content and path aliases are
  /// fallback representatives for databases written before identity aliases.
  pub retained_logical_data_size: u64,
  /// The retained logical total minus the current HEAD total. This includes
  /// snapshots/forks, system records outside HEAD, path-safety records, and
  /// unreachable versions awaiting GC. It saturates at zero when corruption
  /// makes either independently measured side incomplete.
  pub non_head_retained_logical_data_size: u64,
  /// Number of unique retained FileRecord versions in live KV.
  pub retained_file_versions: u64,
  /// Serialized FileRecord value bytes physically encountered in the WAL,
  /// including aliases, superseded entries, and entries awaiting reclamation.
  pub file_record_payload_size: u64,
  pub chunk_data_size: u64,
  pub dedup_savings: u64,

  // Integrity
  pub valid_entries: u64,
  pub corrupt_hash: u64,
  pub corrupt_header: u64,
  pub skipped_regions: Vec<(u64, u64)>, // (offset, length)

  // Directory consistency
  pub directories_checked: u64,
  pub missing_children: Vec<String>,      // paths where child doesn't exist
  pub unlisted_files: Vec<String>,        // files that exist but aren't in parent dir
  pub dangling_file_records: Vec<String>, // live path-key FileRecords with missing chunks
  pub btree_directory_issues: Vec<String>,
  pub btree_directory_issue_details: Vec<BTreeDirectoryIssue>,

  // KV index
  pub kv_entries: u64,
  pub stale_kv_entries: u64,
  pub missing_kv_entries: u64,
  pub stale_kv_details: Vec<String>,
  pub missing_kv_details: Vec<String>,
  pub invalid_kv_offsets: Vec<String>,
  pub invalid_hot_tail_voids: Vec<String>,
  pub verification_errors: Vec<String>,

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
      retained_logical_data_size: 0,
      non_head_retained_logical_data_size: 0,
      retained_file_versions: 0,
      file_record_payload_size: 0,
      chunk_data_size: 0,
      dedup_savings: 0,
      valid_entries: 0,
      corrupt_hash: 0,
      corrupt_header: 0,
      skipped_regions: Vec::new(),
      directories_checked: 0,
      missing_children: Vec::new(),
      unlisted_files: Vec::new(),
      dangling_file_records: Vec::new(),
      btree_directory_issues: Vec::new(),
      btree_directory_issue_details: Vec::new(),
      kv_entries: 0,
      stale_kv_entries: 0,
      missing_kv_entries: 0,
      stale_kv_details: Vec::new(),
      missing_kv_details: Vec::new(),
      invalid_kv_offsets: Vec::new(),
      invalid_hot_tail_voids: Vec::new(),
      verification_errors: Vec::new(),
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
      || !self.dangling_file_records.is_empty()
      || !self.btree_directory_issues.is_empty()
      || self.stale_kv_entries > 0
      || self.missing_kv_entries > 0
      || !self.invalid_kv_offsets.is_empty()
      || !self.invalid_hot_tail_voids.is_empty()
      || !self.verification_errors.is_empty()
      || !self.broken_snapshots.is_empty()
      || !self.stale_dir_path_keys.is_empty()
  }
}

/// Run a full integrity check on the database.
pub fn verify(engine: &StorageEngine, db_path: &str) -> VerifyReport {
  match verify_checked(engine, db_path) {
    Ok(report) => report,
    Err(error) => {
      let mut report = VerifyReport::new(db_path);
      report.hash_algorithm = format!("{:?}", engine.hash_algo());
      report.file_size = std::fs::metadata(engine.database_path()).map(|metadata| metadata.len()).unwrap_or(0);
      report.verification_errors.push(error.to_string());
      report
    }
  }
}

/// Run a full integrity check and surface operational/resource failures to
/// callers that must not mistake an incomplete scan for a clean database.
pub fn verify_checked(engine: &StorageEngine, db_path: &str) -> EngineResult<VerifyReport> {
  engine.with_repair_maintenance("verify", || verify_checked_inner(engine, db_path))
}

fn verify_checked_inner(engine: &StorageEngine, db_path: &str) -> EngineResult<VerifyReport> {
  let mut report = VerifyReport::new(db_path);

  report.file_size = std::fs::metadata(engine.database_path())?.len();
  report.hash_algorithm = format!("{:?}", engine.hash_algo());

  check_hot_tail_voids(engine, &mut report)?;
  let expected = scan_entries(engine, &mut report)?;
  let actual = scan_kv_index(engine, &mut report)?;
  compare_kv_runs(&mut report, &expected, &actual)?;
  drop(expected);
  drop(actual);

  // Phase 3: Check directory consistency
  check_directories(engine, &mut report)?;
  check_path_file_records(engine, &mut report)?;

  report.non_head_retained_logical_data_size = report.retained_logical_data_size.saturating_sub(report.logical_data_size);
  report.dedup_savings = report.logical_data_size.saturating_sub(report.chunk_data_size);

  // Phase 4: Check snapshot tree integrity (detects GC damage)
  check_snapshot_integrity(engine, &mut report)?;

  Ok(report)
}

/// Run the final clean verification pass for an explicit durability repair.
/// The returned proof is opaque outside the engine and is bound to the exact
/// header/coordinator frontier observed after the scan.
pub fn verify_durability_repair(engine: &StorageEngine, db_path: &str) -> EngineResult<(VerifyReport, DurabilityRepairVerification)> {
  let recovery = engine
    .persistent_durability_recovery()
    .filter(|recovery| recovery.blocks_writes && recovery.is_repair_verifying())
    .ok_or_else(|| EngineError::InvalidInput("durability repair verification requires an active repair-verifying latch".to_string()))?;
  let report = verify_checked(engine, db_path)?;
  if report.has_issues() {
    return Err(EngineError::DurabilityFailure(format!(
      "durability repair verification still reports unresolved database issues: corrupt_hash={}, corrupt_header={}, missing_children={} (sample={:?}), dangling_file_records={}, btree_issues={}, stale_kv={}, missing_kv={}, invalid_kv_offsets={}, invalid_hot_tail_voids={}, verification_errors={}, broken_snapshots={}, stale_dir_keys={}",
      report.corrupt_hash,
      report.corrupt_header,
      report.missing_children.len(),
      report.missing_children.first(),
      report.dangling_file_records.len(),
      report.btree_directory_issues.len(),
      report.stale_kv_entries,
      report.missing_kv_entries,
      report.invalid_kv_offsets.len(),
      report.invalid_hot_tail_voids.len(),
      report.verification_errors.len(),
      report.broken_snapshots.len(),
      report.stale_dir_path_keys.len(),
    )));
  }
  let selected_header_sequence = engine.writer_read_lock()?.file_header().sequence;
  let durable_sequence = engine.durability_snapshot()?.hard_frontier;
  if selected_header_sequence == 0 || durable_sequence == 0 {
    return Err(EngineError::DurabilityFailure(
      "durability repair verification did not observe nonzero header/frontier evidence".to_string(),
    ));
  }

  let mut summary = Vec::with_capacity(16 + 11 * 8 + 2);
  summary.extend_from_slice(&recovery.database_id);
  summary.extend_from_slice(&engine.hash_algo().to_u16().to_le_bytes());
  for value in [
    report.file_size,
    report.total_entries,
    report.valid_entries,
    report.chunks,
    report.file_records,
    report.directory_indexes,
    report.kv_entries,
    report.directories_checked,
    report.snapshots_checked,
    selected_header_sequence,
    durable_sequence,
  ] {
    summary.extend_from_slice(&value.to_le_bytes());
  }
  let evidence_digest = digest_parts(engine.hash_algo(), &[b"aeordb.durability-repair-verification.v1\0", &summary]);
  Ok((report, DurabilityRepairVerification::new(evidence_digest, selected_header_sequence, durable_sequence)))
}

/// Run verify with auto-repair (KV rebuild + directory tree rebuild).
///
/// For KV block expansion, use the CLI's verify command which handles
/// the engine drop/reopen cycle needed for WAL relocation.
pub fn verify_and_repair(engine: &StorageEngine, db_path: &str) -> VerifyReport {
  match verify_and_repair_checked(engine, db_path) {
    Ok(report) => report,
    Err(error) => {
      let mut report = VerifyReport::new(db_path);
      report.hash_algorithm = format!("{:?}", engine.hash_algo());
      report.file_size = std::fs::metadata(engine.database_path()).map(|metadata| metadata.len()).unwrap_or(0);
      report.verification_errors.push(error.to_string());
      report
    }
  }
}

pub fn verify_and_repair_checked(engine: &StorageEngine, db_path: &str) -> EngineResult<VerifyReport> {
  let report = verify_checked(engine, db_path)?;
  let repairs = repair_verified_report(engine, &report)?;
  if repairs.is_empty() {
    return Ok(report);
  }

  let mut final_report = verify_checked(engine, db_path)?;
  final_report.repairs = repairs;
  Ok(final_report)
}

fn repair_verified_report(engine: &StorageEngine, report: &VerifyReport) -> EngineResult<Vec<String>> {
  let mut repairs = Vec::new();
  let mut mutated = false;

  // Repair 1: Rebuild KV if there are missing or stale entries
  if report.missing_kv_entries > 0 || report.stale_kv_entries > 0 {
    engine.rebuild_kv()?;
    mutated = true;
    repairs
      .push(format!("KV index rebuilt ({} missing + {} stale entries recovered)", report.missing_kv_entries, report.stale_kv_entries,));
  }

  // Repair 2: Note corrupt entries
  if report.corrupt_hash > 0 || report.corrupt_header > 0 {
    repairs.push(format!(
      "Found {} corrupt entries ({} hash failures + {} header failures)",
      report.corrupt_hash + report.corrupt_header,
      report.corrupt_hash,
      report.corrupt_header,
    ));
  }

  // Repair 3: Rebuild directory tree
  if (report.missing_kv_entries > 0 && report.file_records > 0)
    || !report.missing_children.is_empty()
    || !report.btree_directory_issues.is_empty()
  {
    let ops = DirectoryOps::new(engine);
    let ctx = crate::engine::request_context::RequestContext::system();

    let mut targeted_repair_failed = false;
    let mut targeted_repair_succeeded = false;
    if !report.btree_directory_issue_details.is_empty() {
      let mut paths: Vec<String> = report.btree_directory_issue_details.iter().map(|issue| issue.path.clone()).collect();
      paths.sort();
      paths.dedup();
      for path in paths {
        match ops.repair_directory_index_from_path_records(&path) {
          Ok(count) => {
            targeted_repair_succeeded = true;
            mutated |= count > 0;
            repairs.push(format!("B-tree directory repaired from path records: {} ({} directory written)", path, count));
          }
          Err(error) if is_operational_verification_error(&error) => return Err(error),
          Err(error) => {
            targeted_repair_failed = true;
            repairs.push(format!("B-tree directory targeted repair failed for {}: {}; falling back to full rebuild", path, error));
          }
        }
      }
    }

    let missing_kv_needs_full_rebuild = report.missing_kv_entries > 0 && report.file_records > 0 && !targeted_repair_succeeded;
    if targeted_repair_failed || !report.missing_children.is_empty() || missing_kv_needs_full_rebuild {
      let count = ops.rebuild_directory_tree(&ctx)?;
      mutated |= count > 0;
      repairs.push(format!("Directory tree rebuilt ({} directories written)", count));
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
    for path in &report.stale_dir_path_keys {
      if ops.repair_stale_dir_key(path)? {
        repaired = repaired.saturating_add(1);
      }
    }
    mutated |= repaired > 0;
    repairs.push(format!("Stale dir_keys rewritten: {} fixed", repaired));
  }

  if !report.invalid_hot_tail_voids.is_empty() {
    engine.sync_voids_to_kv_writer()?;
    mutated = true;
    repairs.push(format!("Hot-tail void snapshot republished after {} invalid diagnostic(s)", report.invalid_hot_tail_voids.len()));
  }

  if mutated {
    engine.force_hot_tail_flush()?;
    repairs.push("Repairs durably published.".to_string());
  }

  Ok(repairs)
}

#[cfg(test)]
mod repair_tests {
  use super::*;
  use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
  use crate::server::create_temp_engine_for_tests;

  #[test]
  fn checked_repair_propagates_rebuild_pressure_without_shutting_down_engine() {
    let (engine, temp) = create_temp_engine_for_tests();
    let coordinator = engine.memory_coordinator();
    let snapshot = coordinator.snapshot().unwrap();
    let policy = snapshot.policy.unwrap();
    let remaining_critical = policy.emergency_reserve_bytes.checked_sub(snapshot.critical_reserved_bytes).unwrap();
    let pressure = coordinator
      .reserve(MemoryOwner::Repair, remaining_critical, AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery))
      .unwrap();

    let db_path = temp.path().join("test.aeordb");
    let mut report = VerifyReport::new(db_path.to_str().unwrap());
    report.missing_kv_entries = 1;
    let error = repair_verified_report(&engine, &report).unwrap_err();
    assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected repair failure: {error}");

    drop(pressure);
    let key = engine.compute_hash(b"repair-pressure-released").unwrap();
    engine.store_entry(EntryType::Chunk, &key, b"still-writable").unwrap();
    assert!(engine.has_entry(&key).unwrap());
  }
}

/// Scan the WAL, accumulating per-type counts, integrity counts, and
/// the set of KV hash keys expected to be live. Void entries are storage
/// bookkeeping, not user/content records, so they are counted in the storage
/// summary but excluded from live-KV completeness checks.
fn scan_entries(engine: &StorageEngine, report: &mut VerifyReport) -> EngineResult<KvRebuildWorkspace> {
  let coordinator = engine.memory_coordinator();
  let mut expected = KvRebuildWorkspace::new_for_purpose(
    engine.database_path(),
    "verify-expected",
    engine.hash_algo(),
    Some(coordinator.as_ref()),
    AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery),
    Some(engine.repair_cancellation()),
  )?;
  let mut scanner = engine.writer_read_lock()?.scan_entries_reporting_current_wal(Some(engine.repair_cancellation()))?;
  while let Some(result) = scanner.next_verify_entry() {
    match result {
      Ok(scanned) => {
        report.total_entries = report.total_entries.saturating_add(1);
        report.valid_entries = report.valid_entries.saturating_add(1);
        match scanned.header.entry_type {
          EntryType::Chunk => {
            report.chunks = report.chunks.saturating_add(1);
            report.chunk_data_size = report.chunk_data_size.saturating_add(scanned.header.value_length as u64);
          }
          EntryType::FileRecord => {
            report.file_records = report.file_records.saturating_add(1);
            report.file_record_payload_size = report.file_record_payload_size.saturating_add(scanned.header.value_length as u64);
          }
          EntryType::DirectoryIndex => report.directory_indexes = report.directory_indexes.saturating_add(1),
          EntryType::Symlink => report.symlinks = report.symlinks.saturating_add(1),
          EntryType::Snapshot => report.snapshots = report.snapshots.saturating_add(1),
          EntryType::DeletionRecord => report.deletion_records = report.deletion_records.saturating_add(1),
          EntryType::Fork => report.forks = report.forks.saturating_add(1),
          EntryType::Void => {}
        }

        if engine.entry_overlaps_current_void(scanned.offset, scanned.header.total_length)? {
          continue;
        }
        let order = RebuildOrder { timestamp: scanned.header.timestamp, offset: scanned.offset };
        if scanned.header.entry_type == EntryType::DeletionRecord {
          let value = scanned.value.as_deref().ok_or_else(|| EngineError::CorruptEntry {
            offset: scanned.offset,
            reason: "verification scanner omitted a deletion-record payload".to_string(),
          })?;
          let deletion = crate::engine::deletion_record::DeletionRecord::deserialize(value, scanned.header.entry_version)?;
          expected.push_deletion_path(&deletion.path, order)?;
        }
        if scanned.header.entry_type != EntryType::Void {
          expected.push_value(
            scanned.header.entry_type.to_kv_type(),
            &scanned.key,
            scanned.offset,
            scanned.header.value_length,
            scanned.header.total_length,
            order,
          )?;
        }
      }
      Err(EngineError::CorruptEntry { reason, .. }) => {
        report.total_entries = report.total_entries.saturating_add(1);
        if reason.contains("Hash verification") {
          report.corrupt_hash = report.corrupt_hash.saturating_add(1);
        } else {
          report.corrupt_header = report.corrupt_header.saturating_add(1);
        }
      }
      Err(error) => return Err(error),
    }
  }
  report.skipped_regions.extend(scanner.skipped_regions.iter().map(|(offset, length)| (*offset, *length as u64)));
  if scanner.skipped_region_count() > scanner.skipped_regions.len() as u64 && report.verification_errors.len() < MAX_VERIFY_DIAGNOSTICS {
    report.verification_errors.push(format!(
      "{} additional corrupt WAL regions ({} total skipped bytes) were omitted from bounded diagnostics",
      scanner.skipped_region_count().saturating_sub(scanner.skipped_regions.len() as u64),
      scanner.skipped_region_bytes()
    ));
  }
  expected.finish()?;
  Ok(expected)
}

fn scan_kv_index(engine: &StorageEngine, report: &mut VerifyReport) -> EngineResult<KvRebuildWorkspace> {
  let coordinator = engine.memory_coordinator();
  let mut actual = KvRebuildWorkspace::new_for_purpose(
    engine.database_path(),
    "verify-actual",
    engine.hash_algo(),
    Some(coordinator.as_ref()),
    AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery),
    Some(engine.repair_cancellation()),
  )?;
  let (wal_start, wal_end) = {
    let writer = engine.writer_read_lock()?;
    let header = writer.file_header();
    (header.kv_block_offset.saturating_add(header.kv_block_length), writer.current_offset())
  };
  engine.visit_kv_entries_for_repair(|entry| {
    if entry.entry_type() == KV_TYPE_VOID {
      return Ok(true);
    }
    report.kv_entries = report.kv_entries.saturating_add(1);
    if !StorageEngine::valid_reusable_range(entry.offset, entry.total_length, wal_start, wal_end)
      && report.invalid_kv_offsets.len() < MAX_VERIFY_DIAGNOSTICS
    {
      report.invalid_kv_offsets.push(format!(
        "hash {} offset {} length {} outside WAL region {}..{}",
        short_hash(&entry.hash),
        entry.offset,
        entry.total_length,
        wal_start,
        wal_end
      ));
    }
    actual.push_value(
      entry.type_flags,
      &entry.hash,
      entry.offset,
      0,
      entry.total_length,
      RebuildOrder { timestamp: 0, offset: entry.offset },
    )?;
    Ok(true)
  })?;
  actual.finish()?;
  Ok(actual)
}

fn compare_kv_runs(report: &mut VerifyReport, expected: &KvRebuildWorkspace, actual: &KvRebuildWorkspace) -> EngineResult<()> {
  let mut expected_cursor = expected.resolved_cursor()?;
  let mut actual_cursor = actual.resolved_cursor()?;
  let mut expected_entry = next_live_record(&mut expected_cursor)?;
  let mut actual_entry = next_live_record(&mut actual_cursor)?;
  loop {
    match (&expected_entry, &actual_entry) {
      (Some(expected), Some(actual)) if expected.hash < actual.hash => {
        record_missing(report, expected);
        expected_entry = next_live_record(&mut expected_cursor)?;
      }
      (Some(expected), Some(actual)) if actual.hash < expected.hash => {
        record_stale(report, actual);
        actual_entry = next_live_record(&mut actual_cursor)?;
      }
      (Some(expected), Some(actual)) => {
        if expected.offset != actual.offset
          || expected.total_length != actual.total_length
          || expected.type_flags & 0x0f != actual.type_flags & 0x0f
        {
          record_missing(report, expected);
          record_stale(report, actual);
        }
        expected_entry = next_live_record(&mut expected_cursor)?;
        actual_entry = next_live_record(&mut actual_cursor)?;
      }
      (Some(expected), None) => {
        record_missing(report, expected);
        expected_entry = next_live_record(&mut expected_cursor)?;
      }
      (None, Some(actual)) => {
        record_stale(report, actual);
        actual_entry = next_live_record(&mut actual_cursor)?;
      }
      (None, None) => return Ok(()),
    }
  }
}

fn next_live_record(cursor: &mut crate::engine::kv_rebuild_workspace::ResolvedRecordCursor) -> EngineResult<Option<ResolvedKvRecord>> {
  loop {
    let Some(record) = cursor.next_record()? else {
      return Ok(None);
    };
    if record.type_flags & KV_FLAG_DELETED == 0 && record.type_flags & 0x0f != KV_TYPE_VOID {
      return Ok(Some(record));
    }
  }
}

fn record_missing(report: &mut VerifyReport, entry: &ResolvedKvRecord) {
  report.missing_kv_entries = report.missing_kv_entries.saturating_add(1);
  if report.missing_kv_details.len() < MAX_VERIFY_DETAILS {
    report.missing_kv_details.push(format!("hash {} offset {} length {}", short_hash(&entry.hash), entry.offset, entry.total_length));
  }
}

fn record_stale(report: &mut VerifyReport, entry: &ResolvedKvRecord) {
  report.stale_kv_entries = report.stale_kv_entries.saturating_add(1);
  if report.stale_kv_details.len() < MAX_VERIFY_DETAILS {
    report.stale_kv_details.push(format!(
      "hash {} type_flags=0x{:02x} offset {} length {}",
      short_hash(&entry.hash),
      entry.type_flags,
      entry.offset,
      entry.total_length
    ));
  }
}

fn short_hash(hash: &[u8]) -> String {
  hex::encode(&hash[..8.min(hash.len())])
}

fn check_hot_tail_voids(engine: &StorageEngine, report: &mut VerifyReport) -> EngineResult<()> {
  let mut file = File::open(engine.database_path())?;
  let (header, _) = read_active_header(&mut file)?;
  if header.hot_tail_offset == 0 {
    return Ok(());
  }

  let hash_length = header.hash_algo.hash_length();
  let wal_start = header.kv_block_offset.saturating_add(header.kv_block_length);
  let hot_tail_offset = header.hot_tail_offset;
  let current_wal_end = engine.writer_read_lock()?.current_offset();
  let validation_end = hot_tail_offset.max(current_wal_end);
  let mut previous_end = None;
  let cancellation = engine.repair_cancellation();
  let mut inspect_void = |index: u64, void: crate::engine::hot_tail::VoidRecord| -> EngineResult<()> {
    ensure_repair_active(engine)?;
    report.voids = report.voids.saturating_add(1);
    report.void_bytes = report.void_bytes.saturating_add(void.size as u64);
    if !StorageEngine::valid_reusable_range(void.offset, void.size, wal_start, validation_end) {
      if report.invalid_hot_tail_voids.len() < MAX_VERIFY_DIAGNOSTICS {
        report.invalid_hot_tail_voids.push(format!(
          "void #{} offset {} length {} outside WAL region {}..{}",
          index, void.offset, void.size, wal_start, validation_end
        ));
      }
    } else if previous_end.is_some_and(|end| void.offset < end) && report.invalid_hot_tail_voids.len() < MAX_VERIFY_DIAGNOSTICS {
      report.invalid_hot_tail_voids.push(format!("void #{} offset {} overlaps or is out of order", index, void.offset));
    }
    previous_end = Some(previous_end.unwrap_or(0).max(void.offset.saturating_add(void.size as u64)));
    Ok(())
  };

  // An admitted embedded write can advance the live WAL before the periodic
  // checkpoint republishes the hot-tail offset. During that bounded interval
  // the old offset now contains WAL data, so validate the in-memory void
  // authority rather than misclassifying the expected entry magic as damage.
  if hot_tail_offset < current_wal_end {
    let mut index = 0u64;
    return engine.visit_current_voids_for_repair(|offset, size| {
      let current = index;
      index = index.saturating_add(1);
      inspect_void(current, crate::engine::hot_tail::VoidRecord { offset, size })
    });
  }

  let result = hot_tail::visit_hot_tail_voids(&mut file, hot_tail_offset, hash_length, Some(cancellation.as_ref()), |index, void| {
    inspect_void(u64::from(index), void)
  });
  if let Err(error) = result {
    match error {
      EngineError::InvalidMagic
      | EngineError::InvalidEntryVersion(_)
      | EngineError::InvalidEntryType(_)
      | EngineError::InvalidHashAlgorithm(_)
      | EngineError::CorruptEntry { .. }
      | EngineError::UnexpectedEof => {
        if report.invalid_hot_tail_voids.len() < MAX_VERIFY_DIAGNOSTICS {
          report.invalid_hot_tail_voids.push(error.to_string());
        }
      }
      operational => return Err(operational),
    }
  }
  Ok(())
}

fn check_directories(engine: &StorageEngine, report: &mut VerifyReport) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);

  // List root directory and recursively check all children
  check_directory_recursive(&ops, engine, "/", report, 0)
}

fn check_directory_recursive(
  ops: &DirectoryOps,
  engine: &StorageEngine,
  path: &str,
  report: &mut VerifyReport,
  depth: usize,
) -> EngineResult<()> {
  // Limit recursion depth to prevent infinite loops on corrupt directory cycles
  if depth > 100 {
    record_verification_error(report, format!("directory traversal exceeded 100 levels at {path}"));
    return Ok(());
  }
  ensure_repair_active(engine)?;

  report.directories_checked = report.directories_checked.saturating_add(1);

  let result = ops.visit_directory_for_verification(path, |child| {
    ensure_repair_active(engine)?;
    let child_path = if path == "/" { format!("/{}", child.name) } else { format!("{}/{}", path.trim_end_matches('/'), child.name) };

    match EntryType::from_u8(child.entry_type) {
      Ok(EntryType::DirectoryIndex) => check_directory_recursive(ops, engine, &child_path, report, depth + 1)?,
      Ok(EntryType::FileRecord) => {
        report.logical_data_size = report.logical_data_size.saturating_add(child.total_size);
        let key = crate::engine::directory_ops::file_path_hash(&child_path, &engine.hash_algo())?;
        match engine.get_entry_header(&key) {
          Ok(Some(header)) if header.entry_type == EntryType::FileRecord => {}
          Ok(Some(header)) => record_missing_child(report, format!("{} (path key resolves to {:?})", child_path, header.entry_type)),
          Ok(None) => record_missing_child(report, format!("{} (file record not found)", child_path)),
          Err(error) if is_operational_verification_error(&error) => return Err(error),
          Err(error) => record_missing_child(report, format!("{} ({})", child_path, error)),
        }
      }
      Ok(EntryType::Symlink) => {
        let key = crate::engine::symlink_record::symlink_path_hash(&child_path, &engine.hash_algo())?;
        match engine.has_entry(&key) {
          Ok(true) => {}
          Ok(false) => record_missing_child(report, format!("{} (symlink record not found)", child_path)),
          Err(error) if is_operational_verification_error(&error) => return Err(error),
          Err(error) => record_missing_child(report, format!("{} ({})", child_path, error)),
        }
      }
      Ok(other) => record_missing_child(report, format!("{} (unexpected directory child type {:?})", child_path, other)),
      Err(error) => record_missing_child(report, format!("{} ({})", child_path, error)),
    }
    Ok(())
  });
  match result {
    Ok((warnings, recovered_stale_path_key)) => {
      if recovered_stale_path_key && report.stale_dir_path_keys.len() < MAX_VERIFY_DIAGNOSTICS {
        report.stale_dir_path_keys.push(path.to_string());
      }
      for warning in warnings {
        record_btree_directory_issue(report, path, &warning);
      }
      Ok(())
    }
    Err(error) if is_operational_verification_error(&error) => Err(error),
    Err(error) => {
      record_missing_child(report, format!("{} (directory unreadable: {})", path, error));
      Ok(())
    }
  }
}

fn ensure_repair_active(engine: &StorageEngine) -> EngineResult<()> {
  if engine.repair_cancellation().load(std::sync::atomic::Ordering::Acquire) {
    return Err(EngineError::ShuttingDown);
  }
  Ok(())
}

fn record_missing_child(report: &mut VerifyReport, detail: String) {
  if report.missing_children.len() < MAX_VERIFY_DIAGNOSTICS {
    report.missing_children.push(detail);
  }
}

fn record_btree_directory_issue(report: &mut VerifyReport, path: &str, warning: &crate::engine::btree::BTreeWalkWarning) {
  if report.btree_directory_issue_details.len() >= MAX_VERIFY_DIAGNOSTICS {
    return;
  }
  let node_hash = warning.node_hash_hex();
  let issue = BTreeDirectoryIssue { path: path.to_string(), node_hash, reason: warning.reason.clone() };
  report.btree_directory_issues.push(format_btree_directory_issue(&issue));
  report.btree_directory_issue_details.push(issue);
}

fn format_btree_directory_issue(issue: &BTreeDirectoryIssue) -> String {
  format!("{} (B-tree node {}: {})", issue.path, issue.node_hash.as_deref().unwrap_or("inline-root"), issue.reason)
}

fn check_path_file_records(engine: &StorageEngine, report: &mut VerifyReport) -> EngineResult<()> {
  let hash_length = engine.hash_algo().hash_length();
  let algo = engine.hash_algo();
  let family_policy = SystemFamilyPolicyResolver::new(algo)?;
  let mut memory = OperationMemoryBudget::new(
    engine,
    "FileRecord verification",
    MemoryOwner::Repair,
    AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery),
    0,
    None,
  )?;
  let result = engine.visit_kv_entries_for_repair(|entry| {
    if entry.entry_type() != crate::engine::kv_store::KV_TYPE_FILE_RECORD {
      return Ok(true);
    }
    ensure_repair_active(engine)?;
    let checkpoint = memory.checkpoint();
    let result = check_path_file_record_entry(engine, entry, hash_length, &algo, family_policy, report, &mut memory);
    let release = memory.release_to(checkpoint, "FileRecord verification entry release failed");
    match (result, release) {
      (Ok(()), Ok(())) => Ok(true),
      (Err(error), Ok(())) => Err(error),
      (_, Err(error)) => Err(error),
    }
  });
  result?;
  Ok(())
}

fn check_path_file_record_entry(
  engine: &StorageEngine,
  entry: &crate::engine::kv_store::KVEntry,
  hash_length: usize,
  algo: &crate::engine::hash_algorithm::HashAlgorithm,
  family_policy: SystemFamilyPolicyResolver,
  report: &mut VerifyReport,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<()> {
  let header = match engine.get_entry_header(&entry.hash) {
    Ok(Some(header)) => header,
    Ok(None) => {
      record_verification_error(report, format!("FileRecord KV entry {} is not readable", short_hash(&entry.hash)));
      return Ok(());
    }
    Err(error) if is_operational_verification_error(&error) => return Err(error),
    Err(error) => {
      record_verification_error(report, format!("FileRecord KV entry {} could not be read: {error}", short_hash(&entry.hash)));
      return Ok(());
    }
  };
  reserve_decoded_payload(memory, header.value_length, "FileRecord payload admission failed")?;
  let (header, value) = match engine.get_entry_verified_bounded(&entry.hash, header.value_length) {
    Ok(Some((header, _key, value))) => (header, value),
    Ok(None) => {
      record_verification_error(report, format!("FileRecord KV entry {} disappeared during verification", short_hash(&entry.hash)));
      return Ok(());
    }
    Err(error) if is_operational_verification_error(&error) => return Err(error),
    Err(error) => {
      record_verification_error(report, format!("FileRecord KV entry {} failed integrity read: {error}", short_hash(&entry.hash)));
      return Ok(());
    }
  };
  if let Some(task_record) = crate::engine::task_queue::validate_task_storage_record(&entry.hash, &value) {
    if let Err(error) = task_record {
      record_verification_error(report, error.to_string());
    }
    return Ok(());
  }
  let record = match crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version) {
    Ok(record) => record,
    Err(error) => {
      record_verification_error(report, format!("FileRecord {} is malformed: {error}", short_hash(&entry.hash)));
      return Ok(());
    }
  };
  let normalized = crate::engine::path_utils::normalize_path(&record.path);
  let identity_key =
    crate::engine::directory_ops::file_identity_hash(&normalized, record.content_type.as_deref(), &record.chunk_hashes, algo)?;
  let path_key = crate::engine::directory_ops::file_path_hash(&normalized, algo)?;
  let represents_retained_version = if entry.hash == identity_key {
    true
  } else if engine.has_entry(&identity_key)? {
    false
  } else {
    let content_key = crate::engine::directory_ops::file_content_hash(&value, algo)?;
    entry.hash == content_key || (entry.hash == path_key && !engine.has_entry(&content_key)?)
  };
  if represents_retained_version {
    report.retained_file_versions = report.retained_file_versions.saturating_add(1);
    report.retained_logical_data_size = report.retained_logical_data_size.saturating_add(record.total_size);
  }
  if entry.hash != path_key {
    return Ok(());
  }
  match family_policy.verify_path_selection(&normalized)? {
    VerifyPathSelection::Strict => {}
    VerifyPathSelection::Rebuildable => return Ok(()),
    VerifyPathSelection::StructuralContainer => {
      record_verification_error(report, format!("FileRecord path {} is a structural SystemFamily container", normalized));
      return Ok(());
    }
  }

  let mut missing_count = 0usize;
  let mut missing_examples = Vec::with_capacity(3);
  for chunk_hash in &record.chunk_hashes {
    ensure_repair_active(engine)?;
    match engine.get_entry_header(chunk_hash) {
      Ok(Some(chunk_header)) if chunk_header.entry_type == EntryType::Chunk => {}
      Ok(Some(chunk_header)) => {
        missing_count = missing_count.saturating_add(1);
        if missing_examples.len() < 3 {
          missing_examples.push(format!("{} ({:?})", hex::encode(chunk_hash), chunk_header.entry_type));
        }
      }
      Ok(None) => {
        missing_count = missing_count.saturating_add(1);
        if missing_examples.len() < 3 {
          missing_examples.push(hex::encode(chunk_hash));
        }
      }
      Err(error) if is_operational_verification_error(&error) => return Err(error),
      Err(error) => {
        record_verification_error(report, format!("FileRecord {} chunk {} lookup failed: {error}", normalized, short_hash(chunk_hash)));
        missing_count = missing_count.saturating_add(1);
      }
    }
  }
  if missing_count > 0 && report.dangling_file_records.len() < MAX_VERIFY_DIAGNOSTICS {
    report.dangling_file_records.push(format!("{} ({} missing chunk(s): {})", normalized, missing_count, missing_examples.join(", ")));
  }
  Ok(())
}

/// Phase 4: Walk each snapshot's directory tree and verify all entries
/// are reachable. Detects damage from GC sweeping snapshot-referenced data.
fn check_snapshot_integrity(engine: &StorageEngine, report: &mut VerifyReport) -> EngineResult<()> {
  let hash_length = engine.hash_algo().hash_length();
  let mut memory = OperationMemoryBudget::new(
    engine,
    "snapshot verification",
    MemoryOwner::Repair,
    AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery),
    0,
    None,
  )?;
  engine.visit_kv_entries_for_repair(|entry| {
    if entry.entry_type() != crate::engine::kv_store::KV_TYPE_SNAPSHOT {
      return Ok(true);
    }
    ensure_repair_active(engine)?;
    report.snapshots_checked = report.snapshots_checked.saturating_add(1);
    let checkpoint = memory.checkpoint();
    let result = verify_snapshot_entry(engine, entry, hash_length, report, &mut memory);
    let release = memory.release_to(checkpoint, "snapshot verification entry release failed");
    match (result, release) {
      (Ok(()), Ok(())) => Ok(true),
      (Err(error), Ok(())) => Err(error),
      (_, Err(error)) => Err(error),
    }
  })?;
  Ok(())
}

fn is_operational_verification_error(error: &EngineError) -> bool {
  matches!(
    error,
    EngineError::IoError(_)
      | EngineError::ResourceExhausted(_)
      | EngineError::DurabilityFailure(_)
      | EngineError::PostMutationDurabilityFailure(_)
      | EngineError::ShuttingDown
      | EngineError::Cancelled(_)
  )
}

fn reserve_decoded_payload(memory: &mut OperationMemoryBudget, value_length: u32, context: &'static str) -> EngineResult<()> {
  let bytes = u64::from(value_length)
    .checked_mul(3)
    .and_then(|bytes| bytes.checked_add(512))
    .ok_or_else(|| EngineError::ResourceExhausted(format!("{context}: decoded payload estimate overflow")))?;
  memory.reserve(bytes, context)
}

fn record_verification_error(report: &mut VerifyReport, message: String) {
  if report.verification_errors.len() < MAX_VERIFY_DIAGNOSTICS {
    report.verification_errors.push(message);
  }
}

fn verify_snapshot_entry(
  engine: &StorageEngine,
  entry: &crate::engine::kv_store::KVEntry,
  hash_length: usize,
  report: &mut VerifyReport,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<()> {
  let header = match engine.get_entry_header(&entry.hash) {
    Ok(Some(header)) => header,
    Ok(None) => {
      record_broken_snapshot(report, format!("snapshot {} has no readable WAL entry", short_hash(&entry.hash)));
      return Ok(());
    }
    Err(error) if is_operational_verification_error(&error) => return Err(error),
    Err(error) => {
      record_broken_snapshot(report, format!("snapshot {} metadata entry is corrupt: {error}", short_hash(&entry.hash)));
      return Ok(());
    }
  };
  reserve_decoded_payload(memory, header.value_length, "snapshot metadata admission failed")?;
  let (header, value) = match engine.get_entry_verified_bounded(&entry.hash, header.value_length) {
    Ok(Some((header, _key, value))) => (header, value),
    Ok(None) => {
      record_broken_snapshot(report, format!("snapshot {} disappeared during verification", short_hash(&entry.hash)));
      return Ok(());
    }
    Err(error) if is_operational_verification_error(&error) => return Err(error),
    Err(error) => {
      record_broken_snapshot(report, format!("snapshot {} metadata body is corrupt: {error}", short_hash(&entry.hash)));
      return Ok(());
    }
  };
  let snapshot = match crate::engine::version_manager::SnapshotInfo::deserialize(&value, hash_length, header.entry_version) {
    Ok(snapshot) => snapshot,
    Err(error) => {
      record_verification_error(report, format!("snapshot {} metadata is malformed: {error}", short_hash(&entry.hash)));
      return Ok(());
    }
  };

  let mut missing_count = 0u64;
  let mut missing_details = Vec::with_capacity(5);
  walk_snapshot_tree(engine, &snapshot.root_hash, "/", hash_length, &mut missing_count, &mut missing_details, 0, memory)?;
  if missing_count > 0 {
    record_broken_snapshot(
      report,
      format!(
        "{} (id: {}): {} broken references - {}",
        snapshot.name,
        hex::encode(&snapshot.root_hash),
        missing_count,
        missing_details.join(", "),
      ),
    );
  }
  Ok(())
}

fn record_broken_snapshot(report: &mut VerifyReport, detail: String) {
  if report.broken_snapshots.len() < MAX_VERIFY_DIAGNOSTICS {
    report.broken_snapshots.push(detail);
  }
}

fn record_snapshot_missing(count: &mut u64, details: &mut Vec<String>, detail: String) {
  *count = count.saturating_add(1);
  if details.len() < 5 {
    details.push(detail);
  }
}

/// Recursively walk a snapshot's directory tree, retaining only a bounded
/// diagnostic sample while charging each live payload before it is loaded.
#[allow(clippy::too_many_arguments)]
fn walk_snapshot_tree(
  engine: &StorageEngine,
  root_hash: &[u8],
  dir_path: &str,
  hash_length: usize,
  missing_count: &mut u64,
  missing_details: &mut Vec<String>,
  depth: usize,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<()> {
  ensure_repair_active(engine)?;
  if depth > 100 {
    record_snapshot_missing(missing_count, missing_details, format!("{} (directory depth exceeds 100)", dir_path));
    return Ok(());
  }

  let header = match engine.get_entry_header_including_deleted(root_hash) {
    Ok(Some(header)) => header,
    Ok(None) => {
      record_snapshot_missing(missing_count, missing_details, format!("{} (dir entry missing)", dir_path));
      return Ok(());
    }
    Err(error) if is_operational_verification_error(&error) => return Err(error),
    Err(error) => {
      record_snapshot_missing(missing_count, missing_details, format!("{} (dir entry corrupt: {})", dir_path, error));
      return Ok(());
    }
  };
  let checkpoint = memory.checkpoint();
  reserve_decoded_payload(memory, header.value_length, "snapshot directory admission failed")?;
  let result = (|| {
    let (header, value) = match engine.get_entry_including_deleted_verified_bounded(root_hash, header.value_length) {
      Ok(Some((header, _key, value))) => (header, value),
      Ok(None) => {
        record_snapshot_missing(missing_count, missing_details, format!("{} (dir entry missing)", dir_path));
        return Ok(());
      }
      Err(error) if is_operational_verification_error(&error) => return Err(error),
      Err(error) => {
        record_snapshot_missing(missing_count, missing_details, format!("{} (dir entry corrupt: {})", dir_path, error));
        return Ok(());
      }
    };

    if value.is_empty() {
      return Ok(());
    }

    if crate::engine::btree::is_btree_format(&value) {
      let mut visitor = |child: &crate::engine::directory_entry::ChildEntry| -> EngineResult<bool> {
        walk_snapshot_child(engine, child, dir_path, hash_length, missing_count, missing_details, depth, memory)?;
        Ok(true)
      };
      let visit = crate::engine::btree::btree_visit_from_node_with_mode(
        &value,
        engine,
        hash_length,
        true,
        crate::engine::btree::BTreeWalkMode::BestEffort,
        &mut visitor,
      )?;
      for warning in visit.warnings {
        let issue = BTreeDirectoryIssue { path: dir_path.to_string(), node_hash: warning.node_hash_hex(), reason: warning.reason };
        record_snapshot_missing(missing_count, missing_details, format_btree_directory_issue(&issue));
      }
      return Ok(());
    }

    if let Err(error) = DirectoryOps::visit_bounded_flat_children(&value, hash_length, header.entry_version, |child| {
      walk_snapshot_child(engine, child, dir_path, hash_length, missing_count, missing_details, depth, memory)?;
      Ok(true)
    }) {
      if is_operational_verification_error(&error) {
        return Err(error);
      }
      record_snapshot_missing(missing_count, missing_details, format!("{} (corrupt flat index: {})", dir_path, error));
    }
    Ok(())
  })();
  let release = memory.release_to(checkpoint, "snapshot directory release failed");
  match (result, release) {
    (Ok(()), Ok(())) => Ok(()),
    (Err(error), Ok(())) => Err(error),
    (_, Err(error)) => Err(error),
  }
}

#[allow(clippy::too_many_arguments)]
fn walk_snapshot_child(
  engine: &StorageEngine,
  child: &crate::engine::directory_entry::ChildEntry,
  dir_path: &str,
  hash_length: usize,
  missing_count: &mut u64,
  missing_details: &mut Vec<String>,
  depth: usize,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<()> {
  ensure_repair_active(engine)?;
  let child_path = if dir_path == "/" { format!("/{}", child.name) } else { format!("{}/{}", dir_path, child.name) };

  match crate::engine::entry_type::EntryType::from_u8(child.entry_type) {
    Ok(crate::engine::entry_type::EntryType::DirectoryIndex) => {
      walk_snapshot_tree(engine, &child.hash, &child_path, hash_length, missing_count, missing_details, depth + 1, memory)
    }
    Ok(crate::engine::entry_type::EntryType::FileRecord) => {
      let header = match engine.get_entry_header_including_deleted(&child.hash) {
        Ok(Some(header)) => header,
        Ok(None) => {
          record_snapshot_missing(missing_count, missing_details, format!("{} (file record missing)", child_path));
          return Ok(());
        }
        Err(error) if is_operational_verification_error(&error) => return Err(error),
        Err(error) => {
          record_snapshot_missing(missing_count, missing_details, format!("{} (file record corrupt: {})", child_path, error));
          return Ok(());
        }
      };
      let checkpoint = memory.checkpoint();
      reserve_decoded_payload(memory, header.value_length, "snapshot FileRecord admission failed")?;
      let result = (|| {
        let (header, value) = match engine.get_entry_including_deleted_verified_bounded(&child.hash, header.value_length) {
          Ok(Some((header, _key, value))) => (header, value),
          Ok(None) => {
            record_snapshot_missing(missing_count, missing_details, format!("{} (file record missing)", child_path));
            return Ok(());
          }
          Err(error) if is_operational_verification_error(&error) => return Err(error),
          Err(error) => {
            record_snapshot_missing(missing_count, missing_details, format!("{} (file record corrupt: {})", child_path, error));
            return Ok(());
          }
        };
        let record = match crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version) {
          Ok(record) => record,
          Err(error) => {
            record_snapshot_missing(missing_count, missing_details, format!("{} (corrupt file record: {})", child_path, error));
            return Ok(());
          }
        };
        for chunk_hash in &record.chunk_hashes {
          ensure_repair_active(engine)?;
          match engine.get_entry_header_including_deleted(chunk_hash) {
            Ok(Some(chunk_header)) if chunk_header.entry_type == EntryType::Chunk => {}
            Ok(Some(chunk_header)) => record_snapshot_missing(
              missing_count,
              missing_details,
              format!("{} (chunk {} resolves to {:?})", child_path, hex::encode(chunk_hash), chunk_header.entry_type),
            ),
            Ok(None) => {
              record_snapshot_missing(missing_count, missing_details, format!("{} (chunk {} missing)", child_path, hex::encode(chunk_hash)))
            }
            Err(error) if is_operational_verification_error(&error) => return Err(error),
            Err(error) => record_snapshot_missing(
              missing_count,
              missing_details,
              format!("{} (chunk {} entry corrupt: {})", child_path, hex::encode(chunk_hash), error),
            ),
          }
        }
        Ok(())
      })();
      let release = memory.release_to(checkpoint, "snapshot FileRecord release failed");
      match (result, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (_, Err(error)) => Err(error),
      }
    }
    Ok(_) => match engine.get_entry_header_including_deleted(&child.hash) {
      Ok(Some(_)) => Ok(()),
      Ok(None) => {
        record_snapshot_missing(missing_count, missing_details, format!("{} (entry missing)", child_path));
        Ok(())
      }
      Err(error) if is_operational_verification_error(&error) => Err(error),
      Err(error) => {
        record_snapshot_missing(missing_count, missing_details, format!("{} (entry corrupt: {})", child_path, error));
        Ok(())
      }
    },
    Err(error) => {
      record_snapshot_missing(missing_count, missing_details, format!("{} (invalid entry type: {})", child_path, error));
      Ok(())
    }
  }
}
