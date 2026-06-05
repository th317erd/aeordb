//! JSON helpers for plugin authors.
//!
//! The SDK re-exports `serde_json`, but these helpers keep common plugin
//! request parsing, response serialization, and recursive object merging
//! consistent across first-party and user plugins.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{PluginError, PluginRequest};

pub use serde_json::{json, Map, Number, Value};

/// Parse raw JSON bytes into a typed value.
pub fn parse_bytes<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, PluginError> {
  serde_json::from_slice(bytes).map_err(|error| PluginError::SerializationFailed(format!("failed to parse JSON bytes: {}", error)))
}

/// Parse a JSON string into a typed value.
pub fn parse_str<T: DeserializeOwned>(text: &str) -> Result<T, PluginError> {
  serde_json::from_str(text).map_err(|error| PluginError::SerializationFailed(format!("failed to parse JSON string: {}", error)))
}

/// Parse a query plugin request body into a typed value.
pub fn parse_request<T: DeserializeOwned>(request: &PluginRequest) -> Result<T, PluginError> {
  parse_bytes(&request.arguments)
}

/// Convert a serializable value into a JSON value.
pub fn to_value<T: Serialize>(value: &T) -> Result<Value, PluginError> {
  serde_json::to_value(value).map_err(|error| PluginError::SerializationFailed(format!("failed to serialize JSON value: {}", error)))
}

/// Serialize a value to JSON bytes.
pub fn to_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, PluginError> {
  serde_json::to_vec(value).map_err(|error| PluginError::SerializationFailed(format!("failed to serialize JSON bytes: {}", error)))
}

/// Serialize a value to a compact JSON string.
pub fn to_string<T: Serialize>(value: &T) -> Result<String, PluginError> {
  serde_json::to_string(value).map_err(|error| PluginError::SerializationFailed(format!("failed to serialize JSON string: {}", error)))
}

/// Merge `overlay` into `base`.
///
/// If both sides at a branch are objects, keys are merged recursively. All
/// other overlay values replace the existing base value, including arrays and
/// `null`.
pub fn merge_into(base: &mut Value, overlay: Value) {
  match (base, overlay) {
    (Value::Object(base_object), Value::Object(overlay_object)) => {
      for (key, overlay_value) in overlay_object {
        match base_object.get_mut(&key) {
          Some(base_value) => merge_into(base_value, overlay_value),
          None => {
            base_object.insert(key, overlay_value);
          }
        }
      }
    }
    (base_value, overlay_value) => {
      *base_value = overlay_value;
    }
  }
}

/// Return a merged copy of two JSON values.
pub fn merged(mut base: Value, overlay: Value) -> Value {
  merge_into(&mut base, overlay);
  base
}

/// Merge multiple overlays into a base value from left to right.
pub fn merge_all(mut base: Value, overlays: impl IntoIterator<Item = Value>) -> Value {
  for overlay in overlays {
    merge_into(&mut base, overlay);
  }
  base
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde::{Deserialize, Serialize};

  #[derive(Debug, Deserialize, PartialEq, Serialize)]
  struct Payload {
    name: String,
    count: u64,
  }

  #[test]
  fn parse_bytes_parses_typed_payload() {
    let payload: Payload = parse_bytes(br#"{"name":"alpha","count":2}"#).unwrap();
    assert_eq!(payload, Payload { name: "alpha".to_string(), count: 2 });
  }

  #[test]
  fn parse_bytes_reports_invalid_json() {
    let error = parse_bytes::<Payload>(br#"{"name":"alpha""#).unwrap_err();
    match error {
      PluginError::SerializationFailed(message) => {
        assert!(message.contains("failed to parse JSON bytes"));
      }
      other => panic!("expected serialization failure, got {:?}", other),
    }
  }

  #[test]
  fn parse_str_parses_typed_payload() {
    let payload: Payload = parse_str(r#"{"name":"beta","count":3}"#).unwrap();
    assert_eq!(payload.name, "beta");
    assert_eq!(payload.count, 3);
  }

  #[test]
  fn parse_request_reads_plugin_arguments() {
    let request = PluginRequest { arguments: br#"{"name":"gamma","count":4}"#.to_vec(), metadata: Default::default() };
    let payload: Payload = parse_request(&request).unwrap();
    assert_eq!(payload.count, 4);
  }

  #[test]
  fn to_value_to_bytes_and_to_string_serialize_payloads() {
    let payload = Payload { name: "delta".to_string(), count: 5 };

    let value = to_value(&payload).unwrap();
    assert_eq!(value["name"], "delta");
    assert_eq!(value["count"], 5);

    let bytes = to_bytes(&payload).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), json!({"name": "delta", "count": 5}));

    let string = to_string(&payload).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&string).unwrap(), json!({"name": "delta", "count": 5}));
  }

  #[test]
  fn merge_into_recursively_merges_objects() {
    let mut base = json!({
        "name": "doc",
        "meta": {
            "owner": "alice",
            "tags": ["one"],
            "flags": { "archived": false }
        }
    });
    let overlay = json!({
        "meta": {
            "tags": ["two"],
            "flags": { "starred": true }
        },
        "extra": 10
    });

    merge_into(&mut base, overlay);

    assert_eq!(
      base,
      json!({
          "name": "doc",
          "meta": {
              "owner": "alice",
              "tags": ["two"],
              "flags": { "archived": false, "starred": true }
          },
          "extra": 10
      })
    );
  }

  #[test]
  fn merge_into_replaces_non_object_branches_including_null() {
    let mut base = json!({
        "array": [1, 2],
        "scalar": "old",
        "object": { "kept": true }
    });

    merge_into(
      &mut base,
      json!({
          "array": [3],
          "scalar": null,
          "object": "replaced"
      }),
    );

    assert_eq!(
      base,
      json!({
          "array": [3],
          "scalar": null,
          "object": "replaced"
      })
    );
  }

  #[test]
  fn merged_returns_new_value_without_mutating_original() {
    let base = json!({"a": {"b": 1}});
    let result = merged(base.clone(), json!({"a": {"c": 2}}));

    assert_eq!(base, json!({"a": {"b": 1}}));
    assert_eq!(result, json!({"a": {"b": 1, "c": 2}}));
  }

  #[test]
  fn merge_all_applies_overlays_left_to_right() {
    let result =
      merge_all(json!({"a": 1, "nested": {"x": true}}), [json!({"b": 2, "nested": {"y": true}}), json!({"a": 3, "nested": {"x": false}})]);

    assert_eq!(result, json!({"a": 3, "b": 2, "nested": {"x": false, "y": true}}));
  }
}
