use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::directory_ops::EngineFileStream;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::SymlinkRecord;

pub(crate) struct ResolvedVersionEntry {
  pub(crate) hash: Vec<u8>,
  pub(crate) header: EntryHeader,
  pub(crate) value: Vec<u8>,
}

fn entry_type_label(entry_type: EntryType) -> &'static str {
  match entry_type {
    EntryType::Chunk => "chunk",
    EntryType::FileRecord => "file",
    EntryType::DirectoryIndex => "directory",
    EntryType::DeletionRecord => "deletion record",
    EntryType::Snapshot => "snapshot",
    EntryType::Void => "void record",
    EntryType::Fork => "fork",
    EntryType::Symlink => "symlink",
  }
}

fn resolve_child(
  engine: &StorageEngine,
  directory_hash: &[u8],
  directory_header: &EntryHeader,
  directory_value: &[u8],
  name: &str,
) -> EngineResult<Option<ChildEntry>> {
  let hash_length = engine.hash_algo().hash_length();
  if crate::engine::btree::is_btree_format(directory_value) {
    crate::engine::btree::btree_lookup(engine, directory_hash, name, hash_length, true)
  } else if directory_value.is_empty() {
    Ok(None)
  } else {
    Ok(
      deserialize_child_entries(directory_value, hash_length, directory_header.entry_version)?.into_iter().find(|child| child.name == name),
    )
  }
}

fn load_version_entry(
  engine: &StorageEngine,
  hash: &[u8],
  expected_type: EntryType,
  context: &str,
) -> EngineResult<(EntryHeader, Vec<u8>)> {
  let (header, stored_key, value) =
    engine.get_entry_verified_including_deleted(hash)?.ok_or_else(|| EngineError::NotFound(format!("{context} {}", hex::encode(hash))))?;
  if stored_key != hash || header.entry_type != expected_type {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!(
        "{context} {} resolved to {:?} under key {} instead of {:?}",
        hex::encode(hash),
        header.entry_type,
        hex::encode(stored_key),
        expected_type
      ),
    });
  }
  Ok((header, value))
}

fn resolve_child_at_version(engine: &StorageEngine, root_hash: &[u8], path: &str) -> EngineResult<ChildEntry> {
  let normalized = normalize_path(path);
  let segments = normalized.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
  if segments.is_empty() {
    return Err(EngineError::NotFound("Empty path".to_string()));
  }

  let mut directory_hash = root_hash.to_vec();
  for (index, segment) in segments.iter().enumerate() {
    let (directory_header, directory_value) = load_version_entry(engine, &directory_hash, EntryType::DirectoryIndex, "version directory")?;
    let child = resolve_child(engine, &directory_hash, &directory_header, &directory_value, segment)?
      .ok_or_else(|| EngineError::NotFound(format!("Path '{normalized}' not found at selected version")))?;
    let child_type = EntryType::from_u8(child.entry_type)?;
    let is_final = index + 1 == segments.len();
    if !is_final {
      if child_type != EntryType::DirectoryIndex {
        return Err(EngineError::NotFound(format!("Directory segment '{segment}' not found at selected version")));
      }
      directory_hash = child.hash;
      continue;
    }
    return Ok(child);
  }

  Err(EngineError::NotFound(format!("Path '{normalized}' not found at selected version")))
}

fn resolve_typed_entry_at_version(
  engine: &StorageEngine,
  root_hash: &[u8],
  path: &str,
  expected_type: EntryType,
) -> EngineResult<ResolvedVersionEntry> {
  let normalized = normalize_path(path);
  let (hash, entry_type) = resolve_entry_reference_at_version(engine, root_hash, &normalized)?;
  if entry_type != expected_type {
    return Err(EngineError::NotFound(format!(
      "Path '{normalized}' is a {}, not a {}, at selected version",
      entry_type_label(entry_type),
      entry_type_label(expected_type)
    )));
  }
  let (header, value) = load_version_entry(engine, &hash, expected_type, "version child")?;
  Ok(ResolvedVersionEntry { hash, header, value })
}

