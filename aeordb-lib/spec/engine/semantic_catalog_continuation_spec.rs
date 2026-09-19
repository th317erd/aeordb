//! Whole-step catalog snapshots do not select tasks or namespace authority.
use super::*;
use aeordb::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogConfigurationMutationV1, SemanticCatalogContinuationV1, admit_semantic_catalog_v1,
};
use aeordb::engine::v4::semantic_mutation_control::SemanticMutationPhaseV1;

#[path = "semantic_catalog_continuation_boundary_spec.rs"]
mod boundary_spec;
#[path = "semantic_catalog_continuation_bounded_spec.rs"]
mod bounded_spec;
#[path = "semantic_catalog_continuation_fault_spec.rs"]
mod fault_spec;
#[path = "semantic_catalog_continuation_restart_spec.rs"]
mod restart_spec;

#[test]
fn catalog_continuation_stages_whole_configurations_before_a_complete_result() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut work = SemanticCatalogContinuationV1::start(request(algorithm, 2), &registry, &mut store, &memory, &|| false)
      .expect("catalog continuation must expose whole-step progress before final state");
    assert_eq!(work.phase(), SemanticMutationPhaseV1::Compiling);
    assert_eq!(work.configuration_count(), 0);
    assert_eq!(work.catalog().record_count, 1);
    assert_eq!(work.pruning_candidates().record_count, 0);
    // Expected bindings come from declared inputs, never by walking the
    // resulting catalog. Patricia bytes/counts use the independent oracle.
    let mut expected = vec![catalog_oracle::Binding {
      kind: 2,
      owner: b"\x02\x00/.aeordb-config/parsers.json".to_vec(),
      semantic: registry.projection().semantic_id.clone(),
      definition: registry.projection().object.object_id.clone(),
    }];
    for (index, owner) in ["/", "/nested"].into_iter().enumerate() {
      let configuration = configuration(algorithm, &registry, &memory, owner).unwrap();
      let scope = aeordb::engine::v4::namespace::encode_semantic_definition_object(3, &configuration.scope().value, algorithm).unwrap();
      let mut key = vec![1, 0];
      key.extend_from_slice(owner.trim_end_matches('/').as_bytes());
      key.extend_from_slice(b"/.aeordb-config/indexes.json");
      expected.push(catalog_oracle::Binding {
        kind: 1,
        owner: key,
        semantic: configuration.projection().semantic_id.clone(),
        definition: configuration.projection().object.object_id.clone(),
      });
      expected.push(catalog_oracle::Binding {
        kind: 3,
        owner: scope.semantic_id.clone(),
        semantic: scope.semantic_id,
        definition: scope.object.object_id,
      });
      work = work.apply(SemanticCatalogConfigurationMutationV1::Upsert(configuration), &mut store).unwrap();
      assert_eq!(work.configuration_count(), index as u64 + 1);
      assert_eq!(work.catalog().record_count, 1 + 2 * (index as u64 + 1));
      assert_eq!(work.dependency_count(), 0);
      let (oracle, nodes) = catalog_oracle::oracle(algorithm, &expected, 0);
      assert_eq!(work.catalog().root_object_id, Some(oracle.object_id.as_slice()));
      assert_eq!(work.catalog().node_count, nodes);
      assert_eq!(store.objects[&(3, oracle.object_id)], oracle.value);
      assert!(!store.objects.keys().any(|(kind, _)| *kind == 1), "unfinished steps must not synthesize Complete state");
    }
    let pruning = work.finish_configurations(&mut store).unwrap();
    assert_eq!(pruning.phase(), SemanticMutationPhaseV1::Pruning);
    assert!(pruning.pruning_candidates().root_object_id.is_none());
    let result = pruning.finish(&mut store).unwrap();
    let mut fresh_store = Store::new(algorithm);
    let fresh = compile_semantic_catalog_v1(
      request(algorithm, 2),
      &registry,
      ["/", "/nested"].into_iter().map(|owner| configuration(algorithm, &registry, &memory, owner)),
      &mut fresh_store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(result.semantic_state(), fresh.semantic_state());
    let admitted =
      admit_semantic_catalog_v1(request(algorithm, 2), &result.semantic_state().object_id, &registry, &store, &memory, &|| false).unwrap();
    assert_eq!(admitted.configuration_count(), 2);
    drop(admitted);
    drop(fresh);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
}

#[test]
fn catalog_continuation_prunes_one_remaining_dependency_binding_per_step() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let configuration = compile_index_configuration_v1(
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
    .unwrap();
    assert_eq!(configuration.dependencies().len(), 4);
    let base = compile_semantic_catalog_v1(request(algorithm, 1), &registry, [Ok(configuration)], &mut store, &memory, &|| false).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let work = SemanticCatalogContinuationV1::from_complete(request(algorithm, 0), &base, &registry, &store, &memory, &|| false)
      .expect("incremental continuation must retain the exact complete base");
    let work = work.apply(SemanticCatalogConfigurationMutationV1::Remove("/".into()), &mut store).unwrap();
    assert_eq!(work.configuration_count(), 0);
    assert_eq!(work.catalog().record_count, 5);
    assert_eq!(work.pruning_candidates().record_count, 4);
    let mut work = work.finish_configurations(&mut store).unwrap();
    assert_eq!(work.phase(), SemanticMutationPhaseV1::Pruning);
    for remaining in (0..4).rev() {
      work = work.prune_one(&mut store).unwrap();
      assert_eq!(work.dependency_count(), remaining);
      assert_eq!(work.pruning_candidates().record_count, remaining);
      assert_eq!(work.catalog().record_count, 1 + remaining);
    }
    let result = work.finish(&mut store).unwrap();
    let mut fresh_store = Store::new(algorithm);
    let fresh = compile_semantic_catalog_v1(request(algorithm, 0), &registry, [], &mut fresh_store, &memory, &|| false).unwrap();
    assert_eq!(result.semantic_state(), fresh.semantic_state());
    let admitted =
      admit_semantic_catalog_v1(request(algorithm, 0), &result.semantic_state().object_id, &registry, &store, &memory, &|| false).unwrap();
    assert_eq!(admitted.configuration_count(), 0);
    drop(admitted);
    drop(fresh);
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
}
