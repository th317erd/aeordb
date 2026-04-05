use aeordb::engine::source_resolver::{resolve_source, walk_path};
use serde_json::json;

#[test]
fn test_simple_string_key() {
  let data = json!({"name": "Alice"});
  let source = vec![json!("name")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"Alice");
}

#[test]
fn test_nested_keys() {
  let data = json!({"metadata": {"title": "Report"}});
  let source = vec![json!("metadata"), json!("title")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"Report");
}

#[test]
fn test_array_index() {
  let data = json!({"items": ["first", "second", "third"]});
  let source = vec![json!("items"), json!(0)];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"first");
}

#[test]
fn test_array_index_middle() {
  let data = json!({"items": ["a", "b", "c"]});
  let source = vec![json!("items"), json!(2)];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"c");
}

#[test]
fn test_integer_as_object_key() {
  let data = json!({"data": {"0": "zero_val", "1": "one_val"}});
  let source = vec![json!("data"), json!(0)];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"zero_val");
}

#[test]
fn test_deep_nesting() {
  let data = json!({"a": {"b": [null, null, {"c": "deep"}]}});
  let source = vec![json!("a"), json!("b"), json!(2), json!("c")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"deep");
}

#[test]
fn test_missing_key_returns_none() {
  let data = json!({"name": "Alice"});
  let source = vec![json!("missing")];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_missing_nested_returns_none() {
  let data = json!({"a": {"b": 1}});
  let source = vec![json!("a"), json!("b"), json!("c")];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_array_out_of_bounds() {
  let data = json!({"items": [1, 2]});
  let source = vec![json!("items"), json!(99)];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_empty_segments_returns_root() {
  let data = json!({"name": "Alice"});
  let source: Vec<serde_json::Value> = vec![];
  let result = walk_path(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), data);
}

#[test]
fn test_literal_dot_in_key() {
  let data = json!({"metadata.title": "Dotted Key"});
  let source = vec![json!("metadata.title")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"Dotted Key");
}

#[test]
fn test_boolean_segment_returns_none() {
  let data = json!({"a": 1});
  let source = vec![json!(true)];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_null_segment_returns_none() {
  let data = json!({"a": 1});
  let source = vec![serde_json::Value::Null];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_number_value_returns_bytes() {
  let data = json!({"count": 42});
  let source = vec![json!("count")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  // json_value_to_bytes converts u64 to big-endian 8 bytes
  assert_eq!(result.unwrap(), 42u64.to_be_bytes().to_vec());
}

#[test]
fn test_nested_array() {
  let data = json!({"matrix": [[10, 20], [30, 40]]});
  let source = vec![json!("matrix"), json!(1), json!(0)];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), 30u64.to_be_bytes().to_vec());
}

#[test]
fn test_length_as_regular_key() {
  let data = json!({"items": {"length": 5}});
  let source = vec![json!("items"), json!("length")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), 5u64.to_be_bytes().to_vec());
}

#[test]
fn test_walk_path_returns_json_value() {
  let data = json!({"a": {"b": [1, 2, 3]}});
  let source = vec![json!("a"), json!("b")];
  let result = walk_path(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), json!([1, 2, 3]));
}

#[test]
fn test_string_with_special_chars() {
  let data = json!({"key with spaces": "value"});
  let source = vec![json!("key with spaces")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"value");
}

#[test]
fn test_empty_string_key() {
  let data = json!({"": "empty_key_value"});
  let source = vec![json!("")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), b"empty_key_value");
}

#[test]
fn test_resolve_bool_value() {
  let data = json!({"active": true});
  let source = vec![json!("active")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), vec![1u8]); // json_value_to_bytes: true → [1]
}

#[test]
fn test_resolve_null_value() {
  let data = json!({"nothing": null});
  let source = vec![json!("nothing")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert!(result.unwrap().is_empty()); // json_value_to_bytes: null → []
}

// --- Additional edge case and failure path tests ---

#[test]
fn test_negative_number_segment_returns_none() {
  // Negative numbers can't convert to u64, so as_u64() returns None
  let data = json!({"items": ["a", "b", "c"]});
  let source = vec![json!("items"), json!(-1)];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_float_segment_returns_none() {
  // Floats can't convert to u64 via as_u64(), returns None
  let data = json!({"items": ["a", "b"]});
  let source = vec![json!("items"), json!(1.5)];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_array_segment_returns_none() {
  let data = json!({"a": 1});
  let source = vec![json!(["invalid"])];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_object_segment_returns_none() {
  let data = json!({"a": 1});
  let source = vec![json!({"invalid": true})];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_index_on_string_value_returns_none() {
  // Trying to index into a string value (not array or object) should fail
  let data = json!({"name": "Alice"});
  let source = vec![json!("name"), json!(0)];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_key_on_number_value_returns_none() {
  // Trying to key into a number value should fail
  let data = json!({"count": 42});
  let source = vec![json!("count"), json!("sub")];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_key_on_null_value_returns_none() {
  let data = json!({"nothing": null});
  let source = vec![json!("nothing"), json!("sub")];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_key_on_bool_value_returns_none() {
  let data = json!({"active": true});
  let source = vec![json!("active"), json!("sub")];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_resolve_float_value() {
  let data = json!({"pi": 3.14});
  let source = vec![json!("pi")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), 3.14f64.to_be_bytes().to_vec());
}

#[test]
fn test_resolve_negative_integer_value() {
  let data = json!({"temp": -10});
  let source = vec![json!("temp")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), (-10i64).to_be_bytes().to_vec());
}

#[test]
fn test_resolve_array_value_as_json_string() {
  // When the resolved value is an array, json_value_to_bytes serializes it as JSON string
  let data = json!({"tags": ["a", "b"]});
  let source = vec![json!("tags")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  let bytes = result.unwrap();
  let text = String::from_utf8(bytes).unwrap();
  assert_eq!(text, r#"["a","b"]"#);
}

#[test]
fn test_resolve_object_value_as_json_string() {
  let data = json!({"meta": {"k": "v"}});
  let source = vec![json!("meta")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  let bytes = result.unwrap();
  let text = String::from_utf8(bytes).unwrap();
  assert_eq!(text, r#"{"k":"v"}"#);
}

#[test]
fn test_resolve_false_value() {
  let data = json!({"active": false});
  let source = vec![json!("active")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), vec![0u8]); // json_value_to_bytes: false → [0]
}

#[test]
fn test_integer_key_not_found_on_object() {
  // Object doesn't have key "5"
  let data = json!({"a": "b"});
  let source = vec![json!(5)];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_deeply_nested_missing() {
  let data = json!({"a": {"b": {"c": {"d": 1}}}});
  let source = vec![json!("a"), json!("b"), json!("c"), json!("e")];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_unicode_key() {
  let data = json!({"名前": "太郎"});
  let source = vec![json!("名前")];
  let result = resolve_source(&data, &source);
  assert!(result.is_some());
  assert_eq!(result.unwrap(), "太郎".as_bytes());
}

#[test]
fn test_empty_array_index_out_of_bounds() {
  let data = json!({"items": []});
  let source = vec![json!("items"), json!(0)];
  assert!(resolve_source(&data, &source).is_none());
}

#[test]
fn test_large_index() {
  let data = json!({"items": [1]});
  let source = vec![json!("items"), json!(u64::MAX)];
  assert!(resolve_source(&data, &source).is_none());
}
