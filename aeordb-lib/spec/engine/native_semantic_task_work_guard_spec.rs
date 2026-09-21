//! Work acquisition must retain all final admission and receipt boundaries.
use super::*;

#[test]
fn native_task_work_late_cancellation_keeps_the_committed_reservation_receipt() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let mut late_cancel = CancelRetirementAfterCommitObserver { cancellation: fixture.cancellation.clone() };
    let work = protection
      .begin_semantic_task_work_observed(
        &observed,
        input,
        fixture.memory,
        fixture.cancellation,
        fixture.retirement,
        (|| {}, &mut late_cancel),
      )
      .unwrap();
    assert!(fixture.cancellation.is_cancelled());
    assert_eq!(work.receipt().control_sequence, 2);
    assert!(!work.receipt().idempotent);
    let selected =
      fixture.publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap();
    assert_eq!(selected.control_digest, work.receipt().control_digest);
    assert_eq!(selected.control_sequence, 2);
  });
}

#[test]
fn native_task_work_reserves_above_a_legal_checkpoint_larger_than_its_control() {
  with_initial_task_for_work(|fixture| {
    let algorithm = HashAlgorithm::Blake3_256;
    let mut old_identity = [0u8; 24];
    old_identity[..16].fill(2);
    old_identity[16..].copy_from_slice(&1u64.to_le_bytes());
    let mut next_identity = old_identity;
    next_identity[16..].copy_from_slice(&100u64.to_le_bytes());
    let mut checkpoint = fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticMutationCheckpoint, &[1; 16], &old_identity)
      .unwrap()
      .unwrap()
      .bytes;
    checkpoint[32 + 32..32 + 40].copy_from_slice(&100u64.to_le_bytes());
    crc(&mut checkpoint);
    let digest = digest_parts(algorithm, &[&checkpoint]);
    let mut companion = fixture
      .publisher
      .load_immutable_system_control(SystemControlKindV1::SemanticSourceCapture, &[1; 16], &old_identity)
      .unwrap()
      .unwrap()
      .bytes;
    companion[32 + 32..32 + 40].copy_from_slice(&100u64.to_le_bytes());
    companion[32 + 112 + 5 * 32..32 + 112 + 6 * 32].copy_from_slice(&digest);
    crc(&mut companion);
    let mut task =
      fixture.publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes;
    task[32 + 100..32 + 108].copy_from_slice(&100u64.to_le_bytes());
    task[32 + 112..32 + 144].copy_from_slice(&digest);
    crc(&mut task);
    seed(
      fixture.publisher,
      &[
        (SystemControlKindV1::SemanticMutationCheckpoint, &next_identity, SystemControlSlotV1::Immutable, &checkpoint),
        (SystemControlKindV1::SemanticSourceCapture, &next_identity, SystemControlSlotV1::Immutable, &companion),
        (SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &task),
      ],
    );
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    assert_eq!(observed.task().unwrap().unwrap().control_sequence, 1);
    assert_eq!(observed.checkpoint().unwrap().unwrap().checkpoint_sequence, 100);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(work.reserved_checkpoint_sequence(), 101);
    assert_eq!(work.receipt().control_sequence, 2);
    let current = observe_work_task(&fixture);
    assert_eq!(current.task().unwrap().unwrap().checkpoint_sequence, 100);
    assert_eq!(current.task().unwrap().unwrap().fencing_token, 101);
    work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
    let selected = observe_work_task(&fixture);
    assert_eq!(selected.task().unwrap().unwrap().control_sequence, 3);
    assert_eq!(selected.task().unwrap().unwrap().checkpoint_sequence, 101);
    assert_eq!(selected.task().unwrap().unwrap().fencing_token, 101);
  });
}

