use crate::engine::deletion_record::DeletionRecord;
use crate::engine::file_record::FileRecord;
use crate::engine::directory_ops::{file_path_hash, directory_path_hash, file_content_hash, is_system_path};

/// Credential paths are always excluded from backups. Importing credentials
/// would tie the target's auth state to the source's identity — confusing
/// at best, security risk at worst. The target uses its own bootstrap key.
fn is_credential_path(path: &str) -> bool {
  path.starts_with("/.aeordb-system/api-keys")
    || path.starts_with("/.aeordb-system/refresh-tokens")
    || path.starts_with("/.aeordb-system/magic-links")
}
use crate::engine::engine_event::{ImportEventData, EVENT_IMPORTS_COMPLETED};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::{KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_DELETION, KV_TYPE_SYMLINK};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::symlink_path_hash;
use crate::engine::tree_walker::{diff_trees_with_budget, walk_subtree_with_budget, walk_version_tree_with_budget, VersionTree};
use crate::engine::entry_type::EntryType;
use crate::engine::version_manager::SnapshotInfo;
use tokio_util::sync::CancellationToken;
use std::path::Path;

const BACKUP_MINIMUM_WORKSPACE_BYTES: u64 = 4 * 1024;
const KV_INVENTORY_ENTRY_BYTES: u64 = std::mem::size_of::<crate::engine::kv_store::KVEntry>() as u64 + 96;

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
/// backup_type = 1 (full export), base_hash = target_hash = version_hash.
///
/// If `include_system` is true, all `/.aeordb-system/` entries (users,
/// groups, API keys, etc.) are included. Otherwise they are filtered out.
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
    let tree = walk_version_tree_with_budget(source, version_hash, budget)?;
    budget.check_cancellation()?;
    let output = StorageEngine::create_with_memory_coordinator(part_path, source.memory_coordinator())?;
    output.set_backup_info(1, version_hash, version_hash)?;
    let stats = write_tree_to_engine(&tree, source, &output, include_system, budget)?;
    budget.check_cancellation()?;
    output.update_head(version_hash)?;

    Ok(ExportResult {
      chunks_written: stats.0,
      files_written: stats.1,
      directories_written: stats.2,
      version_hash: version_hash.to_vec(),
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
  export_snapshot_controlled(source, snapshot_name, output_path, include_system, None)
}

pub fn export_snapshot_with_cancellation(
  source: &StorageEngine,
  snapshot_name: Option<&str>,
  output_path: &str,
  include_system: bool,
  cancellation: &CancellationToken,
) -> EngineResult<ExportResult> {
  export_snapshot_controlled(source, snapshot_name, output_path, include_system, Some(cancellation))
}

fn export_snapshot_controlled(
  source: &StorageEngine,
  snapshot_name: Option<&str>,
  output_path: &str,
  include_system: bool,
  cancellation: Option<&CancellationToken>,
) -> EngineResult<ExportResult> {
  let mut budget = backup_budget(source, cancellation)?;
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
/// `include_system` controls whether `/.aeordb-system/` entries (users, groups,
/// API keys) are included. Callers should validate root key authority before
/// passing `include_system = true`.
pub fn export_full(source: &StorageEngine, output_path: &str, include_system: bool) -> EngineResult<ExportResult> {
  let mut budget = backup_budget(source, None)?;
  export_atomic(output_path, |part_path| {
    budget.check_cancellation()?;
    let output = StorageEngine::create_with_memory_coordinator(part_path, source.memory_coordinator())?;

    let head_hash = source.head_hash()?;
    output.set_backup_info(1, &head_hash, &head_hash)?;

    let mut total_chunks = 0u64;
    let mut total_files = 0u64;
    let mut total_dirs = 0u64;

    let mut walked: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

    let head_checkpoint = budget.checkpoint();
    let stats = {
      let head_tree = walk_version_tree_with_budget(source, &head_hash, &mut budget)?;
      write_tree_to_engine(&head_tree, source, &output, include_system, &mut budget)?
    };
    budget.release_to(head_checkpoint, "HEAD tree release failed")?;
    total_chunks += stats.0;
    total_files += stats.1;
    total_dirs += stats.2;
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
        let tree = walk_version_tree_with_budget(source, &snap.root_hash, &mut budget)?;
        write_tree_to_engine(&tree, source, &output, include_system, &mut budget)?
      };
      budget.release_to(tree_checkpoint, "snapshot tree release failed")?;
      total_chunks += stats.0;
      total_files += stats.1;
      total_dirs += stats.2;
      walked.insert(snap.root_hash.clone());
    }

    if include_system {
      let system_checkpoint = budget.checkpoint();
      let system_stats = export_system_subtree(source, &output, &mut budget)?;
      total_files += system_stats.0;
      total_dirs += system_stats.1;
      budget.release_to(system_checkpoint, "system tree release failed")?;
      copy_snapshot_entries(source, &output, &mut budget)?;
    }

    output.update_head(&head_hash)?;

    Ok(ExportResult {
      chunks_written: total_chunks,
      files_written: total_files,
      directories_written: total_dirs,
      version_hash: head_hash,
      snapshots_written: if include_system { snapshot_count } else { 0 },
    })
  })
}

