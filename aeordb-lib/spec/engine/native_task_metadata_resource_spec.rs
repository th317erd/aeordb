//! Native metadata quota, lifetime and operational refusal coverage.
use super::*;

#[test]
fn native_task_metadata_inventory_uses_exact_cumulative_header_and_dependency_quota() {
  with_fixture(|publisher, memory, path, _| {
    publish_payload(publisher);
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(memory, &cancellation).unwrap();
    let before = fs::read(path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let run = |limit| {
      let capture = protection
        .capture_semantic_mutation_inventory(
          NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: limit, maximum_entity_bytes: 64 << 10, ..bounds() },
          memory,
          &cancellation,
        )
        .unwrap();
      capture.visit_metadata(|_| Ok(true))
    };
    let mut low = 1;
    let mut high = 64 << 10;
    assert_eq!(run(low).unwrap_err().code(), "semantic_task_inventory_read_bound");
    assert_eq!(run(high).unwrap().tasks, 1);
    while low < high {
      let middle = low + (high - low) / 2;
      match run(middle) {
        Ok(summary) => {
          assert!(summary.complete);
          high = middle;
        }
        Err(error) => {
          assert_eq!(error.code(), "semantic_task_inventory_read_bound");
          low = middle + 1;
        }
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(run(low - 1).unwrap_err().code(), "semantic_task_inventory_read_bound");
    assert_eq!(run(low).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
    let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
    let payload = vec![0x45; 128 << 10];
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
    // Independent frozen framing: a second ordinary chunk adds exactly 77+2H
    // physical bytes, not its 128KiB payload and not a reset of the first budget.
    let additional = (77 + 2 * algorithm.hash_length()) as u64;
    assert_eq!(run(low + additional - 1).unwrap_err().code(), "semantic_task_inventory_read_bound");
    assert_eq!(run(low + additional).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
    let after_publication = fs::read(path).unwrap();
    assert_ne!(after_publication, before);
    assert_eq!(run(low + additional).unwrap().tasks, 1);
    assert_eq!(fs::read(path).unwrap(), after_publication);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  });
}

#[test]
fn native_task_metadata_inventory_preserves_history_nested_scratch_and_callback_completion() {
  with_fixture(|publisher, memory, path, algorithm| {
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut outer_charge = 0;
    let mut inner_charge = 0;
    let old_header = publisher.observe().unwrap();
    let result = capture
      .visit_metadata(|observation| {
        assert_eq!(observation.task_selection().unwrap().control_sequence, 1);
        assert_eq!(observation.header(), &old_header);
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        outer_charge = memory.snapshot().unwrap().reserved_bytes;
        capture.visit_metadata(|_| {
          inner_charge = memory.snapshot().unwrap().reserved_bytes;
          Ok(true)
        })?;
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, outer_charge);
        seed(publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &released_task(algorithm, 2, 2))]);
        publish_ordinary_record(publisher, "/from-metadata-callback.txt", 0);
        Ok(true)
      })
      .unwrap();
    assert_eq!(result, SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
    assert!(outer_charge > retained);
    assert!(inner_charge >= outer_charge + bounds().maximum_entity_bytes as u64);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    let before_reads = fs::read(path).unwrap();
    assert_eq!(
      capture
        .visit_metadata(|observation| {
          assert_eq!(observation.task_selection().unwrap().control_sequence, 1);
          Ok(false)
        })
        .unwrap(),
      SemanticMutationInventorySummaryV1 { tasks: 1, complete: false }
    );
    let fresh = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
    assert!(
      fresh
        .visit_metadata(|observation| {
          assert_eq!(observation.task_selection().unwrap().control_sequence, 2);
          Ok(true)
        })
        .unwrap()
        .complete
    );
    let error = capture
      .visit_metadata(|_| {
        cancellation.cancel();
        Err(SemanticMutationObservationErrorV1::Invalid { code: "metadata_callback_original", message: "original callback error" })
      })
      .unwrap_err();
    assert_eq!(error.code(), "metadata_callback_original");
    let mut callbacks = 0;
    assert!(capture
      .visit_metadata(|_| {
        callbacks += 1;
        Ok(true)
      })
      .is_err());
    assert_eq!(callbacks, 0);
    assert_eq!(fs::read(path).unwrap(), before_reads);
  });
}

#[test]
fn native_task_metadata_inventory_allocation_pressure_and_eof_refuse_without_losing_tasks() {
  use crate::engine::memory_coordinator::HostMemorySample;
  with_fixture(|publisher, memory, path, algorithm| {
    let (task, checkpoint) = largest_checkpoint_pair(algorithm);
    populate(publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(path).unwrap();
    let (result, allocations) = measure(checkpoint.len(), || capture.visit_metadata(|_| Ok(true)));
    assert!(allocations.injected_failure, "{allocations:?}");
    assert_eq!(result.unwrap_err().code(), "first_authority_system_file_allocation");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(capture.visit_metadata(|_| Ok(true)).unwrap().tasks, 1);
    memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
    assert!(capture.visit_metadata(|_| Ok(true)).is_err());
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(capture.visit_metadata(|_| Ok(true)).unwrap().tasks, 1);
    assert_eq!(fs::read(path).unwrap(), before);
    let key = publish_payload(publisher);
    let locator = publisher.locator(&key).unwrap().unwrap();
    let complete = fs::read(path).unwrap();
    let fresh = protection.capture_semantic_mutation_inventory(bounds(), memory, &cancellation).unwrap();
    publisher.file.set_len(locator.offset + u64::from(locator.total_length) - 1).unwrap();
    let truncated = fs::read(path).unwrap();
    assert_eq!(fresh.visit_metadata(|_| Ok(true)).unwrap_err().code(), "first_authority_metadata_extent");
    assert_eq!(fs::read(path).unwrap(), truncated);
    write_file_at_native(&publisher.file, 0, &complete).unwrap();
    assert_eq!(fresh.visit_metadata(|_| Ok(true)).unwrap().tasks, 1);
    assert_eq!(fs::read(path).unwrap(), complete);
  });
}