pub(crate) fn resolve_entry_reference_at_version(
  engine: &StorageEngine,
  root_hash: &[u8],
  path: &str,
) -> EngineResult<(Vec<u8>, EntryType)> {
  let normalized = normalize_path(path);
  if normalized == "/" {
    let header = engine
      .get_entry_header_including_deleted(root_hash)?
      .ok_or_else(|| EngineError::NotFound(format!("version root {}", hex::encode(root_hash))))?;
    if header.entry_type != EntryType::DirectoryIndex {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("version root {} resolves to {:?} instead of DirectoryIndex", hex::encode(root_hash), header.entry_type),
      });
    }
    return Ok((root_hash.to_vec(), EntryType::DirectoryIndex));
  }

  let child = resolve_child_at_version(engine, root_hash, &normalized)?;
  Ok((child.hash, EntryType::from_u8(child.entry_type)?))
}

/// Resolve the stored entity type selected by a namespace root without
/// consulting mutable path locators or materializing the final entity body.
pub fn resolve_entry_type_at_version(engine: &StorageEngine, root_hash: &[u8], path: &str) -> EngineResult<EntryType> {
  let normalized = normalize_path(path);
  let (hash, expected_type) = resolve_entry_reference_at_version(engine, root_hash, &normalized)?;
  let header = engine
    .get_entry_header_including_deleted(&hash)?
    .ok_or_else(|| EngineError::NotFound(format!("Path '{normalized}' content is missing at selected version")))?;
  if header.entry_type != expected_type {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("Path '{normalized}' resolves to {:?} instead of {expected_type:?} at selected version", header.entry_type),
    });
  }
  Ok(expected_type)
}

pub(crate) fn resolve_directory_at_version(engine: &StorageEngine, root_hash: &[u8], path: &str) -> EngineResult<ResolvedVersionEntry> {
  resolve_typed_entry_at_version(engine, root_hash, path, EntryType::DirectoryIndex)
}

/// Resolve a file at a historical version by walking only the root-to-file path.
pub fn resolve_file_at_version(engine: &StorageEngine, root_hash: &[u8], path: &str) -> EngineResult<(Vec<u8>, FileRecord)> {
  let normalized = normalize_path(path);
  let resolved = resolve_typed_entry_at_version(engine, root_hash, &normalized, EntryType::FileRecord)?;
  let record = FileRecord::deserialize(&resolved.value, engine.hash_algo().hash_length(), resolved.header.entry_version)?;
  if record.path != normalized {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("version FileRecord path '{}' does not match requested path '{normalized}'", record.path),
    });
  }
  Ok((resolved.hash, record))
}

/// Resolve a symlink at a historical version by walking only its namespace path.
pub fn resolve_symlink_at_version(engine: &StorageEngine, root_hash: &[u8], path: &str) -> EngineResult<(Vec<u8>, SymlinkRecord)> {
  let normalized = normalize_path(path);
  let resolved = resolve_typed_entry_at_version(engine, root_hash, &normalized, EntryType::Symlink)?;
  let record = SymlinkRecord::deserialize(&resolved.value, resolved.header.entry_version)?;
  if record.path != normalized {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("version Symlink path '{}' does not match requested path '{normalized}'", record.path),
    });
  }
  Ok((resolved.hash, record))
}

/// Read a file's full content at a historical version.
pub fn read_file_at_version(engine: &StorageEngine, root_hash: &[u8], path: &str) -> EngineResult<Vec<u8>> {
  let (_hash, file_record) = resolve_file_at_version(engine, root_hash, path)?;
  let stream = EngineFileStream::from_chunk_hashes_including_deleted(file_record.chunk_hashes, engine)?;
  stream.collect_to_vec()
}
