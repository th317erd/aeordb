use crate::engine::deletion_record::DeletionRecord;
use crate::engine::directory_entry::{deserialize_child_entries, serialize_child_entries, ChildEntry};
use crate::engine::directory_ops::{
  DirectoryOps, deletion_record_hash, directory_content_hash, directory_path_hash, file_path_hash, validate_existing_chunk_locator,
};
use crate::engine::engine_event::{ImportEventData, EVENT_IMPORTS_COMPLETED};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::kv_store::{KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_DELETION, KV_TYPE_SYMLINK};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::namespace_mutation::{
  NamespaceMutationAcknowledgement, NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationFanout, NamespaceMutationKind,
  NamespaceMutationSourceIdentity, publish_namespace_root_from_with_fanout, publish_namespace_root_with_fanout,
};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_family_policy::{SystemFamilyPolicyResolver, TransferPathSelection};
use crate::engine::symlink_record::{symlink_path_hash, SymlinkRecord};
use crate::engine::tree_walker::{
  diff_trees_with_budget, walk_version_tree_for_transfer_from_source_with_budget, walk_version_tree_for_transfer_with_budget,
  HistoricalEntry, HistoricalEntrySource, VersionTree,
};
use crate::engine::v4::system_family::SystemFamilyTransferOperationV1;
use crate::engine::entry_type::EntryType;
use crate::engine::version_manager::SnapshotInfo;
use tokio_util::sync::CancellationToken;
use std::path::Path;
use std::sync::Arc;

const BACKUP_MINIMUM_WORKSPACE_BYTES: u64 = 4 * 1024;
const KV_INVENTORY_ENTRY_BYTES: u64 = std::mem::size_of::<crate::engine::kv_store::KVEntry>() as u64 + 96;
const BACKUP_COLLECTION_OVERHEAD_BYTES: u64 = 96;
const IMPORT_LOCATOR_BATCH_MAX_ENTRIES: usize = 256;
const IMPORT_LOCATOR_BATCH_MAX_BYTES: u64 = 8 * 1024 * 1024;

struct TreeWriteResult {
  chunks_written: u64,
  files_written: u64,
  files_mutated: u64,
  directories_written: u64,
  directories_mutated: u64,
  symlinks_written: u64,
  symlinks_mutated: u64,
  root_hash: Vec<u8>,
}

struct ImportHeadFanout {
  context: RequestContext,
  payload: serde_json::Value,
}

impl NamespaceMutationFanout for ImportHeadFanout {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement) {
    let mut payload = self.payload.clone();
    if let Err(error) = acknowledgement.annotate_event_payload(&mut payload) {
      tracing::error!(operation_id = %acknowledgement.operation_id, error = %error, "Import event payload is invalid");
      return;
    }
    self.context.emit(EVENT_IMPORTS_COMPLETED, payload);
  }
}

fn finish_import_head(
  context: &RequestContext,
  target: &StorageEngine,
  root_hash: &[u8],
  promote: bool,
  backup_type: &str,
  entries_imported: u64,
  expected_root_hash: Option<&[u8]>,
) -> EngineResult<bool> {
  let event_payload = |head_promoted| {
    serde_json::json!({"imports": [ImportEventData {
      backup_type: backup_type.to_string(),
      version_hash: hex::encode(root_hash),
      entries_imported,
      head_promoted,
    }]})
  };
  if promote {
    let fanout = Arc::new(ImportHeadFanout { context: context.clone(), payload: event_payload(true) });
    let acknowledgement = match expected_root_hash {
      Some(expected_root) => {
        publish_namespace_root_from_with_fanout(target, expected_root, root_hash, NamespaceMutationKind::Import, fanout)?
      }
      None => publish_namespace_root_with_fanout(target, root_hash, NamespaceMutationKind::Import, fanout)?,
    };
    if acknowledgement.is_some() {
      return Ok(true);
    }
    context.emit(EVENT_IMPORTS_COMPLETED, event_payload(true));
    return Ok(true);
  }
  context.emit(EVENT_IMPORTS_COMPLETED, event_payload(false));
  Ok(false)
}

struct PartialBackupArtifact {
  path: String,
}

impl PartialBackupArtifact {
  fn new(output_path: &str) -> Self {
    Self { path: format!("{output_path}.part-{}", uuid::Uuid::new_v4()) }
  }

  fn path(&self) -> &str {
    &self.path
  }
}

impl Drop for PartialBackupArtifact {
  fn drop(&mut self) {
    cleanup_backup_artifact(Path::new(&self.path));
  }
}

pub(crate) fn cleanup_backup_artifact(path: &Path) {
  remove_backup_artifact_file(path);
  let mut lock_path = path.as_os_str().to_os_string();
  lock_path.push(".lock");
  remove_backup_artifact_file(Path::new(&lock_path));
}

fn remove_backup_artifact_file(path: &Path) {
  if let Err(error) = std::fs::remove_file(path) {
    if error.kind() != std::io::ErrorKind::NotFound {
      tracing::warn!(path = %path.display(), %error, "failed to remove temporary backup artifact");
    }
  }
}

fn backup_budget(source: &StorageEngine, cancellation: Option<&CancellationToken>) -> EngineResult<OperationMemoryBudget> {
  OperationMemoryBudget::new(
    source,
    "backup/restore",
    MemoryOwner::BackupRestore,
    AdmissionClass::Maintenance,
    BACKUP_MINIMUM_WORKSPACE_BYTES,
    cancellation,
  )
}

fn export_transfer_operation(include_system: bool) -> SystemFamilyTransferOperationV1 {
  if include_system {
    SystemFamilyTransferOperationV1::LogicalBackup
  } else {
    SystemFamilyTransferOperationV1::DataExport
  }
}

fn store_file_record_entry_preserving_version(
  engine: &StorageEngine,
  key: &[u8],
  value: &[u8],
  flags: u8,
  entry_version: u8,
) -> EngineResult<()> {
  if flags != 0 {
    engine.store_entry_with_flags_and_version(EntryType::FileRecord, key, value, flags, entry_version)?;
  } else {
    engine.store_entry_with_version(EntryType::FileRecord, key, value, entry_version)?;
  }
  Ok(())
}

/// Export a complete version as a clean, self-contained .aeordb file.
///
/// The output database contains only live entries at the given version:
/// no voids, no deletion records, no stale overwrites, no history.
/// backup_type = 1 (full export), with base_hash and target_hash set to the
/// root actually written to the artifact. When operation policy omits any
/// reachable child, parent directory closure is rebuilt and the returned root
/// differs from the requested source hash.
///
/// If `include_system` is true, registry-selected logical-backup families are
/// included. Credentials, secrets, node-local controls, logs, and derived
/// indexes remain omitted. Otherwise the data-export policy is applied.
pub fn export_version(source: &StorageEngine, version_hash: &[u8], output_path: &str, include_system: bool) -> EngineResult<ExportResult> {
  export_version_controlled(source, version_hash, output_path, include_system, None)
}

pub fn export_version_with_cancellation(
  source: &StorageEngine,
  version_hash: &[u8],
  output_path: &str,
  include_system: bool,
  cancellation: &CancellationToken,
) -> EngineResult<ExportResult> {
  export_version_controlled(source, version_hash, output_path, include_system, Some(cancellation))
}

fn export_version_controlled(
  source: &StorageEngine,
  version_hash: &[u8],
  output_path: &str,
  include_system: bool,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<ExportResult> {
  let mut budget = backup_budget(source, cancellation)?;
  export_version_with_budget(source, version_hash, output_path, include_system, &mut budget)
}

fn export_version_with_budget(
  source: &StorageEngine,
  version_hash: &[u8],
  output_path: &str,
  include_system: bool,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<ExportResult> {
  export_atomic(output_path, |part_path| {
    budget.check_cancellation()?;
    let operation = export_transfer_operation(include_system);
    let resolver = SystemFamilyPolicyResolver::new(source.hash_algo())?;
    let tree = walk_version_tree_for_transfer_with_budget(source, version_hash, operation, include_system, budget)?;
    budget.check_cancellation()?;
    let output = StorageEngine::create_with_memory_coordinator(part_path, source.memory_coordinator())?;
    let stats = write_tree_to_engine(&tree, source, &output, resolver, operation, TransferDestinationMode::Artifact, budget)?;
    budget.check_cancellation()?;
    output.set_backup_info(1, &stats.root_hash, &stats.root_hash)?;
    output.update_head(&stats.root_hash)?;

    Ok(ExportResult {
      chunks_written: stats.chunks_written,
      files_written: stats.files_written,
      directories_written: stats.directories_written,
      version_hash: stats.root_hash,
      snapshots_written: 0,
    })
  })
}

/// Wrap an export operation so it writes to a unique same-filesystem sibling first, then
/// renames atomically once the StorageEngine is dropped (which fsyncs). If
/// the operation fails or the process is killed mid-write, the destination
/// is never partially populated. The parent directory is also fsynced so the
/// rename itself is durable.
fn export_atomic<F>(output_path: &str, work: F) -> EngineResult<ExportResult>
where
  F: FnOnce(&str) -> EngineResult<ExportResult>,
{
  // Refuse to overwrite an existing destination — callers should remove first.
  // This preserves the pre-atomicity contract (StorageEngine::create rejected
  // existing files) so accidental clobbers are still caught.
  if std::path::Path::new(output_path).exists() {
    return Err(EngineError::AlreadyExists(format!("export destination '{}' already exists", output_path)));
  }
  let part = PartialBackupArtifact::new(output_path);

  let result = work(part.path());
  // `output` is dropped at the end of `work`, which fsyncs the file (see
  // StorageEngine::drop → shutdown → sync_all). The partial file is now durable.

  match result {
    Ok(stats) => {
      crate::engine::durability::rename_durable(part.path(), output_path)?;
      Ok(stats)
    }
    Err(error) => Err(error),
  }
}

/// Export HEAD or a named snapshot.
pub fn export_snapshot(
  source: &StorageEngine,
  snapshot_name: Option<&str>,
  output_path: &str,
  include_system: bool,
) -> EngineResult<ExportResult> {
  export_snapshot_controlled(source, snapshot_name, output_path, include_system, None, || {})
}

pub fn export_snapshot_with_cancellation(
  source: &StorageEngine,
  snapshot_name: Option<&str>,
  output_path: &str,
  include_system: bool,
  cancellation: &CancellationToken,
) -> EngineResult<ExportResult> {
  export_snapshot_controlled(source, snapshot_name, output_path, include_system, Some(cancellation), || {})
}

/// Deterministic operation-boundary hook used to prove pressure changes after
/// backup workspace admission and before material export work.
#[doc(hidden)]
pub fn export_snapshot_with_post_admission_hook<F>(
  source: &StorageEngine,
  snapshot_name: Option<&str>,
  output_path: &str,
  include_system: bool,
  post_admission_hook: F,
) -> EngineResult<ExportResult>
where
  F: FnOnce(),
{
  export_snapshot_controlled(source, snapshot_name, output_path, include_system, None, post_admission_hook)
}

fn export_snapshot_controlled<F>(
  source: &StorageEngine,
  snapshot_name: Option<&str>,
  output_path: &str,
  include_system: bool,
  cancellation: Option<&CancellationToken>,
  post_admission_hook: F,
) -> EngineResult<ExportResult>
where
  F: FnOnce(),
{
  let mut budget = backup_budget(source, cancellation)?;
  post_admission_hook();
  budget.record_work(128)?;
  let snapshot_checkpoint = budget.checkpoint();
  let version_hash = match snapshot_name {
    Some(name) => {
      let snapshots = load_snapshot_infos(source, &mut budget)?;
      let snap =
        snapshots.iter().find(|s| s.name == name).ok_or_else(|| EngineError::NotFound(format!("Snapshot '{}' not found", name)))?;
      snap.root_hash.clone()
    }
    None => source.head_hash()?,
  };
  budget.release_to(snapshot_checkpoint, "snapshot resolution release failed")?;

  export_version_with_budget(source, &version_hash, output_path, include_system, &mut budget)
}

/// Export the FULL database: HEAD + every named snapshot + (optionally) system data.
///
/// This is the proper "full backup" mode. Each named snapshot's tree is walked
/// and all reachable entries are written. Snapshot records themselves are
/// included so the imported database has the same snapshot history.
///
/// `include_system` selects registry-governed logical-backup behavior,
/// including required portable system families and all named snapshots while
/// omitting credentials, secrets, node-local controls, logs, and derived
/// indexes. Callers must validate root-key authority before passing true.
pub fn export_full(source: &StorageEngine, output_path: &str, include_system: bool) -> EngineResult<ExportResult> {
  let mut budget = backup_budget(source, None)?;
  export_atomic(output_path, |part_path| {
    budget.check_cancellation()?;
    let output = StorageEngine::create_with_memory_coordinator(part_path, source.memory_coordinator())?;

    let head_hash = source.head_hash()?;

    let mut total_chunks = 0u64;
    let mut total_files = 0u64;
    let mut total_dirs = 0u64;

    let mut walked: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let operation = export_transfer_operation(include_system);
    let resolver = SystemFamilyPolicyResolver::new(source.hash_algo())?;

    let head_checkpoint = budget.checkpoint();
    let stats = {
      let head_tree = walk_version_tree_for_transfer_with_budget(source, &head_hash, operation, include_system, &mut budget)?;
      write_tree_to_engine(&head_tree, source, &output, resolver, operation, TransferDestinationMode::Artifact, &mut budget)?
    };
    budget.release_to(head_checkpoint, "HEAD tree release failed")?;
    total_chunks += stats.chunks_written;
    total_files += stats.files_written;
    total_dirs += stats.directories_written;
    let exported_head_hash = stats.root_hash;
    budget.reserve(KV_INVENTORY_ENTRY_BYTES, "walked-version set admission failed")?;
    walked.insert(head_hash.clone());

    let snapshots = load_snapshot_infos(source, &mut budget)?;
    let snapshot_count = snapshots.len() as u64;
    for snap in &snapshots {
      budget.record_work(1)?;
      if walked.contains(&snap.root_hash) {
        continue;
      }
      budget.reserve(KV_INVENTORY_ENTRY_BYTES, "walked-version set admission failed")?;
      let tree_checkpoint = budget.checkpoint();
      let stats = {
        let tree = walk_version_tree_for_transfer_with_budget(source, &snap.root_hash, operation, false, &mut budget)?;
        write_tree_to_engine(&tree, source, &output, resolver, operation, TransferDestinationMode::Artifact, &mut budget)?
      };
      budget.release_to(tree_checkpoint, "snapshot tree release failed")?;
      total_chunks += stats.chunks_written;
      total_files += stats.files_written;
      total_dirs += stats.directories_written;
      walked.insert(snap.root_hash.clone());
    }

    if include_system {
      copy_snapshot_entries(source, &output, &mut budget)?;
    }

    output.set_backup_info(1, &exported_head_hash, &exported_head_hash)?;
    output.update_head(&exported_head_hash)?;

    Ok(ExportResult {
      chunks_written: total_chunks,
      files_written: total_files,
      directories_written: total_dirs,
      version_hash: exported_head_hash,
      snapshots_written: if include_system { snapshot_count } else { 0 },
    })
  })
}

/// Write all entries from a VersionTree into an output engine.
/// Returns exact write counts and the root identity published by the export.
///
/// The tree has already been classified by the operation-specific registry
/// policy. Directory indexes are rebuilt as needed so omitted descendants do
/// not remain reachable through copied parent metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransferDestinationMode {
  Artifact,
  FullImport,
  HistoricalImport,
  SparseImport,
}

impl TransferDestinationMode {
  fn records_runtime_metrics(self) -> bool {
    self != Self::Artifact
  }

  fn coordinates_active_locators(self) -> bool {
    matches!(self, Self::FullImport | Self::SparseImport)
  }
}

struct ImportLocatorBatch<'a> {
  output: &'a StorageEngine,
  budget: OperationMemoryBudget,
  batch: NamespaceMutationBatch,
  entry_count: usize,
  retained_bytes: u64,
}

