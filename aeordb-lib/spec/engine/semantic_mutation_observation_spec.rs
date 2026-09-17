//! Native physical fixtures; semantic payloads come from frozen independent bytes.
use super::*;
#[path = "../support/allocation_probe.rs"]
mod allocation_probe;
#[path = "semantic_mutation_inventory_spec.rs"]
mod inventory_spec;
#[path = "native_protected_source_spec.rs"]
mod protected_source_spec;
use allocation_probe::measure;
use crate::engine::v4::semantic_mutation_control::{SemanticMutationPhaseV1, SemanticMutationTaskStateV1};

#[test]
fn canonical_system_file_valid_empty_and_exact_bodies_remain_fallibly_loaded() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("canonical-length-valid", None, [1; 16], algorithm, 0);
    let control_path = "/.aeordb-system/reader-fixture.bin";
    let content_type = "application/octet-stream";
    for body in [Vec::new(), vec![0x53; 131_071]] {
      seed_files(&publisher, &[(control_path.to_string(), content_type, &body)]);
      let before = fs::read(&path).unwrap();
      let header = publisher.observe().unwrap().selected.header;
      let kv = publisher.lock_kv().unwrap();
      let load = || load_canonical_system_file_at_path(&publisher.file, &*kv, &header, control_path, content_type, body.len());
      assert_eq!(load().unwrap().unwrap().body, body);
      if !body.is_empty() {
        let (result, allocations) = measure(body.len(), load);
        assert!(allocations.injected_failure, "{allocations:?}");
        assert_eq!(allocations.matching_requests, 1);
        assert_eq!(result.err().expect("output allocation must fail").code(), "first_authority_system_file_allocation");
      }
      assert_eq!(load().unwrap().unwrap().body, body);
      assert_eq!(fs::read(&path).unwrap(), before);
      assert_eq!(publisher.observe().unwrap().selected.header, header);
    }
  }
}

#[test]
fn canonical_system_file_captured_output_allocation_failure_releases_visit_and_retries() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("canonical-captured-allocation", None, [1; 16], algorithm, 0);
    let (task, checkpoint) = largest_checkpoint_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection
      .capture_semantic_mutation_inventory(
        NativeSemanticMutationInventoryBoundsV1 {
          maximum_work: 4096,
          maximum_entity_bytes: 1024 * 1024,
          maximum_read_bytes: 64 * 1024 * 1024,
        },
        &memory,
        &cancellation,
      )
      .unwrap();
    assert!(captured.visit(|_| Ok(true)).unwrap().complete);
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut callbacks = 0;
    let (result, allocations) = measure(checkpoint.len(), || {
      captured.visit(|_| {
        callbacks += 1;
        Ok(true)
      })
    });
    assert!(allocations.injected_failure, "{allocations:?}");
    assert_eq!(allocations.matching_requests, 1);
    assert_eq!(result.unwrap_err().code(), "first_authority_system_file_allocation");
    assert_eq!(callbacks, 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    let retried = captured.visit(|_| Ok(true)).unwrap();
    assert!(retried.complete);
    assert_eq!(retried.tasks, 1);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(captured);
    drop(protection);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

fn replace_test_file_declared_size(publisher: &V4FirstAuthorityPublisher, path: &str, declared_size: u64) {
  let header = publisher.observe().unwrap().selected.header;
  let key = first_authority_file_path_hash(path, header.hash_algorithm);
  let kv = publisher.lock_kv().unwrap();
  let locator = kv.get(&key).unwrap().unwrap();
  let bytes = read_entity_bounded(&publisher.file, &*kv, &key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)
    .unwrap()
    .unwrap();
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).unwrap();
  let mut record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), 1).unwrap();
  record.total_size = declared_size;
  let body = record.serialize(header.hash_algorithm.hash_length()).unwrap();
  let replacement = encode_entity(
    entity.entity_version,
    entity.entry_type,
    entity.flags,
    header.hash_algorithm,
    EntityPublicationOrder { timestamp_ms: entity.timestamp_ms, write_sequence: entity.write_sequence },
    entity.key,
    &body,
  )
  .unwrap();
  assert_eq!(replacement.len(), bytes.len());
  write_file_at_native(&publisher.file, locator.offset, &replacement).unwrap();
}

