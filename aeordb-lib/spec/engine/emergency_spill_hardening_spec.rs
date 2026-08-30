use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::emergency_spill::{
  EMERGENCY_SPILL_FORMAT, EMERGENCY_SPILL_FORMAT_V2, EMERGENCY_SPILL_FORMAT_V3, INDEX_RUNTIME_EMERGENCY_STATE_FORMAT_V1,
  EmergencySpillApplyReport, EmergencySpillFormatVersion, EmergencySpillLocation, SpillLocationClass, apply_wal_tails_to_database,
  mark_artifacts_applied, scan_for_database_with_dirs, scan_for_database_with_locations, update_typed_manifest_latest,
};
use aeordb::engine::durability_coordinator::{DurabilityOperation, OsErrorClass};

const DATABASE_ID: [u8; 16] = [0x21; 16];

fn digest(bytes: &[u8]) -> String {
  blake3::hash(bytes).to_hex().to_string()
}

#[cfg(unix)]
fn native_path_bytes(path: &Path) -> Vec<u8> {
  use std::os::unix::ffi::OsStrExt;
  path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn native_path_bytes(path: &Path) -> Vec<u8> {
  use std::os::windows::ffi::OsStrExt;
  path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect()
}

fn write_v2_artifact(
  base: &Path,
  name: &str,
  database: &Path,
  incident_id: [u8; 16],
  created_at_ms: i64,
  creation_sequence: u64,
  wal_bytes: &[u8],
) -> PathBuf {
  let directory = base.join(name);
  fs::create_dir_all(&directory).unwrap();
  let wal_path = directory.join("wal-tail.bin");
  fs::write(&wal_path, wal_bytes).unwrap();
  let manifest = serde_json::json!({
    "format": EMERGENCY_SPILL_FORMAT_V2,
    "database_id": hex::encode(DATABASE_ID),
    "incident_id": hex::encode(incident_id),
    "source_location_class": SpillLocationClass::ConfiguredFallback as u16,
    "path_encoding": if cfg!(windows) { 2 } else { 1 },
    "creation_sequence": creation_sequence,
    "first_failure_at_ms": created_at_ms,
    "latest_failure_at_ms": created_at_ms,
    "failed_operation": DurabilityOperation::AuthorityBarrier.stable_id(),
    "os_error_class": OsErrorClass::MediaIo.stable_id(),
    "os_error_code": 5,
    "last_selected_header_sequence": 7,
    "last_durable_write_sequence": 8,
    "last_durable_publication_sequence": 8,
    "attempted_at": chrono::DateTime::from_timestamp_millis(created_at_ms).unwrap().to_rfc3339(),
    "db_path": database.display().to_string(),
    "db_path_bytes": hex::encode(native_path_bytes(database)),
    "context": "test barrier",
    "failure": "synthetic EIO",
    "hash_algorithm": "Blake3_256",
    "components": [{
      "kind": "wal_tail",
      "file_name": "wal-tail.bin",
      "length": wal_bytes.len(),
      "blake3": digest(wal_bytes),
    }],
    "hot_tail_writes": 0,
    "hot_tail_voids": 0,
    "wal_tail_copy_start": 3,
    "wal_tail_end": 3 + wal_bytes.len(),
    "wal_tail_bytes": wal_bytes.len(),
    "wal_tail_truncated": false,
    "errors": [],
  });
  fs::write(directory.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
  directory
}

fn write_v3_artifact(base: &Path, database: &Path, runtime_bytes_override: Option<&[u8]>) -> PathBuf {
  let directory = base.join("v3-artifact");
  fs::create_dir_all(&directory).unwrap();
  let workspace = database.parent().unwrap().join("runtime-workspace");
  let runtime = serde_json::json!({
    "format": INDEX_RUNTIME_EMERGENCY_STATE_FORMAT_V1,
    "database_id": hex::encode(DATABASE_ID),
    "destination_physical_instance_id": hex::encode([0x52; 16]),
    "runtime_id": hex::encode([0x53; 16]),
    "hash_algorithm": 1,
    "lifecycle": 4,
    "highest_checkpoint_sequence": 7,
    "publication_in_flight": false,
    "soft_queued_notices": 2,
    "soft_retained_bytes": 128,
    "soft_reconciliation_required": true,
    "soft_lost_through_sequence": 43,
    "producer_pending_tasks": 3,
    "producer_pending_bytes": 256,
    "producer_leased_tasks": 0,
    "mutation_active_records": 4,
    "mutation_active_bytes": 512,
    "mutation_frozen_records": 0,
    "mutation_frozen_bytes": 0,
    "reconciliation_required": true,
    "reason": "shutdown_drain_incomplete",
    "workspace": {
      "path_encoding": if cfg!(windows) { 2 } else { 1 },
      "path": workspace.display().to_string(),
      "path_bytes": hex::encode(native_path_bytes(&workspace)),
      "workspace_id": hex::encode([0x54; 16]),
      "selected_head": {
        "manifest_digest": hex::encode([0x55; 32]),
        "durable_sequence": 7,
        "durable_bytes": 4096
      }
    }
  });
  let runtime_bytes = runtime_bytes_override.map(ToOwned::to_owned).unwrap_or_else(|| serde_json::to_vec_pretty(&runtime).unwrap());
  fs::write(directory.join("index-runtime-state.json"), &runtime_bytes).unwrap();
  let incident_id = [0x56; 16];
  let manifest = serde_json::json!({
    "format": EMERGENCY_SPILL_FORMAT_V3,
    "database_id": hex::encode(DATABASE_ID),
    "incident_id": hex::encode(incident_id),
    "source_location_class": SpillLocationClass::ConfiguredFallback as u16,
    "path_encoding": if cfg!(windows) { 2 } else { 1 },
    "creation_sequence": 1,
    "first_failure_at_ms": 1_700_000_000_000i64,
    "latest_failure_at_ms": 1_700_000_000_000i64,
    "failed_operation": DurabilityOperation::ShutdownFlush.stable_id(),
    "os_error_class": OsErrorClass::OtherPersistentIo.stable_id(),
    "os_error_code": -1,
    "last_selected_header_sequence": 7,
    "last_durable_write_sequence": 8,
    "last_durable_publication_sequence": 8,
    "attempted_at": "2023-11-14T22:13:20Z",
    "db_path": database.display().to_string(),
    "db_path_bytes": hex::encode(native_path_bytes(database)),
    "context": "shutdown",
    "failure": "injected runtime drain failure",
    "hash_algorithm": "Blake3_256",
    "components": [{
      "kind": "index_runtime_state",
      "file_name": "index-runtime-state.json",
      "length": runtime_bytes.len(),
      "blake3": digest(&runtime_bytes),
    }],
    "hot_tail_writes": 0,
    "hot_tail_voids": 0,
    "wal_tail_bytes": 0,
    "wal_tail_truncated": false,
    "errors": [],
  });
  fs::write(directory.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
  directory
}

#[test]
fn v3_scan_retains_typed_runtime_reconciliation_evidence_and_marks_only_the_exact_manifest() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_v3_artifact(&base, &database, None);

  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  assert_eq!(artifacts.len(), 1);
  assert_eq!(artifacts[0].format_version, EmergencySpillFormatVersion::V3);
  let runtime = artifacts[0].index_runtime_state.as_ref().expect("typed runtime state");
  assert_eq!(runtime.database_id, DATABASE_ID);
  assert_eq!(runtime.destination_physical_instance_id, [0x52; 16]);
  assert_eq!(runtime.runtime_id, [0x53; 16]);
  assert_eq!(runtime.highest_checkpoint_sequence, 7);
  assert_eq!(runtime.soft_queued_notices, 2);
  assert_eq!(runtime.producer_pending_tasks, 3);
  assert_eq!(runtime.mutation_active_records, 4);
  assert!(runtime.reconciliation_required);
  let workspace = runtime.workspace.as_ref().expect("workspace evidence");
  assert_eq!(workspace.workspace_id, [0x54; 16]);
  assert_eq!(workspace.selected_head.as_ref().unwrap().durable_sequence, 7);

  update_typed_manifest_latest(
    &artifacts[0].manifest_path,
    artifacts[0].database_id.unwrap(),
    artifacts[0].incident_id.unwrap(),
    1_700_000_000_001,
    "later runtime shutdown failure",
  )
  .unwrap();
  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  assert_eq!(artifacts[0].latest_failure_at_ms, 1_700_000_000_001);
  assert_eq!(artifacts[0].latest_failure.as_deref(), Some("later runtime shutdown failure"));

  let report = EmergencySpillApplyReport { artifact_count: 1, ..EmergencySpillApplyReport::default() };
  mark_artifacts_applied(&database, &artifacts, &report).unwrap();
  assert!(scan_for_database_with_locations(&database, &[location(&base)]).unwrap().is_empty());

  let marker_path = base.join("v3-artifact").join("applied.json");
  let mut marker: serde_json::Value = serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
  marker["manifest_blake3"] = serde_json::json!(hex::encode([0x99; 32]));
  fs::write(&marker_path, serde_json::to_vec_pretty(&marker).unwrap()).unwrap();
  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("does not match"), "{error}");
}

#[test]
fn v3_scan_rejects_tampered_or_malformed_runtime_reconciliation_evidence() {
  for malformed in [false, true] {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("test.aeordb");
    fs::write(&database, b"abc").unwrap();
    let base = temp.path().join("spill");
    fs::create_dir_all(&base).unwrap();
    let override_bytes = malformed.then_some(br#"{"format":"unknown"}"#.as_slice());
    let directory = write_v3_artifact(&base, &database, override_bytes);
    if !malformed {
      let state_path = directory.join("index-runtime-state.json");
      let mut tampered = fs::read(&state_path).unwrap();
      tampered[0] ^= 0x01;
      fs::write(state_path, tampered).unwrap();
    }

    let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
    assert!(error.to_string().contains(if malformed { "runtime" } else { "digest" }), "malformed={malformed}: {error}");
  }
}

#[test]
fn v3_scan_rejects_workspace_diagnostic_path_that_disagrees_with_native_bytes() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v3_artifact(&base, &database, None);

  let state_path = directory.join("index-runtime-state.json");
  let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
  state["workspace"]["path"] = serde_json::json!(database.parent().unwrap().join("different-workspace").display().to_string());
  let state_bytes = serde_json::to_vec_pretty(&state).unwrap();
  fs::write(&state_path, &state_bytes).unwrap();

  let manifest_path = directory.join("manifest.json");
  let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
  manifest["components"][0]["length"] = serde_json::json!(state_bytes.len());
  manifest["components"][0]["blake3"] = serde_json::json!(digest(&state_bytes));
  fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("workspace path representations disagree"), "{error}");
}