/// Write all entries from a VersionTree into an output engine.
/// Returns (chunks_written, files_written, directories_written).
///
/// SECURITY: All entries under /.aeordb-system/ are filtered out. Exports must
/// contain only user data, never system internals (JWT keys, API key
/// hashes, refresh tokens, user records).
fn write_tree_to_engine(
  tree: &VersionTree,
  source: &StorageEngine,
  output: &StorageEngine,
  include_system: bool,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<(u64, u64, u64)> {
  let mut chunks_written = 0u64;
  let mut files_written = 0u64;
  let mut dirs_written = 0u64;

  // Walk file-owned hashes directly. The destination KV de-duplicates shared
  // chunks, avoiding a second unbounded in-memory set alongside VersionTree.
  for (path, (_file_hash, record)) in &tree.files {
    budget.record_work(1)?;
    if is_credential_path(path) {
      continue;
    }
    if include_system || !is_system_path(path) {
      for chunk_hash in &record.chunk_hashes {
        budget.record_work(1)?;
        if output.has_entry(chunk_hash)? {
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
        budget.release(charge, "chunk copy buffer release failed")?;
        chunks_written += 1;
      }
    }
  }

  // Write FileRecords at both content-hash and path-hash keys.
  // The tree walker stores content hashes as file_hash, but read_file
  // looks up by path hash, so both must be present in the exported database.
  let file_algo = output.hash_algo();
  for (path, (file_hash, _record)) in &tree.files {
    budget.record_work(1)?;
    if is_credential_path(path) {
      continue;
    }
    if !include_system && is_system_path(path) {
      continue;
    }
    {
      let ((header, key, value), charge) = required_backup_entry(source, file_hash, "FileRecord", budget)?;
      if header.entry_type != EntryType::FileRecord {
        return Err(EngineError::CorruptEntry { offset: 0, reason: format!("backup file '{}' resolved to {:?}", path, header.entry_type) });
      }
      if !output.has_entry(&key)? {
        if is_system_path(path) {
          // System entries need the FLAG_SYSTEM bit set
          store_file_record_entry_preserving_version(
            &output,
            &key,
            &value,
            crate::engine::entry_header::FLAG_SYSTEM,
            header.entry_version,
          )?;
        } else {
          store_file_record_entry_preserving_version(&output, &key, &value, 0, header.entry_version)?;
        }
      }
      // Also write at path-hash key (for read_file lookups)
      let path_key = file_path_hash(path, &file_algo)?;
      if path_key != key && !output.has_entry(&path_key)? {
        if is_system_path(path) {
          store_file_record_entry_preserving_version(
            &output,
            &path_key,
            &value,
            crate::engine::entry_header::FLAG_SYSTEM,
            header.entry_version,
          )?;
        } else {
          store_file_record_entry_preserving_version(&output, &path_key, &value, 0, header.entry_version)?;
        }
      }
      files_written += 1;
      budget.release(charge, "FileRecord copy buffer release failed")?;
    }
  }

  // Write DirectoryIndexes at both content-hash and path-hash keys.
  let algo = output.hash_algo();
  write_btree_nodes(tree, source, output, budget, 0)?;
  for (path, (dir_hash, _data)) in &tree.directories {
    budget.record_work(1)?;
    if is_credential_path(path) {
      continue;
    }
    if !include_system && is_system_path(path) {
      continue;
    }
    {
      let ((header, key, value), charge) = required_backup_entry(source, dir_hash, "DirectoryIndex", budget)?;
      if header.entry_type != EntryType::DirectoryIndex {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("backup directory '{}' resolved to {:?}", path, header.entry_type),
        });
      }
      if !output.has_entry(&key)? {
        output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
      }
      let path_key = directory_path_hash(path, &algo)?;
      if path_key != key && !output.has_entry(&path_key)? {
        output.store_entry(EntryType::DirectoryIndex, &path_key, &value)?;
      }
      dirs_written += 1;
      budget.release(charge, "DirectoryIndex copy buffer release failed")?;
    }
  }

  // Write symlink entries at both content-hash and path-hash keys.
  let symlink_algo = output.hash_algo();
  for (path, (symlink_hash, _record)) in &tree.symlinks {
    budget.record_work(1)?;
    if is_credential_path(path) {
      continue;
    }
    if !include_system && is_system_path(path) {
      continue;
    }
    {
      let ((header, key, value), charge) = required_backup_entry(source, symlink_hash, "Symlink", budget)?;
      if header.entry_type != EntryType::Symlink {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("backup symlink '{}' resolved to {:?}", path, header.entry_type),
        });
      }
      if !output.has_entry(&key)? {
        output.store_entry(EntryType::Symlink, &key, &value)?;
      }
      let path_key = symlink_path_hash(path, &symlink_algo)?;
      if path_key != key && !output.has_entry(&path_key)? {
        output.store_entry(EntryType::Symlink, &path_key, &value)?;
      }
      budget.release(charge, "Symlink copy buffer release failed")?;
    }
  }

  Ok((chunks_written, files_written, dirs_written))
}

