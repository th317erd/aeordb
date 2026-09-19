//! Unregistered TDD draft for the next guarded source-node persistence boundary.
use super::*;

#[path = "native_source_control_staging_boundary_spec.rs"]
mod boundary;
#[path = "native_source_control_staging_collision_spec.rs"]
mod collision;

fn source_node_fixture(algorithm: HashAlgorithm, internal: bool) -> Vec<u8> {
  let profile = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("the independent fixtures here exercise both content widths"),
  };
  let name = if internal { "semantic-source-node-internal" } else { "semantic-source-node" };
  fs::read(format!("{}/spec/fixtures/v4/system-control-v1/control-{profile}-{name}-valid.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn declare_source_capabilities(publisher: &V4FirstAuthorityPublisher) {
  let mut header = publisher.observe().unwrap().selected.header;
  header.required_reader_capabilities[3] |= 0b1010;
  header.required_writer_capabilities[3] |= 0b1010;
  write_redundant_header(publisher, &header);
}

fn stage_source_nodes(
  capture: &NativeSemanticMutationInventoryV1<'_>,
  nodes: &[&[u8]],
  timestamp: u64,
) -> Result<ImmutableSystemControlBatchPublicationReceiptV1, NativeSemanticSourceControlPublicationErrorV1> {
  capture.stage_semantic_source_nodes(NativeSemanticSourceNodeStagingRequestV1 {
    encoded_nodes: nodes,
    publication_timestamp_ms: timestamp,
    maximum_workspace_bytes: 16 << 20,
  })
}

#[test]
fn native_source_control_staging_persists_exact_independent_nodes_and_reopens_without_task_selection() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-publication", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let before = publisher.observe().unwrap().selected.header;
    let left = source_node_fixture(algorithm, false);
    let right = source_node_fixture(algorithm, true);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let receipt = stage_source_nodes(&captured, &[&left, &right], before.updated_at_ms + 1).unwrap();
      assert!(!receipt.idempotent);
      assert_eq!(receipt.controls.len(), 2);
      assert!(receipt.controls.iter().all(|control| control.kind == SystemControlKindV1::SemanticSourceNode));
      assert_eq!(receipt.observation.selected.header.head_hash, before.head_hash);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert!(publisher.root_state.try_lock().is_ok());
      assert!(publisher.kv.try_lock().is_ok());
      let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let inventory = fresh.visit(|_| panic!("unselected catalog nodes must not create a task")).unwrap();
      assert!(inventory.complete);
      assert_eq!(inventory.tasks, 0);
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let bytes_before_read = fs::read(&path).unwrap();
    for expected in [&left, &right] {
      let decoded = decode_system_control(expected, algorithm).unwrap();
      let actual =
        reopened.load_immutable_system_control(SystemControlKindV1::SemanticSourceNode, &[1; 16], &decoded.identity).unwrap().unwrap();
      assert_eq!(&actual.bytes, expected);
      assert_eq!(actual.control_sequence, 1);
    }
    assert_eq!(reopened.observe().unwrap().selected.header.head_hash, before.head_hash);
    assert_eq!(fs::read(&path).unwrap(), bytes_before_read);
  }
}

#[test]
fn native_source_control_staging_deduplicates_equal_pairs_and_reuses_old_wrappers_at_a_new_time() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-reuse", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let node = source_node_fixture(algorithm, false);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    let first = stage_source_nodes(&captured, &[&node, &node], timestamp).unwrap();
    assert_eq!(first.controls.len(), 1);
    assert!(!first.idempotent);
    let before = fs::read(&path).unwrap();
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let repeated = stage_source_nodes(&fresh, &[&node], timestamp + 100).unwrap();
    assert!(repeated.idempotent);
    assert_eq!(repeated.controls.len(), 1);
    assert!(repeated.controls[0].idempotent);
    assert_eq!(repeated.controls[0].write_sequence, first.controls[0].write_sequence);
    assert_eq!(repeated.observation.selected.header, first.observation.selected.header);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_source_control_staging_mixed_existing_and_new_nodes_preserves_original_identity_and_bytes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, _path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-node-mixed", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    declare_source_capabilities(&publisher);
    let left = source_node_fixture(algorithm, false);
    let right = source_node_fixture(algorithm, true);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    let first = stage_source_nodes(&captured, &[&left], timestamp).unwrap();
    let identity = decode_system_control(&left, algorithm).unwrap().identity;
    let path_key = first_authority_file_path_hash(
      &system_control_path(SystemControlKindV1::SemanticSourceNode, &identity, SystemControlSlotV1::Immutable).unwrap(),
      algorithm,
    );
    let original_locator = publisher.lock_kv().unwrap().get(&path_key).unwrap().unwrap();
    let fresh = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let mixed = stage_source_nodes(&fresh, &[&left, &right], timestamp + 100).unwrap();
    assert_eq!(mixed.controls.len(), 2);
    assert!(!mixed.idempotent);
    assert!(mixed.controls[0].idempotent);
    assert!(!mixed.controls[1].idempotent);
    assert_eq!(mixed.controls[0].write_sequence, first.controls[0].write_sequence);
    let after_locator = publisher.lock_kv().unwrap().get(&path_key).unwrap().unwrap();
    assert_eq!(after_locator.offset, original_locator.offset);
    assert_eq!(after_locator.total_length, original_locator.total_length);
    assert_eq!(
      publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceNode, &[1; 16], &identity).unwrap().unwrap().bytes,
      left
    );
  }
}
