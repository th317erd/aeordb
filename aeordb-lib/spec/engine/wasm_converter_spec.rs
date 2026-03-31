use aeordb::engine::scalar_converter::{ScalarConverter, CONVERTER_TYPE_WASM, deserialize_converter};
use aeordb::engine::wasm_converter::{WasmConverter, WasmBatchConverter};

#[test]
fn test_wasm_converter_name() {
  let converter = WasmConverter::new(
    "my_wasm_plugin".to_string(),
    false,
    vec![0x00, 0x61, 0x73, 0x6D], // fake wasm magic bytes
  );
  assert_eq!(converter.name(), "my_wasm_plugin");
}

#[test]
fn test_wasm_converter_order_preserving_flag() {
  let not_preserving = WasmConverter::new(
    "unordered".to_string(),
    false,
    vec![],
  );
  assert!(!not_preserving.is_order_preserving());

  let preserving = WasmConverter::new(
    "ordered".to_string(),
    true,
    vec![],
  );
  assert!(preserving.is_order_preserving());
}

#[test]
fn test_wasm_converter_type_tag() {
  let converter = WasmConverter::new("test".to_string(), false, vec![]);
  assert_eq!(converter.type_tag(), CONVERTER_TYPE_WASM);
}

#[test]
fn test_wasm_converter_serialize_deserialize() {
  let wasm_bytes = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
  let original = WasmConverter::new(
    "geo_converter".to_string(),
    true,
    wasm_bytes.clone(),
  );

  let serialized = ScalarConverter::serialize(&original);
  let deserialized = WasmConverter::deserialize(&serialized).unwrap();

  assert_eq!(deserialized.name(), "geo_converter");
  assert!(deserialized.is_order_preserving());
  assert_eq!(deserialized.wasm_bytes(), &wasm_bytes);
}

#[test]
fn test_wasm_converter_deserialize_via_global_deserializer() {
  let wasm_bytes = vec![0xDE, 0xAD, 0xBE, 0xEF];
  let original = WasmConverter::new(
    "test_plugin".to_string(),
    false,
    wasm_bytes,
  );

  let serialized = ScalarConverter::serialize(&original);
  let deserialized = deserialize_converter(&serialized).unwrap();

  assert_eq!(deserialized.name(), "test_plugin");
  assert!(!deserialized.is_order_preserving());
  assert_eq!(deserialized.type_tag(), CONVERTER_TYPE_WASM);
}

#[test]
fn test_wasm_converter_stub_returns_half() {
  let converter = WasmConverter::new("stub".to_string(), false, vec![]);
  assert_eq!(converter.to_scalar(b"any data"), 0.5);
  assert_eq!(converter.to_scalar(&[]), 0.5);
  assert_eq!(converter.to_scalar(&[1, 2, 3, 4, 5, 6, 7, 8]), 0.5);
}

#[test]
fn test_wasm_converter_empty_wasm_bytes() {
  let converter = WasmConverter::new("empty".to_string(), true, vec![]);
  let serialized = ScalarConverter::serialize(&converter);
  let deserialized = WasmConverter::deserialize(&serialized).unwrap();
  assert_eq!(deserialized.wasm_bytes(), &[] as &[u8]);
  assert_eq!(deserialized.name(), "empty");
  assert!(deserialized.is_order_preserving());
}

#[test]
fn test_wasm_converter_deserialize_invalid_tag() {
  let result = WasmConverter::deserialize(&[0xFF, 0x00]);
  assert!(result.is_err());
}

#[test]
fn test_wasm_converter_deserialize_empty_data() {
  let result = WasmConverter::deserialize(&[]);
  assert!(result.is_err());
}

#[test]
fn test_wasm_converter_deserialize_truncated_after_tag() {
  let result = WasmConverter::deserialize(&[CONVERTER_TYPE_WASM]);
  assert!(result.is_err());
}

#[test]
fn test_wasm_converter_deserialize_truncated_name_length() {
  // type_tag + order_preserving but missing name length
  let result = WasmConverter::deserialize(&[CONVERTER_TYPE_WASM, 0x01]);
  assert!(result.is_err());
}

#[test]
fn test_wasm_converter_deserialize_truncated_name() {
  // type_tag + order_preserving + name_len=10 but only 2 bytes of name
  let data = vec![CONVERTER_TYPE_WASM, 0x00, 0x0A, 0x00, b'a', b'b'];
  let result = WasmConverter::deserialize(&data);
  assert!(result.is_err());
}

#[test]
fn test_wasm_batch_converter_struct() {
  let wasm_bytes = vec![0x00, 0x61, 0x73, 0x6D];
  let batch = WasmBatchConverter::new(
    "batch_plugin".to_string(),
    true,
    wasm_bytes.clone(),
  );

  assert_eq!(batch.name(), "batch_plugin");
  assert!(batch.is_order_preserving());
  assert_eq!(batch.wasm_bytes(), &wasm_bytes);
}

#[test]
fn test_wasm_batch_converter_convert_batch_stub() {
  let batch = WasmBatchConverter::new("stub".to_string(), false, vec![]);

  let values: Vec<&[u8]> = vec![b"hello", b"world", b"test"];
  let results = batch.convert_batch(&values).unwrap();

  assert_eq!(results.len(), 3);
  assert!(results.iter().all(|v| *v == 0.5));
}

#[test]
fn test_wasm_batch_converter_empty_batch() {
  let batch = WasmBatchConverter::new("stub".to_string(), false, vec![]);

  let values: Vec<&[u8]> = vec![];
  let results = batch.convert_batch(&values).unwrap();

  assert!(results.is_empty());
}

#[test]
fn test_wasm_batch_converter_single_item() {
  let batch = WasmBatchConverter::new("single".to_string(), true, vec![]);

  let values: Vec<&[u8]> = vec![b"only one"];
  let results = batch.convert_batch(&values).unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0], 0.5);
}

#[test]
fn test_wasm_converter_large_name() {
  let name = "a".repeat(1000);
  let converter = WasmConverter::new(name.clone(), false, vec![0x42]);
  let serialized = ScalarConverter::serialize(&converter);
  let deserialized = WasmConverter::deserialize(&serialized).unwrap();
  assert_eq!(deserialized.name(), name);
}

#[test]
fn test_wasm_converter_large_wasm_bytes() {
  let wasm_bytes = vec![0xAB; 10_000];
  let converter = WasmConverter::new("big".to_string(), true, wasm_bytes.clone());
  let serialized = ScalarConverter::serialize(&converter);
  let deserialized = WasmConverter::deserialize(&serialized).unwrap();
  assert_eq!(deserialized.wasm_bytes(), &wasm_bytes);
}
