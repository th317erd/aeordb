//! Provisional callbacks never authorize task recovery or reclamation.
use super::*;

#[test]
fn native_semantic_task_graph_keeps_one_read_and_work_budget_across_all_branches() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-graph-budget", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  seed_captured_graph(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let original = capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap();
  let exact = NativeSemanticTaskGraphBoundsV1 { maximum_work: original.work, maximum_read_bytes: original.read_bytes, ..graph_bounds() };
  for case in 0..3 {
    let mut bounds = exact;
    if case == 1 {
      bounds.maximum_read_bytes -= 1;
    }
    if case == 2 {
      bounds.maximum_work -= 1;
    }
    let mut calls = 0;
    let result = capture.visit_captured_semantic_task_physical_entries(&[2; 16], bounds, |_| {
      calls += 1;
      Ok(())
    });
    if case == 0 {
      assert_eq!(result.unwrap(), original);
    } else {
      let error = result.unwrap_err();
      assert_eq!(error.code(), if case == 1 { "semantic_task_inventory_read_bound" } else { "semantic_task_graph_work_bound" });
      assert!(calls > 4, "failure must occur after multiple provisional branches");
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_checks_cancellation_and_memory_even_on_final_callback() {
  for pressure in [false, true] {
    for last in [false, true] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("task-graph-interruption", None, [1; 16], HashAlgorithm::Blake3_256, 0);
      seed_captured_graph(&publisher);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let total = capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap().physical_reads;
      let stop_at = if last { total } else { 1 };
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = fs::read(&path).unwrap();
      let mut calls = 0;
      let error = capture
        .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| {
          calls += 1;
          if calls == stop_at {
            if pressure {
              memory
                .update_host_sample(crate::engine::memory_coordinator::HostMemorySample { rss_bytes: 96 << 20, ..Default::default() })
                .unwrap();
            } else {
              cancellation.cancel();
            }
          }
          Ok(())
        })
        .unwrap_err();
      assert_eq!(calls, stop_at);
      assert_eq!(error.code(), if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" });
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_task_graph_preserves_callback_failure_when_cancellation_arrives_together() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-graph-callback", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  seed_captured_graph(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let error = capture
    .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| {
      cancellation.cancel();
      Err(SemanticMutationObservationErrorV1::Resource { code: "task_graph_distinctive_callback", message: "original cause" })
    })
    .unwrap_err();
  assert!(matches!(
    error,
    SemanticTaskGraphErrorV1::Source(SemanticMutationObservationErrorV1::Resource {
      code: "task_graph_distinctive_callback",
      message: "original cause"
    })
  ));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_graph_missing_or_corrupt_ordinary_chunks_cannot_complete() {
  for missing in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-graph-broken-chunk", None, [1; 16], algorithm, 0);
    seed_captured_graph(&publisher);
    let key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
    if missing {
      assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
      seed_files(&publisher, &[]);
    } else {
      corrupt_last_entity_byte(&publisher, &key);
    }
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut calls = 0;
    let error = capture
      .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| {
        calls += 1;
        Ok(())
      })
      .unwrap_err();
    assert_eq!(error.code(), if missing { "semantic_source_chunk_missing" } else { "integrity_hash_mismatch" });
    assert!(calls > 4);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_graph_missing_companion_does_not_invent_empty_sources() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-graph-no-companion", None, [1; 16], algorithm, 0);
  seed_captured_graph(&publisher);
  let key = first_authority_file_path_hash(
    &system_control_path(SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity(), SystemControlSlotV1::Immutable).unwrap(),
    algorithm,
  );
  assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
  seed_files(&publisher, &[]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let error = capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap_err();
  assert_eq!(error.code(), "semantic_source_catalog_capture_missing");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_graph_keeps_old_selection_when_a_later_slot_releases_pins() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("task-graph-snapshot", None, [1; 16], algorithm, 0);
  let (expected, _, _) = seed_captured_graph(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let mut released = frozen(algorithm, "task");
  let later_sequence = u64::from_le_bytes(released[16..24].try_into().unwrap()) + 1;
  released[16..24].copy_from_slice(&later_sequence.to_le_bytes());
  released[128..130].copy_from_slice(&9u16.to_le_bytes());
  released[130..132].copy_from_slice(&1u16.to_le_bytes());
  crc(&mut released);
  seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &released)]);
  let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut actual = PhysicalSet::new();
  assert_eq!(
    old
      .visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap()
      .disposition,
    SemanticMutationObservationDispositionV1::CheckpointHeld
  );
  assert_eq!(actual, expected);
  assert_eq!(
    fresh.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap().disposition,
    SemanticMutationObservationDispositionV1::ReleasedTerminal
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}
