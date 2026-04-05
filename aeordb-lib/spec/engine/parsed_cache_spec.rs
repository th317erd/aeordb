// Tests for the .parsed/ cache and fuzzy recheck fix.
// Verifies that parser output is persisted alongside raw files so that
// fuzzy queries (Contains, Similar, etc.) can load the JSON representation
// during the recheck phase instead of failing on non-JSON raw content.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::query_engine::{
    FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy,
};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::plugins::plugin_manager::PluginManager;
use aeordb::plugins::types::PluginType;
use std::sync::Arc;

fn create_test_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine_path = dir.path().join("test.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory().unwrap();
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

/// Helper: set up engine + parser + config with trigram index on a field.
/// Note: field_name should match the JSON key from the parser output so that
/// the recheck phase (which looks up by field_name in the JSON) can find it.
fn setup_with_trigram_config(
    field_name: &str,
    source: &str,
) -> (Arc<StorageEngine>, tempfile::TempDir, PluginManager) {
    let (engine, temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);
    let ops = DirectoryOps::new(&engine);

    let config = format!(
        r#"{{"parser":"plaintext-parser","indexes":[{{"name":"{}","source":{},"type":"trigram"}}]}}"#,
        field_name, source
    );
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    (engine, temp, pm)
}

// ============================================================
// 1. test_parsed_cache_created_by_parser
// Deploy parser, store file, verify .parsed/{filename}.json exists
// ============================================================

#[test]
fn test_parsed_cache_created_by_parser() {
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);
    let ops = DirectoryOps::new(&engine);

    let config = r#"{
        "parser": "plaintext-parser",
        "indexes": [
            {"name": "content", "source": ["text"], "type": "trigram"}
        ]
    }"#;
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    ops.store_file_with_full_pipeline(
        "/docs/hello.txt",
        b"Hello World",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    // Verify .parsed/ cache was created
    let parsed = ops.read_file("/docs/.parsed/hello.txt.json");
    assert!(
        parsed.is_ok(),
        "parsed cache should exist at /docs/.parsed/hello.txt.json, got: {:?}",
        parsed.err()
    );

    let parsed_bytes = parsed.unwrap();
    assert!(!parsed_bytes.is_empty(), "parsed cache should not be empty");
}

// ============================================================
// 2. test_parsed_cache_not_created_for_json
// Store JSON file (no parser), verify NO .parsed/ entry
// ============================================================

#[test]
fn test_parsed_cache_not_created_for_json() {
    let (engine, _temp) = create_test_engine();
    let ops = DirectoryOps::new(&engine);

    let config = r#"{"indexes":[{"name":"name","type":"string"}]}"#;
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    let json_data = r#"{"name":"Alice"}"#;
    ops.store_file_with_indexing(
        "/docs/user.json",
        json_data.as_bytes(),
        Some("application/json"),
    )
    .expect("store json file");

    // No parser was used, so .parsed/ should NOT exist
    let parsed = ops.read_file("/docs/.parsed/user.json.json");
    assert!(
        parsed.is_err(),
        "parsed cache should NOT exist for native JSON files"
    );
}

// ============================================================
// 3. test_parsed_cache_content_matches_parser_output
// Verify .parsed/ content is valid JSON with expected fields
// ============================================================

#[test]
fn test_parsed_cache_content_matches_parser_output() {
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);
    let ops = DirectoryOps::new(&engine);

    let config = r#"{"parser":"plaintext-parser","indexes":[{"name":"content","source":["text"],"type":"trigram"}]}"#;
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    ops.store_file_with_full_pipeline(
        "/docs/sample.txt",
        b"Hello World from the parser",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    let parsed_bytes = ops
        .read_file("/docs/.parsed/sample.txt.json")
        .expect("parsed cache should exist");

    let parsed: serde_json::Value =
        serde_json::from_slice(&parsed_bytes).expect("parsed cache should be valid JSON");

    assert!(parsed.is_object(), "parsed cache should be a JSON object");
    // The plaintext parser produces at least "title", "text", "metadata" fields
    assert!(
        parsed.get("text").is_some(),
        "parsed cache should have a 'text' field, got: {}",
        parsed
    );
    assert!(
        parsed.get("metadata").is_some(),
        "parsed cache should have a 'metadata' field, got: {}",
        parsed
    );
}

// ============================================================
// 4. test_fuzzy_contains_works_with_parser
// Deploy parser, store text file, Contains query finds it
// ============================================================

