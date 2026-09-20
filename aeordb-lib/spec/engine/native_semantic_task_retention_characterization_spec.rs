//! Existing separate graph calls do not enforce a composed operation ceiling.
use super::*;
use crate::engine::v4::semantic_source_capture::decode_semantic_source_capture_v1;

pub(super) fn seed_second_task(publisher: &V4FirstAuthorityPublisher, expected: &mut PhysicalSet) {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let old_identity = checkpoint_identity();
  let loaded =
    publisher.load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &old_identity).unwrap().unwrap();
  let mut checkpoint = loaded.bytes;
  // Independent envelope/body offsets from the frozen ASMC/ASMT contracts.
  checkpoint[48..64].fill(3);
  crc(&mut checkpoint);
  let checkpoint_hash = digest_parts(algorithm, &[&checkpoint]);
  let old_task = publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap();
  let mut task = old_task.bytes;
  task[48..64].fill(3);
  task[144..144 + algorithm.hash_length()].copy_from_slice(&checkpoint_hash);
  crc(&mut task);
  let old_companion =
    publisher.load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &old_identity).unwrap().unwrap();
  let companion_view = decode_semantic_source_capture_v1(&old_companion.bytes, algorithm).unwrap();
  let companion = encode_semantic_source_capture_v1(
    &SemanticSourceCaptureV1 { task_id: &[3; 16], checkpoint_payload_hash: &checkpoint_hash, ..companion_view },
    algorithm,
  )
  .unwrap();
  let mut identity = old_identity;
  identity[..16].fill(3);
  let controls: Vec<(SystemControlKindV1, &[u8], SystemControlSlotV1, &[u8])> = vec![
    (SystemControlKindV1::SemanticMutationTask, &[3; 16], SystemControlSlotV1::B, &task),
    (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
    (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion),
  ];
  seed(publisher, &controls);
  for (kind, identity, slot, bytes) in controls {
    include_control(publisher, expected, kind, identity, slot, bytes);
  }
}

#[test]
fn native_semantic_task_retention_characterizes_separate_graph_budget_reset() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-retention-characterization", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    seed_second_task(&publisher, &mut expected);
    assert_eq!(expected.len(), 26);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection
      .capture_semantic_mutation_inventory(
        NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 256 << 10, ..capture_bounds() },
        &memory,
        &cancellation,
      )
      .unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let first = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], graph_bounds(), |_| Ok(())).unwrap();
    let second = capture.visit_captured_semantic_task_metadata_entries(&[3; 16], graph_bounds(), |_| Ok(())).unwrap();
    let single_limit = first.read_bytes.max(second.read_bytes);
    let per_task = NativeSemanticTaskGraphBoundsV1 { maximum_read_bytes: single_limit, ..graph_bounds() };
    let mut actual = PhysicalSet::new();
    let mut combined_bytes = 0;
    let discovery = capture
      .visit_metadata(|observation| {
        let task_id: &[u8; 16] = observation.task().unwrap().unwrap().task_id.try_into().unwrap();
        let graph = capture
          .visit_captured_semantic_task_metadata_entries(task_id, per_task, |entry| {
            actual.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
            Ok(())
          })
          .unwrap();
        combined_bytes += graph.read_bytes;
        Ok(true)
      })
      .unwrap();
    assert_eq!(discovery.tasks, 2);
    assert!(discovery.complete);
    assert_eq!(actual, expected);
    assert_eq!(combined_bytes, first.read_bytes + second.read_bytes);
    assert!(combined_bytes > single_limit, "independent calls cannot enforce the combined ceiling");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
