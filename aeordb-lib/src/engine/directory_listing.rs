use crate::engine::directory_entry::deserialize_child_entries;
use crate::engine::directory_ops::{DirectoryOps, directory_path_hash};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::StorageEngine;

/// Live tree metrics reachable from HEAD's root.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveTreeMetrics {
  pub files: u64,
  pub directories: u64,
  pub logical_data_size: u64,
}

/// Count live files + directories reachable from HEAD's root.
///
/// "Live" means present in the current tree — i.e., reachable by walking
/// child entries from the root directory's hash. Excludes superseded
/// revisions still pinned by snapshots/forks; those are visible via
/// `KvSnapshot::count_by_type(KV_TYPE_FILE_RECORD)` instead.
///
/// The implicit root directory is NOT counted: an empty database returns
/// `(0, 0)`. This matches the runtime tracking in `DirectoryOps`, which
/// only `increment_directories()` on explicit user-created directories.
///
/// Returns `(files, directories)`.
pub fn count_live_tree(engine: &StorageEngine) -> EngineResult<(u64, u64)> {
  let metrics = measure_live_tree(engine)?;
  Ok((metrics.files, metrics.directories))
}

/// Measure live files, directories, and logical byte size reachable from HEAD.
///
/// This walks directory entries only. File sizes come from `ChildEntry::total_size`,
/// so it does not read file payload chunks and is safe to run during startup.
pub fn measure_live_tree(engine: &StorageEngine) -> EngineResult<LiveTreeMetrics> {
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let root_key = directory_path_hash("/", &algo)?;
  let ops = crate::engine::directory_ops::DirectoryOps::new(engine);
  let root_value = match ops.read_directory_data(&root_key)? {
    Some((_header, value)) => value,
    None => return Ok(LiveTreeMetrics::default()),
  };
  let mut metrics = LiveTreeMetrics::default();
  if !root_value.is_empty() {
    measure_walk(engine, &root_value, hash_length, &mut metrics)?;
  }
  Ok(metrics)
}

fn measure_walk(engine: &StorageEngine, dir_value: &[u8], hash_length: usize, metrics: &mut LiveTreeMetrics) -> EngineResult<()> {
  let children = if crate::engine::btree::is_btree_format(dir_value) {
    crate::engine::btree::btree_list_from_node(dir_value, engine, hash_length, false)?
  } else {
    deserialize_child_entries(dir_value, hash_length, 0)?
  };
  for child in &children {
    let entry_type = EntryType::from_u8(child.entry_type)?;
    match entry_type {
      EntryType::FileRecord => {
        metrics.files += 1;
        metrics.logical_data_size = metrics.logical_data_size.saturating_add(child.total_size);
      }
      EntryType::DirectoryIndex => {
        metrics.directories += 1;
        if let Some((_header, _key, sub_value)) = engine.get_entry(&child.hash)? {
          if !sub_value.is_empty() {
            measure_walk(engine, &sub_value, hash_length, metrics)?;
          }
        }
      }
      // Symlinks and other types don't contribute to file/dir counts.
      _ => {}
    }
  }
  Ok(())
}

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
/// - `glob_pattern`: optional glob matched against file name, relative path, or full path
///
/// Returns files only (no directory entries) when recursing (depth > 0 or depth == -1).
/// At depth=0, returns both files and directories (for backwards compat with existing listing).
pub fn list_directory_recursive(
  engine: &StorageEngine,
  base_path: &str,
  depth: i32,
  glob_pattern: Option<&str>,
  max_results: Option<usize>,
) -> EngineResult<Vec<ListingEntry>> {
  let normalized = normalize_path(base_path);
  let ops = DirectoryOps::new(engine);
  let children = ops.list_directory(&normalized)?;

  // recursive_mode: when depth > 0 or depth == -1, we only return files
  let recursive_mode = depth != 0;

  let mut results = Vec::new();
  let ctx = WalkContext { engine, base_path: normalized.as_str(), recursive_mode, glob_pattern, max_results };
  walk_listing(&ctx, &children, &normalized, depth, &mut results)?;

  Ok(results)
}

/// Walk-invariant arguments shared across every recursive `walk_listing`
/// call. Carrying these in a context struct lets the per-level args
/// (children, current_path, remaining_depth) stay short and obvious.
struct WalkContext<'a> {
  engine: &'a StorageEngine,
  base_path: &'a str,
  recursive_mode: bool,
  glob_pattern: Option<&'a str>,
  max_results: Option<usize>,
}

fn walk_listing(
  ctx: &WalkContext<'_>,
  children: &[crate::engine::directory_entry::ChildEntry],
  current_path: &str,
  remaining_depth: i32,
  results: &mut Vec<ListingEntry>,
) -> EngineResult<()> {
  let engine = ctx.engine;
  let recursive_mode = ctx.recursive_mode;
  let glob_pattern = ctx.glob_pattern;
  let max_results = ctx.max_results;
  for child in children {
    // Early-exit when the result cap has been reached
    if let Some(cap) = max_results {
      if results.len() >= cap {
        return Ok(());
      }
    }
    let child_path = if current_path == "/" { format!("/{}", child.name) } else { format!("{}/{}", current_path, child.name) };

    let entry_type = EntryType::from_u8(child.entry_type)?;

    match entry_type {
      EntryType::FileRecord => {
        if let Some(pattern) = glob_pattern {
          if !listing_glob_matches(pattern, ctx.base_path, &child_path, &child.name) {
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
            if !listing_glob_matches(pattern, ctx.base_path, &child_path, &child.name) {
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
          let ops = DirectoryOps::new(engine);
          let sub_children = ops.list_directory(&child_path)?;
          let next_depth = if remaining_depth == -1 { -1 } else { remaining_depth - 1 };

          walk_listing(ctx, &sub_children, &child_path, next_depth, results)?;
        }
        // remaining_depth == 0 in recursive mode: don't include dir, don't recurse
      }
      EntryType::Symlink => {
        if let Some(pattern) = glob_pattern {
          if !listing_glob_matches(pattern, ctx.base_path, &child_path, &child.name) {
            continue;
          }
        }

        let target = DirectoryOps::new(engine).get_symlink(&child_path).ok().flatten().map(|record| record.target);

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

fn listing_glob_matches(pattern: &str, base_path: &str, child_path: &str, child_name: &str) -> bool {
  if glob_match::glob_match(pattern, child_name) {
    return true;
  }

  let relative = relative_listing_path(base_path, child_path);
  crate::engine::indexing_pipeline::glob_matches(pattern, &relative) || crate::engine::indexing_pipeline::glob_matches(pattern, child_path)
}

fn relative_listing_path(base_path: &str, child_path: &str) -> String {
  if base_path == "/" {
    return child_path.trim_start_matches('/').to_string();
  }

  child_path.strip_prefix(base_path.trim_end_matches('/')).unwrap_or(child_path).trim_start_matches('/').to_string()
}
