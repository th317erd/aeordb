use super::*;
use aeordb::engine::v4::semantic_catalog_compiler::{
  admit_semantic_catalog_v1, update_semantic_catalog_v1, SemanticCatalogConfigurationMutationV1, SemanticCatalogUpdateRequestV1,
};
use aeordb::engine::v4::namespace::{encode_semantic_state_object, SemanticStateWriteV1};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value, encode_canonical_value};
use aeordb::engine::v4::namespace::{decode_semantic_definition_record, encode_semantic_definition_object};

fn definition_bytes(store: &Store, binding: &catalog_oracle::Binding) -> Vec<u8> {
  let object = store.objects.get(&(4, binding.definition.clone())).unwrap();
  decode_semantic_definition_record(object, store.algorithm).unwrap().definition.to_vec()
}

fn replace_definition(store: &mut Store, binding: &mut catalog_oracle::Binding, bytes: &[u8]) {
  let definition = encode_semantic_definition_object(binding.kind, bytes, store.algorithm).unwrap();
  store.publish_semantic_objects(std::slice::from_ref(&definition.object)).unwrap();
  if binding.kind >= 3 {
    binding.owner = definition.semantic_id.clone();
  }
  binding.semantic = definition.semantic_id;
  binding.definition = definition.object.object_id;
}

fn substitute_identity(value: &mut CanonicalConfigValueV1, old: &[u8], new: &[u8]) {
  match value {
    CanonicalConfigValueV1::Bytes(bytes) if bytes == old => *bytes = new.to_vec(),
    CanonicalConfigValueV1::Array(values) => {
      for value in values {
        substitute_identity(value, old, new);
      }
    }
    CanonicalConfigValueV1::Map(values) => {
      for value in values.values_mut() {
        substitute_identity(value, old, new);
      }
    }
    _ => {}
  }
}

#[test]
fn structurally_valid_native_parser_swaps_are_not_current_compiler_outputs() {
  use aeordb::engine::v4::parser_plan::{ParserCandidateKind, encode_parser_resolution_plan};
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  for algorithm in [ALGORITHMS[0], ALGORITHMS[2]] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let original = compile_semantic_catalog_v1(
      request(algorithm, 1),
      &registry,
      [Ok(configured(algorithm, &memory, &registry, "/"))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap()
    .semantic_state()
    .clone();
    let mut changed = bindings(&store, &original);
    let value_binding = changed.iter_mut().find(|binding| binding.kind == 4).unwrap();
    let old_value = value_binding.semantic.clone();
    let mut bytes = definition_bytes(&store, value_binding);
    let mut plan = decode_value_store_definition(&bytes, algorithm).unwrap().parser_plan;
    let raw = plan.candidates.iter().position(|candidate| candidate.kind == ParserCandidateKind::RawJson).unwrap();
    let native = plan.candidates.iter().position(|candidate| candidate.kind == ParserCandidateKind::NativeSuite).unwrap();
    let raw_ordinal = plan.candidates[raw].dependency_ordinal;
    plan.candidates[raw].dependency_ordinal = plan.candidates[native].dependency_ordinal;
    plan.candidates[native].dependency_ordinal = raw_ordinal;
    let encoded_plan = encode_parser_resolution_plan(&plan).unwrap();
    let fixed = 32 + algorithm.hash_length();
    let field_length = u32::from_le_bytes(bytes[fixed..fixed + 4].try_into().unwrap()) as usize;
    let selector_length = u32::from_le_bytes(bytes[fixed + 4..fixed + 8].try_into().unwrap()) as usize;
    let start = fixed + 80 + field_length + selector_length;
    bytes[start..start + encoded_plan.len()].copy_from_slice(&encoded_plan);
    // Structural decoding and all hashes are still valid; only the compiler's
    // native candidate-to-component contract has been violated.
    decode_value_store_definition(&bytes, algorithm).unwrap();
    replace_definition(&mut store, value_binding, &bytes);
    let new_value = value_binding.semantic.clone();
    let index_binding = changed.iter_mut().find(|binding| binding.kind == 5).unwrap();
    let old_index = index_binding.semantic.clone();
    let mut index = definition_bytes(&store, index_binding);
    index[32..32 + algorithm.hash_length()].copy_from_slice(&new_value);
    replace_definition(&mut store, index_binding, &index);
    let new_index = index_binding.semantic.clone();
    let projection = changed.iter_mut().find(|binding| binding.kind == 1).unwrap();
    let mut value = decode_canonical_value(&definition_bytes(&store, projection), CanonicalValueBounds::CONFIG).unwrap();
    substitute_identity(&mut value, &old_value, &new_value);
    substitute_identity(&mut value, &old_index, &new_index);
    replace_definition(&mut store, projection, &encode_canonical_value(&value, CanonicalValueBounds::CONFIG).unwrap());
    let state = replace_bindings(&mut store, &original, &changed);
    let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &registry, &store, &memory, &|| false));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_native_candidate"));
  }
}

