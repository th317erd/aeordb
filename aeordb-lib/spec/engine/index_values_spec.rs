// Tests for FieldIndex value storage and fuzzy query recheck using index-stored values.
// Verifies that field values are stored in the index's values map during insert_expanded,
// persisted through serialize/deserialize, and used by the fuzzy query recheck phase
// instead of relying on a .parsed/ file cache.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_store::{FieldIndex, IndexManager};
use aeordb::engine::query_engine::{
    FieldQuery, Query, QueryEngine, QueryNode, QueryOp, QueryStrategy,
};
use aeordb::engine::scalar_converter::{StringConverter, TrigramConverter};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;
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

/// Helper: set up engine + parser + config with trigram index on a field.
fn setup_with_trigram_config(
    field_name: &str,
    source: &str,
) -> (Arc<StorageEngine>, tempfile::TempDir, PluginManager) {
  let ctx = RequestContext::system();
    let (engine, temp) = create_test_engine();
    let pm = deploy_plaintext_parser(&engine);
    let ops = DirectoryOps::new(&engine);

    let config = format!(
        r#"{{"parser":"plaintext-parser","indexes":[{{"name":"{}","source":{},"type":"trigram"}}]}}"#,
        field_name, source
    );
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    (engine, temp, pm)
}

// ============================================================
// 1. test_field_index_stores_value
// insert_expanded stores value in values map
// ============================================================

#[test]
fn test_field_index_stores_value() {
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let value = b"Hello World";
    let file_hash = vec![1, 2, 3, 4];

    index.insert_expanded(value, file_hash.clone());

    assert!(
        index.values.contains_key(&file_hash),
        "values map should contain the file hash after insert_expanded"
    );
    assert_eq!(
        index.values.get(&file_hash).unwrap().as_slice(),
        value,
        "stored value should match the original value"
    );
}

// ============================================================
// 2. test_field_index_get_value
// get_value retrieves stored value
// ============================================================

#[test]
fn test_field_index_get_value() {
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let value = b"Test data for lookup";
    let file_hash = vec![10, 20, 30, 40];

    index.insert_expanded(value, file_hash.clone());

    let retrieved = index.get_value(&file_hash);
    assert!(retrieved.is_some(), "get_value should return Some for existing hash");
    assert_eq!(
        retrieved.unwrap(),
        value,
        "get_value should return the original value bytes"
    );

    // Non-existent hash should return None
    let missing = index.get_value(&[99, 99, 99, 99]);
    assert!(missing.is_none(), "get_value should return None for non-existent hash");
}

// ============================================================
// 3. test_field_index_remove_clears_value
// remove clears from values map
// ============================================================

#[test]
fn test_field_index_remove_clears_value() {
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let value = b"Data to be removed";
    let file_hash = vec![5, 6, 7, 8];

    index.insert_expanded(value, file_hash.clone());
    assert!(index.values.contains_key(&file_hash), "precondition: value should exist");

    index.remove(&file_hash);

    assert!(
        !index.values.contains_key(&file_hash),
        "values map should not contain the file hash after remove"
    );
    assert!(
        index.get_value(&file_hash).is_none(),
        "get_value should return None after remove"
    );
    assert!(index.entries.is_empty(), "entries should be empty after remove");
}

// ============================================================
// 4. test_field_index_serialize_deserialize_with_values
// round-trip preserves values
// ============================================================

#[test]
fn test_field_index_serialize_deserialize_with_values() {
    let hash_length = 32;
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let value1 = b"First document content";
    let file_hash1 = vec![0u8; hash_length];

    let value2 = b"Second document content";
    let mut file_hash2 = vec![0u8; hash_length];
    file_hash2[0] = 1;

    index.insert_expanded(value1, file_hash1.clone());
    index.insert_expanded(value2, file_hash2.clone());

    // Serialize
    let serialized = index.serialize(hash_length);

    // Deserialize
    let deserialized = FieldIndex::deserialize(&serialized, hash_length)
        .expect("deserialization should succeed");

    // Verify values survived round-trip
    assert_eq!(deserialized.values.len(), 2, "should have 2 values after round-trip");

    let v1 = deserialized.get_value(&file_hash1);
    assert!(v1.is_some(), "value for hash1 should exist after round-trip");
    assert_eq!(v1.unwrap(), value1, "value for hash1 should match");

    let v2 = deserialized.get_value(&file_hash2);
    assert!(v2.is_some(), "value for hash2 should exist after round-trip");
    assert_eq!(v2.unwrap(), value2, "value for hash2 should match");

    // Verify entries also survived
    assert_eq!(
        deserialized.entries.len(),
        index.entries.len(),
        "entry count should match after round-trip"
    );
}

// ============================================================
// 5. test_field_index_backward_compat_no_values
// deserialize old format (no values section) works
// ============================================================