fn preserve_import_primary_error(primary: EngineError, cleanup: EngineResult<()>, context: &str) -> EngineError {
  if let Err(cleanup_error) = cleanup {
    tracing::error!(error = %cleanup_error, primary_error = %primary, "{context}");
  }
  primary
}

impl<'a> ImportLocatorBatch<'a> {
  fn new(output: &'a StorageEngine) -> EngineResult<Self> {
    Ok(Self {
      output,
      budget: backup_budget(output, None)?,
      batch: NamespaceMutationBatch::new(NamespaceMutationKind::MaintenanceRepair),
      entry_count: 0,
      retained_bytes: 0,
    })
  }

  #[allow(clippy::too_many_arguments)]
  fn replace(
    &mut self,
    entry_type: EntryType,
    key: Vec<u8>,
    value: Vec<u8>,
    flags: u8,
    entry_version: u8,
    path: String,
    new_identity: Vec<u8>,
  ) -> EngineResult<()> {
    let charge = import_locator_charge(&key, &value, &path, Some(&new_identity), None)?;
    self.flush_before(charge)?;
    self.budget.reserve(charge, "import locator batch admission failed")?;
    if let Err(error) = self.batch.replace_locator_with_version(entry_type, key, value, flags, entry_version) {
      return Err(preserve_import_primary_error(
        error,
        self.budget.release(charge, "failed import locator batch release failed"),
        "Import locator planning failed and reservation cleanup also failed",
      ));
    }
    if let Err(error) = self.batch.add_source_identity(NamespaceMutationSourceIdentity {
      path,
      entry_type: Some(entry_type.to_u8()),
      previous_identity: None,
      new_identity: Some(new_identity),
    }) {
      return Err(preserve_import_primary_error(
        error,
        self.budget.release(charge, "failed import locator identity release failed"),
        "Import locator identity planning failed and reservation cleanup also failed",
      ));
    }
    self.entry_count += 1;
    self.retained_bytes = self
      .retained_bytes
      .checked_add(charge)
      .ok_or_else(|| EngineError::ResourceExhausted("import locator batch retained-byte count overflow".to_string()))?;
    if self.entry_count >= IMPORT_LOCATOR_BATCH_MAX_ENTRIES || self.retained_bytes >= IMPORT_LOCATOR_BATCH_MAX_BYTES {
      self.flush()?;
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  fn retire_with_dependency(
    &mut self,
    dependency_type: EntryType,
    dependency_key: Vec<u8>,
    dependency_value: Vec<u8>,
    dependency_flags: u8,
    dependency_version: u8,
    locator_key: Vec<u8>,
    path: String,
    previous_identity: Vec<u8>,
  ) -> EngineResult<()> {
    let dependency_charge = dependency_key
      .len()
      .checked_mul(2)
      .and_then(|bytes| bytes.checked_add(BACKUP_COLLECTION_OVERHEAD_BYTES as usize))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| EngineError::ResourceExhausted("import deletion batch estimate overflow".to_string()))?;
    let charge = import_locator_charge(&locator_key, &dependency_value, &path, Some(&previous_identity), None)?
      .checked_add(dependency_charge)
      .ok_or_else(|| EngineError::ResourceExhausted("import deletion batch estimate overflow".to_string()))?;
    self.flush_before(charge)?;
    self.budget.reserve(charge, "import deletion batch admission failed")?;
    if let Err(error) =
      self.batch.store_dependency_with_version(dependency_type, dependency_key, dependency_value, dependency_flags, dependency_version)
    {
      return Err(preserve_import_primary_error(
        error,
        self.budget.release(charge, "failed import deletion dependency release failed"),
        "Import deletion dependency planning failed and reservation cleanup also failed",
      ));
    }
    if let Err(error) = self.batch.retire_locator(locator_key) {
      return Err(preserve_import_primary_error(
        error,
        self.budget.release(charge, "failed import deletion locator release failed"),
        "Import deletion locator planning failed and reservation cleanup also failed",
      ));
    }
    if let Err(error) = self.batch.add_source_identity(NamespaceMutationSourceIdentity {
      path,
      entry_type: None,
      previous_identity: Some(previous_identity),
      new_identity: None,
    }) {
      return Err(preserve_import_primary_error(
        error,
        self.budget.release(charge, "failed import deletion identity release failed"),
        "Import deletion identity planning failed and reservation cleanup also failed",
      ));
    }
    self.entry_count += 1;
    self.retained_bytes = self
      .retained_bytes
      .checked_add(charge)
      .ok_or_else(|| EngineError::ResourceExhausted("import deletion batch retained-byte count overflow".to_string()))?;
    if self.entry_count >= IMPORT_LOCATOR_BATCH_MAX_ENTRIES || self.retained_bytes >= IMPORT_LOCATOR_BATCH_MAX_BYTES {
      self.flush()?;
    }
    Ok(())
  }

  fn flush_before(&mut self, next_charge: u64) -> EngineResult<()> {
    if self.entry_count > 0
      && (self.entry_count >= IMPORT_LOCATOR_BATCH_MAX_ENTRIES
        || self.retained_bytes.checked_add(next_charge).is_none_or(|total| total > IMPORT_LOCATOR_BATCH_MAX_BYTES))
    {
      self.flush()?;
    }
    Ok(())
  }

  fn flush(&mut self) -> EngineResult<()> {
    if self.entry_count == 0 {
      return Ok(());
    }
    let batch = std::mem::replace(&mut self.batch, NamespaceMutationBatch::new(NamespaceMutationKind::MaintenanceRepair));
    let entry_count = std::mem::take(&mut self.entry_count);
    let retained_bytes = std::mem::take(&mut self.retained_bytes);
    let result = NamespaceMutationCoordinator::new(self.output).execute(batch);
    let release_result = self.budget.release(retained_bytes, "import locator batch release failed");
    match result {
      Ok(_) => {
        release_result?;
        for _ in 0..entry_count {
          self.output.counters().record_write(0);
        }
        Ok(())
      }
      Err(error) => {
        Err(preserve_import_primary_error(error, release_result, "Import locator publication failed and reservation cleanup also failed"))
      }
    }
  }
}

fn import_locator_charge(
  key: &[u8],
  value: &[u8],
  path: &str,
  previous_identity: Option<&[u8]>,
  new_identity: Option<&[u8]>,
) -> EngineResult<u64> {
  key
    .len()
    .checked_mul(2)
    .and_then(|bytes| bytes.checked_add(value.len()))
    .and_then(|bytes| bytes.checked_add(path.len()))
    .and_then(|bytes| bytes.checked_add(previous_identity.map_or(0, <[u8]>::len)))
    .and_then(|bytes| bytes.checked_add(new_identity.map_or(0, <[u8]>::len)))
    .and_then(|bytes| bytes.checked_add((BACKUP_COLLECTION_OVERHEAD_BYTES as usize) * 4))
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| EngineError::ResourceExhausted("import locator batch estimate overflow".to_string()))
}

fn write_tree_to_engine<S: HistoricalEntrySource + ?Sized>(
  tree: &VersionTree,
  source: &S,
  output: &StorageEngine,
  resolver: SystemFamilyPolicyResolver,
  operation: SystemFamilyTransferOperationV1,
  destination_mode: TransferDestinationMode,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<TreeWriteResult> {
  let mut chunks_written = 0u64;
  let mut files_written = 0u64;
  let mut files_mutated = 0u64;
  let mut locator_batch = destination_mode.coordinates_active_locators().then(|| ImportLocatorBatch::new(output)).transpose()?;

  // Walk file-owned hashes directly. The destination KV de-duplicates shared
  // chunks, avoiding a second unbounded in-memory set alongside VersionTree.
  for (_file_hash, record) in tree.files.values() {
    budget.record_work(1)?;
    for chunk_hash in &record.chunk_hashes {
      budget.record_work(1)?;
      if validate_existing_chunk_locator(output, "backup import", chunk_hash)? {
        continue;
      }
      let ((header, key, value), charge) = required_backup_entry(source, chunk_hash, "chunk", budget)?;
      if header.entry_type != EntryType::Chunk {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("backup chunk {} resolved to {:?}", hex::encode(chunk_hash), header.entry_type),
        });
      }
      output.store_entry(EntryType::Chunk, &key, &value)?;
      if destination_mode.records_runtime_metrics() {
        output.counters().record_chunk_stored(value.len() as u64);
        output.counters().record_write(value.len() as u64);
      }
      budget.release(charge, "chunk copy buffer release failed")?;
      chunks_written += 1;
    }
  }

  // Write immutable FileRecords plus stable path locators. Active imports
  // publish locator replacements in bounded hard-authority batches; artifact
  // databases still materialize their standalone path index directly.
  let file_algo = output.hash_algo();
  for (path, (file_hash, _record)) in &tree.files {
    budget.record_work(1)?;
    {
      let ((header, key, value), charge) = required_backup_entry(source, file_hash, "FileRecord", budget)?;
      if header.entry_type != EntryType::FileRecord {
        return Err(EngineError::CorruptEntry { offset: 0, reason: format!("backup file '{}' resolved to {:?}", path, header.entry_type) });
      }
      let mut mutated = false;
      let mut locator_staged = false;
      if !validate_existing_transfer_entry(output, &key, EntryType::FileRecord, "backup FileRecord identity", budget)? {
        store_file_record_entry_preserving_version(output, &key, &value, header.flags, header.entry_version)?;
        mutated = true;
      }
      // Also write at path-hash key (for read_file lookups)
      let path_key = file_path_hash(path, &file_algo)?;
      if path_key != key
        && should_store_transfer_alias(
          output,
          &path_key,
          EntryType::FileRecord,
          &value,
          header.flags,
          header.entry_version,
          destination_mode,
          budget,
        )?
      {
        if destination_mode.coordinates_active_locators() {
          locator_batch.as_mut().expect("active import mode creates a locator batch").replace(
            EntryType::FileRecord,
            path_key,
            value.clone(),
            header.flags,
            header.entry_version,
            path.clone(),
            file_hash.clone(),
          )?;
          locator_staged = true;
        } else {
          store_file_record_entry_preserving_version(output, &path_key, &value, header.flags, header.entry_version)?;
        }
        mutated = true;
      }
      if destination_mode.records_runtime_metrics() && mutated && !locator_staged {
        output.counters().record_write(0);
      }
      files_written += 1;
      files_mutated += u64::from(mutated);
      budget.release(charge, "FileRecord copy buffer release failed")?;
    }
  }

  let (dirs_written, dirs_mutated, root_hash) =
    write_transfer_directories(tree, source, output, resolver, operation, destination_mode, locator_batch.as_mut(), budget)?;

  // Write symlink entries at both content-hash and path-hash keys.
  let symlink_algo = output.hash_algo();
  let mut symlinks_mutated = 0u64;
  for (path, (symlink_hash, _record)) in &tree.symlinks {
    budget.record_work(1)?;
    {
      let ((header, key, value), charge) = required_backup_entry(source, symlink_hash, "Symlink", budget)?;
      if header.entry_type != EntryType::Symlink {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("backup symlink '{}' resolved to {:?}", path, header.entry_type),
        });
      }
      let mut mutated = false;
      let mut locator_staged = false;
      if !validate_existing_transfer_entry(output, &key, EntryType::Symlink, "backup symlink identity", budget)? {
        output.store_entry_with_flags_and_version(EntryType::Symlink, &key, &value, header.flags, header.entry_version)?;
        mutated = true;
      }
      let path_key = symlink_path_hash(path, &symlink_algo)?;
      if path_key != key
        && should_store_transfer_alias(
          output,
          &path_key,
          EntryType::Symlink,
          &value,
          header.flags,
          header.entry_version,
          destination_mode,
          budget,
        )?
      {
        if destination_mode.coordinates_active_locators() {
          locator_batch.as_mut().expect("active import mode creates a locator batch").replace(
            EntryType::Symlink,
            path_key,
            value.clone(),
            header.flags,
            header.entry_version,
            path.clone(),
            symlink_hash.clone(),
          )?;
          locator_staged = true;
        } else {
          output.store_entry_with_flags_and_version(EntryType::Symlink, &path_key, &value, header.flags, header.entry_version)?;
        }
        mutated = true;
      }
      if destination_mode.records_runtime_metrics() && mutated && !locator_staged {
        output.counters().record_write(0);
      }
      symlinks_mutated += u64::from(mutated);
      budget.release(charge, "Symlink copy buffer release failed")?;
    }
  }
  if let Some(locator_batch) = locator_batch.as_mut() {
    locator_batch.flush()?;
  }

  Ok(TreeWriteResult {
    chunks_written,
    files_written,
    files_mutated,
    directories_written: dirs_written,
    directories_mutated: dirs_mutated,
    symlinks_written: tree.symlinks.len() as u64,
    symlinks_mutated,
    root_hash,
  })
}

