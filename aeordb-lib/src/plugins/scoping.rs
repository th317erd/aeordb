/// Parsed components of a plugin path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPath {
  pub database: Option<String>,
  pub schema: Option<String>,
  pub table: Option<String>,
  pub function_name: Option<String>,
}

/// Parse a slash-separated plugin path into its components.
///
/// Paths follow the pattern: "database/schema/table/function_name"
/// Fewer segments are allowed — only as many as are present will be populated.
pub fn parse_plugin_path(path: &str) -> PluginPath {
  let segments: Vec<&str> = path
    .split('/')
    .filter(|segment| !segment.is_empty())
    .collect();

  PluginPath {
    database: segments.first().map(|s| s.to_string()),
    schema: segments.get(1).map(|s| s.to_string()),
    table: segments.get(2).map(|s| s.to_string()),
    function_name: segments.get(3).map(|s| s.to_string()),
  }
}

/// Given a current scope path and a function name, return the list of paths
/// to search from most specific to least specific.
///
/// For example, with current_path "mydb/public/users" and function_name "validate":
///   - "mydb/public/users/validate"
///   - "mydb/public/validate"
///   - "mydb/validate"
pub fn resolve_function_path(current_path: &str, function_name: &str) -> Vec<String> {
  let segments: Vec<&str> = current_path
    .split('/')
    .filter(|segment| !segment.is_empty())
    .collect();

  let mut paths = Vec::with_capacity(segments.len());

  for depth in (0..segments.len()).rev() {
    let prefix = segments[..=depth].join("/");
    paths.push(format!("{}/{}", prefix, function_name));
  }

  // Also add standalone function name at the global scope
  // (but only the hierarchical ones are typically searched).
  paths
}

/// Check whether a target path is accessible from a requester path.
///
/// Parent scopes are accessible (a child can call functions at its parent level).
/// Same-level scopes are accessible.
/// Sibling scopes are NOT accessible (can't reach into a sibling's subtree).
///
/// The rule: target must be a prefix of (or equal to) the requester's ancestor chain.
/// In other words, every segment of target must match the corresponding segment of requester.
pub fn is_scope_accessible(requester_path: &str, target_path: &str) -> bool {
  let requester_segments: Vec<&str> = requester_path
    .split('/')
    .filter(|segment| !segment.is_empty())
    .collect();
  let target_segments: Vec<&str> = target_path
    .split('/')
    .filter(|segment| !segment.is_empty())
    .collect();

  // Target must not be deeper than requester (can't reach into sibling's children).
  if target_segments.len() > requester_segments.len() {
    return false;
  }

  // Every segment of the target must match the corresponding requester segment.
  for (index, target_segment) in target_segments.iter().enumerate() {
    if requester_segments[index] != *target_segment {
      return false;
    }
  }

  true
}
