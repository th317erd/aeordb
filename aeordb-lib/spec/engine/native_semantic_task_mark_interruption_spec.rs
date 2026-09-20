//! Deterministic interruptions at the first and final admitted mark callbacks.
use super::*;
use crate::engine::memory_coordinator::HostMemorySample;

#[test]
fn native_semantic_task_mark_preserves_first_error_at_final_interruptions() {
  for pressure in [false, true] {
    for original_error in [false, true] {
      for last in [false, true] {
        let (_directory, path, _coordinator, publisher) =
          create_environment_for_algorithm_at_kv_stage("mark-final-interrupt", None, [1; 16], HashAlgorithm::Blake3_256, 0);
        let (mut expected, _, _) = seed_captured_graph(&publisher);
        seed_second_task(&publisher, &mut expected);
        flush_mark_fixture(&publisher);
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
          .mark_captured_semantic_tasks_observed_for_test(mark_bounds(), |ordinal| {
            calls += 1;
            assert_eq!(ordinal, calls);
            if calls == stop_at {
              if pressure {
                memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
              } else {
                cancellation.cancel();
              }
              if original_error {
                return Err(SemanticMutationObservationErrorV1::Resource { code: "mark_original_callback", message: "original cause" });
              }
            }
            Ok(())
          })
          .unwrap_err();
        assert_eq!(calls, stop_at);
        assert_eq!(
          error.code(),
          if original_error {
            "mark_original_callback"
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
fn native_semantic_task_mark_callbacks_can_publish_without_changing_capture() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("mark-concurrent", None, [1; 16], algorithm, 0);
  seed_captured_graph(&publisher);
  flush_mark_fixture(&publisher);
  let task_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
  let key = first_authority_file_path_hash(&task_path, algorithm);
  let old_locator = publisher.locator(&key).unwrap().unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mark = capture
    .mark_captured_semantic_tasks_observed_for_test(mark_bounds(), |ordinal| {
      if ordinal == 1 {
        let mut replacement =
          publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes;
        let sequence = u64::from_le_bytes(replacement[16..24].try_into().unwrap()) + 1;
        replacement[16..24].copy_from_slice(&sequence.to_le_bytes());
        crc(&mut replacement);
        seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &replacement)]);
        flush_mark_fixture(&publisher);
      }
      Ok(())
    })
    .unwrap();
  let current_locator = publisher.locator(&key).unwrap().unwrap();
  assert_ne!(current_locator.offset, old_locator.offset);
  let before = fs::read(&path).unwrap();
  assert_eq!(mark.summary().marked_slots, 20);
  assert!(mark.is_captured_locator_marked(&old_locator).unwrap());
  assert!(!mark.is_captured_locator_marked(&current_locator).unwrap());
  drop(mark);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