#[test]
fn impossible_reachability_budget_is_refused_before_catalog_reads() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let original = compile_semantic_catalog_v1(request(algorithm, 0), &registry, std::iter::empty(), &mut store, &memory, &|| false)
    .unwrap()
    .semantic_state()
    .clone();
  let mut state = decode_semantic_object(&original.value, algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete { catalog_record_count, definition_count, .. } = &mut state.availability else {
    unreachable!()
  };
  *catalog_record_count = 1 << 40;
  *definition_count = *catalog_record_count;
  let state = install_state(&mut store, &state);
  store.reads.set(0);
  let before = memory.snapshot().unwrap().reserved_bytes;
  let error = failure(admit_semantic_catalog_v1(request(algorithm, 0), &state.object_id, &registry, &store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_reachability_memory"));
  assert_eq!(store.reads.get(), 1, "budget refusal read the catalog");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before);
}

fn bindings(store: &Store, state: &EncodedSemanticObjectV1) -> Vec<catalog_oracle::Binding> {
  let state = decode_semantic_object(&state.value, store.algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete { catalog_root, catalog_record_count, catalog_node_count, .. } = state.availability else {
    unreachable!()
  };
  let mut bindings = Vec::new();
  SemanticCatalogReaderV1::new(store.algorithm, store)
    .walk_catalog(
      &catalog_root,
      SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count).unwrap(),
      &|| false,
      |record| {
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
  bindings
}

fn install_oracle(store: &mut Store, bindings: &[catalog_oracle::Binding], depth: usize) -> (Vec<u8>, u64) {
  let algorithm = store.algorithm;
  let (object, nodes) = catalog_oracle::oracle(algorithm, bindings, depth);
  store.publish_semantic_objects(std::slice::from_ref(&object)).unwrap();
  if bindings.len() > 1 {
    let first = catalog_oracle::lookup(algorithm, &bindings[0]);
    let branch = (depth..algorithm.hash_length())
      .find(|position| bindings.iter().any(|binding| catalog_oracle::lookup(algorithm, binding)[*position] != first[*position]))
      .unwrap();
    let mut children: BTreeMap<u8, Vec<catalog_oracle::Binding>> = BTreeMap::new();
    for binding in bindings {
      children.entry(catalog_oracle::lookup(algorithm, binding)[branch]).or_default().push(binding.clone());
    }
    for child in children.into_values() {
      install_oracle(store, &child, branch + 1);
    }
  }
  (object.object_id, nodes)
}

fn replace_bindings(
  store: &mut Store,
  original: &EncodedSemanticObjectV1,
  bindings: &[catalog_oracle::Binding],
) -> EncodedSemanticObjectV1 {
  let (root, nodes) = install_oracle(store, bindings, 0);
  let mut state = decode_semantic_object(&original.value, store.algorithm).unwrap().semantic_state.unwrap();
  let SemanticAvailabilityV1::Complete {
    catalog_root, catalog_record_count, catalog_node_count, definition_count, dependency_count, ..
  } = &mut state.availability
  else {
    unreachable!()
  };
  *catalog_root = root;
  *catalog_record_count = bindings.len() as u64;
  *catalog_node_count = nodes;
  *definition_count = bindings.len() as u64;
  *dependency_count = bindings.iter().filter(|binding| matches!(binding.kind, 6 | 7)).count() as u64;
  install_state(store, &state)
}

fn configured(
  algorithm: HashAlgorithm,
  memory: &MemoryCoordinator,
  registry: &CompiledParserRegistryV1,
  owner: &str,
) -> CompiledIndexConfigurationV1 {
  compile_index_configuration_v1(
    IndexConfigurationCompilationRequestV1 {
      source: br#"{"$v":1,"indexes":[{"name":"title","type":"unicode_trigram_v1"}]}"#,
      owner_path: owner,
      registry,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 128 << 20,
    },
    &AvailableSnapshot,
    memory,
    &|| false,
  )
  .unwrap()
}

fn install_state(store: &mut Store, state: &aeordb::engine::v4::namespace::SemanticStateV1) -> EncodedSemanticObjectV1 {
  let object = encode_semantic_state_object(
    &SemanticStateWriteV1 { required_capabilities: state.required_capabilities, availability: state.availability.clone() },
    store.algorithm,
  )
  .unwrap();
  store.publish_semantic_objects(std::slice::from_ref(&object)).unwrap();
  object
}

#[test]
fn persisted_catalog_reopens_without_writes_and_supports_a_later_incremental_update() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let configurations = ["/", "/nested"].into_iter().map(|path| configuration(algorithm, &registry, &memory, path));
    let original = compile_semantic_catalog_v1(request(algorithm, 2), &registry, configurations, &mut store, &memory, &|| false).unwrap();
    let state = original.semantic_state().clone();
    drop(original);
    let before = store.objects.clone();
    let admitted = admit_semantic_catalog_v1(request(algorithm, 2), &state.object_id, &registry, &store, &memory, &|| false).unwrap();
    assert_eq!(admitted.semantic_state(), &state);
    assert_eq!(admitted.configuration_count(), 2);
    assert_eq!(store.objects, before);
    let updated = update_semantic_catalog_v1(
      SemanticCatalogUpdateRequestV1 { compilation: request(algorithm, 1), expected_mutation_count: 1 },
      &admitted,
      &registry,
      [Ok(SemanticCatalogConfigurationMutationV1::Remove("/nested".into()))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(updated.configuration_count(), 1);
    assert_eq!(admitted.semantic_state(), &state);
  }
}

#[test]
fn persisted_default_configuration_admits_shared_dependencies_at_every_hash_width() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let configurations = ["/", "/nested"].into_iter().map(|owner_path| {
      compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source: aeordb::engine::v4::index_configuration_compiler::default_index_configuration_v1(),
          owner_path,
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 128 << 20,
        },
        &Snapshot,
        &memory,
        &|| false,
      )
    });
    let original = compile_semantic_catalog_v1(request(algorithm, 2), &registry, configurations, &mut store, &memory, &|| false).unwrap();
    let state = original.semantic_state().clone();
    drop(original);
    let writes = store.writes;
    let admitted = admit_semantic_catalog_v1(request(algorithm, 2), &state.object_id, &registry, &store, &memory, &|| false).unwrap();
    assert_eq!(admitted.semantic_state(), &state);
    assert_eq!(store.writes, writes);
  }
}

