//! Compiler start preserves old selected dependencies until guarded selection.
use super::*;
use crate::engine::v4::semantic_mutation_control::decode_semantic_mutation_task;

fn work_pair_identity(sequence: u64) -> [u8; 24] {
  let mut identity = [0; 24];
  identity[..16].fill(2);
  identity[16..].copy_from_slice(&sequence.to_le_bytes());
  identity
}

fn current_work_selection(publisher: &V4FirstAuthorityPublisher) -> LoadedMutableSystemControlV1 {
  publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap()
}

struct FailingTaskReplacementObserver {
  previous: KVEntry,
  called: bool,
}

#[test]
fn native_task_work_compiler_pair_guard_cannot_cross_physical_publisher_instances() {
  use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    // A disposable byte-for-byte copy deliberately has identical persisted IDs.
    // Its different in-process publisher must not borrow the original's lock.
    let copied_directory = tempfile::tempdir().unwrap();
    let copied_path = copied_directory.path().join("copied-task-owner.aeordb");
    fs::copy(fixture.path, &copied_path).unwrap();
    let (_coordinator, copied) = reopen(&copied_path);
    let copied_protection = copied.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let capture =
      copied_protection.capture_semantic_mutation_inventory(input.inventory_bounds, fixture.memory, fixture.cancellation).unwrap();
    let old_identity = work_pair_identity(1);
    let identity = work_pair_identity(2);
    let mut checkpoint = fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &old_identity)
      .unwrap()
      .unwrap()
      .bytes;
    checkpoint[32 + 32..32 + 40].copy_from_slice(&2u64.to_le_bytes());
    crc(&mut checkpoint);
    let digest = digest_parts(HashAlgorithm::Blake3_256, &[&checkpoint]);
    let mut companion = fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &old_identity)
      .unwrap()
      .unwrap()
      .bytes;
    companion[32 + 32..32 + 40].copy_from_slice(&2u64.to_le_bytes());
    companion[32 + 112 + 5 * 32..32 + 112 + 6 * 32].copy_from_slice(&digest);
    crc(&mut companion);
    let controls = [
      ImmutableSystemControlWriteV1 {
        kind: SystemControlKindV1::SemanticMutationCheckpoint,
        identity: &identity,
        encoded_control: &checkpoint,
      },
      ImmutableSystemControlWriteV1 { kind: SystemControlKindV1::SemanticSourceCapture, identity: &identity, encoded_control: &companion },
    ];
    let memory = fixture.memory.reserve(MemoryOwner::Task, 16 << 20, AdmissionClass::Maintenance).unwrap();
    let original_before = fs::read(fixture.path).unwrap();
    let copied_before = fs::read(&copied_path).unwrap();
    let error = capture
      .stage_captured_work_controls(
        &controls,
        input.publication_timestamp_ms + 20,
        &memory,
        &work,
        (|| {}, &mut NoopFirstAuthorityDependencyObserverV1),
      )
      .unwrap_err();
    assert_eq!(error.code(), "semantic_task_work_publisher");
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), original_before);
    assert_eq!(fs::read(&copied_path).unwrap(), copied_before);
  });
}

impl FirstAuthorityDependencyObserverV1 for FailingTaskReplacementObserver {
  fn staged(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    self.called = true;
    assert!(entities.iter().any(|entity| entity.key == self.previous.hash));
    let visible = kv.snapshot_handle().load().get(&self.previous.hash).unwrap().unwrap();
    assert_eq!(visible, self.previous);
    assert!(kv.get_buffered(&self.previous.hash).is_some());
    Err(NativeDurabilityError::invalid(NativeDurabilityOperation::ReadBack, "injected task replacement before authority selection"))
  }
}

