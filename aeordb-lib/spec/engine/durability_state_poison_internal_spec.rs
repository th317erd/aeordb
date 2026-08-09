use super::*;

fn poison<T>(lock: &Mutex<T>) {
  let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _guard = lock.lock().unwrap();
    panic!("intentional durability-state poison");
  }));
}

#[test]
fn poisoned_runtime_failure_authority_closes_write_admission() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("runtime-failure.aeordb").to_str().unwrap()).unwrap();

  poison(&engine.durability_failure);

  let error = engine.ensure_writable().expect_err("unknown durability-failure state must close writes");
  assert!(error.to_string().contains("poison"));
  assert!(engine.durability_failure().is_some(), "health diagnostics must expose the fail-closed latch");
}

#[test]
fn poisoned_persistent_recovery_authority_closes_write_admission() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("persistent-recovery.aeordb").to_str().unwrap()).unwrap();

  poison(&engine.persistent_durability_recovery);

  assert!(engine.durability_failure().is_some(), "the first health read must expose the poisoned authority");
  let error = engine.ensure_writable().expect_err("unknown persistent recovery state must close writes");
  assert!(error.to_string().contains("poison"));
}

#[test]
fn poisoned_repair_owner_authority_closes_write_admission() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("repair-owner.aeordb").to_str().unwrap()).unwrap();

  poison(&engine.durability_repair_owner);

  let error = engine.ensure_writable().expect_err("unknown repair ownership must close writes");
  assert!(error.to_string().contains("poison"));
  assert!(engine.durability_failure().is_some(), "health diagnostics must expose the fail-closed latch");
}

#[test]
fn poisoned_emergency_spill_evidence_closes_write_admission() {
  let temporary = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(temporary.path().join("spill-evidence.aeordb").to_str().unwrap()).unwrap();

  poison(&engine.emergency_spill);
  let _ = engine.emergency_spill_report();

  let error = engine.ensure_writable().expect_err("unknown emergency-spill evidence must close writes");
  assert!(error.to_string().contains("poison"));
  assert!(engine.durability_failure().is_some(), "health diagnostics must expose the fail-closed latch");
}
