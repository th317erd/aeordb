//! Catalog-only drop/admit/continue; placeholder ASMC fields are not task proof.
use super::*;
use aeordb::engine::v4::semantic_catalog_compiler::admit_semantic_catalog_progress_v1;
use aeordb::engine::v4::semantic_mutation_control::{decode_semantic_mutation_checkpoint, encode_semantic_mutation_checkpoint};
#[path = "../support/semantic_progress_checkpoint.rs"]
mod input;

pub(super) fn checkpoint(
  work: &SemanticCatalogContinuationV1<'_>,
  original: &EncodedSemanticObjectV1,
  expected: u64,
  algorithm: HashAlgorithm,
) -> Vec<u8> {
  let bytes = input::checkpoint(algorithm, original, work.catalog(), work.pruning_candidates(), work.dependency_count());
  let mut decoded = decode_semantic_mutation_checkpoint(&bytes, algorithm).unwrap();
  decoded.phase = work.phase();
  decoded.configuration_count = work.configuration_count();
  decoded.expected_configuration_count = expected;
  encode_semantic_mutation_checkpoint(&decoded, algorithm).unwrap()
}

fn restart<'a>(
  work: SemanticCatalogContinuationV1<'a>,
  original: &EncodedSemanticObjectV1,
  request: SemanticCatalogCompilationRequestV1,
  registry: &'a CompiledParserRegistryV1,
  store: &Store,
  memory: &'a MemoryCoordinator,
) -> SemanticCatalogContinuationV1<'a> {
  let bytes = checkpoint(&work, original, request.expected_configuration_count, request.hash_algorithm);
  let before = (
    work.phase(),
    work.configuration_count(),
    work.dependency_count(),
    work.catalog().root_object_id.unwrap().to_vec(),
    work.pruning_candidates().root_object_id.map(<[u8]>::to_vec),
  );
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let writes = store.writes;
  drop(work);
  let admitted = admit_semantic_catalog_progress_v1(request, &bytes, registry, store, memory, &|| false).unwrap();
  let next = SemanticCatalogContinuationV1::from_progress(request, admitted, registry, store, memory, &|| false)
    .expect("admitted partial closure must continue under the same captured registry");
  assert_eq!(next.phase(), before.0);
  assert_eq!(next.configuration_count(), before.1);
  assert_eq!(next.dependency_count(), before.2);
  assert_eq!(next.catalog().root_object_id, Some(before.3.as_slice()));
  assert_eq!(next.pruning_candidates().root_object_id, before.4.as_deref());
  assert_eq!(store.writes, writes, "resume does not republish selected progress");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  next
}

pub(super) fn native_configuration(
  algorithm: HashAlgorithm,
  registry: &CompiledParserRegistryV1,
  memory: &MemoryCoordinator,
  owner: &str,
) -> CompiledIndexConfigurationV1 {
  compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#,
      owner_path: owner,
      registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &Snapshot,
    memory,
    &|| false,
  )
  .unwrap()
}

#[test]
fn catalog_continuation_restarts_after_every_whole_configuration_and_phase_boundary() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let original =
      compile_semantic_catalog_v1(request(algorithm, 0), &registry, [], &mut store, &memory, &|| false).unwrap().semantic_state().clone();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut work = SemanticCatalogContinuationV1::start(request(algorithm, 2), &registry, &mut store, &memory, &|| false).unwrap();
    work = restart(work, &original, request(algorithm, 2), &registry, &store, &memory);
    for owner in ["/", "/nested"] {
      work = work
        .apply(SemanticCatalogConfigurationMutationV1::Upsert(native_configuration(algorithm, &registry, &memory, owner)), &mut store)
        .unwrap();
      work = restart(work, &original, request(algorithm, 2), &registry, &store, &memory);
    }
    work = work.finish_configurations(&mut store).unwrap();
    work = restart(work, &original, request(algorithm, 2), &registry, &store, &memory);
    let result = work.finish(&mut store).unwrap();
    let mut fresh_store = Store::new(algorithm);
    let fresh = compile_semantic_catalog_v1(
      request(algorithm, 2),
      &registry,
      ["/", "/nested"].into_iter().map(|owner| Ok(native_configuration(algorithm, &registry, &memory, owner))),
      &mut fresh_store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(result.semantic_state(), fresh.semantic_state());
    let admitted =
      admit_semantic_catalog_v1(request(algorithm, 2), &result.semantic_state().object_id, &registry, &store, &memory, &|| false).unwrap();
    drop(admitted);
    drop(fresh);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
}

#[test]
fn catalog_continuation_restarts_live_candidates_and_each_dependency_pruning_step() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let base = compile_semantic_catalog_v1(
      request(algorithm, 2),
      &registry,
      ["/left", "/right"].into_iter().map(|owner| Ok(native_configuration(algorithm, &registry, &memory, owner))),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let work = SemanticCatalogContinuationV1::from_complete(request(algorithm, 0), &base, &registry, &store, &memory, &|| false).unwrap();
    let work = work.apply(SemanticCatalogConfigurationMutationV1::Remove("/left".into()), &mut store).unwrap();
    assert_eq!(work.pruning_candidates().record_count, 4);
    let work = restart(work, base.semantic_state(), request(algorithm, 0), &registry, &store, &memory);
    let work = work.apply(SemanticCatalogConfigurationMutationV1::Remove("/right".into()), &mut store).unwrap();
    let work = restart(work, base.semantic_state(), request(algorithm, 0), &registry, &store, &memory);
    let mut work = work.finish_configurations(&mut store).unwrap();
    work = restart(work, base.semantic_state(), request(algorithm, 0), &registry, &store, &memory);
    for remaining in (0..4).rev() {
      work = work.prune_one(&mut store).unwrap();
      assert_eq!(work.pruning_candidates().record_count, remaining);
      assert_eq!(work.catalog().record_count, remaining + 1);
      work = restart(work, base.semantic_state(), request(algorithm, 0), &registry, &store, &memory);
    }
    let result = work.finish(&mut store).unwrap();
    let mut fresh_store = Store::new(algorithm);
    let fresh = compile_semantic_catalog_v1(request(algorithm, 0), &registry, [], &mut fresh_store, &memory, &|| false).unwrap();
    assert_eq!(result.semantic_state(), fresh.semantic_state());
    drop(fresh);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
}
