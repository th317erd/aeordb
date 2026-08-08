use crate::engine::directory_ops::DirectoryOps;
use crate::engine::directory_ops::{chunk_content_hash, file_identity_hash, symlink_identity_hash};
use crate::engine::batch_commit::BufferedFile;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::merge::{ConflictEntry, MergeOp};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// Store a conflict as a regular database entry under `/.aeordb-conflicts/`.
///
/// Structure:
///   `/.aeordb-conflicts/{path}/.meta` — JSON metadata with winner/loser details
///
/// Conflict evidence remains local authority: the SystemFamily registry omits
/// it from peer replication while logical backup and GC retain it.
pub fn store_conflict(engine: &StorageEngine, ctx: &RequestContext, conflict: &ConflictEntry) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);
  let file = conflict_metadata_file(conflict)?;
  ops.store_file_buffered(ctx, &file.path, &file.data, file.content_type.as_deref())?;

  Ok(())
}

pub(crate) fn conflict_metadata_file(conflict: &ConflictEntry) -> EngineResult<BufferedFile> {
  let normalized_path = crate::engine::path_utils::normalize_path(&conflict.path);
  let base_path = format!("/.aeordb-conflicts{normalized_path}");

  let meta = serde_json::json!({
      "path": normalized_path,
      "conflict_type": format!("{:?}", conflict.conflict_type),
      "auto_winner": "winner",
      "created_at": chrono::Utc::now().timestamp_millis(),
      "winner": {
          "hash": hex::encode(&conflict.winner.hash),
          "virtual_time": conflict.winner.virtual_time,
          "node_id": conflict.winner.node_id,
          "size": conflict.winner.size,
          "content_type": conflict.winner.content_type,
      },
      "loser": {
          "hash": hex::encode(&conflict.loser.hash),
          "virtual_time": conflict.loser.virtual_time,
          "node_id": conflict.loser.node_id,
          "size": conflict.loser.size,
          "content_type": conflict.loser.content_type,
      },
  });

  let meta_json = serde_json::to_vec_pretty(&meta).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  Ok(BufferedFile { path: format!("{}/.meta", base_path), data: meta_json, content_type: Some("application/json".to_string()) })
}

/// Keep only conflicts that do not already have the exact unresolved evidence.
///
/// Detached v3 system families have no historical namespace root. The same
/// divergent pair can therefore be rediscovered after peer checkpoints move.
/// Existing evidence is the durable local acknowledgement for that pair, but
/// it is reusable only after its metadata and immutable versions validate.
pub(crate) fn unrecorded_conflicts(engine: &StorageEngine, conflicts: &[ConflictEntry]) -> EngineResult<Vec<ConflictEntry>> {
  let mut unrecorded = Vec::with_capacity(conflicts.len());
  for conflict in conflicts {
    if !exact_conflict_is_recorded(engine, conflict)? {
      unrecorded.push(conflict.clone());
    }
  }
  Ok(unrecorded)
}

fn exact_conflict_is_recorded(engine: &StorageEngine, conflict: &ConflictEntry) -> EngineResult<bool> {
  let normalized_path = crate::engine::path_utils::normalize_path(&conflict.path);
  let metadata_path = conflict_metadata_path(&normalized_path);
  let operations = DirectoryOps::new(engine);
  let metadata = match load_conflict_metadata(&operations, &normalized_path, &metadata_path) {
    Ok(metadata) => metadata,
    Err(EngineError::NotFound(_)) => return Ok(false),
    Err(error) => return Err(error),
  };

  if metadata.conflict_type != format!("{:?}", conflict.conflict_type)
    || !stored_conflict_version_matches(&metadata.winner, &conflict.winner)
    || !stored_conflict_version_matches(&metadata.loser, &conflict.loser)
  {
    return Ok(false);
  }

  for (label, version) in [("winner", &metadata.winner), ("loser", &metadata.loser)] {
    if version.hash.is_empty() {
      validate_conflict_tombstone(version)?;
    } else {
      load_conflict_version(engine, version, label, &normalized_path)?;
    }
  }
  Ok(true)
}

fn stored_conflict_version_matches(stored: &StoredConflictVersion, expected: &crate::engine::merge::ConflictVersion) -> bool {
  stored.hash == hex::encode(&expected.hash)
    && stored.virtual_time == expected.virtual_time
    && stored.node_id == expected.node_id
    && stored.size == expected.size
    && stored.content_type == expected.content_type
}

