//! Incremental composition over an opaque compiler-produced base.
use super::*;
use aeordb::engine::v4::semantic_catalog_compiler::{
  SemanticCatalogConfigurationMutationV1, SemanticCatalogUpdateRequestV1, update_semantic_catalog_v1,
};

const NATIVE: &[u8] = br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#;
const MAPPER: &[u8] = br#"{"$v":1,"parser":"p","indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m"}}]}"#;

#[test]
fn a_fieldless_addition_does_not_walk_unrelated_definitions_as_the_base_grows() {
  let algorithm = ALGORITHMS[0];
  let mut measured = Vec::new();
  for count in [32, 128] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let configurations = (0..count).map(|index| configuration(algorithm, &registry, &memory, &format!("/existing/{index}")));
    let previous =
      compile_semantic_catalog_v1(request(algorithm, count), &registry, configurations, &mut store, &memory, &|| false).unwrap();
    let mutation = configuration(algorithm, &registry, &memory, "/added").map(SemanticCatalogConfigurationMutationV1::Upsert);
    let before = store.reads.get();
    let result =
      update_semantic_catalog_v1(update_request(algorithm, count + 1, 1), &previous, &registry, [mutation], &mut store, &memory, &|| false)
        .unwrap();
    measured.push(store.reads.get() - before);
    assert_eq!(result.configuration_count(), count + 1);
  }
  assert!(measured[1] <= measured[0] * 2 + 32, "fieldless update rescans the growing catalog: {measured:?}");
}

#[test]
fn deleting_all_configurations_keeps_registry_only_parser_but_prunes_same_artifact_mapper() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: Some(br#"{"$v":1,"parsers":{"text/plain":"p"}}"#),
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &AvailableSnapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    let mut store = Store::new(algorithm);
    let configurations = ["/", "/nested"].into_iter().map(|owner| compiled(algorithm, &registry, &memory, owner, MAPPER));
    let previous = compile_semantic_catalog_v1(request(algorithm, 2), &registry, configurations, &mut store, &memory, &|| false).unwrap();
    let mutations = ["/", "/nested"].into_iter().map(|owner| Ok(SemanticCatalogConfigurationMutationV1::Remove(owner.into())));
    let result =
      update_semantic_catalog_v1(update_request(algorithm, 0, 2), &previous, &registry, mutations, &mut store, &memory, &|| false).unwrap();
    assert_fresh(algorithm, &registry, &memory, &result, &[], &store);
    let state = decode_semantic_object(&result.semantic_state().value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete { dependency_count, catalog_record_count, .. } = state.availability else {
      panic!("complete");
    };
    assert_eq!((dependency_count, catalog_record_count), (1, 2));
  }
}

#[test]
fn incremental_stream_releases_each_compiled_change_before_requesting_the_next() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
  let observed = Cell::new(None);
  let mutations = (0..64).map(|index| {
    let retained = memory.snapshot().unwrap().reserved_bytes;
    match observed.get() {
      None => observed.set(Some(retained)),
      Some(expected) => assert_eq!(retained, expected, "earlier compiled change or COW plan stayed resident"),
    }
    compiled(algorithm, &registry, &memory, &format!("/scope/{index}"), EMPTY).map(SemanticCatalogConfigurationMutationV1::Upsert)
  });
  let result =
    update_semantic_catalog_v1(update_request(algorithm, 64, 64), &previous, &registry, mutations, &mut store, &memory, &|| false).unwrap();
  assert_eq!(result.configuration_count(), 64);
  drop(result);
  drop(previous);
  drop(registry);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

fn compiled(
  algorithm: HashAlgorithm,
  registry: &CompiledParserRegistryV1,
  memory: &MemoryCoordinator,
  owner_path: &str,
  source: &[u8],
) -> Result<CompiledIndexConfigurationV1, SemanticCompilationErrorV1> {
  compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source,
      owner_path,
      registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &AvailableSnapshot,
    memory,
    &|| false,
  )
}

fn update_request(algorithm: HashAlgorithm, final_count: u64, mutation_count: u64) -> SemanticCatalogUpdateRequestV1 {
  SemanticCatalogUpdateRequestV1 { compilation: request(algorithm, final_count), expected_mutation_count: mutation_count }
}

