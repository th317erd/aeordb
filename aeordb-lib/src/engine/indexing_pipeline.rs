use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::index_config::{PathIndexConfig, IndexFieldConfig, create_converter_from_config};
use crate::engine::index_store::IndexManager;
use crate::engine::path_utils::{normalize_path, parent_path};
use crate::engine::source_resolver::resolve_source;
use crate::engine::storage_engine::StorageEngine;

/// Manages the indexing pipeline for stored files.
/// Handles: config loading, JSON parsing, source path resolution, index updates.
/// Parser plugin invocation and plugin mapper will be added in later tasks.
pub struct IndexingPipeline<'a> {
  engine: &'a StorageEngine,
}

impl<'a> IndexingPipeline<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    IndexingPipeline { engine }
  }

  /// Run the indexing pipeline for a stored file.
  pub fn run(
    &self,
    path: &str,
    data: &[u8],
    _content_type: Option<&str>,
  ) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());

    let config = match self.load_config(&parent)? {
      Some(c) => c,
      None => return Ok(()),
    };

    // Get JSON data (for now, parse raw data as JSON — parser plugins added later)
    let json_data = match self.parse_json(data) {
      Ok(json) => json,
      Err(e) => {
        if config.logging {
          self.log_system(&parent, "parsing.log",
            &format!("JSON parse failed for {}: {}", path, e));
        }
        return Ok(()); // Don't fail the store
      }
    };

    let algo = self.engine.hash_algo();
    let file_key = crate::engine::directory_ops::file_path_hash(&normalized, &algo)?;
    let index_manager = IndexManager::new(self.engine);

    for field_config in &config.indexes {
      if let Err(e) = self.index_field(field_config, &json_data, &file_key, &parent, &index_manager) {
        if config.logging {
          self.log_system(&parent, "indexing.log",
            &format!("field '{}' indexing failed for {}: {}", field_config.name, path, e));
        }
      }
    }

    Ok(())
  }

  fn load_config(&self, parent: &str) -> EngineResult<Option<PathIndexConfig>> {
    let config_path = if parent.ends_with('/') {
      format!("{}.config/indexes.json", parent)
    } else {
      format!("{}/.config/indexes.json", parent)
    };

    let ops = DirectoryOps::new(self.engine);
    match ops.read_file(&config_path) {
      Ok(config_data) => PathIndexConfig::deserialize(&config_data).map(Some),
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(e) => Err(e),
    }
  }

  fn parse_json(&self, data: &[u8]) -> EngineResult<serde_json::Value> {
    let text = std::str::from_utf8(data).map_err(|e| {
      EngineError::JsonParseError(format!("Invalid UTF-8: {}", e))
    })?;
    serde_json::from_str(text).map_err(|e| {
      EngineError::JsonParseError(format!("Invalid JSON: {}", e))
    })
  }

  fn index_field(
    &self,
    field_config: &IndexFieldConfig,
    json_data: &serde_json::Value,
    file_key: &[u8],
    parent: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<()> {
    // Resolve source path
    let source_segments = field_config.source.as_ref()
      .and_then(|v| v.as_array())
      .cloned()
      .unwrap_or_else(|| vec![serde_json::Value::String(field_config.name.clone())]);

    let field_value = match resolve_source(json_data, &source_segments) {
      Some(bytes) => bytes,
      None => return Ok(()), // source not found, skip
    };

    // Load or create index
    let converter = create_converter_from_config(field_config)?;
    let strategy = converter.strategy().to_string();
    let mut index = match index_manager.load_index_by_strategy(parent, &field_config.name, &strategy)? {
      Some(idx) => idx,
      None => index_manager.create_index(parent, &field_config.name, converter)?,
    };

    index.remove(file_key);
    index.insert_expanded(&field_value, file_key.to_vec());
    index_manager.save_index(parent, &index)?;

    Ok(())
  }

  /// Write a log entry to .logs/system/{log_name}
  fn log_system(&self, parent: &str, log_name: &str, message: &str) {
    let log_path = if parent.ends_with('/') {
      format!("{}.logs/system/{}", parent, log_name)
    } else {
      format!("{}/.logs/system/{}", parent, log_name)
    };

    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let entry = format!("{} WARN  {}\n", timestamp, message);

    let ops = DirectoryOps::new(self.engine);
    let existing = ops.read_file(&log_path).unwrap_or_default();
    let mut combined = existing;
    combined.extend_from_slice(entry.as_bytes());

    let _ = ops.store_file(&log_path, &combined, Some("text/plain"));
  }
}
