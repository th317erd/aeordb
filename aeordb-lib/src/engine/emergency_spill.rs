use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::engine::durability::{rename_durable, sync_parent_dir};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::native_durability::sync_file_all_native;

pub const EMERGENCY_SPILL_FORMAT: &str = "aeordb-emergency-spill-v1";
pub const EMERGENCY_SPILL_FORMAT_V2: &str = "aeordb-emergency-spill-v2";
pub const EMERGENCY_SPILL_FORMAT_V3: &str = "aeordb-emergency-spill-v3";
pub const EMERGENCY_SPILL_PENDING_FORMAT_V2: &str = "aeordb-emergency-spill-pending-v2";
pub const EMERGENCY_SPILL_PENDING_FORMAT_V3: &str = "aeordb-emergency-spill-pending-v3";
pub const EMERGENCY_SPILL_APPLIED_FORMAT: &str = "aeordb-emergency-spill-applied-v1";
pub const EMERGENCY_SPILL_APPLIED_FORMAT_V2: &str = "aeordb-emergency-spill-applied-v2";
pub const EMERGENCY_SPILL_APPLIED_FORMAT_V3: &str = "aeordb-emergency-spill-applied-v3";
pub const INDEX_RUNTIME_EMERGENCY_STATE_FORMAT_V1: &str = "aeordb-index-runtime-emergency-state-v1";
pub(crate) const MANIFEST_SIZE_CAP: u64 = 1024 * 1024;
const INDEX_RUNTIME_STATE_SIZE_CAP: u64 = 64 * 1024;