fn write_transfer_directories<S: HistoricalEntrySource + ?Sized>(
  tree: &VersionTree,
  source: &S,
  output: &StorageEngine,
  resolver: SystemFamilyPolicyResolver,
  operation: SystemFamilyTransferOperationV1,
  destination_mode: TransferDestinationMode,
  locator_batch: Option<&mut ImportLocatorBatch<'_>>,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<(u64, u64, Vec<u8>)> {
  let checkpoint = budget.checkpoint();
  let result = (|| {
    let directory_count = u64::try_from(tree.directories.len())
      .map_err(|_| EngineError::ResourceExhausted("transfer directory count exceeds u64".to_string()))?;
    let retained_path_bytes = tree.directories.keys().try_fold(0u64, |total, path| {
      let path_bytes =
        u64::try_from(path.len()).map_err(|_| EngineError::ResourceExhausted("transfer directory path length exceeds u64".to_string()))?;
      total.checked_add(path_bytes).ok_or_else(|| EngineError::ResourceExhausted("transfer directory path estimate overflow".to_string()))
    })?;
    let per_directory_bytes =
      u64::try_from(std::mem::size_of::<&String>() + std::mem::size_of::<String>() + std::mem::size_of::<Vec<u8>>())
        .map_err(|_| EngineError::ResourceExhausted("transfer directory workspace estimate exceeds u64".to_string()))?
        .checked_add(u64::try_from(output.hash_algo().hash_length()).unwrap_or(u64::MAX))
        .and_then(|bytes| bytes.checked_add(BACKUP_COLLECTION_OVERHEAD_BYTES))
        .ok_or_else(|| EngineError::ResourceExhausted("transfer directory entry estimate overflow".to_string()))?;
    let workspace_charge = directory_count
      .checked_mul(per_directory_bytes)
      .and_then(|bytes| bytes.checked_add(retained_path_bytes))
      .ok_or_else(|| EngineError::ResourceExhausted("transfer directory workspace estimate overflow".to_string()))?;
    budget.reserve(workspace_charge, "transfer directory routing workspace admission failed")?;
    write_transfer_directories_admitted(tree, source, output, resolver, operation, destination_mode, locator_batch, budget)
  })();
  let release_result = budget.release_to(checkpoint, "transfer directory routing workspace release failed");
  match result {
    Ok(result) => {
      release_result?;
      Ok(result)
    }
    Err(error) => {
      Err(preserve_import_primary_error(error, release_result, "Transfer directory routing failed and workspace cleanup also failed"))
    }
  }
}

fn write_transfer_directories_admitted<S: HistoricalEntrySource + ?Sized>(
  tree: &VersionTree,
  source: &S,
  output: &StorageEngine,
  resolver: SystemFamilyPolicyResolver,
  operation: SystemFamilyTransferOperationV1,
  destination_mode: TransferDestinationMode,
  mut locator_batch: Option<&mut ImportLocatorBatch<'_>>,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<(u64, u64, Vec<u8>)> {
  let algorithm = output.hash_algo();
  let hash_length = algorithm.hash_length();
  let mut paths = tree.directories.keys().collect::<Vec<_>>();
  paths.sort_by(|left, right| path_depth(right).cmp(&path_depth(left)).then_with(|| left.cmp(right)));
  let mut written_hashes = std::collections::HashMap::<String, Vec<u8>>::new();
  let mut directories_written = 0u64;
  let mut directories_mutated = 0u64;

  for path in paths {
    budget.record_work(1)?;
    let checkpoint = budget.checkpoint();
    let result = (|| {
      let (source_hash, _) = tree.directories.get(path).expect("directory path came from the same immutable map");
      let ((header, source_key, source_value), loaded_charge) = required_backup_entry(source, source_hash, "DirectoryIndex", budget)?;
      if header.entry_type != EntryType::DirectoryIndex {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("backup directory '{}' resolved to {:?}", path, header.entry_type),
        });
      }

      let source_is_btree = crate::engine::btree::is_btree_format(&source_value);
      let mut children = if source_value.is_empty() {
        Vec::new()
      } else if source_is_btree {
        collect_transfer_btree_entries(tree, source, &source_value, header.entry_version, hash_length, budget)?
      } else {
        let parse_charge = scaled_backup_charge(source_value.len(), 4, "flat transfer directory parse estimate overflow")?;
        budget.reserve(parse_charge, "flat transfer directory parse admission failed")?;
        deserialize_child_entries(&source_value, hash_length, header.entry_version)?
      };
      let original_len = children.len();
      let mut child_hash_changed = false;
      let mut retained = Vec::with_capacity(children.len());
      for mut child in children.drain(..) {
        let child_path = join_transfer_path(path, &child.name);
        let keep = match EntryType::from_u8(child.entry_type)? {
          EntryType::DirectoryIndex => match written_hashes.get(&child_path) {
            Some(written_hash) => {
              if child.hash != *written_hash {
                child.hash = written_hash.clone();
                child_hash_changed = true;
              }
              true
            }
            None => false,
          },
          EntryType::FileRecord => tree.files.contains_key(&child_path),
          EntryType::Symlink => tree.symlinks.contains_key(&child_path),
          other => {
            return Err(EngineError::CorruptEntry {
              offset: 0,
              reason: format!("directory '{}' contains unsupported namespace child '{}' of type {:?}", path, child.name, other),
            });
          }
        };
        if keep {
          retained.push(child);
        }
      }
      let changed = child_hash_changed || retained.len() != original_len;
      if path != "/"
        && retained.is_empty()
        && matches!(
          resolver.transfer_policy_for_path(path, operation)?,
          crate::engine::v4::system_family::SystemFamilyPolicyDecisionV1::StructuralContainer
        )
      {
        budget.release(loaded_charge, "empty structural transfer directory buffer release failed")?;
        return Ok(());
      }
      let flags = header.flags;
      let mut mutated = false;
      let mut locator_staged = false;
      let exported_hash = if !changed {
        if !validate_existing_transfer_entry(output, &source_key, EntryType::DirectoryIndex, "backup directory identity", budget)? {
          store_directory_entry_preserving_version(output, &source_key, &source_value, flags, header.entry_version)?;
          mutated = true;
        }
        if source_is_btree {
          copy_reachable_btree_nodes(&source_value, header.entry_version, source, output, budget, flags)?;
        }
        source_key
      } else if source_is_btree {
        let rebuilt_hash = crate::engine::btree::btree_from_entries(output, retained, hash_length, &algorithm)?;
        let rebuilt_header = output
          .get_entry_header_including_deleted(&rebuilt_hash)?
          .ok_or_else(|| EngineError::NotFound(format!("rebuilt transfer directory {} was not stored", hex::encode(&rebuilt_hash))))?;
        budget.reserve(
          u64::from(rebuilt_header.value_length)
            .checked_add(BACKUP_COLLECTION_OVERHEAD_BYTES)
            .ok_or_else(|| EngineError::ResourceExhausted("rebuilt transfer directory read estimate overflow".to_string()))?,
          "rebuilt transfer directory read admission failed",
        )?;
        let (_, _, rebuilt_value) = output
          .get_entry_including_deleted_bounded(&rebuilt_hash, rebuilt_header.value_length)?
          .ok_or_else(|| EngineError::NotFound(format!("rebuilt transfer directory {} disappeared", hex::encode(&rebuilt_hash))))?;
        store_directory_entry_preserving_version(output, &rebuilt_hash, &rebuilt_value, flags, rebuilt_header.entry_version)?;
        mutated = true;
        rebuilt_hash
      } else {
        let rebuilt_value = serialize_child_entries(&retained, hash_length)?;
        let rebuilt_hash = directory_content_hash(&rebuilt_value, &algorithm)?;
        store_directory_entry_preserving_version(output, &rebuilt_hash, &rebuilt_value, flags, 0)?;
        mutated = true;
        rebuilt_hash
      };

      let path_key = directory_path_hash(path, &algorithm)?;
      if path_key != exported_hash {
        let exported_header = output
          .get_entry_header_including_deleted(&exported_hash)?
          .ok_or_else(|| EngineError::NotFound(format!("exported directory {} was not stored", hex::encode(&exported_hash))))?;
        budget.reserve(
          u64::from(exported_header.value_length)
            .checked_add(BACKUP_COLLECTION_OVERHEAD_BYTES)
            .ok_or_else(|| EngineError::ResourceExhausted("exported directory read estimate overflow".to_string()))?,
          "exported directory read admission failed",
        )?;
        let (_, _, exported_value) = output
          .get_entry_including_deleted_bounded(&exported_hash, exported_header.value_length)?
          .ok_or_else(|| EngineError::NotFound(format!("exported directory {} disappeared", hex::encode(&exported_hash))))?;
        if should_store_transfer_alias(
          output,
          &path_key,
          EntryType::DirectoryIndex,
          &exported_value,
          flags,
          exported_header.entry_version,
          destination_mode,
          budget,
        )? {
          if destination_mode.coordinates_active_locators() {
            locator_batch.as_deref_mut().expect("active import mode creates a locator batch").replace(
              EntryType::DirectoryIndex,
              path_key,
              exported_value,
              flags,
              exported_header.entry_version,
              path.clone(),
              exported_hash.clone(),
            )?;
            locator_staged = true;
          } else {
            store_directory_entry_preserving_version(output, &path_key, &exported_value, flags, exported_header.entry_version)?;
          }
          mutated = true;
        }
      }
      if destination_mode.records_runtime_metrics() && mutated && !locator_staged {
        output.counters().record_write(0);
      }
      written_hashes.insert(path.clone(), exported_hash);
      directories_written += 1;
      directories_mutated += u64::from(mutated);
      budget.release(loaded_charge, "DirectoryIndex copy buffer release failed")?;
      Ok(())
    })();
    let release_result = budget.release_to(checkpoint, "transfer directory workspace release failed");
    match result {
      Ok(()) => release_result?,
      Err(error) => {
        return Err(preserve_import_primary_error(
          error,
          release_result,
          "Transfer directory copy failed and workspace cleanup also failed",
        ));
      }
    }
  }

  let root_hash = written_hashes
    .remove("/")
    .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "backup version tree does not contain a root directory".to_string() })?;
  Ok((directories_written, directories_mutated, root_hash))
}

#[allow(clippy::too_many_arguments)]
fn should_store_transfer_alias(
  output: &StorageEngine,
  key: &[u8],
  expected_type: EntryType,
  expected_value: &[u8],
  expected_flags: u8,
  expected_version: u8,
  mode: TransferDestinationMode,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<bool> {
  match mode {
    TransferDestinationMode::Artifact => {
      return Ok(!validate_existing_transfer_entry(output, key, expected_type, "backup artifact alias", budget)?);
    }
    TransferDestinationMode::FullImport => return Ok(true),
    TransferDestinationMode::HistoricalImport => return Ok(false),
    TransferDestinationMode::SparseImport => {}
  }

  let Some(header) = output.get_entry_header_including_deleted(key)? else {
    return Ok(true);
  };
  if header.entry_type != expected_type || header.flags != expected_flags || header.entry_version != expected_version {
    return Ok(true);
  }
  let charge = backup_entry_charge(&header)?;
  budget.reserve(charge, "sparse import alias comparison admission failed")?;
  let result = output.get_entry_including_deleted_verified_bounded(key, header.value_length);
  let matches = match result {
    Ok(Some((_header, _key, value))) => value == expected_value,
    Ok(None) => false,
    Err(error) => {
      return Err(preserve_import_primary_error(
        error,
        budget.release(charge, "failed sparse import alias comparison release failed"),
        "Sparse import alias comparison failed and reservation cleanup also failed",
      ));
    }
  };
  budget.release(charge, "sparse import alias comparison release failed")?;
  Ok(!matches)
}

fn collect_transfer_btree_entries<S: HistoricalEntrySource + ?Sized>(
  tree: &VersionTree,
  source: &S,
  root_data: &[u8],
  root_entry_version: u8,
  hash_length: usize,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<Vec<ChildEntry>> {
  let mut entries = Vec::new();
  let mut frontier = Vec::<(Vec<u8>, u64)>::new();
  let mut visited = std::collections::HashSet::<Vec<u8>>::new();
  collect_transfer_btree_node(root_data, root_entry_version, hash_length, budget, &mut frontier, &mut entries)?;
  while let Some((node_hash, frontier_charge)) = frontier.pop() {
    budget.release(frontier_charge, "transfer B-tree frontier release failed")?;
    if !visited.insert(node_hash.clone()) {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("duplicate or cyclic B-tree node {} while rebuilding transfer directory", hex::encode(node_hash)),
      });
    }
    budget.reserve(backup_collection_charge(node_hash.len())?, "transfer B-tree visited-set admission failed")?;
    let node_data = tree.btree_nodes.get(&node_hash).ok_or_else(|| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("missing retained B-tree node {} while rebuilding transfer directory", hex::encode(&node_hash)),
    })?;
    let node_header = source.historical_entry_header(&node_hash)?.ok_or_else(|| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("missing B-tree node header {} while rebuilding transfer directory", hex::encode(&node_hash)),
    })?;
    if node_header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "B-tree node {} resolved to {:?} while rebuilding transfer directory",
          hex::encode(&node_hash),
          node_header.entry_type
        ),
      });
    }
    collect_transfer_btree_node(node_data, node_header.entry_version, hash_length, budget, &mut frontier, &mut entries)?;
  }
  Ok(entries)
}