/// List all unresolved conflicts.
///
/// Walks the `/.aeordb-conflicts/` directory tree recursively, collecting
/// every `.meta` file it finds.
pub fn list_conflicts(engine: &StorageEngine) -> EngineResult<Vec<serde_json::Value>> {
  let ops = DirectoryOps::new(engine);
  let mut conflicts = Vec::new();

  // Use recursive listing to find all .meta files under /.aeordb-conflicts
  let entries = match crate::engine::directory_listing::list_directory_recursive_strict(
    engine,
    "/.aeordb-conflicts",
    -1,             // unlimited depth
    Some("*.meta"), // glob for .meta files only
    None,
  ) {
    Ok(e) => e,
    Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
    Err(e) => return Err(e),
  };

  for entry in &entries {
    if entry.name == ".meta" {
      let data = ops.read_file_buffered(&entry.path)?;
      let meta = serde_json::from_slice::<serde_json::Value>(&data).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
      conflicts.push(meta);
    }
  }

  Ok(conflicts)
}

/// Get a specific conflict's metadata.
pub fn get_conflict(engine: &StorageEngine, path: &str) -> EngineResult<Option<serde_json::Value>> {
  let ops = DirectoryOps::new(engine);
  let meta_path = format!("/.aeordb-conflicts{}/.meta", path);

  match ops.read_file_buffered(&meta_path) {
    Ok(data) => {
      let meta = serde_json::from_slice(&data).map_err(|e| EngineError::JsonParseError(e.to_string()))?;
      Ok(Some(meta))
    }
    Err(EngineError::NotFound(_)) => Ok(None),
    Err(e) => Err(e),
  }
}

/// Resolve a conflict by picking a version ("winner" or "loser").
///
/// The chosen file or symlink version is validated by its identity hash and
/// published at the real path. An empty version hash selects deletion. The
/// selected mutation and conflict-evidence cleanup share one receipt.
pub fn resolve_conflict(engine: &StorageEngine, ctx: &RequestContext, path: &str, pick: &str) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);
  let normalized_path = crate::engine::path_utils::normalize_path(path);
  let meta_path = conflict_metadata_path(&normalized_path);
  let meta = load_conflict_metadata(&ops, &normalized_path, &meta_path)?;

  // Validate the pick value
  if pick != "winner" && pick != "loser" {
    return Err(EngineError::InvalidInput(format!("Invalid pick '{}': must be 'winner' or 'loser'", pick)));
  }

  let chosen = if pick == "winner" { &meta.winner } else { &meta.loser };
  let selected_operation = if chosen.hash.is_empty() {
    validate_conflict_tombstone(chosen)?;
    let retained = if pick == "winner" { &meta.loser } else { &meta.winner };
    match load_conflict_version(engine, retained, "retained", &normalized_path)? {
      LoadedConflictVersion::File(_) => MergeOp::DeleteFile { path: normalized_path },
      LoadedConflictVersion::Symlink(_) => MergeOp::DeleteSymlink { path: normalized_path },
    }
  } else {
    match load_conflict_version(engine, chosen, "chosen", &normalized_path)? {
      LoadedConflictVersion::File(mut record) => {
        record.content_hash = validate_conflict_file_chunks(engine, &record)?;
        record.path = normalized_path.clone();
        let identity = file_identity_hash(&normalized_path, record.content_type.as_deref(), &record.chunk_hashes, &engine.hash_algo())?;
        MergeOp::AddFile { path: normalized_path, file_hash: identity, file_record: record }
      }
      LoadedConflictVersion::Symlink(mut record) => {
        record.path = normalized_path.clone();
        let identity = symlink_identity_hash(&normalized_path, &record.target, &engine.hash_algo())?;
        MergeOp::AddSymlink { path: normalized_path, symlink_hash: identity, symlink_record: record }
      }
    }
  };
  ops.apply_sync_merge(ctx, &[selected_operation, MergeOp::DeleteFile { path: meta_path }])
}

/// Dismiss a conflict (accept the auto-winner, just clean up the conflict entry).
///
/// The auto-winner is already at the real path from the merge, so we only
/// need to remove the conflict metadata.
pub fn dismiss_conflict(engine: &StorageEngine, ctx: &RequestContext, path: &str) -> EngineResult<()> {
  let ops = DirectoryOps::new(engine);
  let normalized_path = crate::engine::path_utils::normalize_path(path);
  let meta_path = conflict_metadata_path(&normalized_path);

  match load_conflict_metadata(&ops, &normalized_path, &meta_path) {
    Ok(_) => {}
    Err(EngineError::NotFound(_)) => {
      return Err(EngineError::NotFound(format!("No conflict found for path: {}", path)));
    }
    Err(e) => return Err(e),
  }

  ops.apply_sync_merge(ctx, &[MergeOp::DeleteFile { path: meta_path }])
}