fn assert_fresh(
  algorithm: HashAlgorithm,
  registry: &CompiledParserRegistryV1,
  memory: &MemoryCoordinator,
  result: &CompiledSemanticCatalogV1,
  sources: &[(&str, &[u8])],
  store: &Store,
) {
  let mut fresh_store = Store::new(algorithm);
  let configurations = sources.iter().map(|(owner, source)| compiled(algorithm, registry, memory, owner, source));
  let fresh =
    compile_semantic_catalog_v1(request(algorithm, sources.len() as u64), registry, configurations, &mut fresh_store, memory, &|| false)
      .unwrap();
  assert_eq!(result.semantic_state(), fresh.semantic_state(), "incremental and complete composition disagree");
  assert_eq!(result.configuration_count(), sources.len() as u64);
  let state = decode_semantic_object(&result.semantic_state().value, algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, .. } = state.availability else {
    panic!("complete candidate");
  };
  let reader = SemanticCatalogReaderV1::new(algorithm, store);
  let mut bindings = Vec::new();
  reader
    .walk_catalog(
      &catalog_root,
      SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count).unwrap(),
      &|| false,
      |record| {
        reader.with_definition(record, &|| false, |_| Ok(()))?;
        bindings.push(catalog_oracle::Binding {
          kind: record.record_kind,
          owner: record.owner_key.to_vec(),
          semantic: record.semantic_id.to_vec(),
          definition: record.definition_object_id.to_vec(),
        });
        Ok(())
      },
    )
    .unwrap();
  let (independent, nodes) = catalog_oracle::oracle(algorithm, &bindings, 0);
  assert_eq!(independent.object_id, catalog_root);
  assert_eq!(nodes, catalog_node_count);
}

#[test]
fn incremental_add_replace_remove_prunes_only_the_last_shared_dependency_reference() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let inputs = ["/", "/nested"].into_iter().map(|owner| compiled(algorithm, &registry, &memory, owner, NATIVE));
    let mut previous = compile_semantic_catalog_v1(request(algorithm, 2), &registry, inputs, &mut store, &memory, &|| false).unwrap();
    for (owner, replacement, expected) in [
      ("/", None, vec![("/nested", NATIVE)]),
      ("/nested", Some(EMPTY), vec![("/nested", EMPTY)]),
      ("/", Some(MAPPER), vec![("/", MAPPER), ("/nested", EMPTY)]),
      ("/second", Some(MAPPER), vec![("/", MAPPER), ("/nested", EMPTY), ("/second", MAPPER)]),
      ("/", None, vec![("/nested", EMPTY), ("/second", MAPPER)]),
      ("/second", None, vec![("/nested", EMPTY)]),
      ("/nested", None, vec![]),
    ] {
      let mutation = match replacement {
        Some(source) => SemanticCatalogConfigurationMutationV1::Upsert(compiled(algorithm, &registry, &memory, owner, source).unwrap()),
        None => SemanticCatalogConfigurationMutationV1::Remove(owner.to_string()),
      };
      let result = update_semantic_catalog_v1(
        update_request(algorithm, expected.len() as u64, 1),
        &previous,
        &registry,
        [Ok(mutation)],
        &mut store,
        &memory,
        &|| false,
      )
      .unwrap();
      assert_fresh(algorithm, &registry, &memory, &result, &expected, &store);
      previous = result;
    }
    drop(previous);
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn ordered_changes_are_atomic_candidates_and_repeated_owners_apply_in_stream_order() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let previous =
      compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
    let steps = [Some(NATIVE), Some(EMPTY), None, Some(MAPPER)];
    let mutations = steps.into_iter().map(|source| match source {
      Some(source) => compiled(algorithm, &registry, &memory, "/", source).map(SemanticCatalogConfigurationMutationV1::Upsert),
      None => Ok(SemanticCatalogConfigurationMutationV1::Remove("/".into())),
    });
    let result =
      update_semantic_catalog_v1(update_request(algorithm, 1, 4), &previous, &registry, mutations, &mut store, &memory, &|| false).unwrap();
    assert_fresh(algorithm, &registry, &memory, &result, &[("/", MAPPER)], &store);
    assert_fresh(algorithm, &registry, &memory, &previous, &[], &store);
  }
}

