use crate::engine::compression::{should_compress, CompressionAlgorithm};
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::EngineResult;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::engine::storage_engine::StorageEngine;

/// Simple glob matching for index config path patterns.
///
/// Supported wildcards:
///   - `*`  matches exactly one path segment (anything between slashes)
///   - `**` matches zero or more path segments (any depth)
///   - `?`  matches a single character within a segment
///
/// Both `pattern` and `path` are split by `/` and matched segment by segment.
pub use crate::engine::path_utils::glob_matches;

/// Resolves `.aeordb-config/indexes.json` ownership and derived policies.
pub struct IndexConfigResolver<'a> {
  engine: &'a StorageEngine,
}

impl<'a> IndexConfigResolver<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    IndexConfigResolver { engine }
  }

  pub fn config_path_for_directory(parent: &str) -> String {
    let normalized_parent = normalize_path(parent);
    if normalized_parent == "/" {
      "/.aeordb-config/indexes.json".to_string()
    } else {
      format!("{}/.aeordb-config/indexes.json", normalized_parent)
    }
  }

  pub fn load_config(&self, parent: &str) -> EngineResult<Option<PathIndexConfig>> {
    let normalized_parent = normalize_path(parent);
    self.engine.index_config_cache.get(&normalized_parent, self.engine)
  }

  /// Find the config that applies to a normalized file path.
  pub fn find_config_for_path(&self, normalized_path: &str) -> EngineResult<Option<(PathIndexConfig, String)>> {
    let immediate_parent = parent_path(normalized_path).unwrap_or_else(|| "/".to_string());

    if let Some(config) = self.load_config(&immediate_parent)? {
      if config.glob.is_none() {
        return Ok(Some((config, immediate_parent)));
      }

      let filename = file_name(normalized_path).unwrap_or_default();
      if glob_matches(config.glob.as_deref().unwrap_or(""), filename) {
        return Ok(Some((config, immediate_parent)));
      }
    }

    let mut ancestor = parent_path(&immediate_parent);
    while let Some(ref dir) = ancestor {
      if let Some(config) = self.load_config(dir)? {
        if let Some(ref glob_pattern) = config.glob {
          let prefix = if dir == "/" { "/".to_string() } else { format!("{}/", dir) };
          if let Some(relative) = normalized_path.strip_prefix(&prefix) {
            if glob_matches(glob_pattern, relative) {
              return Ok(Some((config, dir.clone())));
            }
          }
        }
      }

      if dir == "/" {
        break;
      }
      ancestor = parent_path(dir);
    }

    Ok(None)
  }

  /// Find the config that governs a directory-scoped reindex.
  ///
  /// A config stored directly in the requested directory always owns that
  /// scope. Otherwise, only ancestor glob configs can govern descendants,
  /// matching the ownership rules used by `find_config_for_path`.
  pub fn find_config_for_reindex_scope(&self, directory_path: &str) -> EngineResult<Option<(PathIndexConfig, String)>> {
    let scope = normalize_path(directory_path);
    if let Some(config) = self.load_config(&scope)? {
      return Ok(Some((config, scope)));
    }

    let mut ancestor = parent_path(&scope);
    while let Some(ref dir) = ancestor {
      if let Some(config) = self.load_config(dir)? {
        if config.glob.is_some() {
          return Ok(Some((config, dir.clone())));
        }
      }

      if dir == "/" {
        break;
      }
      ancestor = parent_path(dir);
    }

    Ok(None)
  }

  pub fn compression_for_path(&self, path: &str, content_type: Option<&str>, data_length: usize) -> CompressionAlgorithm {
    let normalized = normalize_path(path);
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());
    let config_path = Self::config_path_for_directory(&parent);
    let ops = DirectoryOps::new(self.engine);

    match ops.read_file_buffered(&config_path) {
      Ok(config_data) => match PathIndexConfig::deserialize_with_compression(&config_data) {
        Ok(Some(algo_str)) if algo_str == "zstd" && should_compress(content_type, data_length) => CompressionAlgorithm::Zstd,
        _ => CompressionAlgorithm::None,
      },
      Err(_) => CompressionAlgorithm::None,
    }
  }
}
