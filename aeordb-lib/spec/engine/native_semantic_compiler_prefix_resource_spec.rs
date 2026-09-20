use super::*;

fn empty_model() -> Model {
  Model {
    base: Configurations::new(),
    requested: Configurations::new(),
    compiled_base: false,
    complete_empty_base: false,
    cursor: None,
    pruning: false,
  }
}

fn file_cost(publisher: &V4FirstAuthorityPublisher, path: &str) -> u64 {
  let header = publisher.observe().unwrap().selected.header;
  let kv = publisher.lock_kv().unwrap();
  let key = first_authority_file_path_hash(path, header.hash_algorithm);
  let bytes = read_entity_bounded(&publisher.file, &*kv, &key, 4 << 20, header.write_sequence_high_water).unwrap().unwrap();
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).unwrap();
  let record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), entity.entity_version).unwrap();
  u64::from(kv.get(&key).unwrap().unwrap().total_length)
    + record.chunk_hashes.iter().map(|key| u64::from(kv.get(key).unwrap().unwrap().total_length)).sum::<u64>()
}

#[test]
fn retained_compiler_prefix_independent_physical_cost_covers_the_entire_shared_operation() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    with_model(algorithm, empty_model(), |capture, checkpoint_bytes, memory, _, publisher| {
      let checkpoint = decode_semantic_mutation_checkpoint(checkpoint_bytes, algorithm).unwrap();
      let identity = checkpoint_identity();
      let control_path = |kind, identity: &[u8]| system_control_path(kind, identity, SystemControlSlotV1::Immutable).unwrap();
      let header = publisher.observe().unwrap().selected.header;
      let companion = {
        let kv = publisher.lock_kv().unwrap();
        load_immutable_system_control_file(&publisher.file, &*kv, &header, SystemControlKindV1::SemanticSourceCapture, &identity)
          .unwrap()
          .unwrap()
          .bytes
      };
      let manifest = crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1(&companion, algorithm).unwrap();
      let initial = request_for_database_and_algorithm([1; 16], algorithm);
      let registry = crate::engine::v4::parser_registry_compiler::compile_parser_registry_v1(
        ParserRegistryCompilationRequestV1 {
          source: None,
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: 64 << 20,
        },
        &NoAliases,
        memory,
        &|| false,
      )
      .unwrap();
      let source_node = file_cost(publisher, &control_path(SystemControlKindV1::SemanticSourceNode, manifest.base_source_catalog));
      assert_eq!(manifest.base_source_catalog, manifest.requested_source_catalog);
      let tree = u64::from(publisher.locator(checkpoint.staged_directory_root).unwrap().unwrap().total_length);
      let leaf = file_cost(publisher, &semantic_object_path(algorithm, 2, checkpoint.catalog_root.unwrap()).unwrap());
      let definition = file_cost(publisher, &semantic_object_path(algorithm, 4, &registry.projection().object.object_id).unwrap());
      // Three full source-catalog passes and three point selections; three
      // pairs of empty namespace reads; two complete catalog walks followed by
      // one absent global point lookup; registry definition in each walk.
      let expected = file_cost(publisher, &control_path(SystemControlKindV1::SemanticSourceCapture, &identity))
        + file_cost(publisher, &control_path(SystemControlKindV1::SemanticMutationCheckpoint, &identity))
        + u64::from(publisher.locator(checkpoint.base_namespace_root).unwrap().unwrap().total_length)
        + file_cost(publisher, &semantic_object_path(algorithm, 1, &initial.semantic_state.object_id).unwrap())
        + file_cost(publisher, &control_path(SystemControlKindV1::RootAdmissionCommit, checkpoint.base_namespace_root))
        + 6 * source_node
        + 6 * tree
        + 3 * leaf
        + 2 * definition;
      let mut exact = prefix_bounds(checkpoint_bytes, algorithm);
      exact.sources.catalog.maximum_read_bytes = expected;
      exact.sources.catalog.maximum_work = 46;
      exact.sources.namespace.sources.maximum_read_bytes = 6 * tree;
      exact.sources.namespace.maximum_work = 7;
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let admitted = capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, exact).unwrap();
      assert_eq!((admitted.sources().read_bytes, admitted.sources().catalog_work, admitted.sources().namespace_work), (expected, 46, 7));
      drop(admitted);
      for case in 0..4 {
        let mut limited = exact;
        match case {
          0 => limited.sources.catalog.maximum_read_bytes -= 1,
          1 => limited.sources.catalog.maximum_work -= 1,
          2 => limited.sources.namespace.sources.maximum_read_bytes -= 1,
          3 => limited.sources.namespace.maximum_work -= 1,
          _ => unreachable!(),
        }
        assert!(capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, limited).is_err(), "case{case}");
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        drop(capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, exact).unwrap());
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      }
    });
  }
}

