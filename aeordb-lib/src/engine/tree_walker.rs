use std::collections::{HashMap, HashSet};

use crate::engine::directory_entry::deserialize_child_entries;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::file_record::FileRecord;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::symlink_record::SymlinkRecord;

/// The complete tree state at a version: all files, directories, symlinks, and chunk hashes.
#[derive(Debug, Clone)]
pub struct VersionTree {
  /// All files: path -> (file_hash, FileRecord)
  pub files: HashMap<String, (Vec<u8>, FileRecord)>,
  /// All directory entries: path -> (dir_hash, raw_data)
  pub directories: HashMap<String, (Vec<u8>, Vec<u8>)>,
  /// All chunk hashes referenced by any file in the tree
  pub chunks: HashSet<Vec<u8>>,
  /// All symlinks: path -> (symlink_hash, SymlinkRecord)
  pub symlinks: HashMap<String, (Vec<u8>, SymlinkRecord)>,
}

impl VersionTree {
  pub fn new() -> Self {
    VersionTree {
      files: HashMap::new(),
      directories: HashMap::new(),
      chunks: HashSet::new(),
      symlinks: HashMap::new(),
    }
  }
}

/// Walk a version's directory tree starting from a root hash.
/// Collects all files, directories, and chunk hashes reachable from the root.
///
/// Uses a visited set for cycle detection: if corrupted data creates a
/// circular reference (directory A contains directory B which contains A),
/// the walk terminates that branch instead of recursing infinitely.
pub fn walk_version_tree(
  engine: &StorageEngine,
  root_hash: &[u8],
) -> EngineResult<VersionTree> {
  let mut tree = VersionTree::new();
  let mut visited = HashSet::new();
  let hash_length = engine.hash_algo().hash_length();
  walk_directory(engine, root_hash, "/", hash_length, &mut tree, &mut visited)?;
  Ok(tree)
}

/// Walk a subtree rooted at a given path. Used for collecting system data
/// (/.aeordb-system/) which is not reachable from the user-visible HEAD tree
/// because system paths are not propagated to root.
///
/// Adds entries into the provided tree.
pub fn walk_subtree(
  engine: &StorageEngine,
  start_path: &str,
  start_dir_hash: &[u8],
  tree: &mut VersionTree,
) -> EngineResult<()> {
  let mut visited = HashSet::new();
  let hash_length = engine.hash_algo().hash_length();
  walk_directory(engine, start_dir_hash, start_path, hash_length, tree, &mut visited)
}

/// Augment `tree` with `/.aeordb-system/{users,groups,snapshots,config}` and
/// `/.aeordb-config` subtrees plus the single-file `email-config.json`.
///
/// This is what replication peers use to merge system data into a tree the
/// diff is computed from. `walk_version_tree(HEAD)` deliberately does NOT
/// include system paths; this function fills the gap.
///
/// Credential subdirectories (`api-keys`, `refresh-tokens`, `magic-links`)
/// are excluded — they're tied to the issuing node's identity and must not
/// replicate.
pub fn augment_with_system_subtrees(
  engine: &crate::engine::StorageEngine,
  tree: &mut VersionTree,
) {
  use crate::engine::directory_ops::{directory_path_hash, file_path_hash};
  use crate::engine::file_record::FileRecord;

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();

  let system_dirs = [
    "/.aeordb-system/users",
    "/.aeordb-system/groups",
    "/.aeordb-system/snapshots",
    "/.aeordb-system/config",
    "/.aeordb-config",
  ];
  let system_single_files: &[&str] = &["/.aeordb-system/email-config.json"];

  for sys_path in &system_dirs {
    let key = match directory_path_hash(sys_path, &algo) {
      Ok(k) => k,
      Err(_) => continue,
    };
    let raw_value = match engine.get_entry_including_deleted(&key) {
      Ok(Some((_h, _k, value))) => value,
      _ => continue,
    };
    let sys_dir_hash = if raw_value.len() == hash_length {
      raw_value
    } else {
      match algo.compute_hash(&raw_value) {
        Ok(h) => h,
        Err(_) => continue,
      }
    };
    tree.directories.insert(sys_path.to_string(), (sys_dir_hash.clone(), Vec::new()));
    let _ = walk_subtree(engine, sys_path, &sys_dir_hash, tree);
  }

  for file_path in system_single_files {
    let key = match file_path_hash(file_path, &algo) {
      Ok(k) => k,
      Err(_) => continue,
    };
    let (record, content_hash) = match engine.get_entry_including_deleted(&key) {
      Ok(Some((header, _key, raw))) => match FileRecord::deserialize(&raw, hash_length, header.entry_version) {
        Ok(record) => {
          let serialized = match record.serialize(hash_length) { Ok(s) => s, Err(_) => continue };
          match algo.compute_hash(&serialized) { Ok(h) => (record, h), Err(_) => continue }
        }
        Err(_) => continue,
      },
      _ => continue,
    };
    tree.files.insert(file_path.to_string(), (content_hash, record));
  }
}

