#[test]
fn authoritative_backup_and_import_paths_contain_no_panic_methods() {
  let source = include_str!("../../src/engine/backup.rs");
  let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);

  assert!(!production.contains(".expect("));
  assert!(!production.contains("unreachable!("));
}