fn collect_transfer_btree_node(
  node_data: &[u8],
  entry_version: u8,
  hash_length: usize,
  budget: &mut OperationMemoryBudget,
  frontier: &mut Vec<(Vec<u8>, u64)>,
  entries: &mut Vec<ChildEntry>,
) -> EngineResult<()> {
  let parse_charge = scaled_backup_charge(node_data.len(), 4, "transfer B-tree parse estimate overflow")?;
  budget.reserve(parse_charge, "transfer B-tree parse admission failed")?;
  match crate::engine::btree::BTreeNode::deserialize(node_data, hash_length, entry_version)? {
    crate::engine::btree::BTreeNode::Leaf(leaf) => {
      for entry in leaf.entries {
        budget.reserve(child_entry_charge(&entry)?, "transfer B-tree child retention admission failed")?;
        entries.push(entry);
      }
    }
    crate::engine::btree::BTreeNode::Internal(internal) => {
      for child_hash in internal.children.into_iter().rev() {
        let charge = backup_collection_charge(child_hash.len())?;
        budget.reserve(charge, "transfer B-tree frontier admission failed")?;
        frontier.push((child_hash, charge));
      }
    }
  }
  budget.release(parse_charge, "transfer B-tree parse release failed")
}

fn path_depth(path: &str) -> usize {
  path.split('/').filter(|segment| !segment.is_empty()).count()
}

fn join_transfer_path(parent: &str, child: &str) -> String {
  if parent == "/" {
    format!("/{child}")
  } else {
    format!("{parent}/{child}")
  }
}

fn store_directory_entry_preserving_version(
  engine: &StorageEngine,
  key: &[u8],
  value: &[u8],
  flags: u8,
  entry_version: u8,
) -> EngineResult<()> {
  if flags == 0 {
    engine.store_entry_with_version(EntryType::DirectoryIndex, key, value, entry_version)?;
  } else {
    engine.store_entry_with_flags_and_version(EntryType::DirectoryIndex, key, value, flags, entry_version)?;
  }
  Ok(())
}

fn child_entry_charge(entry: &ChildEntry) -> EngineResult<u64> {
  let content_type_len = entry.content_type.as_ref().map_or(0, String::len);
  entry
    .hash
    .len()
    .checked_add(entry.name.len())
    .and_then(|bytes| bytes.checked_add(content_type_len))
    .and_then(|bytes| bytes.checked_add(BACKUP_COLLECTION_OVERHEAD_BYTES as usize))
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| EngineError::ResourceExhausted("root child retention estimate overflow".to_string()))
}

fn scaled_backup_charge(bytes: usize, multiplier: u64, context: &'static str) -> EngineResult<u64> {
  u64::try_from(bytes)
    .ok()
    .and_then(|bytes| bytes.checked_mul(multiplier))
    .and_then(|bytes| bytes.checked_add(BACKUP_COLLECTION_OVERHEAD_BYTES))
    .ok_or_else(|| EngineError::ResourceExhausted(context.to_string()))
}

fn backup_collection_charge(bytes: usize) -> EngineResult<u64> {
  u64::try_from(bytes)
    .ok()
    .and_then(|bytes| bytes.checked_add(BACKUP_COLLECTION_OVERHEAD_BYTES))
    .ok_or_else(|| EngineError::ResourceExhausted("backup collection estimate overflow".to_string()))
}

fn copy_reachable_btree_nodes<S: HistoricalEntrySource + ?Sized>(
  root_data: &[u8],
  root_entry_version: u8,
  source: &S,
  output: &StorageEngine,
  budget: &mut OperationMemoryBudget,
  flags: u8,
) -> EngineResult<()> {
  let hash_length = output.hash_algo().hash_length();
  let mut frontier = Vec::<(Vec<u8>, u64)>::new();
  enqueue_btree_children(root_data, root_entry_version, hash_length, budget, &mut frontier)?;
  while let Some((node_hash, frontier_charge)) = frontier.pop() {
    budget.record_work(1)?;
    budget.release(frontier_charge, "B-tree copy frontier release failed")?;
    if validate_existing_transfer_entry(output, &node_hash, EntryType::DirectoryIndex, "backup B-tree node", budget)? {
      continue;
    }
    let ((header, key, value), charge) = required_backup_entry(source, &node_hash, "B-tree DirectoryIndex", budget)?;
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("B-tree node {} resolved to {:?}", hex::encode(&node_hash), header.entry_type),
      });
    }
    enqueue_btree_children(&value, header.entry_version, hash_length, budget, &mut frontier)?;
    store_directory_entry_preserving_version(output, &key, &value, flags, header.entry_version)?;
    budget.release(charge, "B-tree node buffer release failed")?;
  }
  Ok(())
}

fn enqueue_btree_children(
  node_data: &[u8],
  entry_version: u8,
  hash_length: usize,
  budget: &mut OperationMemoryBudget,
  frontier: &mut Vec<(Vec<u8>, u64)>,
) -> EngineResult<()> {
  let parse_charge = scaled_backup_charge(node_data.len(), 3, "B-tree copy parse estimate overflow")?;
  budget.reserve(parse_charge, "B-tree copy parse admission failed")?;
  let result = (|| {
    if let crate::engine::btree::BTreeNode::Internal(internal) =
      crate::engine::btree::BTreeNode::deserialize(node_data, hash_length, entry_version)?
    {
      for child_hash in internal.children.into_iter().rev() {
        let charge = backup_collection_charge(child_hash.len())?;
        budget.reserve(charge, "B-tree copy frontier admission failed")?;
        frontier.push((child_hash, charge));
      }
    }
    Ok(())
  })();
  let release_result = budget.release(parse_charge, "B-tree copy parse release failed");
  match result {
    Ok(()) => release_result,
    Err(error) => {
      Err(preserve_import_primary_error(error, release_result, "B-tree copy parsing failed and reservation cleanup also failed"))
    }
  }
}

/// Copy all Snapshot-type entries from source to output. These represent
/// the version history chain — without them, the imported database has
/// no snapshot list even if the per-snapshot data is present.
fn copy_snapshot_entries(source: &StorageEngine, output: &StorageEngine, budget: &mut OperationMemoryBudget) -> EngineResult<u64> {
  use crate::engine::kv_store::KV_TYPE_SNAPSHOT;
  let mut copied = 0u64;
  let (snapshot_entries, inventory_charge) = load_type_inventory(source, KV_TYPE_SNAPSHOT, budget, "snapshot inventory admission failed")?;
  for entry in snapshot_entries {
    budget.record_work(1)?;
    if !validate_existing_transfer_entry(output, &entry.hash, EntryType::Snapshot, "backup snapshot identity", budget)? {
      let ((header, key, value), charge) = required_backup_entry(source, &entry.hash, "Snapshot", budget)?;
      if header.entry_type != EntryType::Snapshot {
        return Err(EngineError::CorruptEntry {
          offset: entry.offset,
          reason: format!("snapshot inventory key resolved to {:?}", header.entry_type),
        });
      }
      output.store_entry(EntryType::Snapshot, &key, &value)?;
      budget.release(charge, "snapshot copy buffer release failed")?;
      copied += 1;
    }
  }
  budget.release(inventory_charge, "snapshot inventory release failed")?;
  Ok(copied)
}

type BackupEntry = HistoricalEntry;

fn load_backup_entry<S: HistoricalEntrySource + ?Sized>(
  source: &S,
  hash: &[u8],
  budget: &mut OperationMemoryBudget,
) -> EngineResult<Option<(BackupEntry, u64)>> {
  let Some(header) = source.historical_entry_header(hash)? else {
    return Ok(None);
  };
  let charge = u64::from(header.key_length)
    .checked_add(u64::from(header.value_length))
    .and_then(|bytes| bytes.checked_add(header.header_size() as u64))
    .and_then(|bytes| bytes.checked_add(96))
    .ok_or_else(|| EngineError::ResourceExhausted("backup entry buffer estimate overflow".to_string()))?;
  budget.reserve(charge, "entry buffer admission failed")?;
  match source.historical_entry_bounded(hash, header.value_length) {
    Ok(Some(entry)) => Ok(Some((entry, charge))),
    Ok(None) => {
      budget.release(charge, "missing entry buffer release failed")?;
      Ok(None)
    }
    Err(error) => {
      budget.release(charge, "failed entry buffer release failed")?;
      Err(error)
    }
  }
}

