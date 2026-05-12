use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::index_config::{PathIndexConfig, IndexFieldConfig, create_converter_from_config};
use crate::engine::index_store::IndexManager;
use crate::engine::path_utils::{normalize_path, parent_path};
use crate::engine::request_context::RequestContext;
use crate::engine::source_resolver::resolve_sources;
use crate::engine::storage_engine::StorageEngine;
use crate::plugins::PluginManager;

/// Simple glob matching for index config path patterns.
///
/// Supported wildcards:
///   - `*`  matches exactly one path segment (anything between slashes)
///   - `**` matches zero or more path segments (any depth)
///   - `?`  matches a single character within a segment
///
/// Both `pattern` and `path` are split by `/` and matched segment by segment.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
  let pat_segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
  let path_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
  glob_match_segments(&pat_segments, &path_segments)
}

fn glob_match_segments(pattern: &[&str], path: &[&str]) -> bool {
  if pattern.is_empty() {
    return path.is_empty();
  }

  if pattern[0] == "**" {
    // `**` can match zero or more path segments
    // Try consuming 0, 1, 2, ... path segments
    for skip in 0..=path.len() {
      if glob_match_segments(&pattern[1..], &path[skip..]) {
        return true;
      }
    }
    return false;
  }

  if path.is_empty() {
    return false;
  }

  // Match current segment with possible `*` and `?` wildcards
  if segment_matches(pattern[0], path[0]) {
    glob_match_segments(&pattern[1..], &path[1..])
  } else {
    false
  }
}

/// Match a single pattern segment against a single path segment.
/// `*` as a whole segment matches any single segment.
/// `?` matches exactly one character. `*` within a segment matches
/// zero or more characters (but not `/`).
fn segment_matches(pattern: &str, segment: &str) -> bool {
  // Whole-segment wildcard
  if pattern == "*" {
    return true;
  }
  char_glob_match(pattern.as_bytes(), segment.as_bytes())
}

