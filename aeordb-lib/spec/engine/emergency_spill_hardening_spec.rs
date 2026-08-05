use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::emergency_spill::{
  EMERGENCY_SPILL_FORMAT, EMERGENCY_SPILL_FORMAT_V2, EmergencySpillApplyReport, EmergencySpillFormatVersion, EmergencySpillLocation,
  SpillLocationClass, apply_wal_tails_to_database, mark_artifacts_applied, scan_for_database_with_dirs, scan_for_database_with_locations,
};

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

#[cfg(unix)]
#[test]
fn v2_database_identity_uses_raw_native_path_bytes() {
  use std::ffi::OsString;
  use std::os::unix::ffi::OsStringExt;

  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join(OsString::from_vec(b"database-\xff.aeordb".to_vec()));
  fs::write(&database, b"abc").unwrap();
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
  assert_eq!(artifacts[1].creation_sequence, 12);
  assert_eq!(artifacts[0].source_location_class, SpillLocationClass::ConfiguredFallback);
  assert_eq!(artifacts[0].components.len(), 1);
  assert_eq!(artifacts[0].components[0].length, 3);
  assert_eq!(artifacts[0].components[0].digest, *blake3::hash(b"def").as_bytes());
  assert!(artifacts[0].manifest_length > 0);
  assert_ne!(artifacts[0].manifest_digest, [0u8; 32]);
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

  let artifacts = scan_for_database_with_dirs(&database, &[base.clone()]).unwrap();
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
