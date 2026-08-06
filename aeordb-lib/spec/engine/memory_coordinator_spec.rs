use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use aeordb::engine::memory_coordinator::{
  AdmissionClass, CriticalMemoryPurpose, HostMemorySample, MemoryCoordinator, MemoryCoordinatorError, MemoryObservation, MemoryOwner,
  MemoryPolicy, MemoryPressure,
};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};

fn policy() -> MemoryPolicy {
  MemoryPolicy::new(600, 800, 200, 100).unwrap()
}

#[test]
fn policy_rejects_impossible_soft_hard_and_emergency_relationships() {
  assert!(matches!(MemoryPolicy::new(0, 800, 200, 100), Err(MemoryCoordinatorError::InvalidPolicy(_))));
  assert!(matches!(MemoryPolicy::new(600, 0, 200, 100), Err(MemoryCoordinatorError::InvalidPolicy(_))));
  assert!(matches!(MemoryPolicy::new(600, 800, 0, 100), Err(MemoryCoordinatorError::InvalidPolicy(_))));
  assert!(matches!(MemoryPolicy::new(701, 800, 200, 100), Err(MemoryCoordinatorError::InvalidPolicy(_))));
  assert!(matches!(MemoryPolicy::new(600, 800, 200, 800), Err(MemoryCoordinatorError::InvalidPolicy(_))));
}

#[test]
fn owner_registry_is_complete_unique_and_stable() {
  let names = MemoryOwner::ALL.map(MemoryOwner::as_str);
  assert_eq!(
    names,
    [
      "kv_resident_pages",
      "kv_snapshot_generations",
      "kv_write_buffers",
      "durability_waiters",
      "directory_cache",
      "index_clean_cache",
      "index_dirty_buffers",
      "query",
      "streaming_read",
      "parser_plugin",
      "task",
      "garbage_collection",
      "migration",
      "backup_restore",
      "repair",
      "void_manager",
      "server_caches",
      "health_status",
      "emergency_spill",
      "shutdown",
    ]
  );
  assert_eq!(names.into_iter().collect::<std::collections::BTreeSet<_>>().len(), MemoryOwner::ALL.len());
}

#[test]
fn legacy_observations_reconcile_with_rss_and_reject_invalid_breakdowns() {
  let coordinator = MemoryCoordinator::new(policy());
  coordinator
    .observe_legacy(
      MemoryOwner::IndexCleanCache,
      MemoryObservation { resident_bytes: 200, clean_bytes: 200, evictable_bytes: 180, items: 4, ..Default::default() },
    )
    .unwrap();
  coordinator.update_host_sample(HostMemorySample { rss_bytes: 500, host_available_bytes: Some(1_000), ..Default::default() }).unwrap();

  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.accounted_bytes, 200);
  assert_eq!(snapshot.unaccounted_rss_bytes, 300);
  assert_eq!(snapshot.owner(MemoryOwner::IndexCleanCache).unwrap().observed.resident_bytes, 200);

  let invalid = MemoryObservation { resident_bytes: 10, clean_bytes: 8, dirty_bytes: 4, ..Default::default() };
  assert!(matches!(
    coordinator.observe_legacy(MemoryOwner::IndexCleanCache, invalid),
    Err(MemoryCoordinatorError::InvalidObservation { .. })
  ));
  let overflow = MemoryObservation { resident_bytes: u64::MAX, clean_bytes: u64::MAX, dirty_bytes: 1, ..Default::default() };
  assert!(matches!(
    coordinator.observe_legacy(MemoryOwner::IndexCleanCache, overflow),
    Err(MemoryCoordinatorError::InvalidObservation { .. })
  ));
  assert!(matches!(
    coordinator.observe_legacy(
      MemoryOwner::IndexCleanCache,
      MemoryObservation { resident_bytes: 10, clean_bytes: 5, evictable_bytes: 6, ..Default::default() }
    ),
    Err(MemoryCoordinatorError::InvalidObservation { .. })
  ));
  assert!(matches!(
    coordinator
      .observe_legacy(MemoryOwner::IndexCleanCache, MemoryObservation { resident_bytes: 10, pinned_bytes: 11, ..Default::default() }),
    Err(MemoryCoordinatorError::InvalidObservation { .. })
  ));
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().observed.resident_bytes, 200);
}

