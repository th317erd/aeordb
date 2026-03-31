use crate::engine::errors::{EngineError, EngineResult};

/// Parse JSON data and extract specified fields.
/// Returns (field_name, field_value_as_bytes) pairs.
/// For numbers, serializes as big-endian bytes. For strings, returns UTF-8 bytes.
/// Fields not found in the JSON are silently skipped.
pub fn parse_json_fields(data: &[u8], field_names: &[&str]) -> EngineResult<Vec<(String, Vec<u8>)>> {
  let text = std::str::from_utf8(data).map_err(|error| {
    EngineError::JsonParseError(format!("Invalid UTF-8: {}", error))
  })?;

  let parsed: serde_json::Value = serde_json::from_str(text).map_err(|error| {
    EngineError::JsonParseError(format!("Invalid JSON: {}", error))
  })?;

  let object = match parsed.as_object() {
    Some(object) => object,
    None => {
      return Err(EngineError::JsonParseError(
        "JSON root is not an object".to_string(),
      ));
    }
  };

  let mut results = Vec::new();

  for field_name in field_names {
    let value = match object.get(*field_name) {
      Some(value) => value,
      None => continue, // skip missing fields
    };

    let bytes = json_value_to_bytes(value);
    results.push((field_name.to_string(), bytes));
  }

  Ok(results)
}

/// Convert a JSON value to bytes suitable for scalar conversion.
/// - Integers (u64): 8 bytes big-endian u64
/// - Integers (i64): 8 bytes big-endian i64
/// - Floats: 8 bytes big-endian f64
/// - Strings: UTF-8 bytes
/// - Booleans: 1 byte (0 or 1)
/// - Null: empty vec
fn json_value_to_bytes(value: &serde_json::Value) -> Vec<u8> {
  match value {
    serde_json::Value::Number(number) => {
      if let Some(unsigned) = number.as_u64() {
        unsigned.to_be_bytes().to_vec()
      } else if let Some(signed) = number.as_i64() {
        signed.to_be_bytes().to_vec()
      } else if let Some(float) = number.as_f64() {
        float.to_be_bytes().to_vec()
      } else {
        Vec::new()
      }
    }
    serde_json::Value::String(string) => string.as_bytes().to_vec(),
    serde_json::Value::Bool(boolean) => {
      vec![if *boolean { 1 } else { 0 }]
    }
    serde_json::Value::Null => Vec::new(),
    // Arrays and objects: serialize as JSON string bytes
    other => other.to_string().into_bytes(),
  }
}
