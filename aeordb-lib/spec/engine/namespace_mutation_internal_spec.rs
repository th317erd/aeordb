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
    NamespaceMutationTestFaults { fail_after_dependency_writes: Some(1), fail_hard_before_commit: false },
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
    NamespaceMutationTestFaults { fail_after_dependency_writes: None, fail_hard_before_commit: true },
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
