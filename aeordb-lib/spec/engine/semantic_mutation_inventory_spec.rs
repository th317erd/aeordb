use super::*;

fn bounds() -> NativeSemanticMutationInventoryBoundsV1 {
  NativeSemanticMutationInventoryBoundsV1 { maximum_work: 100_000, maximum_entity_bytes: 4 << 20, maximum_read_bytes: 64 << 20 }
}

fn released_task(algorithm: HashAlgorithm, identity: u8, sequence: u64) -> Vec<u8> {
  let mut task = frozen(algorithm, "task");
  task[16..24].copy_from_slice(&sequence.to_le_bytes());
  task[48..64].fill(identity);
  task[128..130].copy_from_slice(&6u16.to_le_bytes());
  task[130..132].copy_from_slice(&1u16.to_le_bytes());
  crc(&mut task);
  task
}

#[test]
fn native_task_inventory_discovers_all_unlinked_tasks_once_at_both_widths() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-inventory-unlinked", None, [1; 16], algorithm, 0);
    let initial = publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed(
      &publisher,
      &[
        (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &released_task(algorithm, 2, 1)),
        (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &released_task(algorithm, 2, 2)),
        (SystemControlKindV1::SemanticMutationTask, &[4; 16], SystemControlSlotV1::B, &released_task(algorithm, 4, 1)),
        (SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &frozen(algorithm, "generation")),
      ],
    );
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
    let mut selected = Vec::new();
    let summary = captured
      .visit(|observation| {
        let task = observation.task().unwrap().unwrap();
        selected.push((task.task_id.to_vec(), task.control_sequence));
        assert_eq!(observation.disposition(), SemanticMutationObservationDispositionV1::ReleasedTerminal);
        Ok(true)
      })
      .unwrap();
    selected.sort();
    assert_eq!(selected, vec![(vec![2; 16], 2), (vec![4; 16], 1)]);
    assert_eq!(summary, SemanticMutationInventorySummaryV1 { tasks: 2, complete: true });
    assert_eq!(publisher.observe().unwrap().selected.header.head_hash, initial.namespace_root.root_hash);
    assert_eq!(fs::read(&path).unwrap(), before, "inventory is read-only while its publisher remains alive");
    drop(captured);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_task_inventory_keeps_captured_controls_while_callbacks_publish_replacements() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, _coordinator, publisher) = create_environment_for_database("task-inventory-capture", None, [1; 16]);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  assert!(publisher.root_state.try_lock().is_ok());
  assert!(publisher.kv.try_lock().is_ok());
  let mut callbacks = 0;
  let summary = old
    .visit(|observation| {
      assert_eq!(observation.task_selection().unwrap().control_sequence, 1);
      assert!(publisher.root_state.try_lock().is_ok(), "scan callback retained publication mutex");
      assert!(publisher.kv.try_lock().is_ok(), "scan callback retained KV mutex");
      callbacks += 1;
      seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &released_task(algorithm, 2, 2))]);
      Ok(true)
    })
    .unwrap();
  assert_eq!(summary.tasks, 1);
  assert!(summary.complete);
  assert_eq!(callbacks, 1);
  let fresh = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  for (captured, sequence) in [(&old, 1), (&fresh, 2)] {
    let mut callbacks = 0;
    assert!(
      captured
        .visit(|observation| {
          assert_eq!(observation.task_selection().unwrap().control_sequence, sequence);
          callbacks += 1;
          Ok(true)
        })
        .unwrap()
        .complete
    );
    assert_eq!(callbacks, 1);
  }
}

fn scan(publisher: &V4FirstAuthorityPublisher) -> Result<SemanticMutationInventorySummaryV1, SemanticMutationObservationErrorV1> {
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation)?;
  capture.visit(|_| Ok(true))
}