/// Walk the /.aeordb-system/ subtree(s) and copy all entries.
///
/// System paths are not propagated to root, so the regular HEAD walker
/// can't reach them. Furthermore, individual system subdirectories
/// (/.aeordb-system/users/, /groups/, etc.) may be orphaned from the
/// /.aeordb-system/ children list itself — they exist as path-hash
/// entries but aren't linked. We walk each known system subdirectory
/// individually to ensure we capture everything.
///
/// Returns (file_records_copied, directories_copied).
fn export_system_subtree(source: &StorageEngine, output: &StorageEngine, budget: &mut OperationMemoryBudget) -> EngineResult<(u64, u64)> {
  let algo = source.hash_algo();
  let hash_length = algo.hash_length();

  // List of system subdirectories to include in the backup.
  // CREDENTIALS (api-keys, refresh-tokens, magic-links) are always excluded —
  // they're tied to the database identity that issued them. After a restore,
  // the target's own bootstrap key is the new identity, and api keys are
  // regenerated per user. Importing credentials would create confusion
  // (which key is valid? which database authorized this token?) and
  // doesn't match the encryption model where the root key is the
  // master key for the database that owns it.
  //
  // We also exclude /.aeordb-system itself — the target's own writes
  // will reconstruct that directory listing as users/groups/etc. land.
  // Otherwise we'd inherit a listing that references credential subdirs
  // we deliberately skipped.
  let system_paths = [
    "/.aeordb-system/users",
    "/.aeordb-system/groups",
    "/.aeordb-system/snapshots",
    "/.aeordb-system/config",
    // /.aeordb-config holds cron schedules, webhook configs, parser
    // registry, per-directory indexes.json, and other non-credential
    // operational state. Restore must include these — otherwise a
    // restored cluster has no scheduled jobs, no webhooks, no parsers.
    "/.aeordb-config",
  ];

  // Single files under /.aeordb-system/ (not in a subdirectory). These
  // are individually enumerated because walk_subtree only visits children
  // of a directory; bare files under /.aeordb-system would otherwise be
  // skipped.
  let system_single_files: &[&str] = &["/.aeordb-system/email-config.json"];

  let mut sys_tree = crate::engine::tree_walker::VersionTree::new();

  for sys_path in &system_paths {
    budget.record_work(1)?;
    let sys_dir_key = directory_path_hash(sys_path, &algo)?;
    let (raw_value, path_charge) = match load_backup_entry(source, &sys_dir_key, budget)? {
      Some(((_header, _key, value), charge)) => (value, charge),
      None => continue, // subdirectory doesn't exist in this source
    };

    // Resolve hard link if present
    let sys_dir_hash = if raw_value.len() == hash_length {
      raw_value
    } else {
      // Inline data — compute content hash to walk it
      algo.compute_hash(&raw_value)?
    };
    budget.release(path_charge, "system directory path-entry buffer release failed")?;

    // Also record the path-hash → content-hash mapping in the tree
    budget.reserve((sys_path.len() + sys_dir_hash.len() + 128) as u64, "system directory tree admission failed")?;
    sys_tree.directories.insert(sys_path.to_string(), (sys_dir_hash.clone(), Vec::new()));

    walk_subtree_with_budget(source, sys_path, &sys_dir_hash, &mut sys_tree, budget)?;
  }

  // Single files under /.aeordb-system/. Each is keyed by path-hash;
  // resolve to a FileRecord and add it to the tree. The content hash for
  // the tree key is recomputed from the serialized FileRecord — that's
  // what the chunk-domain entries use elsewhere in the engine.
  for file_path in system_single_files {
    budget.record_work(1)?;
    let key = crate::engine::directory_ops::file_path_hash(file_path, &algo)?;
    let (record, content_hash) = match load_backup_entry(source, &key, budget)? {
      Some(((header, _key, raw), _charge)) => {
        if header.entry_type != EntryType::FileRecord {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("system file '{}' resolved to {:?} instead of FileRecord", file_path, header.entry_type),
          });
        }
        let record = FileRecord::deserialize(&raw, hash_length, header.entry_version).map_err(|error| EngineError::CorruptEntry {
          offset: 0,
          reason: format!("system FileRecord '{}' is malformed: {}", file_path, error),
        })?;
        let h = file_content_hash(&raw, &algo)?;
        (record, h)
      }
      None => continue, // file doesn't exist in source — fine
    };
    budget.reserve((file_path.len() + content_hash.len() + 128) as u64, "system file tree admission failed")?;
    sys_tree.files.insert(file_path.to_string(), (content_hash, record));
  }

  // Write system tree entries with overwrite. Old snapshots may have
  // included /.aeordb-system/ in their root listings (before the change
  // that stops system path propagation to root), so a snapshot walk
  // could have written stale system directory data. We need to overwrite
  // those with the current state.
  write_system_tree(&sys_tree, source, output, budget)
}

