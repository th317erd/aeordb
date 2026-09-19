//! Exact reuse and competing source-node staging boundaries.
use super::*;

#[test]
fn native_source_control_staging_conflicting_existing_body_refuses_before_staging_other_node() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-existing-collision", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let first = source_node_fixture(algorithm, false);
    let second = source_node_fixture(algorithm, true);
    let identity = decode_system_control(&second, algorithm).unwrap().identity;
    let control_path = system_control_path(SystemControlKindV1::SemanticSourceNode, &identity, SystemControlSlotV1::Immutable).unwrap();
    let mut different = second.clone();
    different[32] ^= 0x20;
    crc(&mut different);
    // A canonical physical wrapper under the original path must not justify
    // trusting different bytes, even when its own framing/content checks pass.
    seed_files(&publisher, &[(control_path, SYSTEM_CONTROL_CONTENT_TYPE, &different)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let error =
      stage_source_nodes(&capture, &[&first, &second], publisher.observe().unwrap().selected.header.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "semantic_source_node_collision");
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
    let first_identity = decode_system_control(&first, algorithm).unwrap().identity;
    assert!(publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceNode, &[1; 16], &first_identity).unwrap().is_none());
  }
}

#[test]
fn native_source_control_staging_competing_captures_publish_once_and_reuse_exactly() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-node-competing-captures", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  declare_source_capabilities(&publisher);
  let node = source_node_fixture(algorithm, false);
  let before = publisher.observe().unwrap().selected.header;
  let (ready_sender, ready_receiver) = mpsc::channel();
  let (release_first, first_receiver) = mpsc::channel();
  let (release_second, second_receiver) = mpsc::channel();
  let results = std::thread::scope(|scope| {
    let run = |release: mpsc::Receiver<()>, offset| {
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      capture.stage_semantic_source_nodes_observed(
        NativeSemanticSourceNodeStagingRequestV1 {
          encoded_nodes: &[&node],
          publication_timestamp_ms: before.updated_at_ms + offset,
          maximum_workspace_bytes: 16 << 20,
        },
        || {
          ready_sender.send(()).unwrap();
          release.recv_timeout(Duration::from_secs(2)).unwrap();
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
    };
    let first = scope.spawn(move || run(first_receiver, 1));
    let second = scope.spawn(move || run(second_receiver, 2));
    let ready = ready_receiver.recv_timeout(Duration::from_secs(2)).is_ok() && ready_receiver.recv_timeout(Duration::from_secs(2)).is_ok();
    let released_first = release_first.send(());
    let released_second = release_second.send(());
    let results = [first.join().unwrap().unwrap(), second.join().unwrap().unwrap()];
    assert!(ready && released_first.is_ok() && released_second.is_ok());
    results
  });
  assert_eq!(results.iter().filter(|receipt| receipt.idempotent).count(), 1);
  assert_eq!(results[0].controls[0].path_key, results[1].controls[0].path_key);
  assert_eq!(results[0].controls[0].write_sequence, results[1].controls[0].write_sequence);
  let after = publisher.observe().unwrap().selected.header;
  assert_eq!(after.head_hash, before.head_hash);
  assert_eq!(after.entry_count, before.entry_count + 2);
  assert_eq!(after.write_sequence_high_water, before.write_sequence_high_water + 2);
}
