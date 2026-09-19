//! Guarded source-node staging refusal, resource, durability and reopen coverage.
use super::*;

#[test]
fn native_source_control_staging_preserves_generic_semantic_control_refusal() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-node-generic-refusal", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  declare_source_capabilities(&publisher);
  let node = source_node_fixture(algorithm, false);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  stage_source_nodes(&captured, &[&node], timestamp).unwrap();
  let decoded = decode_system_control(&node, algorithm).unwrap();
  let before = fs::read(&path).unwrap();
  let error = publisher
    .publish_immutable_system_controls(ImmutableSystemControlBatchPublicationRequestV1 {
      database_id: &[1; 16],
      publication_timestamp_ms: timestamp + 1,
      controls: &[ImmutableSystemControlWriteV1 {
        kind: SystemControlKindV1::SemanticSourceNode,
        identity: &decoded.identity,
        encoded_control: &node,
      }],
    })
    .unwrap_err();
  assert_eq!(error.code(), "semantic_task_writer_not_qualified");
  assert!(error.committed_receipt().is_none());
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_source_control_staging_preserves_actual_commit_receipts_and_reopen_recovery() {
  for case in 0..6 {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-faults", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let node = source_node_fixture(algorithm, false);
    let identity = decode_system_control(&node, algorithm).unwrap().identity;
    let key = first_authority_file_path_hash(
      &system_control_path(SystemControlKindV1::SemanticSourceNode, &identity, SystemControlSlotV1::Immutable).unwrap(),
      algorithm,
    );
    {
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = publisher.observe().unwrap();
      let timestamp = before.selected.header.updated_at_ms + 1;
      let mut visibility = FailingVisibilityObserver;
      let mut post_commit = FailingPostCommitObserver;
      let mut cancel_after = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };
      let mut dependency = FailingDependencyObserver {
        phase: match case {
          3 => DependencyFailurePhase::BeforeEntity,
          4 => DependencyFailurePhase::EntityWritten,
          _ => DependencyFailurePhase::EntityStaged,
        },
        entity_index: 0,
      };
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = match case {
        0 => &mut visibility,
        1 => &mut post_commit,
        2 => &mut cancel_after,
        _ => &mut dependency,
      };
      let result = captured.stage_semantic_source_nodes_observed(
        NativeSemanticSourceNodeStagingRequestV1 {
          encoded_nodes: &[&node],
          publication_timestamp_ms: timestamp,
          maximum_workspace_bytes: 16 << 20,
        },
        || {},
        observer,
      );
      if case == 2 {
        assert!(cancellation.is_cancelled());
        assert!(!result.unwrap().idempotent);
      } else {
        let error = result.unwrap_err();
        if case == 1 {
          assert_eq!(error.code(), "immutable_entity_committed_postcondition_failure");
          let receipt = error.committed_receipt().unwrap();
          assert_eq!(receipt.controls.len(), 1);
          assert_eq!(receipt.controls[0].path_key, key);
          assert!(publisher.locator(&key).unwrap().is_some());
          assert!(stage_source_nodes(&captured, &[&node], timestamp + 1).unwrap().idempotent);
        } else {
          assert_eq!(error.code(), "durability_failure");
          assert!(error.committed_receipt().is_none());
          assert_eq!(publisher.observe().unwrap(), before);
          assert!(publisher.locator(&key).unwrap().is_none());
          assert!(coordinator.hard_failure().unwrap().is_some());
          let retry = stage_source_nodes(&captured, &[&node], timestamp + 1).unwrap_err();
          assert_eq!(retry.code(), "durability_failure");
          assert!(retry.committed_receipt().is_none());
        }
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, before.selected.header.head_hash);
    }
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    if case != 1 && case != 2 {
      assert!(reopened.locator(&key).unwrap().is_none());
      assert!(!stage_source_nodes(&captured, &[&node], reopened.observe().unwrap().selected.header.updated_at_ms + 1).unwrap().idempotent);
    }
    assert_eq!(
      reopened.load_immutable_system_control(SystemControlKindV1::SemanticSourceNode, &[1; 16], &identity).unwrap().unwrap().bytes,
      node
    );
  }
}

#[test]
fn native_source_control_staging_actual_descriptor_allocation_failure_releases_admission_and_retries() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-node-allocation", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  declare_source_capabilities(&publisher);
  let left = source_node_fixture(algorithm, false);
  let right = source_node_fixture(algorithm, true);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  let allocation_bytes = 2 * std::mem::size_of::<crate::engine::v4::system_control::SystemControlV1<'_>>();
  let (result, allocations) = measure(allocation_bytes, || stage_source_nodes(&captured, &[&left, &right], timestamp));
  assert!(allocations.injected_failure, "{allocations:?}");
  let error = result.unwrap_err();
  assert_eq!(error.code(), "semantic_source_node_allocation");
  assert!(error.committed_receipt().is_none());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
  assert!(!stage_source_nodes(&captured, &[&left, &right], timestamp).unwrap().idempotent);
}

