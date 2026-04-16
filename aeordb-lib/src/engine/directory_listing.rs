use crate::engine::directory_entry::deserialize_child_entries;
use crate::engine::directory_ops::directory_path_hash;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;

/// A file entry from a directory listing with full path and content hash.
pub struct ListingEntry {
    pub path: String,
    pub name: String,
    pub entry_type: u8,
    pub hash: Vec<u8>,
    pub total_size: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub content_type: Option<String>,
    /// Symlink target path (only populated for symlink entries)
    pub target: Option<String>,
}

/// List files in a directory with optional recursion and glob filtering.
///
/// - `depth`: 0 = immediate children only, positive = that many levels, -1 = unlimited
/// - `glob_pattern`: optional glob matched against file NAME only (not full path)
///
/// Returns files only (no directory entries) when recursing (depth > 0 or depth == -1).
/// At depth=0, returns both files and directories (for backwards compat with existing listing).
pub fn list_directory_recursive(
    engine: &StorageEngine,
    base_path: &str,
    depth: i32,
    glob_pattern: Option<&str>,
) -> EngineResult<Vec<ListingEntry>> {
    let normalized = normalize_path(base_path);
    let algo = engine.hash_algo();
    let hash_length = algo.hash_length();

    let dir_key = directory_path_hash(&normalized, &algo)?;
    let value = match engine.get_entry(&dir_key)? {
        Some((_header, _key, value)) => value,
        None => return Err(EngineError::NotFound(normalized)),
    };

    if value.is_empty() {
        return Ok(Vec::new());
    }

    let children = if crate::engine::btree::is_btree_format(&value) {
        crate::engine::btree::btree_list_from_node(&value, engine, hash_length)?
    } else {
        deserialize_child_entries(&value, hash_length, 0)?
    };

    // recursive_mode: when depth > 0 or depth == -1, we only return files
    let recursive_mode = depth != 0;

    let mut results = Vec::new();
    walk_listing(
        engine,
        &children,
        &normalized,
        depth,
        recursive_mode,
        glob_pattern,
        hash_length,
        &mut results,
    )?;

    Ok(results)
}

fn walk_listing(
    engine: &StorageEngine,
    children: &[crate::engine::directory_entry::ChildEntry],
    current_path: &str,
    remaining_depth: i32,
    recursive_mode: bool,
    glob_pattern: Option<&str>,
    hash_length: usize,
    results: &mut Vec<ListingEntry>,
) -> EngineResult<()> {
    for child in children {
        let child_path = if current_path == "/" {
            format!("/{}", child.name)
        } else {
            format!("{}/{}", current_path, child.name)
        };

        let entry_type = EntryType::from_u8(child.entry_type)?;

        match entry_type {
            EntryType::FileRecord => {
                if let Some(pattern) = glob_pattern {
                    if !glob_match::glob_match(pattern, &child.name) {
                        continue;
                    }
                }
                results.push(ListingEntry {
                    path: child_path,
                    name: child.name.clone(),
                    entry_type: child.entry_type,
                    hash: child.hash.clone(),
                    total_size: child.total_size,
                    created_at: child.created_at,
                    updated_at: child.updated_at,
                    content_type: child.content_type.clone(),
                    target: None,
                });
            }
            EntryType::DirectoryIndex => {
                if !recursive_mode {
                    // depth=0 mode: include directories in output, do NOT recurse
                    if let Some(pattern) = glob_pattern {
                        if !glob_match::glob_match(pattern, &child.name) {
                            continue;
                        }
                    }
                    results.push(ListingEntry {
                        path: child_path,
                        name: child.name.clone(),
                        entry_type: child.entry_type,
                        hash: child.hash.clone(),
                        total_size: child.total_size,
                        created_at: child.created_at,
                        updated_at: child.updated_at,
                        content_type: child.content_type.clone(),
                        target: None,
                    });
                } else if remaining_depth > 0 || remaining_depth == -1 {
                    // Recursive mode: traverse into subdirectory, do NOT include dir in output
                    if let Some((_header, _key, sub_value)) = engine.get_entry(&child.hash)? {
                        if !sub_value.is_empty() {
                            let sub_children =
                                if crate::engine::btree::is_btree_format(&sub_value) {
                                    crate::engine::btree::btree_list_from_node(
                                        &sub_value,
                                        engine,
                                        hash_length,
                                    )?
                                } else {
                                    deserialize_child_entries(&sub_value, hash_length, 0)?
                                };

                            let next_depth = if remaining_depth == -1 {
                                -1
                            } else {
                                remaining_depth - 1
                            };

                            walk_listing(
                                engine,
                                &sub_children,
                                &child_path,
                                next_depth,
                                recursive_mode,
                                glob_pattern,
                                hash_length,
                                results,
                            )?;
                        }
                    }
                }
                // remaining_depth == 0 in recursive mode: don't include dir, don't recurse
            }
            EntryType::Symlink => {
                if let Some(pattern) = glob_pattern {
                    if !glob_match::glob_match(pattern, &child.name) {
                        continue;
                    }
                }

                // Load the SymlinkRecord to get the target
                let target = if let Ok(Some((_header, _key, value))) = engine.get_entry(&child.hash) {
                    if let Ok(record) = crate::engine::symlink_record::SymlinkRecord::deserialize(&value, 0) {
                        Some(record.target)
                    } else {
                        None
                    }
                } else {
                    None
                };

                results.push(ListingEntry {
                    path: child_path,
                    name: child.name.clone(),
                    entry_type: child.entry_type,
                    hash: child.hash.clone(),
                    total_size: child.total_size,
                    created_at: child.created_at,
                    updated_at: child.updated_at,
                    content_type: child.content_type.clone(),
                    target,
                });
            }
            _ => {
                // Skip other entry types
            }
        }
    }

    Ok(())
}
