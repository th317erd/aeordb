//! Unselected checkpoint graph boundaries and independently enumerated edges.
use super::*;

pub(super) fn remove_fixture_task_selection(publisher: &V4FirstAuthorityPublisher, expected: &mut PhysicalSet) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  for (kind, identity) in
    [(SystemControlKindV1::SemanticMutationTask, [2; 16].as_slice()), (SystemControlKindV1::SemanticMutationGeneration, [].as_slice())]
  {
    let selected = publisher.load_mutable_system_control(kind, &[1; 16], identity).unwrap().unwrap();
    let key = first_authority_file_path_hash(&system_control_path(kind, identity, SystemControlSlotV1::A).unwrap(), algorithm);
    assert!(expected.remove(&key).is_some());
    assert!(expected.remove(&digest_parts(algorithm, &[b"system::", &selected.bytes])).is_some());
  }
  // Finish both authority reads before changing the fixture KV entry count.
  let task_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
  let task_key = first_authority_file_path_hash(&task_path, algorithm);
  assert!(publisher.lock_kv().unwrap().mark_deleted(&task_key).unwrap());
  // Existing fixture flush path aligns captured KV/header without creating a task.
  seed_files(publisher, &[]);
}

#[test]
fn native_unselected_checkpoint_graph_retains_exact_independent_edges_for_all_hashes_after_reopen() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("unselected-checkpoint-graph", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    remove_fixture_task_selection(&publisher, &mut expected);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    assert_eq!(capture.visit(|_| panic!("no task selection survives fixture setup")).unwrap().tasks, 0);
    let absent = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap();
    assert_eq!(absent.disposition, SemanticMutationObservationDispositionV1::Absent);
    let mut actual = PhysicalSet::new();
    let summary = capture
      .visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 1, graph_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(summary.checkpoint_sequence, 1);
    assert_eq!(summary.source_paths, 2);
    assert_eq!(summary.opaque_chunk_references, 1);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_unselected_checkpoint_graph_refuses_a_late_missing_ordinary_chunk() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("unselected-checkpoint-missing-leaf", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    remove_fixture_task_selection(&publisher, &mut expected);
    let key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
    assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
    seed_files(&publisher, &[]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut seen = PhysicalSet::new();
    let error = capture
      .visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 1, graph_bounds(), |entry| {
        seen.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap_err();
    assert_eq!(error.code(), "semantic_source_chunk_missing");
    assert!(!seen.is_empty(), "late failure must invalidate prior provisional visits");
    assert!(!seen.contains_key(&key));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_unselected_checkpoint_graph_keeps_ordinary_payloads_opaque() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("unselected-checkpoint-opaque-leaf", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    remove_fixture_task_selection(&publisher, &mut expected);
    let key = digest_parts(algorithm, &[b"chunk:", b"ordinary staged file"]);
    corrupt_last_entity_byte(&publisher, &key);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut actual = PhysicalSet::new();
    let summary = capture
      .visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 1, graph_bounds(), |entry| {
        actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
        Ok(())
      })
      .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(summary.opaque_chunk_references, 1);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_unselected_checkpoint_graph_refuses_companion_binding_mismatch() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("unselected-checkpoint-binding", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    remove_fixture_task_selection(&publisher, &mut expected);
    let identity = checkpoint_identity();
    let mut companion =
      publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &identity).unwrap().unwrap().bytes;
    // Literal ASCM field offset: common envelope, fixed body and five H-wide IDs.
    companion[32 + 112 + 5 * algorithm.hash_length()] ^= 1;
    crc(&mut companion);
    seed(&publisher, &[(SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let error = capture.visit_captured_semantic_checkpoint_metadata_entries(&[2; 16], 1, graph_bounds(), |_| Ok(())).unwrap_err();
    assert_eq!(error.code(), "semantic_capture_checkpoint_binding");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