#[test]
fn persisted_admission_refuses_foreign_profiles_counts_and_unavailable_sources() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let compiled = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [configuration(algorithm, &registry, &memory, "/")],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap();
  let original = compiled.semantic_state().clone();
  drop(compiled);
  let state = decode_semantic_object(&original.value, algorithm).unwrap().semantic_state.unwrap();
  for change in 0..4 {
    let mut changed = state.clone();
    let SemanticAvailabilityV1::Complete { compiler_fingerprint, semantic_registry_fingerprint, definition_count, .. } =
      &mut changed.availability
    else {
      unreachable!()
    };
    match change {
      0 => compiler_fingerprint[0] ^= 1,
      1 => semantic_registry_fingerprint[0] ^= 1,
      2 => *definition_count += 1,
      _ => changed.required_capabilities[0] = 1,
    }
    let object = install_state(&mut store, &changed);
    assert!(admit_semantic_catalog_v1(request(algorithm, 1), &object.object_id, &registry, &store, &memory, &|| false).is_err());
  }
  assert!(admit_semantic_catalog_v1(request(algorithm, 2), &original.object_id, &registry, &store, &memory, &|| false).is_err());
  store.read_fault = Some((store.present_reads.get() + 1, ReadFault::Unavailable));
  let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &original.object_id, &registry, &store, &memory, &|| false));
  assert!(
    matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Unavailable)
  );
}

