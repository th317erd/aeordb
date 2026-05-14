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

fn load_plaintext_parser_wasm() -> Option<Vec<u8>> {
    let release_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../target/wasm32-unknown-unknown/release/aeordb_parser_plaintext.wasm"
    );
    let debug_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../target/wasm32-unknown-unknown/debug/aeordb_parser_plaintext.wasm"
    );

    if let Ok(bytes) = std::fs::read(release_path) {
        return Some(bytes);
    }
    if let Ok(bytes) = std::fs::read(debug_path) {
        return Some(bytes);
    }
    None
}

/// Helper macro: skip the test if the WASM parser binary is not available.
macro_rules! require_wasm_parser {
    () => {
        match load_plaintext_parser_wasm() {
            Some(bytes) => bytes,
            None => {
                eprintln!(
                    "SKIPPED: Plaintext parser WASM not built. Run:\n  \
                     cd aeordb-parsers/plaintext && cargo build --target wasm32-unknown-unknown --release"
                );
                return;
            }
        }
    };
}

fn deploy_plaintext_parser(engine: &Arc<StorageEngine>, wasm_bytes: Vec<u8>) -> PluginManager {
    let pm = PluginManager::new(engine.clone());
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
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

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
    ops.store_file_buffered(&ctx, "/docs/.aeordb-config/indexes.json", config.as_bytes(), Some("application/json"))
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
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);
    // Index word_count as u64 — Eq queries on u64 use the scalar path (no recheck)
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
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
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let results = qe.execute(&query).expect("query word_count=3");
    assert!(
        !results.is_empty(),
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
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let results_6 = qe.execute(&query_6).expect("query word_count=6");
    assert!(
        !results_6.is_empty(),
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
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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
    let data = ops.read_file_buffered("/docs/binary.bin").expect("read back");
    assert_eq!(data, binary_data);
}

// ============================================================
// Test: Nested source path resolution (metadata.line_count)
// ============================================================

#[test]
fn test_e2e_parser_with_source_path_resolution() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);
    // Use nested source path: metadata.line_count
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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
    let log = ops.read_file_buffered("/docs/.logs/system/parsing.log");
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
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
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
        aggregate: None,
        explain: ExplainMode::Off,
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
        aggregate: None,
        explain: ExplainMode::Off,
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
        aggregate: None,
        explain: ExplainMode::Off,
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
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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
    let read_back = ops.read_file_buffered("/docs/preserved.txt").expect("read back");
    assert_eq!(read_back, original_text.to_vec());
}

// ============================================================
// Test: Parser with empty file input
// ============================================================

#[test]
fn test_e2e_parser_empty_file() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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
    let wasm_bytes = require_wasm_parser!();
    assert!(!wasm_bytes.is_empty(), "WASM binary should not be empty");
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
    ops.store_file_buffered(&ctx,
        "/docs/.aeordb-config/indexes.json",
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

// ============================================================
// Test: Contains query on trigram-indexed parser field
// ============================================================

#[test]
fn test_e2e_parser_contains_on_trigram_field() {
    let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);

    // Config with trigram index on text field
    let config = r#"{
        "parser": "plaintext-parser",
        "indexes": [
            {"name": "text", "source": ["text"], "type": "trigram"}
        ]
    }"#;
    ops.store_file_buffered(&ctx, "/docs/.aeordb-config/indexes.json", config.as_bytes(), Some("application/json"))
        .expect("store config");

    // Store a text file
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/fox.txt",
        b"The quick brown fox jumps over the lazy dog",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store fox.txt");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
    };
    let qe = QueryEngine::new(&engine);

    // Contains query for "quick brown"
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Contains("quick brown".to_string()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let results = qe.execute(&query).expect("contains query");
    assert!(
        results.iter().any(|r| r.file_record.path.contains("fox.txt")),
        "Contains('quick brown') should find fox.txt, got: {:?}",
        results.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );
}

// ============================================================
// Test: Similar query on trigram-indexed parser field
// ============================================================