#[test]
fn native_task_work_explicit_fresh_observation_can_adopt_a_new_writer_epoch() {
  with_initial_task_for_work(|fixture| {
    let stale = observe_work_task(&fixture);
    let mut header = fixture.publisher.observe().unwrap().selected.header;
    header.slot_sequence += 1;
    header.writer_fence_epoch += 1;
    write_redundant_header(fixture.publisher, &header);
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let before = fs::read(fixture.path).unwrap();
    assert!(protection.begin_semantic_task_work(&stale, input, fixture.memory, fixture.cancellation, fixture.retirement).is_err());
    assert_eq!(fs::read(fixture.path).unwrap(), before);
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    let current = observe_work_task(&fixture);
    assert_eq!(current.task().unwrap().unwrap().writer_fence_epoch, header.writer_fence_epoch);
    assert!(current.checkpoint().unwrap().unwrap().writer_fence_epoch < header.writer_fence_epoch);
    assert_eq!(current.task().unwrap().unwrap().checkpoint_sequence, 1);
    let original_capture_epoch = current.checkpoint().unwrap().unwrap().writer_fence_epoch;
    work.start_compilation(start_request(fixture.tree, input.publication_timestamp_ms + 20), fixture.retirement).unwrap();
    let selected = observe_work_task(&fixture);
    assert_eq!(selected.task().unwrap().unwrap().writer_fence_epoch, header.writer_fence_epoch);
    assert_eq!(selected.checkpoint().unwrap().unwrap().writer_fence_epoch, original_capture_epoch);
    assert_eq!(selected.checkpoint().unwrap().unwrap().phase, SemanticMutationPhaseV1::Compiling);
  });
}

#[test]
fn native_task_work_missing_retained_base_or_companion_refuses_without_writes() {
  for companion in [false, true] {
    with_initial_task_for_work(|fixture| {
      let observed = observe_work_task(&fixture);
      let key = if companion {
        first_authority_file_path_hash(
          &system_control_path(SystemControlKindV1::SemanticSourceCapture, &checkpoint_identity(), SystemControlSlotV1::Immutable).unwrap(),
          HashAlgorithm::Blake3_256,
        )
      } else {
        observed.checkpoint().unwrap().unwrap().base_namespace_root.to_vec()
      };
      assert!(fixture.publisher.lock_kv().unwrap().mark_deleted(&key).unwrap());
      seed_files(fixture.publisher, &[]);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let before = fs::read(fixture.path).unwrap();
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let error =
        protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap_err();
      assert!(error.committed_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), before);
    });
  }
}

#[test]
fn native_task_work_final_cancel_and_pressure_refuse_new_and_exact_retry() {
  use crate::engine::memory_coordinator::HostMemorySample;
  for retry in [false, true] {
    for cancel in [false, true] {
      with_initial_task_for_work(|fixture| {
        let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
        let observed = observe_work_task(&fixture);
        let input = work_request(observed.header().selected.header.updated_at_ms + 1);
        if retry {
          drop(protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap());
        }
        let before = fs::read(fixture.path).unwrap();
        let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
        let mut callback = false;
        let error = protection
          .begin_semantic_task_work_observed(
            &observed,
            input,
            fixture.memory,
            fixture.cancellation,
            fixture.retirement,
            (
              || {
                callback = true;
                assert!(fixture.publisher.root_state.try_lock().is_ok());
                assert!(fixture.publisher.kv.try_lock().is_ok());
                if cancel {
                  fixture.cancellation.cancel();
                } else {
                  fixture.memory.update_host_sample(HostMemorySample { rss_bytes: 512 << 20, ..HostMemorySample::default() }).unwrap();
                }
              },
              &mut NoopFirstAuthorityDependencyObserverV1,
            ),
          )
          .unwrap_err();
        assert!(callback);
        assert_eq!(error.code(), if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" });
        assert!(error.committed_receipt().is_none());
        if !cancel {
          fixture.memory.update_host_sample(HostMemorySample::default()).unwrap();
        }
        assert_eq!(fs::read(fixture.path).unwrap(), before);
        assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
      });
    }
  }
}

#[test]
fn native_task_work_owner_generation_and_capabilities_guard_new_and_retry() {
  for retry in [false, true] {
    for change in 0..5 {
      with_initial_task_for_work(|fixture| {
        let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
        let observed = observe_work_task(&fixture);
        let input = work_request(observed.header().selected.header.updated_at_ms + 1);
        if retry {
          drop(protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap());
        }
        if change == 4 {
          let generation =
            fixture.publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[]).unwrap().unwrap();
          let mut bytes = generation.bytes;
          bytes[16..24].copy_from_slice(&(generation.control_sequence + 1).to_le_bytes());
          crc(&mut bytes);
          seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::B, &bytes)]);
        } else {
          let mut header = fixture.publisher.observe().unwrap().selected.header;
          header.slot_sequence += 1;
          match change {
            0 => header.physical_instance_id = [9; 16],
            1 => header.writer_fence_epoch += 1,
            2 => header.required_reader_capabilities[3] &= !0b0010,
            3 => header.required_writer_capabilities[3] &= !0b1000,
            _ => unreachable!(),
          }
          write_redundant_header(fixture.publisher, &header);
        }
        let before = fs::read(fixture.path).unwrap();
        let error =
          protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap_err();
        assert!(error.committed_receipt().is_none());
        assert_eq!(fs::read(fixture.path).unwrap(), before);
      });
    }
  }
}