#[test]
fn v3_post_scan_tamper_refuses_replay_and_applied_marker_publication() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v3_artifact(&base, &database, None);
  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  let state_path = directory.join("index-runtime-state.json");
  let mut tampered = fs::read(&state_path).unwrap();
  tampered[0] ^= 0x01;
  fs::write(&state_path, tampered).unwrap();

  let replay_error = apply_wal_tails_to_database(&database, &artifacts).unwrap_err();
  assert!(replay_error.to_string().contains("digest"), "{replay_error}");
  let report = EmergencySpillApplyReport { artifact_count: 1, ..EmergencySpillApplyReport::default() };
  let marker_error = mark_artifacts_applied(&database, &artifacts, &report).unwrap_err();
  assert!(marker_error.to_string().contains("digest"), "{marker_error}");
  assert!(!directory.join("applied.json").exists());
}

#[test]
fn frozen_v2_component_limit_and_kind_do_not_expand_when_v3_is_added() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x57; 16], 1_700_000_000_000, 1, b"def");
  fs::write(directory.join("index-runtime-state.json"), b"{}").unwrap();
  let manifest_path = directory.join("manifest.json");
  let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
  manifest["components"].as_array_mut().unwrap().push(serde_json::json!({
    "kind": "index_runtime_state",
    "file_name": "index-runtime-state.json",
    "length": 2,
    "blake3": digest(b"{}"),
  }));
  fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("unknown emergency spill component kind"), "{error}");
}

