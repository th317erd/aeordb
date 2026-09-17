//! Transactional, concurrent and final-admission source-staging regressions.
use super::*;

#[test]
fn source_staging_concurrent_validations_publish_once_and_refuse_the_stale_competitor() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-staging-concurrent", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  let before = publisher.observe().unwrap().selected.header;
  let (ready_sender, ready_receiver) = mpsc::channel();
  let (release_first, first_receiver) = mpsc::channel();
  let (release_second, second_receiver) = mpsc::channel();
  let results = std::thread::scope(|scope| {
    let run = |release: mpsc::Receiver<()>| {
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      source.stage_retained_copy_observed(
        source_bounds(),
        timestamp,
        || {
          ready_sender.send(()).unwrap();
          release.recv_timeout(Duration::from_secs(2)).unwrap();
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
    };
    let first = scope.spawn(move || run(first_receiver));
    let second = scope.spawn(move || run(second_receiver));
    let both_ready =
      ready_receiver.recv_timeout(Duration::from_secs(2)).is_ok() && ready_receiver.recv_timeout(Duration::from_secs(2)).is_ok();
    // Always release before joining/asserting, including a failed ready gate.
    let released_first = release_first.send(());
    let released_second = release_second.send(());
    let results = [first.join().unwrap(), second.join().unwrap()];
    assert!(both_ready);
    assert!(released_first.is_ok() && released_second.is_ok());
    results
  });
  assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
  for result in results {
    match result {
      Ok(receipt) => {
        assert!(!receipt.idempotent);
        assert_eq!(receipt.entities.len(), 1);
      }
      Err(error) => {
        assert_eq!(error.code(), "semantic_source_stage_changed");
        assert!(error.committed_receipt().is_none());
      }
    }
  }
  let after = publisher.observe().unwrap().selected.header;
  assert_eq!(after.entry_count, before.entry_count + 1);
  assert_eq!(after.write_sequence_high_water, before.write_sequence_high_water + 1);
  assert_eq!(after.head_hash, before.head_hash);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let bytes = fs::read(&path).unwrap();
  assert!(source.stage_retained_copy(source_bounds(), timestamp + 1).unwrap().idempotent);
  assert!(fs::read(&path).unwrap() == bytes);
}

#[test]
fn source_staging_rechecks_healthy_protection_after_unlocked_validation() {
  for accounting_failed in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-final-protection", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    let before = fs::read(&path).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let old_count = publisher.root_state.lock().unwrap().active_staging_protections;
    let result = source.stage_retained_copy_observed(
      source_bounds(),
      timestamp,
      || {
        let mut authority = publisher.root_state.lock().unwrap();
        if accounting_failed {
          authority.staging_accounting_failed = true;
        } else {
          authority.active_staging_protections = 0;
        }
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    );
    {
      // Restore the test-only mutation before the real guard can drop.
      let mut authority = publisher.root_state.lock().unwrap();
      authority.staging_accounting_failed = false;
      authority.active_staging_protections = old_count;
    }
    let error = result.unwrap_err();
    assert_eq!(error.code(), "semantic_source_stage_protection");
    assert!(error.committed_receipt().is_none());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(fs::read(&path).unwrap() == before);
    assert!(!source.stage_retained_copy(source_bounds(), timestamp).unwrap().idempotent);
  }
}

#[test]
fn source_staging_header_race_refuses_without_holding_root_or_kv_and_retry_succeeds() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("source-staging-race", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
  seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
  let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
  let mut raced_bytes = Vec::new();
  let error = source
    .stage_retained_copy_observed(
      source_bounds(),
      timestamp,
      || {
        assert!(publisher.root_state.try_lock().is_ok());
        assert!(publisher.kv.try_lock().is_ok());
        seed_files(&publisher, &[("/.aeordb-config/parsers.json".to_string(), "application/json", b"{}")]);
        raced_bytes = fs::read(&path).unwrap();
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap_err();
  assert_eq!(error.code(), "semantic_source_stage_changed");
  assert!(error.committed_receipt().is_none());
  assert!(publisher.locator(source.revision()).unwrap().is_none());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&path).unwrap(), raced_bytes);
  assert!(!source.stage_retained_copy(source_bounds(), timestamp + 1).unwrap().idempotent);
}

#[test]
fn source_staging_final_validation_cancellation_or_pressure_never_creates_a_record() {
  use crate::engine::memory_coordinator::HostMemorySample;
  for cancel in [false, true] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-final-admission", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::None);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let timestamp = publisher.observe().unwrap().selected.header.updated_at_ms + 1;
    let before = fs::read(&path).unwrap();
    let error = source
      .stage_retained_copy_observed(
        source_bounds(),
        timestamp,
        || {
          if cancel {
            cancellation.cancel();
          } else {
            memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
          }
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap_err();
    assert_eq!(error.code(), if cancel { "semantic_task_observation_cancelled" } else { "semantic_task_observation_memory" });
    assert!(error.committed_receipt().is_none());
    memory.update_host_sample(HostMemorySample::default()).unwrap();
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert_eq!(fs::read(&path).unwrap(), before);
    if !cancel {
      assert!(source.stage_retained_copy(source_bounds(), timestamp).is_ok());
    }
  }
}

#[test]
fn source_staging_preserves_uncommitted_and_committed_failure_receipts_and_commit_wins_cancel() {
  for case in 0..6 {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, path, coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-staging-transaction", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let original = seed_raw_source(&publisher, INDEX_SOURCE, 1, 1, 0, false, CompressionAlgorithm::Zstd);
    let revision = digest_parts(algorithm, &[b"filec:", &original]);
    {
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = publisher.observe().unwrap();
      let timestamp = before.selected.header.updated_at_ms + 1;
      let mut visibility = FailingVisibilityObserver;
      let mut post_commit = FailingPostCommitObserver;
      let mut cancel_after = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };
      let mut dependency = FailingDependencyObserver {
        phase: match case {
          3 => DependencyFailurePhase::BeforeEntity,
          4 => DependencyFailurePhase::EntityWritten,
          _ => DependencyFailurePhase::EntityStaged,
        },
        entity_index: 0,
      };
      let observer: &mut dyn FirstAuthorityDependencyObserverV1 = match case {
        0 => &mut visibility,
        1 => &mut post_commit,
        2 => &mut cancel_after,
        _ => &mut dependency,
      };
      let result = source.stage_retained_copy_observed(source_bounds(), timestamp, || {}, observer);
      if case == 2 {
        assert!(cancellation.is_cancelled());
        assert!(!result.unwrap().idempotent);
      } else {
        let error = result.unwrap_err();
        if case == 1 {
          assert_eq!(error.code(), "immutable_entity_committed_postcondition_failure");
          assert_eq!(error.committed_receipt().unwrap().entities[0].key, revision);
          assert!(publisher.locator(&revision).unwrap().is_some());
        } else {
          assert!(error.committed_receipt().is_none());
          assert_eq!(publisher.observe().unwrap(), before);
          assert!(publisher.locator(&revision).unwrap().is_none());
        }
        if case == 1 {
          assert!(source.stage_retained_copy(source_bounds(), timestamp + 1).unwrap().idempotent);
        } else {
          // A failed hard-authority dependency poisons the coordinator by
          // contract. In-process retry must remain refused; reopen below is
          // the recovery boundary, not clearing or bypassing hard_failure.
          assert_eq!(error.code(), "durability_failure");
          assert!(coordinator.hard_failure().unwrap().is_some());
          let retry = source.stage_retained_copy(source_bounds(), timestamp + 1).unwrap_err();
          assert_eq!(retry.code(), "durability_failure");
          assert!(retry.committed_receipt().is_none());
          assert_eq!(publisher.observe().unwrap(), before);
          assert!(publisher.locator(&revision).unwrap().is_none());
        }
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(publisher.observe().unwrap().selected.header.head_hash, before.selected.header.head_hash);
    }
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    if case != 1 && case != 2 {
      assert!(reopened.locator(&revision).unwrap().is_none());
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      let timestamp = reopened.observe().unwrap().selected.header.updated_at_ms + 1;
      assert!(!source.stage_retained_copy(source_bounds(), timestamp).unwrap().idempotent);
    }
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    assert_eq!(captured.read_retained_protected_source(INDEX_SOURCE, &revision, source_bounds()).unwrap().encoded_record(), original);
  }
}
