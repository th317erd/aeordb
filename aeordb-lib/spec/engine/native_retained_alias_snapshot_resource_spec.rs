//! Retained inputs do not become retained module buffers; copies remain fallible.
use super::*;

fn fixture(publisher: &V4FirstAuthorityPublisher, module: &[u8]) {
  let (alias, artifact) = seed_module(publisher, module, "both");
  let mut rows = [
    (INDEX_SOURCE.to_string(), None),
    (PARSER_SOURCE.to_string(), None),
    (fixtures::alias_path(), Some(alias)),
    (fixtures::artifact_path(module), Some(artifact)),
  ];
  rows.sort_by(|left, right| left.0.cmp(&right.0));
  let rows: Vec<_> = rows.iter().map(|(path, hash)| (path.as_str(), hash.as_deref())).collect();
  seed_catalog_pair(publisher, &rows, &rows);
}

const SOURCE: &[u8] =
  br#"{"$v":1,"parser":"parse","indexes":[{"name":"value","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;

#[test]
fn retained_alias_snapshot_releases_catalog_and_large_module_memory_before_returning_borrows() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-memory", None, [1; 16]);
  let mut module = fixtures::module("both");
  fixtures::custom("padding", &vec![0; 2 << 20], &mut module);
  fixture(&publisher, &module);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(SOURCE));
  request.plugins.maximum_module_bytes = 4 << 20;
  request.plugins.maximum_chunk_entity_bytes = 4 << 20;
  let mut bounds = catalog_bounds();
  bounds.maximum_source_bytes = 4 << 20;
  bounds.maximum_chunk_entity_bytes = 4 << 20;
  for side in [SemanticSourceCatalogSideV1::Base, SemanticSourceCatalogSideV1::Requested] {
    let snapshot = capture.prepare_captured_semantic_alias_snapshot(&[2; 16], 1, side, request, bounds).unwrap();
    assert!(memory.snapshot().unwrap().reserved_bytes - retained < 64 << 10, "only bounded alias/dependency metadata survives");
    assert_eq!(snapshot.resolve_parser_alias("parse").unwrap().unwrap().artifact_length, module.len() as u64);
    assert_eq!(
      encode_dependency_record(&snapshot.resolve_mapper_alias("parse").unwrap().unwrap()).unwrap(),
      expected_dependency(&module, 2)
    );
    drop(snapshot);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn retained_alias_snapshot_actual_dependency_copy_refusals_release_all_leases_and_retry() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-copy", None, [1; 16]);
  let module = fixtures::module("both");
  fixture(&publisher, &module);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(SOURCE));
  let size = expected_dependency(&module, 1).len();
  let prepare =
    || capture.prepare_captured_semantic_alias_snapshot(&[2; 16], 1, SemanticSourceCatalogSideV1::Requested, request, catalog_bounds());
  let (baseline, observed) = allocation_probe::measure_nth(size, usize::MAX, prepare);
  drop(baseline.unwrap());
  assert!(observed.matching_requests >= 4 && !observed.injected_failure);
  // Both copied role records are the final size-matching allocations, after
  // catalog/source decoding and pair construction have completed. This targets
  // those fallible copies, not every inherited allocation in the input readers.
  for occurrence in [observed.matching_requests - 1, observed.matching_requests] {
    let (result, allocations) = allocation_probe::measure_nth(size, occurrence, prepare);
    assert!(allocations.injected_failure && allocations.matching_requests == occurrence, "{allocations:?}");
    let error = result.err().expect("failed dependency copy cannot return a prepared snapshot");
    assert!(matches!(
      error,
      NativeSemanticPluginSourceErrorV1::Identity(crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1::Resource {
        path: "<native-semantic-alias-snapshot>",
        ..
      })
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    drop(prepare().unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn retained_alias_snapshot_body_chunk_work_and_preparation_bounds_refuse_then_retry() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("retained-alias-bounds", None, [1; 16]);
  let module = fixtures::module("both");
  fixture(&publisher, &module);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for mode in 0..9 {
    let mut request = snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(SOURCE));
    let mut bounds = catalog_bounds();
    match mode {
      0 => request.plugins.maximum_module_bytes = module.len() - 1,
      1 => bounds.maximum_source_bytes = 1,
      2 => request.plugins.maximum_chunk_entity_bytes = 1,
      3 => bounds.maximum_work = 1,
      4 => request.maximum_snapshot_bytes = 1,
      5 => request.source.maximum_source_bytes = 1,
      6 => request.source.maximum_workspace_bytes = 1,
      7 => request.source.maximum_alias_occurrences = 1,
      8 => request.plugins.maximum_workspace_bytes = 1,
      _ => unreachable!(),
    }
    assert!(
      capture.prepare_captured_semantic_alias_snapshot(&[2; 16], 1, SemanticSourceCatalogSideV1::Requested, request, bounds).is_err(),
      "mode{mode}"
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    drop(
      capture
        .prepare_captured_semantic_alias_snapshot(
          &[2; 16],
          1,
          SemanticSourceCatalogSideV1::Requested,
          snapshot_request(SemanticSourceAliasKindV1::IndexConfiguration, Some(SOURCE)),
          catalog_bounds(),
        )
        .unwrap(),
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}