#[test]
fn native_task_inventory_preserves_shared_pair_selection_and_physical_error_policy() {
  for case in ["a-only", "b-only", "equal-identical", "torn-a", "torn-b", "ambiguous", "physical-a", "physical-b"] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-slots", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let mut a = released_task(algorithm, 2, 1);
    let mut b = released_task(algorithm, 2, 1);
    if case == "ambiguous" {
      b[120..128].copy_from_slice(&102u64.to_le_bytes());
      crc(&mut b);
    }
    if case == "torn-a" {
      a[0] ^= 1;
    }
    if case == "torn-b" {
      b[0] ^= 1;
    }
    let generation = frozen(algorithm, "generation");
    let mut controls =
      vec![(SystemControlKindV1::SemanticMutationGeneration, [].as_slice(), SystemControlSlotV1::A, generation.as_slice())];
    if case != "b-only" {
      controls.push((SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &a));
    }
    if case != "a-only" {
      controls.push((SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &b));
    }
    seed(&publisher, &controls);
    if case.starts_with("physical-") {
      let slot = if case == "physical-a" { SystemControlSlotV1::A } else { SystemControlSlotV1::B };
      let path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], slot).unwrap();
      corrupt_last_entity_byte(&publisher, &first_authority_file_path_hash(&path, algorithm));
    }
    let before = fs::read(&path).unwrap();
    let result = scan(&publisher);
    match case {
      "ambiguous" => assert_eq!(result.unwrap_err().code(), "system_control_equal_sequence"),
      "physical-a" | "physical-b" => assert_eq!(result.unwrap_err().code(), "integrity_hash_mismatch"),
      _ => assert_eq!(result.unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: true }, "{case}"),
    }
    assert_eq!(fs::read(&path).unwrap(), before, "{case}");
  }
}

#[test]
fn native_task_inventory_refuses_missing_dependencies_and_bad_checkpoint_binding() {
  for (case, code) in [
    ("generation", "semantic_task_observation_generation_missing"),
    ("checkpoint", "semantic_task_observation_checkpoint_missing"),
    ("digest", "semantic_task_checkpoint_digest"),
    ("phase", "semantic_task_checkpoint_phase"),
  ] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-checkpoint", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let (mut task, checkpoint) = ready_pair(algorithm);
    if case == "digest" {
      task[144] ^= 1;
    }
    if case == "phase" {
      task[128..130].copy_from_slice(&1u16.to_le_bytes());
    }
    crc(&mut task);
    let generation = frozen(algorithm, "generation");
    populate(&publisher, &task, (case != "checkpoint").then_some(&checkpoint), (case != "generation").then_some(&generation));
    let before = fs::read(&path).unwrap();
    assert_eq!(scan(&publisher).unwrap_err().code(), code, "{case}");
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_inventory_does_not_hide_controls_mistagged_in_kv() {
  for both in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-kv-role", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let task = released_task(algorithm, 2, 1);
    let generation = frozen(algorithm, "generation");
    seed(
      &publisher,
      &[
        (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task),
        (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &task),
        (SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, &generation),
      ],
    );
    let mut kv = publisher.lock_kv().unwrap();
    for slot in [SystemControlSlotV1::A, SystemControlSlotV1::B] {
      if slot == SystemControlSlotV1::B && !both {
        continue;
      }
      let path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], slot).unwrap();
      let key = first_authority_file_path_hash(&path, algorithm);
      let mut entry = kv.get(&key).unwrap().unwrap();
      entry.type_flags = KV_TYPE_CHUNK;
      kv.insert(entry).unwrap();
    }
    drop(kv);
    let before = fs::read(&path).unwrap();
    assert!(scan(&publisher).is_err(), "a mistagged task must not become a successful empty/partial inventory");
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_inventory_rejects_foreign_database_and_noncanonical_task_identity() {
  for foreign_database in [false, true] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-identity", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let mut task = released_task(algorithm, 2, 1);
    if foreign_database {
      task[32..48].fill(9);
    } else {
      task[48..64].fill(9);
    }
    crc(&mut task);
    populate(&publisher, &task, None, Some(&frozen(algorithm, "generation")));
    let before = fs::read(&path).unwrap();
    assert!(scan(&publisher).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_inventory_distinguishes_empty_complete_stop_error_and_cancellation() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-completion", None, [1; 16]);
  let before = fs::read(&path).unwrap();
  assert_eq!(scan(&publisher).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 0, complete: true });
  assert_eq!(fs::read(&path).unwrap(), before);
  let algorithm = HashAlgorithm::Blake3_256;
  populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  assert_eq!(captured.visit(|_| Ok(false)).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: false });
  let error = captured
    .visit(|_| Err(SemanticMutationObservationErrorV1::Invalid { code: "inventory_callback_test", message: "injected" }))
    .unwrap_err();
  assert_eq!(error.code(), "inventory_callback_test");
  assert!(captured
    .visit(|_| {
      cancellation.cancel();
      Ok(true)
    })
    .is_err());
  let mut callbacks = 0;
  assert!(captured
    .visit(|_| {
      callbacks += 1;
      Ok(true)
    })
    .is_err());
  assert_eq!(callbacks, 0);
}

