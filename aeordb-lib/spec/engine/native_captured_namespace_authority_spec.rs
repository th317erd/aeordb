//! Metadata authority fixtures, not a recursive namespace/task closure proof.
use super::*;
#[path = "native_captured_namespace_authority_boundary_spec.rs"]
mod boundary;

#[test]
fn native_captured_namespace_authority_keeps_historical_root_after_head_advance_and_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("captured-historical-authority", None, [1; 16], algorithm, 0);
    let request = request_for_database_and_algorithm([1; 16], algorithm);
    let first = publisher.publish(&request).unwrap();
    let mut next = successor_request(&publisher, 0x73, "later-file");
    next.semantic_state = request_for_database_and_algorithm([1; 16], algorithm).semantic_state;
    let second = publisher.publish_successor_authority(&next).unwrap();
    assert_ne!(first.namespace_root.root_hash, second.namespace_root.root_hash);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let selected = publisher.observe().unwrap().selected;
    let expected = publisher
      .load_namespace_authority_at_captured_header(&selected, &first.namespace_root.root_hash, &CancellationToken::new())
      .unwrap()
      .unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mut calls = 0;
    capture
      .with_namespace_authority_for_test(&first.namespace_root.root_hash, 8 << 20, |authority| {
        calls += 1;
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        let authority = authority.unwrap();
        assert_eq!(authority, &expected);
        assert_eq!(authority.root.root_hash, first.namespace_root.root_hash);
        assert_ne!(authority.root.root_hash, selected.header.head_hash);
        assert_eq!(authority.namespace_tree.root_hash, request.namespace_tree.root_hash);
        assert_eq!(authority.semantic_state.object_id, request.semantic_state.object_id);
        assert_eq!(authority.admission.publication_sequence, first.publication_sequence);
        assert!(memory.snapshot().unwrap().reserved_bytes > baseline);
      })
      .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_captured_namespace_authority_does_not_find_future_root_through_current_locators() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("captured-authority-no-fallback", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let second = publisher.publish_successor_authority(&successor_request(&publisher, 0x74, "future-file")).unwrap();
  let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mut calls = 0;
  for (capture, present) in [(&old, false), (&fresh, true)] {
    capture
      .with_namespace_authority_for_test(&second.namespace_root.root_hash, 8 << 20, |authority| {
        calls += 1;
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        assert_eq!(authority.is_some(), present);
        if let Some(authority) = authority {
          assert_eq!(authority.root.root_hash, second.namespace_root.root_hash);
        }
      })
      .unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  assert_eq!(calls, 2);
  assert_eq!(fs::read(&path).unwrap(), before);
}
