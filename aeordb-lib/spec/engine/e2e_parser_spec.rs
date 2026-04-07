// End-to-end parser tests: deploy a real WASM parser binary, store files,
// and verify the indexing pipeline produces correct indexes.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::RequestContext;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::plugins::plugin_manager::PluginManager;
use aeordb::plugins::types::PluginType;
use std::sync::Arc;

fn create_test_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let ctx = RequestContext::system();
    let dir = tempfile::tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    (Arc::new(engine), dir)
}

fn load_plaintext_parser_wasm() -> Vec<u8> {
    let release_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../target/wasm32-unknown-unknown/release/aeordb_parser_plaintext.wasm"
    );
    let debug_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../target/wasm32-unknown-unknown/debug/aeordb_parser_plaintext.wasm"
    );

    if let Ok(bytes) = std::fs::read(release_path) {
        return bytes;
    }
    if let Ok(bytes) = std::fs::read(debug_path) {
        return bytes;
    }
    panic!(
        "Plaintext parser WASM not found. Build it first:\n\
         cd aeordb-parsers/plaintext && cargo build --target wasm32-unknown-unknown --release"
    );
}

fn deploy_plaintext_parser(engine: &Arc<StorageEngine>) -> PluginManager {
    let pm = PluginManager::new(engine.clone());
    let wasm_bytes = load_plaintext_parser_wasm();
    pm.deploy_plugin(
        "plaintext-parser",
        "plaintext-parser",
        PluginType::Wasm,
        wasm_bytes,
    )
    .expect("deploy parser");
    pm
}

// ============================================================
// Test: Deploy parser and store a file, verify indexes created
// ============================================================

#[test]
fn test_e2e_parser_deploy_and_store() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);

    let ops = DirectoryOps::new(&engine);

    // Store index config with parser and source paths
    let config = r#"{
        "parser": "plaintext-parser",
        "indexes": [
            {"name": "title", "source": ["title"], "type": "trigram"},
            {"name": "word_count", "source": ["metadata", "word_count"], "type": "u64"},
            {"name": "content", "source": ["text"], "type": "trigram"}
        ]
    }"#;
    ops.store_file(&ctx, "/docs/.config/indexes.json", config.as_bytes(), Some("application/json"))
        .expect("store config");

    // Store a text file — this should trigger the parser pipeline
    let text_content = "Hello World\nThis is a test document.\nIt has three lines.";
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/test.txt",
        text_content.as_bytes(),
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    // Verify indexes were created
    let index_manager = IndexManager::new(&engine);

    // Check title index
    let title_indexes = index_manager
        .load_indexes_for_field("/docs/", "title")
        .expect("load title indexes");
    assert!(!title_indexes.is_empty(), "title index should exist");

    // Check word_count index
    let wc_indexes = index_manager
        .load_indexes_for_field("/docs/", "word_count")
        .expect("load word_count indexes");
    assert!(!wc_indexes.is_empty(), "word_count index should exist");

    // Check content index
    let content_indexes = index_manager
        .load_indexes_for_field("/docs/", "content")
        .expect("load content indexes");
    assert!(!content_indexes.is_empty(), "content index should exist");
}

// ============================================================
// Test: Query u64 indexes after storing parsed files
// ============================================================
//
// NOTE: Trigram Contains/Similar queries use a recheck phase that
// re-reads the stored file and parses it as JSON to extract field
// values. This means Contains queries don't work for non-JSON files
// that were indexed via a parser (the stored data is plain text,
// not the parser's JSON output). Scalar (u64/i64/string) Eq queries
// work because they go through the standard index lookup path
// without recheck. This is a known limitation; future work could
// store parsed output or re-invoke the parser during recheck.