#[test]
fn native_task_inventory_admission_and_work_limits_refuse_without_writes_then_retry() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-bounds", None, [1; 16]);
  let algorithm = HashAlgorithm::Blake3_256;
  populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let cancelled = CancellationToken::new();
  cancelled.cancel();
  assert!(protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancelled).is_err());
  for constrained in [MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap()), MemoryCoordinator::without_policy()] {
    assert!(protection.capture_semantic_mutation_inventory(bounds(), &constrained, &cancellation).is_err());
    assert_eq!(constrained.snapshot().unwrap().reserved_bytes, 0);
  }
  for limited in [
    NativeSemanticMutationInventoryBoundsV1 { maximum_work: 1, ..bounds() },
    NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: 1, ..bounds() },
    NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 32, ..bounds() },
  ] {
    match protection.capture_semantic_mutation_inventory(limited, &memory, &cancellation) {
      Err(_) => {}
      Ok(captured) => assert!(captured.visit(|_| Ok(true)).is_err(), "resource refusal must never become empty complete"),
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  let captured = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  assert_eq!(captured.visit(|_| Ok(true)).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 1, complete: true });
  drop(captured);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_task_inventory_requires_capabilities_and_exact_header_kv_alignment() {
  for case in ["reader", "writer", "entry-count"] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-header", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
    let mut header = publisher.observe().unwrap().selected.header;
    match case {
      "reader" => header.required_reader_capabilities[3] &= !2,
      "writer" => header.required_writer_capabilities[3] &= !2,
      "entry-count" => header.entry_count += 1,
      _ => unreachable!(),
    }
    let encoded = encode_database_header_slot(&header).unwrap();
    write_file_at_native(&publisher.file, 0, &encoded).unwrap();
    write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &encoded).unwrap();
    let before = fs::read(&path).unwrap();
    let error = scan(&publisher).unwrap_err();
    assert_eq!(
      error.code(),
      if case == "entry-count" { "first_authority_kv_header_mismatch" } else { "semantic_task_observation_capability" }
    );
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_inventory_holds_checkpoint_edges_and_preserves_allocation_and_read_failures() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-inventory-native-failures", None, [1; 16], algorithm, 0);
    let (task, checkpoint) = largest_checkpoint_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
    let mut visits = 0;
    assert!(
      captured
        .visit(|observation| {
          let checkpoint = observation.checkpoint().unwrap().unwrap();
          assert_eq!(checkpoint.phase, SemanticMutationPhaseV1::Compiling);
          use crate::engine::v4::semantic_mutation_control::SemanticMutationReferenceRoleV1 as Role;
          let expected = [
            (Role::AdmittedBaseNamespaceRoot, vec![1; algorithm.hash_length()]),
            (Role::StagedDirectoryTree, vec![2; algorithm.hash_length()]),
            (Role::SemanticCatalog, vec![3; algorithm.hash_length()]),
          ];
          assert!(checkpoint.references().eq(expected.iter().map(|(role, key)| (*role, key.as_slice()))));
          visits += 1;
          Ok(true)
        })
        .unwrap()
        .complete
    );
    assert_eq!(visits, 1);
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let complete = fs::read(&path).unwrap();
    for size in [checkpoint.len(), checkpoint.len() + 77 + 2 * algorithm.hash_length()] {
      let (result, allocations) = measure(size, || captured.visit(|_| Ok(true)));
      assert!(allocations.injected_failure, "{size}/{allocations:?}");
      assert!(result.is_err(), "allocation failure must not become torn-slot fallback or incomplete success");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), complete);
      assert_eq!(captured.visit(|_| Ok(true)).unwrap().tasks, 1);
    }
    let locator = publisher.lock_kv().unwrap().get(&first_authority_system_chunk_hash(&checkpoint, algorithm)).unwrap().unwrap();
    publisher.file.set_len(locator.offset + 2).unwrap();
    let damaged = fs::read(&path).unwrap();
    assert!(captured.visit(|_| Ok(true)).is_err());
    assert_eq!(fs::read(&path).unwrap(), damaged);
    write_file_at_native(&publisher.file, 0, &complete).unwrap();
    assert_eq!(captured.visit(|_| Ok(true)).unwrap().tasks, 1);
    assert_eq!(fs::read(&path).unwrap(), complete);
  }
}

