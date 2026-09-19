use super::*;
use crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1;

// Fixture-only physical byte oracle; never consult validator summaries/counters.
fn physical_file_cost(publisher: &V4FirstAuthorityPublisher, key: &[u8]) -> (u64, u64) {
  let header = publisher.observe().unwrap().selected.header;
  let kv = publisher.lock_kv().unwrap();
  let bytes = read_entity_bounded(&publisher.file, &*kv, key, 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).unwrap();
  let record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), entity.entity_version).unwrap();
  let mut bytes = u64::from(kv.get(key).unwrap().unwrap().total_length);
  for chunk in &record.chunk_hashes {
    bytes += u64::from(kv.get(chunk).unwrap().unwrap().total_length);
  }
  (bytes, 1 + record.chunk_hashes.len() as u64)
}

fn physical_control_cost(publisher: &V4FirstAuthorityPublisher, kind: SystemControlKindV1, identity: &[u8]) -> (Vec<u8>, u64, u64) {
  let header = publisher.observe().unwrap().selected.header;
  let path = system_control_path(kind, identity, SystemControlSlotV1::Immutable).unwrap();
  let (bytes, reads) = physical_file_cost(publisher, &first_authority_file_path_hash(&path, header.hash_algorithm));
  let kv = publisher.lock_kv().unwrap();
  let body = load_immutable_system_control_file(&publisher.file, &*kv, &header, kind, identity).unwrap().unwrap().bytes;
  (body, bytes, reads)
}

#[test]
fn retained_source_union_validation_exact_shared_byte_and_separate_work_ceilings() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retained-union-exact-cost", None, [1; 16], algorithm, 0);
    let initial = request_for_database_and_algorithm([1; 16], algorithm);
    let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
    let tree = &initial.namespace_tree.root_hash;
    let globals = absent_globals();
    seed_validation_pair(&publisher, &base, tree, &globals, &globals, &globals, 0);
    let (companion, companion_bytes, companion_reads) =
      physical_control_cost(&publisher, SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity());
    let (_, checkpoint_bytes, checkpoint_reads) =
      physical_control_cost(&publisher, SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity());
    let manifest = decode_semantic_source_capture_v1(&companion, algorithm).unwrap();
    assert_eq!(manifest.base_source_catalog, manifest.requested_source_catalog);
    let (_, node_bytes, node_reads) =
      physical_control_cost(&publisher, SystemControlKindV1::SemanticSourceNode, manifest.base_source_catalog);
    let (_, admission_bytes, admission_reads) = physical_control_cost(&publisher, SystemControlKindV1::RootAdmissionCommit, &base);
    let semantic_key =
      first_authority_file_path_hash(&semantic_object_path(algorithm, 1, &initial.semantic_state.object_id).unwrap(), algorithm);
    let (semantic_bytes, semantic_reads) = physical_file_cost(&publisher, &semantic_key);
    let root_bytes = u64::from(publisher.locator(&base).unwrap().unwrap().total_length);
    let tree_bytes = u64::from(publisher.locator(tree).unwrap().unwrap().total_length);
    // Two paired catalog leaves then one base fingerprint leaf; two empty
    // namespace trees in each pass. Each leaf also charges one node/two rows.
    let expected_bytes =
      companion_bytes + checkpoint_bytes + root_bytes + semantic_bytes + admission_bytes + 3 * node_bytes + 4 * tree_bytes;
    let expected_catalog_work = companion_reads + checkpoint_reads + 1 + semantic_reads + admission_reads + 3 * (node_reads + 1 + 2);
    with_validation_capture(&publisher, &path, |capture, memory, _| {
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let mut exact = validation_bounds(tree);
      exact.catalog.maximum_read_bytes = expected_bytes;
      exact.catalog.maximum_work = expected_catalog_work;
      exact.namespace.sources.maximum_read_bytes = 4 * tree_bytes;
      exact.namespace.maximum_work = 4;
      let summary = capture.validate_captured_semantic_source_union(&[2; 16], 1, exact).unwrap();
      assert_eq!((summary.read_bytes, summary.catalog_work, summary.namespace_work), (expected_bytes, expected_catalog_work, 4));
      for case in 0..4 {
        let mut bounds = exact;
        match case {
          0 => bounds.catalog.maximum_read_bytes -= 1,
          1 => bounds.catalog.maximum_work -= 1,
          2 => bounds.namespace.sources.maximum_read_bytes -= 1,
          3 => bounds.namespace.maximum_work -= 1,
          _ => unreachable!(),
        }
        let error = capture.validate_captured_semantic_source_union(&[2; 16], 1, bounds).unwrap_err();
        let expected = match case {
          0 | 2 => "semantic_source_read_bound",
          1 => "semantic_source_catalog_work_bound",
          3 => "semantic_namespace_source_work_bound",
          _ => unreachable!(),
        };
        assert_eq!(validation_error_code(error), expected, "{algorithm:?}/{case}");
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
        assert_eq!(capture.validate_captured_semantic_source_union(&[2; 16], 1, exact).unwrap(), summary);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      }
    });
  }
}