#[test]
fn absent_deletes_and_empty_streams_preserve_the_exact_semantic_state() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let previous =
      compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
    let result =
      update_semantic_catalog_v1(update_request(algorithm, 0, 0), &previous, &registry, std::iter::empty(), &mut store, &memory, &|| false)
        .unwrap();
    assert_eq!(result.semantic_state(), previous.semantic_state());
    let result = update_semantic_catalog_v1(
      update_request(algorithm, 0, 1),
      &previous,
      &registry,
      [Ok(SemanticCatalogConfigurationMutationV1::Remove("/absent".into()))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(result.semantic_state(), previous.semantic_state());
  }
}

#[test]
fn incremental_count_errors_and_source_failures_never_return_a_complete_candidate() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  for (expected_mutations, actual_mutations, final_count) in [(0, 1, 0), (2, 1, 0), (1, 1, 1)] {
    let mutations = (0..actual_mutations).map(|_| Ok(SemanticCatalogConfigurationMutationV1::Remove("/absent".into())));
    let error = failure(update_semantic_catalog_v1(
      update_request(algorithm, final_count, expected_mutations),
      &previous,
      &registry,
      mutations,
      &mut store,
      &memory,
      &|| false,
    ));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { .. }));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
  let mutations = [
    compiled(algorithm, &registry, &memory, "/", NATIVE).map(SemanticCatalogConfigurationMutationV1::Upsert),
    Err(SemanticCompilationErrorV1::Operational { path: "test-update-source", message: "source failed".into() }),
  ];
  let error =
    failure(update_semantic_catalog_v1(update_request(algorithm, 1, 2), &previous, &registry, mutations, &mut store, &memory, &|| false));
  assert!(matches!(
    error,
    SemanticCatalogCompilationErrorV1::Input(SemanticCompilationErrorV1::Operational { path: "test-update-source", .. })
  ));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  assert_fresh(algorithm, &registry, &memory, &previous, &[], &store);
}

#[test]
fn incremental_cancellation_and_memory_admission_precede_source_and_storage_access() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  for cancelled in [true, false] {
    let before = (store.reads.get(), store.writes);
    let mutations = std::iter::from_fn(|| -> Option<Result<SemanticCatalogConfigurationMutationV1, SemanticCompilationErrorV1>> {
      panic!("mutation source accessed before admission");
    });
    let mut request = update_request(algorithm, 0, 0);
    if !cancelled {
      request.compilation.maximum_workspace_bytes = 1;
    }
    let error = failure(update_semantic_catalog_v1(request, &previous, &registry, mutations, &mut store, &memory, &|| cancelled));
    let expected = if cancelled { SemanticCatalogReadErrorClassV1::Cancelled } else { SemanticCatalogReadErrorClassV1::ResourceLimit };
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == expected));
    assert_eq!((store.reads.get(), store.writes), before);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
  let cancelled = Cell::new(false);
  let mutations = std::iter::once_with(|| {
    cancelled.set(true);
    Ok(SemanticCatalogConfigurationMutationV1::Remove("/absent".into()))
  });
  let writes = store.writes;
  let error =
    failure(update_semantic_catalog_v1(update_request(algorithm, 0, 1), &previous, &registry, mutations, &mut store, &memory, &|| {
      cancelled.get()
    }));
  assert!(
    matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == SemanticCatalogReadErrorClassV1::Cancelled)
  );
  assert_eq!(store.writes, writes);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
}

#[test]
fn incremental_publication_failure_retries_from_the_unchanged_base() {
  let algorithm = ALGORITHMS[0];
  for after_write in [false, true] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let previous =
      compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    store.write_fault = Some((store.writes + 2, after_write));
    let mutation = compiled(algorithm, &registry, &memory, "/", NATIVE).map(SemanticCatalogConfigurationMutationV1::Upsert);
    let error =
      failure(update_semantic_catalog_v1(update_request(algorithm, 1, 1), &previous, &registry, [mutation], &mut store, &memory, &|| {
        false
      }));
    assert!(
      matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == SemanticCatalogReadErrorClassV1::Unavailable)
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    store.write_fault = None;
    assert_fresh(algorithm, &registry, &memory, &previous, &[], &store);
    let mutation = compiled(algorithm, &registry, &memory, "/", NATIVE).map(SemanticCatalogConfigurationMutationV1::Upsert);
    let result =
      update_semantic_catalog_v1(update_request(algorithm, 1, 1), &previous, &registry, [mutation], &mut store, &memory, &|| false)
        .unwrap();
    assert_fresh(algorithm, &registry, &memory, &result, &[("/", NATIVE)], &store);
  }
}

