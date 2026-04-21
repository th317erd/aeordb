use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::index_config::{PathIndexConfig, IndexFieldConfig, create_converter_from_config};
use crate::engine::index_store::IndexManager;
use crate::engine::path_utils::{normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::source_resolver::resolve_source;
use crate::engine::storage_engine::StorageEngine;
use crate::plugins::PluginManager;

/// Manages the indexing pipeline for stored files.
/// Handles: config loading, JSON parsing, parser plugin invocation,
/// content-type registry fallback, plugin mapper sources, and index updates.
pub struct IndexingPipeline<'a> {
  engine: &'a StorageEngine,
  plugin_manager: Option<&'a PluginManager>,
}

impl<'a> IndexingPipeline<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    IndexingPipeline { engine, plugin_manager: None }
  }

  pub fn with_plugin_manager(engine: &'a StorageEngine, plugin_manager: &'a PluginManager) -> Self {
    IndexingPipeline { engine, plugin_manager: Some(plugin_manager) }
  }

  /// Run the indexing pipeline for a stored file.
  pub fn run(
    &self,
    _ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
  ) -> EngineResult<()> {
    let normalized = normalize_path(path);
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());

    let config = match self.load_config(&parent)? {
      Some(c) => c,
      None => return Ok(()),
    };

    let ct = content_type.unwrap_or("application/octet-stream");
    let filename = crate::engine::path_utils::file_name(path).unwrap_or_default();

    // Determine parser names from config and content-type registry
    let explicit_parser = config.parser.clone();
    let registry_parser = self.lookup_parser_by_content_type(content_type);

    // Priority order for the indexing pipeline:
    // 1. Explicit parser in config — always honored (user's intent)
    // 2. Content-type registry parser — user mapped this content type
    // 3. Raw JSON parse — preserves actual field structure for indexing
    // 4. Native parser — extracts metadata for non-JSON content (images, audio, etc.)
    //
    // Note: native parsers wrap data in metadata (text, title, metadata fields),
    // which is great for search but loses the original field structure needed for
    // field-level indexing. Raw JSON parsing must come before native parsers so
    // that JSON data (even with non-JSON content types) gets indexed correctly.
    let json_data = if let Some(ref parser) = explicit_parser {
      // Config specifies an explicit parser — always use it
      match self.invoke_parser(parser, data, path, content_type, &config) {
        Ok(json) => json,
        Err(e) => {
          if config.logging {
            self.log_system(&parent, "parsing.log",
              &format!("parser '{}' failed for {}: {}", parser, path, e));
          }
          return Ok(());
        }
      }
    } else if let Some(ref parser) = registry_parser {
      // Content-type registry maps this content type to a specific parser
      match self.invoke_parser(parser, data, path, content_type, &config) {
        Ok(json) => json,
        Err(e) => {
          if config.logging {
            self.log_system(&parent, "parsing.log",
              &format!("parser '{}' failed for {}: {}", parser, path, e));
          }
          return Ok(());
        }
      }
    } else if let Ok(json) = self.parse_json(data) {
      // Data is valid JSON — use it directly to preserve field structure
      json
    } else {
      // Not JSON: try native parser for metadata extraction (images, audio, etc.)
      let native_result = crate::engine::native_parsers::parse_native(
        data, ct, &filename, path, data.len() as u64,
      );

      if let Some(result) = native_result {
        match result {
          Ok(json) => json,
          Err(e) => {
            if config.logging {
              self.log_system(&parent, "parsing.log",
                &format!("native parser failed for {}: {}", path, e));
            }
            return Ok(());
          }
        }
      } else {
        // No parser could handle this content — skip indexing
        if config.logging {
          self.log_system(&parent, "parsing.log",
            &format!("no parser available for {}", path));
        }
        return Ok(());
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
    // Resolve field value based on source type
    let field_value = if let Some(source) = &field_config.source {
      if let Some(obj) = source.as_object() {
        // Plugin mapper: {"plugin": "name", "args": {...}}
        if let Some(plugin_name) = obj.get("plugin").and_then(|v| v.as_str()) {
          let args = obj.get("args").cloned().unwrap_or(serde_json::Value::Null);
          self.invoke_mapper(plugin_name, json_data, &args)?
        } else {
          return Ok(()); // invalid source object
        }
      } else if let Some(segments) = source.as_array() {
        // Array path resolution
        match resolve_source(json_data, segments) {
          Some(bytes) => bytes,
          None => return Ok(()),
        }
      } else {
        return Ok(()); // invalid source type
      }
    } else {
      // Default: use field name as key
      let default_source = vec![serde_json::Value::String(field_config.name.clone())];
      match resolve_source(json_data, &default_source) {
        Some(bytes) => bytes,
        None => return Ok(()),
      }
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

  /// Invoke a parser plugin to transform file bytes into JSON.
  fn invoke_parser(
    &self,
    parser_name: &str,
    data: &[u8],
    path: &str,
    content_type: Option<&str>,
    config: &PathIndexConfig,
  ) -> EngineResult<serde_json::Value> {
    let pm = self.plugin_manager.ok_or_else(|| {
      EngineError::NotFound("Plugin manager required for parser invocation".to_string())
    })?;

    let memory_limit = config.parser_memory_limit.as_deref()
      .map(|s| Self::parse_memory_limit(s))
      .unwrap_or(256 * 1024 * 1024); // 256MB default

    let envelope = Self::build_parser_envelope(data, path, content_type);
    let envelope_bytes = serde_json::to_vec(&envelope).map_err(|e| {
      EngineError::JsonParseError(format!("Failed to serialize parser envelope: {}", e))
    })?;

    let output = pm.invoke_wasm_plugin_with_limits(parser_name, &envelope_bytes, memory_limit)
      .map_err(|e| EngineError::NotFound(format!("Parser '{}' failed: {}", parser_name, e)))?;

    // Validate output is JSON object
    let text = std::str::from_utf8(&output).map_err(|_| {
      EngineError::JsonParseError("Parser returned invalid UTF-8".to_string())
    })?;
    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| {
      EngineError::JsonParseError(format!("Parser returned invalid JSON: {}", e))
    })?;
    if !parsed.is_object() {
      return Err(EngineError::JsonParseError("Parser must return a JSON object".to_string()));
    }
    Ok(parsed)
  }

  /// Invoke a mapper plugin to extract field values from JSON data.
  fn invoke_mapper(
    &self,
    plugin_name: &str,
    json_data: &serde_json::Value,
    args: &serde_json::Value,
  ) -> EngineResult<Vec<u8>> {
    let pm = self.plugin_manager.ok_or_else(|| {
      EngineError::NotFound("Plugin manager required for mapper".to_string())
    })?;

    let mapper_input = serde_json::json!({
      "data": json_data,
      "args": args,
    });
    let input_bytes = serde_json::to_vec(&mapper_input).map_err(|e| {
      EngineError::JsonParseError(format!("Mapper input serialization failed: {}", e))
    })?;

    pm.invoke_wasm_plugin(plugin_name, &input_bytes)
      .map_err(|e| EngineError::NotFound(format!("Mapper '{}' failed: {}", plugin_name, e)))
  }

  /// Look up a parser name from the global registry at /.config/parsers.json
  fn lookup_parser_by_content_type(&self, content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    // Don't look up JSON — it's handled natively
    if ct == "application/json" {
      return None;
    }

    let ops = DirectoryOps::new(self.engine);
    match ops.read_file("/.config/parsers.json") {
      Ok(data) => {
        let text = std::str::from_utf8(&data).ok()?;
        let registry: serde_json::Value = serde_json::from_str(text).ok()?;
        registry.get(ct)
          .and_then(|v| v.as_str())
          .map(String::from)
      }
      Err(_) => None,
    }
  }

  /// Parse a memory limit string like "256mb", "1gb", "512kb", or raw bytes.
  fn parse_memory_limit(limit_str: &str) -> usize {
    let s = limit_str.trim().to_lowercase();
    if let Some(mb) = s.strip_suffix("mb") {
      mb.trim().parse::<usize>().unwrap_or(256) * 1024 * 1024
    } else if let Some(gb) = s.strip_suffix("gb") {
      gb.trim().parse::<usize>().unwrap_or(1) * 1024 * 1024 * 1024
    } else if let Some(kb) = s.strip_suffix("kb") {
      kb.trim().parse::<usize>().unwrap_or(256 * 1024) * 1024
    } else {
      s.parse::<usize>().unwrap_or(256 * 1024 * 1024)
    }
  }

  /// Build the parser envelope: base64-encoded data + metadata.
  fn build_parser_envelope(
    data: &[u8],
    path: &str,
    content_type: Option<&str>,
  ) -> serde_json::Value {
    use base64::Engine as _;
    let filename = crate::engine::path_utils::file_name(path).unwrap_or_default();
    serde_json::json!({
      "data": base64::engine::general_purpose::STANDARD.encode(data),
      "meta": {
        "filename": filename,
        "path": path,
        "content_type": content_type.unwrap_or("application/octet-stream"),
        "size": data.len(),
      }
    })
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

    let ctx = RequestContext::system();
    let _ = ops.store_file(&ctx, &log_path, &combined, Some("text/plain"));
  }
}
