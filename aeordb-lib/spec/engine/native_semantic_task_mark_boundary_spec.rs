//! Boundary cases for the capture-bound task contribution.
use super::*;
use crate::engine::memory_coordinator::HostMemorySample;

#[test]
fn native_semantic_task_mark_all_hashes_require_every_locator_component() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("mark-components", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    seed_second_task(&publisher, &mut expected);
    flush_mark_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    assert_eq!(mark.summary().retention.tasks, 2);
    assert_eq!(mark.summary().marked_slots, 26);
    assert!(mark.summary().slot_lookups > 26, "shared references are still charged");
    for (key, (flags, offset, length)) in expected {
      let locator = KVEntry { hash: key, type_flags: flags, offset, total_length: length };
      assert!(mark.is_captured_locator_marked(&locator).unwrap());
      for changed in [
        KVEntry { offset: offset + 1, ..locator.clone() },
        KVEntry { total_length: length + 1, ..locator.clone() },
        KVEntry { type_flags: flags ^ 0x10, ..locator.clone() },
        KVEntry { hash: vec![0xFF; algorithm.hash_length()], ..locator.clone() },
      ] {
        assert!(!mark.is_captured_locator_marked(&changed).unwrap());
      }
      let malformed = KVEntry { hash: vec![0; algorithm.hash_length() - 1], ..locator };
      assert!(mark.is_captured_locator_marked(&malformed).unwrap_err().to_string().contains("width"));
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained + mark.bitmap_bytes().len() as u64);
    drop(mark);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_mark_refuses_buffered_snapshots_even_without_tasks() {
  for tasks in [false, true] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("mark-buffered", None, [1; 16], HashAlgorithm::Blake3_256, 0);
    if tasks {
      seed_captured_graph(&publisher);
    } else {
      seed_files(&publisher, &[("/ordinary".into(), "application/octet-stream", b"ordinary")]);
    }
    assert!(publisher.lock_kv().unwrap().capture_settled_snapshot().unwrap().buffer_len() > 0);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    assert_eq!(capture.mark_captured_semantic_tasks(mark_bounds()).unwrap_err().code(), "semantic_task_mark_buffered");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(publisher.lock_kv().unwrap().capture_settled_snapshot().unwrap().buffer_len() > 0);
  }
}