#[test]
fn admission_cancellation_and_workspace_refusal_precede_storage_access() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let store = Store::new(algorithm);
  let error = failure(admit_semantic_catalog_v1(request(algorithm, 0), &[1; 32], &registry, &store, &memory, &|| true));
  assert!(
    matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Cancelled)
  );
  let mut limited = request(algorithm, 0);
  limited.maximum_workspace_bytes = 1;
  let error = failure(admit_semantic_catalog_v1(limited, &[1; 32], &registry, &store, &memory, &|| false));
  assert!(
    matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::ResourceLimit)
  );
  assert_eq!(store.reads.get(), 0);
}

#[test]
fn accurate_counts_do_not_admit_missing_or_orphaned_typed_definitions() {
  for algorithm in [ALGORITHMS[0], ALGORITHMS[2]] {
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let original = compile_semantic_catalog_v1(
      request(algorithm, 1),
      &registry,
      [Ok(configured(algorithm, &memory, &registry, "/"))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap()
    .semantic_state()
    .clone();
    let original_bindings = bindings(&store, &original);
    // Keep every selected object available; mutate only the retained bindings
    // using an independent tree and accurate counts.
    for class in [2, 3, 4, 5, 7] {
      let mut incomplete = original_bindings.clone();
      incomplete.remove(incomplete.iter().position(|binding| binding.kind == class).unwrap());
      let state = replace_bindings(&mut store, &original, &incomplete);
      assert!(
        admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &registry, &store, &memory, &|| false).is_err(),
        "missing class {class}"
      );
    }
    let other = compile_semantic_catalog_v1(
      request(algorithm, 1),
      &registry,
      [Ok(configured(algorithm, &memory, &registry, "/orphan"))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap()
    .semantic_state()
    .clone();
    let other_bindings = bindings(&store, &other);
    for class in [3, 4, 5] {
      let mut extra = original_bindings.clone();
      extra.push(other_bindings.iter().find(|binding| binding.kind == class).unwrap().clone());
      let state = replace_bindings(&mut store, &original, &extra);
      let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &registry, &store, &memory, &|| false));
      assert!(
        matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_orphan"),
        "orphan class {class}"
      );
    }
    let dependency = aeordb::engine::v4::dependency::encode_dependency_record(&dependency(1)).unwrap();
    let dependency = aeordb::engine::v4::namespace::encode_semantic_definition_object(6, &dependency, algorithm).unwrap();
    store.publish_semantic_objects(std::slice::from_ref(&dependency.object)).unwrap();
    let mut extra = original_bindings;
    extra.push(catalog_oracle::Binding {
      kind: 6,
      owner: dependency.semantic_id.clone(),
      semantic: dependency.semantic_id,
      definition: dependency.object.object_id,
    });
    let state = replace_bindings(&mut store, &original, &extra);
    let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &registry, &store, &memory, &|| false));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_orphan"));
  }
}

#[test]
fn a_valid_projection_cannot_be_rebound_to_another_configuration_owner() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let original = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [configuration(algorithm, &registry, &memory, "/")],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap()
  .semantic_state()
  .clone();
  let mut changed = bindings(&store, &original);
  changed.iter_mut().find(|binding| binding.kind == 1).unwrap().owner = b"\x01\0/other/.aeordb-config/indexes.json".to_vec();
  let state = replace_bindings(&mut store, &original, &changed);
  let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &registry, &store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_configuration_owner"));
}

#[test]
fn persisted_admission_preserves_source_error_classes_and_releases_memory() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let original = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [Ok(configured(algorithm, &memory, &registry, "/"))],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap()
  .semantic_state()
  .clone();
  store.present_reads.set(0);
  drop(admit_semantic_catalog_v1(request(algorithm, 1), &original.object_id, &registry, &store, &memory, &|| false).unwrap());
  let reads = store.present_reads.get();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  for (fault, class) in [
    (ReadFault::Unavailable, SemanticCatalogReadErrorClassV1::Unavailable),
    (ReadFault::Resource, SemanticCatalogReadErrorClassV1::ResourceLimit),
    (ReadFault::Missing, SemanticCatalogReadErrorClassV1::Corrupt),
    (ReadFault::Corrupt, SemanticCatalogReadErrorClassV1::Corrupt),
  ] {
    for at in [1, reads / 2, reads] {
      store.present_reads.set(0);
      store.read_fault = Some((at, fault));
      let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &original.object_id, &registry, &store, &memory, &|| false));
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == class), "{fault:?}/{at}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
    }
  }
  store.read_fault = None;
  store.present_reads.set(0);
  let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &original.object_id, &registry, &store, &memory, &|| {
    store.present_reads.get() >= reads
  }));
  assert!(
    matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == SemanticCatalogReadErrorClassV1::Cancelled)
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
}

