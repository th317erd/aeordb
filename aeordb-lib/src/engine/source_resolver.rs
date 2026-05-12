use regex::Regex;

/// Resolve a source path (array of JSON segments) against a JSON value,
/// returning ALL matching values (fan-out on `""` and regex segments).
///
/// Segments:
///   - Non-empty string → object key lookup
///   - Integer → array index if current is array, else object key as stringified integer
///   - Empty string `""` → fan out to ALL array elements or ALL object values
///   - Regex `/pattern/flags` → fan out, filtering by regex
///       (objects: match keys, arrays: match stringified elements). Flag `i` = case-insensitive.
///   - Other types (bool, null, object, array) → resolution failure (returns empty)
///
/// Returns every resolved value as bytes (via `json_value_to_bytes`).
pub fn resolve_sources(json: &serde_json::Value, source: &[serde_json::Value]) -> Vec<Vec<u8>> {
    walk_paths(json, source)
        .into_iter()
        .map(|v| crate::engine::json_parser::json_value_to_bytes(&v))
        .collect()
}

/// Walk a JSON value following the given path segments, fanning out on
/// wildcard (`""`) and regex segments.  Returns all matched JSON values.
pub fn walk_paths(json: &serde_json::Value, segments: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut current: Vec<&serde_json::Value> = vec![json];

    for segment in segments {
        let mut next: Vec<&serde_json::Value> = Vec::new();

        for val in &current {
            match segment {
                serde_json::Value::String(key) => {
                    if key.is_empty() {
                        // Fan-out: all array elements or all object values
                        match val {
                            serde_json::Value::Array(arr) => {
                                for item in arr {
                                    next.push(item);
                                }
                            }
                            serde_json::Value::Object(map) => {
                                for (_k, v) in map {
                                    next.push(v);
                                }
                            }
                            _ => {} // scalar — nothing to fan out
                        }
                    } else if let Some(re) = parse_regex(key) {
                        // Regex fan-out
                        match val {
                            serde_json::Value::Object(map) => {
                                for (k, v) in map {
                                    if re.is_match(k) {
                                        next.push(v);
                                    }
                                }
                            }
                            serde_json::Value::Array(arr) => {
                                for item in arr {
                                    let s = value_to_string(item);
                                    if re.is_match(&s) {
                                        next.push(item);
                                    }
                                }
                            }
                            _ => {} // scalar — nothing to match
                        }
                    } else {
                        // Plain key lookup
                        if let Some(child) = val.get(key.as_str()) {
                            next.push(child);
                        }
                    }
                }
                serde_json::Value::Number(n) => {
                    if let Some(idx) = n.as_u64() {
                        let idx = idx as usize;
                        if val.is_array() {
                            if let Some(child) = val.get(idx) {
                                next.push(child);
                            }
                        } else {
                            // Try as string key on object
                            if let Some(child) = val.get(idx.to_string()) {
                                next.push(child);
                            }
                        }
                    }
                    // negative or float numbers → no match, skip
                }
                _ => {} // bool, null, object, array → invalid segment, skip
            }
        }

        current = next;
        if current.is_empty() {
            return Vec::new();
        }
    }

    current.into_iter().cloned().collect()
}

// ── backward-compatible singular wrappers ────────────────────────────

/// Resolve a source path, returning the first matching value as bytes.
/// Delegates to `resolve_sources` for backward compatibility.
pub fn resolve_source(json: &serde_json::Value, source: &[serde_json::Value]) -> Option<Vec<u8>> {
    resolve_sources(json, source).into_iter().next()
}

/// Walk a JSON value following the given path segments, returning the
/// first matching JSON value.  Delegates to `walk_paths`.
pub fn walk_path(json: &serde_json::Value, segments: &[serde_json::Value]) -> Option<serde_json::Value> {
    walk_paths(json, segments).into_iter().next()
}

// ── helpers ──────────────────────────────────────────────────────────

/// Detect regex syntax `/pattern/flags` and compile it.
/// A string is treated as regex when it starts with `/` and contains at
/// least one more `/` (the closing delimiter).
fn parse_regex(s: &str) -> Option<Regex> {
    if !s.starts_with('/') {
        return None;
    }
    // Find the LAST `/` which separates flags from the pattern body.
    let rest = &s[1..]; // skip leading `/`
    let closing = rest.rfind('/')?;
    let pattern = &rest[..closing];
    let flags = &rest[closing + 1..];

    let case_insensitive = flags.contains('i');

    let full_pattern = if case_insensitive {
        format!("(?i){}", pattern)
    } else {
        pattern.to_string()
    };

    Regex::new(&full_pattern).ok()
}

/// Stringify a JSON value for regex matching against array elements.
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(), // arrays/objects → JSON repr
    }
}