#[test]
fn native_source_control_staging_final_cancel_or_pressure_recheck_prevents_all_writes() {
  use crate::engine::memory_coordinator::HostMemorySample;
  for cancel in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-final-admission", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let node = source_node_fixture(algorithm, false);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    let result = captured.stage_semantic_source_nodes_observed(
      NativeSemanticSourceNodeStagingRequestV1 {
        encoded_nodes: &[&node],
        publication_timestamp_ms: timestamp,
        maximum_workspace_bytes: 16 << 20,
      },
      || {
        assert!(publisher.root_state.try_lock().is_ok());
        if cancel {
          cancellation.cancel();
        } else {
          memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
        }
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    );
    let error = result.unwrap_err();
    assert_eq!(error.code(), if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" });
    assert!(error.committed_receipt().is_none());
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
    if !cancel {
      assert!(!stage_source_nodes(&captured, &[&node], timestamp).unwrap().idempotent);
    }
  }
}

#[test]
fn native_source_control_staging_validates_the_entire_batch_before_any_file_change() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-prevalidation", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let node = source_node_fixture(algorithm, false);
    let generation = frozen(algorithm, "generation");
    let mut corrupt = source_node_fixture(algorithm, true);
    *corrupt.last_mut().unwrap() ^= 1;
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    for nodes in [vec![], vec![node.as_slice(); 3], vec![node.as_slice(), &generation], vec![node.as_slice(), &corrupt]] {
      let error = stage_source_nodes(&capture, &nodes, timestamp).unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(&path).unwrap(), before);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    }
    assert!(!stage_source_nodes(&capture, &[&node], timestamp).unwrap().idempotent);
  }
}

#[test]
fn native_source_control_staging_requires_both_reader_and_writer_capabilities_without_upgrading_header() {
  let algorithm = HashAlgorithm::Blake3_256;
  for (reader, writer) in [(0b0010, 0b1010), (0b1000, 0b1010), (0b1010, 0b0010), (0b1010, 0b1000)] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-capabilities", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let mut header = publisher.observe().unwrap().selected.header;
    header.required_reader_capabilities[3] = reader;
    header.required_writer_capabilities[3] = writer;
    write_redundant_header(&publisher, &header);
    let node = source_node_fixture(algorithm, false);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let error = stage_source_nodes(&captured, &[&node], header.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "semantic_source_node_capability");
    assert!(error.committed_receipt().is_none());
    assert_eq!(publisher.observe().unwrap().selected.header, header);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_source_control_staging_workspace_refusal_and_precancellation_are_byte_stable() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-node-workspace", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  declare_source_capabilities(&publisher);
  let node = source_node_fixture(algorithm, false);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  let error = captured
    .stage_semantic_source_nodes(NativeSemanticSourceNodeStagingRequestV1 {
      encoded_nodes: &[&node],
      publication_timestamp_ms: timestamp,
      maximum_workspace_bytes: 1,
    })
    .unwrap_err();
  assert_eq!(error.code(), "semantic_source_node_workspace");
  assert!(error.committed_receipt().is_none());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
  cancellation.cancel();
  let error = stage_source_nodes(&captured, &[&node], timestamp).unwrap_err();
  assert_eq!(error.code(), "semantic_task_observation_cancelled");
  assert!(error.committed_receipt().is_none());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
  let retry_cancel = CancellationToken::new();
  let retried = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &retry_cancel).unwrap();
  assert!(!stage_source_nodes(&retried, &[&node], timestamp).unwrap().idempotent);
}

#[test]
fn native_source_control_staging_never_admits_a_capture_from_an_older_physical_owner_or_fence() {
  let algorithm = HashAlgorithm::Blake3_256;
  for physical_change in [false, true] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-owner", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let node = source_node_fixture(algorithm, false);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mut current = publisher.observe().unwrap().selected.header;
    if physical_change {
      current.physical_instance_id = [0x72; 16];
    } else {
      current.writer_fence_epoch += 1;
    }
    write_redundant_header(&publisher, &current);
    let before = fs::read(&path).unwrap();
    let error = stage_source_nodes(&captured, &[&node], current.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "semantic_source_node_owner");
    assert!(error.committed_receipt().is_none());
    assert_eq!(publisher.observe().unwrap().selected.header, current);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