#[test]
fn native_task_inventory_review_refuses_unsettled_visibility_even_when_counts_match() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-unsettled-visibility", None, [1; 16]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let header = publisher.observe().unwrap().selected.header;
  let batch = publisher.lock_kv().unwrap().begin_atomic_visibility_batch(1, header.slot_sequence + 1).unwrap();
  // Models the unresolved KV visibility boundary, not a second file owner.
  // No entry need be staged for old/new scalar counts to agree.
  let before = fs::read(&path).unwrap();
  let result = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation);
  publisher.lock_kv().unwrap().abort_atomic_visibility_batch(batch).unwrap();
  assert!(result.is_err(), "capture accepted an unresolved visibility owner because scalar metadata matched");
  drop(result);
  for (depth, pre_admitted) in [(1, false), (2, false), (1, true), (0, true)] {
    {
      let mut kv = publisher.lock_kv().unwrap();
      kv.transaction_depth = depth;
      kv.pre_admitted_transaction_active = pre_admitted;
    }
    let result = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation);
    {
      let mut kv = publisher.lock_kv().unwrap();
      kv.transaction_depth = 0;
      kv.pre_admitted_transaction_active = false;
    }
    assert!(result.is_err(), "capture accepted transaction depth {depth}, pre-admitted {pre_admitted}");
    drop(result);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
  assert_eq!(scan(&publisher).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 0, complete: true });
}

#[test]
fn native_task_inventory_review_preserves_callback_error_when_cancellation_arrives_together() {
  let (_directory, _path, _coordinator, publisher) = create_environment_for_database("task-inventory-error-precedence", None, [1; 16]);
  let algorithm = HashAlgorithm::Blake3_256;
  populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  let error = captured
    .visit(|_| {
      cancellation.cancel();
      Err(SemanticMutationObservationErrorV1::Invalid {
        code: "inventory_original_callback_error",
        message: "injected error and cancellation",
      })
    })
    .unwrap_err();
  assert_eq!(error.code(), "inventory_original_callback_error");
}