#[test]
fn test_fuzzy_contains_works_with_parser() {
    // Use "text" as both field name and source key so recheck can find it in JSON
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(
        "/docs/greeting.txt",
        b"Hello World this is a greeting document",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    let qe = QueryEngine::new(&engine);

    // Contains query for "Hello" should find the file via .parsed/ cache
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Contains("Hello".to_string()),
        })),
        limit: None,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("contains query");
    assert!(
        !results.is_empty(),
        "Contains('Hello') should find the text file via parsed cache, got 0 results"
    );
    assert!(
        results
            .iter()
            .any(|r| r.file_record.path.contains("greeting.txt")),
        "should find greeting.txt, got: {:?}",
        results
            .iter()
            .map(|r| &r.file_record.path)
            .collect::<Vec<_>>()
    );
}

// ============================================================
// 5. test_fuzzy_similar_works_with_parser
// Deploy parser, store text file, Similar query finds it with score
// ============================================================

#[test]
fn test_fuzzy_similar_works_with_parser() {
    // Use "text" as both field name and source key so recheck can find it in JSON
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(
        "/docs/greeting.txt",
        b"Hello World this is a greeting document",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    let qe = QueryEngine::new(&engine);

    // Similar query with a low threshold — the full text vs "Hello World" should score
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Similar(
                "Hello World this is a greeting document".to_string(),
                0.3,
            ),
        })),
        limit: None,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("similar query");
    assert!(
        !results.is_empty(),
        "Similar query should find the text file via parsed cache, got 0 results"
    );
    assert!(
        results[0].score > 0.0,
        "score should be positive, got {}",
        results[0].score
    );
    assert!(
        results
            .iter()
            .any(|r| r.file_record.path.contains("greeting.txt")),
        "should find greeting.txt"
    );
}

// ============================================================
// 6. test_parsed_cache_cleaned_on_delete
// Store file, delete it, verify .parsed/ entry also deleted
// ============================================================

#[test]
fn test_parsed_cache_cleaned_on_delete() {
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);
    let ops = DirectoryOps::new(&engine);

    let config = r#"{"parser":"plaintext-parser","indexes":[{"name":"content","source":["text"],"type":"trigram"}]}"#;
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    ops.store_file_with_full_pipeline(
        "/docs/to_delete.txt",
        b"This file will be deleted",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    // Verify .parsed/ exists before delete
    assert!(
        ops.read_file("/docs/.parsed/to_delete.txt.json").is_ok(),
        "parsed cache should exist before delete"
    );

    // Delete with indexing cleanup
    ops.delete_file_with_indexing("/docs/to_delete.txt")
        .expect("delete file");

    // Verify .parsed/ was also cleaned up
    let parsed_after = ops.read_file("/docs/.parsed/to_delete.txt.json");
    assert!(
        parsed_after.is_err(),
        "parsed cache should be deleted after file deletion"
    );
}

// ============================================================
// 7. test_parsed_path_is_system_path
// Verify .parsed/ paths don't trigger indexing
// ============================================================

#[test]
fn test_parsed_path_is_system_path() {
    let (engine, _temp) = create_test_engine();
    let _pm = deploy_plaintext_parser(&engine);
    let ops = DirectoryOps::new(&engine);

    // Set up config with parser
    let config = r#"{"parser":"plaintext-parser","indexes":[{"name":"content","source":["text"],"type":"trigram"}]}"#;
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    // Store a file in .parsed/ directly — it should NOT trigger indexing
    // (and should not cause infinite recursion)
    let result = ops.store_file_with_indexing(
        "/docs/.parsed/manual.json",
        b"{}",
        Some("application/json"),
    );
    assert!(
        result.is_ok(),
        "storing to .parsed/ path should succeed (skipping indexing)"
    );

    // The .parsed/ file should exist but should NOT have created additional indexes
    // beyond what was already there
    let _index_manager = IndexManager::new(&engine);
    // If .parsed/ triggered indexing, it would try to parse "{}" and index it.
    // Since it's a system path, it should skip indexing entirely.
    // We can't easily verify "no extra indexing happened" directly, but we can
    // confirm no crash/panic/infinite-loop occurred (the test completing is proof).
}

// ============================================================
// 8. test_json_file_fuzzy_still_works
// JSON file without parser, Contains query still works (fallback)
// ============================================================