#[test]
fn test_e2e_parser_similar_on_trigram_field() {
    let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);

    let config = r#"{
        "parser": "plaintext-parser",
        "indexes": [
            {"name": "text", "source": ["text"], "type": "trigram"}
        ]
    }"#;
    ops.store_file_buffered(&ctx, "/docs/.aeordb-config/indexes.json", config.as_bytes(), Some("application/json"))
        .expect("store config");

    // Store two files
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/fox.txt",
        b"The quick brown fox jumps over the lazy dog",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store fox.txt");

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/greeting.txt",
        b"Hello World",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store greeting.txt");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
    };
    let qe = QueryEngine::new(&engine);

    // Similar query with typos — should still find greeting.txt
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Similar("Helllo Wrld".to_string(), 0.3),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let results = qe.execute(&query).expect("similar query");
    assert!(
        results.iter().any(|r| r.file_record.path.contains("greeting.txt")),
        "Similar('Helllo Wrld', 0.3) should find greeting.txt, got: {:?}",
        results.iter().map(|r| (&r.file_record.path, r.score)).collect::<Vec<_>>()
    );
    // Score should be positive
    let greeting_result = results.iter().find(|r| r.file_record.path.contains("greeting.txt")).unwrap();
    assert!(
        greeting_result.score > 0.0,
        "greeting.txt should have positive similarity score, got {}",
        greeting_result.score
    );
}

// ============================================================
// Test: Contains returns empty for non-matching text
// ============================================================

#[test]
fn test_e2e_parser_contains_no_match() {
    let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);

    let config = r#"{
        "parser": "plaintext-parser",
        "indexes": [
            {"name": "text", "source": ["text"], "type": "trigram"}
        ]
    }"#;
    ops.store_file_buffered(&ctx, "/docs/.aeordb-config/indexes.json", config.as_bytes(), Some("application/json"))
        .expect("store config");

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/fox.txt",
        b"The quick brown fox jumps over the lazy dog",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store fox.txt");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
    };
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Contains("zzzzznothere".to_string()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let results = qe.execute(&query).expect("contains no-match query");
    assert!(
        results.is_empty(),
        "Contains('zzzzznothere') should return empty results, got {} results: {:?}",
        results.len(),
        results.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );
}

// ============================================================
// Test: String index Eq on parser-extracted title field
// ============================================================

#[test]
fn test_e2e_parser_string_eq_on_title() {
    let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);

    // Config with string index on title
    let config = r#"{
        "parser": "plaintext-parser",
        "indexes": [
            {"name": "title", "source": ["title"], "type": "string"}
        ]
    }"#;
    ops.store_file_buffered(&ctx, "/docs/.aeordb-config/indexes.json", config.as_bytes(), Some("application/json"))
        .expect("store config");

    // Store a file whose first line is the title
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/important.txt",
        b"My Important Document\nThis is the body of the document.\nIt has multiple lines.",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store important.txt");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
    };
    let qe = QueryEngine::new(&engine);

    // Eq query on title field with the exact first line
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "title".to_string(),
            operation: QueryOp::Eq("My Important Document".as_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let results = qe.execute(&query).expect("string eq query on title");
    assert!(
        results.iter().any(|r| r.file_record.path.contains("important.txt")),
        "Eq('My Important Document') on title should find important.txt, got: {:?}",
        results.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );
}

// ============================================================
// Test: Content-type auto-routing via parsers.json
// ============================================================