#[test]
fn retained_compiler_prefix_workspace_limits_and_actual_control_allocation_refusal_release_and_retry() {
  with_model(HashAlgorithm::Blake3_256, empty_model(), |capture, checkpoint, memory, _, _| {
    let bounds = prefix_bounds(checkpoint, HashAlgorithm::Blake3_256);
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    for case in 0..3 {
      let mut limited = bounds;
      match case {
        0 => limited.maximum_compiler_workspace_bytes = 1,
        1 => limited.maximum_alias_snapshot_bytes = 1,
        2 => limited.maximum_semantic_decode_workspace_bytes = (16 << 20) - 1,
        _ => unreachable!(),
      }
      assert!(capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, limited).is_err(), "case{case}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      drop(capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, bounds).unwrap());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    let (result, allocations) =
      allocation_probe::measure(checkpoint.len(), || capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, bounds));
    assert!(allocations.injected_failure, "{allocations:?}");
    let error = result.err().expect("failed checkpoint-body allocation cannot produce admitted progress");
    assert!(error.to_string().contains("semantic_source_catalog_allocation"), "{error}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    drop(capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, bounds).unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  });
}

#[test]
fn retained_compiler_prefix_entry_and_final_cancellation_and_pressure_refuse_read_only() {
  for pressure in [false, true] {
    for late in [false, true] {
      with_model(HashAlgorithm::Blake3_256, empty_model(), |capture, checkpoint, memory, cancellation, publisher| {
        let baseline = memory.snapshot().unwrap().reserved_bytes;
        let bounds = prefix_bounds(checkpoint, HashAlgorithm::Blake3_256);
        let interrupt = || {
          if pressure {
            memory
              .update_host_sample(crate::engine::memory_coordinator::HostMemorySample { rss_bytes: 512 << 20, ..Default::default() })
              .unwrap();
          } else {
            cancellation.cancel();
          }
        };
        if !late {
          interrupt();
        }
        let mut calls = 0;
        let result = capture.admit_captured_semantic_compiler_progress_observed(&[2; 16], 1, bounds, || {
          calls += 1;
          assert!(late);
          interrupt();
        });
        let error = result.err().expect("interrupted prefix must refuse");
        assert!(error.to_string().contains(if pressure { "memory" } else { "cancelled" }), "{error}");
        assert_eq!(calls, usize::from(late));
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        memory.update_host_sample(Default::default()).unwrap();
        if pressure {
          drop(capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, bounds).unwrap());
        } else {
          let retry_cancellation = CancellationToken::new();
          let retry_protection = publisher.acquire_staging_protection(memory, &retry_cancellation).unwrap();
          let retry_capture = retry_protection.capture_semantic_mutation_inventory(capture_bounds(), memory, &retry_cancellation).unwrap();
          drop(retry_capture.admit_captured_semantic_compiler_progress(&[2; 16], 1, bounds).unwrap());
        }
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      });
    }
  }
}