#[test]
fn native_task_work_compiler_late_cancel_keeps_committed_selection_receipt() {
  for cancel_retirement in [false, true] {
    with_initial_task_for_work(|fixture| {
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let attempt = if cancel_retirement { fixture.cancellation.clone() } else { CancellationToken::new() };
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, &attempt, fixture.retirement).unwrap();
      let mut observer = CancelRetirementAfterCommitObserver { cancellation: attempt.clone() };
      let result = work.start_compilation_observed(
        start_request(fixture.tree, input.publication_timestamp_ms + 20),
        fixture.retirement,
        || {},
        || {},
        &mut observer,
      );
      assert!(attempt.is_cancelled());
      assert_eq!(fixture.cancellation.is_cancelled(), cancel_retirement);
      let receipt = if cancel_retirement {
        let error = result.unwrap_err();
        assert_eq!(error.code(), "mutable_control_retirement_flush");
        assert!(error.committed_checkpoint_receipt().is_none());
        let receipt = error.committed_receipt().expect("late journal cancellation must retain task commitment").clone();
        assert!(receipt.retirement_hard_publication_sequence.is_none());
        receipt
      } else {
        let receipt = result.unwrap();
        assert!(receipt.retirement_hard_publication_sequence.is_some());
        receipt
      };
      assert_eq!(receipt.control_sequence, 3);
      assert_eq!(current_work_selection(fixture.publisher).control_digest, receipt.control_digest);
    });
  }
}

fn check_compiler_final_guards(at_pair: bool) {
  use crate::engine::memory_coordinator::HostMemorySample;
  for change in 0..9 {
    with_initial_task_for_work(|fixture| {
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let mut bytes_after_change = None;
      let mut change_authority = || {
        assert!(fixture.publisher.root_state.try_lock().is_ok());
        match change {
          0 => fixture.cancellation.cancel(),
          1 => {
            fixture.memory.update_host_sample(HostMemorySample { rss_bytes: 512 << 20, ..HostMemorySample::default() }).unwrap();
          }
          2..=5 => {
            let mut header = fixture.publisher.observe().unwrap().selected.header;
            header.slot_sequence += 1;
            match change {
              2 => header.physical_instance_id = [9; 16],
              3 => header.writer_fence_epoch += 1,
              4 => header.required_reader_capabilities[3] &= !0b0010,
              5 => header.required_writer_capabilities[3] &= !0b1000,
              _ => unreachable!(),
            }
            write_redundant_header(fixture.publisher, &header);
          }
          6 => {
            let generation = fixture
              .publisher
              .load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[])
              .unwrap()
              .unwrap();
            let mut bytes = generation.bytes;
            bytes[16..24].copy_from_slice(&(generation.control_sequence + 1).to_le_bytes());
            crc(&mut bytes);
            seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::B, &bytes)]);
          }
          7 => {
            let mut bytes = current_work_selection(fixture.publisher).bytes;
            bytes[16..24].copy_from_slice(&3u64.to_le_bytes());
            bytes[32 + 64..32 + 72].copy_from_slice(&3u64.to_le_bytes());
            crc(&mut bytes);
            seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &bytes)]);
          }
          8 => {
            fixture.publisher.root_state.lock().unwrap().staging_accounting_failed = true;
          }
          _ => unreachable!(),
        }
        bytes_after_change = Some(fs::read(fixture.path).unwrap());
      };
      let request = start_request(fixture.tree, input.publication_timestamp_ms + 20);
      let result = if at_pair {
        work.start_compilation_observed(
          request,
          fixture.retirement,
          &mut change_authority,
          || {},
          &mut NoopFirstAuthorityDependencyObserverV1,
        )
      } else {
        work.start_compilation_observed(
          request,
          fixture.retirement,
          || {},
          &mut change_authority,
          &mut NoopFirstAuthorityDependencyObserverV1,
        )
      };
      let error = result.unwrap_err();
      assert!(bytes_after_change.is_some(), "case {change} failed before its target boundary: {error}");
      assert!(error.committed_receipt().is_none());
      assert!(error.committed_checkpoint_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), bytes_after_change.unwrap());
      if change == 1 {
        fixture.memory.update_host_sample(HostMemorySample::default()).unwrap();
      }
      if change == 8 {
        fixture.publisher.root_state.lock().unwrap().staging_accounting_failed = false;
      }
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(current_work_selection(fixture.publisher).control_sequence, if change == 7 { 3 } else { 2 });
    });
  }
}

#[test]
fn native_task_work_compiler_pair_rechecks_all_final_guards() {
  check_compiler_final_guards(true);
}

#[test]
fn native_task_work_compiler_selection_rechecks_all_final_guards() {
  check_compiler_final_guards(false);
}