fn required_backup_entry<S: HistoricalEntrySource + ?Sized>(
  source: &S,
  hash: &[u8],
  kind: &str,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<(BackupEntry, u64)> {
  let (entry, charge) = load_backup_entry(source, hash, budget)?.ok_or_else(|| EngineError::CorruptEntry {
    offset: 0,
    reason: format!("backup references missing {kind} entry {}", hex::encode(hash)),
  })?;
  if entry.1 != hash {
    let error = EngineError::CorruptEntry {
      offset: 0,
      reason: format!("backup {kind} lookup {} returned stored key {}", hex::encode(hash), hex::encode(&entry.1)),
    };
    return Err(preserve_import_primary_error(
      error,
      budget.release(charge, "mismatched backup entry buffer release failed"),
      "Backup key validation failed and reservation cleanup also failed",
    ));
  }
  Ok((entry, charge))
}

fn validate_existing_transfer_entry(
  output: &StorageEngine,
  key: &[u8],
  expected_type: EntryType,
  context: &str,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<bool> {
  let Some(kv_entry) = output.get_kv_entry(key)? else {
    return Ok(false);
  };
  if kv_entry.is_deleted() {
    return Ok(false);
  }
  if kv_entry.entry_type() != expected_type.to_kv_type() {
    return Err(EngineError::CorruptEntry {
      offset: kv_entry.offset,
      reason: format!("{context} {} collides with {:?}", hex::encode(key), kv_entry.entry_type()),
    });
  }

  let header = output.get_entry_header(key)?.ok_or_else(|| EngineError::CorruptEntry {
    offset: kv_entry.offset,
    reason: format!("{context} {} disappeared during validation", hex::encode(key)),
  })?;
  let charge = backup_entry_charge(&header)?;
  budget.reserve(charge, "existing transfer entry validation admission failed")?;
  let result = output.get_entry_verified_bounded(key, header.value_length).and_then(|entry| {
    let (stored_header, stored_key, _value) = entry.ok_or_else(|| EngineError::CorruptEntry {
      offset: kv_entry.offset,
      reason: format!("{context} {} disappeared during verified read", hex::encode(key)),
    })?;
    if stored_key != key || stored_header.entry_type != expected_type {
      return Err(EngineError::CorruptEntry {
        offset: kv_entry.offset,
        reason: format!("{context} {} does not resolve to an exact {expected_type:?} entry", hex::encode(key)),
      });
    }
    Ok(true)
  });
  let release_result = budget.release(charge, "existing transfer entry validation release failed");
  match result {
    Ok(exists) => {
      release_result?;
      Ok(exists)
    }
    Err(error) => Err(preserve_import_primary_error(
      error,
      release_result,
      "Existing transfer entry validation failed and reservation cleanup also failed",
    )),
  }
}

fn load_type_inventory(
  engine: &StorageEngine,
  entry_type: u8,
  budget: &mut OperationMemoryBudget,
  context: &'static str,
) -> EngineResult<(Vec<crate::engine::kv_store::KVEntry>, u64)> {
  let mut charge = 0u64;
  let entries = engine.kv_entries_by_type_admitted(entry_type, |count| {
    let count = u64::try_from(count)
      .map_err(|_| EngineError::ResourceExhausted("backup inventory count does not fit memory accounting".to_string()))?;
    charge = count
      .checked_mul(KV_INVENTORY_ENTRY_BYTES)
      .ok_or_else(|| EngineError::ResourceExhausted("backup inventory estimate overflow".to_string()))?;
    budget.reserve(charge, context)
  })?;
  let required = u64::try_from(entries.len())
    .ok()
    .and_then(|count| count.checked_mul(KV_INVENTORY_ENTRY_BYTES))
    .ok_or_else(|| EngineError::ResourceExhausted("backup inventory allocation estimate overflow".to_string()))?;
  if required != charge {
    return Err(EngineError::IoError(std::io::Error::other(format!(
      "backup inventory count changed within one immutable KV snapshot: admitted {} bytes, materialized {} bytes",
      charge, required
    ))));
  }
  Ok((entries, charge))
}

fn load_snapshot_infos(source: &StorageEngine, budget: &mut OperationMemoryBudget) -> EngineResult<Vec<SnapshotInfo>> {
  const SNAPSHOT_DECODE_AMPLIFICATION: u64 = 8;

  let (entries, inventory_charge) =
    load_type_inventory(source, crate::engine::kv_store::KV_TYPE_SNAPSHOT, budget, "snapshot inventory admission failed")?;
  let vector_charge = u64::try_from(entries.len())
    .ok()
    .and_then(|count| count.checked_mul((std::mem::size_of::<SnapshotInfo>() + 32) as u64))
    .ok_or_else(|| EngineError::ResourceExhausted("snapshot list allocation estimate overflow".to_string()))?;
  budget.reserve(vector_charge, "snapshot list admission failed")?;
  let mut snapshots = Vec::with_capacity(entries.len());
  let hash_length = source.hash_algo().hash_length();

  for entry in entries {
    budget.record_work(1)?;
    let ((header, _key, value), raw_charge) = required_backup_entry(source, &entry.hash, "Snapshot", budget)?;
    if header.entry_type != EntryType::Snapshot {
      return Err(EngineError::CorruptEntry {
        offset: entry.offset,
        reason: format!("snapshot inventory key resolved to {:?}", header.entry_type),
      });
    }
    let retained_charge = raw_charge
      .checked_mul(SNAPSHOT_DECODE_AMPLIFICATION)
      .ok_or_else(|| EngineError::ResourceExhausted("snapshot decode allocation estimate overflow".to_string()))?;
    budget.reserve(retained_charge, "snapshot decode admission failed")?;
    let snapshot = SnapshotInfo::deserialize(&value, hash_length, header.entry_version)?;
    budget.release(raw_charge, "snapshot serialized buffer release failed")?;
    snapshots.push(snapshot);
  }

  budget.release(inventory_charge, "snapshot inventory release failed")?;
  snapshots.sort_by_key(|snapshot| snapshot.created_at);
  Ok(snapshots)
}

/// Result of an export operation.
#[derive(Debug, Clone)]
pub struct ExportResult {
  pub chunks_written: u64,
  pub files_written: u64,
  pub directories_written: u64,
  pub version_hash: Vec<u8>,
  pub snapshots_written: u64,
}

impl std::fmt::Display for ExportResult {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    if self.snapshots_written > 0 {
      write!(
        f,
        "Export complete.\n  Files: {}\n  Chunks: {}\n  Directories: {}\n  Snapshots: {}\n  HEAD: {}",
        self.files_written,
        self.chunks_written,
        self.directories_written,
        self.snapshots_written,
        hex::encode(&self.version_hash),
      )
    } else {
      write!(
        f,
        "Export complete.\n  Files: {}\n  Chunks: {}\n  Directories: {}\n  Version: {}",
        self.files_written,
        self.chunks_written,
        self.directories_written,
        hex::encode(&self.version_hash),
      )
    }
  }
}

/// Create a patch .aeordb containing only the changeset between two versions.
///
/// The output contains: new/changed chunks, updated FileRecords, updated
/// DirectoryIndexes, and DeletionRecords for removed files.
/// backup_type = 2 (patch), with base_hash and target_hash set to the
/// LogicalBackup-selected directory roots.
///
/// Only chunks that don't exist in the base version are included.
pub fn create_patch(source: &StorageEngine, from_hash: &[u8], to_hash: &[u8], output_path: &str) -> EngineResult<PatchResult> {
  create_patch_controlled(source, from_hash, to_hash, output_path, None)
}

pub fn create_patch_with_cancellation(
  source: &StorageEngine,
  from_hash: &[u8],
  to_hash: &[u8],
  output_path: &str,
  cancellation: &CancellationToken,
) -> EngineResult<PatchResult> {
  create_patch_controlled(source, from_hash, to_hash, output_path, Some(cancellation))
}

fn create_patch_controlled(
  source: &StorageEngine,
  from_hash: &[u8],
  to_hash: &[u8],
  output_path: &str,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<PatchResult> {
  let mut budget = backup_budget(source, cancellation)?;
  create_patch_with_budget(source, from_hash, to_hash, output_path, &mut budget)
}

fn create_patch_inner(
  source: &StorageEngine,
  from_hash: &[u8],
  to_hash: &[u8],
  output_path: &str,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<PatchResult> {
  budget.check_cancellation()?;
  let mut base_tree =
    walk_version_tree_for_transfer_with_budget(source, from_hash, SystemFamilyTransferOperationV1::LogicalBackup, false, budget)?;
  let mut target_tree =
    walk_version_tree_for_transfer_with_budget(source, to_hash, SystemFamilyTransferOperationV1::LogicalBackup, false, budget)?;
  let resolver = SystemFamilyPolicyResolver::new(source.hash_algo())?;
  prune_empty_structural_directories(&mut base_tree, resolver, SystemFamilyTransferOperationV1::LogicalBackup)?;
  prune_empty_structural_directories(&mut target_tree, resolver, SystemFamilyTransferOperationV1::LogicalBackup)?;
  if version_trees_semantically_equal(&base_tree, &target_tree) {
    return Err(EngineError::NotFound("No changes visible under logical-backup policy between the two versions".to_string()));
  }
  let diff = diff_trees_with_budget(&base_tree, &target_tree, budget)?;

  if diff.is_empty() {
    return Err(EngineError::NotFound("No changes between the two versions".to_string()));
  }

  budget.check_cancellation()?;
  let output = StorageEngine::create_with_memory_coordinator(output_path, source.memory_coordinator())?;

  // Raw source roots are not stable patch identities after a filtered logical
  // export/import rebuild. Keep both selected directory closures in the sparse
  // artifact so import can prove semantic base equivalence while unchanged
  // file records and chunks still come from the target overlay.
  let (_, _, logical_base_hash) = write_transfer_directories(
    &base_tree,
    source,
    &output,
    resolver,
    SystemFamilyTransferOperationV1::LogicalBackup,
    TransferDestinationMode::Artifact,
    None,
    budget,
  )?;
  let (_, _, logical_target_hash) = write_transfer_directories(
    &target_tree,
    source,
    &output,
    resolver,
    SystemFamilyTransferOperationV1::LogicalBackup,
    TransferDestinationMode::Artifact,
    None,
    budget,
  )?;
  output.set_backup_info(2, &logical_base_hash, &logical_target_hash)?;

  let mut chunks_written = 0u64;
  let mut files_added = 0u64;
  let mut files_modified = 0u64;
  let mut files_deleted = 0u64;
  let dirs_written = u64::try_from(diff.changed_directories.len()).unwrap_or(u64::MAX);

  // Write only NEW chunks (chunks in target but not in base)
  for chunk_hash in &diff.new_chunks {
    budget.record_work(1)?;
    let ((header, key, value), charge) = required_backup_entry(source, chunk_hash, "patch chunk", budget)?;
    if header.entry_type != EntryType::Chunk {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("patch chunk {} resolved to {:?}", hex::encode(chunk_hash), header.entry_type),
      });
    }
    output.store_entry(EntryType::Chunk, &key, &value)?;
    budget.release(charge, "patch chunk buffer release failed")?;
    chunks_written += 1;
  }

  // Write added FileRecords at both content-hash and path-hash keys
  let patch_algo = output.hash_algo();
  for (path, (file_hash, _record)) in &diff.added {
    budget.record_work(1)?;
    let ((header, key, value), charge) = required_backup_entry(source, file_hash, "added patch FileRecord", budget)?;
    if header.entry_type != EntryType::FileRecord {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("added patch file '{}' resolved to {:?}", path, header.entry_type),
      });
    }
    store_file_record_entry_preserving_version(&output, &key, &value, header.flags, header.entry_version)?;
    let path_key = file_path_hash(path, &patch_algo)?;
    if path_key != key {
      store_file_record_entry_preserving_version(&output, &path_key, &value, header.flags, header.entry_version)?;
    }
    budget.release(charge, "added patch FileRecord buffer release failed")?;
    files_added += 1;
  }

  // Write modified FileRecords at both content-hash and path-hash keys
  for (path, (file_hash, _record)) in &diff.modified {
    budget.record_work(1)?;
    let ((header, key, value), charge) = required_backup_entry(source, file_hash, "modified patch FileRecord", budget)?;
    if header.entry_type != EntryType::FileRecord {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("modified patch file '{}' resolved to {:?}", path, header.entry_type),
      });
    }
    store_file_record_entry_preserving_version(&output, &key, &value, header.flags, header.entry_version)?;
    let path_key = file_path_hash(path, &patch_algo)?;
    if path_key != key {
      store_file_record_entry_preserving_version(&output, &path_key, &value, header.flags, header.entry_version)?;
    }
    budget.release(charge, "modified patch FileRecord buffer release failed")?;
    files_modified += 1;
  }

  // Write DeletionRecords for deleted files
  for path in &diff.deleted {
    budget.record_work(1)?;
    let charge = patch_deletion_charge(path)?;
    budget.reserve(charge, "patch file deletion buffer admission failed")?;
    let algo = source.hash_algo();
    let deletion_record = DeletionRecord::new(path.clone(), Some("patch-deletion".to_string()));
    let deletion_data = deletion_record.serialize();
    let deletion_key = file_path_hash(path, &algo)?;
    output.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_data)?;
    budget.release(charge, "patch file deletion buffer release failed")?;
    files_deleted += 1;
  }

  // Write added symlinks at both content-hash and path-hash keys
  let symlink_algo = output.hash_algo();
  for (path, (symlink_hash, _record)) in &diff.symlinks_added {
    budget.record_work(1)?;
    let ((header, key, value), charge) = required_backup_entry(source, symlink_hash, "added patch Symlink", budget)?;
    if header.entry_type != EntryType::Symlink {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("added patch symlink '{}' resolved to {:?}", path, header.entry_type),
      });
    }
    output.store_entry_with_flags_and_version(EntryType::Symlink, &key, &value, header.flags, header.entry_version)?;
    let path_key = symlink_path_hash(path, &symlink_algo)?;
    if path_key != key {
      output.store_entry_with_flags_and_version(EntryType::Symlink, &path_key, &value, header.flags, header.entry_version)?;
    }
    budget.release(charge, "added patch Symlink buffer release failed")?;
  }

  // Write modified symlinks at both content-hash and path-hash keys
  for (path, (symlink_hash, _record)) in &diff.symlinks_modified {
    budget.record_work(1)?;
    let ((header, key, value), charge) = required_backup_entry(source, symlink_hash, "modified patch Symlink", budget)?;
    if header.entry_type != EntryType::Symlink {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("modified patch symlink '{}' resolved to {:?}", path, header.entry_type),
      });
    }
    output.store_entry_with_flags_and_version(EntryType::Symlink, &key, &value, header.flags, header.entry_version)?;
    let path_key = symlink_path_hash(path, &symlink_algo)?;
    if path_key != key {
      output.store_entry_with_flags_and_version(EntryType::Symlink, &path_key, &value, header.flags, header.entry_version)?;
    }
    budget.release(charge, "modified patch Symlink buffer release failed")?;
  }

  // Write DeletionRecords for deleted symlinks
  for path in &diff.symlinks_deleted {
    budget.record_work(1)?;
    let charge = patch_deletion_charge(path)?;
    budget.reserve(charge, "patch symlink deletion buffer admission failed")?;
    let algo = source.hash_algo();
    let deletion_record = DeletionRecord::new(path.clone(), Some("patch-deletion".to_string()));
    let deletion_data = deletion_record.serialize();
    let deletion_key = symlink_path_hash(path, &algo)?;
    output.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_data)?;
    budget.release(charge, "patch symlink deletion buffer release failed")?;
  }

  // Directory closure was normalized and stored above. The public statistic
  // remains the number of semantically changed directories rather than every
  // routing record needed to prove both sparse roots.

  // Set HEAD to the selected target root.
  budget.check_cancellation()?;
  output.update_head(&logical_target_hash)?;

  Ok(PatchResult {
    chunks_written,
    files_added,
    files_modified,
    files_deleted,
    directories_written: dirs_written,
    from_hash: logical_base_hash,
    to_hash: logical_target_hash,
  })
}

fn patch_deletion_charge(path: &str) -> EngineResult<u64> {
  u64::try_from(path.len())
    .ok()
    .and_then(|bytes| bytes.checked_mul(2))
    .and_then(|bytes| bytes.checked_add(256))
    .ok_or_else(|| EngineError::ResourceExhausted("patch deletion allocation estimate overflow".to_string()))
}

/// Create a patch from a named snapshot (or HEAD) to another.
pub fn create_patch_from_snapshots(
  source: &StorageEngine,
  from_snapshot: &str,
  to_snapshot: Option<&str>,
  output_path: &str,
) -> EngineResult<PatchResult> {
  let mut budget = backup_budget(source, None)?;
  create_patch_from_snapshots_with_budget(source, from_snapshot, to_snapshot, output_path, &mut budget)
}

