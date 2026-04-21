use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::indexing_pipeline::IndexingPipeline;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

/// Store an index config at the given parent path.
fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file(&ctx, &config_path, &config_data, Some("application/json")).unwrap();
}

fn make_simple_config(field_name: &str, index_type: &str) -> PathIndexConfig {
  PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: field_name.to_string(),
        index_type: index_type.to_string(),
        source: None,
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  }
}

fn make_config_with_logging(field_name: &str, index_type: &str, logging: bool) -> PathIndexConfig {
  PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging,
    indexes: vec![
      IndexFieldConfig {
        name: field_name.to_string(),
        index_type: index_type.to_string(),
        source: None,
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  }
}

// ============================================================
// Recursive guard tests (Task 4)
// ============================================================

#[test]
fn test_system_path_logs() {
  let ctx = RequestContext::system();
  // .logs paths should be recognized as system paths
  // We test via store_file_with_indexing behavior since is_system_path is private
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Set up index config at /data/
  let config = make_simple_config("name", "string");
  store_index_config(&engine, "/data", &config);

  // Store a JSON file at .logs path — should not create indexes
  let data = br#"{"name":"test"}"#;
  ops.store_file_with_indexing(&ctx, "/data/.logs/entry.json", &data[..], Some("application/json")).unwrap();

  // Verify no indexes at /data/.logs
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/data/.logs").unwrap();
  assert!(indexes.is_empty(), "Expected no indexes at .logs path, got: {:?}", indexes);
}

#[test]
fn test_system_path_indexes() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Set up index config at /data/
  let config = make_simple_config("name", "string");
  store_index_config(&engine, "/data", &config);

  // Store a file at .indexes path — should not trigger indexing
  let data = br#"{"name":"test"}"#;
  ops.store_file_with_indexing(&ctx, "/data/.indexes/something.json", &data[..], Some("application/json")).unwrap();

  // Verify no indexes at /data/.indexes
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/data/.indexes").unwrap();
  assert!(indexes.is_empty(), "Expected no indexes at .indexes path, got: {:?}", indexes);
}

#[test]
fn test_system_path_config() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store a JSON file at .config path — should not trigger indexing pipeline
  let data = br#"{"name":"test"}"#;
  ops.store_file_with_indexing(&ctx, "/data/.config/settings.json", &data[..], Some("application/json")).unwrap();

  // File should still be stored
  let stored = ops.read_file("/data/.config/settings.json").unwrap();
  assert_eq!(stored, data.to_vec());
}

#[test]
fn test_normal_path_not_system() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Set up index config at /users/
  let config = make_simple_config("name", "string");
  store_index_config(&engine, "/users", &config);

  // Store a regular file — should trigger indexing
  let data = br#"{"name":"Alice"}"#;
  ops.store_file_with_indexing(&ctx, "/users/alice.json", &data[..], Some("application/json")).unwrap();

  // Verify index was created
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/users", "name").unwrap();
  assert!(index.is_some(), "Expected index to be created for normal path");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_store_to_logs_does_not_index() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store config for /app path
  let config = make_simple_config("level", "string");
  store_index_config(&engine, "/app", &config);

  // Store multiple JSON files under .logs
  let log1 = br#"{"level":"INFO","message":"started"}"#;
  let log2 = br#"{"level":"ERROR","message":"failed"}"#;
  ops.store_file_with_indexing(&ctx, "/app/.logs/log1.json", &log1[..], Some("application/json")).unwrap();
  ops.store_file_with_indexing(&ctx, "/app/.logs/log2.json", &log2[..], Some("application/json")).unwrap();

  // Files should be stored
  let stored1 = ops.read_file("/app/.logs/log1.json").unwrap();
  assert_eq!(stored1, log1.to_vec());
  let stored2 = ops.read_file("/app/.logs/log2.json").unwrap();
  assert_eq!(stored2, log2.to_vec());

  // No indexes should exist at /app/.logs
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/app/.logs").unwrap();
  assert!(indexes.is_empty(), "System path .logs should not have indexes");
}

#[test]
fn test_store_to_config_does_not_index() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store a file under .config (not indexes.json, a different file)
  let data = br#"{"setting":"value"}"#;
  ops.store_file_with_indexing(&ctx, "/myapp/.config/other.json", &data[..], Some("application/json")).unwrap();

  // File should exist
  let stored = ops.read_file("/myapp/.config/other.json").unwrap();
  assert_eq!(stored, data.to_vec());

  // No indexes at /myapp/.config
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/myapp/.config").unwrap();
  assert!(indexes.is_empty(), "System path .config should not have indexes");
}

