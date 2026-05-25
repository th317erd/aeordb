//! Schema-version helpers for entities whose payload is stored without a
//! surrounding [`EntryHeader`] (e.g. JSON files under `/.aeordb-system/`,
//! `/.aeordb-config/`, etc.).
//!
//! KV-backed entities (FileRecord, ChildEntry, SymlinkRecord, …) get their
//! version byte from the KV `EntryHeader.entry_version` and pass it as an
//! argument to their `deserialize` function. This module exists for the
//! other class: payloads that have to carry their own version because there
//! is no outer header at the read site.
//!
//! Convention:
//! - JSON payloads embed a top-level `"$v": <u8>` field. [`read_json_version`]
//!   extracts it (returns 0 if absent for forward-compat with the first
//!   version that didn't have the field — DO NOT add new schemas at v0;
//!   start v1+).
//! - [`write_json_with_version`] serializes a `T: Serialize` value and
//!   injects `"$v"` into the resulting object so the reader can dispatch.
//! - Binary payloads put the version byte at offset 0 — see each entity's
//!   `deserialize` for the exact layout.

use serde::Serialize;

use crate::engine::errors::{EngineError, EngineResult};

/// Convenience macro for JSON-stored entities that only have a v0 format
/// today. Generates a [`JsonVersioned`] impl that reads `$v`, dispatches
/// to v0, and errors on anything else. When you actually need v1+,
/// replace the macro invocation with a hand-written impl that has
/// `deserialize_v0`, `deserialize_v1`, etc. helper methods.
///
/// Usage:
/// ```ignore
/// crate::impl_json_versioned_v0!(MyType);
/// ```
#[macro_export]
macro_rules! impl_json_versioned_v0 {
    ($t:ty) => {
        impl $crate::engine::schema_version::JsonVersioned for $t {
            const SCHEMA_VERSION: u8 = 0;

            fn serialize_versioned(&self) -> Vec<u8> {
                $crate::engine::schema_version::write_json_with_version(self, 0)
                    .expect(concat!(stringify!($t), " serialization should never fail"))
            }

            fn deserialize_versioned(
                data: &[u8],
            ) -> $crate::engine::errors::EngineResult<Self> {
                let version = $crate::engine::schema_version::read_json_version(data)?;
                match version {
                    0 => ::serde_json::from_slice(data).map_err(|e| {
                        $crate::engine::errors::EngineError::JsonParseError(format!(
                            "Failed to deserialize {}: {}",
                            stringify!($t),
                            e
                        ))
                    }),
                    _ => Err($crate::engine::errors::EngineError::InvalidEntryVersion(version)),
                }
            }
        }
    };
}

/// Trait every JSON-stored entity must implement so the storage layer can
/// route through versioned serializers without knowing about each concrete
/// type. Implementations should follow the standard shape:
///
/// ```ignore
/// const SCHEMA_VERSION: u8 = 0;
/// fn serialize(&self) -> Vec<u8> {
///     crate::engine::schema_version::write_json_with_version(self, Self::SCHEMA_VERSION).unwrap()
/// }
/// fn deserialize(data: &[u8]) -> EngineResult<Self> {
///     let v = crate::engine::schema_version::read_json_version(data)?;
///     match v {
///         0 => Self::deserialize_v0(data),
///         _ => Err(EngineError::InvalidEntryVersion(v)),
///     }
/// }
/// ```
pub trait JsonVersioned: Sized {
    /// Schema version this type writes today.
    const SCHEMA_VERSION: u8;

    /// Serialize self to JSON bytes with `$v` injected.
    fn serialize_versioned(&self) -> Vec<u8>;

    /// Read `$v` from bytes and dispatch to the matching `deserialize_v{n}`.
    fn deserialize_versioned(data: &[u8]) -> EngineResult<Self>;
}

/// Current schema version for entities written via this module. New entity
/// formats should bump this constant and add a corresponding `deserialize_v{n}`
/// arm in the type's `deserialize` function.
pub const CURRENT_JSON_SCHEMA_VERSION: u8 = 0;

/// Extract the `$v` schema version from a JSON-encoded payload. Returns 0
/// when the field is absent (legacy pre-versioning blobs) so the v0 reader
/// can still parse them.
///
/// Errors only when the bytes aren't valid JSON or aren't an object at the
/// top level — both are corruption rather than a legitimate format we don't
/// know how to read.
pub fn read_json_version(data: &[u8]) -> EngineResult<u8> {
    let value: serde_json::Value = serde_json::from_slice(data)
        .map_err(|e| EngineError::JsonParseError(
            format!("schema_version: payload is not valid JSON: {}", e)
        ))?;
    let version = value
        .get("$v")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if version > u8::MAX as u64 {
        return Err(EngineError::InvalidEntryVersion(0xff));
    }
    Ok(version as u8)
}

/// Serialize `value` to JSON and inject a `"$v": version` field at the top
/// level. The value MUST serialize to a JSON object; primitives and arrays
/// have no place to attach the version and will return an error.
pub fn write_json_with_version<T: Serialize>(
    value: &T,
    version: u8,
) -> EngineResult<Vec<u8>> {
    let mut json = serde_json::to_value(value)
        .map_err(|e| EngineError::JsonParseError(
            format!("schema_version: cannot serialize to JSON: {}", e)
        ))?;
    match json.as_object_mut() {
        Some(obj) => {
            obj.insert("$v".to_string(), serde_json::json!(version));
        }
        None => {
            return Err(EngineError::JsonParseError(
                "schema_version: value must serialize to a JSON object".to_string()
            ));
        }
    }
    serde_json::to_vec(&json)
        .map_err(|e| EngineError::JsonParseError(
            format!("schema_version: re-serialization failed: {}", e)
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Sample {
        name: String,
        n: u64,
    }

    #[test]
    fn injects_and_extracts_version() {
        let s = Sample { name: "x".to_string(), n: 7 };
        let bytes = write_json_with_version(&s, 3).unwrap();
        assert_eq!(read_json_version(&bytes).unwrap(), 3);

        // Inner structure preserved alongside $v.
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["name"], "x");
        assert_eq!(parsed["n"], 7);
        assert_eq!(parsed["$v"], 3);
    }

    #[test]
    fn missing_v_defaults_to_zero() {
        let raw = br#"{"name":"y","n":1}"#;
        assert_eq!(read_json_version(raw).unwrap(), 0);
    }

    #[test]
    fn rejects_non_object() {
        let s = vec![1, 2, 3];
        let result = write_json_with_version(&s, 0);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(read_json_version(b"not json").is_err());
    }
}