#[test]
fn changed_registry_cannot_reuse_previous_automatic_parser_contexts() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [compiled(algorithm, &registry, &memory, "/", NATIVE)],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap();
  let changed = compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: Some(br#"{"$v":1,"parsers":{"text/plain":"p"}}"#),
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    },
    &AvailableSnapshot,
    &memory,
    &|| false,
  )
  .unwrap();
  let writes = store.writes;
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let error = failure(update_semantic_catalog_v1(
    update_request(algorithm, 1, 0),
    &previous,
    &changed,
    std::iter::empty(),
    &mut store,
    &memory,
    &|| false,
  ));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_registry_changed", .. }));
  assert_eq!(store.writes, writes);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
}

#[test]
fn affected_base_read_failures_preserve_their_classes_and_never_publish() {
  let algorithm = ALGORITHMS[0];
  for (fault, expected) in [
    (ReadFault::Unavailable, SemanticCatalogReadErrorClassV1::Unavailable),
    (ReadFault::Resource, SemanticCatalogReadErrorClassV1::ResourceLimit),
    (ReadFault::Missing, SemanticCatalogReadErrorClassV1::Corrupt),
    (ReadFault::Corrupt, SemanticCatalogReadErrorClassV1::Corrupt),
  ] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let previous =
      compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let writes = store.writes;
    store.read_fault = Some((store.present_reads.get() + 1, fault));
    let error = failure(update_semantic_catalog_v1(
      update_request(algorithm, 0, 0),
      &previous,
      &registry,
      std::iter::empty(),
      &mut store,
      &memory,
      &|| false,
    ));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == expected));
    assert_eq!(store.writes, writes);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    store.read_fault = None;
    let retry =
      update_semantic_catalog_v1(update_request(algorithm, 0, 0), &previous, &registry, std::iter::empty(), &mut store, &memory, &|| false)
        .unwrap();
    assert_eq!(retry.semantic_state(), previous.semantic_state());
  }
}

#[test]
fn invalid_removal_paths_cannot_be_normalized_into_another_owner() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [configuration(algorithm, &registry, &memory, "/")],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap();
  for owner in ["", "relative", "/double//segment", "/dot/../segment", "/nul\0x", "/trailing/"] {
    let reserved = memory.snapshot().unwrap().reserved_bytes;
    let writes = store.writes;
    let error = failure(update_semantic_catalog_v1(
      update_request(algorithm, 1, 1),
      &previous,
      &registry,
      [Ok(SemanticCatalogConfigurationMutationV1::Remove(owner.into()))],
      &mut store,
      &memory,
      &|| false,
    ));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { .. }), "{owner:?}: {error}");
    assert_eq!(store.writes, writes);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
  assert_fresh(algorithm, &registry, &memory, &previous, &[("/", EMPTY)], &store);
}

#[test]
fn changed_glob_and_owner_remove_old_scope_value_and_index_bindings() {
  let changed_glob: &[u8] = br#"{"$v":1,"glob":"**/*.json","indexes":[{"name":"value","type":"typed_exact_blake3_v1"}]}"#;
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let previous = compile_semantic_catalog_v1(
      request(algorithm, 1),
      &registry,
      [compiled(algorithm, &registry, &memory, "/before", NATIVE)],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    let mutations = [
      compiled(algorithm, &registry, &memory, "/before", changed_glob).map(SemanticCatalogConfigurationMutationV1::Upsert),
      Ok(SemanticCatalogConfigurationMutationV1::Remove("/before".into())),
      compiled(algorithm, &registry, &memory, "/after", changed_glob).map(SemanticCatalogConfigurationMutationV1::Upsert),
    ];
    let result =
      update_semantic_catalog_v1(update_request(algorithm, 1, 3), &previous, &registry, mutations, &mut store, &memory, &|| false).unwrap();
    assert_fresh(algorithm, &registry, &memory, &result, &[("/after", changed_glob)], &store);
    assert_fresh(algorithm, &registry, &memory, &previous, &[("/before", NATIVE)], &store);
  }
}

