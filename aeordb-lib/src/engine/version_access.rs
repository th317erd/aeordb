use crate::engine::directory_entry::deserialize_child_entries;
use crate::engine::directory_ops::EngineFileStream;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;

/// Resolve a file at a historical version by walking the directory tree path-by-path.
/// O(depth) -- only reads directories on the path to the target file.
pub fn resolve_file_at_version(
    engine: &StorageEngine,
    root_hash: &[u8],
    path: &str,
) -> EngineResult<(Vec<u8>, FileRecord)> {
    let normalized = normalize_path(path);
    let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();

    if segments.is_empty() {
        return Err(EngineError::NotFound("Empty path".to_string()));
    }

    // Load the root directory
    let mut dir_data = match engine.get_entry(root_hash)? {
        Some((_header, _key, value)) => value,
        None => {
            return Err(EngineError::NotFound(format!(
                "Directory not found at version for path '{}'",
                path
            )));
        }
    };

    let hash_length = engine.hash_algo().hash_length();

    // Walk intermediate directory segments
    for segment in &segments[..segments.len() - 1] {
        let children = if crate::engine::btree::is_btree_format(&dir_data) {
            crate::engine::btree::btree_list_from_node(&dir_data, engine, hash_length)?
        } else {
            deserialize_child_entries(&dir_data, hash_length)?
        };

        let child = children
            .iter()
            .find(|c| {
                c.name == *segment
                    && EntryType::from_u8(c.entry_type)
                        .map(|t| t == EntryType::DirectoryIndex)
                        .unwrap_or(false)
            })
            .ok_or_else(|| {
                EngineError::NotFound(format!("Directory '{}' not found at version", segment))
            })?;

        dir_data = match engine.get_entry(&child.hash)? {
            Some((_header, _key, value)) => value,
            None => {
                return Err(EngineError::NotFound(format!(
                    "Directory '{}' not found at version",
                    segment
                )));
            }
        };
    }

    // Resolve the final segment as a file
    let final_segment = segments[segments.len() - 1];

    let children = if crate::engine::btree::is_btree_format(&dir_data) {
        crate::engine::btree::btree_list_from_node(&dir_data, engine, hash_length)?
    } else {
        deserialize_child_entries(&dir_data, hash_length)?
    };

    // Check if the final segment is a symlink — return a specific error if so
    let is_symlink = children
        .iter()
        .any(|c| {
            c.name == final_segment
                && EntryType::from_u8(c.entry_type)
                    .map(|t| t == EntryType::Symlink)
                    .unwrap_or(false)
        });
    if is_symlink {
        return Err(EngineError::NotFound(
            format!("Path '{}' is a symlink at this version, not a file", path)
        ));
    }

    let child = children
        .iter()
        .find(|c| {
            c.name == final_segment
                && EntryType::from_u8(c.entry_type)
                    .map(|t| t == EntryType::FileRecord)
                    .unwrap_or(false)
        })
        .ok_or_else(|| {
            EngineError::NotFound(format!("File '{}' not found at version", path))
        })?;

    let value = match engine.get_entry(&child.hash)? {
        Some((_header, _key, value)) => value,
        None => {
            return Err(EngineError::NotFound(format!(
                "File '{}' not found at version",
                path
            )));
        }
    };

    let file_record = FileRecord::deserialize(&value, hash_length)?;
    Ok((child.hash.clone(), file_record))
}

/// Read a file's full content at a historical version.
/// Resolves the FileRecord, then reads and concatenates all chunks.
pub fn read_file_at_version(
    engine: &StorageEngine,
    root_hash: &[u8],
    path: &str,
) -> EngineResult<Vec<u8>> {
    let (_hash, file_record) = resolve_file_at_version(engine, root_hash, path)?;
    let stream = EngineFileStream::from_chunk_hashes(file_record.chunk_hashes, engine)?;
    stream.collect_to_vec()
}
