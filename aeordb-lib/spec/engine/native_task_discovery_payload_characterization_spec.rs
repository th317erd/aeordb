//! Following discovery entry: characterize the existing deep scan, not a fix.
use super::*;

#[test]
fn native_task_discovery_characterizes_ordinary_payload_read_budget() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-discovery-payload-baseline", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
    let payload = vec![0x6b; 256 << 10];
    let key = digest_parts(algorithm, &[b"chunk:", &payload]);
    let header = publisher.observe().unwrap().selected.header;
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
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    {
      let limited = NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: 64 << 10, ..bounds() };
      let capture = protection.capture_semantic_mutation_inventory(limited, &memory, &cancellation).unwrap();
      // This asserts the old deep inspection contract; it must continue to
      // refuse its actual payload budget, never summarize a partial scan as complete.
      assert_eq!(capture.visit(|_| Ok(true)).unwrap_err().code(), "semantic_task_inventory_read_bound");
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    {
      let capture = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
      let mut tasks = Vec::new();
      let summary = capture
        .visit(|observation| {
          tasks.push(observation.task().unwrap().unwrap().task_id.to_vec());
          Ok(true)
        })
        .unwrap();
      assert_eq!(tasks, vec![vec![2; 16]]);
      assert_eq!(summary, SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