#[test]
fn native_task_work_compiler_pair_failure_retains_staging_receipt_not_task_commit() {
  for committed in [false, true] {
    let (_directory, path) = with_initial_task_for_work(|fixture| {
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let before = current_work_selection(fixture.publisher);
      let mut before_commit = FailingVisibilityObserver;
      let mut after_commit = FailingPostCommitObserver;
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if committed { &mut after_commit } else { &mut before_commit };
      let mut reached_pair = false;
      let error = work
        .start_compilation_with_observers(
          start_request(fixture.tree, input.publication_timestamp_ms + 20),
          fixture.retirement,
          || {
            reached_pair = true;
          },
          || panic!("pair failure must not reach task selection"),
          (observer, &mut NoopFirstAuthorityDependencyObserverV1),
        )
        .unwrap_err();
      assert!(reached_pair);
      assert!(error.committed_receipt().is_none());
      assert_eq!(error.committed_checkpoint_receipt().is_some(), committed);
      assert_eq!(current_work_selection(fixture.publisher), before);
    });
    let (_coordinator, publisher) = reopen(&path);
    assert_eq!(current_work_selection(&publisher).control_sequence, 2);
    for kind in [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture] {
      assert_eq!(publisher.load_immutable_system_control(kind, &[1; 16], &work_pair_identity(2)).unwrap().is_some(), committed);
    }
  }
}

#[test]
fn native_task_work_compiler_unsupported_captured_profiles_refuse_before_staging() {
  for fingerprint in [6usize, 7] {
    with_initial_task_for_work(|fixture| {
      let algorithm = HashAlgorithm::Blake3_256;
      let identity = work_pair_identity(1);
      let mut checkpoint = fixture
        .publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &identity)
        .unwrap()
        .unwrap()
        .bytes;
      checkpoint[200 + fingerprint * 32] ^= 0x80;
      crc(&mut checkpoint);
      let digest = digest_parts(algorithm, &[&checkpoint]);
      let mut companion = fixture
        .publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &identity)
        .unwrap()
        .unwrap()
        .bytes;
      companion[32 + 112 + 5 * 32..32 + 112 + 6 * 32].copy_from_slice(&digest);
      crc(&mut companion);
      let mut task = current_work_selection(fixture.publisher).bytes;
      task[32 + 112..32 + 144].copy_from_slice(&digest);
      crc(&mut task);
      seed(
        fixture.publisher,
        &[
          (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
          (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &companion),
          (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task),
        ],
      );
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let before = fs::read(fixture.path).unwrap();
      let error = work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap_err();
      assert!(error.to_string().contains("semantic_catalog_base_profile"), "{error}");
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), before);
      assert_eq!(current_work_selection(fixture.publisher).control_sequence, 2);
    });
  }
}

#[test]
fn native_task_work_compiler_orphan_reopen_resumes_existing_retirement_and_uses_fresh_pair() {
  let mut tree = Vec::new();
  let (_directory, path) = with_initial_task_for_work(|fixture| {
    tree.extend_from_slice(fixture.tree);
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    for sequence in [2, 3] {
      let observed = observe_work_task(&fixture);
      let attempt = CancellationToken::new();
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, &attempt, fixture.retirement).unwrap();
      assert_eq!(work.reserved_checkpoint_sequence(), sequence);
      let mut interrupted = false;
      let error = work
        .start_compilation_observed(
          start_request(fixture.tree, input.publication_timestamp_ms + 20),
          fixture.retirement,
          || {},
          || {
            interrupted = true;
            attempt.cancel();
          },
          &mut NoopFirstAuthorityDependencyObserverV1,
        )
        .unwrap_err();
      assert!(interrupted, "{error}");
      assert!(error.committed_receipt().is_none());
    }
    let task = current_work_selection(fixture.publisher);
    assert_eq!(task.control_sequence, 3);
    assert_eq!(decode_semantic_mutation_task(&task.bytes, HashAlgorithm::Blake3_256).unwrap().checkpoint_sequence, 1);
    assert!(fixture
      .publisher
      .reconstruct_retirement_journal_summary(fixture.cancellation, fixture.memory, 64, 64, 64, 4 << 20)
      .unwrap()
      .is_some());
  });
  let (_coordinator, publisher) = reopen(&path);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let summary = publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 64, 64, 64, 4 << 20).unwrap().unwrap();
  let mut retirement = RetirementJournalOwnerV1::resume_chain(
    HashAlgorithm::Blake3_256,
    [1; 16],
    &summary,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let orphan = publisher
    .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &work_pair_identity(3))
    .unwrap()
    .unwrap();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let observed = publisher
    .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
      database_id: &[1; 16],
      task_id: &[2; 16],
      memory: &memory,
      cancellation: &cancellation,
    })
    .unwrap();
  let input = work_request(observed.header().selected.header.updated_at_ms + 1);
  let work = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
  assert_eq!(work.reserved_checkpoint_sequence(), 4);
  assert_eq!(
    work.start_compilation(start_request(&tree, input.publication_timestamp_ms + 20), &mut retirement).unwrap().control_sequence,
    5
  );
  assert_eq!(
    publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &work_pair_identity(3))
      .unwrap()
      .unwrap(),
    orphan
  );
  let after = publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 64, 64, 64, 4 << 20).unwrap().unwrap();
  assert!(after.segment_count > summary.segment_count);
  assert!(after.last_segment_ordinal > summary.last_segment_ordinal);
  assert!(after.last_replacement_sequence > summary.last_replacement_sequence);
}

