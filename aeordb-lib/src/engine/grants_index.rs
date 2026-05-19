use std::collections::HashMap;

use crate::engine::cache::CacheLoader;
use crate::engine::directory_listing::list_directory_recursive;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::EngineResult;
use crate::engine::permissions::PathPermissions;
use crate::engine::storage_engine::StorageEngine;

const MAX_SCAN_DEPTH: i32 = 10;
const MAX_PERM_FILES: usize = 1_000;

/// A single permission link, indexed by its containing directory path.
/// Mirrors `PermissionLink` from `permissions.rs` but with the absolute
/// directory path attached, so callers can answer "which paths grant this
/// group anything?" without re-scanning the tree.
#[derive(Debug, Clone)]
pub struct GrantRecord {
  /// The directory that contains this `.aeordb-permissions` link
  /// (the file lives at `{dir_path}/.aeordb-permissions`).
  pub dir_path: String,
  pub allow: String,
  pub deny: String,
  pub others_allow: Option<String>,
  pub others_deny: Option<String>,
  /// When set, the grant only applies to a single filename in `dir_path`.
  pub path_pattern: Option<String>,
}

impl GrantRecord {
  /// The absolute path the grant ultimately targets.
  /// For a directory grant this is `dir_path`; for a file-pattern grant
  /// it is `{dir_path}/{path_pattern}`.
  pub fn target_path(&self) -> String {
    match &self.path_pattern {
      Some(name) => {
        if self.dir_path == "/" {
          format!("/{}", name)
        } else {
          format!("{}/{}", self.dir_path, name)
        }
      }
      None => self.dir_path.clone(),
    }
  }
}

/// Aggregate of every `.aeordb-permissions` link in the database, grouped
/// by the group_name they apply to. Built lazily by the singleton
/// `Cache<GrantsIndexLoader>` and rebuilt on `evict_all`.
#[derive(Debug, Clone, Default)]
pub struct GrantsIndex {
  pub by_group: HashMap<String, Vec<GrantRecord>>,
}

impl GrantsIndex {
  /// True if the user's groups grant any access STRICTLY below `path`.
  /// "Strictly" — equal-to-target does NOT count — because the direct
  /// resolver walk already handles permissions at the target itself, and
  /// softening "Read/List on the target" would override the grant's
  /// actual flag set (e.g. a create-only file-pattern grant must not
  /// gain Read just because it appears in the index at its own path).
  pub fn user_has_descendant_grants(&self, user_groups: &[String], path: &str) -> bool {
    for group in user_groups {
      let Some(records) = self.by_group.get(group) else { continue };
      for record in records {
        let target = record.target_path();
        if path_is_strict_ancestor(path, &target) {
          return true;
        }
      }
    }
    false
  }

  /// Return the set of immediate child names of `parent_path` that the
  /// user can either access directly or must traverse to reach a deeper
  /// grant. Used by listing handlers to filter results when the user
  /// reached `parent_path` via ancestor navigation.
  pub fn accessible_child_names(&self, user_groups: &[String], parent_path: &str) -> Vec<String> {
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for group in user_groups {
      let Some(records) = self.by_group.get(group) else { continue };
      for record in records {
        let target = record.target_path();
        if let Some(segment) = next_segment_below(parent_path, &target) {
          names.insert(segment.to_string());
        }
      }
    }
    names.into_iter().collect()
  }
}

/// Loader for the singleton `GrantsIndex` cache. Key is `()` because
/// there is only ever one entry — the whole index. Eviction is coarse:
/// `evict_all` on any `.aeordb-permissions` write rebuilds from scratch.
pub struct GrantsIndexLoader;

impl CacheLoader for GrantsIndexLoader {
  type Key = ();
  type Value = GrantsIndex;