// ============================================================
// Pipeline tests (Task 5)
// ============================================================

#[test]
fn test_pipeline_indexes_json_file() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Set up index config
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: "age".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/people", &config);

  // Run the pipeline directly
  let data = br#"{"age":42,"name":"Bob"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/people/bob.json", &data[..], Some("application/json")).unwrap();

  // Verify index was created with entry
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/people", "age").unwrap();
  assert!(index.is_some(), "Expected age index to be created");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_pipeline_source_path_resolution() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Config with source pointing to nested path: ["metadata", "title"]
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: "title".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!(["metadata", "title"])),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/docs", &config);

  let data = br#"{"metadata":{"title":"Hello World","author":"Alice"},"body":"content"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/docs/doc1.json", &data[..], Some("application/json")).unwrap();

  // Verify index entry exists
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/docs", "title").unwrap();
  assert!(index.is_some(), "Expected title index to be created");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_pipeline_missing_source_skips_field() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Config with source pointing to nonexistent path
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: "missing_field".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!(["nonexistent", "deeply", "nested"])),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/items", &config);

  let data = br#"{"name":"test","value":42}"#;
  let pipeline = IndexingPipeline::new(&engine);

  // Should not error
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/items/item1.json", &data[..], Some("application/json"));
  assert!(result.is_ok(), "Pipeline should not error on missing source");

  // No index entries should be created (index file may or may not exist)
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/items").unwrap();
  // Either no index file, or an empty one
  if !indexes.is_empty() {
    let idx = index_manager.load_index("/items", "missing_field").unwrap();
    if let Some(index) = idx {
      assert_eq!(index.len(), 0, "Expected no entries for missing source field");
    }
  }
}

#[test]
fn test_pipeline_no_config_no_indexing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // No config stored — pipeline should do nothing
  let data = br#"{"name":"test"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/nocfg/file.json", &data[..], Some("application/json"));
  assert!(result.is_ok());

  // No indexes should exist
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/nocfg").unwrap();
  assert!(indexes.is_empty());
}

#[test]
fn test_pipeline_non_json_data_skips() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Set up config
  let config = make_simple_config("name", "string");
  store_index_config(&engine, "/bindata", &config);

  // Store binary (non-JSON) data via store_file_with_indexing
  let binary_data = vec![0xFF, 0xFE, 0x00, 0x01, 0x02];
  let result = ops.store_file_with_indexing(&ctx,
    "/bindata/blob.bin",
    &binary_data,
    Some("application/octet-stream"),
  );
  assert!(result.is_ok(), "Non-JSON data should not cause an error");

  // File should still be stored
  let stored = ops.read_file("/bindata/blob.bin").unwrap();
  assert_eq!(stored, binary_data);

  // No index entries for the binary file
  let index_manager = IndexManager::new(&engine);
  let idx = index_manager.load_index("/bindata", "name").unwrap();
  match idx {
    Some(index) => assert_eq!(index.len(), 0),
    None => {} // also fine
  }
}

#[test]
fn test_pipeline_type_array_expansion() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Config with type as array: ["string", "trigram"]
  // PathIndexConfig::deserialize handles this by creating two IndexFieldConfig entries
  let config_json = br#"{"indexes":[{"name":"title","type":["string","trigram"]}]}"#;
  let ops = DirectoryOps::new(&engine);
  ops.store_file(&ctx,
    "/articles/.config/indexes.json",
    &config_json[..],
    Some("application/json"),
  ).unwrap();

  let data = br#"{"title":"Hello World"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  pipeline.run(&ctx, "/articles/post1.json", &data[..], Some("application/json")).unwrap();

  // Verify both indexes were created
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/articles").unwrap();

  // Should have both string and trigram indexes for "title"
  let has_string = indexes.iter().any(|name| name.contains("string"));
  let has_trigram = indexes.iter().any(|name| name.contains("trigram"));

  assert!(has_string, "Expected string index for title, got: {:?}", indexes);
  assert!(has_trigram, "Expected trigram index for title, got: {:?}", indexes);
}