#[test]
fn test_field_index_backward_compat_no_values() {
    let hash_length = 32;
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let value = b"Some data";
    let file_hash = vec![0u8; hash_length];
    index.insert_expanded(value, file_hash.clone());

    // Serialize the full format
    let _full_serialized = index.serialize(hash_length);

    // Manually truncate to remove the values section (last part of serialized data).
    // The values section starts after all entries. We need to find where it is.
    // Strategy: serialize with no values to get the "old format" size.
    let old_format_index = {
        let converter2 = Box::new(TrigramConverter);
        let mut idx = FieldIndex::new("content".to_string(), converter2);
        // Insert same entries but then clear values to simulate old format
        idx.insert_expanded(value, file_hash.clone());
        let mut serialized = idx.serialize(hash_length);
        // The values section is: 4 bytes (count) + entries.
        // For 1 value with hash_length=32 and value="Some data" (9 bytes):
        // 4 + 32 + 4 + 9 = 49 bytes at the end.
        // Truncate to remove the values section entirely.
        let values_section_size = 4 + hash_length + 4 + value.len();
        serialized.truncate(serialized.len() - values_section_size);
        serialized
    };

    // Deserialize the truncated (old format) data
    let deserialized = FieldIndex::deserialize(&old_format_index, hash_length)
        .expect("old format deserialization should succeed");

    // Values map should be empty (no values section in old format)
    assert!(
        deserialized.values.is_empty(),
        "old format should deserialize with empty values map"
    );

    // But entries should still be there
    assert!(
        !deserialized.entries.is_empty(),
        "entries should still be present in old format"
    );
}

// ============================================================
// 6. test_fuzzy_contains_with_parser
// deploy parser, store text, Contains query finds it
// ============================================================