pub(crate) struct ConflictVersionReference {
  pub(crate) path: String,
  pub(crate) hash: Vec<u8>,
}

/// Resolve every immutable version referenced by unresolved conflict evidence.
///
/// Conflict metadata is authoritative local state, so GC must treat these
/// hashes as edges rather than opaque JSON. Every record is validated against
/// its metadata path and referenced FileRecord/symlink before any sweep can
/// begin. A malformed record therefore aborts mark instead of authorizing data
/// loss.
pub(crate) fn retained_conflict_version_references(engine: &StorageEngine) -> EngineResult<Vec<ConflictVersionReference>> {
  let entries =
    match crate::engine::directory_listing::list_directory_recursive_strict(engine, "/.aeordb-conflicts", -1, Some("*.meta"), None) {
      Ok(entries) => entries,
      Err(EngineError::NotFound(_)) => return Ok(Vec::new()),
      Err(error) => return Err(error),
    };
  let operations = DirectoryOps::new(engine);
  let mut references = Vec::new();
  for entry in entries {
    if entry.name != ".meta" {
      continue;
    }
    let relative = entry.path.strip_prefix("/.aeordb-conflicts").ok_or_else(|| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("Conflict metadata path {:?} is outside the conflict authority", entry.path),
    })?;
    let conflict_path = relative.strip_suffix("/.meta").ok_or_else(|| EngineError::CorruptEntry {
      offset: 0,
      reason: format!("Conflict metadata path {:?} does not end in /.meta", entry.path),
    })?;
    if conflict_path.is_empty() || conflict_path == "/" || crate::engine::path_utils::normalize_path(conflict_path) != conflict_path {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Conflict metadata path {:?} does not identify a canonical file or symlink path", entry.path),
      });
    }
    let metadata = load_conflict_metadata(&operations, conflict_path, &entry.path)?;
    for (label, version) in [("winner", &metadata.winner), ("loser", &metadata.loser)] {
      if version.hash.is_empty() {
        validate_conflict_tombstone(version)?;
        continue;
      }
      let hash = decode_conflict_hash(engine, version, label)?;
      load_conflict_version(engine, version, label, conflict_path)?;
      references.push(ConflictVersionReference { path: conflict_path.to_string(), hash });
    }
  }
  Ok(references)
}

fn conflict_metadata_path(normalized_path: &str) -> String {
  format!("/.aeordb-conflicts{normalized_path}/.meta")
}

fn load_conflict_metadata(ops: &DirectoryOps<'_>, normalized_path: &str, meta_path: &str) -> EngineResult<StoredConflictMetadata> {
  let meta_data = ops.read_file_buffered(meta_path)?;
  let meta: StoredConflictMetadata = serde_json::from_slice(&meta_data)
    .map_err(|error| EngineError::JsonParseError(format!("conflict evidence '{meta_path}' is malformed: {error}")))?;
  if meta.path != normalized_path {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("Conflict metadata path {:?} does not match evidence path {normalized_path:?}", meta.path),
    });
  }

  let winner_is_deletion = meta.winner.hash.is_empty();
  let loser_is_deletion = meta.loser.hash.is_empty();
  match meta.conflict_type.as_str() {
    "ConcurrentModify" | "ConcurrentCreate" if !winner_is_deletion && !loser_is_deletion => {}
    "ModifyDelete" if winner_is_deletion != loser_is_deletion => {}
    "ConcurrentModify" | "ConcurrentCreate" | "ModifyDelete" => {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Conflict metadata type {} is inconsistent with its versions", meta.conflict_type),
      });
    }
    _ => {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("Unknown conflict metadata type {}", meta.conflict_type) });
    }
  }

  Ok(meta)
}

enum LoadedConflictVersion {
  File(crate::engine::file_record::FileRecord),
  Symlink(crate::engine::symlink_record::SymlinkRecord),
}

