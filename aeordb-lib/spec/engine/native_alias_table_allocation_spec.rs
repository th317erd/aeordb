//! Direct table and final-name failure injection, beyond retained-record copies.
use super::*;

fn repeated_long_alias_source() -> (String, Vec<u8>) {
  let alias = "x".repeat(997);
  let mut source = String::from("{\"$v\":1,\"parsers\":{");
  for index in 0..17 {
    if index != 0 {
      source.push(',');
    }
    source.push_str(&format!("\"text/a{index:02}\":\"{alias}\""));
  }
  source.push_str("}}");
  (alias, source.into_bytes())
}

#[test]
fn native_alias_snapshot_table_allocator_refusal_releases_then_retries() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-table-allocation", None, [1; 16]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let (alias, source) = repeated_long_alias_source();
  let request = snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(&source));
  let row_bytes = std::mem::size_of::<String>() + 2 * std::mem::size_of::<Option<Vec<u8>>>() + std::mem::align_of::<String>();
  let table_bytes = 17 * row_bytes;
  let (prepared, observed) =
    allocation_probe::measure_nth(table_bytes, usize::MAX, || capture.prepare_current_semantic_alias_snapshot(request));
  let prepared = prepared.unwrap();
  assert_eq!(observed.matching_requests, 1, "table-sized allocation must be unambiguous: {observed:?}");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes - baseline, (4096 + table_bytes + 17 * alias.len()) as u64);
  assert!(prepared.resolve_parser_alias(&alias).unwrap().is_none());
  drop(prepared);
  let (result, allocations) = allocation_probe::measure(table_bytes, || capture.prepare_current_semantic_alias_snapshot(request));
  assert!(allocations.injected_failure && allocations.matching_requests == 1, "{allocations:?}");
  assert!(matches!(result, Err(NativeSemanticPluginSourceErrorV1::Identity(SemanticCompilationErrorV1::Resource { .. }))));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(capture.prepare_current_semantic_alias_snapshot(request).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_alias_snapshot_last_name_allocator_refusal_releases_partial_table_then_retries() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("alias-name-allocation", None, [1; 16]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let (alias, source) = repeated_long_alias_source();
  let request = snapshot_request(SemanticSourceAliasKindV1::ParserRegistry, Some(&source));
  // The final matching allocation is the last copied table name: schema
  // parsing precedes it, and missing-alias lookup only builds fixed-size paths.
  let (prepared, observed) =
    allocation_probe::measure_nth(alias.len(), usize::MAX, || capture.prepare_current_semantic_alias_snapshot(request));
  drop(prepared.unwrap());
  assert!(observed.matching_requests >= 17 && !observed.injected_failure);
  let (result, allocations) =
    allocation_probe::measure_nth(alias.len(), observed.matching_requests, || capture.prepare_current_semantic_alias_snapshot(request));
  assert!(allocations.injected_failure && allocations.matching_requests == observed.matching_requests, "{allocations:?}");
  assert!(matches!(result, Err(NativeSemanticPluginSourceErrorV1::Identity(SemanticCompilationErrorV1::Resource { .. }))));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  drop(capture.prepare_current_semantic_alias_snapshot(request).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}
