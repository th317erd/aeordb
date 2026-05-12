use crate::engine::deletion_record::DeletionRecord;
use crate::engine::file_record::FileRecord;
use crate::engine::directory_ops::{file_path_hash, directory_path_hash, is_system_path};

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
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::symlink_path_hash;
use crate::engine::tree_walker::{walk_version_tree, diff_trees, VersionTree};
use crate::engine::entry_type::EntryType;
use crate::engine::version_manager::VersionManager;

/// Export a complete version as a clean, self-contained .aeordb file.
///
/// The output database contains only live entries at the given version:
/// no voids, no deletion records, no stale overwrites, no history.
/// backup_type = 1 (full export), base_hash = target_hash = version_hash.
///
/// If `include_system` is true, all `/.aeordb-system/` entries (users,
/// groups, API keys, etc.) are included. Otherwise they are filtered out.
pub fn export_version(
    source: &StorageEngine,
    version_hash: &[u8],
    output_path: &str,
    include_system: bool,
) -> EngineResult<ExportResult> {
    export_atomic(output_path, |part_path| {
        let tree = walk_version_tree(source, version_hash)?;
        let output = StorageEngine::create(part_path)?;
        output.set_backup_info(1, version_hash, version_hash)?;
        let stats = write_tree_to_engine(&tree, source, &output, include_system)?;
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

/// Wrap an export operation so it writes to `<output_path>.part` first, then
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
        return Err(EngineError::AlreadyExists(format!(
            "export destination '{}' already exists",
            output_path
        )));
    }
    let part_path = format!("{}.part", output_path);
    let _ = std::fs::remove_file(&part_path);

    let result = work(&part_path);
    // `output` is dropped at the end of `work`, which fsyncs the file (see
    // StorageEngine::drop → shutdown → sync_all). The .part file is now durable.

    match result {
        Ok(stats) => {
            std::fs::rename(&part_path, output_path).map_err(EngineError::from)?;
            // fsync the parent directory so the rename survives a crash.
            if let Some(parent) = std::path::Path::new(output_path).parent() {
                let parent_path = if parent.as_os_str().is_empty() {
                    std::path::Path::new(".")
                } else {
                    parent
                };
                if let Ok(dir) = std::fs::File::open(parent_path) {
                    let _ = dir.sync_all();
                }
            }
            Ok(stats)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&part_path);
            Err(error)
        }
    }
}