fn load_conflict_version(
  engine: &StorageEngine,
  version: &StoredConflictVersion,
  label: &str,
  expected_path: &str,
) -> EngineResult<LoadedConflictVersion> {
  let hash_length = engine.hash_algo().hash_length();
  let hash = decode_conflict_hash(engine, version, label)?;
  let (header, stored_key, value) = engine
    .get_entry_verified(&hash)?
    .ok_or_else(|| EngineError::NotFound(format!("{label} conflict version {} is missing", version.hash)))?;
  if stored_key != hash {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("{label} conflict version {} did not resolve to its exact key", version.hash),
    });
  }

  match header.entry_type {
    EntryType::FileRecord => {
      let record = crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version)?;
      if record.path != expected_path {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("{label} conflict FileRecord path {:?} does not match evidence path {expected_path:?}", record.path),
        });
      }
      let identity = file_identity_hash(&record.path, record.content_type.as_deref(), &record.chunk_hashes, &engine.hash_algo())?;
      if identity != hash {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("{label} conflict FileRecord {} is stored under a noncanonical identity", version.hash),
        });
      }
      if version.size != record.total_size || version.content_type != record.content_type {
        return Err(EngineError::CorruptEntry { offset: 0, reason: format!("{label} conflict metadata does not match its FileRecord") });
      }
      Ok(LoadedConflictVersion::File(record))
    }
    EntryType::Symlink => {
      let record = crate::engine::symlink_record::SymlinkRecord::deserialize(&value, header.entry_version)?;
      if record.path != expected_path {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("{label} conflict symlink path {:?} does not match evidence path {expected_path:?}", record.path),
        });
      }
      let identity = symlink_identity_hash(&record.path, &record.target, &engine.hash_algo())?;
      if identity != hash {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("{label} conflict symlink {} is stored under a noncanonical identity", version.hash),
        });
      }
      if version.size != 0 || version.content_type.is_some() {
        return Err(EngineError::CorruptEntry { offset: 0, reason: format!("{label} conflict metadata does not describe a symlink") });
      }
      Ok(LoadedConflictVersion::Symlink(record))
    }
    entry_type => Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("{label} conflict hash {} resolves to unsupported entry type {entry_type:?}", version.hash),
    }),
  }
}

fn decode_conflict_hash(engine: &StorageEngine, version: &StoredConflictVersion, label: &str) -> EngineResult<Vec<u8>> {
  let hash_length = engine.hash_algo().hash_length();
  let hash = hex::decode(&version.hash).map_err(|_| EngineError::InvalidInput(format!("Invalid {label} conflict hash hex")))?;
  if hash.len() != hash_length {
    return Err(EngineError::InvalidInput(format!("{label} conflict hash must be exactly {hash_length} bytes")));
  }
  Ok(hash)
}

fn validate_conflict_tombstone(version: &StoredConflictVersion) -> EngineResult<()> {
  if version.size != 0 || version.content_type.is_some() {
    return Err(EngineError::CorruptEntry { offset: 0, reason: "Conflict deletion version carries nonempty file metadata".to_string() });
  }
  Ok(())
}

fn validate_conflict_file_chunks(engine: &StorageEngine, record: &crate::engine::file_record::FileRecord) -> EngineResult<Vec<u8>> {
  let mut content_hasher = engine.hash_algo().incremental_hasher()?;
  let mut total_size = 0u64;
  for chunk_hash in &record.chunk_hashes {
    let chunk_data = engine
      .read_chunk(chunk_hash)?
      .ok_or_else(|| EngineError::NotFound(format!("Chosen conflict FileRecord references missing chunk {}", hex::encode(chunk_hash))))?;
    if chunk_content_hash(&chunk_data, &engine.hash_algo())? != *chunk_hash {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Chosen conflict chunk {} does not match its content-addressed key", hex::encode(chunk_hash)),
      });
    }
    total_size = total_size
      .checked_add(chunk_data.len() as u64)
      .ok_or_else(|| EngineError::ResourceExhausted("Chosen conflict file size overflow".to_string()))?;
    content_hasher.update(&chunk_data);
  }
  if total_size != record.total_size {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("Chosen conflict FileRecord declares {} bytes but its chunks contain {} bytes", record.total_size, total_size),
    });
  }
  let computed_content_hash = content_hasher.finalize();
  if !record.content_hash.is_empty() && computed_content_hash != record.content_hash {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: "Chosen conflict FileRecord whole-file hash does not match its chunks".to_string(),
    });
  }
  Ok(computed_content_hash)
}

#[derive(serde::Deserialize)]
struct StoredConflictMetadata {
  path: String,
  conflict_type: String,
  winner: StoredConflictVersion,
  loser: StoredConflictVersion,
}

#[derive(serde::Deserialize)]
struct StoredConflictVersion {
  hash: String,
  virtual_time: u64,
  node_id: u64,
  size: u64,
  content_type: Option<String>,
}