#[cfg(unix)]
#[test]
fn v2_database_identity_uses_raw_native_path_bytes() {
  use std::ffi::OsString;
  use std::os::unix::ffi::OsStringExt;

  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join(OsString::from_vec(b"database-\xff.aeordb".to_vec()));
  // macOS preserves these native bytes in `OsString` but rejects them in a
  // filesystem syscall. Spill ownership only needs the database identity.
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_v2_artifact(&base, "artifact", &database, [0x33; 16], 1_700_000_000_000, 1, b"def");

  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  assert_eq!(artifacts.len(), 1);
  assert_eq!(artifacts[0].db_path_native.as_deref(), Some(native_path_bytes(&database).as_slice()));
}

fn location(base: &Path) -> EmergencySpillLocation {
  EmergencySpillLocation { class: SpillLocationClass::ConfiguredFallback, path: base.to_path_buf() }
}

fn write_pending_artifact(base: &Path, name: &str, database: &Path, incident_id: [u8; 16]) -> PathBuf {
  let directory = base.join(name);
  fs::create_dir_all(&directory).unwrap();
  let pending = serde_json::json!({
    "format": "aeordb-emergency-spill-pending-v2",
    "database_id": hex::encode(DATABASE_ID),
    "incident_id": hex::encode(incident_id),
    "source_location_class": SpillLocationClass::ConfiguredFallback as u16,
    "path_encoding": if cfg!(windows) { 2 } else { 1 },
    "creation_sequence": 1,
    "first_failure_at_ms": 1_700_000_000_000i64,
    "db_path": database.display().to_string(),
    "db_path_bytes": hex::encode(native_path_bytes(database)),
  });
  fs::write(directory.join("pending.json"), serde_json::to_vec_pretty(&pending).unwrap()).unwrap();
  directory
}