/// Recursively walk a directory and its children.
///
/// The `visited` set tracks directory hashes already traversed to prevent
/// infinite recursion on corrupted data that contains cycles.
fn walk_directory(
  engine: &StorageEngine,
  dir_hash: &[u8],
  current_path: &str,
  hash_length: usize,
  tree: &mut VersionTree,
  visited: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  // Cycle detection: if we've already visited this directory hash, bail out.
  if !visited.insert(dir_hash.to_vec()) {
    return Ok(());
  }

  // Load the directory entry from the engine.
  // Use get_entry_including_deleted() because version/snapshot trees may
  // reference entries that have been deleted at HEAD but still exist in
  // the snapshot being walked.
  let dir_data = match engine.get_entry_including_deleted(dir_hash)? {
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
    crate::engine::btree::btree_list_from_node(&dir_data, engine, hash_length, true)?
  } else {
    deserialize_child_entries(&dir_data, hash_length, 0)?
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
        walk_directory(engine, &child.hash, &child_path, hash_length, tree, visited)?;
      }
      EntryType::FileRecord => {
        // Load the file record using the hash stored in ChildEntry.
        // Must include deleted entries — see comment on get_entry_including_deleted above.
        if let Some((header, _key, value)) = engine.get_entry_including_deleted(&child.hash)? {
          let file_record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;

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
      EntryType::Symlink => {
        // Must include deleted entries — see comment on get_entry_including_deleted above.
        if let Some((header, _key, value)) = engine.get_entry_including_deleted(&child.hash)? {
          let symlink_record = SymlinkRecord::deserialize(&value, header.entry_version)?;
          tree.symlinks.insert(
            child_path.clone(),
            (child.hash.clone(), symlink_record),
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
  /// Symlinks added
  pub symlinks_added: HashMap<String, (Vec<u8>, SymlinkRecord)>,
  /// Symlinks modified (target changed)
  pub symlinks_modified: HashMap<String, (Vec<u8>, SymlinkRecord)>,
  /// Symlinks deleted
  pub symlinks_deleted: Vec<String>,
}

impl TreeDiff {
  pub fn is_empty(&self) -> bool {
    self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
      && self.symlinks_added.is_empty() && self.symlinks_modified.is_empty() && self.symlinks_deleted.is_empty()
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

  // Symlink diffs
  let mut symlinks_added = HashMap::new();
  let mut symlinks_modified = HashMap::new();
  let mut symlinks_deleted = Vec::new();

  for (path, (target_hash, target_record)) in &target.symlinks {
    match base.symlinks.get(path) {
      None => {
        symlinks_added.insert(path.clone(), (target_hash.clone(), target_record.clone()));
      }
      Some((_, base_record)) => {
        if base_record.target != target_record.target {
          symlinks_modified.insert(path.clone(), (target_hash.clone(), target_record.clone()));
        }
      }
    }
  }

  for path in base.symlinks.keys() {
    if !target.symlinks.contains_key(path) {
      symlinks_deleted.push(path.clone());
    }
  }

  TreeDiff {
    added,
    modified,
    deleted,
    new_chunks,
    changed_directories,
    symlinks_added,
    symlinks_modified,
    symlinks_deleted,
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