#[test]
fn native_task_inventory_rechecks_cancellation_and_pressure_after_waiting_for_capture() {
  use crate::engine::memory_coordinator::HostMemorySample;

  for cancel_while_waiting in [true, false] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-wait-recheck", None, [1; 16]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let authority = publisher.root_state.lock().unwrap();
    std::thread::scope(|scope| {
      let capture = scope
        .spawn(|| protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).map(drop).map_err(|error| error.code()));
      let deadline = std::time::Instant::now() + Duration::from_secs(2);
      while memory.snapshot().unwrap().reserved_bytes == baseline && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
      }
      let waiting_reservation = memory.snapshot().unwrap().reserved_bytes;
      if cancel_while_waiting {
        cancellation.cancel();
      } else {
        memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
      }
      drop(authority);
      let result = capture.join().unwrap();
      assert!(waiting_reservation > baseline, "capture never reached its held authority boundary");
      assert_eq!(
        result.unwrap_err(),
        if cancel_while_waiting { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" }
      );
    });
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(publisher.root_state.lock().unwrap().active_staging_protections, 1);
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    drop(protection.capture_semantic_mutation_inventory(bounds(), &memory, &CancellationToken::new()).unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_inventory_rejects_invalid_bounds_before_allocating_capture_scratch() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-invalid-bounds", None, [1; 16]);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let before = fs::read(&path).unwrap();
  for invalid in [
    NativeSemanticMutationInventoryBoundsV1 { maximum_work: 0, ..bounds() },
    NativeSemanticMutationInventoryBoundsV1 { maximum_read_bytes: 0, ..bounds() },
    NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 0, ..bounds() },
    NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: (64 << 20) + 8193, ..bounds() },
    NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: usize::MAX, ..bounds() },
  ] {
    let result = protection.capture_semantic_mutation_inventory(invalid, &memory, &cancellation);
    assert_eq!(result.err().unwrap().code(), "semantic_task_inventory_bounds");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_task_inventory_refuses_locators_outside_its_captured_data_region() {
  for case in ["header", "kv", "tail", "overflow"] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-extent", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
    let header = publisher.observe().unwrap().selected.header;
    let control_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
    let key = first_authority_file_path_hash(&control_path, algorithm);
    let mut kv = publisher.lock_kv().unwrap();
    let mut locator = kv.get(&key).unwrap().unwrap();
    locator.offset = match case {
      "header" => 0,
      "kv" => header.kv_block_offset,
      "tail" => header.hot_tail_offset,
      "overflow" => u64::MAX - 1,
      _ => unreachable!(),
    };
    kv.insert(locator).unwrap();
    drop(kv);
    let before = fs::read(&path).unwrap();
    assert_eq!(scan(&publisher).unwrap_err().code(), "semantic_task_inventory_extent", "{case}");
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_inventory_review_accounts_for_each_simultaneous_visit() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-nested-memory", None, [1; 16]);
  let algorithm = HashAlgorithm::Blake3_256;
  populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let before = fs::read(&path).unwrap();
  let guard_charge = memory.snapshot().unwrap().reserved_bytes;
  let observed = observe(&publisher, &memory, &cancellation).unwrap();
  let observation_charge = memory.snapshot().unwrap().reserved_bytes - guard_charge;
  drop(observed);
  let captured = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let mut outer_charge = 0;
  let mut inner_charge = 0;
  assert!(
    captured
      .visit(|_| {
        outer_charge = memory.snapshot().unwrap().reserved_bytes;
        assert_eq!(
          captured
            .visit(|_| {
              inner_charge = memory.snapshot().unwrap().reserved_bytes;
              Ok(true)
            })?
            .tasks,
          1
        );
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, outer_charge);
        Ok(true)
      })
      .unwrap()
      .complete
  );
  assert!(outer_charge >= retained + observation_charge);
  assert!(inner_charge >= outer_charge + observation_charge + bounds().maximum_entity_bytes as u64,
    "a second live scan reused the first scan's scratch reservation: outer={outer_charge}, inner={inner_charge}, observation={observation_charge}");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_task_inventory_refuses_unknown_protected_and_noncanonical_control_paths() {
  for (case, code) in [
    ("unknown", "unknown_protected_system_family"),
    ("wrong-identity", "semantic_task_inventory_control_identity"),
    ("wrong-slot", "semantic_task_inventory_control_path"),
    ("unknown-family", "unknown_protected_system_family"),
    ("missing-slot", "semantic_task_inventory_control_path"),
    ("missing-digest", "semantic_task_inventory_control_path"),
  ] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("task-inventory-unknown", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let control_path = match case {
      "unknown" => format!("/.aeordb-system/controls/v1/ffff/{}/a.ctrl", "1".repeat(64)),
      "wrong-identity" => system_control_path(SystemControlKindV1::SemanticMutationTask, &[9; 16], SystemControlSlotV1::A).unwrap(),
      "wrong-slot" => system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A)
        .unwrap()
        .replace("/a.ctrl", "/i.ctrl"),
      "unknown-family" => "/.aeordb-unreviewed/control".to_string(),
      "missing-slot" => format!("/.aeordb-system/controls/v1/0044/{}", "1".repeat(64)),
      "missing-digest" => "/.aeordb-system/controls/v1/0044".to_string(),
      _ => unreachable!(),
    };
    seed_files(&publisher, &[(control_path, SYSTEM_CONTROL_CONTENT_TYPE, &released_task(algorithm, 2, 1))]);
    let before = fs::read(&path).unwrap();
    assert_eq!(scan(&publisher).unwrap_err().code(), code, "{case}");
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

fn publish_ordinary_record(publisher: &V4FirstAuthorityPublisher, path: &str, metadata_bytes: usize) -> Vec<u8> {
  let header = publisher.observe().unwrap().selected.header;
  let record = FileRecord {
    path: path.to_string(),
    content_type: None,
    total_size: 0,
    created_at: 100,
    updated_at: 100,
    metadata: vec![0; metadata_bytes],
    content_hash: first_authority_content_hash(&[], header.hash_algorithm),
    chunk_hashes: Vec::new(),
  };
  let value = record.serialize(header.hash_algorithm.hash_length()).unwrap();
  let key = digest_parts(header.hash_algorithm, &[b"filec:", &value]);
  let receipt = publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &header.database_id,
      entities: &[ImmutableEntityWriteV1 {
        entity_version: 1,
        entry_type: EntryTypeV4::FileRecord,
        flags: 0,
        key: &key,
        stored_value: &value,
      }],
      publication_timestamp_ms: header.updated_at_ms + 1,
    })
    .unwrap();
  assert!(!receipt.idempotent);
  key
}