/// Like write_tree_to_engine but unconditionally writes system entries
/// (overwrites instead of skipping if existing). Used when copying the
/// authoritative current system state.
fn write_system_tree(
  tree: &crate::engine::tree_walker::VersionTree,
  source: &StorageEngine,
  output: &StorageEngine,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<(u64, u64)> {
  use crate::engine::entry_header::FLAG_SYSTEM;
  let mut files_written = 0u64;
  let mut dirs_written = 0u64;

  let algo = output.hash_algo();

  write_btree_nodes(tree, source, output, budget, FLAG_SYSTEM)?;

  // Chunks for system files (system files rarely have chunks, but include them).
  // Destination lookups de-duplicate without retaining a second hash set.
  for (_file_hash, record) in tree.files.values() {
    for ch in &record.chunk_hashes {
      budget.record_work(1)?;
      if !output.has_entry(ch)? {
        let ((header, key, value), charge) = required_backup_entry(source, ch, "system chunk", budget)?;
        if header.entry_type != EntryType::Chunk {
          return Err(EngineError::CorruptEntry {
            offset: 0,
            reason: format!("system chunk {} resolved to {:?}", hex::encode(ch), header.entry_type),
          });
        }
        output.store_entry(EntryType::Chunk, &key, &value)?;
        budget.release(charge, "system chunk buffer release failed")?;
      }
    }
  }

  // FileRecords (overwrite to ensure latest system state)
  for (path, (file_hash, _record)) in &tree.files {
    budget.record_work(1)?;
    {
      let ((header, key, value), charge) = required_backup_entry(source, file_hash, "system FileRecord", budget)?;
      if header.entry_type != EntryType::FileRecord {
        return Err(EngineError::CorruptEntry { offset: 0, reason: format!("system file '{}' resolved to {:?}", path, header.entry_type) });
      }
      store_file_record_entry_preserving_version(&output, &key, &value, FLAG_SYSTEM, header.entry_version)?;
      let path_key = file_path_hash(path, &algo)?;
      if path_key != key {
        store_file_record_entry_preserving_version(&output, &path_key, &value, FLAG_SYSTEM, header.entry_version)?;
      }
      files_written += 1;
      budget.release(charge, "system FileRecord buffer release failed")?;
    }
  }

  // DirectoryIndex (overwrite — this is the critical fix for the api-keys bug)
  for (path, (dir_hash, _data)) in &tree.directories {
    budget.record_work(1)?;
    {
      let ((header, key, value), charge) = required_backup_entry(source, dir_hash, "system DirectoryIndex", budget)?;
      if header.entry_type != EntryType::DirectoryIndex {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("system directory '{}' resolved to {:?}", path, header.entry_type),
        });
      }
      output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
      let path_key = directory_path_hash(path, &algo)?;
      if path_key != key {
        output.store_entry(EntryType::DirectoryIndex, &path_key, &value)?;
      }
      dirs_written += 1;
      budget.release(charge, "system DirectoryIndex buffer release failed")?;
    }
  }

  // Symlinks
  for (path, (sym_hash, _record)) in &tree.symlinks {
    budget.record_work(1)?;
    {
      let ((header, key, value), charge) = required_backup_entry(source, sym_hash, "system Symlink", budget)?;
      if header.entry_type != EntryType::Symlink {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("system symlink '{}' resolved to {:?}", path, header.entry_type),
        });
      }
      output.store_entry_with_flags(EntryType::Symlink, &key, &value, FLAG_SYSTEM)?;
      let path_key = symlink_path_hash(path, &algo)?;
      if path_key != key {
        output.store_entry_with_flags(EntryType::Symlink, &path_key, &value, FLAG_SYSTEM)?;
      }
      budget.release(charge, "system Symlink buffer release failed")?;
    }
  }

  Ok((files_written, dirs_written))
}

