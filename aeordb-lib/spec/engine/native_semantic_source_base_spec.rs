use super::*;
#[path = "native_semantic_source_base_boundary_spec.rs"]
mod boundary;
#[path = "native_captured_namespace_authority_spec.rs"]
mod captured_authority;

fn capture_bounds() -> NativeSemanticMutationInventoryBoundsV1 {
  NativeSemanticMutationInventoryBoundsV1 { maximum_work: 4096, maximum_entity_bytes: 4 << 20, maximum_read_bytes: 64 << 20 }
}

fn source_generation(algorithm: HashAlgorithm, sequence: u64) -> Vec<u8> {
  let mut bytes = frozen(algorithm, "generation");
  bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
  crc(&mut bytes);
  bytes
}

#[test]
fn native_semantic_source_base_keeps_captured_root_and_generation_together() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-base-capture", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let generation10 = source_generation(algorithm, 10);
    seed(&publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &generation10)]);
    let old_authority = publisher.load_selected_semantic_authority().unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut next = successor_request(&publisher, 0x71, "new-file");
    next.semantic_state = request_for_database_and_algorithm([1; 16], algorithm).semantic_state;
    publisher.publish_successor_authority(&next).unwrap();
    let generation11 = source_generation(algorithm, 11);
    seed(&publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::B, &generation11)]);
    let current_authority = publisher.load_selected_semantic_authority().unwrap();
    assert_ne!(old_authority.root_hash, current_authority.root_hash);
    let current = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    for (capture, authority, sequence, bytes) in
      [(&old, &old_authority, 10, &generation10), (&current, &current_authority, 11, &generation11)]
    {
      let binding = capture
        .read_source_base_for_test(&authority.root_hash, 8 << 20, || {
          assert!(publisher.root_state.try_lock().is_ok());
          assert!(publisher.kv.try_lock().is_ok());
        })
        .unwrap();
      assert_eq!(&binding.authority, authority);
      assert_eq!(binding.generation.control_sequence, sequence);
      assert_eq!(&binding.generation.bytes, bytes);
      assert_eq!(binding.generation.control_digest, digest_parts(algorithm, &[b"aeordb.mutable-system-control-cas.v1\0", bytes]));
      assert!(memory.snapshot().unwrap().reserved_bytes > retained);
      drop(binding);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(current);
    drop(old);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_semantic_source_base_missing_generation_is_not_zero_or_live_fallback() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-base-absent", None, [1; 16], algorithm, 0);
  let initial = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let root = initial.namespace_root.root_hash;
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  seed(&publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &source_generation(algorithm, 10))]);
  let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  assert_eq!(old.read_source_base_for_test(&root, 8 << 20, || {}).err().unwrap().code(), "semantic_source_base_generation_missing");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fresh.read_source_base_for_test(&root, 8 << 20, || {}).unwrap().generation.control_sequence, 10);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_source_base_wrong_root_and_read_budget_release_then_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-base-limits", None, [1; 16], algorithm, 0);
  let initial = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let root = initial.namespace_root.root_hash;
  seed(&publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &source_generation(algorithm, 10))]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for wrong in [vec![], vec![0; 32], vec![1; 32], vec![1; 64]] {
    assert_eq!(capture.read_source_base_for_test(&wrong, 8 << 20, || {}).err().unwrap().code(), "semantic_source_base_root_mismatch");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(capture.read_source_base_for_test(&root, 1, || {}).err().unwrap().code(), "semantic_task_inventory_read_bound");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(capture.read_source_base_for_test(&root, 8 << 20, || {}).unwrap().generation.control_sequence, 10);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}
