//! Targeted continuation root-copy refusals, not universal host OOM recovery.
use super::*;
use super::super::super::measure_nth;
use aeordb::engine::v4::semantic_catalog_compiler::{SemanticCatalogContinuationV1, admit_semantic_catalog_progress_v1};

#[test]
fn catalog_continuation_partial_root_copies_refuse_before_reads_and_release_both_leases() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 32 << 20).unwrap());
  let registry = compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: None,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    },
    &NoAliases,
    &memory,
    &|| false,
  )
  .unwrap();
  let request = SemanticCatalogCompilationRequestV1 {
    hash_algorithm: algorithm,
    expected_configuration_count: 0,
    required_capabilities: [0; 32],
    maximum_workspace_bytes: 64 << 20,
  };
  let configuration = compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#,
      owner_path: "/",
      registry: &registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &NoAliases,
    &memory,
    &|| false,
  )
  .unwrap();
  let mut store = Objects::default();
  let base = compile_semantic_catalog_v1(
    SemanticCatalogCompilationRequestV1 { expected_configuration_count: 1, ..request },
    &registry,
    [Ok(configuration)],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap();
  let work = SemanticCatalogContinuationV1::from_complete(request, &base, &registry, &store, &memory, &|| false)
    .unwrap()
    .apply(SemanticCatalogConfigurationMutationV1::Remove("/".into()), &mut store)
    .unwrap()
    .finish_configurations(&mut store)
    .unwrap();
  let bytes = input::checkpoint(algorithm, base.semantic_state(), work.catalog(), work.pruning_candidates(), work.dependency_count());
  assert_eq!(work.pruning_candidates().record_count, 4);
  drop(work);
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for occurrence in [1, 2] {
    // Admission/registry initialization and test-store copies finish before
    // arming. The two next H-sized allocations are main and candidate copies.
    let admitted = admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &|| false).unwrap();
    let before = (store.reads.get(), store.writes);
    let (result, allocations) = measure_nth(algorithm.hash_length(), occurrence, || {
      SemanticCatalogContinuationV1::from_progress(request, admitted, &registry, &store, &memory, &|| false)
    });
    assert!(allocations.injected_failure, "copy {occurrence}: {allocations:?}");
    assert_eq!(allocations.matching_requests, occurrence);
    let error = result.err().expect("refused continuation copy cannot return work");
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error)
      if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit && error.code() == "semantic_catalog_allocation"));
    assert_eq!((store.reads.get(), store.writes), before);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    let admitted = admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &|| false).unwrap();
    let retry = SemanticCatalogContinuationV1::from_progress(request, admitted, &registry, &store, &memory, &|| false).unwrap();
    drop(retry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
}
