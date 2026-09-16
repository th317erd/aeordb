//! Actual allocator refusal on the incremental compiler's retained buffers.
use std::cell::Cell;
use super::*;
use super::super::{FAIL_OCCURRENCE, FAIL_SIZE};
use aeordb::engine::v4::index_configuration_compiler::{
  IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
};
use aeordb::engine::v4::namespace::{decode_semantic_definition_record, decode_semantic_object};
use aeordb::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogConfigurationMutationV1, SemanticCatalogUpdateRequestV1, update_semantic_catalog_v1,
};

impl IndexConfigurationAliasSnapshotV1 for NoAliases {
  fn resolve_mapper_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    panic!("fieldless input must not resolve mapper aliases")
  }
}

#[derive(Default)]
struct Objects {
  values: Vec<(u16, EncodedSemanticObjectV1)>,
  arm_projection_copy: Cell<bool>,
  reads: Cell<usize>,
  writes: usize,
}
impl SemanticCatalogObjectSourceV1 for Objects {
  fn load_semantic_object(&self, kind: u16, identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    self.reads.set(self.reads.get() + 1);
    let value = self
      .values
      .iter()
      .find(|(stored_kind, object)| *stored_kind == kind && object.object_id == identity)
      .map(|(_, object)| object.value.clone());
    if kind == 4 && self.arm_projection_copy.get() {
      let definition = decode_semantic_definition_record(value.as_ref().unwrap(), HashAlgorithm::Blake3_256).unwrap();
      if definition.class == 1 {
        // Arm only after this test store's clone/inspection has finished.
        // The next allocation of this size must be production's retained body.
        self.arm_projection_copy.set(false);
        FAIL_SIZE.with(|target| target.set(definition.definition.len()));
        FAIL_OCCURRENCE.with(|remaining| remaining.set(1));
      }
    }
    Ok(value)
  }
}
impl SemanticCatalogStagingStoreV1 for Objects {
  fn publish_semantic_objects(&mut self, objects: &[EncodedSemanticObjectV1]) -> Result<(), SemanticCatalogReadErrorV1> {
    self.writes += 1;
    for object in objects {
      if let Some((_, previous)) = self.values.iter().find(|(_, previous)| previous.object_id == object.object_id) {
        assert_eq!(previous, object);
      } else {
        let kind = decode_semantic_object(&object.value, HashAlgorithm::Blake3_256).unwrap().kind_id;
        self.values.push((kind, object.clone()));
      }
    }
    Ok(())
  }
}

#[test]
fn admission_bitmap_allocation_refusal_releases_the_charge_and_allows_retry() {
  use aeordb::engine::v4::semantic_catalog_compiler::admit_semantic_catalog_v1;
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 8 << 20).unwrap());
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
  let mut store = Objects::default();
  let state =
    compile_semantic_catalog_v1(request, &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap().semantic_state().clone();
  let before = memory.snapshot().unwrap().reserved_bytes;
  let writes = store.writes;
  // A registry-only tree has exactly one record, so its reachability bitmap
  // requests one byte. No source/test allocation of this size occurs here.
  let (result, allocations) = measure(1, || admit_semantic_catalog_v1(request, &state.object_id, &registry, &store, &memory, &|| false));
  assert!(allocations.injected_failure);
  let error = match result {
    Err(error) => error,
    Ok(_) => panic!("refused bitmap returned an admitted base"),
  };
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error)
    if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit && error.code() == "semantic_catalog_reachability_memory"));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
  let admitted = admit_semantic_catalog_v1(request, &state.object_id, &registry, &store, &memory, &|| false).unwrap();
  assert_eq!(admitted.semantic_state(), &state);
  drop(admitted);
  assert_eq!(store.writes, writes);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
}

#[test]
fn refused_base_root_projection_copy_and_result_buffers_return_resource_without_leaks() {
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
    expected_configuration_count: 1,
    required_capabilities: [0; 32],
    maximum_workspace_bytes: 64 << 20,
  };
  let configuration = compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: br#"{"$v":1,"indexes":[]}"#,
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
  let previous = compile_semantic_catalog_v1(request, &registry, [Ok(configuration)], &mut store, &memory, &|| false).unwrap();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  for mode in 0..3 {
    let before = (store.reads.get(), store.writes);
    store.arm_projection_copy.set(mode == 1);
    let size = match mode {
      0 => algorithm.hash_length(),
      1 => 0,
      _ => 148 + 3 * algorithm.hash_length(),
    };
    let mutations = [Ok(SemanticCatalogConfigurationMutationV1::Remove("/".into()))];
    let (result, allocations) = measure(size, || {
      update_semantic_catalog_v1(
        SemanticCatalogUpdateRequestV1 {
          compilation: SemanticCatalogCompilationRequestV1 { expected_configuration_count: 0, ..request },
          expected_mutation_count: 1,
        },
        &previous,
        &registry,
        mutations,
        &mut store,
        &memory,
        &|| false,
      )
    });
    assert!(allocations.injected_failure, "mode {mode} never exercised the requested allocation");
    let error = match result {
      Err(error) => error,
      Ok(_) => panic!("refused allocation produced a candidate"),
    };
    match error {
      SemanticCatalogCompilationErrorV1::Catalog(source) => {
        assert_eq!(source.class(), SemanticCatalogReadErrorClassV1::ResourceLimit);
        assert_eq!(source.code(), if mode == 2 { "semantic_state_allocation" } else { "semantic_catalog_allocation" });
      }
      other => panic!("wrong allocation error at mode {mode}: {other}"),
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    if mode == 0 {
      assert_eq!((store.reads.get(), store.writes), before);
    }
    if mode == 1 {
      assert_eq!(store.writes, before.1);
    }
    let retry = update_semantic_catalog_v1(
      SemanticCatalogUpdateRequestV1 {
        compilation: SemanticCatalogCompilationRequestV1 { expected_configuration_count: 0, ..request },
        expected_mutation_count: 1,
      },
      &previous,
      &registry,
      [Ok(SemanticCatalogConfigurationMutationV1::Remove("/".into()))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(retry.configuration_count(), 0);
    drop(retry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
}

#[test]
fn semantic_state_buffers_refuse_host_allocation_without_abort_for_every_hash() {
  use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let width = algorithm.hash_length();
    let complete = SemanticAvailabilityV1::Complete {
      compiler_fingerprint: vec![1; width],
      semantic_registry_fingerprint: vec![2; width],
      catalog_root: vec![3; width],
      catalog_record_count: 1,
      catalog_node_count: 1,
      definition_count: 1,
      dependency_count: 0,
    };
    for availability in
      [complete, SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured }]
    {
      let request = SemanticStateWriteV1 { required_capabilities: [0; 32], availability };
      for bytes in [112 + 3 * width, 148 + 3 * width] {
        let (result, allocations) = measure(bytes, || encode_semantic_state_object(&request, algorithm));
        assert!(allocations.injected_failure, "state buffer {bytes} was not exercised");
        let error = result.unwrap_err();
        assert!(error.is_allocation_failure(), "{error}");
        assert_eq!(error.code(), "semantic_state_allocation");
        assert!(encode_semantic_state_object(&request, algorithm).is_ok(), "one refusal cannot poison retry");
      }
    }
  }
}
