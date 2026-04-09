use crate::engine::deletion_record::DeletionRecord;
use crate::engine::directory_ops::{file_path_hash, directory_path_hash, file_content_hash};
use crate::engine::engine_event::{ImportEventData, EVENT_IMPORTS_COMPLETED};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::{KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_DELETION};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::tree_walker::{walk_version_tree, diff_trees, VersionTree};
use crate::engine::entry_type::EntryType;
use crate::engine::version_manager::VersionManager;

/// Export a complete version as a clean, self-contained .aeordb file.
///
/// The output database contains only live entries at the given version:
/// no voids, no deletion records, no stale overwrites, no history.
/// backup_type = 1 (full export), base_hash = target_hash = version_hash.
pub fn export_version(
    source: &StorageEngine,
    version_hash: &[u8],
    output_path: &str,
) -> EngineResult<ExportResult> {
    // Walk the source tree to collect all live entries
    let tree = walk_version_tree(source, version_hash)?;

    // Create output database
    let output = StorageEngine::create(output_path)?;

    // Set backup metadata: type=1 (full export), base=target=version_hash
    output.set_backup_info(1, version_hash, version_hash)?;

    let stats = write_tree_to_engine(&tree, source, &output)?;

    // Set HEAD to the version hash
    output.update_head(version_hash)?;

    Ok(ExportResult {
        chunks_written: stats.0,
        files_written: stats.1,
        directories_written: stats.2,
        version_hash: version_hash.to_vec(),
    })
}

/// Export HEAD or a named snapshot.
pub fn export_snapshot(
    source: &StorageEngine,
    snapshot_name: Option<&str>,
    output_path: &str,
) -> EngineResult<ExportResult> {
    let version_hash = match snapshot_name {
        Some(name) => {
            let vm = VersionManager::new(source);
            let snapshots = vm.list_snapshots()?;
            let snap = snapshots.iter()
                .find(|s| s.name == name)
                .ok_or_else(|| EngineError::NotFound(format!("Snapshot '{}' not found", name)))?;
            snap.root_hash.clone()
        }
        None => source.head_hash()?,
    };

    export_version(source, &version_hash, output_path)
}

/// Write all entries from a VersionTree into an output engine.
/// Returns (chunks_written, files_written, directories_written).
fn write_tree_to_engine(
    tree: &VersionTree,
    source: &StorageEngine,
    output: &StorageEngine,
) -> EngineResult<(u64, u64, u64)> {
    let mut chunks_written = 0u64;
    let mut files_written = 0u64;
    let mut dirs_written = 0u64;

    // Write chunks first (referenced by FileRecords)
    for chunk_hash in &tree.chunks {
        if let Some((_header, key, value)) = source.get_entry(chunk_hash)? {
            output.store_entry(EntryType::Chunk, &key, &value)?;
            chunks_written += 1;
        }
    }

    // Write FileRecords at both content-hash and path-hash keys.
    // The tree walker stores content hashes as file_hash, but read_file
    // looks up by path hash, so both must be present in the exported database.
    let file_algo = output.hash_algo();
    for (path, (file_hash, _record)) in &tree.files {
        if let Some((_header, key, value)) = source.get_entry(file_hash)? {
            // Write at content-hash key (for tree walking / snapshots)
            output.store_entry(EntryType::FileRecord, &key, &value)?;
            // Also write at path-hash key (for read_file lookups)
            let path_key = file_path_hash(path, &file_algo)?;
            if path_key != key {
                output.store_entry(EntryType::FileRecord, &path_key, &value)?;
            }
            files_written += 1;
        }
    }

    // Write DirectoryIndexes at both content-hash and path-hash keys.
    // The tree walker stores content hashes as dir_hash, but list_directory
    // looks up by path hash, so both must be present in the exported database.
    let algo = output.hash_algo();
    for (path, (dir_hash, _data)) in &tree.directories {
        if let Some((_header, key, value)) = source.get_entry(dir_hash)? {
            // Write at content-hash key (for tree walking / snapshots)
            output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
            // Also write at path-hash key (for list_directory lookups)
            let path_key = directory_path_hash(path, &algo)?;
            if path_key != key {
                output.store_entry(EntryType::DirectoryIndex, &path_key, &value)?;
            }
            dirs_written += 1;
        }
    }

    Ok((chunks_written, files_written, dirs_written))
}

