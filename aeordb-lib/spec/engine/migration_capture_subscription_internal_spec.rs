use super::*;

use std::collections::BTreeMap;
use std::ffi::OsString;

use crate::engine::entry_type::EntryType;
use crate::engine::namespace_mutation::{
  NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationKind, NamespaceMutationSourceIdentity,
};
use crate::engine::v4::coverage_runtime::{SoftMutationHubOptionsV1, SoftMutationHubV1, SoftMutationLossReasonV1};
use crate::engine::v4::migration_capture_subscription::MigrationCaptureSubscriptionIdentityV1;

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

fn identity(seed: u8, fencing_token: u64) -> MigrationCaptureSubscriptionIdentityV1 {
  MigrationCaptureSubscriptionIdentityV1::new([seed; 16], [seed.wrapping_add(1); 16], fencing_token, [seed.wrapping_add(2); 16]).unwrap()
}

fn hub(maximum_notices: usize) -> Arc<SoftMutationHubV1> {
  Arc::new(SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(maximum_notices, 64 * 1_024, 16 * 1_024).unwrap()).unwrap())
}

fn batch(seed: u8, path: &str) -> (NamespaceMutationBatch, Vec<u8>) {
  let dependency_key = vec![seed; 32];
  let locator_key = vec![seed.wrapping_add(64); 32];
  let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::FileWrite);
  batch.store_dependency(EntryType::FileRecord, dependency_key, vec![seed; 64], 0).unwrap();
  batch.replace_locator(EntryType::FileRecord, locator_key.clone(), vec![seed.wrapping_add(1); 48], 0).unwrap();
  batch
    .add_source_identity(NamespaceMutationSourceIdentity {
      path: path.to_string(),
      entry_type: Some(EntryType::FileRecord.to_u8()),
      previous_identity: None,
      new_identity: Some(locator_key.clone()),
    })
    .unwrap();
  (batch, locator_key)
}

#[test]
fn registration_observes_one_exact_boundary_and_both_independent_queues_receive_future_commits() {
  let (engine, _temporary) = test_engine("capture-registration");
  let expected_root = engine.head_hash().unwrap();
  let expected_frontier = engine.durability_snapshot().unwrap().hard_frontier;
  let primary_before = engine.soft_mutation_runtime_snapshot().unwrap();
  let subscription = engine.register_migration_capture_subscription(identity(1, 7), hub(4)).unwrap();

  assert_eq!(subscription.boundary().source_namespace_root, expected_root);
  assert_eq!(subscription.boundary().publication_sequence, expected_frontier);

  let (batch, locator_key) = batch(1, "/captured.json");
  let acknowledgement = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap();
  assert!(engine.get_kv_entry(&locator_key).unwrap().is_some());

  let migration = subscription.snapshot().unwrap();
  let primary = engine.soft_mutation_runtime_snapshot().unwrap();
  assert_eq!(migration.queued_notices, 1);
  assert_eq!(migration.latest_queued_publication_sequence, Some(acknowledgement.publication_sequence));
  assert_eq!(primary.queued_notices, primary_before.queued_notices + 1);

  let drained = subscription.try_drain(4, 64 * 1_024).unwrap();
  assert_eq!(drained.notices.len(), 1);
  assert_eq!(engine.soft_mutation_runtime_snapshot().unwrap().queued_notices, primary_before.queued_notices + 1);
}

