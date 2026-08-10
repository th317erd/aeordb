use serde::Serialize;

use super::{required_dispatch_value, serialize_response_value};

struct SerializationFailure;

impl Serialize for SerializationFailure {
  fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    Err(serde::ser::Error::custom("injected serialization failure"))
  }
}

#[test]
fn internal_dispatch_state_failure_returns_an_http_error() {
  let response = required_dispatch_value::<Vec<u8>>(None, "DirectoryIndex", "deadbeef").unwrap_err();

  assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn response_serialization_failure_returns_an_http_error() {
  let response = serialize_response_value(&SerializationFailure, "query result").unwrap_err();

  assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn production_hash_and_query_routes_contain_no_dispatch_or_serialization_panics() {
  let source = include_str!("../../src/server/engine_routes.rs");
  let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);

  assert!(!production.contains(".expect(\"FileRecord dispatch"));
  assert!(!production.contains(".expect(\"raw entry dispatch"));
  assert!(!production.contains("serde_json::to_value(&result).unwrap()"));
  assert!(!production.contains("serde_json::to_value(meta).unwrap()"));
  assert!(!production.contains("serde_json::to_value(generation.matches).unwrap_or_else"));
}