#[test]
fn v2_scan_validates_identity_components_and_deterministic_order() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_v2_artifact(&base, "later-sequence", &database, [0x32; 16], 1_700_000_000_000, 12, b"def");
  write_v2_artifact(&base, "earlier-sequence", &database, [0x31; 16], 1_700_000_000_000, 11, b"def");

  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  assert_eq!(artifacts.len(), 2);
  assert_eq!(artifacts[0].format_version, EmergencySpillFormatVersion::V2);
  assert_eq!(artifacts[0].database_id, Some(DATABASE_ID));
  assert_eq!(artifacts[0].incident_id, Some([0x31; 16]));
  assert_eq!(artifacts[0].creation_sequence, 11);
  assert_eq!(artifacts[0].failed_operation, Some(DurabilityOperation::AuthorityBarrier.stable_id()));
  assert_eq!(artifacts[0].os_error_class, Some(OsErrorClass::MediaIo.stable_id()));
  assert_eq!(artifacts[0].os_error_code, Some(5));
  assert_eq!(artifacts[1].creation_sequence, 12);
  assert_eq!(artifacts[0].source_location_class, SpillLocationClass::ConfiguredFallback);
  assert_eq!(artifacts[0].components.len(), 1);
  assert_eq!(artifacts[0].components[0].length, 3);
  assert_eq!(artifacts[0].components[0].digest, *blake3::hash(b"def").as_bytes());
  assert!(artifacts[0].manifest_length > 0);
  assert_ne!(artifacts[0].manifest_digest, [0u8; 32]);
}

#[test]
fn v2_scan_rejects_partial_typed_failure_evidence() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x39; 16], 1_700_000_000_000, 1, b"def");
  let manifest_path = directory.join("manifest.json");
  let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
  manifest.as_object_mut().unwrap().remove("os_error_code");
  fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();

  assert!(error.to_string().contains("typed failure evidence must be complete"), "{error}");
}

#[test]
fn v2_scan_rejects_missing_or_wrong_typed_count_and_truncation_evidence() {
  for field in ["wal_tail_bytes", "hot_tail_writes", "hot_tail_voids", "wal_tail_truncated"] {
    for wrong_type in [false, true] {
      let temp = tempfile::tempdir().unwrap();
      let database = temp.path().join("test.aeordb");
      fs::write(&database, b"abc").unwrap();
      let base = temp.path().join("spill");
      fs::create_dir_all(&base).unwrap();
      let directory = write_v2_artifact(&base, "artifact", &database, [0x3b; 16], 1_700_000_000_000, 1, b"def");
      let manifest_path = directory.join("manifest.json");
      let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
      if wrong_type {
        manifest[field] = serde_json::json!("not-the-declared-type");
      } else {
        manifest.as_object_mut().unwrap().remove(field);
      }
      fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

      let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
      assert!(error.to_string().contains(field), "field={field} wrong_type={wrong_type}: {error}");
    }
  }
}