#[test]
fn canonical_system_file_length_mismatch_refuses_before_output_allocation() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("canonical-length-before-copy", None, [1; 16], algorithm, 0);
    let (task, checkpoint) = largest_checkpoint_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let control_path =
      system_control_path(SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity(), SystemControlSlotV1::Immutable).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    drop(observe(&publisher, &memory, &cancellation).unwrap());
    for declared in [0, 1, checkpoint.len() - 1, checkpoint.len() + 1] {
      replace_test_file_declared_size(&publisher, &control_path, declared as u64);
      let before = fs::read(&path).unwrap();
      let allocation_size = if declared == 0 { checkpoint.len() } else { declared };
      // Count the allocation without aborting the test process on the old
      // infallible growth path. Refusal injection follows only after this RED.
      let (result, allocations) =
        allocation_probe::measure_nth(allocation_size, usize::MAX, || observe(&publisher, &memory, &cancellation));
      assert_eq!(result.unwrap_err().code(), "first_authority_system_file_content");
      assert_eq!(allocations.matching_requests, 0, "declared={declared}, {allocations:?}");
      assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
      assert_eq!(fs::read(&path).unwrap(), before);
      replace_test_file_declared_size(&publisher, &control_path, checkpoint.len() as u64);
      drop(observe(&publisher, &memory, &cancellation).unwrap());
    }
  }
}

#[test]
fn canonical_system_file_length_mismatch_blocks_captured_task_completion() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("canonical-captured-length", None, [1; 16], algorithm, 0);
    let (task, checkpoint) = largest_checkpoint_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let control_path =
      system_control_path(SystemControlKindV1::SemanticMutationCheckpoint, &checkpoint_identity(), SystemControlSlotV1::Immutable).unwrap();
    replace_test_file_declared_size(&publisher, &control_path, 0);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection
      .capture_semantic_mutation_inventory(
        NativeSemanticMutationInventoryBoundsV1 {
          maximum_work: 4096,
          maximum_entity_bytes: 1024 * 1024,
          maximum_read_bytes: 64 * 1024 * 1024,
        },
        &memory,
        &cancellation,
      )
      .unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut callbacks = 0;
    let (result, allocations) = allocation_probe::measure_nth(checkpoint.len(), usize::MAX, || {
      captured.visit(|_| {
        callbacks += 1;
        Ok(true)
      })
    });
    assert_eq!(result.unwrap_err().code(), "first_authority_system_file_content");
    assert_eq!(allocations.matching_requests, 0, "{allocations:?}");
    assert_eq!(callbacks, 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

fn observation_memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 16 * 1024 * 1024).unwrap())
}

