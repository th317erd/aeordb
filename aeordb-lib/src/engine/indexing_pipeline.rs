use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use crate::engine::index_config_resolver::IndexConfigResolver;
use crate::engine::index_store::{IndexManager, IndexWriteBuffer};
use crate::engine::path_utils::normalize_path;
use crate::engine::request_context::RequestContext;
use crate::engine::source_resolver::resolve_sources;
use crate::engine::storage_engine::StorageEngine;
use crate::plugins::PluginManager;

pub use crate::engine::index_config_resolver::glob_matches;

fn canonical_metadata_field_name(field_name: &str) -> Option<&'static str> {
  match field_name {
    "@path" => Some("@path"),
    "@filename" | "@file_name" => Some("@filename"),
    "@extension" => Some("@extension"),
    "@content_type" => Some("@content_type"),
    "@size" => Some("@size"),
    "@created_at" => Some("@created_at"),
    "@updated_at" => Some("@updated_at"),
    "@hash" => Some("@hash"),
    _ => None,
  }
}

/// Manages the indexing pipeline for stored files.
/// Handles: config loading, JSON parsing, parser plugin invocation,
/// content-type registry fallback, plugin mapper sources, and index updates.
pub struct IndexingPipeline<'a> {
  engine: &'a StorageEngine,
  plugin_manager: Option<&'a PluginManager>,
}

trait IndexSink {
  fn update_index(
    &mut self,
    parent: &str,
    field_name: &str,
    field_config: &IndexFieldConfig,
    values: &[Vec<u8>],
    file_key: &[u8],
  ) -> EngineResult<()>;
}

impl IndexSink for IndexManager<'_> {
  fn update_index(
    &mut self,
    parent: &str,
    field_name: &str,
    field_config: &IndexFieldConfig,
    values: &[Vec<u8>],
    file_key: &[u8],
  ) -> EngineResult<()> {
    IndexManager::update_index(self, parent, field_name, field_config, values, file_key)
  }
}

impl IndexSink for IndexWriteBuffer<'_> {
  fn update_index(
    &mut self,
    parent: &str,
    field_name: &str,
    field_config: &IndexFieldConfig,
    values: &[Vec<u8>],
    file_key: &[u8],
  ) -> EngineResult<()> {
    IndexWriteBuffer::update_index(self, parent, field_name, field_config, values, file_key)
  }
}