#[test]
fn native_task_work_final_frontier_change_refuses_and_fresh_retry_progresses() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let mut after_other_publication = None;
    let error = protection
      .begin_semantic_task_work_observed(
        &observed,
        input,
        fixture.memory,
        fixture.cancellation,
        fixture.retirement,
        (
          || {
            assert!(fixture.publisher.root_state.try_lock().is_ok());
            assert!(fixture.publisher.kv.try_lock().is_ok());
            let mut next = successor_request(fixture.publisher, 0x95, "ordinary-after-work-capture");
            next.semantic_state = request_for_database_and_algorithm([1; 16], HashAlgorithm::Blake3_256).semantic_state;
            fixture.publisher.publish_successor_authority(&next).unwrap();
            after_other_publication = Some(fs::read(fixture.path).unwrap());
          },
          &mut NoopFirstAuthorityDependencyObserverV1,
        ),
      )
      .unwrap_err();
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), after_other_publication.unwrap());
    let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
    assert_eq!(work.reserved_checkpoint_sequence(), 2);
  });
}

#[test]
fn native_task_work_task_output_allocation_refuses_and_releases_memory() {
  with_initial_task_for_work(|fixture| {
    let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
    let observed = observe_work_task(&fixture);
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let before = fs::read(fixture.path).unwrap();
    let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
    assert!(!fixture.cancellation.is_cancelled());
    let (result, allocations) = measure(36 + 112 + 32, || {
      protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement)
    });
    assert!(allocations.injected_failure, "{allocations:?}");
    let error = result.unwrap_err();
    assert_eq!(error.code(), "system_control_output_allocation");
    assert!(error.committed_receipt().is_none());
    assert_eq!(fs::read(fixture.path).unwrap(), before);
    assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(
      protection
        .begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement)
        .unwrap()
        .reserved_checkpoint_sequence(),
      2
    );
  });
}

#[test]
fn native_task_work_publication_preserves_precommit_and_postcommit_receipts() {
  for committed in [false, true] {
    let (_directory, path) = with_initial_task_for_work(|fixture| {
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let input = work_request(observed.header().selected.header.updated_at_ms + 1);
      let mut before_commit = FailingVisibilityObserver;
      let mut after_commit = FailingPostCommitObserver;
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = if committed { &mut after_commit } else { &mut before_commit };
      let error = protection
        .begin_semantic_task_work_observed(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement, (|| {}, observer))
        .unwrap_err();
      assert_eq!(error.committed_receipt().is_some(), committed);
      if let Some(receipt) = error.committed_receipt() {
        assert_eq!(receipt.control_sequence, 2);
      }
      let current = observe_work_task(&fixture);
      assert_eq!(current.task().unwrap().unwrap().control_sequence, if committed { 2 } else { 1 });
    });
    // The injected durability failure latches the old writer. Its error is
    // deliberately not cleared; a real close/reopen owns subsequent work.
    let (_coordinator, publisher) = reopen(&path);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
    let cancellation = CancellationToken::new();
    let observed = publisher
      .observe_semantic_mutation_task(SemanticMutationObservationRequestV1 {
        database_id: &[1; 16],
        task_id: &[2; 16],
        memory: &memory,
        cancellation: &cancellation,
      })
      .unwrap();
    assert_eq!(observed.task().unwrap().unwrap().control_sequence, if committed { 2 } else { 1 });
    // Initial A and then B never replaced an existing slot, so no retirement
    // chain exists at this particular restart boundary.
    assert!(publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 16, 16, 1 << 20).unwrap().is_none());
    let mut retirement = selection_retirement(HashAlgorithm::Blake3_256, &memory, &cancellation);
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let input = work_request(observed.header().selected.header.updated_at_ms + 1);
    let recovered = protection.begin_semantic_task_work(&observed, input, &memory, &cancellation, &mut retirement).unwrap();
    assert_eq!(recovered.receipt().control_sequence, if committed { 3 } else { 2 });
    assert!(!recovered.receipt().idempotent);
  }
}
