use aeordb::engine::path_utils::{file_name, normalize_path, parent_path, path_segments};

// ===========================================================================
// normalize_path
// ===========================================================================

#[test]
fn normalize_simple_path() {
    assert_eq!(normalize_path("/foo/bar"), "/foo/bar");
}

#[test]
fn normalize_strips_trailing_slash() {
    assert_eq!(normalize_path("/foo/bar/"), "/foo/bar");
}

#[test]
fn normalize_collapses_double_slashes() {
    assert_eq!(normalize_path("/foo//bar"), "/foo/bar");
    assert_eq!(normalize_path("//foo///bar////baz"), "/foo/bar/baz");
}

#[test]
fn normalize_resolves_dot_segments() {
    assert_eq!(normalize_path("/foo/./bar"), "/foo/bar");
    assert_eq!(normalize_path("/./foo/./bar/."), "/foo/bar");
}

#[test]
fn normalize_resolves_dotdot_traversal() {
    assert_eq!(normalize_path("/foo/bar/../baz"), "/foo/baz");
    assert_eq!(normalize_path("/foo/bar/../../baz"), "/baz");
}

#[test]
fn normalize_dotdot_at_root_stays_at_root() {
    // Going above root should silently stop at root.
    assert_eq!(normalize_path("/foo/../.."), "/");
    assert_eq!(normalize_path("/../../.."), "/");
    assert_eq!(normalize_path("/../"), "/");
}

#[test]
fn normalize_empty_path_returns_root() {
    assert_eq!(normalize_path(""), "/");
}

#[test]
fn normalize_whitespace_only_returns_root() {
    assert_eq!(normalize_path("   "), "/");
    assert_eq!(normalize_path("\t\n"), "/");
}

#[test]
fn normalize_root_stays_root() {
    assert_eq!(normalize_path("/"), "/");
}

#[test]
fn normalize_strips_null_bytes() {
    assert_eq!(normalize_path("/foo\0bar"), "/foobar");
    assert_eq!(normalize_path("\0\0"), "/");
}

#[test]
fn normalize_null_bytes_only() {
    assert_eq!(normalize_path("\0"), "/");
    assert_eq!(normalize_path("\0\0\0"), "/");
}

#[test]
fn normalize_unicode_preserved() {
    assert_eq!(normalize_path("/\u{1F600}/data"), "/\u{1F600}/data");
    assert_eq!(
        normalize_path("/\u{30D5}\u{30A1}\u{30A4}\u{30EB}/\u{30C6}\u{30B9}\u{30C8}"),
        "/\u{30D5}\u{30A1}\u{30A4}\u{30EB}/\u{30C6}\u{30B9}\u{30C8}"
    );
}

#[test]
fn normalize_mixed_traversal_and_slashes() {
    assert_eq!(normalize_path("///foo/./bar//../baz//"), "/foo/baz");
}

#[test]
fn normalize_relative_path_gets_leading_slash() {
    assert_eq!(normalize_path("foo/bar"), "/foo/bar");
}

#[test]
fn normalize_only_dots() {
    assert_eq!(normalize_path("."), "/");
    assert_eq!(normalize_path(".."), "/");
    assert_eq!(normalize_path("./.."), "/");
}

#[test]
fn normalize_single_segment() {
    assert_eq!(normalize_path("/hello"), "/hello");
    assert_eq!(normalize_path("hello"), "/hello");
}

#[test]
fn normalize_deeply_nested() {
    assert_eq!(normalize_path("/a/b/c/d/e"), "/a/b/c/d/e");
}

#[test]
fn normalize_leading_whitespace_trimmed() {
    assert_eq!(normalize_path("  /foo/bar  "), "/foo/bar");
}

// ===========================================================================
// parent_path
// ===========================================================================

#[test]
fn parent_of_root_is_none() {
    assert_eq!(parent_path("/"), None);
}

#[test]
fn parent_of_top_level_file() {
    assert_eq!(parent_path("/hello.txt"), Some("/".to_string()));
}

#[test]
fn parent_of_nested_path() {
    assert_eq!(parent_path("/foo/bar/baz"), Some("/foo/bar".to_string()));
}

#[test]
fn parent_normalizes_input() {
    // Double-slash, trailing slash, dotdot -- normalization should happen first.
    assert_eq!(parent_path("/foo//bar/"), Some("/foo".to_string()));
    assert_eq!(parent_path("/a/b/../c"), Some("/a".to_string()));
}

#[test]
fn parent_of_empty_input() {
    // Empty normalizes to "/", parent of "/" is None.
    assert_eq!(parent_path(""), None);
}

// ===========================================================================
// file_name
// ===========================================================================

#[test]
fn file_name_simple() {
    assert_eq!(file_name("/foo/bar.txt"), Some("bar.txt"));
}

#[test]
fn file_name_root_is_none() {
    assert_eq!(file_name("/"), None);
}

#[test]
fn file_name_empty_is_none() {
    assert_eq!(file_name(""), None);
}

#[test]
fn file_name_trailing_slash_stripped() {
    assert_eq!(file_name("/foo/bar/"), Some("bar"));
}

#[test]
fn file_name_no_slash() {
    assert_eq!(file_name("just_a_name"), Some("just_a_name"));
}

#[test]
fn file_name_whitespace_only_is_none() {
    assert_eq!(file_name("   "), None);
}

#[test]
fn file_name_top_level() {
    assert_eq!(file_name("/top"), Some("top"));
}

// ===========================================================================
// path_segments
// ===========================================================================

#[test]
fn segments_simple() {
    assert_eq!(path_segments("/foo/bar/baz"), vec!["foo", "bar", "baz"]);
}

#[test]
fn segments_filters_dots() {
    assert_eq!(path_segments("/foo/./bar/../baz"), vec!["foo", "bar", "baz"]);
}

#[test]
fn segments_root_is_empty() {
    let result: Vec<&str> = path_segments("/");
    assert!(result.is_empty());
}

#[test]
fn segments_empty_input() {
    let result: Vec<&str> = path_segments("");
    assert!(result.is_empty());
}

#[test]
fn segments_collapses_empty_segments() {
    assert_eq!(path_segments("//foo///bar//"), vec!["foo", "bar"]);
}

#[test]
fn segments_relative() {
    assert_eq!(path_segments("a/b/c"), vec!["a", "b", "c"]);
}

#[test]
fn segments_single() {
    assert_eq!(path_segments("hello"), vec!["hello"]);
}
