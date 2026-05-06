use uuid::Uuid;

use crate::engine::cache::Cache;
use crate::engine::cache_loaders::GroupLoader;
use crate::engine::errors::EngineResult;
use crate::engine::permissions::{merge_flags, parse_crudlify_flags};
use crate::engine::storage_engine::StorageEngine;
use crate::engine::user::is_root;

/// The 8 crudlify operations, each mapping to a position in the flag array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrudlifyOp {
  Create = 0,
  Read = 1,
  Update = 2,
  Delete = 3,
  List = 4,
  Invoke = 5,
  Configure = 6,
  Deploy = 7,
}

/// Resolves permissions for a user at a given path by walking the directory
/// hierarchy and evaluating .permissions files with group membership.
pub struct PermissionResolver<'a> {
  engine: &'a StorageEngine,
  group_cache: &'a Cache<GroupLoader>,
}

impl<'a> PermissionResolver<'a> {
  pub fn new(
    engine: &'a StorageEngine,
    group_cache: &'a Cache<GroupLoader>,
  ) -> Self {
    PermissionResolver {
      engine,
      group_cache,
    }
  }

  /// Check whether a user has permission to perform an operation at a path.
  ///
  /// 1. Root (nil UUID) always gets access.
  /// 2. Walk from "/" to the target path, evaluating .permissions at each level.
  /// 3. At each level: allow adds permissions, deny removes them. Deny wins at same level.
  /// 4. Return the final state for the requested operation.
  pub fn check_permission(
    &self,
    user_id: &Uuid,
    path: &str,
    operation: CrudlifyOp,
  ) -> EngineResult<bool> {
    // Root bypasses everything.
    if is_root(user_id) {
      return Ok(true);
    }

    // Get user's group memberships.
    let user_groups = self.group_cache.get(user_id, self.engine)?;

    // Start with everything denied.
    let mut state = [false; 8];

    // Walk path levels from root to target.
    let levels = path_levels(path);

    for level in &levels {
      let permissions = self.engine.permissions_cache.get(level, self.engine)?;

      let permissions = match permissions {
        Some(permissions) => permissions,
        None => continue, // No .permissions file = no change at this level.
      };

      let mut level_allow: [Option<bool>; 8] = [None; 8];
      let mut level_deny: [Option<bool>; 8] = [None; 8];

      for link in &permissions.links {
        // If link has a path_pattern, only apply when:
        // 1. This level is the immediate parent of the target path
        // 2. The target's filename matches the pattern
        if let Some(ref pattern) = link.path_pattern {
          let target_parent = {
            let trimmed = path.trim_end_matches('/');
            match trimmed.rfind('/') {
              Some(0) => "/".to_string(),
              Some(idx) => trimmed[..idx].to_string(),
              None => "/".to_string(),
            }
          };
          let normalized_level = level.trim_end_matches('/');
          let normalized_parent = target_parent.trim_end_matches('/');
          if normalized_level != normalized_parent {
            continue;
          }
          let filename = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
          if filename != pattern {
            continue;
          }
        }

        let is_member = user_groups.contains(&link.group);

        if is_member {
          let allow_flags = parse_crudlify_flags(&link.allow);
          let deny_flags = parse_crudlify_flags(&link.deny);
          merge_flags(&mut level_allow, &allow_flags);
          merge_flags(&mut level_deny, &deny_flags);
        } else if link.others_allow.is_some() || link.others_deny.is_some() {
          if let Some(ref others_allow) = link.others_allow {
            let flags = parse_crudlify_flags(others_allow);
            merge_flags(&mut level_allow, &flags);
          }
          if let Some(ref others_deny) = link.others_deny {
            let flags = parse_crudlify_flags(others_deny);
            merge_flags(&mut level_deny, &flags);
          }
        }
      }

      // Apply: allow adds, deny removes. Deny wins at same level.
      for index in 0..8 {
        if level_allow[index] == Some(true) {
          state[index] = true;
        }
        if level_deny[index] == Some(true) {
          state[index] = false;
        }
      }
    }

    Ok(state[operation as usize])
  }
}

/// Split a path into hierarchical levels from root to the target.
///
/// Example: "/myapp/users/alice.json" produces:
///   ["/", "/myapp", "/myapp/users"]
///
/// The file itself is not included -- permissions are on directories only.
/// For a directory path like "/myapp/users/", the last directory IS included.
pub fn path_levels(path: &str) -> Vec<String> {
  let mut levels = vec!["/".to_string()];

  let trimmed = path.trim_matches('/');
  if trimmed.is_empty() {
    return levels;
  }

  let segments: Vec<&str> = trimmed.split('/').collect();

  // If the path ends with '/', treat all segments as directories.
  // Otherwise, exclude the last segment (it's the file name).
  let directory_segment_count = if path.ends_with('/') {
    segments.len()
  } else if !segments.is_empty() {
    segments.len() - 1
  } else {
    0
  };

  let mut current = String::new();
  for segment in segments.iter().take(directory_segment_count) {
    current.push('/');
    current.push_str(segment);
    levels.push(current.clone());
  }

  levels
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_path_levels_root() {
    assert_eq!(path_levels("/"), vec!["/"]);
  }

  #[test]
  fn test_path_levels_file() {
    let levels = path_levels("/myapp/users/alice.json");
    assert_eq!(levels, vec!["/", "/myapp", "/myapp/users"]);
  }

  #[test]
  fn test_path_levels_directory() {
    let levels = path_levels("/myapp/users/");
    assert_eq!(levels, vec!["/", "/myapp", "/myapp/users"]);
  }

  #[test]
  fn test_path_levels_top_level_file() {
    let levels = path_levels("/file.json");
    assert_eq!(levels, vec!["/"]);
  }

  #[test]
  fn test_path_levels_top_level_dir() {
    let levels = path_levels("/myapp/");
    assert_eq!(levels, vec!["/", "/myapp"]);
  }

  #[test]
  fn test_path_levels_empty() {
    assert_eq!(path_levels(""), vec!["/"]);
  }
}
