use super::*;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingFanout {
  acknowledgements: Mutex<Vec<NamespaceMutationAcknowledgement>>,
}

impl NamespaceMutationFanout for RecordingFanout {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement) {
    self.acknowledgements.lock().unwrap().push(acknowledgement.clone());
  }
}

fn test_engine(name: &str) -> (StorageEngine, tempfile::TempDir) {
  let temporary = tempfile::tempdir().unwrap();
  let spill = temporary.path().join("spill");
  std::fs::create_dir(&spill).unwrap();
  let database = temporary.path().join(format!("{name}.aeordb"));
  let overrides = crate::engine::config_resolver::CommandLineConfigOverrides::from_registered(BTreeMap::from([(
    "--recovery-emergency-spill-dir".to_string(),
    OsString::from(spill.as_os_str()),
  )]))
  .unwrap();
  let engine = StorageEngine::create_with_hot_dir_and_configuration_overrides(database.to_str().unwrap(), None, overrides).unwrap();
  (engine, temporary)
}

fn dependency_and_locator_batch(engine: &StorageEngine) -> (NamespaceMutationBatch, Vec<u8>, Vec<u8>) {
  let dependency_key = vec![0x41; engine.hash_algo().hash_length()];
  let locator_key = vec![0x42; engine.hash_algo().hash_length()];
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  batch.store_dependency(EntryType::FileRecord, dependency_key.clone(), b"dependency".to_vec(), 0).unwrap();
  batch.replace_locator(EntryType::FileRecord, locator_key.clone(), b"locator".to_vec(), 0).unwrap();
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: "/spec/faulted-file".to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: Some(locator_key.clone()),
    })
    .unwrap();
  (batch, dependency_key, locator_key)
}

#[test]
fn failure_after_dependency_append_emits_no_acknowledgement_and_latches_partial_mutation() {
  let (engine, _temporary) = test_engine("dependency-failure");
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_test_faults(
    &engine,
    fanout.clone(),
    NamespaceMutationTestFaults { fail_after_dependency_writes: Some(1), fail_hard_before_commit: false, panic_soft_handoff: false },
  );
  let (batch, dependency_key, locator_key) = dependency_and_locator_batch(&engine);

  let error = coordinator.execute(batch).expect_err("injected post-dependency failure must refuse acknowledgement");

  assert!(matches!(error, EngineError::DurabilityFailure(_)), "unexpected error: {error}");
  assert!(fanout.acknowledgements.lock().unwrap().is_empty());
  assert!(engine.get_kv_entry(&dependency_key).unwrap().is_some());
  assert!(engine.get_kv_entry(&locator_key).unwrap().is_none());
  assert!(engine.durability_failure().is_some());
  assert_eq!(engine.durability_snapshot().unwrap().pending_hard, 0);
}

#[test]
fn hard_failure_after_locator_mutation_emits_no_acknowledgement_and_latches_read_only() {
  let (engine, _temporary) = test_engine("hard-failure");
  let fanout = Arc::new(RecordingFanout::default());
  let coordinator = NamespaceMutationCoordinator::with_test_faults(
    &engine,
    fanout.clone(),
    NamespaceMutationTestFaults { fail_after_dependency_writes: None, fail_hard_before_commit: true, panic_soft_handoff: false },
  );
  let (batch, _dependency_key, locator_key) = dependency_and_locator_batch(&engine);
  let frontier_before = engine.durability_snapshot().unwrap().hard_frontier;

  let error = coordinator.execute(batch).expect_err("injected hard failure must refuse acknowledgement");

  assert!(matches!(error, EngineError::DurabilityFailure(_)), "unexpected error: {error}");
  assert!(fanout.acknowledgements.lock().unwrap().is_empty());
  assert!(engine.get_kv_entry(&locator_key).unwrap().is_some(), "volatile locator evidence must remain available for repair");
  assert_eq!(engine.durability_snapshot().unwrap().hard_frontier, frontier_before);
  assert_eq!(engine.durability_snapshot().unwrap().pending_hard, 0);
  assert!(engine.durability_failure().is_some());
  assert!(matches!(engine.ensure_writable(), Err(EngineError::DurabilityFailure(_))));
}

#[test]
fn engine_soft_handoff_is_not_emitted_for_failed_hard_publication() {
  let (engine, _temporary) = test_engine("hard-failure-soft-handoff");
  let coordinator = NamespaceMutationCoordinator::with_test_faults(
    &engine,
    Arc::new(RecordingFanout::default()),
    NamespaceMutationTestFaults { fail_after_dependency_writes: None, fail_hard_before_commit: true, panic_soft_handoff: false },
  );
  let (batch, _dependency_key, _locator_key) = dependency_and_locator_batch(&engine);
  let before = engine.soft_mutation_runtime_snapshot().unwrap();

  assert!(coordinator.execute(batch).is_err());

  let after = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(after.queued_notices, before.queued_notices);
  assert_eq!(after.dropped_notices, before.dropped_notices);
}