#[test]
fn matching_spill_with_malformed_attempt_timestamp_fails_closed() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x3a; 16], 1_700_000_000_000, 1, b"def");
  let manifest_path = directory.join("manifest.json");
  let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
  manifest["attempted_at"] = serde_json::json!("not-a-timestamp");
  fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();

  assert!(error.to_string().contains("attempted_at"), "{error}");
}

#[test]
fn v2_scan_and_replay_fail_closed_when_component_changes() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x41; 16], 1_700_000_000_000, 1, b"def");

  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  fs::write(directory.join("wal-tail.bin"), b"BAD").unwrap();
  let error = apply_wal_tails_to_database(&database, &artifacts).unwrap_err();
  assert!(error.to_string().contains("digest"));

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("digest"));
}

#[test]
fn replay_rejects_a_caller_substituted_component_path_before_io() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_v2_artifact(&base, "artifact", &database, [0x47; 16], 1_700_000_000_000, 1, b"def");

  let mut artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  let non_directory = temp.path().join("not-a-directory");
  fs::write(&non_directory, b"ordinary file").unwrap();
  artifacts[0].wal_tail_path = Some(non_directory.join("wal-tail.bin"));

  let error = apply_wal_tails_to_database(&database, &artifacts).expect_err("a caller-substituted component path must fail closed");

  assert!(error.to_string().contains("manifest"), "unexpected replay error: {error}");
  assert_eq!(fs::read(&database).unwrap(), b"abc");
}

#[test]
fn damaged_foreign_spill_does_not_block_an_unrelated_database() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("target.aeordb");
  let foreign_database = temp.path().join("foreign.aeordb");
  fs::write(&database, b"target").unwrap();
  fs::write(&foreign_database, b"foreign").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "foreign", &foreign_database, [0x48; 16], 1_700_000_000_000, 1, b"tail");
  fs::write(directory.join("wal-tail.bin"), b"BAD!").unwrap();
  fs::write(directory.join("applied.json"), b"{}").unwrap();

  assert!(scan_for_database_with_locations(&database, &[location(&base)]).unwrap().is_empty());
}

#[test]
fn foreign_spill_location_class_mismatch_does_not_block_an_unrelated_database() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("target.aeordb");
  let foreign_database = temp.path().join("foreign.aeordb");
  fs::write(&database, b"target").unwrap();
  fs::write(&foreign_database, b"foreign").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_v2_artifact(&base, "foreign", &foreign_database, [0x49; 16], 1_700_000_000_000, 1, b"tail");

  let locations = [EmergencySpillLocation { class: SpillLocationClass::OsUserData, path: base }];
  assert!(scan_for_database_with_locations(&database, &locations).unwrap().is_empty());
}

#[test]
fn foreign_pending_location_class_mismatch_does_not_block_an_unrelated_database() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("target.aeordb");
  let foreign_database = temp.path().join("foreign.aeordb");
  fs::write(&database, b"target").unwrap();
  fs::write(&foreign_database, b"foreign").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_pending_artifact(&base, "foreign", &foreign_database, [0x4a; 16]);

  let locations = [EmergencySpillLocation { class: SpillLocationClass::OsUserData, path: base }];
  assert!(scan_for_database_with_locations(&database, &locations).unwrap().is_empty());
}

#[test]
fn matching_spill_location_class_mismatch_still_fails_closed() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("target.aeordb");
  fs::write(&database, b"target").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_v2_artifact(&base, "matching", &database, [0x4b; 16], 1_700_000_000_000, 1, b"tail");

  let locations = [EmergencySpillLocation { class: SpillLocationClass::OsUserData, path: base }];
  let error = scan_for_database_with_locations(&database, &locations).unwrap_err();
  assert!(error.to_string().contains("location class does not match"), "{error}");
}

