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
  /// 4. For Read/List on a directory: if the direct walk denies but the user
  ///    has any grant in a descendant subtree, return true so the user can
  ///    navigate down to the share. This is the symmetric equivalent of
  ///    `is_ancestor_of_any_rule` for API key rules — directories on the
  ///    path to a grant are implicitly navigable.
  /// 5. Return the final state for the requested operation.
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

    let direct = self.check_direct_permission(user_id, path, operation)?;
    if direct {
      return Ok(true);
    }

    // Allow ancestor navigation: a user with a grant at /A/B/C must be able
    // to Read/List /, /A, and /A/B in order to walk down to it.
    if matches!(operation, CrudlifyOp::Read | CrudlifyOp::List)
      && self.has_descendant_grants(user_id, path)?
    {
      return Ok(true);
    }

    Ok(false)
  }

  /// Like `check_direct_permission`, but tolerant of the trailing-slash
  /// convention: a path like `/A/B` is treated as a possible directory
  /// AND a possible file, returning true if either form grants the
  /// operation. Use this when a handler receives a user-supplied path
  /// whose type (file vs directory) isn't pre-determined — e.g.
  /// `/files/copy`, `/files/download`, `/files/mkdir`. The resolver's
  /// `path_levels` only walks the trailing segment as a directory when
  /// the input ends with `/`, so without this helper a request for the
  /// real directory `/A/B` would silently miss any grants stored at
  /// `/A/B/.aeordb-permissions`.
  pub fn check_path_permission(
    &self,
    user_id: &Uuid,
    path: &str,
    operation: CrudlifyOp,
  ) -> EngineResult<bool> {
    if self.check_direct_permission(user_id, path, operation)? {
      return Ok(true);
    }
    // Try the directory form. If the input already ended with '/', this
    // is a no-op; otherwise it gives `path_levels` one more directory
    // to walk so a grant stored at `path` itself is visible.
    let dir_form = if path.ends_with('/') {
      return Ok(false);
    } else {
      format!("{}/", path)
    };
    self.check_direct_permission(user_id, &dir_form, operation)
  }

  /// Like `check_permission`, but does NOT apply the ancestor-navigation
  /// softening. Returns true only if the user has a direct grant at or
  /// above `path`. Use this when you specifically need to distinguish
  /// "owned/granted" from "merely navigable" — for example, when deciding
  /// whether to filter a directory listing.
  pub fn check_direct_permission(
    &self,
    user_id: &Uuid,
    path: &str,
    operation: CrudlifyOp,
  ) -> EngineResult<bool> {
    if is_root(user_id) {
      return Ok(true);
    }

    // Normalize: callers may pass paths without a leading slash (e.g. the
    // permission middleware strips "/files/" leaving "foo/bar/baz.txt").
    // path_levels returns levels WITH a leading slash, so we must align.
    let normalized = if path.starts_with('/') {
      path.to_string()
    } else {
      format!("/{}", path)
    };
    let path = normalized.as_str();

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

  /// True if the user has any permission grant at or below `path`.
  /// Used to allow ancestor navigation: a user who can read `/A/B/C` must
  /// be able to List its ancestors (`/`, `/A`, `/A/B`) even though those
  /// directories have no `.permissions` link for them.
  pub fn has_descendant_grants(&self, user_id: &Uuid, path: &str) -> EngineResult<bool> {
    if is_root(user_id) {
      return Ok(true);
    }
    let normalized = if path.starts_with('/') {
      path.to_string()
    } else {
      format!("/{}", path)
    };
    let user_groups = self.group_cache.get(user_id, self.engine)?;
    if user_groups.is_empty() {
      return Ok(false);
    }
    let index = self.engine.grants_index_cache.get(&(), self.engine)?;
    Ok(index.user_has_descendant_grants(&user_groups, &normalized))
  }

  /// Return the set of immediate child names of `parent_path` that the
  /// user can either access directly or must traverse to reach a deeper
  /// grant. Used by listing handlers to filter children when the user
  /// reached `parent_path` via ancestor navigation rather than a direct
  /// list grant.
  pub fn accessible_child_names(
    &self,
    user_id: &Uuid,
    parent_path: &str,
  ) -> EngineResult<Vec<String>> {
    if is_root(user_id) {
      return Ok(Vec::new());
    }
    let normalized = if parent_path.starts_with('/') {
      parent_path.to_string()
    } else {
      format!("/{}", parent_path)
    };
    let user_groups = self.group_cache.get(user_id, self.engine)?;
    if user_groups.is_empty() {
      return Ok(Vec::new());
    }
    let index = self.engine.grants_index_cache.get(&(), self.engine)?;
    Ok(index.accessible_child_names(&user_groups, &normalized))
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