#[test]
fn retained_source_union_validation_releases_all_reservations_after_actual_metadata_allocation_refusal() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retained-union-allocation", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  let globals = absent_globals();
  seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &globals, &globals, &globals, 0);
  let length = publisher.locator(&base).unwrap().unwrap().total_length as usize;
  with_validation_capture(&publisher, &path, |capture, memory, _| {
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let bounds = validation_bounds(&initial.namespace_tree.root_hash);
    let (result, allocations) = allocation_probe::measure(length, || capture.validate_captured_semantic_source_union(&[2; 16], 1, bounds));
    assert!(allocations.injected_failure, "{allocations:?}");
    assert_eq!(validation_error_code(result.unwrap_err()), "semantic_source_catalog_allocation");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    capture.validate_captured_semantic_source_union(&[2; 16], 1, bounds).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  });
}

#[test]
fn retained_source_union_validation_entry_and_final_cancellation_and_pressure_refuse_without_writes() {
  for pressure in [false, true] {
    for late in [false, true] {
      let algorithm = HashAlgorithm::Blake3_256;
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("retained-union-interruption", None, [1; 16], algorithm, 0);
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
      let globals = absent_globals();
      seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &globals, &globals, &globals, 0);
      with_validation_capture(&publisher, &path, |capture, memory, cancellation| {
        let bounds = validation_bounds(&initial.namespace_tree.root_hash);
        let retained = memory.snapshot().unwrap().reserved_bytes;
        let interrupt = || {
          if pressure {
            memory
              .update_host_sample(crate::engine::memory_coordinator::HostMemorySample { rss_bytes: 96 << 20, ..Default::default() })
              .unwrap();
          } else {
            cancellation.cancel();
          }
        };
        if !late {
          interrupt();
        }
        let mut calls = 0;
        let result = capture.validate_captured_semantic_source_union_observed(&[2; 16], 1, bounds, || {
          calls += 1;
          assert!(late);
          interrupt();
        });
        assert_eq!(calls, usize::from(late));
        assert_eq!(
          validation_error_code(result.unwrap_err()),
          if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" }
        );
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
        memory.update_host_sample(Default::default()).unwrap();
        if pressure {
          capture.validate_captured_semantic_source_union(&[2; 16], 1, bounds).unwrap();
        }
      });
      // Cancellation is terminal for that capture; a fresh capture can retry.
      with_validation_capture(&publisher, &path, |capture, _, _| {
        capture.validate_captured_semantic_source_union(&[2; 16], 1, validation_bounds(&initial.namespace_tree.root_hash)).unwrap();
      });
    }
  }
}