#[test]
fn full_migration_queue_preserves_source_success_and_primary_index_handoff() {
  let (engine, _temporary) = test_engine("capture-full");
  let subscription = engine.register_migration_capture_subscription(identity(2, 8), hub(1)).unwrap();
  let primary_before = engine.soft_mutation_runtime_snapshot().unwrap().queued_notices;

  let (first, first_locator) = batch(2, "/first.json");
  let first_ack = NamespaceMutationCoordinator::new(&engine).execute(first).unwrap();
  let (second, second_locator) = batch(3, "/second.json");
  let second_ack = NamespaceMutationCoordinator::new(&engine).execute(second).unwrap();

  assert!(engine.get_kv_entry(&first_locator).unwrap().is_some());
  assert!(engine.get_kv_entry(&second_locator).unwrap().is_some());
  assert!(second_ack.publication_sequence > first_ack.publication_sequence);
  let migration = subscription.snapshot().unwrap();
  assert_eq!(migration.queued_notices, 1);
  assert_eq!(migration.lost_through_sequence, Some(second_ack.publication_sequence));
  assert!(migration.loss_reasons.contains(&SoftMutationLossReasonV1::QueueFull));
  assert_eq!(engine.soft_mutation_runtime_snapshot().unwrap().queued_notices, primary_before + 2);
}

#[test]
fn contended_migration_queue_preserves_source_success_and_primary_index_handoff() {
  let (engine, _temporary) = test_engine("capture-contended");
  let subscription = engine.register_migration_capture_subscription(identity(3, 9), hub(2)).unwrap();
  let primary_before = engine.soft_mutation_runtime_snapshot().unwrap().queued_notices;
  let migration_guard = subscription.lock_queue_for_test().unwrap();
  let (batch, locator_key) = batch(4, "/contended.json");

  let acknowledgement = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap();
  drop(migration_guard);

  assert!(engine.get_kv_entry(&locator_key).unwrap().is_some());
  let migration = subscription.snapshot().unwrap();
  assert_eq!(migration.queued_notices, 0);
  assert_eq!(migration.lost_through_sequence, Some(acknowledgement.publication_sequence));
  assert!(migration.loss_reasons.contains(&SoftMutationLossReasonV1::QueueContended));
  assert_eq!(engine.soft_mutation_runtime_snapshot().unwrap().queued_notices, primary_before + 1);
}

#[test]
fn panicked_migration_sink_preserves_source_success_and_primary_index_handoff() {
  let (engine, _temporary) = test_engine("capture-panic");
  let subscription = engine.register_migration_capture_subscription(identity(4, 10), hub(2)).unwrap();
  let primary_before = engine.soft_mutation_runtime_snapshot().unwrap().queued_notices;
  subscription.panic_on_next_offer_for_test();
  let (batch, locator_key) = batch(5, "/panic.json");

  let acknowledgement = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap();

  assert!(engine.get_kv_entry(&locator_key).unwrap().is_some());
  let migration = subscription.snapshot().unwrap();
  assert_eq!(migration.queued_notices, 0);
  assert_eq!(migration.lost_through_sequence, Some(acknowledgement.publication_sequence));
  assert!(migration.loss_reasons.contains(&SoftMutationLossReasonV1::QueueUnavailable));
  assert_eq!(engine.soft_mutation_runtime_snapshot().unwrap().queued_notices, primary_before + 1);
}

#[test]
fn oversized_migration_notice_preserves_source_success_and_primary_index_handoff() {
  let (engine, _temporary) = test_engine("capture-oversized");
  let tiny = Arc::new(SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(2, 4_096, 128).unwrap()).unwrap());
  let subscription = engine.register_migration_capture_subscription(identity(8, 14), tiny).unwrap();
  let primary_before = engine.soft_mutation_runtime_snapshot().unwrap().queued_notices;
  let path = format!("/{}", "large-path".repeat(128));
  let (batch, locator_key) = batch(8, &path);

  let acknowledgement = NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap();

  assert!(engine.get_kv_entry(&locator_key).unwrap().is_some());
  let migration = subscription.snapshot().unwrap();
  assert_eq!(migration.queued_notices, 0);
  assert_eq!(migration.lost_through_sequence, Some(acknowledgement.publication_sequence));
  assert!(migration.loss_reasons.contains(&SoftMutationLossReasonV1::NoticeTooLarge));
  assert_eq!(engine.soft_mutation_runtime_snapshot().unwrap().queued_notices, primary_before + 1);
}