type TypedFailureEvidence = (u16, u16, i32, u64, u64, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmergencySpillFormatVersion {
  V1,
  V2,
  V3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SpillLocationClass {
  OsUserData = 1,
  ConfiguredFallback = 2,
  TempFallback = 3,
}

impl SpillLocationClass {
  fn from_u64(value: u64) -> EngineResult<Self> {
    match value {
      1 => Ok(Self::OsUserData),
      2 => Ok(Self::ConfiguredFallback),
      3 => Ok(Self::TempFallback),
      _ => Err(EngineError::InvalidInput(format!("invalid emergency spill location class {value}"))),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmergencySpillLocation {
  pub class: SpillLocationClass,
  pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmergencySpillComponent {
  pub kind: String,
  pub path: PathBuf,
  pub length: u64,
  pub digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmergencyIndexRuntimeSelectedHeadV1 {
  pub manifest_digest: [u8; 32],
  pub durable_sequence: u64,
  pub durable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmergencyIndexRuntimeWorkspaceV1 {
  pub path_encoding: u16,
  pub path: PathBuf,
  pub workspace_id: [u8; 16],
  pub selected_head: Option<EmergencyIndexRuntimeSelectedHeadV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmergencyIndexRuntimeStateV1 {
  pub database_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub runtime_id: [u8; 16],
  pub hash_algorithm: crate::engine::HashAlgorithm,
  pub lifecycle: u16,
  pub highest_checkpoint_sequence: u64,
  pub publication_in_flight: bool,
  pub soft_queued_notices: u64,
  pub soft_retained_bytes: u64,
  pub soft_reconciliation_required: bool,
  pub soft_lost_through_sequence: Option<u64>,
  pub producer_pending_tasks: u64,
  pub producer_pending_bytes: u64,
  pub producer_leased_tasks: u64,
  pub mutation_active_records: u64,
  pub mutation_active_bytes: u64,
  pub mutation_frozen_records: u64,
  pub mutation_frozen_bytes: u64,
  pub reconciliation_required: bool,
  pub reason: String,
  pub workspace: Option<EmergencyIndexRuntimeWorkspaceV1>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct EmergencyIndexRuntimeSelectedHeadWireV1 {
  manifest_digest: String,
  durable_sequence: u64,
  durable_bytes: u64,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct EmergencyIndexRuntimeWorkspaceWireV1 {
  path_encoding: u16,
  path: String,
  path_bytes: String,
  workspace_id: String,
  selected_head: Option<EmergencyIndexRuntimeSelectedHeadWireV1>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct EmergencyIndexRuntimeStateWireV1 {
  format: String,
  database_id: String,
  destination_physical_instance_id: String,
  runtime_id: String,
  hash_algorithm: u16,
  lifecycle: u16,
  highest_checkpoint_sequence: u64,
  publication_in_flight: bool,
  soft_queued_notices: u64,
  soft_retained_bytes: u64,
  soft_reconciliation_required: bool,
  soft_lost_through_sequence: Option<u64>,
  producer_pending_tasks: u64,
  producer_pending_bytes: u64,
  producer_leased_tasks: u64,
  mutation_active_records: u64,
  mutation_active_bytes: u64,
  mutation_frozen_records: u64,
  mutation_frozen_bytes: u64,
  reconciliation_required: bool,
  reason: String,
  workspace: Option<EmergencyIndexRuntimeWorkspaceWireV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmergencySpillArtifact {
  pub format_version: EmergencySpillFormatVersion,
  pub database_id: Option<[u8; 16]>,
  pub incident_id: Option<[u8; 16]>,
  pub source_location_class: SpillLocationClass,
  pub path_encoding: u16,
  pub creation_sequence: u64,
  pub first_failure_at_ms: i64,
  pub latest_failure_at_ms: i64,
  pub failed_operation: Option<u16>,
  pub os_error_class: Option<u16>,
  pub os_error_code: Option<i32>,
  pub last_selected_header_sequence: Option<u64>,
  pub last_durable_write_sequence: Option<u64>,
  pub last_durable_publication_sequence: Option<u64>,
  pub directory: PathBuf,
  pub manifest_path: PathBuf,
  pub manifest_length: u64,
  pub manifest_digest: [u8; 32],
  pub components: Vec<EmergencySpillComponent>,
  pub index_runtime_state: Option<EmergencyIndexRuntimeStateV1>,
  pub attempted_at: Option<String>,
  pub sort_millis: i64,
  pub db_path: Option<String>,
  pub db_path_native: Option<Vec<u8>>,
  pub context: Option<String>,
  pub failure: Option<String>,
  pub first_failure: Option<String>,
  pub latest_failure: Option<String>,
  pub hot_tail_path: Option<PathBuf>,
  pub wal_tail_path: Option<PathBuf>,
  pub hot_tail_writes: usize,
  pub hot_tail_voids: usize,
  pub wal_tail_copy_start: Option<u64>,
  pub wal_tail_end: Option<u64>,
  pub wal_tail_bytes: u64,
  pub wal_tail_truncated: bool,
}

#[derive(Debug, Default, Clone)]
pub struct EmergencySpillApplyReport {
  pub artifact_count: usize,
  pub wal_tails_seen: usize,
  pub wal_tail_bytes_present: u64,
  pub wal_tail_bytes_written: u64,
  pub hot_tail_files_seen: usize,
}

pub fn emergency_spill_base_dirs() -> Vec<PathBuf> {
  emergency_spill_locations().into_iter().map(|location| location.path).collect()
}

pub fn os_user_data_emergency_spill_dir() -> Option<PathBuf> {
  #[cfg(target_os = "windows")]
  {
    return std::env::var_os("LOCALAPPDATA")
      .or_else(|| std::env::var_os("APPDATA"))
      .map(PathBuf::from)
      .map(|path| path.join("AeorDB").join("emergency-spill"));
  }
  #[cfg(target_os = "macos")]
  {
    return std::env::var_os("HOME")
      .map(PathBuf::from)
      .map(|path| path.join("Library").join("Application Support").join("aeordb").join("emergency-spill"));
  }
  #[cfg(all(unix, not(target_os = "macos")))]
  {
    return std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).map(|path| path.join("aeordb").join("emergency-spill")).or_else(|| {
      std::env::var_os("HOME").map(PathBuf::from).map(|path| path.join(".local").join("share").join("aeordb").join("emergency-spill"))
    });
  }
  #[allow(unreachable_code)]
  None
}

pub fn emergency_spill_locations() -> Vec<EmergencySpillLocation> {
  let configured = std::env::var_os("AEORDB_EMERGENCY_SPILL_DIR").filter(|path| !path.is_empty()).map(PathBuf::from);
  emergency_spill_locations_with_configured(configured)
}

pub fn emergency_spill_locations_with_configured(configured: impl IntoIterator<Item = PathBuf>) -> Vec<EmergencySpillLocation> {
  let mut locations = Vec::new();
  #[cfg(test)]
  let configured_only = std::env::var_os("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY").is_some();
  #[cfg(not(test))]
  let configured_only = false;

  for path in configured {
    locations.push(EmergencySpillLocation { class: SpillLocationClass::ConfiguredFallback, path });
  }

  if !configured_only {
    if let Some(path) = os_user_data_emergency_spill_dir() {
      locations.push(EmergencySpillLocation { class: SpillLocationClass::OsUserData, path });
    }
  }
  if !configured_only {
    locations
      .push(EmergencySpillLocation { class: SpillLocationClass::TempFallback, path: std::env::temp_dir().join("aeordb-emergency-spill") });
  }
  dedupe_locations(locations)
}

pub fn scan_unapplied_for_database(db_path: impl AsRef<Path>) -> EngineResult<Vec<EmergencySpillArtifact>> {
  let db_path = db_path.as_ref();
  let locations = crate::engine::config_resolver::preopen_emergency_spill_locations(
    db_path,
    &crate::engine::config_resolver::CommandLineConfigOverrides::default(),
  )?;
  scan_for_database_with_locations(db_path, &locations)
}

pub fn scan_for_database_with_dirs(db_path: impl AsRef<Path>, base_dirs: &[PathBuf]) -> EngineResult<Vec<EmergencySpillArtifact>> {
  let locations: Vec<_> =
    base_dirs.iter().cloned().map(|path| EmergencySpillLocation { class: SpillLocationClass::ConfiguredFallback, path }).collect();
  scan_for_database_with_locations(db_path, &locations)
}

pub fn scan_for_database_with_locations(
  db_path: impl AsRef<Path>,
  locations: &[EmergencySpillLocation],
) -> EngineResult<Vec<EmergencySpillArtifact>> {
  let db_path = db_path.as_ref();
  let mut artifacts = Vec::new();
  for location in dedupe_locations(locations.to_vec()) {
    if !location.path.exists() {
      continue;
    }
    reject_symlink(&location.path, "emergency spill base directory")?;
    let manifest_paths = manifest_paths_in_base_dir(&location.path)?;
    let mut complete_directories = HashSet::new();
    for manifest_path in manifest_paths {
      let directory = required_parent(&manifest_path, "emergency spill manifest")?.to_path_buf();
      match manifest_matches_database(&manifest_path, db_path)? {
        Some(false) => {
          complete_directories.insert(directory);
          continue;
        }
        Some(true) => {}
        None => continue,
      }
      let Some(artifact) = parse_manifest(&manifest_path, location.class)? else {
        continue;
      };
      complete_directories.insert(artifact.directory.clone());
      if !artifact_matches_database(&artifact, db_path)? {
        continue;
      }
      if artifact_applied(&artifact)? {
        continue;
      }
      validate_artifact_components(&artifact)?;
      artifacts.push(artifact);
    }
    for pending_path in pending_paths_in_base_dir(&location.path)? {
      let directory = required_parent(&pending_path, "emergency spill pending record")?;
      if complete_directories.contains(directory) {
        continue;
      }
      if pending_matches_database(&pending_path, location.class, db_path)? {
        return Err(EngineError::InvalidInput(format!(
          "incomplete emergency spill {} must be recovered before writable startup",
          pending_path.display()
        )));
      }
    }
  }
  artifacts.sort_by(|left, right| {
    left
      .first_failure_at_ms
      .cmp(&right.first_failure_at_ms)
      .then_with(|| left.creation_sequence.cmp(&right.creation_sequence))
      .then_with(|| left.manifest_digest.cmp(&right.manifest_digest))
      .then_with(|| native_path_bytes(&left.manifest_path).cmp(&native_path_bytes(&right.manifest_path)))
  });
  Ok(artifacts)
}

pub fn apply_wal_tails_to_database(
  db_path: impl AsRef<Path>,
  artifacts: &[EmergencySpillArtifact],
) -> EngineResult<EmergencySpillApplyReport> {
  let db_path = db_path.as_ref();
  let mut report = EmergencySpillApplyReport { artifact_count: artifacts.len(), ..EmergencySpillApplyReport::default() };

  for artifact in artifacts {
    validate_artifact_for_database(artifact, db_path)?;
  }

  for artifact in artifacts {
    if artifact.hot_tail_path.is_some() {
      report.hot_tail_files_seen += 1;
    }

    let Some(wal_tail_path) = artifact.wal_tail_path.as_ref() else {
      continue;
    };
    match fs::symlink_metadata(wal_tail_path) {
      Ok(_) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        if artifact.wal_tail_bytes > 0 {
          return Err(EngineError::InvalidInput(format!(
            "emergency spill manifest {} references missing WAL tail {}",
            artifact.manifest_path.display(),
            wal_tail_path.display()
          )));
        }
        continue;
      }
      Err(error) => return Err(EngineError::IoError(error)),
    }

    let copy_start = artifact.wal_tail_copy_start.ok_or_else(|| {
      EngineError::InvalidInput(format!("emergency spill manifest {} is missing wal_tail_copy_start", artifact.manifest_path.display()))
    })?;
    let evidence = artifact.components.iter().find(|component| component.kind == "wal_tail");
    if matches!(artifact.format_version, EmergencySpillFormatVersion::V2 | EmergencySpillFormatVersion::V3) && evidence.is_none() {
      return Err(EngineError::InvalidInput(format!(
        "emergency spill manifest {} has WAL bytes without component evidence",
        artifact.manifest_path.display()
      )));
    }
    let (present, written) = apply_one_wal_tail(db_path, wal_tail_path, copy_start, evidence)?;
    report.wal_tails_seen += 1;
    report.wal_tail_bytes_present = report.wal_tail_bytes_present.saturating_add(present);
    report.wal_tail_bytes_written = report.wal_tail_bytes_written.saturating_add(written);
  }

  Ok(report)
}

pub fn mark_artifacts_applied(
  db_path: impl AsRef<Path>,
  artifacts: &[EmergencySpillArtifact],
  report: &EmergencySpillApplyReport,
) -> EngineResult<()> {
  let db_path = db_path.as_ref();
  for artifact in artifacts {
    validate_artifact_for_database(artifact, db_path)?;
    if matches!(artifact.format_version, EmergencySpillFormatVersion::V2 | EmergencySpillFormatVersion::V3) {
      required_typed_artifact_identities(artifact)?;
    }
  }

  let db_path = db_path.display().to_string();
  for artifact in artifacts {
    let marker = match artifact.format_version {
      EmergencySpillFormatVersion::V1 => serde_json::json!({
        "format": EMERGENCY_SPILL_APPLIED_FORMAT,
        "applied_at": chrono::Utc::now().to_rfc3339(),
        "db_path": db_path.clone(),
        "manifest_path": artifact.manifest_path.display().to_string(),
        "wal_tail_bytes_present": report.wal_tail_bytes_present,
        "wal_tail_bytes_written": report.wal_tail_bytes_written,
      }),
      EmergencySpillFormatVersion::V2 => {
        let (database_id, incident_id) = required_typed_artifact_identities(artifact)?;
        serde_json::json!({
          "format": EMERGENCY_SPILL_APPLIED_FORMAT_V2,
          "applied_at": chrono::Utc::now().to_rfc3339(),
          "db_path": db_path.clone(),
          "database_id": hex::encode(database_id),
          "incident_id": hex::encode(incident_id),
          "manifest_length": artifact.manifest_length,
          "manifest_blake3": hex::encode(artifact.manifest_digest),
          "wal_tail_bytes_present": report.wal_tail_bytes_present,
          "wal_tail_bytes_written": report.wal_tail_bytes_written,
        })
      }
      EmergencySpillFormatVersion::V3 => {
        let (database_id, incident_id) = required_typed_artifact_identities(artifact)?;
        serde_json::json!({
          "format": EMERGENCY_SPILL_APPLIED_FORMAT_V3,
          "applied_at": chrono::Utc::now().to_rfc3339(),
          "db_path": db_path.clone(),
          "database_id": hex::encode(database_id),
          "incident_id": hex::encode(incident_id),
          "manifest_length": artifact.manifest_length,
          "manifest_blake3": hex::encode(artifact.manifest_digest),
          "index_runtime_reconciliation_retained": artifact.index_runtime_state.is_some(),
          "wal_tail_bytes_present": report.wal_tail_bytes_present,
          "wal_tail_bytes_written": report.wal_tail_bytes_written,
        })
      }
    };
    let marker_path = artifact.directory.join("applied.json");
    let bytes = serde_json::to_vec_pretty(&marker).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    write_durable_file(&marker_path, &bytes)?;
  }
  Ok(())
}

fn required_typed_artifact_identities(artifact: &EmergencySpillArtifact) -> EngineResult<([u8; 16], [u8; 16])> {
  let database_id = artifact.database_id.ok_or_else(|| {
    EngineError::InvalidInput(format!(
      "typed emergency spill artifact {} is missing its database identity",
      artifact.manifest_path.display()
    ))
  })?;
  let incident_id = artifact.incident_id.ok_or_else(|| {
    EngineError::InvalidInput(format!(
      "typed emergency spill artifact {} is missing its incident identity",
      artifact.manifest_path.display()
    ))
  })?;
  Ok((database_id, incident_id))
}

pub fn update_v2_manifest_latest(
  manifest_path: &Path,
  database_id: [u8; 16],
  incident_id: [u8; 16],
  latest_failure_at_ms: i64,
  latest_failure: &str,
) -> EngineResult<()> {
  update_manifest_latest(manifest_path, database_id, incident_id, latest_failure_at_ms, latest_failure, Some(EMERGENCY_SPILL_FORMAT_V2))
}

pub fn update_typed_manifest_latest(
  manifest_path: &Path,
  database_id: [u8; 16],
  incident_id: [u8; 16],
  latest_failure_at_ms: i64,
  latest_failure: &str,
) -> EngineResult<()> {
  update_manifest_latest(manifest_path, database_id, incident_id, latest_failure_at_ms, latest_failure, None)
}

fn update_manifest_latest(
  manifest_path: &Path,
  database_id: [u8; 16],
  incident_id: [u8; 16],
  latest_failure_at_ms: i64,
  latest_failure: &str,
  required_format: Option<&str>,
) -> EngineResult<()> {
  let bytes = read_small_file_no_follow(manifest_path, MANIFEST_SIZE_CAP)?;
  let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  let format = manifest.get("format").and_then(|value| value.as_str());
  if !matches!(format, Some(EMERGENCY_SPILL_FORMAT_V2) | Some(EMERGENCY_SPILL_FORMAT_V3))
    || required_format.is_some_and(|required| format != Some(required))
    || required_hex_array::<16>(&manifest, "database_id")? != database_id
    || required_hex_array::<16>(&manifest, "incident_id")? != incident_id
  {
    return Err(EngineError::InvalidInput("refusing to update foreign emergency spill evidence".to_string()));
  }
  let first_failure_at_ms = required_i64(&manifest, "first_failure_at_ms")?;
  if latest_failure_at_ms < first_failure_at_ms {
    return Err(EngineError::InvalidInput("latest emergency spill failure precedes first failure".to_string()));
  }
  let object =
    manifest.as_object_mut().ok_or_else(|| EngineError::InvalidInput("emergency spill manifest must be a JSON object".to_string()))?;
  object.insert("latest_failure_at_ms".to_string(), serde_json::json!(latest_failure_at_ms));
  object.insert("latest_failure".to_string(), serde_json::json!(latest_failure));
  object.insert("failure".to_string(), serde_json::json!(latest_failure));
  let updated = serde_json::to_vec_pretty(&manifest).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  if updated.len() as u64 > MANIFEST_SIZE_CAP {
    return Err(EngineError::InvalidInput("updated emergency spill manifest exceeds its size cap".to_string()));
  }
  let temp_path = manifest_path.with_file_name(format!("manifest.update-{}.tmp", uuid::Uuid::new_v4().simple()));
  if let Err(error) = write_durable_file(&temp_path, &updated) {
    let cleanup = fs::remove_file(&temp_path).err();
    return Err(EngineError::DurabilityFailure(format!(
      "failed to write updated emergency spill evidence: {error}; cleanup error: {cleanup:?}"
    )));
  }
  if let Err(error) = rename_durable(&temp_path, manifest_path) {
    let cleanup = fs::remove_file(&temp_path).err();
    return Err(EngineError::DurabilityFailure(format!(
      "failed to publish updated emergency spill evidence: {error}; cleanup error: {cleanup:?}"
    )));
  }
  Ok(())
}

fn apply_one_wal_tail(
  db_path: &Path,
  wal_tail_path: &Path,
  copy_start: u64,
  evidence: Option<&EmergencySpillComponent>,
) -> EngineResult<(u64, u64)> {
  let mut wal_file = open_regular_file_no_follow(wal_tail_path)?;
  let wal_length = wal_file.metadata()?.len();
  if let Some(evidence) = evidence {
    validate_open_component(&mut wal_file, evidence)?;
  }
  if wal_length == 0 {
    return Ok((0, 0));
  }

  let mut database_options = OpenOptions::new();
  database_options.read(true).write(true);
  configure_no_follow(&mut database_options);
  let mut db_file = database_options.open(db_path)?;
  let db_len = db_file.metadata()?.len();
  if db_len < copy_start {
    return Err(EngineError::InvalidInput(format!(
      "cannot apply WAL tail {} at offset {}: database is only {} bytes",
      wal_tail_path.display(),
      copy_start,
      db_len
    )));
  }

  let existing_overlap = db_len.saturating_sub(copy_start).min(wal_length);
  db_file.seek(SeekFrom::Start(copy_start))?;
  wal_file.seek(SeekFrom::Start(0))?;
  let mut compared = 0u64;
  let mut buffer = vec![0u8; 1024 * 1024];
  let mut existing = vec![0u8; 1024 * 1024];
  while compared < existing_overlap {
    let width = (existing_overlap - compared).min(buffer.len() as u64) as usize;
    wal_file.read_exact(&mut buffer[..width])?;
    db_file.read_exact(&mut existing[..width])?;
    if existing[..width] != buffer[..width] {
      return Err(EngineError::InvalidInput(format!(
        "refusing to apply WAL tail {}: database bytes at {}..{} differ from spill",
        wal_tail_path.display(),
        copy_start + compared,
        copy_start + compared + width as u64
      )));
    }
    compared += width as u64;
  }

  db_file.seek(SeekFrom::Start(copy_start + existing_overlap))?;
  let mut written = 0u64;
  while existing_overlap + written < wal_length {
    let width = (wal_length - existing_overlap - written).min(buffer.len() as u64) as usize;
    wal_file.read_exact(&mut buffer[..width])?;
    db_file.write_all(&buffer[..width])?;
    written += width as u64;
  }
  sync_file_all_native(&db_file).map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
  sync_parent_dir(db_path)?;

  Ok((existing_overlap, written))
}

fn write_durable_file(path: &Path, bytes: &[u8]) -> EngineResult<()> {
  let mut file = create_new_regular_file_no_follow(path)?;
  file.write_all(bytes)?;
  sync_file_all_native(&file).map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
  sync_parent_dir(path)?;
  Ok(())
}

fn manifest_paths_in_base_dir(base_dir: &Path) -> EngineResult<Vec<PathBuf>> {
  let mut paths = Vec::new();
  let direct = base_dir.join("manifest.json");
  if direct.is_file() {
    paths.push(direct);
  }

  for entry in fs::read_dir(base_dir)? {
    let entry = entry?;
    reject_symlink(&entry.path(), "emergency spill base entry")?;
    let file_type = entry.file_type()?;
    if file_type.is_dir() {
      let manifest = entry.path().join("manifest.json");
      if manifest.is_file() {
        paths.push(manifest);
      }
    }
  }

  Ok(paths)
}

fn pending_paths_in_base_dir(base_dir: &Path) -> EngineResult<Vec<PathBuf>> {
  let mut paths = Vec::new();
  let direct = base_dir.join("pending.json");
  if direct.is_file() {
    paths.push(direct);
  }

  for entry in fs::read_dir(base_dir)? {
    let entry = entry?;
    if entry.file_type()?.is_dir() {
      let pending = entry.path().join("pending.json");
      if pending.is_file() {
        paths.push(pending);
      }
    }
  }
  Ok(paths)
}

fn pending_matches_database(pending_path: &Path, source_location_class: SpillLocationClass, db_path: &Path) -> EngineResult<bool> {
  reject_symlink(pending_path, "emergency spill pending record")?;
  let bytes = read_small_file_no_follow(pending_path, MANIFEST_SIZE_CAP)?;
  let pending: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  if !matches!(
    pending.get("format").and_then(|value| value.as_str()),
    Some(EMERGENCY_SPILL_PENDING_FORMAT_V2) | Some(EMERGENCY_SPILL_PENDING_FORMAT_V3)
  ) {
    return Err(EngineError::InvalidInput(format!("invalid emergency spill pending record {}", pending_path.display())));
  }
  let path_encoding = u16::try_from(required_u64(&pending, "path_encoding")?)
    .map_err(|_| EngineError::InvalidInput("emergency spill pending path encoding overflows u16".to_string()))?;
  let db_path_native = required_hex_bytes(&pending, "db_path_bytes")?;
  let pending_database_path = path_from_native_bytes(path_encoding, &db_path_native)?;
  if !paths_equivalent(&pending_database_path, db_path)? {
    return Ok(false);
  }
  let database_id = required_hex_array::<16>(&pending, "database_id")?;
  let incident_id = required_hex_array::<16>(&pending, "incident_id")?;
  if database_id == [0; 16] || incident_id == [0; 16] {
    return Err(EngineError::InvalidInput(format!("emergency spill pending record {} contains a zero identity", pending_path.display())));
  }
  let declared_class = SpillLocationClass::from_u64(required_u64(&pending, "source_location_class")?)?;
  if declared_class != source_location_class {
    return Err(EngineError::InvalidInput(format!(
      "emergency spill pending record {} location class does not match the scanned root",
      pending_path.display()
    )));
  }
  if required_u64(&pending, "creation_sequence")? == 0 {
    return Err(EngineError::InvalidInput("emergency spill pending creation sequence must be nonzero".to_string()));
  }
  required_i64(&pending, "first_failure_at_ms")?;
  Ok(true)
}

/// Identify ownership before validating artifact-specific evidence. Shared OS
/// spill roots may contain incidents for many databases; corruption or policy
/// differences in a foreign incident must not block an unrelated database.
/// Once the canonical path matches, the full parser remains fail-closed.
fn manifest_matches_database(manifest_path: &Path, db_path: &Path) -> EngineResult<Option<bool>> {
  reject_symlink(manifest_path, "emergency spill manifest")?;
  let bytes = read_small_file_no_follow(manifest_path, MANIFEST_SIZE_CAP)?;
  let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  match manifest.get("format").and_then(|value| value.as_str()) {
    Some(EMERGENCY_SPILL_FORMAT) => match manifest.get("db_path").and_then(|value| value.as_str()) {
      Some(path) => Ok(Some(paths_equivalent(Path::new(path), db_path)?)),
      None => Ok(Some(false)),
    },
    Some(EMERGENCY_SPILL_FORMAT_V2) | Some(EMERGENCY_SPILL_FORMAT_V3) => {
      let path_encoding = u16::try_from(required_u64(&manifest, "path_encoding")?)
        .map_err(|_| EngineError::InvalidInput("emergency spill manifest path encoding overflows u16".to_string()))?;
      let native = required_hex_bytes(&manifest, "db_path_bytes")?;
      let declared_path = path_from_native_bytes(path_encoding, &native)?;
      Ok(Some(paths_equivalent(&declared_path, db_path)?))
    }
    _ => Ok(None),
  }
}

fn parse_manifest(manifest_path: &Path, source_location_class: SpillLocationClass) -> EngineResult<Option<EmergencySpillArtifact>> {
  reject_symlink(manifest_path, "emergency spill manifest")?;
  let bytes = read_small_file_no_follow(manifest_path, MANIFEST_SIZE_CAP)?;
  let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  let format_version = match manifest.get("format").and_then(|value| value.as_str()) {
    Some(EMERGENCY_SPILL_FORMAT) => EmergencySpillFormatVersion::V1,
    Some(EMERGENCY_SPILL_FORMAT_V2) => EmergencySpillFormatVersion::V2,
    Some(EMERGENCY_SPILL_FORMAT_V3) => EmergencySpillFormatVersion::V3,
    _ => return Ok(None),
  };

  let directory = required_parent(manifest_path, "emergency spill manifest")?.to_path_buf();
  reject_symlink(&directory, "emergency spill artifact directory")?;
  let attempted_at = manifest.get("attempted_at").and_then(|value| value.as_str()).map(str::to_string);
  let sort_millis = match attempted_at.as_deref() {
    Some(value) => parse_rfc3339_millis(value).ok_or_else(|| {
      EngineError::InvalidInput(format!("emergency spill manifest {} has an invalid attempted_at timestamp", manifest_path.display()))
    })?,
    None => modified_millis(manifest_path)?,
  };
  let db_path = manifest.get("db_path").and_then(|value| value.as_str()).map(str::to_string);
  let context = manifest.get("context").and_then(|value| value.as_str()).map(str::to_string);
  let failure = manifest.get("failure").and_then(|value| value.as_str()).map(str::to_string);
  let first_failure = manifest.get("first_failure").and_then(|value| value.as_str()).map(str::to_string).or_else(|| failure.clone());
  let latest_failure = manifest.get("latest_failure").and_then(|value| value.as_str()).map(str::to_string).or_else(|| failure.clone());
  let manifest_digest = *blake3::hash(&bytes).as_bytes();
  let manifest_length = bytes.len() as u64;

  let (
    database_id,
    incident_id,
    path_encoding,
    creation_sequence,
    first_failure_at_ms,
    latest_failure_at_ms,
    failed_operation,
    os_error_class,
    os_error_code,
    last_selected_header_sequence,
    last_durable_write_sequence,
    last_durable_publication_sequence,
    db_path_native,
    components,
  ) = match format_version {
    EmergencySpillFormatVersion::V1 => {
      (None, None, native_path_encoding(), 0, sort_millis, sort_millis, None, None, None, None, None, None, None, Vec::new())
    }
    EmergencySpillFormatVersion::V2 | EmergencySpillFormatVersion::V3 => {
      let database_id = required_hex_array::<16>(&manifest, "database_id")?;
      let incident_id = required_hex_array::<16>(&manifest, "incident_id")?;
      if database_id == [0u8; 16] || incident_id == [0u8; 16] {
        return Err(EngineError::InvalidInput(format!("emergency spill manifest {} contains a zero identity", manifest_path.display())));
      }
      let declared_class = SpillLocationClass::from_u64(required_u64(&manifest, "source_location_class")?)?;
      if declared_class != source_location_class {
        return Err(EngineError::InvalidInput(format!(
          "emergency spill manifest {} location class does not match the scanned root",
          manifest_path.display()
        )));
      }
      let path_encoding = required_u64(&manifest, "path_encoding")?;
      if !matches!(path_encoding, 1 | 2) {
        return Err(EngineError::InvalidInput(format!("invalid emergency spill path encoding {path_encoding}")));
      }
      let db_path_native = required_hex_bytes(&manifest, "db_path_bytes")?;
      path_from_native_bytes(path_encoding as u16, &db_path_native)?;
      let creation_sequence = required_u64(&manifest, "creation_sequence")?;
      if creation_sequence == 0 {
        return Err(EngineError::InvalidInput("emergency spill creation sequence must be nonzero".to_string()));
      }
      let first_failure_at_ms = required_i64(&manifest, "first_failure_at_ms")?;
      let latest_failure_at_ms = required_i64(&manifest, "latest_failure_at_ms")?;
      if latest_failure_at_ms < first_failure_at_ms {
        return Err(EngineError::InvalidInput("emergency spill latest failure precedes first failure".to_string()));
      }
      let typed_evidence = parse_optional_typed_failure_evidence(&manifest)?;
      let components = parse_typed_components(&manifest, &directory, format_version)?;
      (
        Some(database_id),
        Some(incident_id),
        path_encoding as u16,
        creation_sequence,
        first_failure_at_ms,
        latest_failure_at_ms,
        typed_evidence.map(|evidence| evidence.0),
        typed_evidence.map(|evidence| evidence.1),
        typed_evidence.map(|evidence| evidence.2),
        typed_evidence.map(|evidence| evidence.3),
        typed_evidence.map(|evidence| evidence.4),
        typed_evidence.map(|evidence| evidence.5),
        Some(db_path_native),
        components,
      )
    }
  };

  let index_runtime_state = match format_version {
    EmergencySpillFormatVersion::V3 => {
      let evidence = components
        .iter()
        .find(|component| component.kind == "index_runtime_state")
        .ok_or_else(|| EngineError::InvalidInput("v3 emergency spill manifest is missing index runtime state".to_string()))?;
      let state = read_index_runtime_state(evidence)?;
      if manifest.get("hash_algorithm").and_then(serde_json::Value::as_str) != Some(format!("{:?}", state.hash_algorithm).as_str()) {
        return Err(EngineError::InvalidInput("v3 emergency spill runtime state disagrees with manifest hash identity".to_string()));
      }
      Some(state)
    }
    EmergencySpillFormatVersion::V1 | EmergencySpillFormatVersion::V2 => None,
  };

  let hot_tail_path = match format_version {
    EmergencySpillFormatVersion::V1 => path_from_manifest_or_default(&manifest, "hot_tail_path", &directory, "hot-tail.bin")?,
    EmergencySpillFormatVersion::V2 | EmergencySpillFormatVersion::V3 => {
      components.iter().find(|component| component.kind == "hot_tail").map(|component| component.path.clone())
    }
  };
  let wal_tail_path = match format_version {
    EmergencySpillFormatVersion::V1 => path_from_manifest_or_default(&manifest, "wal_tail_path", &directory, "wal-tail.bin")?,
    EmergencySpillFormatVersion::V2 | EmergencySpillFormatVersion::V3 => {
      components.iter().find(|component| component.kind == "wal_tail").map(|component| component.path.clone())
    }
  };
  let (wal_tail_bytes, hot_tail_writes, hot_tail_voids, wal_tail_truncated) = match format_version {
    EmergencySpillFormatVersion::V1 => (
      optional_u64(&manifest, "wal_tail_bytes")?.unwrap_or(0),
      optional_u64(&manifest, "hot_tail_writes")?.unwrap_or(0),
      optional_u64(&manifest, "hot_tail_voids")?.unwrap_or(0),
      optional_bool(&manifest, "wal_tail_truncated")?.unwrap_or(false),
    ),
    EmergencySpillFormatVersion::V2 | EmergencySpillFormatVersion::V3 => (
      required_u64(&manifest, "wal_tail_bytes")?,
      required_u64(&manifest, "hot_tail_writes")?,
      required_u64(&manifest, "hot_tail_voids")?,
      required_bool(&manifest, "wal_tail_truncated")?,
    ),
  };
  if matches!(format_version, EmergencySpillFormatVersion::V2 | EmergencySpillFormatVersion::V3) {
    let evidenced = components.iter().find(|component| component.kind == "wal_tail").map(|component| component.length).unwrap_or(0);
    if evidenced != wal_tail_bytes {
      return Err(EngineError::InvalidInput(format!(
        "emergency spill manifest {} WAL length disagrees with component evidence",
        manifest_path.display()
      )));
    }
  }

  Ok(Some(EmergencySpillArtifact {
    format_version,
    database_id,
    incident_id,
    source_location_class,
    path_encoding,
    creation_sequence,
    first_failure_at_ms,
    latest_failure_at_ms,
    failed_operation,
    os_error_class,
    os_error_code,
    last_selected_header_sequence,
    last_durable_write_sequence,
    last_durable_publication_sequence,
    directory,
    manifest_path: manifest_path.to_path_buf(),
    manifest_length,
    manifest_digest,
    components,
    index_runtime_state,
    attempted_at,
    sort_millis,
    db_path,
    db_path_native,
    context,
    failure,
    first_failure,
    latest_failure,
    hot_tail_path,
    wal_tail_path,
    hot_tail_writes: usize::try_from(hot_tail_writes)
      .map_err(|_| EngineError::InvalidInput("emergency spill hot_tail_writes overflows this platform".to_string()))?,
    hot_tail_voids: usize::try_from(hot_tail_voids)
      .map_err(|_| EngineError::InvalidInput("emergency spill hot_tail_voids overflows this platform".to_string()))?,
    wal_tail_copy_start: manifest.get("wal_tail_copy_start").and_then(|value| value.as_u64()),
    wal_tail_end: manifest.get("wal_tail_end").and_then(|value| value.as_u64()),
    wal_tail_bytes,
    wal_tail_truncated,
  }))
}

fn parse_optional_typed_failure_evidence(manifest: &serde_json::Value) -> EngineResult<Option<TypedFailureEvidence>> {
  const FIELDS: [&str; 6] = [
    "failed_operation",
    "os_error_class",
    "os_error_code",
    "last_selected_header_sequence",
    "last_durable_write_sequence",
    "last_durable_publication_sequence",
  ];
  let present = FIELDS.iter().filter(|field| manifest.get(**field).is_some()).count();
  if present == 0 {
    return Ok(None);
  }
  if present != FIELDS.len() {
    return Err(EngineError::InvalidInput("emergency spill typed failure evidence must be complete when present".to_string()));
  }
  let operation = u16::try_from(required_u64(manifest, FIELDS[0])?)
    .map_err(|_| EngineError::InvalidInput("emergency spill failed operation overflows u16".to_string()))?;
  let error_class = u16::try_from(required_u64(manifest, FIELDS[1])?)
    .map_err(|_| EngineError::InvalidInput("emergency spill OS error class overflows u16".to_string()))?;
  let error_code = i32::try_from(required_i64(manifest, FIELDS[2])?)
    .map_err(|_| EngineError::InvalidInput("emergency spill OS error code overflows i32".to_string()))?;
  let selected = required_u64(manifest, FIELDS[3])?;
  let durable_write = required_u64(manifest, FIELDS[4])?;
  let durable_publication = required_u64(manifest, FIELDS[5])?;
  if !crate::engine::durability_coordinator::DurabilityOperation::is_stable_id(operation)
    || !crate::engine::durability_coordinator::OsErrorClass::is_stable_id(error_class)
    || error_code == 0
    || selected == 0
    || durable_write == 0
    || durable_publication == 0
  {
    return Err(EngineError::InvalidInput("emergency spill typed failure evidence contains an invalid zero or enum value".to_string()));
  }
  Ok(Some((operation, error_class, error_code, selected, durable_write, durable_publication)))
}

fn path_from_manifest_or_default(
  manifest: &serde_json::Value,
  field: &str,
  directory: &Path,
  fallback_name: &str,
) -> EngineResult<Option<PathBuf>> {
  if let Some(path) = manifest.get(field).and_then(|value| value.as_str()).filter(|path| !path.trim().is_empty()) {
    let path = PathBuf::from(path);
    if !paths_equivalent(required_parent(&path, "legacy emergency spill component")?, directory)?
      || path.file_name().and_then(|name| name.to_str()) != Some(fallback_name)
    {
      return Err(EngineError::InvalidInput(format!("legacy emergency spill {field} escapes its artifact directory")));
    }
    reject_symlink(&path, "legacy emergency spill component")?;
    return Ok(Some(path));
  }
  let fallback = directory.join(fallback_name);
  if fallback.exists() {
    reject_symlink(&fallback, "legacy emergency spill component")?;
    Ok(Some(fallback))
  } else {
    Ok(None)
  }
}

fn artifact_applied(artifact: &EmergencySpillArtifact) -> EngineResult<bool> {
  let marker_path = artifact.directory.join("applied.json");
  if !marker_path.exists() {
    return Ok(false);
  }
  reject_symlink(&marker_path, "emergency spill applied marker")?;
  let bytes = read_small_file_no_follow(&marker_path, MANIFEST_SIZE_CAP)?;
  let marker: serde_json::Value = serde_json::from_slice(&bytes)
    .map_err(|error| EngineError::InvalidInput(format!("invalid emergency spill applied marker {}: {error}", marker_path.display())))?;
  let valid = match artifact.format_version {
    EmergencySpillFormatVersion::V1 => {
      let manifest_matches = match marker.get("manifest_path").and_then(|value| value.as_str()) {
        Some(path) => paths_equivalent(Path::new(path), &artifact.manifest_path)?,
        None => false,
      };
      let database_matches = match marker.get("db_path").and_then(|value| value.as_str()).zip(artifact.db_path.as_deref()) {
        Some((left, right)) => paths_equivalent(Path::new(left), Path::new(right))?,
        None => false,
      };
      marker.get("format").and_then(|value| value.as_str()) == Some(EMERGENCY_SPILL_APPLIED_FORMAT) && manifest_matches && database_matches
    }
    EmergencySpillFormatVersion::V2 => {
      marker.get("format").and_then(|value| value.as_str()) == Some(EMERGENCY_SPILL_APPLIED_FORMAT_V2)
        && marker_hex_matches(&marker, "database_id", artifact.database_id)
        && marker_hex_matches(&marker, "incident_id", artifact.incident_id)
        && marker.get("manifest_length").and_then(|value| value.as_u64()) == Some(artifact.manifest_length)
        && marker_hex_matches(&marker, "manifest_blake3", Some(artifact.manifest_digest))
    }
    EmergencySpillFormatVersion::V3 => {
      marker.get("format").and_then(|value| value.as_str()) == Some(EMERGENCY_SPILL_APPLIED_FORMAT_V3)
        && marker_hex_matches(&marker, "database_id", artifact.database_id)
        && marker_hex_matches(&marker, "incident_id", artifact.incident_id)
        && marker.get("manifest_length").and_then(|value| value.as_u64()) == Some(artifact.manifest_length)
        && marker_hex_matches(&marker, "manifest_blake3", Some(artifact.manifest_digest))
        && marker.get("index_runtime_reconciliation_retained").and_then(|value| value.as_bool()) == Some(true)
        && artifact.index_runtime_state.is_some()
    }
  };
  if !valid {
    return Err(EngineError::InvalidInput(format!("emergency spill applied marker {} does not match its artifact", marker_path.display())));
  }
  Ok(true)
}

fn artifact_matches_database(artifact: &EmergencySpillArtifact, db_path: &Path) -> EngineResult<bool> {
  if let Some(native) = artifact.db_path_native.as_deref() {
    let manifest_path = path_from_native_bytes(artifact.path_encoding, native)?;
    return paths_equivalent(&manifest_path, db_path);
  }
  let Some(manifest_db_path) = artifact.db_path.as_deref() else {
    return Ok(false);
  };
  paths_equivalent(Path::new(manifest_db_path), db_path)
}

fn paths_equivalent(left: &Path, right: &Path) -> EngineResult<bool> {
  if left == right {
    return Ok(true);
  }
  Ok(absolute_path(left)? == absolute_path(right)?)
}

fn absolute_path(path: &Path) -> EngineResult<PathBuf> {
  absolute_path_with_current_dir(path, std::env::current_dir)
}

fn absolute_path_with_current_dir<F>(path: &Path, current_dir: F) -> EngineResult<PathBuf>
where
  F: FnOnce() -> std::io::Result<PathBuf>,
{
  absolute_path_with_resolvers(path, current_dir, Path::canonicalize)
}

fn absolute_path_with_resolvers<F, C>(path: &Path, current_dir: F, canonicalize: C) -> EngineResult<PathBuf>
where
  F: FnOnce() -> std::io::Result<PathBuf>,
  C: FnOnce(&Path) -> std::io::Result<PathBuf>,
{
  match canonicalize(path) {
    Ok(canonical) => Ok(canonical),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      if path.is_absolute() {
        Ok(path.to_path_buf())
      } else {
        Ok(current_dir()?.join(path))
      }
    }
    Err(error) => Err(EngineError::IoError(error)),
  }
}

fn required_parent<'a>(path: &'a Path, context: &str) -> EngineResult<&'a Path> {
  path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .ok_or_else(|| EngineError::InvalidInput(format!("{context} path {} has no containing directory", path.display())))
}

fn parse_typed_components(
  manifest: &serde_json::Value,
  directory: &Path,
  version: EmergencySpillFormatVersion,
) -> EngineResult<Vec<EmergencySpillComponent>> {
  let rows = manifest
    .get("components")
    .and_then(|value| value.as_array())
    .ok_or_else(|| EngineError::InvalidInput("v2 emergency spill manifest is missing components".to_string()))?;
  let maximum_components = if version == EmergencySpillFormatVersion::V3 { 4 } else { 3 };
  if rows.len() > maximum_components {
    return Err(EngineError::InvalidInput(format!("typed emergency spill manifest has more than {maximum_components} components")));
  }
  let mut seen = HashSet::new();
  let mut components = Vec::with_capacity(rows.len());
  for row in rows {
    let kind = row
      .get("kind")
      .and_then(|value| value.as_str())
      .ok_or_else(|| EngineError::InvalidInput("emergency spill component is missing kind".to_string()))?;
    let expected_name = match kind {
      "hot_tail" => "hot-tail.bin",
      "wal_tail" => "wal-tail.bin",
      "index_buffer" => "index-buffer.json",
      "index_runtime_state" if version == EmergencySpillFormatVersion::V3 => "index-runtime-state.json",
      _ => return Err(EngineError::InvalidInput(format!("unknown emergency spill component kind {kind}"))),
    };
    if !seen.insert(kind) {
      return Err(EngineError::InvalidInput(format!("duplicate emergency spill component kind {kind}")));
    }
    let file_name = row
      .get("file_name")
      .and_then(|value| value.as_str())
      .ok_or_else(|| EngineError::InvalidInput("emergency spill component is missing file_name".to_string()))?;
    if file_name != expected_name {
      return Err(EngineError::InvalidInput(format!("emergency spill component {kind} has a non-canonical file name")));
    }
    let length = row
      .get("length")
      .and_then(|value| value.as_u64())
      .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill component {kind} has an invalid length")))?;
    let digest = row
      .get("blake3")
      .and_then(|value| value.as_str())
      .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill component {kind} is missing its digest")))?;
    let digest = decode_hex_array::<32>(digest, "emergency spill component digest")?;
    let evidence = EmergencySpillComponent { kind: kind.to_string(), path: directory.join(expected_name), length, digest };
    components.push(evidence);
  }
  Ok(components)
}

fn read_index_runtime_state(evidence: &EmergencySpillComponent) -> EngineResult<EmergencyIndexRuntimeStateV1> {
  if evidence.length > INDEX_RUNTIME_STATE_SIZE_CAP {
    return Err(EngineError::InvalidInput(format!("index runtime emergency state exceeds {INDEX_RUNTIME_STATE_SIZE_CAP} bytes")));
  }
  let mut file = open_regular_file_no_follow(&evidence.path)?;
  validate_open_component(&mut file, evidence)?;
  let mut bytes = Vec::with_capacity(evidence.length as usize);
  file.read_to_end(&mut bytes)?;
  let wire: EmergencyIndexRuntimeStateWireV1 =
    serde_json::from_slice(&bytes).map_err(|error| EngineError::InvalidInput(format!("invalid index runtime emergency state: {error}")))?;
  if wire.format != INDEX_RUNTIME_EMERGENCY_STATE_FORMAT_V1 {
    return Err(EngineError::InvalidInput("invalid index runtime emergency state format".to_string()));
  }
  let database_id = decode_hex_array::<16>(&wire.database_id, "index runtime database identity")?;
  let destination_physical_instance_id =
    decode_hex_array::<16>(&wire.destination_physical_instance_id, "index runtime destination identity")?;
  let runtime_id = decode_hex_array::<16>(&wire.runtime_id, "index runtime identity")?;
  if database_id == [0; 16] || destination_physical_instance_id == [0; 16] || runtime_id == [0; 16] {
    return Err(EngineError::InvalidInput("index runtime emergency state contains a zero identity".to_string()));
  }
  let hash_algorithm = crate::engine::HashAlgorithm::from_u16(wire.hash_algorithm)
    .ok_or_else(|| EngineError::InvalidInput("index runtime emergency state has an invalid hash algorithm".to_string()))?;
  if !(1..=5).contains(&wire.lifecycle) || !wire.reconciliation_required || !valid_runtime_reason(&wire.reason) {
    return Err(EngineError::InvalidInput(
      "index runtime emergency state has invalid lifecycle, reconciliation, or reason evidence".to_string(),
    ));
  }
  let workspace = wire
    .workspace
    .map(|workspace| {
      let path_bytes = hex::decode(&workspace.path_bytes)
        .map_err(|error| EngineError::InvalidInput(format!("invalid index runtime workspace path bytes: {error}")))?;
      let path = path_from_native_bytes(workspace.path_encoding, &path_bytes)?;
      if !path.is_absolute() || workspace.path.is_empty() {
        return Err(EngineError::InvalidInput("index runtime workspace path is not absolute or diagnostic path is empty".to_string()));
      }
      if workspace.path != path.display().to_string() {
        return Err(EngineError::InvalidInput("index runtime workspace path representations disagree".to_string()));
      }
      let workspace_id = decode_hex_array::<16>(&workspace.workspace_id, "index runtime workspace identity")?;
      if workspace_id == [0; 16] {
        return Err(EngineError::InvalidInput("index runtime emergency state contains a zero workspace identity".to_string()));
      }
      let selected_head = workspace
        .selected_head
        .map(|selected| {
          let manifest_digest = decode_hex_array::<32>(&selected.manifest_digest, "index runtime workspace manifest digest")?;
          if manifest_digest == [0; 32] || selected.durable_sequence == 0 || selected.durable_bytes == 0 {
            return Err(EngineError::InvalidInput("index runtime selected workspace head is not canonical".to_string()));
          }
          Ok(EmergencyIndexRuntimeSelectedHeadV1 {
            manifest_digest,
            durable_sequence: selected.durable_sequence,
            durable_bytes: selected.durable_bytes,
          })
        })
        .transpose()?;
      Ok(EmergencyIndexRuntimeWorkspaceV1 { path_encoding: workspace.path_encoding, path, workspace_id, selected_head })
    })
    .transpose()?;
  Ok(EmergencyIndexRuntimeStateV1 {
    database_id,
    destination_physical_instance_id,
    runtime_id,
    hash_algorithm,
    lifecycle: wire.lifecycle,
    highest_checkpoint_sequence: wire.highest_checkpoint_sequence,
    publication_in_flight: wire.publication_in_flight,
    soft_queued_notices: wire.soft_queued_notices,
    soft_retained_bytes: wire.soft_retained_bytes,
    soft_reconciliation_required: wire.soft_reconciliation_required,
    soft_lost_through_sequence: wire.soft_lost_through_sequence,
    producer_pending_tasks: wire.producer_pending_tasks,
    producer_pending_bytes: wire.producer_pending_bytes,
    producer_leased_tasks: wire.producer_leased_tasks,
    mutation_active_records: wire.mutation_active_records,
    mutation_active_bytes: wire.mutation_active_bytes,
    mutation_frozen_records: wire.mutation_frozen_records,
    mutation_frozen_bytes: wire.mutation_frozen_bytes,
    reconciliation_required: wire.reconciliation_required,
    reason: wire.reason,
    workspace,
  })
}

fn valid_runtime_reason(reason: &str) -> bool {
  !reason.is_empty() && reason.len() <= 128 && reason.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

pub(crate) fn encode_index_runtime_state_v1(state: &EmergencyIndexRuntimeStateV1) -> EngineResult<Vec<u8>> {
  if state.database_id == [0; 16]
    || state.destination_physical_instance_id == [0; 16]
    || state.runtime_id == [0; 16]
    || !(1..=5).contains(&state.lifecycle)
    || !state.reconciliation_required
    || !valid_runtime_reason(&state.reason)
  {
    return Err(EngineError::InvalidInput("invalid index runtime emergency state identity or reconciliation evidence".to_string()));
  }
  let workspace = state
    .workspace
    .as_ref()
    .map(|workspace| {
      if workspace.workspace_id == [0; 16]
        || workspace.path_encoding != native_path_encoding()
        || !workspace.path.is_absolute()
        || workspace.path.as_os_str().is_empty()
      {
        return Err(EngineError::InvalidInput("invalid index runtime emergency workspace evidence".to_string()));
      }
      let selected_head = workspace
        .selected_head
        .as_ref()
        .map(|selected| {
          if selected.manifest_digest == [0; 32] || selected.durable_sequence == 0 || selected.durable_bytes == 0 {
            return Err(EngineError::InvalidInput("invalid index runtime selected workspace head".to_string()));
          }
          Ok(EmergencyIndexRuntimeSelectedHeadWireV1 {
            manifest_digest: hex::encode(selected.manifest_digest),
            durable_sequence: selected.durable_sequence,
            durable_bytes: selected.durable_bytes,
          })
        })
        .transpose()?;
      Ok(EmergencyIndexRuntimeWorkspaceWireV1 {
        path_encoding: workspace.path_encoding,
        path: workspace.path.display().to_string(),
        path_bytes: hex::encode(native_path_bytes(&workspace.path)),
        workspace_id: hex::encode(workspace.workspace_id),
        selected_head,
      })
    })
    .transpose()?;
  let wire = EmergencyIndexRuntimeStateWireV1 {
    format: INDEX_RUNTIME_EMERGENCY_STATE_FORMAT_V1.to_string(),
    database_id: hex::encode(state.database_id),
    destination_physical_instance_id: hex::encode(state.destination_physical_instance_id),
    runtime_id: hex::encode(state.runtime_id),
    hash_algorithm: state.hash_algorithm.to_u16(),
    lifecycle: state.lifecycle,
    highest_checkpoint_sequence: state.highest_checkpoint_sequence,
    publication_in_flight: state.publication_in_flight,
    soft_queued_notices: state.soft_queued_notices,
    soft_retained_bytes: state.soft_retained_bytes,
    soft_reconciliation_required: state.soft_reconciliation_required,
    soft_lost_through_sequence: state.soft_lost_through_sequence,
    producer_pending_tasks: state.producer_pending_tasks,
    producer_pending_bytes: state.producer_pending_bytes,
    producer_leased_tasks: state.producer_leased_tasks,
    mutation_active_records: state.mutation_active_records,
    mutation_active_bytes: state.mutation_active_bytes,
    mutation_frozen_records: state.mutation_frozen_records,
    mutation_frozen_bytes: state.mutation_frozen_bytes,
    reconciliation_required: state.reconciliation_required,
    reason: state.reason.clone(),
    workspace,
  };
  let bytes = serde_json::to_vec_pretty(&wire).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  if bytes.len() as u64 > INDEX_RUNTIME_STATE_SIZE_CAP {
    return Err(EngineError::InvalidInput(format!("index runtime emergency state exceeds {INDEX_RUNTIME_STATE_SIZE_CAP} bytes")));
  }
  Ok(bytes)
}

fn required_u64(manifest: &serde_json::Value, field: &str) -> EngineResult<u64> {
  manifest
    .get(field)
    .and_then(|value| value.as_u64())
    .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill manifest is missing unsigned field {field}")))
}

fn optional_u64(manifest: &serde_json::Value, field: &str) -> EngineResult<Option<u64>> {
  match manifest.get(field) {
    None => Ok(None),
    Some(value) => value
      .as_u64()
      .map(Some)
      .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill manifest field {field} must be an unsigned integer"))),
  }
}

fn required_bool(manifest: &serde_json::Value, field: &str) -> EngineResult<bool> {
  manifest
    .get(field)
    .and_then(|value| value.as_bool())
    .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill manifest is missing boolean field {field}")))
}

fn optional_bool(manifest: &serde_json::Value, field: &str) -> EngineResult<Option<bool>> {
  match manifest.get(field) {
    None => Ok(None),
    Some(value) => value
      .as_bool()
      .map(Some)
      .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill manifest field {field} must be a boolean"))),
  }
}

fn required_i64(manifest: &serde_json::Value, field: &str) -> EngineResult<i64> {
  manifest
    .get(field)
    .and_then(|value| value.as_i64())
    .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill manifest is missing signed field {field}")))
}

fn required_hex_array<const N: usize>(manifest: &serde_json::Value, field: &str) -> EngineResult<[u8; N]> {
  let value = manifest
    .get(field)
    .and_then(|value| value.as_str())
    .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill manifest is missing hexadecimal field {field}")))?;
  decode_hex_array(value, field)
}

fn required_hex_bytes(manifest: &serde_json::Value, field: &str) -> EngineResult<Vec<u8>> {
  let value = manifest
    .get(field)
    .and_then(|value| value.as_str())
    .ok_or_else(|| EngineError::InvalidInput(format!("emergency spill manifest is missing hexadecimal field {field}")))?;
  hex::decode(value).map_err(|error| EngineError::InvalidInput(format!("invalid {field}: {error}")))
}

fn decode_hex_array<const N: usize>(value: &str, field: &str) -> EngineResult<[u8; N]> {
  let bytes = hex::decode(value).map_err(|error| EngineError::InvalidInput(format!("invalid {field}: {error}")))?;
  bytes.try_into().map_err(|bytes: Vec<u8>| EngineError::InvalidInput(format!("invalid {field} width {}, expected {N}", bytes.len())))
}

fn marker_hex_matches<const N: usize>(marker: &serde_json::Value, field: &str, expected: Option<[u8; N]>) -> bool {
  let Some(expected) = expected else {
    return false;
  };
  marker.get(field).and_then(|value| value.as_str()).is_some_and(|value| hex::decode(value).ok().as_deref() == Some(expected.as_slice()))
}

fn validate_open_component(file: &mut fs::File, evidence: &EmergencySpillComponent) -> EngineResult<()> {
  let actual_length = file.metadata()?.len();
  if actual_length != evidence.length {
    return Err(EngineError::InvalidInput(format!(
      "emergency spill component {} length {} does not match evidence {}",
      evidence.path.display(),
      actual_length,
      evidence.length
    )));
  }
  file.seek(SeekFrom::Start(0))?;
  let mut hasher = blake3::Hasher::new();
  let mut buffer = vec![0u8; 1024 * 1024];
  loop {
    let read = file.read(&mut buffer)?;
    if read == 0 {
      break;
    }
    hasher.update(&buffer[..read]);
  }
  let actual = *hasher.finalize().as_bytes();
  if actual != evidence.digest {
    return Err(EngineError::InvalidInput(format!("emergency spill component {} digest does not match evidence", evidence.path.display())));
  }
  file.seek(SeekFrom::Start(0))?;
  Ok(())
}

fn validate_artifact_components(artifact: &EmergencySpillArtifact) -> EngineResult<()> {
  for component in &artifact.components {
    let mut file = open_regular_file_no_follow(&component.path)?;
    validate_open_component(&mut file, component)?;
  }
  Ok(())
}

pub(crate) fn validate_artifact_evidence(artifact: &EmergencySpillArtifact) -> EngineResult<()> {
  let manifest = EmergencySpillComponent {
    kind: "manifest".to_string(),
    path: artifact.manifest_path.clone(),
    length: artifact.manifest_length,
    digest: artifact.manifest_digest,
  };
  let mut file = open_regular_file_no_follow(&manifest.path)?;
  validate_open_component(&mut file, &manifest)?;
  let retained = parse_manifest(&artifact.manifest_path, artifact.source_location_class)?
    .ok_or_else(|| EngineError::InvalidInput("emergency spill artifact manifest no longer has a supported format".to_string()))?;
  if retained != *artifact {
    return Err(EngineError::InvalidInput(
      "emergency spill artifact metadata disagrees with its retained manifest or component evidence".to_string(),
    ));
  }
  if artifact.format_version == EmergencySpillFormatVersion::V3
    && (artifact.index_runtime_state.is_none() || !artifact.components.iter().any(|component| component.kind == "index_runtime_state"))
  {
    return Err(EngineError::InvalidInput("v3 emergency spill is missing retained index runtime reconciliation evidence".to_string()));
  }
  validate_artifact_components(artifact)
}

pub(crate) fn validate_artifact_for_database(artifact: &EmergencySpillArtifact, db_path: &Path) -> EngineResult<()> {
  validate_artifact_evidence(artifact)?;
  if !artifact_matches_database(artifact, db_path)? {
    return Err(EngineError::InvalidInput(format!(
      "emergency spill artifact {} belongs to a different database",
      artifact.manifest_path.display()
    )));
  }
  Ok(())
}

fn read_small_file_no_follow(path: &Path, maximum_length: u64) -> EngineResult<Vec<u8>> {
  let mut file = open_regular_file_no_follow(path)?;
  let length = file.metadata()?.len();
  if length > maximum_length {
    return Err(EngineError::InvalidInput(format!("emergency spill metadata {} exceeds {} bytes", path.display(), maximum_length)));
  }
  let mut bytes = Vec::with_capacity(length as usize);
  file.read_to_end(&mut bytes)?;
  Ok(bytes)
}

pub(crate) fn open_regular_file_no_follow(path: &Path) -> EngineResult<fs::File> {
  reject_symlink(path, "emergency spill file")?;
  let mut options = OpenOptions::new();
  options.read(true);
  configure_no_follow(&mut options);
  let file = options.open(path)?;
  if !file.metadata()?.is_file() {
    return Err(EngineError::InvalidInput(format!("emergency spill path {} is not a regular file", path.display())));
  }
  Ok(file)
}

pub(crate) fn open_regular_file_read_write_no_follow(path: &Path) -> EngineResult<fs::File> {
  reject_symlink(path, "private workspace file")?;
  let mut options = OpenOptions::new();
  options.read(true).write(true);
  configure_no_follow(&mut options);
  let file = options.open(path)?;
  if !file.metadata()?.is_file() {
    return Err(EngineError::InvalidInput(format!("private workspace path {} is not a regular file", path.display())));
  }
  Ok(file)
}

pub(crate) fn create_new_regular_file_no_follow(path: &Path) -> EngineResult<fs::File> {
  create_new_regular_file_no_follow_with_access(path, false)
}

pub(crate) fn create_new_regular_file_read_write_no_follow(path: &Path) -> EngineResult<fs::File> {
  create_new_regular_file_no_follow_with_access(path, true)
}

fn create_new_regular_file_no_follow_with_access(path: &Path, read: bool) -> EngineResult<fs::File> {
  let mut options = OpenOptions::new();
  options.read(read).write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  configure_no_follow(&mut options);
  let file = options.open(path)?;
  if !file.metadata()?.is_file() {
    return Err(EngineError::InvalidInput(format!("emergency spill path {} is not a regular file", path.display())));
  }
  Ok(file)
}

pub(crate) fn create_private_dir_all(path: &Path) -> EngineResult<()> {
  let mut builder = fs::DirBuilder::new();
  builder.recursive(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }
  builder.create(path)?;
  reject_symlink(path, "emergency spill base directory")
}

pub(crate) fn create_private_dir(path: &Path) -> EngineResult<()> {
  #[cfg(unix)]
  let mut builder = fs::DirBuilder::new();
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }
  #[cfg(not(unix))]
  let builder = fs::DirBuilder::new();
  builder.create(path)?;
  reject_symlink(path, "emergency spill artifact directory")
}

pub(crate) fn reject_symlink(path: &Path, role: &str) -> EngineResult<()> {
  let metadata = fs::symlink_metadata(path)?;
  if metadata.file_type().is_symlink() {
    return Err(EngineError::InvalidInput(format!("{role} {} is a symlink; no-follow validation refused it", path.display())));
  }
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
      return Err(EngineError::InvalidInput(format!("{role} {} is a reparse point; no-follow validation refused it", path.display())));
    }
  }
  Ok(())
}

fn configure_no_follow(options: &mut OpenOptions) {
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
  }
  #[cfg(windows)]
  {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
  }
}

pub(crate) fn native_path_encoding() -> u16 {
  if cfg!(windows) {
    2
  } else {
    1
  }
}

#[cfg(unix)]
pub(crate) fn native_path_bytes(path: &Path) -> Vec<u8> {
  use std::os::unix::ffi::OsStrExt;
  path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
pub(crate) fn native_path_bytes(path: &Path) -> Vec<u8> {
  use std::os::windows::ffi::OsStrExt;
  path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn native_path_bytes(path: &Path) -> Vec<u8> {
  path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn path_from_native_bytes(encoding: u16, bytes: &[u8]) -> EngineResult<PathBuf> {
  use std::ffi::OsString;
  use std::os::unix::ffi::OsStringExt;

  if bytes.is_empty() {
    return Err(EngineError::InvalidInput("emergency spill database path is empty".to_string()));
  }
  if encoding != 1 {
    return Err(EngineError::InvalidInput(format!("native Unix repair cannot interpret emergency spill path encoding {encoding}")));
  }
  Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn path_from_native_bytes(encoding: u16, bytes: &[u8]) -> EngineResult<PathBuf> {
  use std::ffi::OsString;
  use std::os::windows::ffi::OsStringExt;

  if bytes.is_empty() {
    return Err(EngineError::InvalidInput("emergency spill database path is empty".to_string()));
  }
  if encoding != 2 || bytes.len() % 2 != 0 {
    return Err(EngineError::InvalidInput(format!("native Windows repair cannot interpret emergency spill path encoding {encoding}")));
  }
  let words = bytes.chunks_exact(2).map(|word| u16::from_le_bytes([word[0], word[1]])).collect::<Vec<_>>();
  Ok(PathBuf::from(OsString::from_wide(&words)))
}

#[cfg(not(any(unix, windows)))]
fn path_from_native_bytes(encoding: u16, bytes: &[u8]) -> EngineResult<PathBuf> {
  if bytes.is_empty() {
    return Err(EngineError::InvalidInput("emergency spill database path is empty".to_string()));
  }
  if encoding != 1 {
    return Err(EngineError::InvalidInput(format!("native repair cannot interpret emergency spill path encoding {encoding}")));
  }
  let path =
    std::str::from_utf8(bytes).map_err(|error| EngineError::InvalidInput(format!("invalid native emergency spill path: {error}")))?;
  Ok(PathBuf::from(path))
}

fn parse_rfc3339_millis(value: &str) -> Option<i64> {
  chrono::DateTime::parse_from_rfc3339(value).ok().map(|timestamp| timestamp.timestamp_millis())
}

fn modified_millis(path: &Path) -> std::io::Result<i64> {
  let modified = path.metadata()?.modified()?;
  let duration = match modified.duration_since(UNIX_EPOCH) {
    Ok(duration) => duration,
    Err(_) => std::time::Duration::ZERO,
  };
  Ok(duration.as_millis().min(i64::MAX as u128) as i64)
}

fn dedupe_locations(locations: Vec<EmergencySpillLocation>) -> Vec<EmergencySpillLocation> {
  let mut seen = HashSet::new();
  let mut deduped = Vec::new();
  for location in locations {
    if seen.insert(location.path.clone()) {
      deduped.push(location);
    }
  }
  deduped
}

#[cfg(test)]
#[path = "../../spec/engine/emergency_spill_path_internal_spec.rs"]
mod emergency_spill_path_internal_spec;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn scan_orders_unapplied_matching_artifacts_oldest_first() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.aeordb");
    fs::write(&db_path, b"db").unwrap();
    let base = temp_dir.path().join("spill-base");
    fs::create_dir_all(&base).unwrap();

    write_manifest(&base.join("newer"), &db_path, "2026-06-15T10:00:00Z");
    write_manifest(&base.join("older"), &db_path, "2026-06-15T09:00:00Z");
    write_manifest(&base.join("other-db"), &temp_dir.path().join("other.aeordb"), "2026-06-15T08:00:00Z");
    let marker = serde_json::json!({
      "format": EMERGENCY_SPILL_APPLIED_FORMAT,
      "db_path": db_path.display().to_string(),
      "manifest_path": base.join("newer").join("manifest.json").display().to_string(),
    });
    fs::write(base.join("newer").join("applied.json"), serde_json::to_vec(&marker).unwrap()).unwrap();

    let artifacts = scan_for_database_with_dirs(&db_path, &[base]).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].directory.ends_with("older"));
  }

  #[test]
  fn apply_wal_tail_appends_only_missing_matching_bytes() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.aeordb");
    fs::write(&db_path, b"abcdef").unwrap();

    let artifact_dir = temp_dir.path().join("artifact");
    fs::create_dir(&artifact_dir).unwrap();
    let tail_path = artifact_dir.join("wal-tail.bin");
    fs::write(&tail_path, b"defghi").unwrap();
    let manifest_path = artifact_dir.join("manifest.json");
    let manifest = serde_json::json!({
      "format": EMERGENCY_SPILL_FORMAT,
      "attempted_at": "2026-08-19T00:00:00Z",
      "db_path": db_path.display().to_string(),
      "wal_tail_path": tail_path.display().to_string(),
      "wal_tail_copy_start": 3,
      "wal_tail_end": 9,
      "wal_tail_bytes": 6,
      "hot_tail_writes": 0,
      "hot_tail_voids": 0,
      "wal_tail_truncated": false,
    });
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    let artifact = parse_manifest(&manifest_path, SpillLocationClass::ConfiguredFallback).unwrap().unwrap();

    let report = apply_wal_tails_to_database(&db_path, &[artifact]).unwrap();
    assert_eq!(report.wal_tail_bytes_present, 3);
    assert_eq!(report.wal_tail_bytes_written, 3);
    assert_eq!(fs::read(&db_path).unwrap(), b"abcdefghi");
  }

  fn write_manifest(directory: &Path, db_path: &Path, attempted_at: &str) {
    fs::create_dir_all(directory).unwrap();
    let manifest = serde_json::json!({
      "format": EMERGENCY_SPILL_FORMAT,
      "attempted_at": attempted_at,
      "db_path": db_path.display().to_string(),
      "hot_tail_writes": 1,
      "hot_tail_voids": 0,
      "wal_tail_bytes": 0,
    });
    fs::write(directory.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
  }
}
