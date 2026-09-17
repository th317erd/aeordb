//! Actual allocator refusals in the assembler and its shared encoders.
use super::*;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use aeordb::engine::v4::semantic_source_capture::{
  build_semantic_source_catalog_pair_v1, SemanticSourceCatalogBuildRequestV1, SemanticSourceCatalogPairRowV1, SemanticSourceLeafEntryV1,
};

fn input(algorithm: HashAlgorithm) -> (SemanticSourceCatalogBuildRequestV1, Vec<SemanticSourceCatalogPairRowV1>) {
  let request = SemanticSourceCatalogBuildRequestV1 {
    database_id: [1; 16],
    hash_algorithm: algorithm,
    expected_path_count: 2,
    maximum_path_bytes: 16,
    maximum_workspace_bytes: 3 << 20,
    maximum_node_pairs: 1,
    maximum_output_bytes: 1 << 20,
  };
  let rows = ["/aa", "/bb"]
    .into_iter()
    .map(|path| SemanticSourceCatalogPairRowV1 {
      path: path.into(),
      base_file_record_id: None,
      requested_file_record_id: Some(vec![1; algorithm.hash_length()]),
    })
    .collect();
  (request, rows)
}

#[test]
fn catalog_build_each_measured_workspace_path_projection_output_and_identity_allocation_refuses_cleanly() {
  for (algorithm, _) in PROFILES {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap());
    // Independent ASCN body plus the existing32-byte envelope and4-byte CRC.
    let encoded_bytes = 32 + 2 * (4 + 3 + algorithm.hash_length()) + 36;
    for size in [
      2 * std::mem::size_of::<SemanticSourceCatalogPairRowV1>(),
      3,
      2 * std::mem::size_of::<SemanticSourceLeafEntryV1<'_>>(),
      encoded_bytes,
      algorithm.hash_length(),
    ] {
      let (request, rows) = input(algorithm);
      let (warm, allocations) = measure_nth(size, usize::MAX, || {
        build_semantic_source_catalog_pair_v1(request, rows.into_iter().map(Ok), &mut |_, _| Ok(()), &memory, &|| false)
      });
      drop(warm.unwrap());
      assert!(allocations.matching_requests > 0, "unexercised allocation {size}");
      assert!(allocations.matching_requests <= 16, "unexpected allocation repetition: {allocations:?}");
      for occurrence in 1..=allocations.matching_requests {
        let (request, rows) = input(algorithm);
        let (result, observed) = measure_nth(size, occurrence, || {
          build_semantic_source_catalog_pair_v1(request, rows.into_iter().map(Ok), &mut |_, _| Ok(()), &memory, &|| false)
        });
        assert!(observed.injected_failure, "{size}/{occurrence}: {observed:?}");
        assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })), "refusal did not remain operational");
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
        let (request, rows) = input(algorithm);
        let retry =
          build_semantic_source_catalog_pair_v1(request, rows.into_iter().map(Ok), &mut |_, _| Ok(()), &memory, &|| false).unwrap();
        drop(retry);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      }
    }
  }
}

#[test]
fn catalog_build_large_declared_count_does_not_allocate_an_input_sized_collection() {
  for (algorithm, _) in PROFILES {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap());
    let (mut request, _) = input(algorithm);
    request.expected_path_count = 1_000_000_000;
    let (result, allocations) = measure(0, || {
      build_semantic_source_catalog_pair_v1(
        request,
        std::iter::empty(),
        &mut |_, _| panic!("empty input must not emit a node"),
        &memory,
        &|| false,
      )
    });
    assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })));
    assert!(allocations.maximum < 32 << 10, "{allocations:?}");
    assert!(allocations.total < 64 << 10, "{allocations:?}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
