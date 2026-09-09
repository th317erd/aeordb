use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    self.0.lock().unwrap().extend_from_slice(bytes);
    Ok(bytes.len())
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

fn capture(action: impl FnOnce()) -> Vec<serde_json::Value> {
  let output = Arc::new(Mutex::new(Vec::new()));
  let writer = output.clone();
  let subscriber = tracing_subscriber::fmt().json().without_time().with_writer(move || CapturedWriter(writer.clone())).finish();
  tracing::subscriber::with_default(subscriber, action);
  let bytes = output.lock().unwrap().clone();
  String::from_utf8(bytes)
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
    .filter(|event| event["target"] == "aeordb::maintenance_progress")
    .map(|event| event["fields"].clone())
    .collect()
}

#[test]
fn verification_reports_each_phase_and_real_cache_activity() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  DirectoryOps::new(&engine).store_file_buffered(&crate::engine::RequestContext::system(), "/progress.txt", b"progress", None).unwrap();
  let events = capture(|| {
    verify_checked(&engine, engine.database_path().to_str().unwrap()).unwrap();
  });
  for phase in [
    "hot_tail_voids",
    "wal_entries",
    "expected_merge",
    "kv_entries",
    "actual_merge",
    "kv_compare",
    "directories",
    "path_file_records",
    "snapshots",
  ] {
    let selected: Vec<_> = events.iter().filter(|event| event["phase"] == phase).collect();
    assert_eq!(selected.len(), 2, "missing start/completion for {phase}: {events:?}");
    assert_eq!(selected[0]["status"], "started");
    assert_eq!(selected[1]["status"], "completed");
    assert!(selected[1]["units"].is_u64());
    assert!(selected[1]["elapsed_ms"].is_u64());
    assert!(selected[1]["cache_hits"].is_u64(), "cache telemetry is missing");
    assert!(selected[1]["cache_eviction_candidates"].is_u64());
  }
  let directories = events.iter().find(|event| event["phase"] == "directories" && event["status"] == "completed").unwrap();
  assert!(directories["units"].as_u64().unwrap() >= 2, "directory and child visits must advance logical work");
}

#[test]
fn repair_reports_stale_locator_publication_and_final_verification() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&crate::engine::RequestContext::system(), "/nested/file.txt", b"retained", None).unwrap();
  let root_key = crate::engine::directory_ops::directory_path_hash("/", &engine.hash_algo()).unwrap();
  engine.store_entry(EntryType::DirectoryIndex, &root_key, &vec![0x5a; engine.hash_algo().hash_length()]).unwrap();
  let events = capture(|| {
    let report = verify_and_repair_checked(&engine, engine.database_path().to_str().unwrap()).unwrap();
    assert!(report.stale_dir_path_keys.is_empty());
    assert!(report.repairs.iter().any(|message| message == "Repairs durably published."));
  });
  for phase in ["repair_actions", "repair_stale_locators", "final_verification"] {
    let selected: Vec<_> = events.iter().filter(|event| event["phase"] == phase).collect();
    assert_eq!(selected.len(), 2, "{phase}: {events:?}");
    assert_eq!(selected[0]["status"], "started");
    assert_eq!(selected[1]["status"], "completed");
    assert_eq!(selected[1]["units"], 1);
  }
  assert!(!events.iter().any(|event| event["status"] == "interrupted"));
}

#[test]
fn failed_repair_reports_interruption_without_final_verification_or_false_completion() {
  use crate::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};

  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let remaining = snapshot.policy.unwrap().emergency_reserve_bytes.checked_sub(snapshot.critical_reserved_bytes).unwrap();
  let _pressure =
    coordinator.reserve(MemoryOwner::Repair, remaining, AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery)).unwrap();
  let events = capture(|| {
    let path = engine.database_path().to_str().unwrap();
    let mut report = VerifyReport::new(path);
    report.missing_kv_entries = 1;
    assert!(matches!(repair_report_checked(&engine, path, report), Err(EngineError::ResourceExhausted(_))));
  });
  for phase in ["repair_actions", "repair_kv_rebuild"] {
    let selected: Vec<_> = events.iter().filter(|event| event["phase"] == phase).collect();
    assert_eq!(selected.len(), 2, "{phase}: {events:?}");
    assert_eq!(selected[0]["status"], "started");
    assert_eq!(selected[1]["status"], "interrupted");
    assert_eq!(selected[1]["units"], 0);
  }
  assert!(!events.iter().any(|event| event["phase"] == "final_verification" || event["status"] == "completed"));
}

#[test]
fn directory_repair_reports_both_collection_paths_and_publication() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&crate::engine::RequestContext::system(), "/nested/file.txt", b"retained", None).unwrap();
  let events = capture(|| {
    assert!(operations.repair_directory_index_from_path_records_unrouted("/nested").unwrap() > 0);
    assert!(operations.rebuild_directory_tree_unrouted().unwrap() > 0);
  });
  for phase in ["targeted_directory_repair_scan", "directory_repair_scan", "directory_repair_publish"] {
    let completed = events.iter().find(|event| event["phase"] == phase && event["status"] == "completed").unwrap();
    assert!(completed["units"].as_u64().unwrap() > 0, "{phase}: {events:?}");
  }
  assert!(!events.iter().any(|event| event["status"] == "interrupted"));
}