#[test]
fn migration_capture_is_not_emitted_for_failed_hard_publication() {
  let (engine, _temporary) = test_engine("hard-failure-migration-capture");
  let identity =
    crate::engine::v4::migration_capture_subscription::MigrationCaptureSubscriptionIdentityV1::new([0x71; 16], [0x72; 16], 7, [0x73; 16])
      .unwrap();
  let hub = Arc::new(
    crate::engine::v4::coverage_runtime::SoftMutationHubV1::new(
      crate::engine::v4::coverage_runtime::SoftMutationHubOptionsV1::new(2, 64 * 1_024, 16 * 1_024).unwrap(),
    )
    .unwrap(),
  );
  let subscription = engine.register_migration_capture_subscription(identity, hub).unwrap();
  let coordinator = NamespaceMutationCoordinator::with_test_faults(
    &engine,
    Arc::new(RecordingFanout::default()),
    NamespaceMutationTestFaults { fail_after_dependency_writes: None, fail_hard_before_commit: true, panic_soft_handoff: false },
  );
  let (batch, _dependency_key, _locator_key) = dependency_and_locator_batch(&engine);

  assert!(coordinator.execute(batch).is_err());

  let migration = subscription.snapshot().unwrap();
  assert_eq!(migration.queued_notices, 0);
  assert_eq!(migration.dropped_notices, 0);
  assert!(!migration.reconciliation_required);
}

#[test]
fn contended_engine_soft_handoff_latches_reconciliation_without_failing_commit() {
  let (engine, _temporary) = test_engine("contended-soft-handoff");
  let guard = engine.lock_soft_mutation_queue_for_test().unwrap();
  let (batch, _dependency_key, stable_key) = dependency_and_locator_batch(&engine);

  let acknowledgement =
    NamespaceMutationCoordinator::new(&engine).execute(batch).expect("recoverable-soft contention must not fail hard publication");
  drop(guard);

  assert!(engine.get_kv_entry(&stable_key).unwrap().is_some());
  let snapshot = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(snapshot.queued_notices, 0);
  assert_eq!(snapshot.lost_through_sequence, Some(acknowledgement.publication_sequence));
  assert!(snapshot.loss_reasons.contains(&crate::engine::v4::coverage_runtime::SoftMutationLossReasonV1::QueueContended));
}

#[test]
fn panicked_engine_soft_handoff_latches_reconciliation_without_failing_commit() {
  let (engine, _temporary) = test_engine("panicked-soft-handoff");
  let identity =
    crate::engine::v4::migration_capture_subscription::MigrationCaptureSubscriptionIdentityV1::new([0x81; 16], [0x82; 16], 8, [0x83; 16])
      .unwrap();
  let migration_hub = Arc::new(
    crate::engine::v4::coverage_runtime::SoftMutationHubV1::new(
      crate::engine::v4::coverage_runtime::SoftMutationHubOptionsV1::new(2, 64 * 1_024, 16 * 1_024).unwrap(),
    )
    .unwrap(),
  );
  let migration = engine.register_migration_capture_subscription(identity, migration_hub).unwrap();
  let coordinator = NamespaceMutationCoordinator::with_test_faults(
    &engine,
    Arc::new(RecordingFanout::default()),
    NamespaceMutationTestFaults { fail_after_dependency_writes: None, fail_hard_before_commit: false, panic_soft_handoff: true },
  );
  let (batch, _dependency_key, stable_key) = dependency_and_locator_batch(&engine);

  let acknowledgement = coordinator.execute(batch).expect("recoverable-soft panic must not fail hard publication");

  assert!(engine.get_kv_entry(&stable_key).unwrap().is_some());
  let snapshot = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(snapshot.queued_notices, 0);
  assert_eq!(snapshot.lost_through_sequence, Some(acknowledgement.publication_sequence));
  assert!(snapshot.loss_reasons.contains(&crate::engine::v4::coverage_runtime::SoftMutationLossReasonV1::QueueUnavailable));
  let migration_snapshot = migration.snapshot().unwrap();
  assert_eq!(migration_snapshot.queued_notices, 0);
  assert_eq!(migration_snapshot.lost_through_sequence, Some(acknowledgement.publication_sequence));
  assert!(migration_snapshot.loss_reasons.contains(&crate::engine::v4::coverage_runtime::SoftMutationLossReasonV1::QueueUnavailable));
}