#[test]
fn test_e2e_parser_content_type_auto_routing() {
    let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);

    // Deploy global content-type -> parser mapping
    let parsers_json = r#"{"text/plain": "plaintext-parser"}"#;
    ops.store_file_buffered(&ctx, "/.aeordb-config/parsers.json", parsers_json.as_bytes(), Some("application/json"))
        .expect("store parsers.json");

    // Index config at /auto/ with NO parser field — relies on content-type routing
    let config = r#"{
        "indexes": [
            {"name": "word_count", "source": ["metadata", "word_count"], "type": "u64"}
        ]
    }"#;
    ops.store_file_buffered(&ctx, "/auto/.aeordb-config/indexes.json", config.as_bytes(), Some("application/json"))
        .expect("store auto config");

    // "one two three four five" = 5 words
    ops.store_file_with_full_pipeline(&ctx,
        "/auto/test.txt",
        b"one two three four five",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store test.txt via auto-routing");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
    };
    let qe = QueryEngine::new(&engine);

    // Query for word_count == 5
    let query = Query {
        path: "/auto/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "word_count".to_string(),
            operation: QueryOp::Eq(5u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let results = qe.execute(&query).expect("query word_count=5 via auto-routing");
    assert!(
        results.iter().any(|r| r.file_record.path.contains("test.txt")),
        "Content-type auto-routing should find test.txt for word_count=5, got: {:?}",
        results.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );
}

// ============================================================
// Test: Multiple files with distinct trigram Contains queries
// ============================================================

#[test]
fn test_e2e_parser_multiple_files_trigram_contains() {
    let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let wasm_bytes = require_wasm_parser!();
    let pm = deploy_plaintext_parser(&engine, wasm_bytes);

    let ops = DirectoryOps::new(&engine);

    let config = r#"{
        "parser": "plaintext-parser",
        "indexes": [
            {"name": "text", "source": ["text"], "type": "trigram"}
        ]
    }"#;
    ops.store_file_buffered(&ctx, "/docs/.aeordb-config/indexes.json", config.as_bytes(), Some("application/json"))
        .expect("store config");

    // Store 3 files with distinct content
    ops.store_file_with_full_pipeline(&ctx,
        "/docs/astronomy.txt",
        b"The Andromeda galaxy is approximately 2.537 million light-years from Earth",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store astronomy.txt");

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/cooking.txt",
        b"Sauteing mushrooms in garlic butter creates a wonderful umami flavor",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store cooking.txt");

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/programming.txt",
        b"Rust borrow checker ensures memory safety without garbage collection",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store programming.txt");

    use aeordb::engine::query_engine::{
        FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy, ExplainMode,
    };
    let qe = QueryEngine::new(&engine);

    // Helper to run a Contains query
    let run_contains = |text: &str| -> Vec<String> {
        let query = Query {
            path: "/docs/".to_string(),
            field_queries: vec![],
            node: Some(QueryNode::Field(FieldQuery {
                field_name: "text".to_string(),
                operation: QueryOp::Contains(text.to_string()),
            })),
            limit: None,
            offset: None,
            order_by: Vec::new(),
            after: None,
            before: None,
            include_total: false,
            strategy: QueryStrategy::Full,
            aggregate: None,
            explain: ExplainMode::Off,
        };
        qe.execute(&query).expect("contains query")
            .iter()
            .map(|r| r.file_record.path.clone())
            .collect()
    };

    // Each unique substring should only match its file
    let astro_results = run_contains("Andromeda galaxy");
    assert!(
        astro_results.iter().any(|p| p.contains("astronomy.txt")),
        "Contains('Andromeda galaxy') should find astronomy.txt, got: {:?}",
        astro_results
    );
    assert!(
        !astro_results.iter().any(|p| p.contains("cooking.txt") || p.contains("programming.txt")),
        "Contains('Andromeda galaxy') should NOT find other files, got: {:?}",
        astro_results
    );

    let cook_results = run_contains("garlic butter");
    assert!(
        cook_results.iter().any(|p| p.contains("cooking.txt")),
        "Contains('garlic butter') should find cooking.txt, got: {:?}",
        cook_results
    );
    assert!(
        !cook_results.iter().any(|p| p.contains("astronomy.txt") || p.contains("programming.txt")),
        "Contains('garlic butter') should NOT find other files, got: {:?}",
        cook_results
    );

    let prog_results = run_contains("borrow checker");
    assert!(
        prog_results.iter().any(|p| p.contains("programming.txt")),
        "Contains('borrow checker') should find programming.txt, got: {:?}",
        prog_results
    );
    assert!(
        !prog_results.iter().any(|p| p.contains("astronomy.txt") || p.contains("cooking.txt")),
        "Contains('borrow checker') should NOT find other files, got: {:?}",
        prog_results
    );
}
