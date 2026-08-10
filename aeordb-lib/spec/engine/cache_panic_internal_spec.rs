#[test]
fn fallible_cache_and_directory_paths_contain_no_panic_assumptions() {
  let cache_source = include_str!("../../src/engine/cache.rs");
  let cache_production = cache_source.split("\n#[cfg(test)]").next().unwrap_or(cache_source);
  assert!(!cache_production.contains(".expect(\"replacement cache entry"));
  assert!(!cache_production.contains("unreachable!(\"cache load wait"));

  let directory_source = include_str!("../../src/engine/directory_ops.rs");
  let directory_production = directory_source.split("\n#[cfg(test)]\nmod tests").next().unwrap_or(directory_source);
  assert!(!directory_production.contains("unreachable!(\"entry type filtered above\")"));
  assert!(!directory_production.contains("unreachable!(\"entry type was constrained above\")"));
}