#[test]
fn poisoned_migration_queue_preserves_source_success_and_remains_explicitly_retirable() {
  let (engine, _temporary) = test_engine("capture-poisoned");
  let selected = identity(9, 15);
  let subscription = engine.register_migration_capture_subscription(selected, hub(2)).unwrap();
  let queue_guard = subscription.lock_queue_for_test().unwrap();
  let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _queue_guard = queue_guard;
    panic!("inject migration queue poison");
  }));
  assert!(poisoned.is_err());
  let primary_before = engine.soft_mutation_runtime_snapshot().unwrap().queued_notices;
  let (batch, locator_key) = batch(9, "/poisoned.json");

  NamespaceMutationCoordinator::new(&engine).execute(batch).unwrap();

  assert!(engine.get_kv_entry(&locator_key).unwrap().is_some());
  assert_eq!(engine.soft_mutation_runtime_snapshot().unwrap().queued_notices, primary_before + 1);
  let retired = engine.unregister_migration_capture_subscription(selected).unwrap();
  assert!(matches!(retired.close_error, Some(crate::engine::v4::coverage_runtime::SoftMutationHubErrorV1::QueueUnavailable)));
}

#[test]
fn exact_owner_removal_closes_only_the_selected_subscription() {
  let (engine, _temporary) = test_engine("capture-owner");
  let selected = identity(5, 11);
  let foreign = identity(6, 12);
  let subscription = engine.register_migration_capture_subscription(selected, hub(2)).unwrap();

  assert_eq!(
    engine.register_migration_capture_subscription(foreign, hub(2)).unwrap_err().code(),
    "migration_capture_subscription_already_registered"
  );
  assert_eq!(engine.unregister_migration_capture_subscription(foreign).unwrap_err().code(), "migration_capture_subscription_owner_fenced");
  assert!(!subscription.snapshot().unwrap().admission_closed);

  let retired = engine.unregister_migration_capture_subscription(selected).unwrap();
  assert!(Arc::ptr_eq(&retired.subscription, &subscription));
  assert!(retired.close_error.is_none());
  assert!(subscription.snapshot().unwrap().admission_closed);
  assert_eq!(
    engine.unregister_migration_capture_subscription(selected).unwrap_err().code(),
    "migration_capture_subscription_not_registered"
  );

  let replacement_identity =
    MigrationCaptureSubscriptionIdentityV1::new(selected.migration_id, selected.holder_boot_id, selected.fencing_token, [0xef; 16])
      .unwrap();
  let replacement = engine.register_migration_capture_subscription(replacement_identity, hub(2)).unwrap();
  assert!(!Arc::ptr_eq(&replacement, &subscription));
  assert_eq!(engine.unregister_migration_capture_subscription(selected).unwrap_err().code(), "migration_capture_subscription_owner_fenced");
  assert!(!replacement.snapshot().unwrap().admission_closed);
}

#[test]
fn concurrent_registration_selects_exactly_one_subscription_owner() {
  let (engine, _temporary) = test_engine("capture-concurrent-registration");
  let engine = Arc::new(engine);
  let barrier = Arc::new(std::sync::Barrier::new(3));
  let mut workers = Vec::new();
  for seed in [10u8, 20u8] {
    let engine = Arc::clone(&engine);
    let barrier = Arc::clone(&barrier);
    workers.push(std::thread::spawn(move || {
      let selected = identity(seed, u64::from(seed));
      barrier.wait();
      (selected, engine.register_migration_capture_subscription(selected, hub(2)))
    }));
  }
  barrier.wait();

  let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
  let winners: Vec<_> = results.iter().filter(|(_, result)| result.is_ok()).collect();
  let losers: Vec<_> = results.iter().filter(|(_, result)| result.is_err()).collect();
  assert_eq!(winners.len(), 1);
  assert_eq!(losers.len(), 1);
  assert_eq!(losers[0].1.as_ref().unwrap_err().code(), "migration_capture_subscription_already_registered");
  engine.unregister_migration_capture_subscription(winners[0].0).unwrap();
}