fn create_patch_from_snapshots_with_budget(
  source: &StorageEngine,
  from_snapshot: &str,
  to_snapshot: Option<&str>,
  output_path: &str,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<PatchResult> {
  let snapshot_checkpoint = budget.checkpoint();
  let snapshots = load_snapshot_infos(source, budget)?;

  let from_hash = snapshots
    .iter()
    .find(|s| s.name == from_snapshot)
    .map(|s| s.root_hash.clone())
    .ok_or_else(|| EngineError::NotFound(format!("Snapshot '{}' not found", from_snapshot)))?;

  let to_hash = match to_snapshot {
    Some(name) => snapshots
      .iter()
      .find(|s| s.name == name)
      .map(|s| s.root_hash.clone())
      .ok_or_else(|| EngineError::NotFound(format!("Snapshot '{}' not found", name)))?,
    None => source.head_hash()?,
  };
  drop(snapshots);
  budget.release_to(snapshot_checkpoint, "snapshot reference release failed")?;

  create_patch_with_budget(source, &from_hash, &to_hash, output_path, budget)
}

/// Resolve each reference as an exact snapshot name first, then as a complete
/// hexadecimal version hash. Snapshot read failures are never converted into
/// hash fallbacks.
pub fn create_patch_from_references(
  source: &StorageEngine,
  from_reference: &str,
  to_reference: Option<&str>,
  output_path: &str,
) -> EngineResult<PatchResult> {
  let mut budget = backup_budget(source, None)?;
  let snapshot_checkpoint = budget.checkpoint();
  let snapshots = load_snapshot_infos(source, &mut budget)?;
  let expected_hash_length = source.hash_algo().hash_length();
  let resolve = |reference: &str| -> EngineResult<Vec<u8>> {
    if let Some(snapshot) = snapshots.iter().find(|snapshot| snapshot.name == reference) {
      return Ok(snapshot.root_hash.clone());
    }
    let hash = hex::decode(reference).map_err(|error| {
      EngineError::InvalidInput(format!("version reference '{}' is neither a snapshot name nor hexadecimal hash: {}", reference, error))
    })?;
    if hash.len() != expected_hash_length {
      return Err(EngineError::InvalidInput(format!(
        "version reference '{}' decodes to {} bytes; expected {}",
        reference,
        hash.len(),
        expected_hash_length
      )));
    }
    Ok(hash)
  };
  let from_hash = resolve(from_reference)?;
  let to_hash = match to_reference {
    Some(reference) => resolve(reference)?,
    None => source.head_hash()?,
  };
  drop(snapshots);
  budget.release_to(snapshot_checkpoint, "version reference release failed")?;
  create_patch_with_budget(source, &from_hash, &to_hash, output_path, &mut budget)
}

fn create_patch_with_budget(
  source: &StorageEngine,
  from_hash: &[u8],
  to_hash: &[u8],
  output_path: &str,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<PatchResult> {
  if std::path::Path::new(output_path).exists() {
    return Err(EngineError::AlreadyExists(format!("patch destination '{}' already exists", output_path)));
  }
  let part = PartialBackupArtifact::new(output_path);
  match create_patch_inner(source, from_hash, to_hash, part.path(), budget) {
    Ok(result) => {
      crate::engine::durability::rename_durable(part.path(), output_path)?;
      Ok(result)
    }
    Err(error) => Err(error),
  }
}

/// Result of a patch/diff operation.
#[derive(Debug, Clone)]
pub struct PatchResult {
  pub chunks_written: u64,
  pub files_added: u64,
  pub files_modified: u64,
  pub files_deleted: u64,
  pub directories_written: u64,
  pub from_hash: Vec<u8>,
  pub to_hash: Vec<u8>,
}

impl std::fmt::Display for PatchResult {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
            f,
            "Patch created.\n  Files added: {}\n  Files modified: {}\n  Files deleted: {}\n  Chunks: {}\n  Directories: {}\n  From: {}\n  To:   {}",
            self.files_added,
            self.files_modified,
            self.files_deleted,
            self.chunks_written,
            self.directories_written,
            hex::encode(&self.from_hash),
            hex::encode(&self.to_hash),
        )
  }
}

/// Detect whether a backup contains registry-selected state that requires a
/// privileged import rather than an ordinary data import.
///
/// Legacy entry flags are intentionally not authority here: an imported
/// artifact can be old, malformed, or adversarially mislabeled. The embedded
/// SystemFamily registry and each decoded record path decide the boundary.
pub fn backup_contains_system_data(backup: &StorageEngine) -> EngineResult<bool> {
  let mut budget = backup_budget(backup, None)?;
  let resolver = SystemFamilyPolicyResolver::new(backup.hash_algo())?;
  let hash_length = backup.hash_algo().hash_length();

  let (entries, inventory_charge) =
    load_type_inventory(backup, KV_TYPE_FILE_RECORD, &mut budget, "system-data FileRecord inventory admission failed")?;
  for entry in entries {
    budget.record_work(1)?;
    let ((header, _key, value), charge) = required_backup_entry(backup, &entry.hash, "system-data FileRecord", &mut budget)?;
    if header.entry_type != EntryType::FileRecord {
      return Err(EngineError::CorruptEntry {
        offset: entry.offset,
        reason: format!("system-data FileRecord inventory key resolved to {:?}", header.entry_type),
      });
    }
    let record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
    budget.release(charge, "system-data FileRecord buffer release failed")?;
    if path_requires_privileged_import(resolver, &record.path)? {
      return Ok(true);
    }
  }
  budget.release(inventory_charge, "system-data FileRecord inventory release failed")?;

  let (entries, inventory_charge) =
    load_type_inventory(backup, KV_TYPE_SYMLINK, &mut budget, "system-data Symlink inventory admission failed")?;
  for entry in entries {
    budget.record_work(1)?;
    let ((header, _key, value), charge) = required_backup_entry(backup, &entry.hash, "system-data Symlink", &mut budget)?;
    if header.entry_type != EntryType::Symlink {
      return Err(EngineError::CorruptEntry {
        offset: entry.offset,
        reason: format!("system-data Symlink inventory key resolved to {:?}", header.entry_type),
      });
    }
    let record = SymlinkRecord::deserialize(&value, header.entry_version)?;
    budget.release(charge, "system-data Symlink buffer release failed")?;
    if path_requires_privileged_import(resolver, &record.path)? {
      return Ok(true);
    }
  }
  budget.release(inventory_charge, "system-data Symlink inventory release failed")?;

  let (entries, inventory_charge) =
    load_type_inventory(backup, KV_TYPE_DELETION, &mut budget, "system-data deletion inventory admission failed")?;
  for entry in entries {
    budget.record_work(1)?;
    let ((header, _key, value), charge) = required_backup_entry(backup, &entry.hash, "system-data DeletionRecord", &mut budget)?;
    if header.entry_type != EntryType::DeletionRecord {
      return Err(EngineError::CorruptEntry {
        offset: entry.offset,
        reason: format!("system-data deletion inventory key resolved to {:?}", header.entry_type),
      });
    }
    let record = DeletionRecord::deserialize(&value, header.entry_version)?;
    budget.release(charge, "system-data deletion buffer release failed")?;
    if path_requires_privileged_import(resolver, &record.path)? {
      return Ok(true);
    }
  }
  budget.release(inventory_charge, "system-data deletion inventory release failed")?;

  let snapshots = load_snapshot_infos(backup, &mut budget)?;
  if !snapshots.is_empty() && entry_type_requires_privileged_import(resolver, EntryType::Snapshot)? {
    return Ok(true);
  }
  Ok(false)
}

fn path_requires_privileged_import(resolver: SystemFamilyPolicyResolver, path: &str) -> EngineResult<bool> {
  requires_privileged_import(
    resolver.transfer_path_selection(path, SystemFamilyTransferOperationV1::Import)?,
    resolver.transfer_path_selection(path, SystemFamilyTransferOperationV1::DataExport)?,
    path,
  )
}

fn entry_type_requires_privileged_import(resolver: SystemFamilyPolicyResolver, entry_type: EntryType) -> EngineResult<bool> {
  requires_privileged_import(
    resolver.transfer_entry_type_selection(entry_type, SystemFamilyTransferOperationV1::Import)?,
    resolver.transfer_entry_type_selection(entry_type, SystemFamilyTransferOperationV1::DataExport)?,
    &format!("{:?}", entry_type),
  )
}

fn requires_privileged_import(
  import_selection: TransferPathSelection,
  data_export_selection: TransferPathSelection,
  subject: &str,
) -> EngineResult<bool> {
  match import_selection {
    TransferPathSelection::Omit => Ok(false),
    TransferPathSelection::StructuralContainer => Err(EngineError::SystemFamilyPolicy {
      code: "system_family_structural_leaf",
      reason: format!("import inspection uses structural container '{subject}' as a leaf"),
    }),
    TransferPathSelection::Include => match data_export_selection {
      TransferPathSelection::Include => Ok(false),
      TransferPathSelection::Omit => Ok(true),
      TransferPathSelection::StructuralContainer => Err(EngineError::SystemFamilyPolicy {
        code: "system_family_structural_leaf",
        reason: format!("data export inspection uses structural container '{subject}' as a leaf"),
      }),
    },
  }
}

fn validate_import_leaf_policies(
  backup: &StorageEngine,
  file_entries: &[crate::engine::kv_store::KVEntry],
  symlink_entries: &[crate::engine::kv_store::KVEntry],
  deletion_entries: &[crate::engine::kv_store::KVEntry],
  operation: SystemFamilyTransferOperationV1,
  maximum_value_length: u32,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<Vec<SelectedPatchDeletion>> {
  let resolver = SystemFamilyPolicyResolver::new(backup.hash_algo())?;
  let hash_length = backup.hash_algo().hash_length();

  for entry in file_entries {
    budget.record_work(1)?;
    let (header, _key, value) =
      required_import_entry(backup, &entry.hash, EntryType::FileRecord, maximum_value_length, "FileRecord policy validation")?;
    let record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
    validate_import_leaf_selection(resolver, &record.path, EntryType::FileRecord, operation)?;
  }

  for entry in symlink_entries {
    budget.record_work(1)?;
    let (header, _key, value) =
      required_import_entry(backup, &entry.hash, EntryType::Symlink, maximum_value_length, "Symlink policy validation")?;
    let record = SymlinkRecord::deserialize(&value, header.entry_version)?;
    validate_import_leaf_selection(resolver, &record.path, EntryType::Symlink, operation)?;
  }

  let mut selected_deletions = Vec::new();
  for entry in deletion_entries {
    budget.record_work(1)?;
    let (header, key, value) =
      required_import_entry(backup, &entry.hash, EntryType::DeletionRecord, maximum_value_length, "DeletionRecord policy validation")?;
    let record = DeletionRecord::deserialize(&value, header.entry_version)?;
    let file_key = file_path_hash(&record.path, &backup.hash_algo())?;
    let symlink_key = symlink_path_hash(&record.path, &backup.hash_algo())?;
    let entry_type = if key == file_key {
      EntryType::FileRecord
    } else if key == symlink_key {
      EntryType::Symlink
    } else {
      return Err(EngineError::CorruptEntry {
        offset: entry.offset,
        reason: format!("DeletionRecord key does not match file or symlink path '{}'", record.path),
      });
    };
    if validate_import_leaf_selection(resolver, &record.path, EntryType::DeletionRecord, operation)? {
      let retained_bytes = key
        .len()
        .checked_add(record.path.len())
        .and_then(|bytes| bytes.checked_add(record.reason.as_ref().map_or(0, String::len)))
        .and_then(|bytes| bytes.checked_add(hash_length))
        .ok_or_else(|| EngineError::ResourceExhausted("selected patch deletion estimate overflow".to_string()))?;
      let retained_charge = backup_collection_charge(retained_bytes)?;
      budget.reserve(retained_charge, "selected patch deletion admission failed")?;
      selected_deletions.push(SelectedPatchDeletion {
        path_key: key,
        record,
        flags: header.flags,
        entry_version: header.entry_version,
        entry_type,
        previous_identity: None,
        retire_locator: false,
        retained_charge,
      });
    }
  }
  Ok(selected_deletions)
}

fn validate_import_leaf_selection(
  resolver: SystemFamilyPolicyResolver,
  path: &str,
  entry_type: EntryType,
  operation: SystemFamilyTransferOperationV1,
) -> EngineResult<bool> {
  match resolver.transfer_path_selection(path, operation)? {
    TransferPathSelection::Include => Ok(true),
    TransferPathSelection::Omit => Ok(false),
    TransferPathSelection::StructuralContainer => Err(EngineError::SystemFamilyPolicy {
      code: "system_family_structural_leaf",
      reason: format!("{} uses structural container '{}' as a {:?}", operation.name(), path, entry_type),
    }),
  }
}

/// Import an export or patch .aeordb file into a target database.
///
/// For full exports (backup_type=1): stores all entries into target.
/// For patches (backup_type=2): verifies an exact or selected-semantic base match, then applies changes.
///
/// Does NOT automatically promote HEAD unless `promote` is true.
///
/// `include_system`: when true, system entries (users, groups, keys) from the
/// backup are imported. The CALLER must verify root-key authority before
/// passing true. When false, system entries in the backup are silently skipped.
/// Check whether the target database contains any registry-selected data.
/// Concealed operational/bootstrap families do not make a fresh target
/// non-empty, while portable namespace state such as permissions does.
fn is_target_empty(target: &StorageEngine) -> EngineResult<bool> {
  let ops = crate::engine::DirectoryOps::new(target);
  let family_policy = SystemFamilyPolicyResolver::new(target.hash_algo())?;
  let children = match ops.list_directory_strict("/") {
    Ok(c) => c,
    Err(EngineError::NotFound(_)) => return Ok(true),
    Err(other) => return Err(other),
  };
  for child in &children {
    if family_policy.generic_data_path_is_visible(&format!("/{}", child.name))? {
      return Ok(false);
    }
  }
  Ok(true)
}

/// What to do with an existing target when importing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
  /// Refuse to import unless the target is empty (or `force=true`). Use this
  /// when restoring from a backup — overlaying onto live data is almost
  /// always wrong.
  Restore,
  /// Union the backup into the target. Use when you genuinely want to layer
  /// backup contents on top of existing data. This is the original behavior.
  Merge,
}

impl ImportMode {
  pub fn parse(s: Option<&str>) -> EngineResult<Self> {
    match s {
      Some("restore") => Ok(ImportMode::Restore),
      Some("merge") | None => Ok(ImportMode::Merge),
      Some(other) => Err(EngineError::InvalidInput(format!("import mode must be 'restore' or 'merge', got '{}'", other))),
    }
  }
}

