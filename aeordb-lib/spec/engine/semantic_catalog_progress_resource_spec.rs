//! Real allocator failures at the partial-admission boundary, not source mocks.
use super::*;
use aeordb::engine::v4::namespace::SemanticAvailabilityV1;
use aeordb::engine::v4::semantic_catalog_mutation::SemanticCatalogSnapshotV1;
use aeordb::engine::v4::semantic_catalog_compiler::admit_semantic_catalog_progress_v1;

#[test]
fn progress_candidate_root_copy_refuses_after_graph_validation_and_releases_every_charge() {
  use aeordb::engine::v4::semantic_catalog::{SemanticCatalogReaderV1, SemanticCatalogTraversalBoundsV1};
  use aeordb::engine::v4::namespace::SemanticCatalogRecordV1;
  use aeordb::engine::v4::semantic_catalog_mutation::{
    SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, plan_semantic_catalog_mutation_v1,
  };
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
  let original =
    compile_semantic_catalog_v1(request, &registry, [Ok(configuration)], &mut store, &memory, &|| false).unwrap().semantic_state().clone();
  let state = decode_semantic_object(&original.value, algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, dependency_count, .. } =
    state.availability
  else {
    panic!("complete");
  };
  let mut bindings = Vec::new();
  SemanticCatalogReaderV1::new(algorithm, &store)
    .walk_catalog(
      &catalog_root,
      SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count).unwrap(),
      &|| false,
      |record| {
        bindings.push((record.record_kind, record.owner_key.to_vec(), record.semantic_id.to_vec(), record.definition_object_id.to_vec()));
        Ok(())
      },
    )
    .unwrap();
  let mut catalogs = [(Some(catalog_root), catalog_record_count, catalog_node_count), (None, 0, 0)];
  for (kind, owner, semantic, definition) in &bindings {
    if *kind == 2 {
      continue;
    }
    let candidate = usize::from(matches!(kind, 6 | 7));
    let (root, records, nodes) = &mut catalogs[candidate];
    let mutation = if candidate == 1 {
      SemanticCatalogMutationV1::Upsert(SemanticCatalogRecordV1 {
        record_kind: *kind,
        owner_key: owner,
        semantic_id: semantic,
        definition_object_id: definition,
      })
    } else {
      SemanticCatalogMutationV1::Remove { record_kind: *kind, owner_key: owner }
    };
    let plan = plan_semantic_catalog_mutation_v1(
      SemanticCatalogMutationRequestV1 {
        hash_algorithm: algorithm,
        snapshot: SemanticCatalogSnapshotV1 { root_object_id: root.as_deref(), record_count: *records, node_count: *nodes },
        mutation,
        maximum_workspace_bytes: 32 << 20,
      },
      &store,
      &memory,
      &|| false,
    )
    .unwrap();
    store.publish_semantic_objects(plan.objects()).unwrap();
    *root = plan.root_object_id().map(<[u8]>::to_vec);
    *records = plan.record_count();
    *nodes = plan.node_count();
  }
  let snapshot = |index: usize| {
    let (root, records, nodes) = &catalogs[index];
    SemanticCatalogSnapshotV1 { root_object_id: root.as_deref(), record_count: *records, node_count: *nodes }
  };
  assert_eq!(catalogs[1].1, dependency_count);
  assert!(dependency_count > 0);
  let bytes = input::checkpoint(algorithm, &original, snapshot(0), snapshot(1), dependency_count);
  let request = SemanticCatalogCompilationRequestV1 { expected_configuration_count: 0, ..request };
  let before = memory.snapshot().unwrap().reserved_bytes;
  let checks = Cell::new(0);
  drop(
    admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &|| {
      checks.set(checks.get() + 1);
      false
    })
    .unwrap(),
  );
  let before_copy = checks.get() - 1;
  assert!(before_copy > 10);
  let writes = store.writes;
  checks.set(0);
  let (result, allocation) = measure(0, || {
    admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &|| {
      checks.set(checks.get() + 1);
      // Last shared-closure check: bitmap already released, candidate root is
      // the next allocation. Final result's check must not be reached on refusal.
      if checks.get() == before_copy {
        FAIL_SIZE.with(|target| target.set(32));
        FAIL_OCCURRENCE.with(|remaining| remaining.set(1));
      }
      false
    })
  });
  assert!(allocation.injected_failure);
  assert_eq!(checks.get(), before_copy);
  let error = match result {
    Ok(_) => panic!("refused candidate copy returned progress"),
    Err(error) => error,
  };
  assert!(
    matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit && error.code() == "semantic_catalog_allocation")
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
  let retry = admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &|| false).unwrap();
  assert_eq!(retry.pruning_candidates().root_object_id, catalogs[1].0.as_deref());
  drop(retry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
  assert_eq!(store.writes, writes);
}

#[test]
fn progress_checkpoint_identity_root_and_bitmap_allocations_refuse_release_and_retry() {
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
  let original =
    compile_semantic_catalog_v1(request, &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap().semantic_state().clone();
  let state = decode_semantic_object(&original.value, algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, .. } = state.availability else {
    panic!("complete");
  };
  let bytes = input::checkpoint(
    algorithm,
    &original,
    SemanticCatalogSnapshotV1 { root_object_id: Some(&catalog_root), record_count: catalog_record_count, node_count: catalog_node_count },
    SemanticCatalogSnapshotV1 { root_object_id: None, record_count: 0, node_count: 0 },
    0,
  );
  let before = memory.snapshot().unwrap().reserved_bytes;
  let writes = store.writes;
  for (size, code) in
    [(24, "semantic_task_identity_allocation"), (32, "semantic_catalog_allocation"), (1, "semantic_catalog_reachability_memory")]
  {
    store.reads.set(0);
    let checks = Cell::new(0);
    let cancel = || {
      checks.set(checks.get() + 1);
      // The fifth check is validate_definition's final check, immediately
      // before the new retained root copy. Do not inject into inherited
      // infallible digest allocations during the preceding definition proof.
      if size == 32 && checks.get() == 5 {
        FAIL_SIZE.with(|target| target.set(32));
        FAIL_OCCURRENCE.with(|remaining| remaining.set(1));
      }
      false
    };
    let (result, allocation) = measure(if size == 32 { 0 } else { size }, || {
      admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &cancel)
    });
    assert!(allocation.injected_failure, "size {size} was not exercised");
    let error = match result {
      Ok(_) => panic!("refused allocation admitted progress"),
      Err(error) => error,
    };
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit && error.code() == code),
      "size {size}"
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
    if size != 1 {
      assert_eq!(store.reads.get(), 0);
    }
    if size == 32 {
      assert_eq!(checks.get(), 5);
    }
    let retry = admit_semantic_catalog_progress_v1(request, &bytes, &registry, &store, &memory, &|| false).unwrap();
    assert_eq!(retry.catalog().root_object_id, Some(catalog_root.as_slice()));
    drop(retry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
    assert_eq!(store.writes, writes);
  }
}
