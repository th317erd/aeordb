//! Native staged catalog progress survives close/reopen without selecting HEAD.
use super::*;
use aeordb::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogConfigurationMutationV1, SemanticCatalogContinuationV1, admit_semantic_catalog_v1,
};
use aeordb::engine::v4::semantic_mutation_control::{decode_semantic_mutation_checkpoint, encode_semantic_mutation_checkpoint};

fn checkpoint(
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

fn native_restarts(prune: bool) {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (directory, coordinator, publisher) = initialized_publisher(algorithm);
    drop(coordinator);
    let path = directory.path().join("migration-execution.aeordb");
    let initial = publisher.observe().unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 32 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: None,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &Snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let configuration = || {
      compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source: br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#,
          owner_path: "/",
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 128 << 20,
        },
        &Snapshot,
        &memory,
        &|| false,
      )
      .unwrap()
    };
    let request = SemanticCatalogCompilationRequestV1 {
      hash_algorithm: algorithm,
      expected_configuration_count: u64::from(!prune),
      required_capabilities: [0; 32],
      maximum_workspace_bytes: 64 << 20,
    };
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let mut store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      initial.selected.header.database_id,
      initial.selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    let original = compile_semantic_catalog_v1(
      SemanticCatalogCompilationRequestV1 { expected_configuration_count: u64::from(prune), ..request },
      &registry,
      prune.then(|| Ok(configuration())),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    let work = if prune {
      SemanticCatalogContinuationV1::from_complete(request, &original, &registry, &store, &memory, &|| false)
        .unwrap()
        .apply(SemanticCatalogConfigurationMutationV1::Remove("/".into()), &mut store)
        .unwrap()
    } else {
      SemanticCatalogContinuationV1::start(request, &registry, &mut store, &memory, &|| false).unwrap()
    };
    let original_state = original.semantic_state().clone();
    drop(original);
    let original = original_state;
    let mut bytes = checkpoint(&work, &original, request.expected_configuration_count, algorithm);
    drop(work);
    drop(protection);
    drop(publisher);
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let steps = if prune { 6 } else { 3 };
    let mut completed = None;
    for step in 0..steps {
      let reopened = V4FirstAuthorityPublisher::open(&path).unwrap();
      let observation = reopened.observe().unwrap();
      assert_eq!(observation.selected.header.head_hash, initial.selected.header.head_hash);
      let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
      let mut store = NativeSemanticCatalogStagingStoreV1::new(
        &protection,
        initial.selected.header.database_id,
        observation.selected.header.updated_at_ms + 1,
        &cancellation,
      )
      .unwrap();
      let before = std::fs::read(&path).unwrap();
      let progress = admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &|| false).unwrap();
      let work = SemanticCatalogContinuationV1::from_progress(request, progress, &registry, &store, &memory, &|| false).unwrap();
      assert_eq!(std::fs::read(&path).unwrap(), before, "admission/resume is read-only");
      if step == steps - 1 {
        completed = Some(work.finish(&mut store).unwrap().semantic_state().clone());
      } else {
        let work = if !prune && step == 0 {
          work.apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration()), &mut store).unwrap()
        } else if (!prune && step == 1) || (prune && step == 0) {
          work.finish_configurations(&mut store).unwrap()
        } else {
          let next = work.prune_one(&mut store).unwrap();
          assert_eq!(next.pruning_candidates().record_count, 4 - step as u64);
          next
        };
        bytes = checkpoint(&work, &original, request.expected_configuration_count, algorithm);
      }
      assert_eq!(reopened.observe().unwrap().selected.header.head_hash, initial.selected.header.head_hash);
      drop(protection);
      drop(reopened);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    let completed = completed.unwrap();
    let reopened = V4FirstAuthorityPublisher::open(&path).unwrap();
    let observation = reopened.observe().unwrap();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let store = NativeSemanticCatalogStagingStoreV1::new(
      &protection,
      initial.selected.header.database_id,
      observation.selected.header.updated_at_ms + 1,
      &cancellation,
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let admitted = admit_semantic_catalog_v1(request, &completed.object_id, &registry, &store, &memory, &|| false).unwrap();
    assert_eq!(admitted.semantic_state(), &completed);
    assert_eq!(admitted.configuration_count(), request.expected_configuration_count);
    drop(admitted);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(reopened.observe().unwrap().selected.header.head_hash, initial.selected.header.head_hash);
    drop(protection);
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn catalog_continuation_fresh_progress_reopens_after_each_phase_at_every_hash() {
  native_restarts(false);
}

#[test]
fn catalog_continuation_pruning_progress_reopens_after_each_dependency_at_every_hash() {
  native_restarts(true);
}