#[test]
fn native_task_inventory_refuses_an_oversized_ordinary_record_instead_of_skipping_it() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("task-inventory-large-record", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    publish_ordinary_record(&publisher, "/ordinary.txt", 128 << 10);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let limited = NativeSemanticMutationInventoryBoundsV1 { maximum_entity_bytes: 64 << 10, ..bounds() };
    let captured = protection.capture_semantic_mutation_inventory(limited, &memory, &cancellation).unwrap();
    assert_eq!(captured.visit(|_| Ok(true)).unwrap_err().code(), "first_authority_locator_exceeds_cap");
    drop(captured);
    let captured = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
    assert_eq!(captured.visit(|_| Ok(true)).unwrap(), SemanticMutationInventorySummaryV1 { tasks: 0, complete: true });
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_inventory_callback_allows_a_real_native_publication_and_keeps_its_old_boundary() {
  let (_directory, _path, _coordinator, publisher) = create_environment_for_database("task-inventory-public-writer", None, [1; 16]);
  let algorithm = HashAlgorithm::Blake3_256;
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  populate(&publisher, &released_task(algorithm, 2, 1), None, Some(&frozen(algorithm, "generation")));
  let before = publisher.observe().unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let old = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  let mut callbacks = 0;
  let mut published = None;
  assert!(
    old
      .visit(|observation| {
        assert_eq!(observation.header(), &before);
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        published = Some(publish_ordinary_record(&publisher, "/from-callback.txt", 0));
        callbacks += 1;
        Ok(true)
      })
      .unwrap()
      .complete
  );
  assert_eq!(callbacks, 1);
  assert!(publisher.locator(&published.unwrap()).unwrap().is_some());
  assert!(publisher.observe().unwrap().selected.header.slot_sequence > before.selected.header.slot_sequence);
  assert!(
    old
      .visit(|observation| {
        assert_eq!(observation.header(), &before);
        Ok(true)
      })
      .unwrap()
      .complete
  );
  let fresh = protection.capture_semantic_mutation_inventory(bounds(), &memory, &cancellation).unwrap();
  assert!(
    fresh
      .visit(|observation| {
        assert_ne!(observation.header(), &before);
        Ok(true)
      })
      .unwrap()
      .complete
  );
}