#[test]
fn test_pipeline_logging_creates_log_on_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Config with logging=true and a source pointing to nonexistent nested key.
  // The field source resolves to None, which means the field is silently skipped
  // (not an error). To trigger an actual indexing error log, we need a different
  // scenario. Let's use a config with a bad converter type to trigger an error
  // during index_field.
  //
  // Actually, resolve_source returning None just skips — no error.
  // Let's test with logging + non-JSON data to trigger parse failure log.
  let config = make_config_with_logging("name", "string", true);
  store_index_config(&engine, "/logged", &config);

  // Send invalid UTF-8 binary data — not valid JSON, and the native text
  // parser also rejects it ("not valid UTF-8"), triggering an error log.
  let data: &[u8] = &[0xFF, 0xFE, 0x00, 0x80];
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/logged/bad.bin", data, Some("application/octet-stream")).unwrap();

  // Check that .logs/system/parsing.log was created
  let ops = DirectoryOps::new(&engine);
  let log_result = ops.read_file("/logged/.logs/system/parsing.log");
  assert!(log_result.is_ok(), "Expected parsing.log to be created");
  let log_content = String::from_utf8(log_result.unwrap()).unwrap();
  assert!(log_content.contains("parser") || log_content.contains("failed") || log_content.contains("no parser"),
    "Log should contain failure message, got: {}", log_content);
  assert!(log_content.contains("/logged/bad.bin"), "Log should reference the file path");
}

#[test]
fn test_pipeline_logging_disabled_no_log() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Config with logging=false
  let config = make_config_with_logging("name", "string", false);
  store_index_config(&engine, "/nolog", &config);

  // Send non-JSON data
  let data = b"not json either";
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/nolog/bad.txt", &data[..], Some("text/plain")).unwrap();

  // Check that .logs/system/parsing.log was NOT created
  let ops = DirectoryOps::new(&engine);
  let log_result = ops.read_file("/nolog/.logs/system/parsing.log");
  assert!(log_result.is_err(), "Expected no parsing.log when logging is disabled");
}

// ============================================================
// Additional edge case tests
// ============================================================

#[test]
fn test_system_path_deeply_nested() {
  let ctx = RequestContext::system();
  // Even deeply nested .logs paths should be caught
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let data = br#"{"name":"deep"}"#;
  ops.store_file_with_indexing(&ctx,
    "/a/b/c/.logs/deep/entry.json",
    &data[..],
    Some("application/json"),
  ).unwrap();

  // File should be stored
  let stored = ops.read_file("/a/b/c/.logs/deep/entry.json").unwrap();
  assert_eq!(stored, data.to_vec());
}

#[test]
fn test_system_path_not_triggered_by_similar_names() {
  let ctx = RequestContext::system();
  // Paths like "/data/logs/file.json" (no dot prefix) should NOT be treated as system paths
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = make_simple_config("name", "string");
  store_index_config(&engine, "/data/logs", &config);

  let data = br#"{"name":"test"}"#;
  ops.store_file_with_indexing(&ctx, "/data/logs/entry.json", &data[..], Some("application/json")).unwrap();

  // This SHOULD trigger indexing since "logs" != ".logs"
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/data/logs", "name").unwrap();
  assert!(index.is_some(), "Regular 'logs' path (no dot) should still be indexed");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_pipeline_empty_json_object() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = make_simple_config("name", "string");
  store_index_config(&engine, "/empty", &config);

  // Store an empty JSON object — field not found, should skip gracefully
  let data = br#"{}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/empty/empty.json", &data[..], Some("application/json"));
  assert!(result.is_ok());
}

#[test]
fn test_pipeline_run_twice_overwrites_index() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
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
  store_index_config(&engine, "/scores", &config);

  // Store first version via full pipeline (store_file_with_indexing creates file + runs pipeline)
  let ops = DirectoryOps::new(&engine);
  let data1 = br#"{"score":100}"#;
  ops.store_file_with_indexing(&ctx, "/scores/player1.json", &data1[..], Some("application/json")).unwrap();

  // Overwrite with new score
  let data2 = br#"{"score":200}"#;
  ops.store_file_with_indexing(&ctx, "/scores/player1.json", &data2[..], Some("application/json")).unwrap();

  // Should still have exactly 1 entry (not 2)
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/scores", "score").unwrap().unwrap();
  assert_eq!(index.len(), 1, "Overwrite should replace, not duplicate index entry");
}