  fn load(&self, _key: &(), engine: &StorageEngine) -> EngineResult<GrantsIndex> {
    let ops = DirectoryOps::new(engine);

    let perm_files = match list_directory_recursive(
      engine, "/", MAX_SCAN_DEPTH, Some(".aeordb-permissions"), Some(MAX_PERM_FILES),
    ) {
      Ok(entries) => entries,
      Err(_) => return Ok(GrantsIndex::default()),
    };

    let mut by_group: HashMap<String, Vec<GrantRecord>> = HashMap::new();

    for entry in &perm_files {
      let data = match ops.read_file_buffered(&entry.path) {
        Ok(d) => d,
        Err(_) => continue,
      };
      let perms = match PathPermissions::deserialize(&data) {
        Ok(p) => p,
        Err(_) => continue,
      };

      let dir_path = if entry.path.ends_with("/.aeordb-permissions") {
        let stripped = &entry.path[..entry.path.len() - "/.aeordb-permissions".len()];
        if stripped.is_empty() { "/".to_string() } else { stripped.to_string() }
      } else if entry.path == "/.aeordb-permissions" {
        "/".to_string()
      } else {
        continue;
      };

      for link in perms.links {
        by_group.entry(link.group.clone()).or_default().push(GrantRecord {
          dir_path: dir_path.clone(),
          allow: link.allow,
          deny: link.deny,
          others_allow: link.others_allow,
          others_deny: link.others_deny,
          path_pattern: link.path_pattern,
        });
      }
    }

    Ok(GrantsIndex { by_group })
  }
}

/// Strip a trailing slash from a path while preserving `"/"` itself.
fn normalize_for_compare(path: &str) -> &str {
  if path.len() > 1 { path.trim_end_matches('/') } else { path }
}

/// True if `ancestor` is `target` itself or a parent directory of `target`.
/// Both paths are expected to be absolute (start with `/`); trailing
/// slashes are tolerated.
fn path_is_ancestor_or_equal(ancestor: &str, target: &str) -> bool {
  let ancestor = normalize_for_compare(ancestor);
  let target = normalize_for_compare(target);
  if ancestor == target { return true; }
  if ancestor == "/" { return target.starts_with('/'); }
  if !target.starts_with(ancestor) { return false; }
  target[ancestor.len()..].starts_with('/')
}

/// True if `ancestor` is a strict parent of `target` — same as
/// `path_is_ancestor_or_equal` but excludes the equal case.
fn path_is_strict_ancestor(ancestor: &str, target: &str) -> bool {
  let a = normalize_for_compare(ancestor);
  let t = normalize_for_compare(target);
  if a == t { return false; }
  path_is_ancestor_or_equal(a, t)
}

