// Tests for parser pipeline integration (Tasks 8-11)
//
// These tests verify the pipeline logic without live WASM invocation,
// covering parser envelope construction, memory limit parsing,
// content-type registry, plugin mapper source detection, and
// the full pipeline method.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::indexing_pipeline::IndexingPipeline;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;
use aeordb::plugins::PluginManager;
use std::sync::Arc;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.aeordb-config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file_buffered(&ctx, &config_path, &config_data, Some("application/json")).unwrap();
}

// ============================================================
// Task 8: Memory limit parsing
// ============================================================

#[test]
fn test_parse_memory_limit_mb() {
  // "256mb" should parse to 256 * 1024 * 1024 = 268435456
  // We test via config round-trip: set parser_memory_limit in config,
  // then invoke parser which uses parse_memory_limit internally.
  // Since parse_memory_limit is private, we verify via the pipeline behavior.
  // But we can test it indirectly through the envelope + config flow.
  //
  // For direct testing, we'll use a helper approach: create a pipeline
  // with a parser that doesn't exist, and verify the error message shows
  // the parser was attempted (which means memory limit was parsed).
  //
  // Actually, let's just test the parse_memory_limit logic by checking
  // the expected values from the spec. Since the method is private to
  // IndexingPipeline, we test the behavior through the pipeline.
  //
  // The simplest approach: verify that the pipeline correctly handles
  // various memory limit formats by ensuring it doesn't panic and
  // properly passes limits through to the plugin manager.
  //
  // Since we can't call parse_memory_limit directly, we test it
  // through the pipeline's behavior with parser configs.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/test".to_string()),
    parser_memory_limit: Some("256mb".to_string()),
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/data", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let data = b"some binary data";
  // Parser not found, but the pipeline should attempt it (not panic on memory limit parsing)
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/data/file.bin", data, Some("application/octet-stream"));
  assert!(result.is_ok(), "Pipeline should not error when parser fails (logs instead)");

  // Verify log was written about the parser failure
  let ops = DirectoryOps::new(&engine);
  let log = ops.read_file_buffered("/data/.logs/system/parsing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("parser '/parsers/test' failed"), "Log: {}", log_str);
}

#[test]
fn test_parse_memory_limit_gb() {
  // "1gb" should be parsed without panic
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/big".to_string()),
    parser_memory_limit: Some("1gb".to_string()),
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/bigdata", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/bigdata/file.bin", b"data", Some("application/octet-stream"));
  assert!(result.is_ok());
}

#[test]
fn test_parse_memory_limit_kb() {
  // "512kb" should be parsed without panic
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/small".to_string()),
    parser_memory_limit: Some("512kb".to_string()),
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/smalldata", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/smalldata/file.bin", b"data", Some("application/octet-stream"));
  assert!(result.is_ok());
}

#[test]
fn test_parse_memory_limit_default_on_invalid() {
  // "invalid" should fall back to default (256MB) without panic
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/fallback".to_string()),
    parser_memory_limit: Some("invalid".to_string()),
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/fallback", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/fallback/file.bin", b"data", Some("application/octet-stream"));
  assert!(result.is_ok());
}

#[test]
fn test_parse_memory_limit_plain_number() {
  // "1048576" (raw bytes) should be parsed without panic
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/raw".to_string()),
    parser_memory_limit: Some("1048576".to_string()),
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/rawlimit", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/rawlimit/file.bin", b"data", Some("application/octet-stream"));
  assert!(result.is_ok());
}

// ============================================================
// Task 8: Parser envelope structure
// ============================================================