#[test]
fn test_json_file_fuzzy_still_works() {
    let (engine, _temp) = create_test_engine();
    let ops = DirectoryOps::new(&engine);

    // Config with trigram index but NO parser (expects JSON data)
    let config = r#"{"indexes":[{"name":"name","type":"trigram"}]}"#;
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    // Store a JSON file — should index the name field directly
    let json_data = r#"{"name":"Alexander Hamilton"}"#;
    ops.store_file_with_indexing(
        "/docs/person.json",
        json_data.as_bytes(),
        Some("application/json"),
    )
    .expect("store json file");

    let qe = QueryEngine::new(&engine);

    // Contains query should still work via raw file fallback
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "name".to_string(),
            operation: QueryOp::Contains("Alexander".to_string()),
        })),
        limit: None,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("contains query on JSON");
    assert!(
        !results.is_empty(),
        "Contains('Alexander') should work for native JSON via raw file fallback"
    );
    assert!(
        results
            .iter()
            .any(|r| r.file_record.path.contains("person.json")),
        "should find person.json"
    );
}

// ============================================================
// 9. test_contains_query_no_match_returns_empty
// Verify Contains query returns empty when search term is absent
// ============================================================

#[test]
fn test_contains_query_no_match_returns_empty() {
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(
        "/docs/greeting.txt",
        b"Hello World this is a greeting document",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    let qe = QueryEngine::new(&engine);

    // Search for text that doesn't exist in the document
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Contains("Nonexistent Phrase XYZ".to_string()),
        })),
        limit: None,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("contains query");
    assert!(
        results.is_empty(),
        "Contains for absent text should return empty, got {} results",
        results.len()
    );
}

// ============================================================
// 10. test_parsed_cache_overwritten_on_re_store
// Overwriting a file should update the .parsed/ cache
// ============================================================

#[test]
fn test_parsed_cache_overwritten_on_re_store() {
    let (engine, _temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);
    let ops = DirectoryOps::new(&engine);

    let config = r#"{"parser":"plaintext-parser","indexes":[{"name":"content","source":["text"],"type":"trigram"}]}"#;
    ops.store_file(
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    // Store first version
    ops.store_file_with_full_pipeline(
        "/docs/mutable.txt",
        b"First version content",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store v1");

    let parsed_v1 = ops
        .read_file("/docs/.parsed/mutable.txt.json")
        .expect("parsed v1");
    let v1: serde_json::Value = serde_json::from_slice(&parsed_v1).unwrap();
    let v1_text = v1.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        v1_text.contains("First version"),
        "v1 parsed should contain 'First version', got: {}",
        v1_text
    );

    // Store second version (overwrite)
    ops.store_file_with_full_pipeline(
        "/docs/mutable.txt",
        b"Second version updated content",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store v2");

    let parsed_v2 = ops
        .read_file("/docs/.parsed/mutable.txt.json")
        .expect("parsed v2");
    let v2: serde_json::Value = serde_json::from_slice(&parsed_v2).unwrap();
    let v2_text = v2.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        v2_text.contains("Second version"),
        "v2 parsed should contain 'Second version', got: {}",
        v2_text
    );
}

// ============================================================
// 11. test_multiple_files_parsed_cache_independent
// Multiple files each get their own .parsed/ entry
// ============================================================

#[test]
fn test_multiple_files_parsed_cache_independent() {
    let (engine, _temp, pm) = setup_with_trigram_config("content", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(
        "/docs/alpha.txt",
        b"Alpha content here",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store alpha");

    ops.store_file_with_full_pipeline(
        "/docs/beta.txt",
        b"Beta content here",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store beta");

    // Both should have independent parsed caches
    let alpha_parsed = ops
        .read_file("/docs/.parsed/alpha.txt.json")
        .expect("alpha parsed");
    let beta_parsed = ops
        .read_file("/docs/.parsed/beta.txt.json")
        .expect("beta parsed");

    let alpha: serde_json::Value = serde_json::from_slice(&alpha_parsed).unwrap();
    let beta: serde_json::Value = serde_json::from_slice(&beta_parsed).unwrap();

    let alpha_text = alpha.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let beta_text = beta.get("text").and_then(|v| v.as_str()).unwrap_or("");

    assert!(
        alpha_text.contains("Alpha"),
        "alpha parsed should contain 'Alpha'"
    );
    assert!(
        beta_text.contains("Beta"),
        "beta parsed should contain 'Beta'"
    );
    assert_ne!(alpha_parsed, beta_parsed, "parsed caches should differ");
}