#[test]
fn native_task_work_compiler_invalid_requests_preserve_reserved_task_and_database() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let mut timestamp = fixture.publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    for case in 0..7 {
      let observed = observe_work_task(&fixture);
      let input = work_request(timestamp);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let valid = start_request(fixture.tree, timestamp + 20);
      let mut invalid = valid;
      match case {
        0 => invalid.publication_timestamp_ms = 0,
        1 => invalid.publication_timestamp_ms = i64::MAX as u64 + 1,
        2 => invalid.publication_timestamp_ms = timestamp - 1,
        3 => invalid.monotonic_now_ms = 0,
        4 => invalid.maximum_workspace_bytes = 1,
        5 => invalid.compiler_bounds.maximum_compiler_workspace_bytes = 1,
        6 => invalid.compiler_bounds.sources.catalog.maximum_work = 0,
        _ => unreachable!(),
      }
      let before = fs::read(fixture.path).unwrap();
      let selected = current_work_selection(fixture.publisher);
      let error = work.start_compilation(invalid, fixture.retirement).unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(current_work_selection(fixture.publisher), selected);
      assert_eq!(fs::read(fixture.path).unwrap(), before);
      timestamp += 100;
    }
    let observed = observe_work_task(&fixture);
    let work = protection
      .begin_semantic_task_work(&observed, work_request(timestamp), fixture.memory, fixture.cancellation, fixture.retirement)
      .unwrap();
    assert!(!work.start_compilation(start_request(fixture.tree, timestamp + 20), fixture.retirement).unwrap().idempotent);
  });
}

#[test]
fn native_task_work_compiler_stale_before_pair_cannot_publish_its_reserved_identity() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let mut replacement = current_work_selection(fixture.publisher).bytes;
    replacement[16..24].copy_from_slice(&3u64.to_le_bytes());
    replacement[32 + 64..32 + 72].copy_from_slice(&3u64.to_le_bytes());
    crc(&mut replacement);
    let mut after_replacement = None;
    let error = work
      .start_compilation_observed(
        start_request(fixture.tree, input.publication_timestamp_ms + 20),
        fixture.retirement,
        || {
          assert!(fixture.publisher.root_state.try_lock().is_ok());
          seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &replacement)]);
          after_replacement = Some(fs::read(fixture.path).unwrap());
        },
        || {},
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap_err();
    assert!(after_replacement.is_some());
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), after_replacement.unwrap());
    assert_eq!(current_work_selection(fixture.publisher).bytes, replacement);
    for kind in [SystemControlKindV1::SemanticMutationCheckpoint, SystemControlKindV1::SemanticSourceCapture] {
      assert!(fixture.publisher.load_immutable_system_control(kind, &[1; 16], &work_pair_identity(2)).unwrap().is_none());
    }
  });
}