#[test]
fn soft_pressure_defers_cache_and_maintenance_but_not_bounded_workload() {
  let coordinator = MemoryCoordinator::new(policy());
  let workload = coordinator.reserve(MemoryOwner::Query, 600, AdmissionClass::Workload).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().pressure, MemoryPressure::Soft);

  assert!(matches!(
    coordinator.reserve(MemoryOwner::DirectoryCache, 1, AdmissionClass::Cache),
    Err(MemoryCoordinatorError::SoftPressureDeferred { .. })
  ));
  assert!(matches!(
    coordinator.reserve(MemoryOwner::GarbageCollection, 1, AdmissionClass::Maintenance),
    Err(MemoryCoordinatorError::SoftPressureDeferred { .. })
  ));
  let extra = coordinator.reserve(MemoryOwner::Query, 100, AdmissionClass::Workload).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().reserved_bytes, 700);

  drop(extra);
  drop(workload);
  assert_eq!(coordinator.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn ordinary_work_cannot_consume_emergency_headroom() {
  let coordinator = MemoryCoordinator::new(policy());
  let reservation = coordinator.reserve(MemoryOwner::Query, 700, AdmissionClass::Workload).unwrap();

  assert!(matches!(
    coordinator.reserve(MemoryOwner::Query, 1, AdmissionClass::Workload),
    Err(MemoryCoordinatorError::HardLimitExceeded { ordinary_limit_bytes: 700, .. })
  ));
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().rejections, 1);
  drop(reservation);
}

#[test]
fn emergency_headroom_is_bounded_and_restricted_to_matching_owners() {
  let coordinator = MemoryCoordinator::new(policy());
  assert!(matches!(
    coordinator.reserve(MemoryOwner::IndexCleanCache, 1, AdmissionClass::Critical(CriticalMemoryPurpose::HealthStatus)),
    Err(MemoryCoordinatorError::InvalidCriticalOwner { .. })
  ));

  let health = coordinator.reserve(MemoryOwner::HealthStatus, 60, AdmissionClass::Critical(CriticalMemoryPurpose::HealthStatus)).unwrap();
  let shutdown = coordinator.reserve(MemoryOwner::Shutdown, 40, AdmissionClass::Critical(CriticalMemoryPurpose::Shutdown)).unwrap();
  assert!(matches!(
    coordinator.reserve(MemoryOwner::EmergencySpill, 1, AdmissionClass::Critical(CriticalMemoryPurpose::EmergencySpill)),
    Err(MemoryCoordinatorError::EmergencyReserveExceeded { emergency_reserve_bytes: 100, .. })
  ));

  drop(health);
  drop(shutdown);
  assert_eq!(coordinator.snapshot().unwrap().critical_reserved_bytes, 0);
}

#[test]
fn every_critical_purpose_has_one_explicit_owner_path() {
  let coordinator = MemoryCoordinator::new(policy());
  let cases = [
    (MemoryOwner::KvWriteBuffers, CriticalMemoryPurpose::DurableWrite),
    (MemoryOwner::DurabilityWaiters, CriticalMemoryPurpose::DurableWrite),
    (MemoryOwner::IndexDirtyBuffers, CriticalMemoryPurpose::DurableWrite),
    (MemoryOwner::StreamingRead, CriticalMemoryPurpose::StreamingRead),
    (MemoryOwner::HealthStatus, CriticalMemoryPurpose::HealthStatus),
    (MemoryOwner::EmergencySpill, CriticalMemoryPurpose::EmergencySpill),
    (MemoryOwner::Shutdown, CriticalMemoryPurpose::Shutdown),
    (MemoryOwner::Repair, CriticalMemoryPurpose::BoundedRecovery),
  ];
  let reservations =
    cases.into_iter().map(|(owner, purpose)| coordinator.reserve(owner, 1, AdmissionClass::Critical(purpose)).unwrap()).collect::<Vec<_>>();
  assert_eq!(coordinator.snapshot().unwrap().critical_reserved_bytes, reservations.len() as u64);
}