#[test]
fn v2_scan_rejects_an_empty_native_database_path() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x44; 16], 1_700_000_000_000, 1, b"def");
  let manifest_path = directory.join("manifest.json");
  let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
  manifest["db_path_bytes"] = serde_json::json!("");
  fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("database path is empty"));
}

#[test]
fn incomplete_v2_spill_is_visible_and_blocks_its_database() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_pending_artifact(&base, "incomplete", &database, [0x45; 16]);

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("incomplete emergency spill"));
}

#[test]
fn completed_v2_spill_supersedes_a_leftover_pending_record() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "complete", &database, [0x46; 16], 1_700_000_000_000, 1, b"def");
  let pending_directory = write_pending_artifact(&base, "pending-source", &database, [0x46; 16]);
  fs::rename(pending_directory.join("pending.json"), directory.join("pending.json")).unwrap();
  fs::remove_dir(pending_directory).unwrap();

  assert_eq!(scan_for_database_with_locations(&database, &[location(&base)]).unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn v2_replay_refuses_a_symlink_substituted_after_scan() {
  use std::os::unix::fs::symlink;

  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x42; 16], 1_700_000_000_000, 1, b"def");
  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();

  let outside = temp.path().join("outside.bin");
  fs::write(&outside, b"def").unwrap();
  fs::remove_file(directory.join("wal-tail.bin")).unwrap();
  symlink(&outside, directory.join("wal-tail.bin")).unwrap();
  let error = apply_wal_tails_to_database(&database, &artifacts).unwrap_err();
  assert!(error.to_string().contains("symlink") || error.to_string().contains("no-follow"));
}

#[test]
fn replay_refuses_an_artifact_scanned_for_a_different_database_before_writing() {
  let temp = tempfile::tempdir().unwrap();
  let source_database = temp.path().join("source.aeordb");
  let target_database = temp.path().join("target.aeordb");
  fs::write(&source_database, b"abc").unwrap();
  fs::write(&target_database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  write_v2_artifact(&base, "artifact", &source_database, [0x48; 16], 1_700_000_000_000, 1, b"def");
  let artifacts = scan_for_database_with_locations(&source_database, &[location(&base)]).unwrap();

  let error = apply_wal_tails_to_database(&target_database, &artifacts).unwrap_err();

  assert!(error.to_string().contains("different database"), "{error}");
  assert_eq!(fs::read(&target_database).unwrap(), b"abc");
}

#[test]
fn marker_refuses_an_artifact_scanned_for_a_different_database_before_publication() {
  let temp = tempfile::tempdir().unwrap();
  let source_database = temp.path().join("source.aeordb");
  let target_database = temp.path().join("target.aeordb");
  fs::write(&source_database, b"abc").unwrap();
  fs::write(&target_database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &source_database, [0x49; 16], 1_700_000_000_000, 1, b"def");
  let artifacts = scan_for_database_with_locations(&source_database, &[location(&base)]).unwrap();
  let report = EmergencySpillApplyReport { artifact_count: 1, ..EmergencySpillApplyReport::default() };

  let error = mark_artifacts_applied(&target_database, &artifacts, &report).unwrap_err();

  assert!(error.to_string().contains("different database"), "{error}");
  assert!(!directory.join("applied.json").exists());
}

#[test]
fn malformed_applied_marker_cannot_hide_v2_evidence() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x43; 16], 1_700_000_000_000, 1, b"def");
  fs::write(directory.join("applied.json"), b"{}").unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("applied marker"));

  fs::remove_file(directory.join("applied.json")).unwrap();
  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  let report = EmergencySpillApplyReport { artifact_count: 1, ..EmergencySpillApplyReport::default() };
  mark_artifacts_applied(&database, &artifacts, &report).unwrap();
  assert!(scan_for_database_with_locations(&database, &[location(&base)]).unwrap().is_empty());
}

