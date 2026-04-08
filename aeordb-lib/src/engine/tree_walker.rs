use std::collections::{HashMap, HashSet};

use crate::engine::directory_entry::deserialize_child_entries;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::file_record::FileRecord;
use crate::engine::storage_engine::StorageEngine;

/// The complete tree state at a version: all files, directories, and chunk hashes.
#[derive(Debug, Clone)]
pub struct VersionTree {
  /// All files: path -> (file_hash, FileRecord)
  pub files: HashMap<String, (Vec<u8>, FileRecord)>,
  /// All directory entries: path -> (dir_hash, raw_data)
  pub directories: HashMap<String, (Vec<u8>, Vec<u8>)>,
  /// All chunk hashes referenced by any file in the tree
  pub chunks: HashSet<Vec<u8>>,
}

impl VersionTree {
  pub fn new() -> Self {
    VersionTree {
      files: HashMap::new(),
      directories: HashMap::new(),
      chunks: HashSet::new(),
    }
  }
}

/// Walk a version's directory tree starting from a root hash.
/// Collects all files, directories, and chunk hashes reachable from the root.
pub fn walk_version_tree(
  engine: &StorageEngine,
  root_hash: &[u8],
) -> EngineResult<VersionTree> {
  let mut tree = VersionTree::new();
  let hash_length = engine.hash_algo().hash_length();
  walk_directory(engine, root_hash, "/", hash_length, &mut tree)?;
  Ok(tree)
}

/// Recursively walk a directory and its children.
fn walk_directory(
  engine: &StorageEngine,
  dir_hash: &[u8],
  current_path: &str,
  hash_length: usize,
  tree: &mut VersionTree,
) -> EngineResult<()> {
  // Load the directory entry from the engine
  let dir_data = match engine.get_entry(dir_hash)? {
    Some((_header, _key, value)) => value,
    None => return Ok(()), // directory hash not found, skip
  };

  // Store the directory itself
  tree.directories.insert(
    current_path.to_string(),
    (dir_hash.to_vec(), dir_data.clone()),
  );

  // Empty directory — no children to parse
  if dir_data.is_empty() {
    return Ok(());
  }

  // Parse child entries from the directory data — handle both flat and B-tree formats
  let children = if crate::engine::btree::is_btree_format(&dir_data) {
    crate::engine::btree::btree_list_from_node(&dir_data, engine, hash_length)?
  } else {
    deserialize_child_entries(&dir_data, hash_length)?
  };

  for child in &children {
    let child_path = if current_path == "/" {
      format!("/{}", child.name)
    } else {
      format!("{}/{}", current_path, child.name)
    };

    let child_entry_type = EntryType::from_u8(child.entry_type)?;

    match child_entry_type {
      EntryType::DirectoryIndex => {
        // Recurse into subdirectory using the hash stored in ChildEntry
        walk_directory(engine, &child.hash, &child_path, hash_length, tree)?;
      }
      EntryType::FileRecord => {
        // Load the file record using the hash stored in ChildEntry
        if let Some((_header, _key, value)) = engine.get_entry(&child.hash)? {
          let file_record = FileRecord::deserialize(&value, hash_length)?;

          // Collect all chunk hashes from this file
          for chunk_hash in &file_record.chunk_hashes {
            tree.chunks.insert(chunk_hash.clone());
          }

          tree.files.insert(
            child_path.clone(),
            (child.hash.clone(), file_record),
          );
        }
      }
      _ => {
        // Skip other entry types (voids, deletions, etc.)
      }
    }
  }

  Ok(())
}

/// The result of comparing two version trees.
#[derive(Debug, Clone)]
pub struct TreeDiff {
  /// Files added (path -> (file_hash, FileRecord))
  pub added: HashMap<String, (Vec<u8>, FileRecord)>,
  /// Files modified (path -> (new_file_hash, new FileRecord))
  pub modified: HashMap<String, (Vec<u8>, FileRecord)>,
  /// Files deleted (paths)
  pub deleted: Vec<String>,
  /// Chunks that exist in target but not in base
  pub new_chunks: HashSet<Vec<u8>>,
  /// Directories that were added or changed
  pub changed_directories: HashMap<String, (Vec<u8>, Vec<u8>)>,
}

impl TreeDiff {
  pub fn is_empty(&self) -> bool {
    self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
  }
}

/// Compute the diff between two version trees.
/// Returns a TreeDiff with added, modified, deleted files and new chunks.
pub fn diff_trees(base: &VersionTree, target: &VersionTree) -> TreeDiff {
  let mut added = HashMap::new();
  let mut modified = HashMap::new();
  let mut deleted = Vec::new();

  // Files in target but not base -> added
  // Files in both but different content -> modified
  // Note: file hashes are path-based (deterministic per path), so we compare
  // chunk_hashes to detect actual content changes.
  for (path, (target_hash, target_record)) in &target.files {
    match base.files.get(path) {
      None => {
        added.insert(path.clone(), (target_hash.clone(), target_record.clone()));
      }
      Some((_, base_record)) => {
        if base_record.chunk_hashes != target_record.chunk_hashes {
          modified.insert(path.clone(), (target_hash.clone(), target_record.clone()));
        }
      }
    }
  }

  // Files in base but not target -> deleted
  for path in base.files.keys() {
    if !target.files.contains_key(path) {
      deleted.push(path.clone());
    }
  }

  // New chunks: chunks in target tree but not in base tree
  let new_chunks: HashSet<Vec<u8>> = target
    .chunks
    .difference(&base.chunks)
    .cloned()
    .collect();

  // Changed directories
  let changed_directories = diff_directories(&base.directories, &target.directories);

  TreeDiff {
    added,
    modified,
    deleted,
    new_chunks,
    changed_directories,
  }
}

/// Find directories that changed between base and target.
/// Compares raw data (not hash) because directory hashes are path-based.
fn diff_directories(
  base: &HashMap<String, (Vec<u8>, Vec<u8>)>,
  target: &HashMap<String, (Vec<u8>, Vec<u8>)>,
) -> HashMap<String, (Vec<u8>, Vec<u8>)> {
  let mut changed = HashMap::new();
  for (path, (target_hash, target_data)) in target {
    match base.get(path) {
      None => {
        changed.insert(path.clone(), (target_hash.clone(), target_data.clone()));
      }
      Some((_, base_data)) => {
        if base_data != target_data {
          changed.insert(path.clone(), (target_hash.clone(), target_data.clone()));
        }
      }
    }
  }
  changed
}