#[test]
fn reservation_grow_shrink_failure_and_explicit_release_are_exact() {
  let coordinator = MemoryCoordinator::new(policy());
  let mut reservation = coordinator.reserve(MemoryOwner::Query, 500, AdmissionClass::Workload).unwrap();
  reservation.grow(150).unwrap();
  assert_eq!(reservation.bytes(), 650);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().active_reservations, 1);
  reservation.shrink(50).unwrap();
  assert_eq!(reservation.bytes(), 600);
  assert!(matches!(reservation.grow(101), Err(MemoryCoordinatorError::HardLimitExceeded { .. })));
  assert_eq!(reservation.bytes(), 600, "a failed grow must not alter accounting");
  assert!(matches!(reservation.shrink(601), Err(MemoryCoordinatorError::InvalidShrink { .. })));
  assert_eq!(reservation.bytes(), 600, "a failed shrink must not alter accounting");
  reservation.release().unwrap();
  assert_eq!(coordinator.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn host_available_floor_pauses_maintenance_without_relabeling_results() {
  let coordinator = MemoryCoordinator::new(policy());
  coordinator.update_host_sample(HostMemorySample { rss_bytes: 100, host_available_bytes: Some(199), ..Default::default() }).unwrap();
  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.pressure, MemoryPressure::Soft);
  assert!(snapshot.maintenance_paused);
  assert!(matches!(
    coordinator.reserve(MemoryOwner::Task, 1, AdmissionClass::Maintenance),
    Err(MemoryCoordinatorError::SoftPressureDeferred { .. })
  ));
  let workload = coordinator.reserve(MemoryOwner::Query, 1, AdmissionClass::Workload).unwrap();
  drop(workload);
}

#[test]
fn rss_hard_pressure_rejects_ordinary_work_but_preserves_emergency_paths() {
  let coordinator = MemoryCoordinator::new(policy());
  coordinator.update_host_sample(HostMemorySample { rss_bytes: 800, host_available_bytes: Some(1_000), ..Default::default() }).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().pressure, MemoryPressure::Hard);
  assert!(matches!(
    coordinator.reserve(MemoryOwner::Query, 1, AdmissionClass::Workload),
    Err(MemoryCoordinatorError::HardLimitExceeded { .. })
  ));
  let health = coordinator.reserve(MemoryOwner::HealthStatus, 1, AdmissionClass::Critical(CriticalMemoryPurpose::HealthStatus)).unwrap();
  drop(health);
}

#[test]
fn unconfigured_coordinator_still_reports_observations_but_refuses_admission() {
  let coordinator = MemoryCoordinator::without_policy();
  coordinator.observe_legacy(MemoryOwner::VoidManager, MemoryObservation { resident_bytes: 25, items: 2, ..Default::default() }).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().accounted_bytes, 25);
  assert_eq!(coordinator.snapshot().unwrap().policy_error.as_deref(), Some("memory policy was not resolved"));
  assert!(matches!(coordinator.reserve(MemoryOwner::Query, 1, AdmissionClass::Workload), Err(MemoryCoordinatorError::PolicyUnavailable)));
}

