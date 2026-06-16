use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::durability::sync_parent_dir;
use crate::engine::errors::{EngineError, EngineResult};

pub const EMERGENCY_SPILL_FORMAT: &str = "aeordb-emergency-spill-v1";
pub const EMERGENCY_SPILL_APPLIED_FORMAT: &str = "aeordb-emergency-spill-applied-v1";

#[derive(Debug, Clone)]
pub struct EmergencySpillArtifact {
  pub directory: PathBuf,
  pub manifest_path: PathBuf,
  pub attempted_at: Option<String>,
  pub sort_millis: i64,
  pub db_path: Option<String>,
  pub context: Option<String>,
  pub failure: Option<String>,
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

#[derive(Debug, Serialize, Deserialize)]
struct AppliedMarker {
  format: String,
  applied_at: String,
  db_path: String,
  manifest_path: String,
  wal_tail_bytes_present: u64,
  wal_tail_bytes_written: u64,
}

pub fn emergency_spill_base_dirs() -> Vec<PathBuf> {
  let mut dirs = Vec::new();
  if let Ok(path) = std::env::var("AEORDB_EMERGENCY_SPILL_DIR") {
    if !path.trim().is_empty() {
      dirs.push(PathBuf::from(path));
    }
  }

  #[cfg(target_os = "windows")]
  {
    if let Ok(path) = std::env::var("LOCALAPPDATA") {
      dirs.push(PathBuf::from(path).join("AeorDB").join("emergency-spill"));
    } else if let Ok(path) = std::env::var("APPDATA") {
      dirs.push(PathBuf::from(path).join("AeorDB").join("emergency-spill"));
    }
  }
  #[cfg(target_os = "macos")]
  {
    if let Ok(home) = std::env::var("HOME") {
      dirs.push(PathBuf::from(home).join("Library").join("Application Support").join("aeordb").join("emergency-spill"));
    }
  }
  #[cfg(all(unix, not(target_os = "macos")))]
  {
    if let Ok(path) = std::env::var("XDG_DATA_HOME") {
      dirs.push(PathBuf::from(path).join("aeordb").join("emergency-spill"));
    } else if let Ok(home) = std::env::var("HOME") {
      dirs.push(PathBuf::from(home).join(".local").join("share").join("aeordb").join("emergency-spill"));
    }
  }

  dirs.push(std::env::temp_dir().join("aeordb-emergency-spill"));
  dedupe_paths(dirs)
}

pub fn scan_unapplied_for_database(db_path: impl AsRef<Path>) -> EngineResult<Vec<EmergencySpillArtifact>> {
  scan_for_database_with_dirs(db_path, &emergency_spill_base_dirs())
}

pub fn scan_for_database_with_dirs(db_path: impl AsRef<Path>, base_dirs: &[PathBuf]) -> EngineResult<Vec<EmergencySpillArtifact>> {
  let db_path = db_path.as_ref();
  let mut artifacts = Vec::new();
  for base_dir in dedupe_paths(base_dirs.to_vec()) {
    if !base_dir.exists() {
      continue;
    }
    let manifest_paths = manifest_paths_in_base_dir(&base_dir)?;
    for manifest_path in manifest_paths {
      let Some(artifact) = parse_manifest(&manifest_path)? else {
        continue;
      };
      if artifact_applied(&artifact.directory) {
        continue;
      }
      if artifact_matches_database(&artifact, db_path) {
        artifacts.push(artifact);
      }
    }
  }
  artifacts.sort_by(|left, right| left.sort_millis.cmp(&right.sort_millis).then_with(|| left.manifest_path.cmp(&right.manifest_path)));
  Ok(artifacts)
}

pub fn apply_wal_tails_to_database(
  db_path: impl AsRef<Path>,
  artifacts: &[EmergencySpillArtifact],
) -> EngineResult<EmergencySpillApplyReport> {
  let db_path = db_path.as_ref();
  let mut report = EmergencySpillApplyReport { artifact_count: artifacts.len(), ..EmergencySpillApplyReport::default() };

  for artifact in artifacts {
    if artifact.hot_tail_path.as_ref().is_some_and(|path| path.exists()) {
      report.hot_tail_files_seen += 1;
    }

    let Some(wal_tail_path) = artifact.wal_tail_path.as_ref() else {
      continue;
    };
    if !wal_tail_path.exists() {
      if artifact.wal_tail_bytes > 0 {
        return Err(EngineError::InvalidInput(format!(
          "emergency spill manifest {} references missing WAL tail {}",
          artifact.manifest_path.display(),
          wal_tail_path.display()
        )));
      }
      continue;
    }

    let copy_start = artifact.wal_tail_copy_start.ok_or_else(|| {
      EngineError::InvalidInput(format!("emergency spill manifest {} is missing wal_tail_copy_start", artifact.manifest_path.display()))
    })?;
    let (present, written) = apply_one_wal_tail(db_path, wal_tail_path, copy_start)?;
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
  let db_path = db_path.as_ref().display().to_string();
  for artifact in artifacts {
    let marker = AppliedMarker {
      format: EMERGENCY_SPILL_APPLIED_FORMAT.to_string(),
      applied_at: chrono::Utc::now().to_rfc3339(),
      db_path: db_path.clone(),
      manifest_path: artifact.manifest_path.display().to_string(),
      wal_tail_bytes_present: report.wal_tail_bytes_present,
      wal_tail_bytes_written: report.wal_tail_bytes_written,
    };
    let marker_path = artifact.directory.join("applied.json");
    let bytes = serde_json::to_vec_pretty(&marker).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
    write_durable_file(&marker_path, &bytes)?;
  }
  Ok(())
}

fn apply_one_wal_tail(db_path: &Path, wal_tail_path: &Path, copy_start: u64) -> EngineResult<(u64, u64)> {
  let wal_bytes = fs::read(wal_tail_path)?;
  if wal_bytes.is_empty() {
    return Ok((0, 0));
  }

  let mut db_file = OpenOptions::new().read(true).write(true).open(db_path)?;
  let db_len = db_file.metadata()?.len();
  if db_len < copy_start {
    return Err(EngineError::InvalidInput(format!(
      "cannot apply WAL tail {} at offset {}: database is only {} bytes",
      wal_tail_path.display(),
      copy_start,
      db_len
    )));
  }

  let existing_overlap = db_len.saturating_sub(copy_start).min(wal_bytes.len() as u64) as usize;
  if existing_overlap > 0 {
    let mut existing = vec![0u8; existing_overlap];
    db_file.seek(SeekFrom::Start(copy_start))?;
    db_file.read_exact(&mut existing)?;
    if existing != wal_bytes[..existing_overlap] {
      return Err(EngineError::InvalidInput(format!(
        "refusing to apply WAL tail {}: database bytes at {}..{} differ from spill",
        wal_tail_path.display(),
        copy_start,
        copy_start + existing_overlap as u64
      )));
    }
  }

  let remaining = &wal_bytes[existing_overlap..];
  if !remaining.is_empty() {
    db_file.seek(SeekFrom::Start(copy_start + existing_overlap as u64))?;
    db_file.write_all(remaining)?;
  }
  db_file.sync_all()?;
  sync_parent_dir(db_path)?;

  Ok((existing_overlap as u64, remaining.len() as u64))
}

fn write_durable_file(path: &Path, bytes: &[u8]) -> EngineResult<()> {
  let mut file = fs::File::create(path)?;
  file.write_all(bytes)?;
  file.sync_all()?;
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

fn parse_manifest(manifest_path: &Path) -> EngineResult<Option<EmergencySpillArtifact>> {
  let bytes = fs::read(manifest_path)?;
  let manifest: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| EngineError::JsonParseError(error.to_string()))?;
  if manifest.get("format").and_then(|value| value.as_str()) != Some(EMERGENCY_SPILL_FORMAT) {
    return Ok(None);
  }

  let directory = manifest_path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
  let attempted_at = manifest.get("attempted_at").and_then(|value| value.as_str()).map(str::to_string);
  let sort_millis = attempted_at.as_deref().and_then(parse_rfc3339_millis).or_else(|| modified_millis(manifest_path).ok()).unwrap_or(0);
  let db_path = manifest.get("db_path").and_then(|value| value.as_str()).map(str::to_string);
  let context = manifest.get("context").and_then(|value| value.as_str()).map(str::to_string);
  let failure = manifest.get("failure").and_then(|value| value.as_str()).map(str::to_string);
  let hot_tail_path = path_from_manifest_or_default(&manifest, "hot_tail_path", &directory, "hot-tail.bin");
  let wal_tail_path = path_from_manifest_or_default(&manifest, "wal_tail_path", &directory, "wal-tail.bin");

  Ok(Some(EmergencySpillArtifact {
    directory,
    manifest_path: manifest_path.to_path_buf(),
    attempted_at,
    sort_millis,
    db_path,
    context,
    failure,
    hot_tail_path,
    wal_tail_path,
    hot_tail_writes: manifest.get("hot_tail_writes").and_then(|value| value.as_u64()).unwrap_or(0) as usize,
    hot_tail_voids: manifest.get("hot_tail_voids").and_then(|value| value.as_u64()).unwrap_or(0) as usize,
    wal_tail_copy_start: manifest.get("wal_tail_copy_start").and_then(|value| value.as_u64()),
    wal_tail_end: manifest.get("wal_tail_end").and_then(|value| value.as_u64()),
    wal_tail_bytes: manifest.get("wal_tail_bytes").and_then(|value| value.as_u64()).unwrap_or(0),
    wal_tail_truncated: manifest.get("wal_tail_truncated").and_then(|value| value.as_bool()).unwrap_or(false),
  }))
}

fn path_from_manifest_or_default(manifest: &serde_json::Value, field: &str, directory: &Path, fallback_name: &str) -> Option<PathBuf> {
  if let Some(path) = manifest.get(field).and_then(|value| value.as_str()).filter(|path| !path.trim().is_empty()) {
    return Some(PathBuf::from(path));
  }
  let fallback = directory.join(fallback_name);
  fallback.exists().then_some(fallback)
}

fn artifact_applied(directory: &Path) -> bool {
  directory.join("applied.json").is_file()
}

fn artifact_matches_database(artifact: &EmergencySpillArtifact, db_path: &Path) -> bool {
  let Some(manifest_db_path) = artifact.db_path.as_deref() else {
    return false;
  };
  paths_equivalent(Path::new(manifest_db_path), db_path)
}

fn paths_equivalent(left: &Path, right: &Path) -> bool {
  if left == right {
    return true;
  }
  absolute_path(left) == absolute_path(right)
}

fn absolute_path(path: &Path) -> PathBuf {
  if let Ok(canonical) = path.canonicalize() {
    return canonical;
  }
  if path.is_absolute() {
    path.to_path_buf()
  } else {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(path)
  }
}

fn parse_rfc3339_millis(value: &str) -> Option<i64> {
  chrono::DateTime::parse_from_rfc3339(value).ok().map(|timestamp| timestamp.timestamp_millis())
}

fn modified_millis(path: &Path) -> std::io::Result<i64> {
  let modified = path.metadata()?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
  let duration = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
  Ok(duration.as_millis().min(i64::MAX as u128) as i64)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
  let mut seen = HashSet::new();
  let mut deduped = Vec::new();
  for path in paths {
    if seen.insert(path.clone()) {
      deduped.push(path);
    }
  }
  deduped
}

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
    fs::write(base.join("newer").join("applied.json"), b"{}").unwrap();

    let artifacts = scan_for_database_with_dirs(&db_path, &[base]).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].directory.ends_with("older"));
  }

  #[test]
  fn apply_wal_tail_appends_only_missing_matching_bytes() {
    let temp_dir = tempfile::tempdir().unwrap();
    let db_path = temp_dir.path().join("test.aeordb");
    fs::write(&db_path, b"abcdef").unwrap();

    let tail_path = temp_dir.path().join("wal-tail.bin");
    fs::write(&tail_path, b"defghi").unwrap();

    let artifact = EmergencySpillArtifact {
      directory: temp_dir.path().to_path_buf(),
      manifest_path: temp_dir.path().join("manifest.json"),
      attempted_at: None,
      sort_millis: 0,
      db_path: Some(db_path.display().to_string()),
      context: None,
      failure: None,
      hot_tail_path: None,
      wal_tail_path: Some(tail_path),
      hot_tail_writes: 0,
      hot_tail_voids: 0,
      wal_tail_copy_start: Some(3),
      wal_tail_end: Some(9),
      wal_tail_bytes: 6,
      wal_tail_truncated: false,
    };

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
