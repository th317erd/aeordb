use serde::{Deserialize, Serialize};

use crate::engine::errors::{EngineError, EngineResult};

/// The 8 crudlify flag positions and their canonical letters.
const CRUDLIFY_LETTERS: [char; 8] = ['c', 'r', 'u', 'd', 'l', 'i', 'f', 'y'];

/// Permission link connecting a group to a path with crudlify flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionLink {
  pub group: String,
  /// 8 chars crudlify: "crudlify", "cr......" etc.
  pub allow: String,
  /// 8 chars deny flags.
  pub deny: String,
  /// Optional allow flags for non-members of this group.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub others_allow: Option<String>,
  /// Optional deny flags for non-members of this group.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub others_deny: Option<String>,
}

/// Permissions for a directory path, stored as `.permissions` JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathPermissions {
  pub links: Vec<PermissionLink>,
}

impl PathPermissions {
  /// Serialize to JSON bytes for storage.
  pub fn serialize(&self) -> Vec<u8> {
    serde_json::to_vec(self).expect("PathPermissions serialization should never fail")
  }

  /// Deserialize from JSON bytes.
  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    serde_json::from_slice(data)
      .map_err(|error| EngineError::JsonParseError(format!("Failed to deserialize PathPermissions: {}", error)))
  }
}

/// Parse a crudlify flag string into an array of 8 tri-state flags.
///
/// Each position maps to a crudlify operation:
///   0=create, 1=read, 2=update, 3=delete, 4=list, 5=invoke, 6=configure, 7=deploy
///
/// A letter at the correct position means `Some(true)` (set).
/// A dot `.` means `None` (no opinion).
/// Any other character at a position is treated as `None`.
pub fn parse_crudlify_flags(flags: &str) -> [Option<bool>; 8] {
  let mut result = [None; 8];
  let chars: Vec<char> = flags.chars().collect();

  for (index, expected_letter) in CRUDLIFY_LETTERS.iter().enumerate() {
    if index < chars.len() && chars[index] == *expected_letter {
      result[index] = Some(true);
    }
  }

  result
}

/// Merge source flags into target using union semantics.
/// Any `Some(true)` in source wins over `None` in target.
pub fn merge_flags(target: &mut [Option<bool>; 8], source: &[Option<bool>; 8]) {
  for index in 0..8 {
    if source[index] == Some(true) {
      target[index] = Some(true);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_all_set() {
    let flags = parse_crudlify_flags("crudlify");
    for flag in &flags {
      assert_eq!(*flag, Some(true));
    }
  }

  #[test]
  fn test_parse_all_dots() {
    let flags = parse_crudlify_flags("........");
    for flag in &flags {
      assert_eq!(*flag, None);
    }
  }

  #[test]
  fn test_parse_mixed() {
    let flags = parse_crudlify_flags("cr..l..y");
    assert_eq!(flags[0], Some(true)); // c
    assert_eq!(flags[1], Some(true)); // r
    assert_eq!(flags[2], None);       // u
    assert_eq!(flags[3], None);       // d
    assert_eq!(flags[4], Some(true)); // l
    assert_eq!(flags[5], None);       // i
    assert_eq!(flags[6], None);       // f
    assert_eq!(flags[7], Some(true)); // y
  }

  #[test]
  fn test_parse_empty_string() {
    let flags = parse_crudlify_flags("");
    for flag in &flags {
      assert_eq!(*flag, None);
    }
  }

  #[test]
  fn test_merge_flags_union() {
    let mut target = [None, Some(true), None, None, None, None, None, None];
    let source = [Some(true), None, Some(true), None, None, None, None, None];
    merge_flags(&mut target, &source);
    assert_eq!(target[0], Some(true));
    assert_eq!(target[1], Some(true));
    assert_eq!(target[2], Some(true));
    assert_eq!(target[3], None);
  }

  #[test]
  fn test_serialize_deserialize_roundtrip() {
    let permissions = PathPermissions {
      links: vec![
        PermissionLink {
          group: "engineers".to_string(),
          allow: "crudli..".to_string(),
          deny: "........".to_string(),
          others_allow: None,
          others_deny: None,
        },
        PermissionLink {
          group: "security".to_string(),
          allow: "crudlify".to_string(),
          deny: "........".to_string(),
          others_allow: Some("........".to_string()),
          others_deny: Some("crudlify".to_string()),
        },
      ],
    };

    let bytes = permissions.serialize();
    let deserialized = PathPermissions::deserialize(&bytes).unwrap();
    assert_eq!(deserialized.links.len(), 2);
    assert_eq!(deserialized.links[0].group, "engineers");
    assert_eq!(deserialized.links[1].others_deny.as_deref(), Some("crudlify"));
  }
}