#[test]
fn v2_applied_marker_requires_both_artifact_identities_without_writing_a_marker() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let directory = write_v2_artifact(&base, "artifact", &database, [0x44; 16], 1_700_000_000_000, 1, b"def");
  let artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  let report = EmergencySpillApplyReport { artifact_count: 1, ..EmergencySpillApplyReport::default() };

  let mut missing_database_id = artifacts.clone();
  missing_database_id[0].database_id = None;
  let error = mark_artifacts_applied(&database, &missing_database_id, &report).unwrap_err();
  assert!(error.to_string().contains("manifest"), "{error}");
  assert!(!directory.join("applied.json").exists());

  let mut missing_incident_id = artifacts;
  missing_incident_id[0].incident_id = None;
  let error = mark_artifacts_applied(&database, &missing_incident_id, &report).unwrap_err();
  assert!(error.to_string().contains("manifest"), "{error}");
  assert!(!directory.join("applied.json").exists());
}

#[test]
fn v2_applied_marker_preflights_the_complete_batch_before_writing() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  fs::create_dir_all(&base).unwrap();
  let first_directory = write_v2_artifact(&base, "first", &database, [0x45; 16], 1_700_000_000_000, 1, b"def");
  let second_directory = write_v2_artifact(&base, "second", &database, [0x46; 16], 1_700_000_000_001, 2, b"ghi");
  let mut artifacts = scan_for_database_with_locations(&database, &[location(&base)]).unwrap();
  artifacts.last_mut().unwrap().incident_id = None;
  let report = EmergencySpillApplyReport { artifact_count: 2, ..EmergencySpillApplyReport::default() };

  let error = mark_artifacts_applied(&database, &artifacts, &report).unwrap_err();
  assert!(error.to_string().contains("manifest"), "{error}");
  assert!(!first_directory.join("applied.json").exists());
  assert!(!second_directory.join("applied.json").exists());
}

#[test]
fn legacy_v1_evidence_remains_scannable_replayable_and_markable() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("legacy.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  let directory = base.join("legacy-artifact");
  fs::create_dir_all(&directory).unwrap();
  let wal_path = directory.join("wal-tail.bin");
  fs::write(&wal_path, b"def").unwrap();
  let manifest = serde_json::json!({
    "format": EMERGENCY_SPILL_FORMAT,
    "attempted_at": "2026-06-15T09:00:00Z",
    "db_path": database.display().to_string(),
    "wal_tail_path": wal_path.display().to_string(),
    "wal_tail_copy_start": 3,
    "wal_tail_end": 6,
    "wal_tail_bytes": 3,
    "wal_tail_truncated": false,
  });
  fs::write(directory.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

  let artifacts = scan_for_database_with_dirs(&database, std::slice::from_ref(&base)).unwrap();
  assert_eq!(artifacts.len(), 1);
  assert_eq!(artifacts[0].format_version, EmergencySpillFormatVersion::V1);
  let report = apply_wal_tails_to_database(&database, &artifacts).unwrap();
  assert_eq!(fs::read(&database).unwrap(), b"abcdef");
  mark_artifacts_applied(&database, &artifacts, &report).unwrap();
  assert!(scan_for_database_with_dirs(&database, &[base]).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn scan_refuses_a_symlinked_manifest() {
  use std::os::unix::fs::symlink;

  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  let directory = base.join("artifact");
  fs::create_dir_all(&directory).unwrap();
  let outside = temp.path().join("outside-manifest.json");
  fs::write(&outside, b"{}").unwrap();
  symlink(&outside, directory.join("manifest.json")).unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("symlink") || error.to_string().contains("no-follow"));
}

#[cfg(unix)]
#[test]
fn scan_refuses_a_symlinked_artifact_directory() {
  use std::os::unix::fs::symlink;

  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("test.aeordb");
  fs::write(&database, b"abc").unwrap();
  let base = temp.path().join("spill");
  let outside = temp.path().join("outside-artifact");
  fs::create_dir_all(&base).unwrap();
  write_v2_artifact(temp.path(), "outside-artifact", &database, [0x47; 16], 1_700_000_000_000, 1, b"def");
  symlink(&outside, base.join("linked-artifact")).unwrap();

  let error = scan_for_database_with_locations(&database, &[location(&base)]).unwrap_err();
  assert!(error.to_string().contains("symlink") || error.to_string().contains("no-follow"));
}
