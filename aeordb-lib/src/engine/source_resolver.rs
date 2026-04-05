/// Resolve a source path (array of JSON segments) against a JSON value.
///
/// Segments:
///   - String → object key lookup
///   - Integer → array index if current is array, else object key as stringified integer
///   - Other types (bool, null, object, array) → resolution failure (returns None)
///
/// Returns the resolved value as bytes suitable for indexing, using the same
/// conversion as json_value_to_bytes (strings → UTF-8, numbers → big-endian, etc.)
pub fn resolve_source(json: &serde_json::Value, source: &[serde_json::Value]) -> Option<Vec<u8>> {
  let resolved = walk_path(json, source)?;
  Some(crate::engine::json_parser::json_value_to_bytes(&resolved))
}

/// Walk a JSON value following the given path segments.
/// Returns the resolved JSON value, or None if any step fails.
pub fn walk_path(json: &serde_json::Value, segments: &[serde_json::Value]) -> Option<serde_json::Value> {
  let mut current = json;
  for segment in segments {
    match segment {
      serde_json::Value::String(key) => {
        current = current.get(key.as_str())?;
      }
      serde_json::Value::Number(n) => {
        let idx = n.as_u64()? as usize;
        if current.is_array() {
          current = current.get(idx)?;
        } else {
          // Try as string key on object
          current = current.get(&idx.to_string())?;
        }
      }
      _ => return None, // bool, null, object, array — invalid segment types
    }
  }
  Some(current.clone())
}