#[test]
fn a_compiler_produced_base_cannot_be_reinterpreted_with_another_hash_algorithm() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false).unwrap();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  let before = (store.reads.get(), store.writes);
  for other in &ALGORITHMS[1..] {
    let error = failure(update_semantic_catalog_v1(
      update_request(*other, 0, 0),
      &previous,
      &registry,
      std::iter::empty(),
      &mut store,
      &memory,
      &|| false,
    ));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_base_algorithm", .. }));
    assert_eq!((store.reads.get(), store.writes), before);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
  }
}

#[test]
fn pruning_many_distinct_dependencies_scans_remaining_references_once() {
  struct Distinct;
  impl ParserAliasSnapshotV1 for Distinct {
    fn resolve_parser_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      Ok(Some(dependency(1)))
    }
  }
  impl IndexConfigurationAliasSnapshotV1 for Distinct {
    fn resolve_mapper_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      let mut record = dependency(2);
      record.fingerprint = [alias.parse::<u8>().unwrap() + 1; 32];
      Ok(Some(record))
    }
  }
  let algorithm = ALGORITHMS[0];
  let mut measured = Vec::new();
  for count in [8, 32] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let compile = |index| {
      let owner = format!("/scope/{index}");
      let source =
        format!(r#"{{"$v":1,"parser":"p","indexes":[{{"name":"x","type":"typed_exact_blake3_v1","source":{{"plugin":"{index}"}}}}]}}"#);
      compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source: source.as_bytes(),
          owner_path: &owner,
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 128 << 20,
        },
        &Distinct,
        &memory,
        &|| false,
      )
    };
    let mut store = Store::new(algorithm);
    let previous =
      compile_semantic_catalog_v1(request(algorithm, count * 2), &registry, (0..count * 2).map(compile), &mut store, &memory, &|| false)
        .unwrap();
    let before = store.reads.get();
    let result = update_semantic_catalog_v1(
      update_request(algorithm, count, count),
      &previous,
      &registry,
      (0..count).map(|index| Ok(SemanticCatalogConfigurationMutationV1::Remove(format!("/scope/{index}")))),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    measured.push(store.reads.get() - before);
    let mut fresh_store = Store::new(algorithm);
    let fresh = compile_semantic_catalog_v1(
      request(algorithm, count),
      &registry,
      (count..count * 2).map(compile),
      &mut fresh_store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(result.semantic_state(), fresh.semantic_state());
    let state = decode_semantic_object(&result.semantic_state().value, algorithm).unwrap().semantic_state.unwrap();
    let SemanticAvailabilityV1::Complete { dependency_count, .. } = state.availability else {
      panic!("complete");
    };
    assert_eq!(dependency_count, count + 1, "only the remaining unique mappers and shared parser survive");
  }
  assert!(measured[1] <= measured[0] * 6, "fourfold dependency growth caused superlinear rescans: {measured:?}");
}

#[test]
fn every_incremental_publication_boundary_can_fail_or_lose_admission_and_retry() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut baseline = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(
    request(algorithm, 2),
    &registry,
    ["/", "/nested"].into_iter().map(|owner| compiled(algorithm, &registry, &memory, owner, NATIVE)),
    &mut baseline,
    &memory,
    &|| false,
  )
  .unwrap();
  let run = |store: &mut Store| {
    update_semantic_catalog_v1(
      update_request(algorithm, 1, 2),
      &previous,
      &registry,
      [
        Ok(SemanticCatalogConfigurationMutationV1::Remove("/".into())),
        compiled(algorithm, &registry, &memory, "/nested", EMPTY).map(SemanticCatalogConfigurationMutationV1::Upsert),
      ],
      store,
      &memory,
      &|| false,
    )
  };
  let mut successful = Store::new(algorithm);
  successful.objects = baseline.objects.clone();
  let expected = run(&mut successful).unwrap();
  let writes = successful.writes;
  assert!(writes > 10, "exercise removal, candidate collection/pruning and final state");
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  for at in 1..=writes {
    for mode in 0..3 {
      let mut store = Store::new(algorithm);
      store.objects = baseline.objects.clone();
      if mode < 2 {
        store.write_fault = Some((at, mode == 1));
      } else {
        store.pressure_after_write = Some((at, memory.clone()));
      }
      let error = failure(run(&mut store));
      let class = if mode < 2 { SemanticCatalogReadErrorClassV1::Unavailable } else { SemanticCatalogReadErrorClassV1::ResourceLimit };
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == class), "boundary {at}, mode {mode}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      store.write_fault = None;
      store.pressure_after_write = None;
      let retry = run(&mut store).unwrap();
      assert_eq!(retry.semantic_state(), expected.semantic_state());
      assert_fresh(algorithm, &registry, &memory, &previous, &[("/", NATIVE), ("/nested", NATIVE)], &store);
    }
  }
}

