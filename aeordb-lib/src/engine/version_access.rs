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
    let mut dir_data = match engine.get_entry_including_deleted(root_hash)? {
        Some((_header, _key, value)) => value,
        None => {
            tracing::debug!(
                root_hash = %hex::encode(root_hash),
                path = %path,
                "resolve_file_at_version: root hash not found in KV"
            );
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
            match crate::engine::btree::btree_list_from_node(&dir_data, engine, hash_length, true) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(segment = %segment, error = %e, dir_data_len = dir_data.len(),
                        "resolve_file_at_version: btree parse failed for intermediate dir");
                    return Err(EngineError::NotFound(format!("File '{}' not found at version", path)));
                }
            }
        } else {
            match deserialize_child_entries(&dir_data, hash_length, 0) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(segment = %segment, error = %e, dir_data_len = dir_data.len(),
                        "resolve_file_at_version: flat parse failed for intermediate dir");
                    return Err(EngineError::NotFound(format!("File '{}' not found at version", path)));
                }
            }
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
                tracing::debug!(
                    segment = %segment,
                    children_count = children.len(),
                    child_names = %children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "),
                    "resolve_file_at_version: directory segment not found"
                );
                EngineError::NotFound(format!("Directory '{}' not found at version", segment))
            })?;

        dir_data = match engine.get_entry_including_deleted(&child.hash) {
            Ok(Some((_header, _key, value))) => value,
            Ok(None) => {
                tracing::debug!(
                    segment = %segment,
                    child_hash = %hex::encode(&child.hash),
                    "resolve_file_at_version: child dir content hash not found in KV"
                );
                return Err(EngineError::NotFound(format!(
                    "Directory '{}' not found at version",
                    segment
                )));
            }
            Err(e) => {
                tracing::debug!(
                    segment = %segment,
                    child_hash = %hex::encode(&child.hash),
                    error = %e,
                    "resolve_file_at_version: error reading child dir"
                );
                return Err(EngineError::NotFound(format!(
                    "Directory '{}' not found at version",
                    segment
                )));
            }
        };
    }

    // Resolve the final segment as a file
    let final_segment = segments[segments.len() - 1];

    let children: Vec<_> = if crate::engine::btree::is_btree_format(&dir_data) {
        crate::engine::btree::btree_list_from_node(&dir_data, engine, hash_length, true)?
    } else {
        match deserialize_child_entries(&dir_data, hash_length, 0) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    dir_data_len = dir_data.len(),
                    final_segment = %final_segment,
                    error = %e,
                    "resolve_file_at_version: failed to deserialize final directory"
                );
                return Err(EngineError::NotFound(format!("File '{}' not found at version", path)));
            }
        }
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
            // Log what children we DO have for debugging
            let names: Vec<String> = children.iter()
                .map(|c| format!("{}(t={})", c.name, c.entry_type))
                .collect();
            tracing::debug!(
                final_segment = %final_segment,
                children = %names.join(", "),
                "resolve_file_at_version: file not found in directory children"
            );
            EngineError::NotFound(format!("File '{}' not found at version", path))
        })?;

    // Use get_entry_including_deleted to find file records even if the file
    // was deleted after the snapshot was taken.
    let (header, _key, value) = match engine.get_entry_including_deleted(&child.hash) {
        Err(e) => {
            return Err(e);
        }
        Ok(None) => {
            return Err(EngineError::NotFound(format!("File '{}' not found at version", path)));
        }
        Ok(Some(entry)) => entry,
    };

    let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;
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
    let stream = EngineFileStream::from_chunk_hashes_including_deleted(file_record.chunk_hashes, engine)?;
    stream.collect_to_vec()
}
