#[test]
fn production_probe_paths_contain_no_panic_methods() {
  let source = include_str!("../../src/commands/probe.rs");

  assert!(!source.contains(".unwrap()"));
  assert!(!source.contains(".expect("));
  assert!(!source.contains("unreachable!("));
  assert!(!source.contains("entries_by_type(aeordb::engine::KV_TYPE_FILE_RECORD).unwrap_or_default()"));
  assert!(!source.contains("entries_by_type(aeordb::engine::KV_TYPE_DIRECTORY).unwrap_or_default()"));
  assert!(!source.contains("map_while(Result::ok)"));
  assert!(!source.contains("Err(_) => continue"));
}