struct PatchOverlaySource<'a> {
  patch: &'a StorageEngine,
  target: &'a StorageEngine,
}

impl<'a> PatchOverlaySource<'a> {
  fn new(patch: &'a StorageEngine, target: &'a StorageEngine) -> EngineResult<Self> {
    if patch.hash_algo() != target.hash_algo() {
      return Err(EngineError::InvalidInput(format!(
        "patch uses {:?} hashing but target uses {:?}",
        patch.hash_algo(),
        target.hash_algo()
      )));
    }
    Ok(Self { patch, target })
  }

  fn patch_contains(&self, hash: &[u8]) -> EngineResult<bool> {
    Ok(self.patch.get_entry_header_including_deleted(hash)?.is_some())
  }
}

impl HistoricalEntrySource for PatchOverlaySource<'_> {
  fn hash_algo(&self) -> crate::engine::hash_algorithm::HashAlgorithm {
    self.target.hash_algo()
  }

  fn historical_entry_header(&self, hash: &[u8]) -> EngineResult<Option<crate::engine::entry_header::EntryHeader>> {
    match self.patch.get_entry_header_including_deleted(hash)? {
      Some(header) => Ok(Some(header)),
      None => self.target.get_entry_header_including_deleted(hash),
    }
  }

  fn historical_entry_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<HistoricalEntry>> {
    if self.patch_contains(hash)? {
      self.patch.get_entry_including_deleted_verified_bounded(hash, maximum_value_length)
    } else {
      self.target.get_entry_including_deleted_verified_bounded(hash, maximum_value_length)
    }
  }
}

struct SelectedPatchDeletion {
  path_key: Vec<u8>,
  record: DeletionRecord,
  flags: u8,
  entry_version: u8,
  entry_type: EntryType,
  previous_identity: Option<Vec<u8>>,
  retire_locator: bool,
  retained_charge: u64,
}

pub fn import_backup(
  ctx: &RequestContext,
  target: &StorageEngine,
  backup_path: &str,
  force: bool,
  promote: bool,
  include_system: bool,
) -> EngineResult<ImportResult> {
  import_backup_with_mode(ctx, target, backup_path, force, promote, include_system, ImportMode::Merge)
}

pub fn import_backup_with_mode(
  ctx: &RequestContext,
  target: &StorageEngine,
  backup_path: &str,
  force: bool,
  promote: bool,
  include_system: bool,
  mode: ImportMode,
) -> EngineResult<ImportResult> {
  let mut budget = backup_budget(target, None)?;
  let backup = StorageEngine::open_for_import_with_memory_coordinator(backup_path, target.memory_coordinator())?;
  let (backup_type, base_hash, target_hash) = backup.backup_info()?;
  let starting_root_hash = target.head_hash()?;
  if !matches!(backup_type, 1 | 2) {
    return Err(EngineError::InvalidInput(format!("import requires backup type 1 (export) or 2 (patch), found {}", backup_type)));
  }

  // Restore-mode safety: refuse to clobber live data unless explicitly forced.
  if mode == ImportMode::Restore && !force && !is_target_empty(target)? {
    return Err(EngineError::InvalidInput(
      "target database is not empty; refusing restore.\n\
                 Use mode=merge to union, or pass force=true to overwrite anyway."
        .to_string(),
    ));
  }

  // Admit every inventory and the largest source-entry buffer before the
  // first target mutation. A memory-pressure or malformed-backup failure must
  // not turn an import into an avoidable partial write.
  let (chunk_kv_entries, chunk_inventory_charge) =
    load_type_inventory(&backup, KV_TYPE_CHUNK, &mut budget, "chunk import inventory admission failed")?;
  let (file_kv_entries, file_inventory_charge) =
    load_type_inventory(&backup, KV_TYPE_FILE_RECORD, &mut budget, "FileRecord import inventory admission failed")?;
  let (dir_kv_entries, directory_inventory_charge) =
    load_type_inventory(&backup, KV_TYPE_DIRECTORY, &mut budget, "DirectoryIndex import inventory admission failed")?;
  let (sym_kv_entries, symlink_inventory_charge) =
    load_type_inventory(&backup, KV_TYPE_SYMLINK, &mut budget, "Symlink import inventory admission failed")?;
  let (snapshot_kv_entries, snapshot_inventory_charge) = if include_system {
    load_type_inventory(&backup, crate::engine::kv_store::KV_TYPE_SNAPSHOT, &mut budget, "Snapshot import inventory admission failed")?
  } else {
    (Vec::new(), 0)
  };
  let (deletion_kv_entries, deletion_inventory_charge) = if backup_type == 2 {
    load_type_inventory(&backup, KV_TYPE_DELETION, &mut budget, "deletion import inventory admission failed")?
  } else {
    (Vec::new(), 0)
  };

  let inventories: &[(&[crate::engine::kv_store::KVEntry], EntryType, &str)] = &[
    (&chunk_kv_entries, EntryType::Chunk, "chunk"),
    (&file_kv_entries, EntryType::FileRecord, "FileRecord"),
    (&dir_kv_entries, EntryType::DirectoryIndex, "DirectoryIndex"),
    (&sym_kv_entries, EntryType::Symlink, "Symlink"),
    (&snapshot_kv_entries, EntryType::Snapshot, "Snapshot"),
    (&deletion_kv_entries, EntryType::DeletionRecord, "DeletionRecord"),
  ];
  let (maximum_entry_charge, maximum_value_length) = preflight_import_inventories(&backup, inventories, &mut budget)?;
  budget.reserve(maximum_entry_charge, "largest import entry buffer admission failed")?;
  validate_import_inventories(&backup, inventories, maximum_value_length, &mut budget)?;
  let operation = if include_system { SystemFamilyTransferOperationV1::Import } else { SystemFamilyTransferOperationV1::DataExport };
  let selected_deletions = validate_import_leaf_policies(
    &backup,
    &file_kv_entries,
    &sym_kv_entries,
    &deletion_kv_entries,
    operation,
    maximum_value_length,
    &mut budget,
  )?;

  drop(chunk_kv_entries);
  drop(file_kv_entries);
  drop(dir_kv_entries);
  drop(sym_kv_entries);
  drop(snapshot_kv_entries);
  drop(deletion_kv_entries);
  budget.release(chunk_inventory_charge, "chunk import inventory release failed")?;
  budget.release(file_inventory_charge, "FileRecord import inventory release failed")?;
  budget.release(directory_inventory_charge, "DirectoryIndex import inventory release failed")?;
  budget.release(symlink_inventory_charge, "Symlink import inventory release failed")?;
  budget.release(snapshot_inventory_charge, "Snapshot import inventory release failed")?;
  budget.release(deletion_inventory_charge, "deletion import inventory release failed")?;

  if backup_type == 1 {
    import_full_export_with_policy(ctx, target, &backup, &target_hash, include_system, force, promote, &starting_root_hash, &mut budget)
  } else {
    import_sparse_patch_with_policy(
      ctx,
      target,
      &backup,
      &base_hash,
      &target_hash,
      operation,
      selected_deletions,
      force,
      promote,
      &starting_root_hash,
      &mut budget,
    )
  }
}

#[allow(clippy::too_many_arguments)]
fn import_sparse_patch_with_policy(
  ctx: &RequestContext,
  target: &StorageEngine,
  patch: &StorageEngine,
  base_hash: &[u8],
  target_hash: &[u8],
  operation: SystemFamilyTransferOperationV1,
  mut selected_deletions: Vec<SelectedPatchDeletion>,
  force: bool,
  promote: bool,
  starting_root_hash: &[u8],
  budget: &mut OperationMemoryBudget,
) -> EngineResult<ImportResult> {
  let overlay = PatchOverlaySource::new(patch, target)?;
  let validation_checkpoint = budget.checkpoint();
  let resolver = SystemFamilyPolicyResolver::new(target.hash_algo())?;
  let mut target_tree = walk_version_tree_for_transfer_from_source_with_budget(&overlay, target_hash, operation, false, budget)?;
  prune_empty_structural_directories(&mut target_tree, resolver, operation)?;

  for deletion in &selected_deletions {
    if target_tree.files.contains_key(&deletion.record.path) || target_tree.symlinks.contains_key(&deletion.record.path) {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("patch both retains and deletes path '{}'", deletion.record.path),
      });
    }
  }

  let visible_change = if force {
    true
  } else {
    let current_head = target.head_hash()?;
    let mut current_tree = match walk_version_tree_for_transfer_with_budget(target, &current_head, operation, false, budget) {
      Ok(tree) => tree,
      Err(EngineError::NotFound(_)) if current_head != base_hash => return Err(patch_base_mismatch(&current_head, base_hash)),
      Err(error) => return Err(error),
    };
    prune_empty_structural_directories(&mut current_tree, resolver, operation)?;

    // Exact raw-root equality keeps legacy patches fast and compatible. New
    // patches also carry their selected base directory closure, allowing a
    // target rebuilt by logical import to prove equivalent namespace content
    // without requiring an unsafe --force override.
    if current_head != base_hash {
      let mut expected_base = match walk_version_tree_for_transfer_from_source_with_budget(&overlay, base_hash, operation, false, budget) {
        Ok(tree) => tree,
        Err(EngineError::NotFound(_)) => return Err(patch_base_mismatch(&current_head, base_hash)),
        Err(error) => return Err(error),
      };
      prune_empty_structural_directories(&mut expected_base, resolver, operation)?;
      if !version_trees_semantically_equal(&current_tree, &expected_base) {
        return Err(patch_base_mismatch(&current_head, base_hash));
      }
      drop(expected_base);
    }

    let changed = !version_trees_semantically_equal(&current_tree, &target_tree);
    drop(current_tree);
    changed
  };
  let current_operations = DirectoryOps::new(target);
  for deletion in &mut selected_deletions {
    budget.record_work(1)?;
    let selected_identity = match current_operations.resolve_current_entry_identity_from(target, &deletion.record.path)? {
      Some((entry_type, identity)) if entry_type == deletion.entry_type => Some(identity),
      Some((entry_type, _identity)) if !force => {
        return Err(EngineError::AlreadyExists(format!(
          "Patch deletes {:?} path '{}' but the target currently selects {:?}",
          deletion.entry_type, deletion.record.path, entry_type
        )));
      }
      Some(_) | None => None,
    };
    let locator_identity = current_operations.resolve_live_locator_identity_from(target, &deletion.record.path, deletion.entry_type)?;
    deletion.retire_locator = locator_identity.is_some();
    deletion.previous_identity = selected_identity.or(locator_identity);
  }
  let effective_deletion = selected_deletions.iter().any(|deletion| deletion.retire_locator);
  drop(target_tree);
  budget.release_to(validation_checkpoint, "sparse patch validation tree release failed")?;

  if !visible_change && !effective_deletion {
    release_selected_patch_deletions(selected_deletions, budget)?;
    return Ok(ImportResult {
      backup_type: 2,
      entries_imported: 0,
      chunks_imported: 0,
      files_imported: 0,
      directories_imported: 0,
      deletions_applied: 0,
      version_hash: target.head_hash()?,
      head_promoted: false,
    });
  }

  let write_checkpoint = budget.checkpoint();
  let tree = walk_version_tree_for_transfer_from_source_with_budget(&overlay, target_hash, operation, false, budget)?;
  let stats = write_tree_to_engine(&tree, &overlay, target, resolver, operation, TransferDestinationMode::SparseImport, budget)?;
  drop(tree);
  budget.release_to(write_checkpoint, "sparse patch write tree release failed")?;

  let mut deletions_applied = 0u64;
  let mut deletion_batch = ImportLocatorBatch::new(target)?;
  for deletion in &selected_deletions {
    budget.record_work(1)?;
    if deletion.retire_locator {
      let previous_identity = deletion.previous_identity.clone().ok_or_else(|| EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Patch deletion '{}' lost its selected prior identity", deletion.record.path),
      })?;
      let value = deletion.record.serialize();
      let deletion_key = deletion_record_hash(&deletion.record.path, deletion.record.deleted_at, &target.hash_algo())?;
      deletion_batch.retire_with_dependency(
        EntryType::DeletionRecord,
        deletion_key,
        value,
        deletion.flags,
        deletion.entry_version,
        deletion.path_key.clone(),
        deletion.record.path.clone(),
        previous_identity,
      )?;
      deletions_applied = deletions_applied.saturating_add(1);
    }
  }
  deletion_batch.flush()?;
  release_selected_patch_deletions(selected_deletions, budget)?;

  let entries_imported = stats
    .chunks_written
    .saturating_add(stats.files_mutated)
    .saturating_add(stats.directories_mutated)
    .saturating_add(stats.symlinks_mutated)
    .saturating_add(deletions_applied);
  let expected_root_hash = if force { None } else { Some(starting_root_hash) };
  let head_promoted = finish_import_head(ctx, target, &stats.root_hash, promote, "patch", entries_imported, expected_root_hash)?;

  Ok(ImportResult {
    backup_type: 2,
    entries_imported,
    chunks_imported: stats.chunks_written,
    files_imported: stats.files_mutated,
    directories_imported: stats.directories_mutated,
    deletions_applied,
    version_hash: stats.root_hash,
    head_promoted,
  })
}

fn patch_base_mismatch(current_head: &[u8], base_hash: &[u8]) -> EngineError {
  EngineError::NotFound(format!(
    "Target database HEAD ({}) does not match patch base version ({}).\n\
             Use --force to apply anyway.",
    hex::encode(current_head),
    hex::encode(base_hash),
  ))
}