/// Export HEAD or a named snapshot.
pub fn export_snapshot(
    source: &StorageEngine,
    snapshot_name: Option<&str>,
    output_path: &str,
    include_system: bool,
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

    export_version(source, &version_hash, output_path, include_system)
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
pub fn export_full(
    source: &StorageEngine,
    output_path: &str,
    include_system: bool,
) -> EngineResult<ExportResult> {
    export_atomic(output_path, |part_path| {
        let output = StorageEngine::create(part_path)?;

        let head_hash = source.head_hash()?;
        output.set_backup_info(1, &head_hash, &head_hash)?;

        let mut total_chunks = 0u64;
        let mut total_files = 0u64;
        let mut total_dirs = 0u64;

        let mut walked: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();

        let head_tree = walk_version_tree(source, &head_hash)?;
        let stats = write_tree_to_engine(&head_tree, source, &output, include_system)?;
        total_chunks += stats.0;
        total_files += stats.1;
        total_dirs += stats.2;
        walked.insert(head_hash.clone());

        let vm = VersionManager::new(source);
        let snapshots = vm.list_snapshots()?;
        let snapshot_count = snapshots.len() as u64;
        for snap in &snapshots {
            if walked.contains(&snap.root_hash) {
                continue;
            }
            let tree = walk_version_tree(source, &snap.root_hash)?;
            let stats = write_tree_to_engine(&tree, source, &output, include_system)?;
            total_chunks += stats.0;
            total_files += stats.1;
            total_dirs += stats.2;
            walked.insert(snap.root_hash.clone());
        }

        if include_system {
            let system_stats = export_system_subtree(source, &output)?;
            total_files += system_stats.0;
            total_dirs += system_stats.1;
            copy_snapshot_entries(source, &output)?;
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
) -> EngineResult<(u64, u64, u64)> {
    let mut chunks_written = 0u64;
    let mut files_written = 0u64;
    let mut dirs_written = 0u64;

    // Collect chunk hashes. If include_system is false, exclude chunks
    // that belong exclusively to /.aeordb-system/ files. If true, include all
    // EXCEPT credentials (which are never backed up).
    let mut chunk_hashes_to_write = std::collections::HashSet::new();
    for (path, (_file_hash, record)) in &tree.files {
        if is_credential_path(path) { continue; }
        if include_system || !is_system_path(path) {
            for chunk_hash in &record.chunk_hashes {
                chunk_hashes_to_write.insert(chunk_hash.clone());
            }
        }
    }

    // Write the chunks
    for chunk_hash in &chunk_hashes_to_write {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(chunk_hash)? {
            // Skip if already written (idempotent for repeat exports)
            if output.has_entry(&key)? { continue; }
            output.store_entry(EntryType::Chunk, &key, &value)?;
            chunks_written += 1;
        }
    }

    // Write FileRecords at both content-hash and path-hash keys.
    // The tree walker stores content hashes as file_hash, but read_file
    // looks up by path hash, so both must be present in the exported database.
    let file_algo = output.hash_algo();
    for (path, (file_hash, _record)) in &tree.files {
        if is_credential_path(path) { continue; }
        if !include_system && is_system_path(path) {
            continue;
        }
        if let Some((_header, key, value)) = source.get_entry_including_deleted(file_hash)? {
            if !output.has_entry(&key)? {
                if is_system_path(path) {
                    // System entries need the FLAG_SYSTEM bit set
                    output.store_entry_with_flags(
                        EntryType::FileRecord, &key, &value,
                        crate::engine::entry_header::FLAG_SYSTEM,
                    )?;
                } else {
                    output.store_entry(EntryType::FileRecord, &key, &value)?;
                }
            }
            // Also write at path-hash key (for read_file lookups)
            let path_key = file_path_hash(path, &file_algo)?;
            if path_key != key && !output.has_entry(&path_key)? {
                if is_system_path(path) {
                    output.store_entry_with_flags(
                        EntryType::FileRecord, &path_key, &value,
                        crate::engine::entry_header::FLAG_SYSTEM,
                    )?;
                } else {
                    output.store_entry(EntryType::FileRecord, &path_key, &value)?;
                }
            }
            files_written += 1;
        }
    }

    // Write DirectoryIndexes at both content-hash and path-hash keys.
    let algo = output.hash_algo();
    for (path, (dir_hash, _data)) in &tree.directories {
        if is_credential_path(path) { continue; }
        if !include_system && is_system_path(path) {
            continue;
        }
        if let Some((_header, key, value)) = source.get_entry_including_deleted(dir_hash)? {
            if !output.has_entry(&key)? {
                output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
            }
            let path_key = directory_path_hash(path, &algo)?;
            if path_key != key && !output.has_entry(&path_key)? {
                output.store_entry(EntryType::DirectoryIndex, &path_key, &value)?;
            }
            dirs_written += 1;
        }
    }

    // Write symlink entries at both content-hash and path-hash keys.
    let symlink_algo = output.hash_algo();
    for (path, (symlink_hash, _record)) in &tree.symlinks {
        if is_credential_path(path) { continue; }
        if !include_system && is_system_path(path) {
            continue;
        }
        if let Some((_header, key, value)) = source.get_entry_including_deleted(symlink_hash)? {
            if !output.has_entry(&key)? {
                output.store_entry(EntryType::Symlink, &key, &value)?;
            }
            let path_key = symlink_path_hash(path, &symlink_algo)?;
            if path_key != key && !output.has_entry(&path_key)? {
                output.store_entry(EntryType::Symlink, &path_key, &value)?;
            }
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
fn export_system_subtree(
    source: &StorageEngine,
    output: &StorageEngine,
) -> EngineResult<(u64, u64)> {
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
    let system_single_files: &[&str] = &[
        "/.aeordb-system/email-config.json",
    ];

    let mut sys_tree = crate::engine::tree_walker::VersionTree::new();

    for sys_path in &system_paths {
        let sys_dir_key = directory_path_hash(sys_path, &algo)?;
        let raw_value = match source.get_entry_including_deleted(&sys_dir_key)? {
            Some((_header, _key, value)) => value,
            None => continue, // subdirectory doesn't exist in this source
        };

        // Resolve hard link if present
        let sys_dir_hash = if raw_value.len() == hash_length {
            raw_value
        } else {
            // Inline data — compute content hash to walk it
            algo.compute_hash(&raw_value)?
        };

        // Also record the path-hash → content-hash mapping in the tree
        sys_tree.directories.insert(
            sys_path.to_string(),
            (sys_dir_hash.clone(), Vec::new()),
        );

        if let Err(e) = crate::engine::tree_walker::walk_subtree(
            source, sys_path, &sys_dir_hash, &mut sys_tree,
        ) {
            tracing::warn!("export: failed to walk {}: {}", sys_path, e);
        }
    }

    // Single files under /.aeordb-system/. Each is keyed by path-hash;
    // resolve to a FileRecord and add it to the tree. The content hash for
    // the tree key is recomputed from the serialized FileRecord — that's
    // what the chunk-domain entries use elsewhere in the engine.
    for file_path in system_single_files {
        let key = crate::engine::directory_ops::file_path_hash(file_path, &algo)?;
        let (record, content_hash) = match source.get_entry_including_deleted(&key)? {
            Some((header, _key, raw)) => {
                match FileRecord::deserialize(&raw, hash_length, header.entry_version) {
                    Ok(record) => {
                        let serialized = record.serialize(hash_length)?;
                        let h = algo.compute_hash(&serialized)?;
                        (record, h)
                    }
                    Err(e) => {
                        tracing::warn!("export: failed to deserialize {} as FileRecord: {}", file_path, e);
                        continue;
                    }
                }
            }
            None => continue, // file doesn't exist in source — fine
        };
        sys_tree.files.insert(file_path.to_string(), (content_hash, record));
    }

    // Write system tree entries with overwrite. Old snapshots may have
    // included /.aeordb-system/ in their root listings (before the change
    // that stops system path propagation to root), so a snapshot walk
    // could have written stale system directory data. We need to overwrite
    // those with the current state.
    write_system_tree(&sys_tree, source, output)
}

/// Like write_tree_to_engine but unconditionally writes system entries
/// (overwrites instead of skipping if existing). Used when copying the
/// authoritative current system state.
fn write_system_tree(
    tree: &crate::engine::tree_walker::VersionTree,
    source: &StorageEngine,
    output: &StorageEngine,
) -> EngineResult<(u64, u64)> {
    use crate::engine::entry_header::FLAG_SYSTEM;
    let mut files_written = 0u64;
    let mut dirs_written = 0u64;

    let algo = output.hash_algo();

    // Chunks for system files (system files rarely have chunks, but include them)
    let mut chunk_hashes: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for (_path, (_file_hash, record)) in &tree.files {
        for ch in &record.chunk_hashes { chunk_hashes.insert(ch.clone()); }
    }
    for ch in &chunk_hashes {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(ch)? {
            if !output.has_entry(&key)? {
                output.store_entry(EntryType::Chunk, &key, &value)?;
            }
        }
    }

    // FileRecords (overwrite to ensure latest system state)
    for (path, (file_hash, _record)) in &tree.files {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(file_hash)? {
            output.store_entry_with_flags(EntryType::FileRecord, &key, &value, FLAG_SYSTEM)?;
            let path_key = file_path_hash(path, &algo)?;
            if path_key != key {
                output.store_entry_with_flags(EntryType::FileRecord, &path_key, &value, FLAG_SYSTEM)?;
            }
            files_written += 1;
        }
    }

    // DirectoryIndex (overwrite — this is the critical fix for the api-keys bug)
    for (path, (dir_hash, _data)) in &tree.directories {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(dir_hash)? {
            output.store_entry(EntryType::DirectoryIndex, &key, &value)?;
            let path_key = directory_path_hash(path, &algo)?;
            if path_key != key {
                output.store_entry(EntryType::DirectoryIndex, &path_key, &value)?;
            }
            dirs_written += 1;
        }
    }

    // Symlinks
    for (path, (sym_hash, _record)) in &tree.symlinks {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(sym_hash)? {
            output.store_entry_with_flags(EntryType::Symlink, &key, &value, FLAG_SYSTEM)?;
            let path_key = symlink_path_hash(path, &algo)?;
            if path_key != key {
                output.store_entry_with_flags(EntryType::Symlink, &path_key, &value, FLAG_SYSTEM)?;
            }
        }
    }

    Ok((files_written, dirs_written))
}

/// Copy all Snapshot-type entries from source to output. These represent
/// the version history chain — without them, the imported database has
/// no snapshot list even if the per-snapshot data is present.
fn copy_snapshot_entries(source: &StorageEngine, output: &StorageEngine) -> EngineResult<u64> {
    use crate::engine::kv_store::KV_TYPE_SNAPSHOT;
    let mut copied = 0u64;
    let snapshot_entries = source.entries_by_type(KV_TYPE_SNAPSHOT)?;
    for (hash, value) in snapshot_entries {
        if !output.has_entry(&hash)? {
            output.store_entry(EntryType::Snapshot, &hash, &value)?;
            copied += 1;
        }
    }
    Ok(copied)
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
        if let Some((_header, key, value)) = source.get_entry_including_deleted(chunk_hash)? {
            output.store_entry(EntryType::Chunk, &key, &value)?;
            chunks_written += 1;
        }
    }

    // Write added FileRecords at both content-hash and path-hash keys
    let patch_algo = output.hash_algo();
    for (path, (file_hash, _record)) in &diff.added {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(file_hash)? {
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
        if let Some((_header, key, value)) = source.get_entry_including_deleted(file_hash)? {
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

    // Write added symlinks at both content-hash and path-hash keys
    let symlink_algo = output.hash_algo();
    for (path, (symlink_hash, _record)) in &diff.symlinks_added {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(symlink_hash)? {
            output.store_entry(EntryType::Symlink, &key, &value)?;
            let path_key = symlink_path_hash(path, &symlink_algo)?;
            if path_key != key {
                output.store_entry(EntryType::Symlink, &path_key, &value)?;
            }
        }
    }

    // Write modified symlinks at both content-hash and path-hash keys
    for (path, (symlink_hash, _record)) in &diff.symlinks_modified {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(symlink_hash)? {
            output.store_entry(EntryType::Symlink, &key, &value)?;
            let path_key = symlink_path_hash(path, &symlink_algo)?;
            if path_key != key {
                output.store_entry(EntryType::Symlink, &path_key, &value)?;
            }
        }
    }

    // Write DeletionRecords for deleted symlinks
    for path in &diff.symlinks_deleted {
        let algo = source.hash_algo();
        let deletion_record = DeletionRecord::new(path.clone(), Some("patch-deletion".to_string()));
        let deletion_data = deletion_record.serialize();
        let deletion_key = symlink_path_hash(path, &algo)?;
        output.store_entry(EntryType::DeletionRecord, &deletion_key, &deletion_data)?;
    }

    // Write changed DirectoryIndexes at both content-hash and path-hash keys
    let algo = output.hash_algo();
    for (path, (dir_hash, _data)) in &diff.changed_directories {
        if let Some((_header, key, value)) = source.get_entry_including_deleted(dir_hash)? {
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

/// Detect whether a backup contains any system data (entries with FLAG_SYSTEM set).
/// Used to determine whether root-key authority is required for import.
pub fn backup_contains_system_data(backup: &StorageEngine) -> EngineResult<bool> {
    use crate::engine::entry_header::FLAG_SYSTEM;
    // Scan FileRecords — system data is stored as FileRecords with FLAG_SYSTEM
    let snapshot = backup.kv_snapshot.load();
    let entries = snapshot.iter_by_type(KV_TYPE_FILE_RECORD);
    for entry in entries {
        // Read the entry's flags from its header
        if let Ok(Some((header, _key, _value))) = backup.get_entry_including_deleted(&entry.hash) {
            if header.flags & FLAG_SYSTEM != 0 {
                return Ok(true);
            }
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
            Some(other) => Err(EngineError::InvalidInput(format!(
                "import mode must be 'restore' or 'merge', got '{}'",
                other
            ))),
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
    // Open backup for import (allows patches)
    let backup = StorageEngine::open_for_import(backup_path)?;
    let (backup_type, base_hash, target_hash) = backup.backup_info()?;

    // Restore-mode safety: refuse to clobber live data unless explicitly forced.
    if mode == ImportMode::Restore && !force {
        if !is_target_empty(target)? {
            return Err(EngineError::InvalidInput(
                "target database is not empty; refusing restore.\n\
                 Use mode=merge to union, or pass force=true to overwrite anyway."
                    .to_string(),
            ));
        }
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

    // Helper: read entry with header so we can inspect FLAG_SYSTEM
    let read_entry = |hash: &[u8]| -> EngineResult<Option<(u8, Vec<u8>)>> {
        match backup.get_entry_including_deleted(hash)? {
            Some((header, _key, value)) => Ok(Some((header.flags, value))),
            None => Ok(None),
        }
    };

    // Import chunks (chunks themselves don't carry FLAG_SYSTEM — they're
    // shared between user and system files. Filtering happens at the file/dir level.)
    let chunk_kv_entries = {
        let snapshot = backup.kv_snapshot.load();
        snapshot.iter_by_type(KV_TYPE_CHUNK)
    };
    for entry in chunk_kv_entries {
        if !target.has_entry(&entry.hash)? {
            if let Some((_flags, value)) = read_entry(&entry.hash)? {
                target.store_entry(EntryType::Chunk, &entry.hash, &value)?;
                chunks_imported += 1;
                entries_imported += 1;
            }
        }
    }

    // Import FileRecords (skip system entries when include_system = false)
    let file_kv_entries = {
        let snapshot = backup.kv_snapshot.load();
        snapshot.iter_by_type(KV_TYPE_FILE_RECORD)
    };
    for entry in file_kv_entries {
        if let Some((flags, value)) = read_entry(&entry.hash)? {
            let is_system = flags & FLAG_SYSTEM != 0;
            if is_system && !include_system {
                continue;
            }
            if is_system {
                target.store_entry_with_flags(EntryType::FileRecord, &entry.hash, &value, FLAG_SYSTEM)?;
            } else {
                target.store_entry(EntryType::FileRecord, &entry.hash, &value)?;
            }
            files_imported += 1;
            entries_imported += 1;
        }
    }

    // Import DirectoryIndexes (skip system dirs when include_system = false)
    let dir_kv_entries = {
        let snapshot = backup.kv_snapshot.load();
        snapshot.iter_by_type(KV_TYPE_DIRECTORY)
    };
    for entry in dir_kv_entries {
        if let Some((flags, value)) = read_entry(&entry.hash)? {
            let is_system = flags & FLAG_SYSTEM != 0;
            if is_system && !include_system {
                continue;
            }
            target.store_entry(EntryType::DirectoryIndex, &entry.hash, &value)?;
            dirs_imported += 1;
            entries_imported += 1;
        }
    }

    // Import Symlinks (skip system symlinks when include_system = false)
    let sym_kv_entries = {
        let snapshot = backup.kv_snapshot.load();
        snapshot.iter_by_type(KV_TYPE_SYMLINK)
    };
    for entry in sym_kv_entries {
        if let Some((flags, value)) = read_entry(&entry.hash)? {
            let is_system = flags & FLAG_SYSTEM != 0;
            if is_system && !include_system {
                continue;
            }
            target.store_entry(EntryType::Symlink, &entry.hash, &value)?;
            entries_imported += 1;
        }
    }

    // Import Snapshot-type entries (only when system data is allowed —
    // snapshots reference system snapshot files and aren't useful without them)
    if include_system {
        use crate::engine::kv_store::KV_TYPE_SNAPSHOT;
        let snap_entries = backup.entries_by_type(KV_TYPE_SNAPSHOT)?;
        for (hash, value) in &snap_entries {
            if !target.has_entry(hash)? {
                target.store_entry(EntryType::Snapshot, hash, value)?;
                entries_imported += 1;
            }
        }
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