#[test]
fn admission_requires_automatic_candidates_from_the_exact_captured_registry() {
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
    let changed_registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: Some(br#"{"$v":1,"parsers":{"text/html":"p"}}"#),
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
    let original = compile_semantic_catalog_v1(
      request(algorithm, 1),
      &registry,
      [Ok(configured(algorithm, &memory, &registry, "/"))],
      &mut store,
      &memory,
      &|| false,
    )
    .unwrap()
    .semantic_state()
    .clone();
    drop(admit_semantic_catalog_v1(request(algorithm, 1), &original.object_id, &registry, &store, &memory, &|| false).unwrap());
    let mut changed = bindings(&store, &original);
    let binding = changed.iter_mut().find(|binding| binding.kind == 2).unwrap();
    let projection = decode_semantic_definition_record(&changed_registry.projection().object.value, algorithm).unwrap();
    replace_definition(&mut store, binding, projection.definition);
    let state = replace_bindings(&mut store, &original, &changed);
    let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &changed_registry, &store, &memory, &|| false));
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_parser_registry"));
  }
}

#[test]
fn projection_schema_field_names_index_order_and_parent_ownership_are_validated() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let configurations = ["/", "/nested"].into_iter().map(|owner_path| {
    compile_index_configuration_v1(
      IndexConfigurationCompilationRequestV1 {
        source: br#"{"$v":1,"indexes":[{"name":"title","type":["unicode_trigram_v1","utf8_binary_order_v1"]}]}"#,
        owner_path,
        registry: &registry,
        hash_algorithm: algorithm,
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 128 << 20,
      },
      &AvailableSnapshot,
      &memory,
      &|| false,
    )
  });
  let original = compile_semantic_catalog_v1(request(algorithm, 2), &registry, configurations, &mut store, &memory, &|| false)
    .unwrap()
    .semantic_state()
    .clone();
  let original_bindings = bindings(&store, &original);
  let primary_position = original_bindings.iter().position(|binding| binding.kind == 1).unwrap();
  for mode in 0..7 {
    let mut changed = original_bindings.clone();
    let mut projection =
      decode_canonical_value(&definition_bytes(&store, &changed[primary_position]), CanonicalValueBounds::CONFIG).unwrap();
    let CanonicalConfigValueV1::Map(members) = &mut projection else {
      unreachable!()
    };
    if mode == 0 {
      members.insert("extra".into(), CanonicalConfigValueV1::Null);
    } else {
      let CanonicalConfigValueV1::Map(fields) = members.get_mut("fields").unwrap() else {
        unreachable!()
      };
      if mode == 1 {
        let field = fields.remove("title").unwrap();
        fields.insert("wrong-name".into(), field);
      } else {
        let CanonicalConfigValueV1::Map(field) = fields.get_mut("title").unwrap() else {
          unreachable!()
        };
        if mode == 2 {
          field.insert("extra".into(), CanonicalConfigValueV1::Null);
        } else {
          let parent = field.get("value_store_id").unwrap().clone();
          let CanonicalConfigValueV1::Array(indexes) = field.get_mut("indexes").unwrap() else {
            unreachable!()
          };
          match mode {
            3 => indexes.clear(),
            4 => indexes.reverse(),
            5 => indexes.push(indexes.last().unwrap().clone()),
            _ => {
              let CanonicalConfigValueV1::Bytes(parent) = parent else {
                unreachable!()
              };
              let foreign = changed
                .iter()
                .filter(|binding| binding.kind == 5)
                .find(|binding| {
                  let bytes = definition_bytes(&store, binding);
                  bytes[32..32 + algorithm.hash_length()] != parent
                })
                .unwrap();
              *indexes = vec![CanonicalConfigValueV1::Bytes(foreign.semantic.clone())];
            }
          }
        }
      }
    }
    let bytes = encode_canonical_value(&projection, CanonicalValueBounds::CONFIG).unwrap();
    replace_definition(&mut store, &mut changed[primary_position], &bytes);
    let state = replace_bindings(&mut store, &original, &changed);
    let error = failure(admit_semantic_catalog_v1(request(algorithm, 2), &state.object_id, &registry, &store, &memory, &|| false));
    let expected = match mode {
      1 => "semantic_catalog_value_owner",
      4 | 5 => "semantic_catalog_index_order",
      6 => "semantic_catalog_index_owner",
      _ => "semantic_catalog_projection_schema",
    };
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == expected), "mode {mode}");
  }
}

