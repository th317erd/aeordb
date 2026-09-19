use super::*;
use crate::engine::memory_coordinator::HostMemorySample;

#[test]
fn native_semantic_source_base_rejects_well_formed_admission_for_another_authority() {
  for case in ["database", "root", "kind", "after", "future-header"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-base-admission", None, [1; 16], algorithm, 0);
    let initial = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let root = initial.namespace_root.root_hash;
    let mut admission = decode_root_admission_commit(&initial.admission_control, algorithm).unwrap();
    match case {
      "database" => admission.database_id = [2; 16],
      "root" => admission.namespace_root = vec![0x51; algorithm.hash_length()],
      "kind" => admission.authority_kind = RootAuthorityKindV1::Snapshot,
      "after" => admission.authority_after = vec![0x52; algorithm.hash_length()],
      "future-header" => admission.selected_header_slot_sequence = u64::MAX,
      _ => unreachable!(),
    }
    let bytes = encode_root_admission_commit_control(&admission, algorithm).unwrap();
    seed(
      &publisher,
      &[
        (SystemControlKindV1::RootAdmissionCommit, &root, SystemControlSlotV1::Immutable, &bytes),
        (SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &source_generation(algorithm, 10)),
      ],
    );
    assert_eq!(publisher.load_selected_semantic_authority().unwrap_err().code(), "selected_semantic_authority_admission", "{case}");
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    assert_eq!(
      capture.read_source_base_for_test(&root, 8 << 20, || panic!("wrong admission must not complete")).err().unwrap().code(),
      "selected_semantic_authority_admission",
      "{case}"
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_source_base_checks_early_and_final_cancellation_and_pressure() {
  for pressure in [false, true] {
    for late in [false, true] {
      let algorithm = HashAlgorithm::Blake3_256;
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("source-base-lifecycle", None, [1; 16], algorithm, 0);
      let root = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap().namespace_root.root_hash;
      seed(
        &publisher,
        &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &source_generation(algorithm, 10))],
      );
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let before = fs::read(&path).unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let interrupt = || {
        if pressure {
          memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
        } else {
          cancellation.cancel();
        }
      };
      if !late {
        interrupt();
      }
      let mut completed = 0;
      let result = capture.read_source_base_for_test(&root, 8 << 20, || {
        completed += 1;
        assert!(late);
        interrupt();
      });
      assert_eq!(completed, usize::from(late));
      assert_eq!(
        result.err().unwrap().code(),
        if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" }
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      if pressure {
        drop(capture.read_source_base_for_test(&root, 8 << 20, || {}).unwrap());
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_source_base_preserves_missing_and_physical_dependency_errors() {
  for (kind, missing_code) in [
    ("root", "selected_semantic_authority_root_missing"),
    ("state", "selected_semantic_authority_state_missing"),
    ("admission", "first_authority_control_missing"),
  ] {
    for missing in [false, true] {
      let algorithm = HashAlgorithm::Blake3_256;
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("source-base-dependency", None, [1; 16], algorithm, 0);
      let request = request_for_database_and_algorithm([1; 16], algorithm);
      let root = publisher.publish(&request).unwrap().namespace_root.root_hash;
      seed(
        &publisher,
        &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &source_generation(algorithm, 10))],
      );
      let key = match kind {
        "root" => root.clone(),
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
      let expected = if missing { missing_code } else { "integrity_hash_mismatch" };
      assert_eq!(publisher.load_selected_semantic_authority().unwrap_err().code(), expected);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let before = fs::read(&path).unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let error = capture.read_source_base_for_test(&root, 8 << 20, || panic!("failed base must not complete")).err().unwrap();
      assert_eq!(error.code(), expected, "{kind}, missing={missing}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_source_base_generation_pair_selection_and_failures_stay_shared() {
  for case in
    ["a-only", "b-only", "newer-b", "equal", "torn-a", "torn-b", "both-torn", "wrong-database", "pair-identity", "physical-a", "physical-b"]
  {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-base-generation", None, [1; 16], algorithm, 0);
    let root = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap().namespace_root.root_hash;
    let mut a = source_generation(algorithm, 10);
    let mut b = source_generation(algorithm, if case == "newer-b" { 11 } else { 10 });
    if matches!(case, "torn-a" | "both-torn") {
      a[0] ^= 1;
    }
    if matches!(case, "torn-b" | "both-torn") {
      b[0] ^= 1;
    }
    if matches!(case, "wrong-database" | "pair-identity") {
      b[32..48].fill(2);
      crc(&mut b);
    }
    if case == "wrong-database" {
      a = b.clone();
    }
    let mut controls = Vec::new();
    if case != "b-only" {
      controls.push((SystemControlKindV1::SemanticMutationGeneration, [].as_slice(), SystemControlSlotV1::A, a.as_slice()));
    }
    if case != "a-only" {
      controls.push((SystemControlKindV1::SemanticMutationGeneration, [].as_slice(), SystemControlSlotV1::B, b.as_slice()));
    }
    seed(&publisher, &controls);
    if case.starts_with("physical-") {
      let slot = if case == "physical-a" { SystemControlSlotV1::A } else { SystemControlSlotV1::B };
      let name = system_control_path(SystemControlKindV1::SemanticMutationGeneration, &[], slot).unwrap();
      corrupt_last_entity_byte(&publisher, &first_authority_file_path_hash(&name, algorithm));
    }
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let result = capture.read_source_base_for_test(&root, 8 << 20, || {});
    let expected_error = match case {
      "both-torn" => Some("system_control_no_valid_slot"),
      "wrong-database" => Some("mutable_control_database_mismatch"),
      "pair-identity" => Some("system_control_pair_identity"),
      "physical-a" | "physical-b" => Some("integrity_hash_mismatch"),
      _ => None,
    };
    if let Some(code) = expected_error {
      assert_eq!(result.err().unwrap().code(), code, "{case}");
    } else {
      let binding = result.unwrap();
      assert_eq!(binding.generation.control_sequence, if case == "newer-b" { 11 } else { 10 }, "{case}");
      assert_eq!(
        binding.generation.selected_slot,
        if matches!(case, "b-only" | "newer-b" | "torn-a") { SystemControlSlotV1::B } else { SystemControlSlotV1::A },
        "{case}"
      );
      assert_eq!(binding.generation.redundancy_degraded, matches!(case, "a-only" | "b-only" | "torn-a" | "torn-b"));
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_source_base_root_allocation_failure_releases_and_retries() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-base-allocation", None, [1; 16], algorithm, 0);
    let root = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap().namespace_root.root_hash;
    seed(&publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &source_generation(algorithm, 10))]);
    let entity_length = publisher.locator(&root).unwrap().unwrap().total_length as usize;
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let (result, allocations) = allocation_probe::measure(entity_length, || capture.read_source_base_for_test(&root, 8 << 20, || {}));
    assert!(allocations.injected_failure, "{allocations:?}");
    assert_eq!(result.err().unwrap().code(), "first_authority_readback_allocation");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    drop(capture.read_source_base_for_test(&root, 8 << 20, || {}).unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