#[test]
fn test_e2e_parser_query_u64_after_store() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);

    let ops = DirectoryOps::new(&engine);
    // Index word_count as u64 — Eq queries on u64 use the scalar path (no recheck)
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"parser":"plaintext-parser","indexes":[{"name":"word_count","source":["metadata","word_count"],"type":"u64"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    // "Hello World greeting" = 3 words
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/hello.txt",
        b"Hello World greeting",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store hello");

    // "Goodbye World farewell extra words here" = 6 words
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/goodbye.txt",
        b"Goodbye World farewell extra words here",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store goodbye");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy,
    };
    let qe = QueryEngine::new(&engine);

    // Query for word_count == 3 (should find hello.txt)
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "word_count".to_string(),
            operation: QueryOp::Eq(3u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("query word_count=3");
    assert!(
        results.len() >= 1,
        "should find at least one result for word_count=3, got {}",
        results.len()
    );
    assert!(
        results.iter().any(|r| r.file_record.path.contains("hello.txt")),
        "should find hello.txt for word_count=3, results: {:?}",
        results.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );

    // Query for word_count == 6 (should find goodbye.txt)
    let query_6 = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "word_count".to_string(),
            operation: QueryOp::Eq(6u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let results_6 = qe.execute(&query_6).expect("query word_count=6");
    assert!(
        results_6.len() >= 1,
        "should find at least one result for word_count=6, got {}",
        results_6.len()
    );
    assert!(
        results_6.iter().any(|r| r.file_record.path.contains("goodbye.txt")),
        "should find goodbye.txt for word_count=6"
    );
}

// ============================================================
// Test: Non-JSON file without parser skips indexing gracefully
// ============================================================

#[test]
fn test_e2e_non_json_without_parser_skips() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let ops = DirectoryOps::new(&engine);

    // Config has indexes but NO parser
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"indexes":[{"name":"title","type":"string"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    // Store binary data — should not crash, just skip indexing
    let binary_data = vec![0u8, 1, 2, 3, 255, 254, 253];
    let result = ops.store_file_with_indexing(&ctx,
        "/docs/binary.bin",
        &binary_data,
        Some("application/octet-stream"),
    );
    assert!(result.is_ok(), "binary file should still be stored");

    // Verify file exists and data is intact
    let data = ops.read_file("/docs/binary.bin").expect("read back");
    assert_eq!(data, binary_data);
}

// ============================================================
// Test: Nested source path resolution (metadata.line_count)
// ============================================================

#[test]
fn test_e2e_parser_with_source_path_resolution() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);

    let ops = DirectoryOps::new(&engine);
    // Use nested source path: metadata.line_count
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"parser":"plaintext-parser","indexes":[{"name":"lines","source":["metadata","line_count"],"type":"u64"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    let text = "line one\nline two\nline three\nline four\nline five";
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/five_lines.txt",
        text.as_bytes(),
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store");

    // Verify the index exists and has an entry
    let index_manager = IndexManager::new(&engine);
    let indexes = index_manager
        .load_indexes_for_field("/docs/", "lines")
        .expect("load");
    assert!(
        !indexes.is_empty(),
        "lines index should exist from nested source path"
    );
}

// ============================================================
// Test: Parser failure logs when parser plugin not found
// ============================================================

#[test]
fn test_e2e_parser_logging_on_failure() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let ops = DirectoryOps::new(&engine);

    // Config references a parser that doesn't exist
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"parser":"nonexistent-parser","logging":true,"indexes":[{"name":"title","type":"string"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    // Store a file — parser not found, should log
    let pm = PluginManager::new(engine.clone());
    let result = ops.store_file_with_full_pipeline(&ctx,
        "/docs/test.txt",
        b"hello",
        Some("text/plain"),
        Some(&pm),
    );
    assert!(result.is_ok(), "file should still be stored even when parser fails");

    // Check that a log was created
    let log = ops.read_file("/docs/.logs/system/parsing.log");
    assert!(log.is_ok(), "parsing log should exist");
    let log_bytes = log.unwrap();
    let log_content = String::from_utf8_lossy(&log_bytes);
    assert!(
        log_content.contains("nonexistent-parser"),
        "log should mention the missing parser, got: {}",
        log_content
    );
}

// ============================================================
// Test: Multiple files indexed, distinguish via u64 range query
// ============================================================

#[test]
fn test_e2e_parser_multiple_files_distinct_queries() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);

    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"parser":"plaintext-parser","indexes":[{"name":"word_count","source":["metadata","word_count"],"type":"u64"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    // Store three files with distinct word counts
    // alpha: 9 words
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/alpha.txt",
        b"The quick brown fox jumps over the lazy dog",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store alpha");

    // beta: 6 words
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/beta.txt",
        b"Python programming language is very popular",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store beta");

    // gamma: 7 words
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/gamma.txt",
        b"The lazy cat sleeps all day long",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store gamma");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy,
    };
    let qe = QueryEngine::new(&engine);

    // Query for word_count == 9 — should find alpha
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "word_count".to_string(),
            operation: QueryOp::Eq(9u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };
    let results = qe.execute(&query).expect("query word_count=9");
    assert!(
        results.iter().any(|r| r.file_record.path.contains("alpha.txt")),
        "should find alpha.txt for word_count=9, got: {:?}",
        results.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );

    // Query for word_count == 6 — should find beta
    let query_6 = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "word_count".to_string(),
            operation: QueryOp::Eq(6u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };
    let results_6 = qe.execute(&query_6).expect("query word_count=6");
    assert!(
        results_6.iter().any(|r| r.file_record.path.contains("beta.txt")),
        "should find beta.txt for word_count=6, got: {:?}",
        results_6.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );

    // Query for word_count between 7 and 10 — should find alpha (9) and gamma (7)
    let query_range = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "word_count".to_string(),
            operation: QueryOp::Between(7u64.to_be_bytes().to_vec(), 10u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };
    let results_range = qe.execute(&query_range).expect("query word_count 7-10");
    assert!(
        results_range.len() >= 2,
        "should find at least 2 results for word_count 7-10, got {}",
        results_range.len()
    );
}

// ============================================================
// Test: File stored via full pipeline can be read back
// ============================================================

#[test]
fn test_e2e_parser_file_data_preserved() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);

    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"parser":"plaintext-parser","indexes":[{"name":"content","source":["text"],"type":"trigram"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    let original_text = b"This is the original file content that should be preserved exactly.";
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/preserved.txt",
        original_text,
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store");

    // Verify raw file data is preserved (parser doesn't alter the stored bytes)
    let read_back = ops.read_file("/docs/preserved.txt").expect("read back");
    assert_eq!(read_back, original_text.to_vec());
}

// ============================================================
// Test: Parser with empty file input
// ============================================================

#[test]
fn test_e2e_parser_empty_file() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);

    let ops = DirectoryOps::new(&engine);
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"parser":"plaintext-parser","indexes":[{"name":"content","source":["text"],"type":"trigram"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    // Store an empty file — parser should handle gracefully
    let result = ops.store_file_with_full_pipeline(&ctx,
        "/docs/empty.txt",
        b"",
        Some("text/plain"),
        Some(&pm),
    );
    assert!(result.is_ok(), "empty file should be stored without error");
}

// ============================================================
// Test: WASM binary can be loaded and validated
// ============================================================

#[test]
fn test_e2e_wasm_binary_is_valid() {
    let wasm_bytes = load_plaintext_parser_wasm();
    assert!(wasm_bytes.len() > 0, "WASM binary should not be empty");
    // WASM magic bytes: \0asm
    assert_eq!(&wasm_bytes[0..4], b"\0asm", "should start with WASM magic bytes");
}

// ============================================================
// Test: Config without parser field falls back to JSON parsing
// ============================================================

#[test]
fn test_e2e_no_parser_json_fallback() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();

    let ops = DirectoryOps::new(&engine);
    // Config with no parser — expects raw JSON data
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        r#"{"indexes":[{"name":"name","type":"string"}]}"#.as_bytes(),
        Some("application/json"),
    )
    .expect("config");

    // Store a JSON file — should index directly without parser
    let json_data = r#"{"name":"Alice","age":30}"#;
    let result = ops.store_file_with_indexing(&ctx,
        "/docs/user.json",
        json_data.as_bytes(),
        Some("application/json"),
    );
    assert!(result.is_ok(), "JSON file without parser should index directly");

    let index_manager = IndexManager::new(&engine);
    let indexes = index_manager
        .load_indexes_for_field("/docs/", "name")
        .expect("load");
    assert!(!indexes.is_empty(), "name index should exist from JSON fallback");
}

// ============================================================
// Test: Deploy parser with invalid WASM bytes fails cleanly
// ============================================================

#[test]
fn test_e2e_deploy_invalid_wasm_fails() {
    let (engine, _temp) = create_test_engine();
    let pm = PluginManager::new(engine.clone());

    let result = pm.deploy_plugin(
        "bad-parser",
        "bad-parser",
        PluginType::Wasm,
        vec![0xFF, 0xFF, 0xFF, 0xFF],
    );
    assert!(result.is_err(), "deploying invalid WASM should fail");
}