/// Given an absolute `parent_path` and an absolute `target` that lies
/// somewhere beneath it, return the first path segment between them.
/// Returns None if `target == parent_path` or `target` is not a descendant.
/// Trailing slashes on either argument are tolerated.
fn next_segment_below<'a>(parent_path: &str, target: &'a str) -> Option<&'a str> {
  let parent_path = normalize_for_compare(parent_path);
  // Note: we deliberately do NOT trim `target` because we need the
  // borrow-checker-friendly &'a str to live as long as the input.
  // Instead, the strip operations below cope with both forms.
  let stripped = if parent_path == "/" {
    target.strip_prefix('/')?
  } else {
    let after = target.strip_prefix(parent_path)?;
    after.strip_prefix('/')?
  };
  if stripped.is_empty() { return None; }
  Some(stripped.split('/').next().unwrap_or(stripped))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn target_path_for_directory_grant() {
    let record = GrantRecord {
      dir_path: "/Pictures/Family/Harlo".to_string(),
      allow: "rl......".to_string(),
      deny: "........".to_string(),
      others_allow: None,
      others_deny: None,
      path_pattern: None,
    };
    assert_eq!(record.target_path(), "/Pictures/Family/Harlo");
  }

  #[test]
  fn target_path_for_file_pattern_grant() {
    let record = GrantRecord {
      dir_path: "/Pictures".to_string(),
      allow: "r.......".to_string(),
      deny: "........".to_string(),
      others_allow: None,
      others_deny: None,
      path_pattern: Some("photo.jpg".to_string()),
    };
    assert_eq!(record.target_path(), "/Pictures/photo.jpg");
  }

  #[test]
  fn target_path_for_file_pattern_at_root() {
    let record = GrantRecord {
      dir_path: "/".to_string(),
      allow: "r.......".to_string(),
      deny: "........".to_string(),
      others_allow: None,
      others_deny: None,
      path_pattern: Some("readme.md".to_string()),
    };
    assert_eq!(record.target_path(), "/readme.md");
  }

  #[test]
  fn path_is_ancestor_or_equal_root_contains_everything() {
    assert!(path_is_ancestor_or_equal("/", "/"));
    assert!(path_is_ancestor_or_equal("/", "/foo"));
    assert!(path_is_ancestor_or_equal("/", "/foo/bar"));
  }

  #[test]
  fn path_is_ancestor_or_equal_strict_prefix() {
    assert!(path_is_ancestor_or_equal("/foo", "/foo"));
    assert!(path_is_ancestor_or_equal("/foo", "/foo/bar"));
    assert!(!path_is_ancestor_or_equal("/foo", "/foobar"));
    assert!(!path_is_ancestor_or_equal("/foo/bar", "/foo"));
    assert!(!path_is_ancestor_or_equal("/foo", "/bar"));
  }

  #[test]
  fn next_segment_below_basic() {
    assert_eq!(next_segment_below("/", "/foo"), Some("foo"));
    assert_eq!(next_segment_below("/", "/foo/bar"), Some("foo"));
    assert_eq!(next_segment_below("/foo", "/foo/bar"), Some("bar"));
    assert_eq!(next_segment_below("/foo/bar", "/foo/bar/baz/qux"), Some("baz"));
  }

  #[test]
  fn next_segment_below_self_and_disjoint() {
    assert_eq!(next_segment_below("/", "/"), None);
    assert_eq!(next_segment_below("/foo", "/foo"), None);
    assert_eq!(next_segment_below("/foo", "/foobar"), None);
    assert_eq!(next_segment_below("/foo", "/bar"), None);
    assert_eq!(next_segment_below("/foo/bar", "/foo"), None);
  }

  #[test]
  fn grants_index_user_has_descendant_grants() {
    let mut by_group: HashMap<String, Vec<GrantRecord>> = HashMap::new();
    by_group.insert("share-1".to_string(), vec![GrantRecord {
      dir_path: "/Pictures/Family/Harlo".to_string(),
      allow: "rl......".to_string(),
      deny: "........".to_string(),
      others_allow: None,
      others_deny: None,
      path_pattern: None,
    }]);
    let index = GrantsIndex { by_group };
    let groups = vec!["share-1".to_string()];

    assert!(index.user_has_descendant_grants(&groups, "/"));
    assert!(index.user_has_descendant_grants(&groups, "/Pictures"));
    assert!(index.user_has_descendant_grants(&groups, "/Pictures/Family"));
    // Strict ancestor: the grant target itself is NOT a "descendant" for
    // this check — the direct resolver walk handles equal-path access.
    assert!(!index.user_has_descendant_grants(&groups, "/Pictures/Family/Harlo"));
    assert!(!index.user_has_descendant_grants(&groups, "/Pictures/Family/Harlo/photos/2024"),
      "deeper than the grant should not register as descendant");
    assert!(!index.user_has_descendant_grants(&groups, "/Music"));
    assert!(!index.user_has_descendant_grants(&[], "/"));
  }

  #[test]
  fn grants_index_accessible_child_names() {
    let mut by_group: HashMap<String, Vec<GrantRecord>> = HashMap::new();
    by_group.insert("share-1".to_string(), vec![
      GrantRecord {
        dir_path: "/Pictures/Family/Harlo".to_string(),
        allow: "rl......".to_string(), deny: "........".to_string(),
        others_allow: None, others_deny: None, path_pattern: None,
      },
      GrantRecord {
        dir_path: "/Documents".to_string(),
        allow: "r.......".to_string(), deny: "........".to_string(),
        others_allow: None, others_deny: None,
        path_pattern: Some("tax-2025.pdf".to_string()),
      },
    ]);
    let index = GrantsIndex { by_group };
    let groups = vec!["share-1".to_string()];

    let mut root_children = index.accessible_child_names(&groups, "/");
    root_children.sort();
    assert_eq!(root_children, vec!["Documents".to_string(), "Pictures".to_string()]);

    let pics = index.accessible_child_names(&groups, "/Pictures");
    assert_eq!(pics, vec!["Family".to_string()]);

    let docs = index.accessible_child_names(&groups, "/Documents");
    assert_eq!(docs, vec!["tax-2025.pdf".to_string()]);

    // No grants under this path
    assert!(index.accessible_child_names(&groups, "/Music").is_empty());
  }

  #[test]
  fn ancestor_descent_does_not_register_grants_in_unrelated_subtrees() {
    let mut by_group: HashMap<String, Vec<GrantRecord>> = HashMap::new();
    by_group.insert("share-1".to_string(), vec![GrantRecord {
      dir_path: "/A/B".to_string(),
      allow: "rl......".to_string(), deny: "........".to_string(),
      others_allow: None, others_deny: None, path_pattern: None,
    }]);
    let index = GrantsIndex { by_group };
    let groups = vec!["share-1".to_string()];

    assert!(!index.user_has_descendant_grants(&groups, "/A/C"));
    assert!(index.accessible_child_names(&groups, "/A/C").is_empty());
  }
}
