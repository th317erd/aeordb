use aeordb::engine::integrity_scanner::run_integrity_scan_cycle;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::{EngineError, EntryType};
use aeordb::server::create_temp_engine_for_tests;
use tokio_util::sync::CancellationToken;

fn store_entries(engine: &aeordb::engine::StorageEngine, count: usize) {
  for index in 0..count {
    let key = engine.compute_hash(format!("integrity-entry-{index}").as_bytes()).unwrap();
    engine.store_entry(EntryType::Chunk, &key, &[index as u8]).unwrap();
  }
}

#[test]
fn integrity_scan_uses_a_bounded_sample_and_releases_repair_memory() {
  let (engine, _temp) = create_temp_engine_for_tests();
  store_entries(&engine, 2_000);
  let coordinator = engine.memory_coordinator();
  let before = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();

  let result = run_integrity_scan_cycle(&engine, &CancellationToken::new()).unwrap();

  assert!(result.checked > 0);
  assert!(result.checked <= 1_000);
  assert_eq!(result.failures, 0);
  let after = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
  assert!(after.peak_reserved_bytes > before.peak_reserved_bytes);
  assert_eq!(after.reserved_bytes, before.reserved_bytes);
  assert_eq!(after.active_reservations, before.active_reservations);
}

#[test]
fn integrity_scan_defers_before_sampling_under_soft_pressure() {
  let (engine, _temp) = create_temp_engine_for_tests();
  store_entries(&engine, 20);
  let coordinator = engine.memory_coordinator();
  let before = coordinator.snapshot().unwrap();
  let policy = before.policy.expect("test engine must have a resolved memory policy");
  let pressure_bytes = policy.soft_limit_bytes.saturating_sub(before.accounted_bytes);
  let _pressure = coordinator.reserve(MemoryOwner::Query, pressure_bytes, AdmissionClass::Workload).unwrap();

  let error = run_integrity_scan_cycle(&engine, &CancellationToken::new()).expect_err("soft pressure must defer maintenance scanning");

  assert!(matches!(error, EngineError::ResourceExhausted(_)));
  let after = coordinator.snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
  assert!(after.deferrals > 0);
  assert_eq!(after.reserved_bytes, 0);
  assert_eq!(after.active_reservations, 0);
}

#[test]
fn integrity_scan_honors_pre_cancel_before_reserving_memory() {
  let (engine, _temp) = create_temp_engine_for_tests();
  store_entries(&engine, 20);
  let cancellation = CancellationToken::new();
  cancellation.cancel();

  let error = run_integrity_scan_cycle(&engine, &cancellation).expect_err("cancelled integrity scan must not start");

  assert!(matches!(error, EngineError::Cancelled(operation) if operation == "integrity scan"));
  let owner = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Repair).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}
