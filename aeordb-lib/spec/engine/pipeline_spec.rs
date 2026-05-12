use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::indexing_pipeline::{IndexingPipeline, glob_matches};
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
    format!("{}/.aeordb-config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file(&ctx, &config_path, &config_data, Some("application/json")).unwrap();
}

fn make_simple_config(field_name: &str, index_type: &str) -> PathIndexConfig {
  PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

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
    glob: None,
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
  ops.store_file_with_indexing(&ctx, "/data/.aeordb-indexes/something.json", &data[..], Some("application/json")).unwrap();

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
  ops.store_file_with_indexing(&ctx, "/data/.aeordb-config/settings.json", &data[..], Some("application/json")).unwrap();

  // File should still be stored
  let stored = ops.read_file("/data/.aeordb-config/settings.json").unwrap();
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
  ops.store_file_with_indexing(&ctx, "/myapp/.aeordb-config/other.json", &data[..], Some("application/json")).unwrap();

  // File should exist
  let stored = ops.read_file("/myapp/.aeordb-config/other.json").unwrap();
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
    glob: None,

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
    glob: None,

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
    glob: None,

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
    "/articles/.aeordb-config/indexes.json",
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

// ============================================================
// glob_matches tests
// ============================================================

#[test]
fn test_glob_matches_single_star_segment() {
  assert!(glob_matches("*/session.json", "s1/session.json"));
  assert!(!glob_matches("*/session.json", "s1/notes.txt"));
}

#[test]
fn test_glob_matches_double_star() {
  assert!(glob_matches("**/*.json", "a/b/c/file.json"));
  assert!(glob_matches("**/*.json", "file.json")); // ** matches zero segments
}

#[test]
fn test_glob_matches_star_extension() {
  assert!(glob_matches("*.json", "test.json"));
  assert!(!glob_matches("*.json", "dir/test.json")); // * matches one segment only
}

#[test]
fn test_glob_matches_question_mark() {
  assert!(glob_matches("?.json", "a.json"));
  assert!(!glob_matches("?.json", "ab.json"));
}

#[test]
fn test_glob_matches_no_wildcards() {
  assert!(glob_matches("exact/match.json", "exact/match.json"));
  assert!(!glob_matches("exact/match.json", "exact/other.json"));
}

#[test]
fn test_glob_matches_double_star_middle() {
  // ** in the middle of a pattern
  assert!(glob_matches("a/**/z.json", "a/b/c/z.json"));
  assert!(glob_matches("a/**/z.json", "a/z.json")); // zero segments matched by **
  assert!(!glob_matches("a/**/z.json", "b/c/z.json")); // first segment must be "a"
}

#[test]
fn test_glob_matches_star_within_segment() {
  // * inside a segment matches characters, not slashes
  assert!(glob_matches("session-*.json", "session-001.json"));
  assert!(!glob_matches("session-*.json", "session.json")); // `-` must be present
}

// ============================================================
// Ancestor config discovery tests
// ============================================================

#[test]
fn test_ancestor_glob_config_indexes_at_config_dir() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Store a glob config at /sessions/ that matches */session.json
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("*/session.json".to_string()),
    indexes: vec![
      IndexFieldConfig {
        name: "status".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/sessions", &config);

  // Store a file at /sessions/s1/session.json — no config at /sessions/s1
  let data = br#"{"status":"active"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/sessions/s1/session.json", &data[..], Some("application/json")).unwrap();

  // Indexes should be created at /sessions/.indexes/ (the config owner dir)
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/sessions", "status").unwrap();
  assert!(index.is_some(), "Expected index at /sessions/ via glob config");
  assert_eq!(index.unwrap().len(), 1);

  // And NOT at /sessions/s1/.indexes/
  let child_indexes = index_manager.list_indexes("/sessions/s1").unwrap();
  assert!(child_indexes.is_empty(), "No indexes should exist at /sessions/s1/");
}

#[test]
fn test_ancestor_glob_config_non_matching_file_skipped() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Glob config at /sessions/ that only matches */session.json
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("*/session.json".to_string()),
    indexes: vec![
      IndexFieldConfig {
        name: "status".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/sessions", &config);

  // Store a file that does NOT match the glob
  let data = br#"{"status":"active"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/sessions/s1/notes.txt", &data[..], Some("application/json")).unwrap();

  // No indexes should be created anywhere
  let index_manager = IndexManager::new(&engine);
  let parent_indexes = index_manager.list_indexes("/sessions").unwrap();
  assert!(parent_indexes.is_empty(), "Non-matching file should not trigger indexing");
}

#[test]
fn test_ancestor_doublestar_glob_deep_nesting() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Glob config at /data/ that matches any .json file at any depth
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![
      IndexFieldConfig {
        name: "type".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/data", &config);

  // Store a deeply nested file
  let data = br#"{"type":"report"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/data/a/b/c/report.json", &data[..], Some("application/json")).unwrap();

  // Indexes should be at /data/.indexes/
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/data", "type").unwrap();
  assert!(index.is_some(), "Expected index at /data/ via ** glob config");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_immediate_parent_non_glob_takes_precedence() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Glob config at /projects/
  let glob_config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: Some("**/*.json".to_string()),
    indexes: vec![
      IndexFieldConfig {
        name: "global_field".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/projects", &glob_config);

  // Non-glob config at /projects/myapp/ (immediate parent)
  let local_config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![
      IndexFieldConfig {
        name: "local_field".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/projects/myapp", &local_config);

  // Store a file at /projects/myapp/data.json — immediate parent has a non-glob config
  let data = br#"{"local_field":"val","global_field":"val2"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/projects/myapp/data.json", &data[..], Some("application/json")).unwrap();

  // Indexes should be at /projects/myapp/ (immediate parent wins)
  let index_manager = IndexManager::new(&engine);
  let local_idx = index_manager.load_index("/projects/myapp", "local_field").unwrap();
  assert!(local_idx.is_some(), "Immediate parent non-glob config should be used");

  // No indexes at /projects/ for this file
  let global_idx = index_manager.load_index("/projects", "global_field").unwrap();
  assert!(global_idx.is_none(), "Ancestor glob should not be used when immediate parent has non-glob config");
}

// ============================================================
// resolve_sources (plural) fan-out tests
// ============================================================

#[test]
fn test_pipeline_fanout_source_indexes_multiple_values() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Config with source using wildcard "" to fan out over array elements
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![
      IndexFieldConfig {
        name: "tag".to_string(),
        index_type: "string".to_string(),
        source: Some(serde_json::json!(["tags", ""])),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/articles", &config);

  let data = br#"{"tags":["rust","database","indexing"]}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  pipeline.run(&ctx, "/articles/post.json", &data[..], Some("application/json")).unwrap();

  // Index should have 3 entries (one per tag) for the same file
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/articles", "tag").unwrap();
  assert!(index.is_some(), "Expected tag index with fan-out entries");
  assert_eq!(index.unwrap().len(), 3, "Fan-out should create one entry per resolved value");
}

// ============================================================
// @-field (metadata) indexing tests
// ============================================================

#[test]
fn test_at_filename_field_gets_indexed() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Config with @filename field using trigram index
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![
      IndexFieldConfig {
        name: "@filename".to_string(),
        index_type: "trigram".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/files", &config);

  // Store a file (creates the FileRecord in the engine)
  let data = br#"{"irrelevant": true}"#;
  ops.store_file_with_indexing(&ctx, "/files/report.json", &data[..], Some("application/json")).unwrap();

  // Verify the @filename index was created
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/files").unwrap();
  let has_filename_idx = indexes.iter().any(|name| name.contains("@filename"));
  assert!(has_filename_idx, "Expected @filename index, got: {:?}", indexes);

  // Load the index and verify it has an entry
  let index = index_manager.load_index("/files", "@filename").unwrap();
  assert!(index.is_some(), "Expected @filename index to be loadable");
  let idx = index.unwrap();
  assert!(!idx.is_empty(), "Expected at least one entry in @filename index");
}

#[test]
fn test_at_size_field_gets_indexed() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Config with @size field using u64 index
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![
      IndexFieldConfig {
        name: "@size".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: Some(0.0),
        max: Some(1_000_000.0),
      },
    ],
  };
  store_index_config(&engine, "/sized", &config);

  let data = br#"{"some": "content"}"#;
  ops.store_file_with_indexing(&ctx, "/sized/doc.json", &data[..], Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/sized", "@size").unwrap();
  assert!(index.is_some(), "Expected @size index to be created");
  assert_eq!(index.unwrap().len(), 1, "Expected one entry in @size index");
}

#[test]
fn test_at_content_type_field_gets_indexed() {
  let ctx = RequestContext::system();
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
        name: "@content_type".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/typed", &config);

  let data = br#"{"val": 1}"#;
  ops.store_file_with_indexing(&ctx, "/typed/item.json", &data[..], Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/typed", "@content_type").unwrap();
  assert!(index.is_some(), "Expected @content_type index to be created");
  assert_eq!(index.unwrap().len(), 1, "Expected one entry in @content_type index");
}

#[test]
fn test_at_created_at_field_gets_indexed() {
  let ctx = RequestContext::system();
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
        name: "@created_at".to_string(),
        index_type: "i64".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/timed", &config);

  let data = br#"{"x": 1}"#;
  ops.store_file_with_indexing(&ctx, "/timed/event.json", &data[..], Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/timed", "@created_at").unwrap();
  assert!(index.is_some(), "Expected @created_at index to be created");
  assert_eq!(index.unwrap().len(), 1);
}

#[test]
fn test_at_field_unknown_name_silently_skipped() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Config with unknown @-field — should be silently skipped
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![
      IndexFieldConfig {
        name: "@nonexistent".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/unknown", &config);

  let data = br#"{"val": 1}"#;
  let result = ops.store_file_with_indexing(&ctx, "/unknown/file.json", &data[..], Some("application/json"));
  assert!(result.is_ok(), "Unknown @-field should not cause an error");

  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/unknown").unwrap();
  assert!(indexes.is_empty(), "No index should exist for unknown @-field");
}

#[test]
fn test_internal_path_skipped_by_pipeline() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Config at root that would match everything
  let config = make_simple_config("name", "string");
  store_index_config(&engine, "/", &config);

  // Run pipeline directly with an internal path (.config)
  let data = br#"{"name":"should not index"}"#;
  let pipeline = IndexingPipeline::new(&engine);
  let ctx = RequestContext::system();
  let result = pipeline.run(&ctx, "/.aeordb-config/indexes.json", &data[..], Some("application/json"));
  assert!(result.is_ok(), "Internal path should return Ok without indexing");

  // Also try .aeordb-indexes
  let result2 = pipeline.run(&ctx, "/data/.aeordb-indexes/something.idx", &data[..], Some("application/octet-stream"));
  assert!(result2.is_ok(), "Internal .aeordb-indexes path should return Ok without indexing");

  // Also try .logs
  let result3 = pipeline.run(&ctx, "/data/.logs/system/parsing.log", &data[..], Some("text/plain"));
  assert!(result3.is_ok(), "Internal .logs path should return Ok without indexing");
}

#[test]
fn test_at_field_mixed_with_regular_fields() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Config with both a regular field and an @-field
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
      IndexFieldConfig {
        name: "@filename".to_string(),
        index_type: "trigram".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/mixed", &config);

  let data = br#"{"name":"Alice"}"#;
  ops.store_file_with_indexing(&ctx, "/mixed/alice.json", &data[..], Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);

  // Regular field should be indexed
  let name_idx = index_manager.load_index("/mixed", "name").unwrap();
  assert!(name_idx.is_some(), "Expected name index");
  assert_eq!(name_idx.unwrap().len(), 1);

  // @-field should also be indexed
  let filename_idx = index_manager.load_index("/mixed", "@filename").unwrap();
  assert!(filename_idx.is_some(), "Expected @filename index");
  assert!(!filename_idx.unwrap().is_empty(), "Expected entries in @filename index");
}

#[test]
fn test_at_field_overwrite_replaces_index_entry() {
  let ctx = RequestContext::system();
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
        name: "@size".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: Some(0.0),
        max: Some(1_000_000.0),
      },
    ],
  };
  store_index_config(&engine, "/overwrite", &config);

  // Store first version
  let data1 = br#"{"short": true}"#;
  ops.store_file_with_indexing(&ctx, "/overwrite/doc.json", &data1[..], Some("application/json")).unwrap();

  // Overwrite with larger content
  let data2 = br#"{"much_longer_content": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
  ops.store_file_with_indexing(&ctx, "/overwrite/doc.json", &data2[..], Some("application/json")).unwrap();

  // Should have exactly 1 entry (not 2)
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/overwrite", "@size").unwrap().unwrap();
  assert_eq!(index.len(), 1, "Overwrite should replace, not duplicate @-field index entry");
}