/// Character-level glob match within a single segment.
/// Supports `*` (zero or more chars) and `?` (one char).
fn char_glob_match(pat: &[u8], seg: &[u8]) -> bool {
  let mut pi = 0;
  let mut si = 0;
  let mut star_pi: Option<usize> = None;
  let mut star_si: usize = 0;

  while si < seg.len() {
    if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == seg[si]) {
      pi += 1;
      si += 1;
    } else if pi < pat.len() && pat[pi] == b'*' {
      star_pi = Some(pi);
      star_si = si;
      pi += 1;
    } else if let Some(sp) = star_pi {
      pi = sp + 1;
      star_si += 1;
      si = star_si;
    } else {
      return false;
    }
  }

  while pi < pat.len() && pat[pi] == b'*' {
    pi += 1;
  }

  pi == pat.len()
}

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
    // Skip internal paths (.logs, .aeordb-indexes, .aeordb-config) — never index engine internals.
    if crate::engine::directory_ops::is_internal_path(path) {
      return Ok(());
    }

    let normalized = normalize_path(path);

    // Find config via ancestor discovery (checks parent first, then walks up
    // looking for glob-based configs that match this file's relative path).
    let (config, config_dir) = match self.find_config_for_path(&normalized)? {
      Some(pair) => pair,
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
            self.log_system(&config_dir, "parsing.log",
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
            self.log_system(&config_dir, "parsing.log",
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
        data, ct, filename, path, data.len() as u64,
      );

      if let Some(result) = native_result {
        match result {
          Ok(json) => json,
          Err(e) => {
            if config.logging {
              self.log_system(&config_dir, "parsing.log",
                &format!("native parser failed for {}: {}", path, e));
            }
            return Ok(());
          }
        }
      } else {
        // No parser could handle this content — skip indexing
        if config.logging {
          self.log_system(&config_dir, "parsing.log",
            &format!("no parser available for {}", path));
        }
        return Ok(());
      }
    };

    let algo = self.engine.hash_algo();
    let file_key = crate::engine::directory_ops::file_path_hash(&normalized, &algo)?;
    let index_manager = IndexManager::new(self.engine);

    for field_config in &config.indexes {
      if let Err(e) = self.index_field(field_config, &json_data, &file_key, &config_dir, &index_manager) {
        if config.logging {
          self.log_system(&config_dir, "indexing.log",
            &format!("field '{}' indexing failed for {}: {}", field_config.name, path, e));
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
    let immediate_parent = parent_path(normalized_path).unwrap_or_else(|| "/".to_string());

    // 1. Check immediate parent (non-glob configs live here)
    if let Some(config) = self.load_config(&immediate_parent)? {
      if config.glob.is_none() {
        // No glob — this config applies to all direct children (existing behavior)
        return Ok(Some((config, immediate_parent)));
      }
      // Has a glob — check if the filename matches
      let filename = crate::engine::path_utils::file_name(normalized_path).unwrap_or_default();
      if glob_matches(config.glob.as_deref().unwrap_or(""), filename) {
        return Ok(Some((config, immediate_parent)));
      }
    }

    // 2. Walk up ancestors looking for glob configs
    let mut ancestor = parent_path(&immediate_parent);
    while let Some(ref dir) = ancestor {
      if let Some(config) = self.load_config(dir)? {
        if let Some(ref glob_pattern) = config.glob {
          // Compute relative path from this ancestor directory to the file
          let prefix = if dir == "/" {
            "/".to_string()
          } else {
            format!("{}/", dir)
          };
          if let Some(relative) = normalized_path.strip_prefix(&prefix) {
            if glob_matches(glob_pattern, relative) {
              return Ok(Some((config, dir.clone())));
            }
          }
        }
        // Non-glob config at ancestor — does not apply to nested files
      }

      if dir == "/" {
        break;
      }
      ancestor = parent_path(dir);
    }

    Ok(None)
  }

  fn load_config(&self, parent: &str) -> EngineResult<Option<PathIndexConfig>> {
    self.engine.index_config_cache.get(&parent.to_string(), self.engine)
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
    // @-prefixed fields: extract values from FileRecord metadata instead of JSON content.
    if field_config.name.starts_with('@') {
      return self.index_meta_field(field_config, file_key, parent, index_manager);
    }

    // Resolve field values based on source type (plural: fan-out supported)
    let field_values: Vec<Vec<u8>> = if let Some(source) = &field_config.source {
      if let Some(obj) = source.as_object() {
        // Plugin mapper: {"plugin": "name", "args": {...}}
        if let Some(plugin_name) = obj.get("plugin").and_then(|v| v.as_str()) {
          let args = obj.get("args").cloned().unwrap_or(serde_json::Value::Null);
          vec![self.invoke_mapper(plugin_name, json_data, &args)?]
        } else {
          return Ok(()); // invalid source object
        }
      } else if let Some(segments) = source.as_array() {
        // Array path resolution — fan-out on wildcards/regex
        let values = resolve_sources(json_data, segments);
        if values.is_empty() {
          return Ok(());
        }
        values
      } else {
        return Ok(()); // invalid source type
      }
    } else {
      // Default: use field name as key
      let default_source = vec![serde_json::Value::String(field_config.name.clone())];
      let values = resolve_sources(json_data, &default_source);
      if values.is_empty() {
        return Ok(());
      }
      values
    };

    // Load or create index
    let converter = create_converter_from_config(field_config)?;
    let strategy = converter.strategy().to_string();
    let mut index = match index_manager.load_index_by_strategy(parent, &field_config.name, &strategy)? {
      Some(idx) => idx,
      None => index_manager.create_index(parent, &field_config.name, converter)?,
    };

    // Clear old entries for this file, then insert all resolved values
    index.remove(file_key);
    for value in &field_values {
      index.insert_expanded(value, file_key.to_vec());
    }
    index_manager.save_index(parent, &index)?;

    Ok(())
  }

  /// Index a @-prefixed field by extracting the value from the FileRecord metadata
  /// rather than parsing the file's JSON content.
  fn index_meta_field(
    &self,
    field_config: &IndexFieldConfig,
    file_key: &[u8],
    parent: &str,
    index_manager: &IndexManager,
  ) -> EngineResult<()> {
    // Load the FileRecord from the storage engine.
    let entry = match self.engine.get_entry(file_key)? {
      Some(entry) => entry,
      None => return Ok(()), // file not yet stored — nothing to index
    };
    let (header, _key, value) = entry;
    let hash_length = self.engine.hash_algo().hash_length();
    let record = FileRecord::deserialize(&value, hash_length, header.entry_version)?;

    // Extract the value based on the @-field name.
    let extracted: Vec<u8> = match field_config.name.as_str() {
      "@filename" => {
        crate::engine::path_utils::file_name(&record.path)
          .unwrap_or_default()
          .as_bytes()
          .to_vec()
      }
      "@hash" => {
        if let Some(first_hash) = record.chunk_hashes.first() {
          hex::encode(first_hash).into_bytes()
        } else {
          Vec::new()
        }
      }
      "@created_at" => record.created_at.to_be_bytes().to_vec(),
      "@updated_at" => record.updated_at.to_be_bytes().to_vec(),
      "@size" => record.total_size.to_be_bytes().to_vec(),
      "@content_type" => {
        record.content_type.as_deref().unwrap_or("").as_bytes().to_vec()
      }
      _ => return Ok(()), // unknown @-field — skip silently
    };

    let field_values = vec![extracted];

    // Load or create index (same logic as the regular field path).
    let converter = create_converter_from_config(field_config)?;
    let strategy = converter.strategy().to_string();
    let mut index = match index_manager.load_index_by_strategy(parent, &field_config.name, &strategy)? {
      Some(idx) => idx,
      None => index_manager.create_index(parent, &field_config.name, converter)?,
    };

    // Clear old entries for this file, then insert all resolved values.
    index.remove(file_key);
    for value in &field_values {
      index.insert_expanded(value, file_key.to_vec());
    }
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
      .map(Self::parse_memory_limit)
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

  /// Look up a parser name from the global registry at /.aeordb-config/parsers.json
  fn lookup_parser_by_content_type(&self, content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    // Don't look up JSON — it's handled natively
    if ct == "application/json" {
      return None;
    }

    let ops = DirectoryOps::new(self.engine);
    match ops.read_file("/.aeordb-config/parsers.json") {
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