#[test]
fn test_fuzzy_contains_with_parser() {
  let ctx = RequestContext::system();
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/greeting.txt",
        b"Hello World this is a greeting document",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Contains("Hello".to_string()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("contains query");
    assert!(
        !results.is_empty(),
        "Contains('Hello') should find the text file via index values, got 0 results"
    );
    assert!(
        results.iter().any(|r| r.file_record.path.contains("greeting.txt")),
        "should find greeting.txt, got: {:?}",
        results.iter().map(|r| &r.file_record.path).collect::<Vec<_>>()
    );
}

// ============================================================
// 7. test_fuzzy_similar_with_parser
// deploy parser, store text, Similar query with score
// ============================================================

#[test]
fn test_fuzzy_similar_with_parser() {
  let ctx = RequestContext::system();
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/greeting.txt",
        b"Hello World this is a greeting document",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    let qe = QueryEngine::new(&engine);

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
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("similar query");
    assert!(
        !results.is_empty(),
        "Similar query should find the text file via index values, got 0 results"
    );
    assert!(
        results[0].score > 0.0,
        "score should be positive, got {}",
        results[0].score
    );
    assert!(
        results.iter().any(|r| r.file_record.path.contains("greeting.txt")),
        "should find greeting.txt"
    );
}

// ============================================================
// 8. test_no_parsed_cache_created
// verify .parsed/ directory does NOT exist after storing
// ============================================================

#[test]
fn test_no_parsed_cache_created() {
  let ctx = RequestContext::system();
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/hello.txt",
        b"Hello World",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    // Verify .parsed/ cache was NOT created
    let parsed = ops.read_file("/docs/.parsed/hello.txt.json");
    assert!(
        parsed.is_err(),
        ".parsed/ cache should NOT exist — values are stored in the index now"
    );
}

// ============================================================
// 9. test_json_file_fuzzy_still_works
// JSON file without parser, Contains still works via fallback
// ============================================================

#[test]
fn test_json_file_fuzzy_still_works() {
  let ctx = RequestContext::system();
    let (engine, _temp) = create_test_engine();
    let ops = DirectoryOps::new(&engine);

    // Config with trigram index but NO parser (expects JSON data)
    let config = r#"{"indexes":[{"name":"name","type":"trigram"}]}"#;
    ops.store_file(&ctx,
        "/docs/.config/indexes.json",
        config.as_bytes(),
        Some("application/json"),
    )
    .expect("store config");

    // Store a JSON file — should index the name field directly
    let json_data = r#"{"name":"Alexander Hamilton"}"#;
    ops.store_file_with_indexing(&ctx,
        "/docs/person.json",
        json_data.as_bytes(),
        Some("application/json"),
    )
    .expect("store json file");

    let qe = QueryEngine::new(&engine);

    // Contains query should work — value is stored in index
    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "name".to_string(),
            operation: QueryOp::Contains("Alexander".to_string()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
    };

    let results = qe.execute(&query).expect("contains query on JSON");
    assert!(
        !results.is_empty(),
        "Contains('Alexander') should work for native JSON via index values"
    );
    assert!(
        results.iter().any(|r| r.file_record.path.contains("person.json")),
        "should find person.json"
    );
}

// ============================================================
// 10. test_multiple_files_values_independent
// two files, each has its own value in index
// ============================================================

#[test]
fn test_multiple_files_values_independent() {
  let ctx = RequestContext::system();
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/alpha.txt",
        b"Alpha content here for testing",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store alpha");

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/beta.txt",
        b"Beta content here for testing",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store beta");

    // Load the trigram index and verify both files have independent values
    let index_manager = IndexManager::new(&engine);
    let index = index_manager
        .load_index_by_strategy("/docs/", "text", "trigram")
        .expect("load index")
        .expect("trigram index should exist");

    assert!(
        index.values.len() >= 2,
        "should have at least 2 values in index, got {}",
        index.values.len()
    );

    // Verify the values are different
    let values: Vec<&Vec<u8>> = index.values.values().collect();
    let has_alpha = values.iter().any(|v| {
        let s = String::from_utf8_lossy(v);
        s.contains("Alpha")
    });
    let has_beta = values.iter().any(|v| {
        let s = String::from_utf8_lossy(v);
        s.contains("Beta")
    });
    assert!(has_alpha, "should have Alpha value in index");
    assert!(has_beta, "should have Beta value in index");

    // Verify no .parsed/ cache was created
    assert!(
        ops.read_file("/docs/.parsed/alpha.txt.json").is_err(),
        ".parsed/ should not exist for alpha"
    );
    assert!(
        ops.read_file("/docs/.parsed/beta.txt.json").is_err(),
        ".parsed/ should not exist for beta"
    );
}

// ============================================================
// 11. test_contains_query_no_match_returns_empty
// Verify Contains query returns empty when search term is absent
// ============================================================

#[test]
fn test_contains_query_no_match_returns_empty() {
  let ctx = RequestContext::system();
    let (engine, _temp, pm) = setup_with_trigram_config("text", r#"["text"]"#);
    let ops = DirectoryOps::new(&engine);

    ops.store_file_with_full_pipeline(&ctx,
        "/docs/greeting.txt",
        b"Hello World this is a greeting document",
        Some("text/plain"),
        Some(&pm),
    )
    .expect("store text file");

    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/docs/".to_string(),
        field_queries: vec![],
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "text".to_string(),
            operation: QueryOp::Contains("Nonexistent Phrase XYZ".to_string()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
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
// 12. test_field_index_value_overwritten_on_reinsert
// Overwriting a file updates the value in the index
// ============================================================

#[test]
fn test_field_index_value_overwritten_on_reinsert() {
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let file_hash = vec![1, 2, 3, 4];

    // Insert first value
    index.insert_expanded(b"First version", file_hash.clone());
    assert_eq!(
        index.get_value(&file_hash).unwrap(),
        b"First version",
    );

    // Remove and reinsert with new value (simulating file overwrite)
    index.remove(&file_hash);
    index.insert_expanded(b"Second version", file_hash.clone());
    assert_eq!(
        index.get_value(&file_hash).unwrap(),
        b"Second version",
        "value should be updated after remove + reinsert"
    );
}

// ============================================================
// 13. test_field_index_empty_value
// Edge case: empty value bytes
// ============================================================

#[test]
fn test_field_index_empty_value() {
    let hash_length = 32;
    let converter = Box::new(StringConverter::new(256));
    let mut index = FieldIndex::new("field".to_string(), converter);

    let file_hash = vec![0u8; hash_length];
    index.insert_expanded(b"", file_hash.clone());

    assert_eq!(index.get_value(&file_hash).unwrap(), b"");

    // Round-trip
    let serialized = index.serialize(hash_length);
    let deserialized = FieldIndex::deserialize(&serialized, hash_length)
        .expect("deserialization should succeed");
    assert_eq!(deserialized.get_value(&file_hash).unwrap(), b"");
}

// ============================================================
// 14. test_field_index_large_value_roundtrip
// Large value (>64KB) survives serialize/deserialize
// ============================================================

#[test]
fn test_field_index_large_value_roundtrip() {
    let hash_length = 32;
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let large_value = vec![b'x'; 100_000]; // 100KB
    let file_hash = vec![0u8; hash_length];
    index.insert_expanded(&large_value, file_hash.clone());

    let serialized = index.serialize(hash_length);
    let deserialized = FieldIndex::deserialize(&serialized, hash_length)
        .expect("deserialization should succeed");

    assert_eq!(
        deserialized.get_value(&file_hash).unwrap(),
        large_value.as_slice(),
        "large value should survive round-trip"
    );
}

// ============================================================
// 15. test_remove_nonexistent_hash_is_noop
// Remove a hash that doesn't exist — no panic, no change
// ============================================================

#[test]
fn test_remove_nonexistent_hash_is_noop() {
    let converter = Box::new(TrigramConverter);
    let mut index = FieldIndex::new("content".to_string(), converter);

    let file_hash = vec![1, 2, 3, 4];
    index.insert_expanded(b"Some data", file_hash.clone());

    let entry_count_before = index.entries.len();
    let values_count_before = index.values.len();

    // Remove a hash that doesn't exist
    index.remove(&[99, 99, 99, 99]);

    assert_eq!(index.entries.len(), entry_count_before);
    assert_eq!(index.values.len(), values_count_before);
}