#[test]
fn retained_source_union_validation_aggregate_alias_and_operational_limits_refuse_then_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retained-union-operational", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  let module = plugin_fixtures::module("both");
  let (alias, artifact) = seed_validation_module(&publisher, &module);
  let configuration = br#"{"$v":1,"indexes":[{"name":"value","type":"typed_exact_blake3_v1","source":{"plugin":"parse"}}]}"#;
  seed_files(&publisher, &[(INDEX_SOURCE.to_string(), "application/json", configuration)]);
  let revision = seed_retained_revision(&publisher, INDEX_SOURCE);
  let mut sources = absent_globals();
  sources.insert(INDEX_SOURCE.to_string(), Some(revision));
  sources.insert(plugin_fixtures::alias_path(), Some(alias));
  sources.insert(plugin_fixtures::artifact_path(&module), Some(artifact));
  seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &sources, &sources, &sources, 1);
  with_validation_capture(&publisher, &path, |capture, memory, _| {
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let good = validation_bounds(&initial.namespace_tree.root_hash);
    for case in 0..11 {
      let mut bounds = good;
      match case {
        0 => bounds.maximum_alias_occurrences = 1,
        1 => bounds.maximum_plugin_module_bytes = module.len() - 1,
        2 => bounds.catalog.maximum_source_bytes = 1,
        3 => bounds.catalog.maximum_chunk_entity_bytes = 1,
        4 => bounds.catalog.maximum_work = 1,
        5 => bounds.namespace.maximum_work = 1,
        6 => bounds.maximum_alias_workspace_bytes = 1,
        7 => bounds.maximum_fingerprint_workspace_bytes = 1,
        8 => bounds.maximum_plugin_workspace_bytes = 1,
        9 => bounds.namespace.maximum_directory_entity_bytes = 1,
        10 => bounds.catalog.maximum_read_bytes = 1,
        _ => unreachable!(),
      }
      let error = capture.validate_captured_semantic_source_union(&[2; 16], 1, bounds).expect_err(&format!("case {case} must refuse"));
      if case == 0 {
        assert_eq!(
          validation_error_code(error),
          "semantic_source_union_alias_work",
          "each source alone fits; their aggregate must not reset"
        );
      } else {
        assert!(
          matches!(
            error,
            NativeSemanticSourceUnionErrorV1::Source(_)
              | NativeSemanticSourceUnionErrorV1::Namespace(_)
              | NativeSemanticSourceUnionErrorV1::Plugin(_)
              | NativeSemanticSourceUnionErrorV1::Compilation(_)
          ),
          "{case}: {error:?}"
        );
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(capture.validate_captured_semantic_source_union(&[2; 16], 1, good).unwrap().requested_configuration_count, 1);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
  });
}

#[test]
fn retained_source_union_validation_large_module_buffers_do_not_survive_the_summary() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retained-union-large-module", None, [1; 16], algorithm, 0);
  let initial = request_for_database_and_algorithm([1; 16], algorithm);
  let base = publisher.publish(&initial).unwrap().namespace_root.root_hash;
  let mut module = plugin_fixtures::module("both");
  plugin_fixtures::custom("padding", &vec![0; 2 << 20], &mut module);
  let (alias, artifact) = seed_validation_module(&publisher, &module);
  let registry = br#"{"$v":1,"parsers":{"text/plain":"parse"}}"#;
  seed_files(&publisher, &[(PARSER_SOURCE.to_string(), "application/json", registry)]);
  let revision = seed_retained_revision(&publisher, PARSER_SOURCE);
  let mut sources = absent_globals();
  sources.insert(PARSER_SOURCE.to_string(), Some(revision));
  sources.insert(plugin_fixtures::alias_path(), Some(alias));
  sources.insert(plugin_fixtures::artifact_path(&module), Some(artifact));
  seed_validation_pair(&publisher, &base, &initial.namespace_tree.root_hash, &sources, &sources, &sources, 0);
  with_validation_capture(&publisher, &path, |capture, memory, _| {
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut bounds = validation_bounds(&initial.namespace_tree.root_hash);
    bounds.maximum_plugin_module_bytes = 4 << 20;
    bounds.catalog.maximum_source_bytes = 4 << 20;
    bounds.catalog.maximum_chunk_entity_bytes = 4 << 20;
    let summary = capture.validate_captured_semantic_source_union(&[2; 16], 1, bounds).unwrap();
    assert_eq!(summary.protected_paths, 4);
    assert!(summary.read_bytes > 6 * module.len() as u64);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  });
}