#[test]
fn native_semantic_task_mark_empty_released_and_zero_bounds() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) = create_environment_for_algorithm_at_kv_stage("mark-empty", None, [1; 16], algorithm, 0);
  flush_mark_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let empty = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let mark = empty.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  assert_eq!(mark.summary().retention.tasks, 0);
  assert!(mark.summary().retention.complete);
  assert_eq!(mark.summary().marked_slots, 0);
  assert_eq!(mark.summary().slot_lookups, 0);
  assert_eq!(mark.summary().slot_page_bytes, 0);
  assert!(mark.bitmap_bytes().iter().all(|byte| *byte == 0));
  drop(mark);
  for (lookups, bytes) in [(0, 1), (1, 0)] {
    assert_eq!(
      empty
        .mark_captured_semantic_tasks(NativeSemanticTaskMarkBoundsV1 {
          maximum_slot_lookups: lookups,
          maximum_slot_page_bytes: bytes,
          ..mark_bounds()
        })
        .unwrap_err()
        .code(),
      "semantic_task_mark_bounds"
    );
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  let mut task = frozen(algorithm, "task");
  task[128..130].copy_from_slice(&9u16.to_le_bytes());
  task[130..132].copy_from_slice(&1u16.to_le_bytes());
  crc(&mut task);
  let generation = frozen(algorithm, "generation");
  populate(&publisher, &task, None, Some(&generation));
  flush_mark_fixture(&publisher);
  let released = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let mark = released.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  assert_eq!(mark.summary().retention.tasks, 1);
  assert_eq!(mark.summary().marked_slots, 4, "only task and generation FileRecords/chunks; no released checkpoint");
  assert_eq!(empty.mark_captured_semantic_tasks(mark_bounds()).unwrap().summary().marked_slots, 0);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_mark_keeps_old_incarnations_after_current_replacement() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("mark-history", None, [1; 16], algorithm, 0);
  seed_captured_graph(&publisher);
  flush_mark_fixture(&publisher);
  let task_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
  let key = first_authority_file_path_hash(&task_path, algorithm);
  let old_locator = publisher.locator(&key).unwrap().unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  let mut replacement =
    publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes;
  let sequence = u64::from_le_bytes(replacement[16..24].try_into().unwrap()) + 1;
  replacement[16..24].copy_from_slice(&sequence.to_le_bytes());
  crc(&mut replacement);
  seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &replacement)]);
  flush_mark_fixture(&publisher);
  let new_locator = publisher.locator(&key).unwrap().unwrap();
  assert_ne!(old_locator.offset, new_locator.offset);
  let current = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let new_mark = current.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  assert!(mark.is_captured_locator_marked(&old_locator).unwrap());
  assert!(!mark.is_captured_locator_marked(&new_locator).unwrap());
  assert!(new_mark.is_captured_locator_marked(&new_locator).unwrap());
  assert!(!new_mark.is_captured_locator_marked(&old_locator).unwrap());
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_mark_allocation_failure_releases_bitmap_and_retries() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("mark-allocation", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (_, base, _) = seed_captured_graph(&publisher);
  flush_mark_fixture(&publisher);
  let bitmap_bytes = (publisher.lock_kv().unwrap().bucket_count() * MAX_ENTRIES_PER_PAGE).div_ceil(8);
  let read_bytes = publisher.locator(&base).unwrap().unwrap().total_length as usize;
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for (length, code) in [(bitmap_bytes, "mark_bitmap_allocation"), (read_bytes, "first_authority_readback_allocation")] {
    let (result, allocations) = measure(length, || capture.mark_captured_semantic_tasks(mark_bounds()));
    assert!(allocations.injected_failure, "{allocations:?}");
    assert_eq!(result.unwrap_err().code(), code);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    assert_eq!(mark.summary().marked_slots, 20);
    drop(mark);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_mark_cancellation_and_pressure_gate_builds_and_queries() {
  for pressure in [false, true] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("mark-interrupt", None, [1; 16], HashAlgorithm::Blake3_256, 0);
    let (_, base, _) = seed_captured_graph(&publisher);
    flush_mark_fixture(&publisher);
    let locator = publisher.locator(&base).unwrap().unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    let summary = *mark.summary();
    if pressure {
      memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
    } else {
      cancellation.cancel();
    }
    let code = if pressure { "semantic_task_observation_memory" } else { "semantic_task_observation_cancelled" };
    assert_eq!(capture.mark_captured_semantic_tasks(mark_bounds()).unwrap_err().code(), code);
    assert_eq!(mark.is_captured_locator_marked(&locator).unwrap_err().code(), code);
    assert_eq!(mark.summary(), &summary);
    drop(mark);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    if pressure {
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      drop(capture.mark_captured_semantic_tasks(mark_bounds()).unwrap());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_semantic_task_mark_later_missing_dependency_discards_earlier_bits() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("mark-later-failure", None, [1; 16], algorithm, 0);
  let (mut expected, _, _) = seed_captured_graph(&publisher);
  seed_second_task(&publisher, &mut expected);
  flush_mark_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let original = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mut order: Vec<[u8; 16]> = Vec::new();
  original
    .visit_metadata(|observation| {
      order.push(observation.task().unwrap().unwrap().task_id.try_into().unwrap());
      Ok(true)
    })
    .unwrap();
  assert_eq!(order.len(), 2);
  let mut first_graph = PhysicalSet::new();
  original
    .visit_captured_semantic_task_metadata_entries(&order[0], graph_bounds(), |entry| {
      first_graph.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
      Ok(())
    })
    .unwrap();
  let mut identity = checkpoint_identity();
  identity[..16].copy_from_slice(&order[1]);
  let key = first_authority_file_path_hash(
    &system_control_path(SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable).unwrap(),
    algorithm,
  );
  let missing = publisher.locator(&key).unwrap().unwrap();
  assert!(publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
  seed_files(&publisher, &[]);
  flush_mark_fixture(&publisher);
  let damaged = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut provisional = PhysicalSet::new();
  let graph_error = damaged
    .visit_captured_semantic_task_retention_entries(retention_bounds(), |entry| {
      provisional.insert(entry.hash.clone(), (entry.type_flags, entry.offset, entry.total_length));
      Ok(())
    })
    .unwrap_err();
  for (key, locator) in first_graph {
    assert_eq!(provisional.get(&key), Some(&locator));
  }
  let error = damaged.mark_captured_semantic_tasks(mark_bounds()).unwrap_err();
  assert_eq!(error.code(), "semantic_source_catalog_capture_missing");
  assert_eq!(error.code(), graph_error.code());
  assert!(matches!(error, SemanticTaskMarkErrorV1::Graph(_)));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
  publisher.lock_kv().unwrap().insert(missing).unwrap();
  seed_files(&publisher, &[]);
  flush_mark_fixture(&publisher);
  let restored = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  assert_eq!(restored.mark_captured_semantic_tasks(mark_bounds()).unwrap().summary().marked_slots, 26);
  assert_eq!(damaged.mark_captured_semantic_tasks(mark_bounds()).unwrap_err().code(), "semantic_source_catalog_capture_missing");
  assert_eq!(original.mark_captured_semantic_tasks(mark_bounds()).unwrap().summary().marked_slots, 26);
}

#[test]
fn native_semantic_task_mark_preserves_capture_limits_and_nested_scratch_admission() {
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("mark-capture-bounds", None, [1; 16], HashAlgorithm::Blake3_256, 0);
  let (mut expected, _, _) = seed_captured_graph(&publisher);
  seed_second_task(&publisher, &mut expected);
  flush_mark_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let ordinary = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let totals = ordinary.visit_captured_semantic_task_retention_entries(retention_bounds(), |_| Ok(())).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  for bytes in [false, true] {
    let limits = if bytes {
      NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: totals.read_bytes - 1, ..retention_capture_bounds() }
    } else {
      NativeSemanticMutationInventoryBoundsV1 { maximum_work: totals.work - 1, ..retention_capture_bounds() }
    };
    let limited = protection.capture_semantic_mutation_inventory(limits, &memory, &cancellation).unwrap();
    let error = limited.mark_captured_semantic_tasks(mark_bounds()).unwrap_err();
    assert_eq!(error.code(), if bytes { "semantic_task_retention_read_bound" } else { "semantic_task_retention_work_bound" });
  }
  let large = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  assert_eq!(large.mark_captured_semantic_tasks(mark_bounds()).unwrap_err().code(), "semantic_task_observation_memory");
  drop(large);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(ordinary.mark_captured_semantic_tasks(mark_bounds()).unwrap().summary().marked_slots, 26);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_mark_rebuilds_from_reopened_native_controls() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("mark-reopen", None, [1; 16], algorithm, 0);
    let (mut expected, _, _) = seed_captured_graph(&publisher);
    seed_second_task(&publisher, &mut expected);
    flush_mark_fixture(&publisher);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    // Explicit run-start fixture preparation remains separate from the mark.
    flush_mark_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    assert_eq!(mark.summary().retention.tasks, 2);
    assert_eq!(mark.summary().marked_slots, 26);
    for (key, (flags, offset, total_length)) in expected {
      assert!(mark.is_captured_locator_marked(&KVEntry { hash: key, type_flags: flags, offset, total_length }).unwrap());
    }
    drop(mark);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