/// Result of an export operation.
#[derive(Debug, Clone)]
pub struct ExportResult {
    pub chunks_written: u64,
    pub files_written: u64,
    pub directories_written: u64,
    pub version_hash: Vec<u8>,
}

impl std::fmt::Display for ExportResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

/// Create a patch .aeordb containing only the changeset between two versions.
///
/// The output contains: new/changed chunks, updated FileRecords, updated
/// DirectoryIndexes, and DeletionRecords for removed files.
/// backup_type = 2 (patch), base_hash = from_hash, target_hash = to_hash.
///
/// Only chunks that don't exist in the base version are included.
pub fn create_patch(
    source: &StorageEngine,
    from_hash: &[u8],
    to_hash: &[u8],
    output_path: &str,
) -> EngineResult<PatchResult> {
    // Walk both trees
    let base_tree = walk_version_tree(source, from_hash)?;
    let target_tree = walk_version_tree(source, to_hash)?;

    // Compute diff
    let diff = diff_trees(&base_tree, &target_tree);

    if diff.is_empty() {
        return Err(EngineError::NotFound(
            "No changes between the two versions".to_string(),
        ));
    }

    // Create output database
    let output = StorageEngine::create(output_path)?;

    // Set backup metadata
    output.set_backup_info(2, from_hash, to_hash)?;

    let mut chunks_written = 0u64;
    let mut files_added = 0u64;
    let mut files_modified = 0u64;
    let mut files_deleted = 0u64;
    let mut dirs_written = 0u64;

    // Write only NEW chunks (chunks in target but not in base)
    for chunk_hash in &diff.new_chunks {
        if let Some((_header, key, value)) = source.get_entry(chunk_hash)? {
            output.store_entry(EntryType::Chunk, &key, &value)?;
            chunks_written += 1;
        }
    }

    // Write added FileRecords at both content-hash and path-hash keys
    let patch_algo = output.hash_algo();
    for (path, (file_hash, _record)) in &diff.added {
        if let Some((_header, key, value)) = source.get_entry(file_hash)? {
            output.store_entry(EntryType::FileRecord, &key, &value)?;
            let path_key = file_path_hash(path, &patch_algo)?;
            if path_key != key {
                output.store_entry(EntryType::FileRecord, &path_key, &value)?;
            }
            files_added += 1;
        }
    }

    // Write modified FileRecords at both content-hash and path-hash keys
    for (path, (file_hash, _record)) in &diff.modified {
        if let Some((_header, key, value)) = source.get_entry(file_hash)? {
            output.store_entry(EntryType::FileRecord, &key, &value)?;
            let path_key = file_path_hash(path, &patch_algo)?;
            if path_key != key {
                output.store_entry(EntryType::FileRecord, &path_key, &value)?;
            }
            files_modified += 1;
        }
    }

    // Write DeletionRecords for deleted files
    for path in &diff.deleted {
        let algo = source.hash_algo();
        let deletion_record = DeletionRecord::new(path.clone(), Some("patch-deletion".to_string()));
        let deletion_data = deletion_record.serialize();
        let deletion_key = file_path_hash(path, &algo)?;
        output.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_data)?;
        files_deleted += 1;
    }

    // Write changed DirectoryIndexes at both content-hash and path-hash keys
    let algo = output.hash_algo();
    for (path, (dir_hash, _data)) in &diff.changed_directories {
        if let Some((_header, key, value)) = source.get_entry(dir_hash)? {
            output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
            let path_key = directory_path_hash(path, &algo)?;
            if path_key != key {
                output.store_entry(EntryType::DirectoryIndex, &path_key, &value)?;
            }
            dirs_written += 1;
        }
    }

    // Set HEAD to the target hash
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

