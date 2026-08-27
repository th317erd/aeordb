use crate::engine::errors::{EngineError, EngineResult};
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};

struct TopLevelFieldVisitor<'field> {
  field_name: &'field str,
}

impl<'de> Visitor<'de> for TopLevelFieldVisitor<'_> {
  type Value = Option<Vec<u8>>;

  fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("a valid JSON value")
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut selected_value = None;
    while let Some(key) = map.next_key::<String>()? {
      if key == self.field_name {
        selected_value = Some(json_value_to_bytes(&map.next_value::<serde_json::Value>()?));
      } else {
        map.next_value::<IgnoredAny>()?;
      }
    }
    Ok(selected_value)
  }

  fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    Ok(None)
  }

  fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    Ok(None)
  }

  fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    Ok(None)
  }

  fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    Ok(None)
  }

  fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    Ok(None)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    Ok(None)
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    Ok(None)
  }

  fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
  where
    A: serde::de::SeqAccess<'de>,
  {
    while sequence.next_element::<IgnoredAny>()?.is_some() {}
    Ok(None)
  }
}

/// Extract one top-level JSON field without retaining unrelated values.
///
/// Duplicate keys preserve serde_json's effective last-value behavior. Valid
/// non-object roots return `None`, matching path resolution against a scalar or
/// array document.
pub fn parse_json_top_level_field(data: &[u8], field_name: &str) -> EngineResult<Option<Vec<u8>>> {
  let mut deserializer = serde_json::Deserializer::from_slice(data);
  let value = deserializer
    .deserialize_any(TopLevelFieldVisitor { field_name })
    .map_err(|error| EngineError::JsonParseError(format!("Invalid JSON: {error}")))?;
  deserializer.end().map_err(|error| EngineError::JsonParseError(format!("Invalid JSON trailing data: {error}")))?;
  Ok(value)
}

/// Parse JSON data and extract specified fields.
/// Returns (field_name, field_value_as_bytes) pairs.
/// For numbers, serializes as big-endian bytes. For strings, returns UTF-8 bytes.
/// Fields not found in the JSON are silently skipped.
pub fn parse_json_fields(data: &[u8], field_names: &[&str]) -> EngineResult<Vec<(String, Vec<u8>)>> {
  let text = std::str::from_utf8(data).map_err(|error| EngineError::JsonParseError(format!("Invalid UTF-8: {}", error)))?;

  let parsed: serde_json::Value =
    serde_json::from_str(text).map_err(|error| EngineError::JsonParseError(format!("Invalid JSON: {}", error)))?;

  let object = match parsed.as_object() {
    Some(object) => object,
    None => {
      return Err(EngineError::JsonParseError("JSON root is not an object".to_string()));
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
pub fn json_value_to_bytes(value: &serde_json::Value) -> Vec<u8> {
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
