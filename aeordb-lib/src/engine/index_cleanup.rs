//! Legacy field-index cleanup resolved from the file's effective index scope.
//!
//! Namespace mutation fanout invokes this after the delete acknowledgement.
//! The same fanout admits root-pinned v4 cleanup intent without a detached
//! queue or a second worker.

use crate::engine::errors::EngineResult;
use crate::engine::index_config_resolver::IndexConfigResolver;
use crate::engine::index_store::IndexManager;
use crate::engine::path_utils::{normalize_path, parent_path};
use crate::engine::storage_engine::StorageEngine;

#[derive(Debug, Clone)]
pub(crate) struct IndexRemovalTarget {
  pub parent: String,
  pub index_names: Vec<String>,
}

pub(crate) fn resolve_index_removal_targets(engine: &StorageEngine, path: &str) -> EngineResult<Vec<IndexRemovalTarget>> {
  let normalized = normalize_path(path);
  let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());
  let index_manager = IndexManager::new(engine);
  let mut targets = Vec::new();

  let parent_index_names = index_manager.list_indexes(&parent)?;
  if !parent_index_names.is_empty() {
    targets.push(IndexRemovalTarget { parent: parent.clone(), index_names: parent_index_names });
  }

  if let Some((_config, config_dir)) = IndexConfigResolver::new(engine).find_config_for_path(&normalized)? {
    if config_dir != parent {
      let ancestor_index_names = index_manager.list_indexes(&config_dir)?;
      if !ancestor_index_names.is_empty() {
        targets.push(IndexRemovalTarget { parent: config_dir, index_names: ancestor_index_names });
      }
    }
  }

  Ok(targets)
}

pub(crate) fn remove_file_from_resolved_indexes(engine: &StorageEngine, path: &str) -> EngineResult<usize> {
  let normalized = normalize_path(path);
  let algo = engine.hash_algo();
  let file_key = crate::engine::directory_ops::file_path_hash(&normalized, &algo)?;
  let index_manager = IndexManager::new(engine);
  let targets = resolve_index_removal_targets(engine, &normalized)?;
  let mut removed_indexes = 0usize;

  for target in targets {
    for field_name in target.index_names {
      index_manager.remove_file_from_index_name_unrouted(&target.parent, &field_name, &file_key)?;
      removed_indexes += 1;
    }
  }

  Ok(removed_indexes)
}