fn version_trees_semantically_equal(left: &VersionTree, right: &VersionTree) -> bool {
  left.directories.len() == right.directories.len()
    && left.directories.keys().all(|path| right.directories.contains_key(path))
    && left.files.len() == right.files.len()
    && left.files.iter().all(|(path, (hash, _))| right.files.get(path).is_some_and(|(other_hash, _)| other_hash == hash))
    && left.symlinks.len() == right.symlinks.len()
    && left.symlinks.iter().all(|(path, (hash, _))| right.symlinks.get(path).is_some_and(|(other_hash, _)| other_hash == hash))
}

fn prune_empty_structural_directories(
  tree: &mut VersionTree,
  resolver: SystemFamilyPolicyResolver,
  operation: SystemFamilyTransferOperationV1,
) -> EngineResult<()> {
  let mut paths = tree.directories.keys().filter(|path| path.as_str() != "/").cloned().collect::<Vec<_>>();
  paths.sort_by_key(|path| std::cmp::Reverse(path_depth(path)));
  for path in paths {
    if resolver.transfer_path_selection(&path, operation)? != TransferPathSelection::StructuralContainer {
      continue;
    }
    let descendant_prefix = format!("{path}/");
    let has_retained_descendant = tree.files.keys().any(|candidate| candidate.starts_with(&descendant_prefix))
      || tree.symlinks.keys().any(|candidate| candidate.starts_with(&descendant_prefix))
      || tree.directories.keys().any(|candidate| candidate != &path && candidate.starts_with(&descendant_prefix));
    if !has_retained_descendant {
      tree.directories.remove(&path);
    }
  }
  Ok(())
}

fn release_selected_patch_deletions(deletions: Vec<SelectedPatchDeletion>, budget: &mut OperationMemoryBudget) -> EngineResult<()> {
  for deletion in deletions {
    budget.release(deletion.retained_charge, "selected patch deletion release failed")?;
  }
  Ok(())
}

fn import_full_export_with_policy(
  ctx: &RequestContext,
  target: &StorageEngine,
  backup: &StorageEngine,
  target_hash: &[u8],
  include_system: bool,
  force: bool,
  promote: bool,
  starting_root_hash: &[u8],
  budget: &mut OperationMemoryBudget,
) -> EngineResult<ImportResult> {
  let operation = if include_system { SystemFamilyTransferOperationV1::Import } else { SystemFamilyTransferOperationV1::DataExport };
  let resolver = SystemFamilyPolicyResolver::new(backup.hash_algo())?;
  let snapshots_checkpoint = budget.checkpoint();
  let snapshots = if include_system { load_snapshot_infos(backup, budget)? } else { Vec::new() };

  // Validate every authoritative tree before the first target write. The
  // second walk performs the copy so a database with many snapshots remains
  // bounded by one decoded tree rather than retaining every tree at once.
  validate_full_import_tree(backup, target_hash, operation, include_system, budget)?;
  for snapshot in &snapshots {
    validate_full_import_tree(backup, &snapshot.root_hash, operation, false, budget)?;
  }

  let head_checkpoint = budget.checkpoint();
  let head_tree = walk_version_tree_for_transfer_with_budget(backup, target_hash, operation, include_system, budget)?;
  let head_stats = write_tree_to_engine(&head_tree, backup, target, resolver, operation, TransferDestinationMode::FullImport, budget)?;
  drop(head_tree);
  budget.release_to(head_checkpoint, "imported HEAD tree release failed")?;

  let mut chunks_imported = head_stats.chunks_written;
  let mut files_imported = head_stats.files_written;
  let mut directories_imported = head_stats.directories_written;
  let mut entries_imported = head_stats
    .chunks_written
    .saturating_add(head_stats.files_written)
    .saturating_add(head_stats.directories_written)
    .saturating_add(head_stats.symlinks_written);

  for mut snapshot in snapshots {
    let tree_checkpoint = budget.checkpoint();
    let tree = walk_version_tree_for_transfer_with_budget(backup, &snapshot.root_hash, operation, false, budget)?;
    let stats = write_tree_to_engine(&tree, backup, target, resolver, operation, TransferDestinationMode::HistoricalImport, budget)?;
    drop(tree);
    budget.release_to(tree_checkpoint, "imported snapshot tree release failed")?;

    chunks_imported = chunks_imported.saturating_add(stats.chunks_written);
    files_imported = files_imported.saturating_add(stats.files_written);
    directories_imported = directories_imported.saturating_add(stats.directories_written);
    entries_imported = entries_imported
      .saturating_add(stats.chunks_written)
      .saturating_add(stats.files_written)
      .saturating_add(stats.directories_written)
      .saturating_add(stats.symlinks_written);

    snapshot.root_hash = stats.root_hash;
    let snapshot_key = target.compute_hash(format!("snap:{}", snapshot.name).as_bytes())?;
    let value = snapshot.serialize(target.hash_algo().hash_length())?;
    if import_snapshot_locator(target, snapshot_key, &snapshot, value.clone(), budget)? {
      target.counters().record_write(value.len() as u64);
      target.counters().increment_snapshots();
      entries_imported = entries_imported.saturating_add(1);
    }
  }
  budget.release_to(snapshots_checkpoint, "imported snapshot inventory release failed")?;

  let expected_root_hash = if force { None } else { Some(starting_root_hash) };
  let head_promoted = finish_import_head(ctx, target, &head_stats.root_hash, promote, "export", entries_imported, expected_root_hash)?;

  Ok(ImportResult {
    backup_type: 1,
    entries_imported,
    chunks_imported,
    files_imported,
    directories_imported,
    deletions_applied: 0,
    version_hash: head_stats.root_hash,
    head_promoted,
  })
}

fn import_snapshot_locator(
  target: &StorageEngine,
  snapshot_key: Vec<u8>,
  snapshot: &SnapshotInfo,
  value: Vec<u8>,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<bool> {
  let source_path = format!("/.aeordb-system/version-locators/snapshots/{}", hex::encode(&snapshot_key));
  let root_hash = snapshot.root_hash.clone();
  let (acknowledgement, ()) = NamespaceMutationCoordinator::new(target).prepare_and_maybe_execute(|planning_engine| {
    if let Some(header) = planning_engine.get_entry_header(&snapshot_key)? {
      if header.entry_type != EntryType::Snapshot {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("imported snapshot locator {} resolves to {:?}", hex::encode(&snapshot_key), header.entry_type),
        });
      }
      let charge = backup_entry_charge(&header)?;
      budget.reserve(charge, "existing imported snapshot validation admission failed")?;
      let validation = planning_engine.get_entry_verified_bounded(&snapshot_key, header.value_length).and_then(|entry| {
        let (stored_header, stored_key, stored_value) = entry.ok_or_else(|| EngineError::CorruptEntry {
          offset: 0,
          reason: format!("imported snapshot locator {} disappeared during validation", hex::encode(&snapshot_key)),
        })?;
        if stored_key != snapshot_key || stored_header.entry_type != EntryType::Snapshot {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("imported snapshot locator {} does not resolve to an exact Snapshot entry", hex::encode(&snapshot_key)),
          });
        }
        let stored = SnapshotInfo::deserialize(&stored_value, planning_engine.hash_algo().hash_length(), stored_header.entry_version)?;
        if stored.name != snapshot.name {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("imported snapshot locator key names '{}' but its record names '{}'", snapshot.name, stored.name),
          });
        }
        Ok(())
      });
      let release_result = budget.release(charge, "existing imported snapshot validation release failed");
      match validation {
        Ok(()) => release_result?,
        Err(error) => {
          return Err(preserve_import_primary_error(
            error,
            release_result,
            "Imported snapshot validation failed and reservation cleanup also failed",
          ));
        }
      }
      return Ok((None, ()));
    }

    let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Import);
    batch.replace_locator_with_version(EntryType::Snapshot, snapshot_key.clone(), value.clone(), 0, 0)?;
    batch.add_source_identity(NamespaceMutationSourceIdentity {
      path: source_path.clone(),
      entry_type: Some(EntryType::Snapshot.to_u8()),
      previous_identity: None,
      new_identity: Some(root_hash.clone()),
    })?;
    Ok((Some(batch), ()))
  })?;
  Ok(acknowledgement.is_some())
}

fn validate_full_import_tree(
  backup: &StorageEngine,
  root_hash: &[u8],
  operation: SystemFamilyTransferOperationV1,
  include_detached_current_state: bool,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<()> {
  let checkpoint = budget.checkpoint();
  let tree = walk_version_tree_for_transfer_with_budget(backup, root_hash, operation, include_detached_current_state, budget)?;
  drop(tree);
  budget.release_to(checkpoint, "validated import tree release failed")
}

fn backup_entry_charge(header: &crate::engine::entry_header::EntryHeader) -> EngineResult<u64> {
  u64::from(header.key_length)
    .checked_add(u64::from(header.value_length))
    .and_then(|bytes| bytes.checked_add(header.header_size() as u64))
    .and_then(|bytes| bytes.checked_add(96))
    .ok_or_else(|| EngineError::ResourceExhausted("backup entry buffer estimate overflow".to_string()))
}

fn required_backup_header(
  backup: &StorageEngine,
  hash: &[u8],
  expected_type: EntryType,
  context: &str,
) -> EngineResult<crate::engine::entry_header::EntryHeader> {
  let header = backup.get_entry_header_including_deleted(hash)?.ok_or_else(|| EngineError::CorruptEntry {
    offset: 0,
    reason: format!("{context} references missing {:?} entry {}", expected_type, hex::encode(hash)),
  })?;
  if header.entry_type != expected_type {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("{context} expected {:?}, found {:?} for {}", expected_type, header.entry_type, hex::encode(hash)),
    });
  }
  Ok(header)
}

fn preflight_import_inventories(
  backup: &StorageEngine,
  inventories: &[(&[crate::engine::kv_store::KVEntry], EntryType, &str)],
  budget: &mut OperationMemoryBudget,
) -> EngineResult<(u64, u32)> {
  let mut maximum_charge = 0u64;
  let mut maximum_value_length = 0u32;
  for (entries, expected_type, context) in inventories {
    for entry in *entries {
      budget.record_work(1)?;
      let header = required_backup_header(backup, &entry.hash, *expected_type, context)?;
      maximum_charge = maximum_charge.max(backup_entry_charge(&header)?);
      maximum_value_length = maximum_value_length.max(header.value_length);
    }
  }
  Ok((maximum_charge, maximum_value_length))
}

fn validate_import_inventories(
  backup: &StorageEngine,
  inventories: &[(&[crate::engine::kv_store::KVEntry], EntryType, &str)],
  maximum_value_length: u32,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<()> {
  for (entries, expected_type, context) in inventories {
    for entry in *entries {
      budget.record_work(1)?;
      required_import_entry(backup, &entry.hash, *expected_type, maximum_value_length, context)?;
    }
  }
  Ok(())
}

fn required_import_entry(
  backup: &StorageEngine,
  hash: &[u8],
  expected_type: EntryType,
  maximum_value_length: u32,
  context: &str,
) -> EngineResult<BackupEntry> {
  let entry = backup.get_entry_including_deleted_verified_bounded(hash, maximum_value_length)?.ok_or_else(|| {
    EngineError::CorruptEntry { offset: 0, reason: format!("import references missing {context} entry {}", hex::encode(hash)) }
  })?;
  if entry.0.entry_type != expected_type {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("import expected {:?}, found {:?} for {}", expected_type, entry.0.entry_type, hex::encode(hash)),
    });
  }
  Ok(entry)
}

/// Result of an import operation.
#[derive(Debug, Clone)]
pub struct ImportResult {
  pub backup_type: u8,
  pub entries_imported: u64,
  pub chunks_imported: u64,
  pub files_imported: u64,
  pub directories_imported: u64,
  pub deletions_applied: u64,
  pub version_hash: Vec<u8>,
  pub head_promoted: bool,
}

impl std::fmt::Display for ImportResult {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let type_name = match self.backup_type {
      1 => "Full export",
      2 => "Patch",
      _ => "Unknown",
    };
    write!(
      f,
      "{} imported.\n  Entries: {}\n  Chunks: {}\n  Files: {}\n  Directories: {}\n  Deletions: {}\n  Version: {}\n\n  HEAD {}",
      type_name,
      self.entries_imported,
      self.chunks_imported,
      self.files_imported,
      self.directories_imported,
      self.deletions_applied,
      hex::encode(&self.version_hash),
      if self.head_promoted {
        "has been promoted.".to_string()
      } else {
        format!("has NOT been changed.\n  To promote: aeordb promote --hash {}", hex::encode(&self.version_hash))
      },
    )
  }
}

#[cfg(test)]
mod atomic_cleanup_tests {
  use std::sync::{Arc, Mutex};
  use std::panic::{catch_unwind, AssertUnwindSafe};

  use super::*;

  #[test]
  fn export_atomic_removes_partial_artifact_during_unwind() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("panic.aeordb");
    let output_string = output.to_string_lossy().into_owned();
    let observed_part = Arc::new(Mutex::new(None::<String>));
    let observed_part_for_work = observed_part.clone();

    let result = catch_unwind(AssertUnwindSafe(|| {
      let _ = export_atomic(&output_string, |part_path| -> EngineResult<ExportResult> {
        *observed_part_for_work.lock().unwrap() = Some(part_path.to_string());
        std::fs::write(part_path, b"partial")?;
        std::fs::write(format!("{part_path}.lock"), b"")?;
        panic!("injected backup writer panic");
      });
    }));

    assert!(result.is_err());
    assert!(!output.exists());
    let part = observed_part.lock().unwrap().clone().expect("work closure should observe partial path");
    assert!(!std::path::Path::new(&part).exists());
    assert!(!std::path::Path::new(&format!("{part}.lock")).exists());
  }
}
