use crate::engine::path_utils::{file_name, normalize_path, parent_path};
use crate::server::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CacheInvalidation {
  Permissions(String),
  GrantsIndex,
  IndexConfig(String),
  ApiKey(String),
  Groups,
}

fn invalidations_for_path(path: &str) -> Vec<CacheInvalidation> {
  let normalized = normalize_path(path);
  let mut invalidations = Vec::new();

  if normalized.ends_with("/.aeordb-permissions") || normalized == "/.aeordb-permissions" {
    let parent = parent_path(&normalized).unwrap_or_else(|| "/".to_string());
    invalidations.push(CacheInvalidation::Permissions(parent));
    invalidations.push(CacheInvalidation::GrantsIndex);
  }

  if normalized.ends_with("/.aeordb-config/indexes.json") {
    if let Some(dir) = normalized.strip_suffix("/.aeordb-config/indexes.json") {
      invalidations.push(CacheInvalidation::IndexConfig(if dir.is_empty() { "/".to_string() } else { dir.to_string() }));
    }
  }

  if normalized.starts_with("/.aeordb-system/api-keys/") {
    if let Some(key_id) = file_name(&normalized) {
      invalidations.push(CacheInvalidation::ApiKey(key_id.to_string()));
    }
  }

  if normalized.starts_with("/.aeordb-system/groups/") || normalized.starts_with("/.aeordb-system/users/") {
    invalidations.push(CacheInvalidation::Groups);
  }

  invalidations
}

/// Evict route-visible caches affected by a path write, delete, or rename.
///
/// This intentionally stays path-based rather than operation-based because the
/// same engine mutations are reachable through PUT, upload commit, merge,
/// delete, restore, copy, and rename routes.
pub fn evict_caches_for_path(state: &AppState, path: &str) {
  for invalidation in invalidations_for_path(path) {
    match invalidation {
      CacheInvalidation::Permissions(parent) => state.engine.permissions_cache.evict(&parent),
      CacheInvalidation::GrantsIndex => state.engine.grants_index_cache.evict_all(),
      CacheInvalidation::IndexConfig(path) => state.engine.index_config_cache.evict(&path),
      CacheInvalidation::ApiKey(key_id) => state.api_key_cache.evict(&key_id),
      CacheInvalidation::Groups => state.group_cache.evict_all(),
    }
  }
}

pub fn evict_caches_for_paths<I, P>(state: &AppState, paths: I)
where
  I: IntoIterator<Item = P>,
  P: AsRef<str>,
{
  for path in paths {
    evict_caches_for_path(state, path.as_ref());
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn classifies_permission_files() {
    assert_eq!(
      invalidations_for_path("/projects/.aeordb-permissions"),
      vec![CacheInvalidation::Permissions("/projects".to_string()), CacheInvalidation::GrantsIndex]
    );
    assert_eq!(
      invalidations_for_path("/.aeordb-permissions"),
      vec![CacheInvalidation::Permissions("/".to_string()), CacheInvalidation::GrantsIndex]
    );
  }

  #[test]
  fn classifies_index_config_files() {
    assert_eq!(invalidations_for_path("/.aeordb-config/indexes.json"), vec![CacheInvalidation::IndexConfig("/".to_string())]);
    assert_eq!(
      invalidations_for_path("/projects/.aeordb-config/indexes.json"),
      vec![CacheInvalidation::IndexConfig("/projects".to_string())]
    );
  }

  #[test]
  fn classifies_system_principal_files() {
    assert_eq!(invalidations_for_path("/.aeordb-system/api-keys/abc123"), vec![CacheInvalidation::ApiKey("abc123".to_string())]);
    assert_eq!(invalidations_for_path("/.aeordb-system/groups/editors.json"), vec![CacheInvalidation::Groups]);
    assert_eq!(invalidations_for_path("/.aeordb-system/users/alice.json"), vec![CacheInvalidation::Groups]);
  }
}