fn frozen(algorithm: HashAlgorithm, kind: &str) -> Vec<u8> {
  let profile = if algorithm.hash_length() == 32 { "blake3-256" } else { "sha512" };
  fs::read(format!(
    "{}/spec/fixtures/v4/system-control-v1/control-{profile}-semantic-mutation-{kind}-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap()
}

fn crc(bytes: &mut [u8]) {
  let offset = bytes.len() - 4;
  let checksum = crc32fast::hash(&bytes[..offset]);
  bytes[offset..].copy_from_slice(&checksum.to_le_bytes());
}

fn ready_pair(algorithm: HashAlgorithm) -> (Vec<u8>, Vec<u8>) {
  let checkpoint = frozen(algorithm, "checkpoint");
  let mut task = frozen(algorithm, "task");
  task[128..130].copy_from_slice(&4u16.to_le_bytes());
  let width = algorithm.hash_length();
  task[144..144 + width].copy_from_slice(&digest_parts(algorithm, &[&checkpoint]));
  crc(&mut task);
  (task, checkpoint)
}

fn checkpoint_identity() -> [u8; 24] {
  let mut identity = [2; 24];
  identity[16..].copy_from_slice(&1u64.to_le_bytes());
  identity
}

// Test-only construction deliberately does not grant the production publisher
// permission to write semantic controls. Physical wrappers use existing codecs;
// task/checkpoint/generation payloads remain independent fixture bytes.
fn seed(publisher: &V4FirstAuthorityPublisher, controls: &[(SystemControlKindV1, &[u8], SystemControlSlotV1, &[u8])]) {
  let files: Vec<_> = controls
    .iter()
    .map(|(kind, identity, slot, body)| (system_control_path(*kind, identity, *slot).unwrap(), SYSTEM_CONTROL_CONTENT_TYPE, *body))
    .collect();
  seed_files(publisher, &files);
}

fn seed_files(publisher: &V4FirstAuthorityPublisher, files: &[(String, &str, &[u8])]) {
  let _guard = publisher.root_state.lock().unwrap();
  let mut observed = publisher.observe().unwrap();
  let header = &mut observed.selected.header;
  let mut entities = Vec::new();
  let mut sequence = header.write_sequence_high_water;
  for (path, content_type, body) in files {
    sequence =
      append_system_file(&mut entities, path.clone(), content_type, body, header.hash_algorithm, header.updated_at_ms + 1, sequence)
        .unwrap();
  }
  let mut kv = publisher.lock_kv().unwrap();
  let mut offset = header.hot_tail_offset;
  for entity in &entities {
    write_file_at_native(&publisher.file, offset, &entity.bytes).unwrap();
    kv.insert(KVEntry { type_flags: entity.kv_type, hash: entity.key.clone(), offset, total_length: entity.bytes.len() as u32 }).unwrap();
    offset += entity.bytes.len() as u64;
  }
  kv.set_hot_tail_offset(offset);
  kv.force_flush_hot_buffer().unwrap();
  let entry_count = kv.len() as u64;
  drop(kv);
  header.slot_sequence += 1;
  header.updated_at_ms += 1;
  header.hot_tail_offset = offset;
  header.write_sequence_high_water = sequence;
  header.entry_count = entry_count;
  header.required_reader_capabilities[3] |= 2;
  header.required_writer_capabilities[3] |= 2;
  let encoded = encode_database_header_slot(header).unwrap();
  write_file_at_native(&publisher.file, 0, &encoded).unwrap();
  write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &encoded).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
}

fn observe(
  publisher: &V4FirstAuthorityPublisher,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<SemanticMutationObservationV1, SemanticMutationObservationErrorV1> {
  publisher.observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
    database_id: &[1; 16],
    task_id: &[2; 16],
    memory,
    cancellation,
  })
}

fn populate(publisher: &V4FirstAuthorityPublisher, task: &[u8], checkpoint: Option<&[u8]>, generation: Option<&[u8]>) {
  let identity = checkpoint_identity();
  let mut controls = vec![(SystemControlKindV1::SemanticMutationTask, [2; 16].as_slice(), SystemControlSlotV1::A, task)];
  if let Some(body) = checkpoint {
    controls.push((SystemControlKindV1::SemanticMutationCheckpoint, identity.as_slice(), SystemControlSlotV1::Immutable, body));
  }
  if let Some(body) = generation {
    controls.push((SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::A, body));
  }
  seed(publisher, &controls);
}

