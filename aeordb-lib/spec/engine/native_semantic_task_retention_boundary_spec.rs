//! Composed quotas and provisional failure/capture semantics.
#[path = "native_semantic_task_retention_budget_spec.rs"]
mod budgets;
#[path = "native_semantic_task_retention_later_failure_spec.rs"]
mod later_failure;
use super::*;
use crate::engine::memory_coordinator::HostMemorySample;

#[test]
fn native_semantic_task_retention_all_algorithms_preserve_full_locator_union() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retention-algorithms", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    seed_second_task(&publisher, &mut expected);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut actual = PhysicalSet::new();
    let summary = capture
      .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(summary.tasks, 2);
    assert!(summary.complete);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_retention_empty_released_and_invalid_total_bounds() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retention-empty", None, [1; 16], algorithm, 0);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let absent = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let empty = absent.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| panic!("empty inventory")).unwrap();
  assert_eq!(empty.tasks, 0);
  assert_eq!(empty.read_bytes, 0);
  assert!(empty.complete && empty.work > 0);
  for (work, bytes) in [(0, 1), (1, 0)] {
    let error = absent
      .visit_captured_semantic_task_retention_entries(
        NativeSemanticTaskRetentionBoundsV1 { maximum_work: work, maximum_read_bytes: bytes, ..retention_bounds() },
        |_| panic!("invalid bounds"),
      )
      .unwrap_err();
    assert_eq!(error.code(), "semantic_task_retention_bounds");
  }
  let mut task = frozen(algorithm, "task");
  task[128..130].copy_from_slice(&9u16.to_le_bytes());
  task[130..132].copy_from_slice(&1u16.to_le_bytes());
  crc(&mut task);
  let generation = frozen(algorithm, "generation");
  populate(&publisher, &task, None, Some(&generation));
  let released = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mut expected = PhysicalSet::new();
  include_control(&publisher, &mut expected, SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task);
  include_control(&publisher, &mut expected, SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &generation);
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut actual = PhysicalSet::new();
  let summary = released
    .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
      actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
      Ok(())
    })
    .unwrap();
  assert_eq!(actual, expected);
  assert_eq!(summary.tasks, 1);
  assert!(summary.complete);
  assert_eq!(absent.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| panic!("old capture")).unwrap(), empty);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_retention_preserves_callback_error_and_final_interruptions() {
  for pressure in [false, true] {
    for original_error in [false, true] {
      for last in [false, true] {
        let (_directory, path, _coordinator, publisher) =
          create_environment_for_algorithm_at_kv_stage("retention-interrupt", None, [1; 16], HashAlgorithm::Blake3_256, 0);
        let (mut expected, _, _) = seed_captured_graph(&publisher);
        seed_second_task(&publisher, &mut expected);
        let memory = observation_memory();
        let cancellation = CancellationToken::new();
        let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
        let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
        let mut total = 0;
        capture
          .visit_captured_semantic_task_retention_entries(retention_bounds(), |_| {
            total += 1;
            Ok(())
          })
          .unwrap();
        let stop_at = if last { total } else { 1 };
        let retained = memory.snapshot().unwrap().reserved_bytes;
        let before = fs::read(&path).unwrap();
        let mut calls = 0;
        let error = capture
          .visit_captured_semantic_task_retention_entries(retention_bounds(), |_| {
            calls += 1;
            if calls == stop_at {
              if pressure {
                memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
              } else {
                cancellation.cancel();
              }
              if original_error {
                return Err(SemanticMutationObservationErrorV1::Resource {
                  code: "retention_original_callback",
                  message: "original cause",
                });
              }
            }
            Ok(())
          })
          .unwrap_err();
        assert_eq!(calls, stop_at);
        assert_eq!(
          error.code(),
          if original_error {
            "retention_original_callback"
          } else if pressure {
            "semantic_task_observation_memory"
          } else {
            "semantic_task_observation_cancelled"
          }
        );
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
        assert_eq!(fs::read(&path).unwrap(), before);
      }
    }
  }
}

#[test]
fn native_semantic_task_retention_opaque_payloads_still_require_present_references() {
  for missing in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("retention-ordinary", None, [1; 16], algorithm, 0);
    let (expected, _, _) = seed_captured_graph(&publisher);
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
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut calls = 0;
    let mut actual = PhysicalSet::new();
    let result = capture.visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
      calls += 1;
      actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
      Ok(())
    });
    if missing {
      assert_eq!(result.unwrap_err().code(), "semantic_source_chunk_missing");
      assert!(calls > 4, "no partial success after earlier callbacks");
    } else {
      assert!(result.unwrap().complete);
      assert_eq!(actual, expected);
      assert_eq!(
        capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap_err().code(),
        "integrity_hash_mismatch"
      );
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_retention_allocation_refusal_is_retryable() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retention-allocation", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (_, base, _) = seed_captured_graph(&publisher);
  let length = publisher.locator(&base).unwrap().unwrap().total_length as usize;
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let (result, allocations) = measure(length, || capture.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().code(), "first_authority_readback_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert!(capture.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap().complete);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_retention_callbacks_can_publish_and_nest_without_changing_capture() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("retention-history", None, [1; 16], algorithm, 0);
  let (expected, _, _) = seed_captured_graph(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mut published = false;
  let mut actual = PhysicalSet::new();
  old
    .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
      if !published {
        published = true;
        let mut released = frozen(algorithm, "task");
        let sequence = u64::from_le_bytes(released[16..24].try_into().unwrap()) + 1;
        released[16..24].copy_from_slice(&sequence.to_le_bytes());
        released[128..130].copy_from_slice(&9u16.to_le_bytes());
        released[130..132].copy_from_slice(&1u16.to_le_bytes());
        crc(&mut released);
        seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &released)]);
        let nested_memory = observation_memory();
        let nested_protection = publisher.acquire_staging_protection(&nested_memory, &cancellation).unwrap();
        let nested =
          nested_protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &nested_memory, &cancellation).unwrap();
        let mut controls = PhysicalSet::new();
        let summary = nested
          .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
            controls.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
            Ok(())
          })
          .unwrap();
        assert_eq!(summary.tasks, 1);
        assert!(summary.complete);
        assert_eq!(controls.len(), 6, "both task slots and generation, without checkpoint dependencies");
      }
      actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
      Ok(())
    })
    .unwrap();
  assert!(published);
  assert_eq!(actual, expected);
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut repeated = PhysicalSet::new();
  assert!(
    old
      .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
        repeated.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap()
      .complete
  );
  assert_eq!(repeated, expected);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
