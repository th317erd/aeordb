//! Native target: metadata discovery is not payload verification.
#[path = "native_task_metadata_boundary_spec.rs"]
mod boundary;
use super::*;

fn publish_payload(publisher: &V4FirstAuthorityPublisher) -> Vec<u8> {
  let header = publisher.observe().unwrap().selected.header;
  let payload = vec![0x6b; 256 << 10];
  let key = digest_parts(header.hash_algorithm, &[b"chunk:", &payload]);
  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &header.database_id,
      entities: &[ImmutableEntityWriteV1 {
        entity_version: 0,
        entry_type: EntryTypeV4::Chunk,
        flags: 0,
        key: &key,
        stored_value: &payload,
      }],
      publication_timestamp_ms: header.updated_at_ms + 1,
    })
    .unwrap();
  key
}

#[test]
fn native_task_metadata_inventory_discovers_exact_tasks_without_ordinary_payload_budget() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-metadata-budget", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let limited = NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: 64 << 10, ..bounds() };
    {
      let baseline = protection.capture_semantic_mutation_inventory(limited, &memory, &cancellation).unwrap();
      assert_eq!(baseline.visit(|_| Ok(true)).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
    }
    publish_payload(&publisher);
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    {
      let capture = protection.capture_semantic_mutation_inventory(limited, &memory, &cancellation).unwrap();
      assert_eq!(capture.visit(|_| Ok(true)).unwrap_err().code(), "semantic_task_inventory_read_bound");
      let mut tasks = Vec::new();
      let summary = capture
        .visit_metadata(|observation| {
          tasks.push(observation.task().unwrap().unwrap().task_id.to_vec());
          Ok(true)
        })
        .expect("metadata discovery must not spend the ordinary payload's byte budget");
      assert_eq!(tasks, vec![vec![2; 16]]);
      assert_eq!(summary, SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_metadata_inventory_keeps_opaque_payload_separate_from_deep_integrity() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-metadata-opaque", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
    let key = publish_payload(&publisher);
    corrupt_last_entity_byte(&publisher, &key);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    {
      let capture = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
      let summary = capture.visit_metadata(|_| Ok(true)).expect("unrelated content damage is not a missing task");
      assert_eq!(summary, SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
      assert_eq!(capture.visit(|_| Ok(true)).unwrap_err().code(), "integrity_hash_mismatch");
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
