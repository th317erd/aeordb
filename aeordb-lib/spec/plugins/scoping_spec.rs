use aeordb::plugins::scoping::{
  is_scope_accessible, parse_plugin_path, resolve_function_path,
};

#[test]
fn test_resolve_function_searches_upward() {
  let paths = resolve_function_path("mydb/public/users", "validate");
  assert_eq!(paths.len(), 3);
  assert_eq!(paths[0], "mydb/public/users/validate");
  assert_eq!(paths[1], "mydb/public/validate");
  assert_eq!(paths[2], "mydb/validate");
}

#[test]
fn test_resolve_function_single_segment() {
  let paths = resolve_function_path("mydb", "validate");
  assert_eq!(paths.len(), 1);
  assert_eq!(paths[0], "mydb/validate");
}

#[test]
fn test_resolve_function_two_segments() {
  let paths = resolve_function_path("mydb/public", "validate");
  assert_eq!(paths.len(), 2);
  assert_eq!(paths[0], "mydb/public/validate");
  assert_eq!(paths[1], "mydb/validate");
}

#[test]
fn test_most_specific_scope_wins() {
  // The first element in the returned list is the most specific.
  let paths = resolve_function_path("a/b/c/d", "fn1");
  assert_eq!(paths[0], "a/b/c/d/fn1");
  assert_eq!(paths[1], "a/b/c/fn1");
  assert_eq!(paths[2], "a/b/fn1");
  assert_eq!(paths[3], "a/fn1");
}

#[test]
fn test_scope_accessible_to_children() {
  // A child scope can access parent-level plugins.
  assert!(is_scope_accessible("mydb/public/users", "mydb/public"));
  assert!(is_scope_accessible("mydb/public/users", "mydb"));
}

#[test]
fn test_scope_accessible_to_same_level() {
  assert!(is_scope_accessible("mydb/public/users", "mydb/public/users"));
}

#[test]
fn test_scope_not_accessible_to_siblings() {
  // "mydb/public/orders" is a sibling of "mydb/public/users" — not accessible.
  assert!(!is_scope_accessible(
    "mydb/public/users",
    "mydb/public/orders"
  ));
}

#[test]
fn test_scope_not_accessible_to_parents_children() {
  // "mydb/other/stuff" is a child of a sibling — not accessible from "mydb/public/users".
  assert!(!is_scope_accessible(
    "mydb/public/users",
    "mydb/other/stuff"
  ));
}

#[test]
fn test_scope_not_accessible_deeper_than_requester() {
  // Target is deeper than requester — can't reach down into children of another branch.
  assert!(!is_scope_accessible("mydb/public", "mydb/public/users/extra"));
}

#[test]
fn test_parse_plugin_path() {
  let path = parse_plugin_path("mydb/public/users/validate");
  assert_eq!(path.database, Some("mydb".to_string()));
  assert_eq!(path.schema, Some("public".to_string()));
  assert_eq!(path.table, Some("users".to_string()));
  assert_eq!(path.function_name, Some("validate".to_string()));
}

#[test]
fn test_parse_plugin_path_partial() {
  let path = parse_plugin_path("mydb/public");
  assert_eq!(path.database, Some("mydb".to_string()));
  assert_eq!(path.schema, Some("public".to_string()));
  assert_eq!(path.table, None);
  assert_eq!(path.function_name, None);
}

#[test]
fn test_parse_plugin_path_empty() {
  let path = parse_plugin_path("");
  assert_eq!(path.database, None);
  assert_eq!(path.schema, None);
  assert_eq!(path.table, None);
  assert_eq!(path.function_name, None);
}

#[test]
fn test_parse_plugin_path_single_segment() {
  let path = parse_plugin_path("mydb");
  assert_eq!(path.database, Some("mydb".to_string()));
  assert_eq!(path.schema, None);
}

#[test]
fn test_scope_accessible_root_to_root() {
  // Empty/root scope is accessible from anywhere as an ancestor.
  // But since empty segments are filtered, an empty target has 0 segments,
  // which means it's always a prefix of anything.
  assert!(is_scope_accessible("mydb/public/users", ""));
}

#[test]
fn test_resolve_function_path_empty_current() {
  let paths = resolve_function_path("", "validate");
  assert!(paths.is_empty());
}