#[test]
fn every_present_read_in_removal_and_pruning_preserves_operational_or_corrupt_failure() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut baseline = Store::new(algorithm);
  let previous = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [compiled(algorithm, &registry, &memory, "/", NATIVE)],
    &mut baseline,
    &memory,
    &|| false,
  )
  .unwrap();
  let run = |store: &mut Store| {
    update_semantic_catalog_v1(
      update_request(algorithm, 0, 1),
      &previous,
      &registry,
      [Ok(SemanticCatalogConfigurationMutationV1::Remove("/".into()))],
      store,
      &memory,
      &|| false,
    )
  };
  let mut successful = Store::new(algorithm);
  successful.objects = baseline.objects.clone();
  let expected = run(&mut successful).unwrap();
  let reads = successful.present_reads.get();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  assert!(reads > 20);
  for at in 1..=reads {
    for (fault, class) in [
      (ReadFault::Unavailable, SemanticCatalogReadErrorClassV1::Unavailable),
      (ReadFault::Resource, SemanticCatalogReadErrorClassV1::ResourceLimit),
      (ReadFault::Missing, SemanticCatalogReadErrorClassV1::Corrupt),
      (ReadFault::Corrupt, SemanticCatalogReadErrorClassV1::Corrupt),
    ] {
      let mut store = Store::new(algorithm);
      store.objects = baseline.objects.clone();
      store.read_fault = Some((at, fault));
      let error = failure(run(&mut store));
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(source) if source.class() == class), "read {at}, fault {fault:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      store.read_fault = None;
      let result = run(&mut store).unwrap();
      assert_eq!(result.semantic_state(), expected.semantic_state());
      assert_fresh(algorithm, &registry, &memory, &previous, &[("/", NATIVE)], &store);
    }
  }
}

#[test]
fn replacement_with_unchanged_dependencies_does_not_scan_unrelated_definitions() {
  let algorithm = ALGORITHMS[0];
  let mut measured = Vec::new();
  for count in [32, 128] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let previous = compile_semantic_catalog_v1(
      request(algorithm, count),
      &registry,
      (0..count).map(|index| compiled(algorithm, &registry, &memory, &format!("/existing/{index}"), NATIVE)),
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    let changed =
      compiled(algorithm, &registry, &memory, "/existing/0", br#"{"$v":1,"indexes":[{"name":"changed","type":"typed_exact_blake3_v1"}]}"#)
        .unwrap();
    let before = store.reads.get();
    let result = update_semantic_catalog_v1(
      update_request(algorithm, count, 1),
      &previous,
      &registry,
      [Ok(SemanticCatalogConfigurationMutationV1::Upsert(changed))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    measured.push(store.reads.get() - before);
    assert_eq!(result.configuration_count(), count);
    assert_ne!(result.semantic_state().object_id, previous.semantic_state().object_id);
  }
  assert!(measured[1] <= measured[0] * 2 + 32, "unchanged dependency closure triggered a whole-catalog scan: {measured:?}");
}

#[test]
fn a_later_removal_prunes_dependencies_reintroduced_by_an_earlier_replacement() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let previous = compile_semantic_catalog_v1(
      request(algorithm, 1),
      &registry,
      [compiled(algorithm, &registry, &memory, "/", NATIVE)],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    let result = update_semantic_catalog_v1(
      update_request(algorithm, 0, 2),
      &previous,
      &registry,
      [
        compiled(algorithm, &registry, &memory, "/", NATIVE).map(SemanticCatalogConfigurationMutationV1::Upsert),
        Ok(SemanticCatalogConfigurationMutationV1::Remove("/".into())),
      ],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_fresh(algorithm, &registry, &memory, &result, &[], &store);
    assert_fresh(algorithm, &registry, &memory, &previous, &[("/", NATIVE)], &store);
  }
}