#[test]
fn test_parser_envelope_structure() {
  // We can't call build_parser_envelope directly since it's private.
  // Instead, verify the envelope indirectly by checking the pipeline
  // attempts a parser invocation with the correct format.
  // The best way: check that when a parser is configured but missing,
  // the log message references the parser name correctly.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/envelope_test".to_string()),
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/envelope", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let data = b"hello world";
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/envelope/test.txt", data, Some("text/plain")).unwrap();

  let ops = DirectoryOps::new(&engine);
  let log = ops.read_file_buffered("/envelope/.logs/system/parsing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("/parsers/envelope_test"), "Log should mention parser name: {}", log_str);
  assert!(log_str.contains("/envelope/test.txt"), "Log should mention file path: {}", log_str);
}

#[test]
fn test_parser_envelope_data_is_base64() {
  // Indirectly tested: the pipeline constructs an envelope with base64-encoded data.
  // Since we can't intercept the WASM call without a real plugin, we verify
  // the pipeline doesn't crash when constructing the envelope for various data types.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/b64test".to_string()),
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/b64", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);

  // Test with binary data (including null bytes)
  let binary_data: Vec<u8> = (0..=255).collect();
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/b64/binary.dat", &binary_data, Some("application/octet-stream"));
  assert!(result.is_ok(), "Pipeline should handle binary data without panic");

  // Test with empty data
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/b64/empty.dat", &[], Some("application/octet-stream"));
  assert!(result.is_ok(), "Pipeline should handle empty data without panic");

  // Test with large data
  let large_data = vec![0xABu8; 10_000];
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/b64/large.dat", &large_data, Some("application/octet-stream"));
  assert!(result.is_ok(), "Pipeline should handle large data without panic");
}

#[test]
fn test_parser_envelope_meta_fields() {
  // The envelope should include meta.filename, meta.path, meta.content_type, meta.size.
  // We verify this by checking the pipeline processes correctly for various paths/types.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/meta_test".to_string()),
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/meta", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let data = b"PDF content";
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/meta/report.pdf", data, Some("application/pdf"));
  assert!(result.is_ok());

  // Also test with None content type
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/meta/unknown.xyz", data, None);
  assert!(result.is_ok(), "Pipeline should handle None content type");
}

#[test]
fn test_parser_envelope_filename_extraction() {
  // "/docs/reports/test.pdf" should extract filename "test.pdf"
  // We verify the pipeline processes the path without issues.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/fname_test".to_string()),
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/docs/reports", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/docs/reports/test.pdf", b"pdf data", Some("application/pdf"));
  assert!(result.is_ok());

  // The log should mention the file path
  let ops = DirectoryOps::new(&engine);
  let log = ops.read_file_buffered("/docs/reports/.logs/system/parsing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("/docs/reports/test.pdf"), "Log: {}", log_str);
}

// ============================================================
// Task 8/9: Parser not configured, raw JSON fallback
// ============================================================

#[test]
fn test_parser_not_configured_uses_raw_json() {
  // When no parser is configured and no content-type registry match,
  // the pipeline should parse the data directly as JSON.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "title".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/noparserdocs", &config);

  let data = br#"{"title":"Hello World"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/noparserdocs/doc.json", data, Some("application/json")).unwrap();

  // Verify index was created with entry from raw JSON
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/noparserdocs", "title").unwrap();
  assert!(index.is_some(), "Expected title index from raw JSON");
  assert_eq!(index.unwrap().len(), 1);
}

// ============================================================
// Task 9: Content-type parser registry
// ============================================================

#[test]
fn test_content_type_registry_lookup() {
  let ctx = RequestContext::system();
  // Store /.aeordb-config/parsers.json with a mapping, then verify the pipeline
  // attempts to use the mapped parser for that content type.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store content-type registry
  let registry = br#"{"application/pdf":"/parsers/pdf","text/csv":"/parsers/csv"}"#;
  ops.store_file_buffered(&ctx, "/.aeordb-config/parsers.json", registry, Some("application/json")).unwrap();

  // Store index config with NO explicit parser (should fall back to registry)
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/uploads", &config);

  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/uploads/report.pdf", b"pdf bytes", Some("application/pdf")).unwrap();

  // The log should mention the PDF parser was attempted
  let log = ops.read_file_buffered("/uploads/.logs/system/parsing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("/parsers/pdf"), "Expected PDF parser to be attempted: {}", log_str);
}

#[test]
fn test_content_type_registry_not_found() {
  let ctx = RequestContext::system();
  // Lookup unregistered content type should return None,
  // so pipeline falls back to raw JSON parsing.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store registry with only PDF
  let registry = br#"{"application/pdf":"/parsers/pdf"}"#;
  ops.store_file_buffered(&ctx, "/.aeordb-config/parsers.json", registry, Some("application/json")).unwrap();

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/csvdata", &config);

  // Use text/csv which is NOT in the registry — should fall back to raw JSON
  let data = br#"{"name":"test"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/csvdata/file.csv", data, Some("text/csv")).unwrap();

  // Raw JSON should be indexed
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/csvdata", "name").unwrap();
  assert!(index.is_some(), "Unregistered content type should fall back to raw JSON");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_content_type_json_skips_registry() {
  let ctx = RequestContext::system();
  // application/json should NEVER trigger a registry lookup —
  // it's handled natively as raw JSON.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store registry that maps application/json to a parser (should be ignored)
  let registry = br#"{"application/json":"/parsers/should_not_be_used"}"#;
  ops.store_file_buffered(&ctx, "/.aeordb-config/parsers.json", registry, Some("application/json")).unwrap();

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "value".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/jsondata", &config);

  // Store JSON data — should be parsed directly, NOT through the registered parser
  let data = br#"{"value":"hello"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/jsondata/test.json", data, Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/jsondata", "value").unwrap();
  assert!(index.is_some(), "JSON should be indexed directly without parser");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_content_type_registry_not_exists() {
  // When /.aeordb-config/parsers.json doesn't exist, lookup returns None
  // and falls back to raw JSON parsing.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // No registry file stored

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "key".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/noreg", &config);

  let data = br#"{"key":"value"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/noreg/file.json", data, Some("text/plain")).unwrap();

  // Should fall back to raw JSON parsing
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/noreg", "key").unwrap();
  assert!(index.is_some(), "Missing registry should fall back to raw JSON");
  assert_eq!(index.unwrap().len(), 1);
}

// ============================================================
// Task 10: Plugin mapper source detection
// ============================================================

#[test]
fn test_plugin_mapper_source_detection() {
  // When source is {"plugin": "name"}, it should be detected as a mapper.
  // Without a real plugin, the pipeline should error (plugin not found)
  // but with logging it should log the failure.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "computed".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!({"plugin": "/plugins/my_mapper", "args": {"mode": "upper"}})),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/mapped", &config);

  let data = br#"{"name":"Alice"}"#;
  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/mapped/user.json", data, Some("application/json")).unwrap();

  // Should have logged a mapper failure
  let ops = DirectoryOps::new(&engine);
  let log = ops.read_file_buffered("/mapped/.logs/system/indexing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("computed"), "Log should mention the field name: {}", log_str);
  assert!(log_str.contains("Mapper"), "Log should mention mapper failure: {}", log_str);
}

#[test]
fn test_plugin_mapper_source_without_args() {
  // {"plugin": "name"} without args should still work (args defaults to null)
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "derived".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!({"plugin": "/plugins/no_args_mapper"})),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/noargs", &config);

  let data = br#"{"x":1}"#;
  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/noargs/item.json", data, Some("application/json"));
  assert!(result.is_ok(), "Pipeline should not crash when mapper has no args");
}

#[test]
fn test_plugin_mapper_invalid_source_object() {
  // Source is an object but without "plugin" key should be skipped
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "weird".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!({"not_plugin": "something"})),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/invalid_src", &config);

  let data = br#"{"weird":"value"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/invalid_src/item.json", data, Some("application/json"));
  assert!(result.is_ok(), "Invalid source object should be silently skipped");

  // No index should be created for this field
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/invalid_src").unwrap();
  assert!(indexes.is_empty(), "Invalid source should not create indexes");
}

#[test]
fn test_array_source_still_works() {
  // source is ["a","b"] — should still do regular path resolution
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "nested_val".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!(["info", "status"])),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/arraysrc", &config);

  let data = br#"{"info":{"status":"active"}}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/arraysrc/item.json", data, Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/arraysrc", "nested_val").unwrap();
  assert!(index.is_some(), "Array source should work for path resolution");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_default_source_uses_field_name() {
  // When source is None, the pipeline should use [field_name] as the source
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "email".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/defaults", &config);

  let data = br#"{"email":"alice@example.com","name":"Alice"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/defaults/user.json", data, Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/defaults", "email").unwrap();
  assert!(index.is_some(), "Default source should use field name");
  assert_eq!(index.unwrap().len(), 1);
}

// ============================================================
// Task 8: Pipeline with no plugin manager
// ============================================================

#[test]
fn test_pipeline_with_none_plugin_manager_parser_config() {
  // When parser is configured but no plugin manager is available,
  // the pipeline should log the error and not crash.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: Some("/parsers/missing_pm".to_string()),
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/nopm", &config);

  // Pipeline without plugin manager
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/nopm/file.bin", b"data", Some("application/octet-stream"));
  assert!(result.is_ok(), "Pipeline should not crash without plugin manager");

  // Log should indicate plugin manager was required
  let ops = DirectoryOps::new(&engine);
  let log = ops.read_file_buffered("/nopm/.logs/system/parsing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("Plugin manager required"), "Log: {}", log_str);
}

#[test]
fn test_pipeline_with_none_plugin_manager_mapper_source() {
  // When mapper source is configured but no plugin manager, should log error
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "mapped".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!({"plugin": "/plugins/mapper"})),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/nopm_mapper", &config);

  let pipeline = IndexingPipeline::new(&engine);
  let data = br#"{"name":"test"}"#;
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/nopm_mapper/item.json", data, Some("application/json"));
  assert!(result.is_ok(), "Pipeline should not crash without plugin manager for mapper");

  let ops = DirectoryOps::new(&engine);
  let log = ops.read_file_buffered("/nopm_mapper/.logs/system/indexing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("Plugin manager required"), "Log: {}", log_str);
}

// ============================================================
// Task 11: Full pipeline method
// ============================================================

#[test]
fn test_full_pipeline_method_exists() {
  let ctx = RequestContext::system();
  // store_file_with_full_pipeline should be callable
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Call without plugin manager
  let result = ops.store_file_with_full_pipeline(&ctx,
    "/test/file.json",
    br#"{"name":"test"}"#,
    Some("application/json"),
    None,
  );
  assert!(result.is_ok(), "store_file_with_full_pipeline should work without plugin manager");

  // Verify file was stored
  let data = ops.read_file_buffered("/test/file.json").unwrap();
  assert_eq!(data, br#"{"name":"test"}"#);
}

#[test]
fn test_full_pipeline_with_plugin_manager() {
  let ctx = RequestContext::system();
  // store_file_with_full_pipeline should accept a PluginManager
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let result = ops.store_file_with_full_pipeline(&ctx,
    "/test2/data.json",
    br#"{"value":"hello"}"#,
    Some("application/json"),
    Some(&pm),
  );
  assert!(result.is_ok());

  let data = ops.read_file_buffered("/test2/data.json").unwrap();
  assert_eq!(data, br#"{"value":"hello"}"#);
}

#[test]
fn test_full_pipeline_indexes_json() {
  let ctx = RequestContext::system();
  // Full pipeline should index JSON data just like store_file_with_indexing
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "score".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: Some(0.0),
        max: Some(1000.0),
      },
    ],
  };
  store_index_config(&engine, "/scored", &config);

  ops.store_file_with_full_pipeline(&ctx,
    "/scored/player.json",
    br#"{"score":42}"#,
    Some("application/json"),
    None,
  ).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/scored", "score").unwrap();
  assert!(index.is_some(), "Full pipeline should create indexes");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_full_pipeline_skips_system_paths() {
  let ctx = RequestContext::system();
  // System paths should not be indexed even with full pipeline
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/app", &config);

  ops.store_file_with_full_pipeline(&ctx,
    "/app/.logs/entry.json",
    br#"{"name":"log_entry"}"#,
    Some("application/json"),
    None,
  ).unwrap();

  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/app/.logs").unwrap();
  assert!(indexes.is_empty(), "System paths should not be indexed via full pipeline");
}

// ============================================================
// Task 11: with_plugin_manager constructor
// ============================================================

#[test]
fn test_with_plugin_manager_constructor() {
  // Verify IndexingPipeline::with_plugin_manager can be created
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  // This should compile and not panic
  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);

  // Run against a path with no config — should be a no-op
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/empty/file.json", br#"{"a":1}"#, None);
  assert!(result.is_ok());
}

// ============================================================
// Edge cases and failure paths
// ============================================================

#[test]
fn test_parser_config_with_no_memory_limit_uses_default() {
  // parser_memory_limit is None, should use 256MB default
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  let config = PathIndexConfig {
    parser: Some("/parsers/default_limit".to_string()),
    parser_memory_limit: None, // No limit specified
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/defmem", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/defmem/file.bin", b"data", Some("application/octet-stream"));
  assert!(result.is_ok());
}

#[test]
fn test_explicit_parser_overrides_content_type_registry() {
  let ctx = RequestContext::system();
  // When both parser and content-type registry match, explicit parser should win
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store registry
  let registry = br#"{"application/pdf":"/parsers/registry_pdf"}"#;
  ops.store_file_buffered(&ctx, "/.aeordb-config/parsers.json", registry, Some("application/json")).unwrap();

  let engine_arc = Arc::new(StorageEngine::create(
    dir.path().join("pm.aeor").to_str().unwrap()
  ).unwrap());
  let pm = PluginManager::new(engine_arc);

  // Config with explicit parser (different from registry)
  let config = PathIndexConfig {
    parser: Some("/parsers/explicit_pdf".to_string()),
    parser_memory_limit: None,
    logging: true,
    glob: None,

    indexes: vec![],
  };
  store_index_config(&engine, "/override", &config);

  let pipeline = IndexingPipeline::with_plugin_manager(&engine, &pm);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/override/doc.pdf", b"pdf data", Some("application/pdf")).unwrap();

  // Log should mention the explicit parser, not the registry one
  let log = ops.read_file_buffered("/override/.logs/system/parsing.log").unwrap();
  let log_str = String::from_utf8(log).unwrap();
  assert!(log_str.contains("/parsers/explicit_pdf"), "Explicit parser should be used: {}", log_str);
  assert!(!log_str.contains("registry_pdf"), "Registry parser should NOT be used: {}", log_str);
}

#[test]
fn test_content_type_none_skips_registry() {
  let ctx = RequestContext::system();
  // When content_type is None, registry lookup should return None
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let registry = br#"{"application/pdf":"/parsers/pdf"}"#;
  ops.store_file_buffered(&ctx, "/.aeordb-config/parsers.json", registry, Some("application/json")).unwrap();

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "data".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/notype", &config);

  // content_type is None
  let data = br#"{"data":"test"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/notype/file.json", data, None).unwrap();

  // Should still index as raw JSON
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/notype", "data").unwrap();
  assert!(index.is_some());
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_source_as_string_value_is_invalid() {
  // source as a plain string (not array, not object) should be skipped
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "field".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!("just_a_string")),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/strsrc", &config);

  let data = br#"{"field":"value"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/strsrc/item.json", data, Some("application/json"));
  assert!(result.is_ok(), "String source should be silently skipped");

  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/strsrc").unwrap();
  assert!(indexes.is_empty(), "String source should not create indexes");
}

#[test]
fn test_multiple_fields_mixed_sources() {
  // Test a config with multiple fields: default source, array source, and plugin mapper
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      // Default source
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
      // Array source
      IndexFieldConfig {
        name: "city".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!(["address", "city"])),
        min: None,
        max: None,
      },
      // Plugin mapper source (will fail without plugin, but shouldn't crash pipeline)
      IndexFieldConfig {
        name: "computed".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!({"plugin": "/plugins/compute"})),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/mixed", &config);

  let data = br#"{"name":"Alice","address":{"city":"Portland","state":"OR"}}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/mixed/user.json", data, Some("application/json"));
  assert!(result.is_ok());

  let index_manager = IndexManager::new(&engine);

  // Default source should work
  let name_index = index_manager.load_index("/mixed", "name").unwrap();
  assert!(name_index.is_some());
  assert_eq!(name_index.unwrap().len(), 1);

  // Array source should work
  let city_index = index_manager.load_index("/mixed", "city").unwrap();
  assert!(city_index.is_some());
  assert_eq!(city_index.unwrap().len(), 1);
}