fn write_btree_nodes(
  tree: &VersionTree,
  source: &StorageEngine,
  output: &StorageEngine,
  budget: &mut OperationMemoryBudget,
  flags: u8,
) -> EngineResult<()> {
  for node_hash in tree.btree_nodes.keys() {
    budget.record_work(1)?;
    if output.has_entry(node_hash)? {
      continue;
    }
    let ((header, key, value), charge) = required_backup_entry(source, node_hash, "B-tree DirectoryIndex", budget)?;
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("B-tree node {} resolved to {:?}", hex::encode(node_hash), header.entry_type),
      });
    }
    if flags == 0 {
      output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
    } else {
      output.store_entry_with_flags(EntryType::DirectoryIndex, &key, &value, flags)?;
    }
    budget.release(charge, "B-tree node buffer release failed")?;
  }
  Ok(())
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
    if !output.has_entry(&entry.hash)? {
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

type BackupEntry = (crate::engine::entry_header::EntryHeader, Vec<u8>, Vec<u8>);

fn load_backup_entry(source: &StorageEngine, hash: &[u8], budget: &mut OperationMemoryBudget) -> EngineResult<Option<(BackupEntry, u64)>> {
  let Some(header) = source.get_entry_header_including_deleted(hash)? else {
    return Ok(None);
  };
  let charge = u64::from(header.key_length)
    .checked_add(u64::from(header.value_length))
    .and_then(|bytes| bytes.checked_add(header.header_size() as u64))
    .and_then(|bytes| bytes.checked_add(96))
    .ok_or_else(|| EngineError::ResourceExhausted("backup entry buffer estimate overflow".to_string()))?;
  budget.reserve(charge, "entry buffer admission failed")?;
  match source.get_entry_including_deleted_verified_bounded(hash, header.value_length) {
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

fn required_backup_entry(
  source: &StorageEngine,
  hash: &[u8],
  kind: &str,
  budget: &mut OperationMemoryBudget,
) -> EngineResult<(BackupEntry, u64)> {
  load_backup_entry(source, hash, budget)?
    .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: format!("backup references missing {kind} entry {}", hex::encode(hash)) })
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
/// backup_type = 2 (patch), base_hash = from_hash, target_hash = to_hash.
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
  let base_tree = walk_version_tree_with_budget(source, from_hash, budget)?;
  let target_tree = walk_version_tree_with_budget(source, to_hash, budget)?;
  let diff = diff_trees_with_budget(&base_tree, &target_tree, budget)?;

  if diff.is_empty() {
    return Err(EngineError::NotFound("No changes between the two versions".to_string()));
  }

  budget.check_cancellation()?;
  let output = StorageEngine::create_with_memory_coordinator(output_path, source.memory_coordinator())?;

  // Set backup metadata
  output.set_backup_info(2, from_hash, to_hash)?;

  for node_hash in target_tree.btree_nodes.keys() {
    if base_tree.btree_nodes.contains_key(node_hash) {
      continue;
    }
    budget.record_work(1)?;
    let ((header, key, value), charge) = required_backup_entry(source, node_hash, "patch B-tree DirectoryIndex", budget)?;
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("patch B-tree node {} resolved to {:?}", hex::encode(node_hash), header.entry_type),
      });
    }
    output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
    budget.release(charge, "patch B-tree node buffer release failed")?;
  }

  let mut chunks_written = 0u64;
  let mut files_added = 0u64;
  let mut files_modified = 0u64;
  let mut files_deleted = 0u64;
  let mut dirs_written = 0u64;

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
    store_file_record_entry_preserving_version(&output, &key, &value, 0, header.entry_version)?;
    let path_key = file_path_hash(path, &patch_algo)?;
    if path_key != key {
      store_file_record_entry_preserving_version(&output, &path_key, &value, 0, header.entry_version)?;
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
    store_file_record_entry_preserving_version(&output, &key, &value, 0, header.entry_version)?;
    let path_key = file_path_hash(path, &patch_algo)?;
    if path_key != key {
      store_file_record_entry_preserving_version(&output, &path_key, &value, 0, header.entry_version)?;
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
    output.store_entry(EntryType::Symlink, &key, &value)?;
    let path_key = symlink_path_hash(path, &symlink_algo)?;
    if path_key != key {
      output.store_entry(EntryType::Symlink, &path_key, &value)?;
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
    output.store_entry(EntryType::Symlink, &key, &value)?;
    let path_key = symlink_path_hash(path, &symlink_algo)?;
    if path_key != key {
      output.store_entry(EntryType::Symlink, &path_key, &value)?;
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

  // Write changed DirectoryIndexes at both content-hash and path-hash keys
  let algo = output.hash_algo();
  for (path, (dir_hash, _data)) in &diff.changed_directories {
    budget.record_work(1)?;
    let ((header, key, value), charge) = required_backup_entry(source, dir_hash, "changed patch DirectoryIndex", budget)?;
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("changed patch directory '{}' resolved to {:?}", path, header.entry_type),
      });
    }
    output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
    let path_key = directory_path_hash(path, &algo)?;
    if path_key != key {
      output.store_entry(EntryType::DirectoryIndex, &path_key, &value)?;
    }
    budget.release(charge, "changed patch DirectoryIndex buffer release failed")?;
    dirs_written += 1;
  }

  // Set HEAD to the target hash
  budget.check_cancellation()?;
  output.update_head(to_hash)?;

  Ok(PatchResult {
    chunks_written,
    files_added,
    files_modified,
    files_deleted,
    directories_written: dirs_written,
    from_hash: from_hash.to_vec(),
    to_hash: to_hash.to_vec(),
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

/// Detect whether a backup contains any system data (entries with FLAG_SYSTEM set).
/// Used to determine whether root-key authority is required for import.
pub fn backup_contains_system_data(backup: &StorageEngine) -> EngineResult<bool> {
  use crate::engine::entry_header::FLAG_SYSTEM;
  let mut budget = backup_budget(backup, None)?;
  let (entries, _inventory_charge) =
    load_type_inventory(backup, KV_TYPE_FILE_RECORD, &mut budget, "system-data inventory admission failed")?;
  for entry in entries {
    budget.record_work(1)?;
    let header = required_backup_header(backup, &entry.hash, EntryType::FileRecord, "system-data inspection")?;
    if header.flags & FLAG_SYSTEM != 0 {
      return Ok(true);
    }
  }
  Ok(false)
}

/// Import an export or patch .aeordb file into a target database.
///
/// For full exports (backup_type=1): stores all entries into target.
/// For patches (backup_type=2): verifies base version match, applies changes.
///
/// Does NOT automatically promote HEAD unless `promote` is true.
///
/// `include_system`: when true, system entries (users, groups, keys) from the
/// backup are imported. The CALLER must verify root-key authority before
/// passing true. When false, system entries in the backup are silently skipped.
/// Check whether the target database contains any user data. Considers
/// system paths (under /.aeordb-system, /.aeordb-config) as empty signal,
/// since fresh databases initialize those with bootstrap data automatically.
fn is_target_empty(target: &StorageEngine) -> EngineResult<bool> {
  let ops = crate::engine::DirectoryOps::new(target);
  let children = match ops.list_directory("/") {
    Ok(c) => c,
    Err(EngineError::NotFound(_)) => return Ok(true),
    Err(other) => return Err(other),
  };
  for child in &children {
    if !is_system_path(&format!("/{}", child.name)) {
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

  // For patches, verify base version
  if backup_type == 2 && !force {
    let current_head = target.head_hash()?;
    if current_head != base_hash {
      return Err(EngineError::NotFound(format!(
        "Target database HEAD ({}) does not match patch base version ({}).\n\
                 Use --force to apply anyway.",
        hex::encode(&current_head),
        hex::encode(&base_hash),
      )));
    }
  }

  let mut entries_imported = 0u64;
  let mut chunks_imported = 0u64;
  let mut files_imported = 0u64;
  let mut dirs_imported = 0u64;
  let mut deletions_applied = 0u64;

  use crate::engine::entry_header::FLAG_SYSTEM;

  // Admit every inventory and the largest source-entry buffer before the
  // first target mutation. A memory-pressure or malformed-backup failure must
  // not turn an import into an avoidable partial write.
  let (chunk_kv_entries, _) = load_type_inventory(&backup, KV_TYPE_CHUNK, &mut budget, "chunk import inventory admission failed")?;
  let (file_kv_entries, _) =
    load_type_inventory(&backup, KV_TYPE_FILE_RECORD, &mut budget, "FileRecord import inventory admission failed")?;
  let (dir_kv_entries, _) =
    load_type_inventory(&backup, KV_TYPE_DIRECTORY, &mut budget, "DirectoryIndex import inventory admission failed")?;
  let (sym_kv_entries, _) = load_type_inventory(&backup, KV_TYPE_SYMLINK, &mut budget, "Symlink import inventory admission failed")?;
  let (snapshot_kv_entries, _) = if include_system {
    load_type_inventory(&backup, crate::engine::kv_store::KV_TYPE_SNAPSHOT, &mut budget, "Snapshot import inventory admission failed")?
  } else {
    (Vec::new(), 0)
  };
  let (deletion_kv_entries, _) = if backup_type == 2 {
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

  // Import chunks (chunks themselves don't carry FLAG_SYSTEM — they're
  // shared between user and system files. Filtering happens at the file/dir level.)
  for entry in chunk_kv_entries {
    budget.record_work(1)?;
    if !target.has_entry(&entry.hash)? {
      let (_header, _key, value) = required_import_entry(&backup, &entry.hash, EntryType::Chunk, maximum_value_length, "chunk")?;
      target.store_entry(EntryType::Chunk, &entry.hash, &value)?;
      target.counters().record_chunk_stored(value.len() as u64);
      target.counters().record_write(value.len() as u64);
      chunks_imported += 1;
      entries_imported += 1;
    }
  }

  // Import FileRecords (skip system entries when include_system = false)
  for entry in file_kv_entries {
    budget.record_work(1)?;
    let (header, _key, value) = required_import_entry(&backup, &entry.hash, EntryType::FileRecord, maximum_value_length, "FileRecord")?;
    let is_system = header.flags & FLAG_SYSTEM != 0;
    if is_system && !include_system {
      continue;
    }
    store_file_record_entry_preserving_version(target, &entry.hash, &value, if is_system { FLAG_SYSTEM } else { 0 }, header.entry_version)?;
    target.counters().record_write(value.len() as u64);
    files_imported += 1;
    entries_imported += 1;
  }

  // Import DirectoryIndexes (skip system dirs when include_system = false)
  for entry in dir_kv_entries {
    budget.record_work(1)?;
    let (header, _key, value) =
      required_import_entry(&backup, &entry.hash, EntryType::DirectoryIndex, maximum_value_length, "DirectoryIndex")?;
    let is_system = header.flags & FLAG_SYSTEM != 0;
    if is_system && !include_system {
      continue;
    }
    target.store_entry(EntryType::DirectoryIndex, &entry.hash, &value)?;
    target.counters().record_write(value.len() as u64);
    dirs_imported += 1;
    entries_imported += 1;
  }

  // Import Symlinks (skip system symlinks when include_system = false)
  for entry in sym_kv_entries {
    budget.record_work(1)?;
    let (header, _key, value) = required_import_entry(&backup, &entry.hash, EntryType::Symlink, maximum_value_length, "Symlink")?;
    let is_system = header.flags & FLAG_SYSTEM != 0;
    if is_system && !include_system {
      continue;
    }
    target.store_entry(EntryType::Symlink, &entry.hash, &value)?;
    target.counters().record_write(value.len() as u64);
    entries_imported += 1;
  }

  // Import Snapshot-type entries (only when system data is allowed —
  // snapshots reference system snapshot files and aren't useful without them)
  if include_system {
    for entry in snapshot_kv_entries {
      budget.record_work(1)?;
      if !target.has_entry(&entry.hash)? {
        let (_header, _key, value) = required_import_entry(&backup, &entry.hash, EntryType::Snapshot, maximum_value_length, "Snapshot")?;
        target.store_entry(EntryType::Snapshot, &entry.hash, &value)?;
        target.counters().record_write(value.len() as u64);
        entries_imported += 1;
      }
    }
  }

  // Apply DeletionRecords (for patches)
  if backup_type == 2 {
    for entry in deletion_kv_entries {
      budget.record_work(1)?;
      // Mark the entry as deleted in the target
      if target.has_entry(&entry.hash)? {
        target.mark_entry_deleted(&entry.hash)?;
        target.counters().record_write(0);
        deletions_applied += 1;
        entries_imported += 1;
      }
    }
  }

  // Promote HEAD if requested
  let head_promoted = if promote {
    target.update_head(&target_hash)?;
    target.counters().record_write(0);
    true
  } else {
    false
  };

  target.reconcile_counters_from_kv()?;

  // Emit import completed event
  ctx.emit(
    EVENT_IMPORTS_COMPLETED,
    serde_json::json!({"imports": [ImportEventData {
        backup_type: match backup_type { 1 => "export".to_string(), 2 => "patch".to_string(), _ => "unknown".to_string() },
        version_hash: hex::encode(&target_hash),
        entries_imported,
        head_promoted,
    }]}),
  );

  Ok(ImportResult {
    backup_type,
    entries_imported,
    chunks_imported,
    files_imported,
    directories_imported: dirs_imported,
    deletions_applied,
    version_hash: target_hash.clone(),
    head_promoted,
  })
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