#[test]
fn concurrent_reservations_never_cross_the_ordinary_ceiling() {
  let coordinator = Arc::new(MemoryCoordinator::new(policy()));
  let start = Arc::new(Barrier::new(17));
  let admitted = Arc::new(Barrier::new(17));
  let release = Arc::new(Barrier::new(17));
  let successes = Arc::new(AtomicUsize::new(0));
  let mut threads = Vec::new();

  for _ in 0..16 {
    let coordinator = Arc::clone(&coordinator);
    let start = Arc::clone(&start);
    let admitted = Arc::clone(&admitted);
    let release = Arc::clone(&release);
    let successes = Arc::clone(&successes);
    threads.push(std::thread::spawn(move || {
      start.wait();
      let reservation = coordinator.reserve(MemoryOwner::Query, 100, AdmissionClass::Workload).ok();
      if reservation.is_some() {
        successes.fetch_add(1, Ordering::Relaxed);
      }
      admitted.wait();
      release.wait();
      drop(reservation);
    }));
  }

  start.wait();
  admitted.wait();
  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.reserved_bytes, successes.load(Ordering::Relaxed) as u64 * 100);
  assert!(snapshot.reserved_bytes <= policy().ordinary_limit_bytes());
  release.wait();
  for thread in threads {
    thread.join().unwrap();
  }
  assert_eq!(coordinator.snapshot().unwrap().reserved_bytes, 0);
}

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let path = directory.path().join("memory.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

#[test]
fn engine_policy_is_derived_from_the_same_immutable_configuration_shadow() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let resolution = engine.configuration_shadow().resolution.as_ref().unwrap().clone();
  let unsigned = |path: &str| match resolution.property(path).unwrap().value.as_ref().unwrap() {
    aeordb::engine::config_resolver::ConfigValue::Unsigned(value) => *value,
    value => panic!("{path} resolved to {value:?}"),
  };

  let snapshot = engine.memory_coordinator_snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  assert_eq!(policy.soft_limit_bytes, unsigned("memory.soft_limit_bytes"));
  assert_eq!(policy.hard_limit_bytes, unsigned("memory.hard_limit_bytes"));
  assert_eq!(policy.host_available_floor_bytes, unsigned("memory.host_available_floor_bytes"));
  assert_eq!(policy.emergency_reserve_bytes, unsigned("memory.emergency_reserve_bytes"));
}

#[test]
fn malformed_runtime_keeps_coordinator_observability_without_inventing_a_policy() {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("memory.aeordb");
  let engine = create_engine(&directory);
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), aeordb::engine::config_resolver::RUNTIME_CONFIG_PATH, b"{", Some("application/json"))
    .unwrap();
  engine.shutdown().unwrap();
  drop(engine);

  let engine = StorageEngine::open(path.to_str().unwrap()).unwrap();
  let snapshot = engine.memory_coordinator_snapshot().unwrap();
  assert!(snapshot.policy.is_none());
  assert!(snapshot.accounted_bytes > 0);
  assert_eq!(snapshot.pressure, MemoryPressure::Unconfigured);
}

#[test]
fn engine_observation_adapter_attributes_current_material_owners() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/folder/file.txt", &vec![b'x'; 512 * 1024], Some("text/plain")).unwrap();
  assert_eq!(ops.read_file_buffered("/folder/file.txt").unwrap().len(), 512 * 1024);

  let snapshot = engine.memory_coordinator_snapshot().unwrap();
  assert_eq!(snapshot.owners.len(), MemoryOwner::ALL.len());
  let resident_pages = snapshot.owner(MemoryOwner::KvResidentPages).unwrap();
  assert_eq!(resident_pages.observed.resident_bytes, 0, "bounded pages are coordinator reservations, not legacy observation");
  assert!(resident_pages.reserved_bytes > 0);
  assert!(snapshot.owner(MemoryOwner::KvSnapshotGenerations).unwrap().observed.resident_bytes > 0);
  assert!(snapshot.owner(MemoryOwner::KvWriteBuffers).unwrap().observed.resident_bytes > 0);
  let directory_cache = snapshot.owner(MemoryOwner::DirectoryCache).unwrap();
  assert_eq!(directory_cache.observed.resident_bytes, 0, "bounded directory entries must not be double-counted as legacy observation");
  assert!(directory_cache.reserved_bytes > 0);
  assert!(snapshot.owner(MemoryOwner::DurabilityWaiters).unwrap().observed.items > 0);
  if cfg!(target_os = "linux") {
    assert!(snapshot.host.rss_bytes > 0);
    assert!(snapshot.host.host_available_bytes.is_some());
  }
  assert_eq!(snapshot.unaccounted_rss_bytes, snapshot.host.rss_bytes.saturating_sub(snapshot.accounted_bytes));
}
