use super::*;

#[test]
fn native_captured_namespace_authority_limits_and_invalid_roots_do_not_leak_or_fall_back() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("captured-authority-bounds", None, [1; 16], algorithm, 0);
  let root = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap().namespace_root.root_hash;
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  for wrong in [vec![], vec![0; 32], vec![1; 64]] {
    let error = capture.with_namespace_authority_for_test(&wrong, 8 << 20, |_| panic!("invalid root cannot complete")).unwrap_err();
    assert_eq!(error.code(), "captured_authority_root_hash");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  for (limit, expected) in [(0, "captured_namespace_read_bound"), (1, "semantic_task_inventory_read_bound")] {
    let error = capture.with_namespace_authority_for_test(&root, limit, |_| panic!("refused read cannot complete")).unwrap_err();
    assert_eq!(error.code(), expected);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  capture.with_namespace_authority_for_test(&root, 8 << 20, |authority| assert!(authority.is_some())).unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_captured_namespace_authority_missing_or_corrupt_dependencies_are_not_absent_roots() {
  for kind in ["tree", "state", "admission"] {
    for missing in [false, true] {
      let algorithm = HashAlgorithm::Blake3_256;
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("captured-authority-dependency", None, [1; 16], algorithm, 0);
      let request = request_for_database_and_algorithm([1; 16], algorithm);
      let root = publisher.publish(&request).unwrap().namespace_root.root_hash;
      let key = match kind {
        "tree" => request.namespace_tree.root_hash,
        "state" => {
          first_authority_file_path_hash(&semantic_object_path(algorithm, 1, &request.semantic_state.object_id).unwrap(), algorithm)
        }
        "admission" => first_authority_file_path_hash(
          &system_control_path(SystemControlKindV1::RootAdmissionCommit, &root, SystemControlSlotV1::Immutable).unwrap(),
          algorithm,
        ),
        _ => unreachable!(),
      };
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
      let error = capture.with_namespace_authority_for_test(&root, 8 << 20, |_| panic!("broken dependency cannot complete")).unwrap_err();
      assert_eq!(error.code(), if missing { "captured_authority_closure" } else { "integrity_hash_mismatch" }, "{kind}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_captured_namespace_authority_preserves_actual_read_allocation_failure_and_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("captured-authority-allocation", None, [1; 16], algorithm, 0);
  let root = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap().namespace_root.root_hash;
  let length = publisher.locator(&root).unwrap().unwrap().total_length as usize;
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  let (result, allocations) =
    measure(length, || capture.with_namespace_authority_for_test(&root, 8 << 20, |_| panic!("allocation failure cannot complete")));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().code(), "first_authority_readback_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  capture.with_namespace_authority_for_test(&root, 8 << 20, |authority| assert!(authority.is_some())).unwrap();
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_captured_namespace_authority_harness_checks_entry_and_final_cancellation_and_pressure() {
  for pressure in [false, true] {
    for late in [false, true] {
      let algorithm = HashAlgorithm::Blake3_256;
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("captured-authority-interruption", None, [1; 16], algorithm, 0);
      let root = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap().namespace_root.root_hash;
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let before = fs::read(&path).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
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
      let result = capture.with_namespace_authority_for_test(&root, 8 << 20, |authority| {
        calls += 1;
        assert!(late && authority.is_some());
        interrupt();
      });
      assert_eq!(calls, usize::from(late));
      assert_eq!(
        result.unwrap_err().code(),
        if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" }
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
      memory.update_host_sample(Default::default()).unwrap();
    }
  }
}

#[test]
fn native_captured_namespace_authority_rejects_later_admission_sequence_and_foreign_database() {
  for future in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("captured-authority-admission", None, [1; 16], algorithm, 0);
    let first = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let root = first.namespace_root.root_hash;
    let mut admission = decode_root_admission_commit(&first.admission_control, algorithm).unwrap();
    if future {
      admission.selected_header_slot_sequence = u64::MAX;
    } else {
      admission.database_id = [0x32; 16];
    }
    let bytes = encode_root_admission_commit_control(&admission, algorithm).unwrap();
    seed(&publisher, &[(SystemControlKindV1::RootAdmissionCommit, &root, SystemControlSlotV1::Immutable, &bytes)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let error = capture.with_namespace_authority_for_test(&root, 8 << 20, |_| panic!("invalid admission cannot complete")).unwrap_err();
    assert_eq!(error.code(), if future { "captured_authority_admission_sequence" } else { "captured_authority_closure" });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