/// Create a patch from a named snapshot (or HEAD) to another.
pub fn create_patch_from_snapshots(
    source: &StorageEngine,
    from_snapshot: &str,
    to_snapshot: Option<&str>,
    output_path: &str,
) -> EngineResult<PatchResult> {
    let vm = VersionManager::new(source);
    let snapshots = vm.list_snapshots()?;

    let from_hash = snapshots
        .iter()
        .find(|s| s.name == from_snapshot)
        .map(|s| s.root_hash.clone())
        .ok_or_else(|| {
            EngineError::NotFound(format!("Snapshot '{}' not found", from_snapshot))
        })?;

    let to_hash = match to_snapshot {
        Some(name) => {
            snapshots
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.root_hash.clone())
                .ok_or_else(|| {
                    EngineError::NotFound(format!("Snapshot '{}' not found", name))
                })?
        }
        None => source.head_hash()?,
    };

    create_patch(source, &from_hash, &to_hash, output_path)
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

/// Import an export or patch .aeordb file into a target database.
///
/// For full exports (backup_type=1): stores all entries into target.
/// For patches (backup_type=2): verifies base version match, applies changes.
///
/// Does NOT automatically promote HEAD unless `promote` is true.
pub fn import_backup(
    ctx: &RequestContext,
    target: &StorageEngine,
    backup_path: &str,
    force: bool,
    promote: bool,
) -> EngineResult<ImportResult> {
    // Open backup for import (allows patches)
    let backup = StorageEngine::open_for_import(backup_path)?;
    let (backup_type, base_hash, target_hash) = backup.backup_info();

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

    // Import chunks
    let chunk_entries = backup.entries_by_type(KV_TYPE_CHUNK)?;
    for (hash, value) in &chunk_entries {
        if !target.has_entry(hash)? {
            target.store_entry(EntryType::Chunk, hash, value)?;
            chunks_imported += 1;
            entries_imported += 1;
        }
    }

    // Import FileRecords
    let file_entries = backup.entries_by_type(KV_TYPE_FILE_RECORD)?;
    for (hash, value) in &file_entries {
        target.store_entry(EntryType::FileRecord, hash, value)?;
        files_imported += 1;
        entries_imported += 1;
    }

    // Import DirectoryIndexes
    let dir_entries = backup.entries_by_type(KV_TYPE_DIRECTORY)?;
    for (hash, value) in &dir_entries {
        target.store_entry(EntryType::DirectoryIndex, hash, value)?;
        dirs_imported += 1;
        entries_imported += 1;
    }

    // Apply DeletionRecords (for patches)
    if backup_type == 2 {
        let deletion_entries = backup.entries_by_type(KV_TYPE_DELETION)?;
        for (hash, _value) in &deletion_entries {
            // Mark the entry as deleted in the target
            if target.has_entry(hash)? {
                let _ = target.mark_entry_deleted(hash);
                deletions_applied += 1;
                entries_imported += 1;
            }
        }
    }

    // Promote HEAD if requested
    let head_promoted = if promote {
        target.update_head(&target_hash)?;
        true
    } else {
        false
    };

    // Emit import completed event
    ctx.emit(EVENT_IMPORTS_COMPLETED, serde_json::json!({"imports": [ImportEventData {
        backup_type: match backup_type { 1 => "export".to_string(), 2 => "patch".to_string(), _ => "unknown".to_string() },
        version_hash: hex::encode(&target_hash),
        entries_imported,
        head_promoted,
    }]}));

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
                format!(
                    "has NOT been changed.\n  To promote: aeordb promote --hash {}",
                    hex::encode(&self.version_hash)
                )
            },
        )
    }
}