#[test]
fn native_task_observation_reopens_both_widths_without_writing_or_granting_ownership() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("semantic-observation", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (task, checkpoint) = ready_pair(algorithm);
    let mut generation = frozen(algorithm, "generation");
    generation[16..24].copy_from_slice(&99u64.to_le_bytes());
    crc(&mut generation);
    populate(&publisher, &task, Some(&checkpoint), Some(&generation));
    drop(publisher);
    let (_reopened_coordinator, reopened) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let header = reopened.observe().unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let observed = observe(&reopened, &memory, &cancellation).unwrap();
    assert_eq!(observed.disposition(), SemanticMutationObservationDispositionV1::CheckpointHeld);
    assert_eq!(observed.header(), &header);
    assert_eq!(observed.generation(), Some(99));
    let selected_task = observed.task().unwrap().unwrap();
    assert_eq!(selected_task.state, SemanticMutationTaskStateV1::ReadyToActivate);
    assert_eq!(selected_task.physical_instance_id, [3; 16]);
    assert_ne!(selected_task.physical_instance_id, header.selected.header.physical_instance_id);
    assert_eq!(observed.checkpoint().unwrap().unwrap().phase, SemanticMutationPhaseV1::Ready);
    assert_eq!(observed.checkpoint().unwrap().unwrap().semantic_generation, 1);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(reopened.observe().unwrap(), header);
    assert!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes > 0);
    drop(observed);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_task_observation_distinguishes_absence_from_unreleased_missing_dependencies() {
  for missing in ["task", "checkpoint", "generation"] {
    let (_directory, _path, _coordinator, publisher) = create_environment_for_database("semantic-observation-missing", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let (task, checkpoint) = ready_pair(algorithm);
    let generation = frozen(algorithm, "generation");
    if missing != "task" {
      populate(
        &publisher,
        &task,
        (missing != "checkpoint").then_some(checkpoint.as_slice()),
        (missing != "generation").then_some(generation.as_slice()),
      );
    }
    let before = publisher.observe().unwrap();
    let memory = observation_memory();
    let result = observe(&publisher, &memory, &CancellationToken::new());
    if missing == "task" {
      let result = result.unwrap();
      assert_eq!(result.disposition(), SemanticMutationObservationDispositionV1::Absent);
      assert!(result.task().unwrap().is_none());
      assert!(result.checkpoint().unwrap().is_none());
      assert_eq!(result.generation(), None);
    } else {
      assert_eq!(
        result.unwrap_err().code(),
        if missing == "checkpoint" {
          "semantic_task_observation_checkpoint_missing"
        } else {
          "semantic_task_observation_generation_missing"
        }
      );
    }
    assert_eq!(publisher.observe().unwrap(), before);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_task_observation_allows_released_terminal_summary_without_checkpoint() {
  for state in [6u16, 7, 8, 9] {
    let (_directory, _path, _coordinator, publisher) = create_environment_for_database("semantic-observation-released", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let (mut task, _) = ready_pair(algorithm);
    task[128..130].copy_from_slice(&state.to_le_bytes());
    task[130..132].copy_from_slice(&1u16.to_le_bytes());
    crc(&mut task);
    populate(&publisher, &task, None, Some(&frozen(algorithm, "generation")));
    let memory = observation_memory();
    let observed = observe(&publisher, &memory, &CancellationToken::new()).unwrap();
    assert_eq!(observed.disposition(), SemanticMutationObservationDispositionV1::ReleasedTerminal);
    assert!(observed.task().unwrap().unwrap().pins_released);
    assert!(observed.checkpoint().unwrap().is_none());
  }
}

#[test]
fn native_task_observation_rejects_pre_cancellation_and_budget_pressure_then_retries() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("semantic-observation-admission", None, [1; 16]);
  let before = fs::read(&path).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
  let cancelled = CancellationToken::new();
  cancelled.cancel();
  assert_eq!(observe(&publisher, &memory, &cancelled).unwrap_err().code(), "semantic_task_observation_cancelled");
  assert_eq!(observe(&publisher, &memory, &CancellationToken::new()).unwrap_err().code(), "semantic_task_observation_memory");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  let admitted = observation_memory();
  assert_eq!(
    observe(&publisher, &admitted, &CancellationToken::new()).unwrap().disposition(),
    SemanticMutationObservationDispositionV1::Absent
  );
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_task_observation_preserves_checkpoint_binding_errors_without_writing() {
  for (case, expected) in [
    ("digest", "semantic_task_checkpoint_digest"),
    ("phase", "semantic_task_checkpoint_phase"),
    ("physical", "semantic_task_checkpoint_identity"),
    ("fence", "semantic_task_checkpoint_identity"),
    ("time", "semantic_task_checkpoint_time"),
    ("path", "immutable_system_control_stored_mismatch"),
  ] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("semantic-observation-binding", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let (mut task, mut checkpoint) = ready_pair(algorithm);
    match case {
      "phase" => task[128..130].copy_from_slice(&1u16.to_le_bytes()),
      "physical" => task[64] ^= 1,
      "fence" => checkpoint[88..96].copy_from_slice(&2u64.to_le_bytes()),
      "time" => task[112..120].copy_from_slice(&101u64.to_le_bytes()),
      "path" => checkpoint[48] ^= 1,
      "digest" => {}
      _ => unreachable!(),
    }
    crc(&mut checkpoint);
    task[144..176].copy_from_slice(&digest_parts(algorithm, &[&checkpoint]));
    if case == "digest" {
      task[144] ^= 1;
    }
    crc(&mut task);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    assert_eq!(observe(&publisher, &memory, &CancellationToken::new()).unwrap_err().code(), expected, "{case}");
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_observation_reuses_slot_selection_and_refuses_ambiguous_or_physically_corrupt_slots() {
  for case in ["newer", "torn-control", "equal-sequence", "torn-entity"] {
    let (_directory, path, _coordinator, publisher) = create_environment_for_database("semantic-observation-slots", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let (task, checkpoint) = ready_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let mut b = task.clone();
    b[16..24].copy_from_slice(&(if case == "equal-sequence" { 7u64 } else { 8 }).to_le_bytes());
    b[120..128].copy_from_slice(&102u64.to_le_bytes());
    crc(&mut b);
    if case == "torn-control" {
      let last = b.len() - 1;
      b[last] ^= 1;
    }
    seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B, &b)]);
    if case == "torn-entity" {
      let path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::B).unwrap();
      corrupt_last_entity_byte(&publisher, &first_authority_file_path_hash(&path, algorithm));
    }
    let before = fs::read(&path).unwrap();
    let result = observe(&publisher, &observation_memory(), &CancellationToken::new());
    match case {
      "newer" | "torn-control" => {
        let observed = result.unwrap();
        let selected = observed.task_selection().unwrap();
        assert_eq!(selected.control_sequence, if case == "newer" { 8 } else { 7 });
        assert_eq!(selected.selected_slot, if case == "newer" { SystemControlSlotV1::B } else { SystemControlSlotV1::A });
        assert_eq!(selected.redundancy_degraded, case == "torn-control");
      }
      "equal-sequence" => assert_eq!(result.unwrap_err().code(), "system_control_equal_sequence"),
      "torn-entity" => assert_eq!(result.unwrap_err().code(), "integrity_hash_mismatch"),
      _ => unreachable!(),
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}

#[test]
fn native_task_observation_cancellation_between_reads_releases_memory_without_writing() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("semantic-observation-mid-cancel", None, [1; 16]);
  let algorithm = HashAlgorithm::Blake3_256;
  let (task, checkpoint) = ready_pair(algorithm);
  populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let result = publisher.observe_semantic_mutation_task_with_observer(
    SemanticMutationObservationRequestV1 { database_id: &[1; 16], task_id: &[2; 16], memory: &memory, cancellation: &cancellation },
    || cancellation.cancel(),
  );
  assert_eq!(result.unwrap_err().code(), "semantic_task_observation_cancelled");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_task_observation_holds_one_guard_through_task_and_generation_selection() {
  let (_directory, _path, _coordinator, publisher) = create_environment_for_database("semantic-observation-concurrent", None, [1; 16]);
  let algorithm = HashAlgorithm::Blake3_256;
  let (task, checkpoint) = ready_pair(algorithm);
  let mut generation = frozen(algorithm, "generation");
  generation[16..24].copy_from_slice(&10u64.to_le_bytes());
  crc(&mut generation);
  populate(&publisher, &task, Some(&checkpoint), Some(&generation));
  generation[16..24].copy_from_slice(&11u64.to_le_bytes());
  crc(&mut generation);
  let publisher = Arc::new(publisher);
  let writer = Arc::clone(&publisher);
  let (attempted_sender, attempted_receiver) = mpsc::channel();
  let (acquired_sender, acquired_receiver) = mpsc::channel();
  let (finished_sender, finished_receiver) = mpsc::channel();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let mut worker = None;
  let observed = publisher
    .observe_semantic_mutation_task_with_observer(
      SemanticMutationObservationRequestV1 { database_id: &[1; 16], task_id: &[2; 16], memory: &memory, cancellation: &cancellation },
      || {
        worker = Some(std::thread::spawn(move || {
          attempted_sender.send(()).unwrap();
          let authority = writer.root_state.lock().unwrap();
          acquired_sender.send(()).unwrap();
          drop(authority);
          seed(&writer, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::B, &generation)]);
          finished_sender.send(()).unwrap();
        }));
        attempted_receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(acquired_receiver.recv_timeout(Duration::from_millis(50)), Err(mpsc::RecvTimeoutError::Timeout));
      },
    )
    .unwrap();
  assert_eq!(observed.generation(), Some(10));
  finished_receiver.recv_timeout(Duration::from_secs(5)).unwrap();
  worker.unwrap().join().unwrap();
  assert_eq!(observe(&publisher, &memory, &cancellation).unwrap().generation(), Some(11));
}

#[test]
fn native_task_observation_requires_both_capability_declarations_for_present_controls() {
  for reader in [false, true] {
    let (_directory, _path, _coordinator, publisher) = create_environment_for_database("semantic-observation-capability", None, [1; 16]);
    let algorithm = HashAlgorithm::Blake3_256;
    let (task, checkpoint) = ready_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let mut header = publisher.observe().unwrap().selected.header;
    if reader {
      header.required_reader_capabilities[3] &= !2;
    } else {
      header.required_writer_capabilities[3] &= !2;
    }
    let encoded = encode_database_header_slot(&header).unwrap();
    write_file_at_native(&publisher.file, 0, &encoded).unwrap();
    write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &encoded).unwrap();
    let error = observe(&publisher, &observation_memory(), &CancellationToken::new()).unwrap_err();
    assert_eq!(error.code(), "semantic_task_observation_capability");
  }
}

fn largest_checkpoint_pair(algorithm: HashAlgorithm) -> (Vec<u8>, Vec<u8>) {
  let width = algorithm.hash_length();
  let mut checkpoint = frozen(algorithm, "checkpoint");
  checkpoint.truncate(checkpoint.len() - 4);
  checkpoint[120..122].copy_from_slice(&2u16.to_le_bytes());
  checkpoint[122..124].copy_from_slice(&1u16.to_le_bytes());
  checkpoint[124..128].copy_from_slice(&65_535u32.to_le_bytes());
  checkpoint[200 + 4 * width..200 + 6 * width].fill(0);
  checkpoint.push(b'/');
  checkpoint.resize(checkpoint.len() + 65_534, b'a');
  let body_length = checkpoint.len() - 32;
  checkpoint.extend_from_slice(&[0; 4]);
  let total_length = checkpoint.len();
  checkpoint[8..12].copy_from_slice(&(total_length as u32).to_le_bytes());
  checkpoint[24..28].copy_from_slice(&(body_length as u32).to_le_bytes());
  crc(&mut checkpoint);
  let mut task = frozen(algorithm, "task");
  task[128..130].copy_from_slice(&3u16.to_le_bytes());
  task[144..144 + width].copy_from_slice(&digest_parts(algorithm, &[&checkpoint]));
  crc(&mut task);
  (task, checkpoint)
}

#[test]
fn native_task_observation_accounts_for_the_largest_checkpoint_and_keeps_the_charge_until_drop() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("semantic-observation-memory-bound", None, [1; 16], algorithm, 0);
    let (task, checkpoint) = largest_checkpoint_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    // Cache/page memory belongs to its existing KV owner; isolate the new
    // operation's heap buffers after warming that already-accounted cache.
    drop(observe(&publisher, &memory, &cancellation).unwrap());
    let (result, allocations) = measure(0, || observe(&publisher, &memory, &cancellation));
    let observed = result.unwrap();
    assert_eq!(observed.checkpoint().unwrap().unwrap().phase, SemanticMutationPhaseV1::Compiling);
    let snapshot = memory.snapshot().unwrap();
    let owner = snapshot.owner(MemoryOwner::Task).unwrap();
    assert_eq!(owner.reserved_bytes, 8_978_720);
    assert_eq!(owner.peak_reserved_bytes, 8_978_720);
    assert_eq!(owner.active_reservations, 1);
    assert!(allocations.total as u64 <= owner.reserved_bytes, "{allocations:?}");
    assert!(allocations.maximum >= checkpoint.len());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(observed);
    let snapshot = memory.snapshot().unwrap();
    assert_eq!(snapshot.owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
    assert_eq!(snapshot.owner(MemoryOwner::Task).unwrap().active_reservations, 0);
  }
}

#[test]
fn native_task_observation_reports_entity_and_body_allocation_refusals_then_retries() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("semantic-observation-allocation-refusal", None, [1; 16], algorithm, 0);
    let (task, checkpoint) = largest_checkpoint_pair(algorithm);
    populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    drop(observe(&publisher, &memory, &cancellation).unwrap());
    let entity_length = checkpoint.len() + 77 + 2 * algorithm.hash_length();
    for (size, code) in
      [(entity_length, "first_authority_readback_allocation"), (checkpoint.len(), "first_authority_system_file_allocation")]
    {
      let (result, allocations) = measure(size, || observe(&publisher, &memory, &cancellation));
      assert!(allocations.injected_failure, "{size}/{allocations:?}");
      assert_eq!(allocations.matching_requests, 1);
      let error = result.unwrap_err();
      assert_eq!(error.code(), code);
      assert!(error.source().is_some());
      assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
      drop(observe(&publisher, &memory, &cancellation).unwrap());
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_task_observation_rejects_invalid_request_identity_and_unconfigured_memory() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("semantic-observation-request", None, [1; 16]);
  let before = fs::read(&path).unwrap();
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  for (database_id, task_id, code) in [
    ([0; 16], [2; 16], "semantic_task_observation_identity"),
    ([1; 16], [0; 16], "semantic_task_observation_identity"),
    ([3; 16], [2; 16], "mutable_control_database_mismatch"),
  ] {
    let result = publisher.observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
      database_id: &database_id,
      task_id: &task_id,
      memory: &memory,
      cancellation: &cancellation,
    });
    assert_eq!(result.unwrap_err().code(), code);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  }
  assert_eq!(
    observe(&publisher, &MemoryCoordinator::without_policy(), &cancellation).unwrap_err().code(),
    "semantic_task_observation_memory"
  );
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_task_observation_preserves_native_read_failure_and_retries_after_fixture_restoration() {
  let (_directory, path, _coordinator, publisher) = create_environment_for_database("semantic-observation-read-failure", None, [1; 16]);
  let algorithm = HashAlgorithm::Blake3_256;
  let (task, checkpoint) = ready_pair(algorithm);
  populate(&publisher, &task, Some(&checkpoint), Some(&frozen(algorithm, "generation")));
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  drop(observe(&publisher, &memory, &cancellation).unwrap());
  let complete = fs::read(&path).unwrap();
  let chunk_key = first_authority_system_chunk_hash(&checkpoint, algorithm);
  let locator = publisher.lock_kv().unwrap().get(&chunk_key).unwrap().unwrap();
  publisher.file.set_len(locator.offset + 2).unwrap();
  let damaged = fs::read(&path).unwrap();
  let error = observe(&publisher, &memory, &cancellation).unwrap_err();
  assert_eq!(error.code(), "first_authority_readback_io");
  assert!(error.source().is_some());
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  assert_eq!(fs::read(&path).unwrap(), damaged);
  write_file_at_native(&publisher.file, 0, &complete).unwrap();
  drop(observe(&publisher, &memory, &cancellation).unwrap());
  assert_eq!(fs::read(&path).unwrap(), complete);
}