#[test]
fn native_task_work_compiler_generation_change_after_pair_cannot_select_partial_authority() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let selected = current_work_selection(fixture.publisher);
    let generation =
      fixture.publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap().unwrap();
    let mut bytes = generation.bytes;
    bytes[16..24].copy_from_slice(&(generation.control_sequence + 1).to_le_bytes());
    crc(&mut bytes);
    let mut after_generation = None;
    let error = work
      .start_compilation_observed(
        start_request(fixture.tree, input.publication_timestamp_ms + 20),
        fixture.retirement,
        || {},
        || {
          assert!(fixture.publisher.root_state.try_lock().is_ok());
          seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::B, &bytes)]);
          after_generation = Some(fs::read(fixture.path).unwrap());
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap_err();
    assert!(after_generation.is_some());
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), after_generation.unwrap());
    assert_eq!(current_work_selection(fixture.publisher), selected);
    assert!(fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &work_pair_identity(2))
      .unwrap()
      .is_some());
  });
}

#[test]
fn native_task_work_compiler_abandoned_pair_preserves_old_retention_and_next_work_uses_fresh_identity() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let attempt_cancel = CancellationToken::new();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, &attempt_cancel, fixture.retirement).unwrap();
    let selected = current_work_selection(fixture.publisher);
    let mut interrupted = false;
    let error = work
      .start_compilation_observed(
        start_request(fixture.tree, input.publication_timestamp_ms + 20),
        fixture.retirement,
        || {},
        || {
          interrupted = true;
          attempt_cancel.cancel();
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap_err();
    assert!(interrupted);
    assert!(error.committed_receipt().is_none());
    assert_eq!(current_work_selection(fixture.publisher), selected);
    let orphan = fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &work_pair_identity(2))
      .unwrap()
      .unwrap();
    {
      let capture =
        protection.capture_semantic_mutation_inventory(work_request(1).inventory_bounds, fixture.memory, fixture.cancellation).unwrap();
      let mut retained = std::collections::BTreeSet::new();
      let summary = capture
        .visit_captured_semantic_task_retention_entries(
          NativeSemanticTaskRetentionBoundsV1 { maximum_work: 16384, maximum_read_bytes: 64 << 20, graphs: selection_bounds() },
          |entry| {
            retained.insert(entry.hash.clone());
            Ok(())
          },
        )
        .unwrap();
      assert!(summary.complete);
      assert_eq!(summary.tasks, 1);
      let old_path =
        system_control_path(SystemControlKindV1::SemanticMutationCheckpoint, &work_pair_identity(1), SystemControlSlotV1::Immutable)
          .unwrap();
      assert!(retained.contains(&first_authority_file_path_hash(&old_path, HashAlgorithm::Blake3_256)));
    }
    let current = observe_work_task(&fixture);
    let next_input = work_request(current.header().selected.header.updated_at_ms + 1);
    let next = protection.begin_semantic_task_work(&current, next_input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(next.reserved_checkpoint_sequence(), 3);
    next.start_compilation(start_request(fixture.tree, next_input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
    assert_eq!(
      fixture
        .publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &work_pair_identity(2))
        .unwrap()
        .unwrap()
        .bytes,
      orphan.bytes
    );
    assert_eq!(observe_work_task(&fixture).task().unwrap().unwrap().checkpoint_sequence, 3);
  });
}

#[test]
fn native_task_work_compiler_selection_preserves_precommit_and_postcommit_task_receipts() {
  for committed in [false, true] {
    with_initial_task_for_work(|fixture| {
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let old_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
      let old_key = first_authority_file_path_hash(&old_path, HashAlgorithm::Blake3_256);
      let previous = fixture.publisher.lock_kv().unwrap().get(&old_key).unwrap().unwrap();
      let mut before_commit = FailingTaskReplacementObserver { previous, called: false };
      let mut after_commit = FailingPostCommitObserver;
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if committed { &mut after_commit } else { &mut before_commit };
      let mut ready_to_select = false;
      let error = work
        .start_compilation_observed(
          start_request(fixture.tree, input.publication_timestamp_ms + 20),
          fixture.retirement,
          || {},
          || {
            ready_to_select = true;
          },
          observer,
        )
        .unwrap_err();
      assert!(ready_to_select);
      assert_eq!(before_commit.called, !committed);
      assert_eq!(error.committed_receipt().is_some(), committed);
      if let Some(receipt) = error.committed_receipt() {
        assert_eq!(receipt.control_sequence, 3);
      }
      let task = current_work_selection(fixture.publisher);
      assert_eq!(task.control_sequence, if committed { 3 } else { 2 });
      assert!(fixture
        .publisher
        .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &work_pair_identity(2))
        .unwrap()
        .is_some());
    });
  }
}