impl<'a> IndexingPipeline<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    IndexingPipeline { engine, plugin_manager: None }
  }

  pub fn with_plugin_manager(engine: &'a StorageEngine, plugin_manager: &'a PluginManager) -> Self {
    IndexingPipeline { engine, plugin_manager: Some(plugin_manager) }
  }

  /// Run the indexing pipeline for a stored file.
  pub fn run(&self, _ctx: &RequestContext, path: &str, data: &[u8], content_type: Option<&str>) -> EngineResult<()> {
    let mut index_manager = IndexManager::new(self.engine);
    self.run_with_sink(path, data, content_type, &mut index_manager)
  }

  /// Run the full indexing pipeline using a buffered writer for index updates.
  ///
  /// This is intended for bulk reindexing. It preserves parser/content indexing
  /// semantics while avoiding a full index-file rewrite for every file/field
  /// mutation.
  pub fn run_buffered(
    &self,
    _ctx: &RequestContext,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    index_buffer: &mut IndexWriteBuffer<'_>,
  ) -> EngineResult<()> {
    self.run_with_sink(path, data, content_type, index_buffer)
  }

  fn run_with_sink<S: IndexSink>(&self, path: &str, data: &[u8], content_type: Option<&str>, index_sink: &mut S) -> EngineResult<()> {
    if crate::engine::directory_ops::is_internal_path(path) {
      return Ok(());
    }

    let normalized = normalize_path(path);
    let (config, config_dir) = match self.find_config_for_path(&normalized)? {
      Some(pair) => pair,
      None => return Ok(()),
    };

    let ct = content_type.unwrap_or("application/octet-stream");
    let filename = crate::engine::path_utils::file_name(path).unwrap_or_default();
    let algo = self.engine.hash_algo();
    let file_key = crate::engine::directory_ops::file_path_hash(&normalized, &algo)?;

    self.index_metadata_fields(&config, &config_dir, path, &file_key, index_sink)?;

    let explicit_parser = config.parser.clone();
    let registry_parser = self.lookup_parser_by_content_type(content_type);
    let content_fields: Vec<&IndexFieldConfig> = config.indexes.iter().filter(|field| !field.name.starts_with('@')).collect();
    if content_fields.is_empty() && explicit_parser.is_none() && registry_parser.is_none() {
      return Ok(());
    }

    let Some(json_data) =
      self.parse_index_document(&config, &config_dir, path, data, content_type, explicit_parser, registry_parser, ct, filename)
    else {
      return Ok(());
    };

    for field_config in content_fields {
      if let Err(e) = self.index_field(field_config, &json_data, &file_key, &config_dir, index_sink) {
        if config.logging {
          self.log_system(&config_dir, "indexing.log", &format!("field '{}' indexing failed for {}: {}", field_config.name, path, e));
        }
      }
    }

    Ok(())
  }

  fn parse_index_document(
    &self,
    config: &PathIndexConfig,
    config_dir: &str,
    path: &str,
    data: &[u8],
    content_type: Option<&str>,
    explicit_parser: Option<String>,
    registry_parser: Option<String>,
    content_type_fallback: &str,
    filename: &str,
  ) -> Option<serde_json::Value> {
    // Priority order for the indexing pipeline:
    // 1. Explicit parser in config — always honored (user's intent)
    // 2. Content-type registry parser — user mapped this content type
    // 3. Raw JSON parse — preserves actual field structure for indexing
    // 4. Native parser — extracts metadata for non-JSON content.
    if let Some(ref parser) = explicit_parser {
      match self.invoke_parser(parser, data, path, content_type, &config) {
        Ok(json) => Some(json),
        Err(e) => {
          if config.logging {
            self.log_system(config_dir, "parsing.log", &format!("parser '{}' failed for {}: {}", parser, path, e));
          }
          None
        }
      }
    } else if let Some(ref parser) = registry_parser {
      match self.invoke_parser(parser, data, path, content_type, &config) {
        Ok(json) => Some(json),
        Err(e) => {
          if config.logging {
            self.log_system(config_dir, "parsing.log", &format!("parser '{}' failed for {}: {}", parser, path, e));
          }
          None
        }
      }
    } else if let Ok(json) = self.parse_json(data) {
      Some(json)
    } else {
      let native_result = crate::engine::native_parsers::parse_native(data, content_type_fallback, filename, path, data.len() as u64);

      if let Some(result) = native_result {
        match result {
          Ok(json) => Some(json),
          Err(e) => {
            if config.logging {
              self.log_system(config_dir, "parsing.log", &format!("native parser failed for {}: {}", path, e));
            }
            None
          }
        }
      } else {
        if config.logging {
          self.log_system(config_dir, "parsing.log", &format!("no parser available for {}", path));
        }
        None
      }
    }
  }

  /// Update only @-prefixed metadata indexes for a stored file.
  ///
  /// This path deliberately does not parse or read the file body. It is used
  /// by chunk/batch commit paths that have already stored a FileRecord but do
  /// not have the full file bytes in memory.
  pub fn run_metadata_only(&self, _ctx: &RequestContext, path: &str) -> EngineResult<()> {
    let mut index_manager = IndexManager::new(self.engine);
    self.run_metadata_only_with_sink(path, &mut index_manager)
  }

  /// Update only @-prefixed metadata indexes using a buffered writer.
  pub fn run_metadata_only_buffered(&self, _ctx: &RequestContext, path: &str, index_buffer: &mut IndexWriteBuffer<'_>) -> EngineResult<()> {
    self.run_metadata_only_with_sink(path, index_buffer)
  }

  fn run_metadata_only_with_sink<S: IndexSink>(&self, path: &str, index_sink: &mut S) -> EngineResult<()> {
    if crate::engine::directory_ops::is_internal_path(path) {
      return Ok(());
    }

    let normalized = normalize_path(path);
    let (config, config_dir) = match self.find_config_for_path(&normalized)? {
      Some(pair) => pair,
      None => return Ok(()),
    };

    let algo = self.engine.hash_algo();
    let file_key = crate::engine::directory_ops::file_path_hash(&normalized, &algo)?;
    self.index_metadata_fields(&config, &config_dir, path, &file_key, index_sink)
  }

  fn index_metadata_fields<S: IndexSink>(
    &self,
    config: &PathIndexConfig,
    config_dir: &str,
    path: &str,
    file_key: &[u8],
    index_sink: &mut S,
  ) -> EngineResult<()> {
    for field_config in config.indexes.iter().filter(|field| field.name.starts_with('@')) {
      if let Err(e) = self.index_meta_field(field_config, file_key, config_dir, index_sink) {
        if config.logging {
          self.log_system(config_dir, "indexing.log", &format!("field '{}' indexing failed for {}: {}", field_config.name, path, e));
        }
      }
    }
    Ok(())
  }

  /// Find an index config for the given normalized file path.
  ///
  /// Discovery order:
  /// 1. Check the file's immediate parent for a config.
  ///    - If found and it has no glob, use it (existing behavior).
  ///    - If found and it has a glob, test it against the filename.
  /// 2. Walk up ancestor directories. At each ancestor, load config.
  ///    If config has a glob, test the file's path relative to that ancestor.
  ///
  /// Returns `Some((config, config_owner_directory))` or `None`.
  pub fn find_config_for_path(&self, normalized_path: &str) -> EngineResult<Option<(PathIndexConfig, String)>> {
    IndexConfigResolver::new(self.engine).find_config_for_path(normalized_path)
  }

  fn parse_json(&self, data: &[u8]) -> EngineResult<serde_json::Value> {
    let text = std::str::from_utf8(data).map_err(|e| EngineError::JsonParseError(format!("Invalid UTF-8: {}", e)))?;
    serde_json::from_str(text).map_err(|e| EngineError::JsonParseError(format!("Invalid JSON: {}", e)))
  }

  fn index_field<S: IndexSink>(
    &self,
    field_config: &IndexFieldConfig,
    json_data: &serde_json::Value,
    file_key: &[u8],
    parent: &str,
    index_sink: &mut S,
  ) -> EngineResult<()> {
    // @-prefixed fields: extract values from FileRecord metadata instead of JSON content.
    if field_config.name.starts_with('@') {
      return self.index_meta_field(field_config, file_key, parent, index_sink);
    }

    let Some(field_values) = self.extract_field_values(field_config, json_data)? else {
      return Ok(());
    };

    index_sink.update_index(parent, &field_config.name, field_config, &field_values, file_key)
  }

  fn extract_field_values(&self, field_config: &IndexFieldConfig, json_data: &serde_json::Value) -> EngineResult<Option<Vec<Vec<u8>>>> {
    let field_values = if let Some(source) = &field_config.source {
      if let Some(obj) = source.as_object() {
        // Plugin mapper: {"plugin": "name", "args": {...}}
        if let Some(plugin_name) = obj.get("plugin").and_then(|v| v.as_str()) {
          let args = obj.get("args").cloned().unwrap_or(serde_json::Value::Null);
          vec![self.invoke_mapper(plugin_name, json_data, &args)?]
        } else {
          return Ok(None); // invalid source object
        }
      } else if let Some(segments) = source.as_array() {
        // Array path resolution — fan-out on wildcards/regex
        let values = resolve_sources(json_data, segments);
        if values.is_empty() {
          return Ok(None);
        }
        values
      } else {
        return Ok(None); // invalid source type
      }
    } else {
      // Default: use field name as key
      let default_source = vec![serde_json::Value::String(field_config.name.clone())];
      let values = resolve_sources(json_data, &default_source);
      if values.is_empty() {
        return Ok(None);
      }
      values
    };

    Ok(Some(field_values))
  }

  /// Index a @-prefixed field by extracting the value from the FileRecord metadata
  /// rather than parsing the file's JSON content.
  fn index_meta_field<S: IndexSink>(
    &self,
    field_config: &IndexFieldConfig,
    file_key: &[u8],
    parent: &str,
    index_sink: &mut S,
  ) -> EngineResult<()> {
    // Load the FileRecord from the storage engine.
    let entry = match self.engine.get_entry(file_key)? {
      Some(entry) => entry,
      None => return Ok(()), // file not yet stored — nothing to index
    };
    let (header, _key, value) = entry;
    let hash_length = self.engine.hash_algo().hash_length();
    let record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;

    // Extract the value based on the @-field name. Aliases are indexed under
    // the canonical physical field name so query planning has one lookup path.
    let Some((index_field_name, extracted)) = Self::extract_metadata_field_value(field_config, &record) else {
      return Ok(()); // unknown @-field -- skip silently
    };

    index_sink.update_index(parent, index_field_name, field_config, &[extracted], file_key)
  }

  fn extract_metadata_field_value(field_config: &IndexFieldConfig, record: &FileRecord) -> Option<(&'static str, Vec<u8>)> {
    let Some(index_field_name) = canonical_metadata_field_name(&field_config.name) else {
      return None;
    };

    let extracted: Vec<u8> = match index_field_name {
      "@path" => record.path.as_bytes().to_vec(),
      "@filename" => crate::engine::path_utils::file_name(&record.path).unwrap_or_default().as_bytes().to_vec(),
      "@extension" => {
        let filename = crate::engine::path_utils::file_name(&record.path).unwrap_or_default();
        let extension = filename.rsplit('.').next().unwrap_or("");
        let extension = if extension == filename { "" } else { extension };
        extension.as_bytes().to_vec()
      }
      "@hash" => record.content_hash_hex().into_bytes(),
      "@created_at" => record.created_at.to_be_bytes().to_vec(),
      "@updated_at" => record.updated_at.to_be_bytes().to_vec(),
      "@size" => record.total_size.to_be_bytes().to_vec(),
      "@content_type" => record.content_type.as_deref().unwrap_or("").as_bytes().to_vec(),
      _ => return None,
    };

    Some((index_field_name, extracted))
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
    let pm = self.plugin_manager.ok_or_else(|| EngineError::NotFound("Plugin manager required for parser invocation".to_string()))?;

    let memory_limit = config.parser_memory_limit.as_deref().map(Self::parse_memory_limit).unwrap_or(256 * 1024 * 1024); // 256MB default

    let envelope = Self::build_parser_envelope(data, path, content_type);
    let envelope_bytes =
      serde_json::to_vec(&envelope).map_err(|e| EngineError::JsonParseError(format!("Failed to serialize parser envelope: {}", e)))?;

    let output = pm
      .invoke_wasm_plugin_with_limits(parser_name, &envelope_bytes, memory_limit)
      .map_err(|e| EngineError::NotFound(format!("Parser '{}' failed: {}", parser_name, e)))?;

    // Validate output is JSON object
    let text = std::str::from_utf8(&output).map_err(|_| EngineError::JsonParseError("Parser returned invalid UTF-8".to_string()))?;
    let parsed: serde_json::Value =
      serde_json::from_str(text).map_err(|e| EngineError::JsonParseError(format!("Parser returned invalid JSON: {}", e)))?;
    if !parsed.is_object() {
      return Err(EngineError::JsonParseError("Parser must return a JSON object".to_string()));
    }
    Ok(parsed)
  }

  /// Invoke a mapper plugin to extract field values from JSON data.
  fn invoke_mapper(&self, plugin_name: &str, json_data: &serde_json::Value, args: &serde_json::Value) -> EngineResult<Vec<u8>> {
    let pm = self.plugin_manager.ok_or_else(|| EngineError::NotFound("Plugin manager required for mapper".to_string()))?;

    let mapper_input = serde_json::json!({
      "data": json_data,
      "args": args,
    });
    let input_bytes =
      serde_json::to_vec(&mapper_input).map_err(|e| EngineError::JsonParseError(format!("Mapper input serialization failed: {}", e)))?;

    pm.invoke_wasm_plugin(plugin_name, &input_bytes).map_err(|e| EngineError::NotFound(format!("Mapper '{}' failed: {}", plugin_name, e)))
  }

  /// Look up a parser name from the global registry at /.aeordb-config/parsers.json
  fn lookup_parser_by_content_type(&self, content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    // Don't look up JSON — it's handled natively
    if ct == "application/json" {
      return None;
    }

    let ops = DirectoryOps::new(self.engine);
    match ops.read_file_buffered("/.aeordb-config/parsers.json") {
      Ok(data) => {
        let text = std::str::from_utf8(&data).ok()?;
        let registry: serde_json::Value = serde_json::from_str(text).ok()?;
        registry.get(ct).and_then(|v| v.as_str()).map(String::from)
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
  fn build_parser_envelope(data: &[u8], path: &str, content_type: Option<&str>) -> serde_json::Value {
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

  /// Write a log entry to .aeordb-logs/system/{log_name}
  fn log_system(&self, parent: &str, log_name: &str, message: &str) {
    let log_path = if parent.ends_with('/') {
      format!("{}.aeordb-logs/system/{}", parent, log_name)
    } else {
      format!("{}/.aeordb-logs/system/{}", parent, log_name)
    };

    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let entry = format!("{} WARN  {}\n", timestamp, message);

    let ops = DirectoryOps::new(self.engine);
    let existing = ops.read_file_buffered(&log_path).unwrap_or_default();
    let mut combined = existing;
    combined.extend_from_slice(entry.as_bytes());

    let ctx = RequestContext::system();
    let _ = ops.store_file_buffered(&ctx, &log_path, &combined, Some("text/plain"));
  }
}