#[test]
fn admission_read_count_grows_with_bindings_not_their_pairwise_product() {
  let mut measured = Vec::new();
  for count in [16, 64] {
    let algorithm = ALGORITHMS[0];
    let memory = memory();
    let registry = registry(algorithm, &memory);
    let mut store = Store::new(algorithm);
    let configurations = (0..count).map(|index| configuration(algorithm, &registry, &memory, &format!("/scope/{index}")));
    let state = compile_semantic_catalog_v1(request(algorithm, count), &registry, configurations, &mut store, &memory, &|| false)
      .unwrap()
      .semantic_state()
      .clone();
    store.reads.set(0);
    drop(admit_semantic_catalog_v1(request(algorithm, count), &state.object_id, &registry, &store, &memory, &|| false).unwrap());
    measured.push(store.reads.get());
    assert!(store.reads.get() < 40 * count as usize + 40);
  }
  assert!(measured[1] <= measured[0] * 5, "pairwise growth: {measured:?}");
}

#[test]
fn cancellation_and_host_pressure_at_each_checkpoint_release_all_admission_memory() {
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  let state = compile_semantic_catalog_v1(
    request(algorithm, 1),
    &registry,
    [configuration(algorithm, &registry, &memory, "/")],
    &mut store,
    &memory,
    &|| false,
  )
  .unwrap()
  .semantic_state()
  .clone();
  let checks = Cell::new(0);
  drop(
    admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &registry, &store, &memory, &|| {
      checks.set(checks.get() + 1);
      false
    })
    .unwrap(),
  );
  let count = checks.get();
  let reserved = memory.snapshot().unwrap().reserved_bytes;
  for pressure in [false, true] {
    for at in 1..=count {
      checks.set(0);
      let error = failure(admit_semantic_catalog_v1(request(algorithm, 1), &state.object_id, &registry, &store, &memory, &|| {
        checks.set(checks.get() + 1);
        if checks.get() < at {
          return false;
        }
        if pressure {
          memory.update_host_sample(HostMemorySample { rss_bytes: 512 << 20, ..Default::default() }).unwrap();
          false
        } else {
          true
        }
      }));
      let expected = if pressure { SemanticCatalogReadErrorClassV1::ResourceLimit } else { SemanticCatalogReadErrorClassV1::Cancelled };
      assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.class() == expected), "{pressure}/{at}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, reserved);
      memory.update_host_sample(HostMemorySample::default()).unwrap();
    }
  }
}

#[test]
fn missing_content_only_wrong_kind_and_wrong_identity_states_never_admit() {
  use aeordb::engine::v4::namespace::SemanticUnavailableReasonV1;
  let algorithm = ALGORITHMS[0];
  let memory = memory();
  let registry = registry(algorithm, &memory);
  let mut store = Store::new(algorithm);
  for identity in [vec![], vec![0; 32], vec![1; 64]] {
    assert!(admit_semantic_catalog_v1(request(algorithm, 0), &identity, &registry, &store, &memory, &|| false).is_err());
  }
  assert_eq!(store.reads.get(), 0);
  assert!(admit_semantic_catalog_v1(request(algorithm, 0), &[1; 32], &registry, &store, &memory, &|| false).is_err());
  let state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    },
    algorithm,
  )
  .unwrap();
  store.publish_semantic_objects(std::slice::from_ref(&state)).unwrap();
  assert!(matches!(
    failure(admit_semantic_catalog_v1(request(algorithm, 0), &state.object_id, &registry, &store, &memory, &|| false)),
    SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_base_unavailable", .. }
  ));
  store.objects.insert((1, vec![1; 32]), state.value);
  let error = failure(admit_semantic_catalog_v1(request(algorithm, 0), &[1; 32], &registry, &store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_state_identity"));
  let object = &registry.projection().object;
  store.objects.insert((1, object.object_id.clone()), object.value.clone());
  let error = failure(admit_semantic_catalog_v1(request(algorithm, 0), &object.object_id, &registry, &store, &memory, &|| false));
  assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(error) if error.code() == "semantic_catalog_state_kind"));
}
