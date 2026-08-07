//! Read-only deployment compatibility checks for v3 transition authority.
//!
//! A pre-P2 binary does not understand durability latches or spill catalogs.
//! This module lets the currently installed compatible binary prove that a
//! downgrade is safe without opening `StorageEngine`, taking writer authority,
//! running recovery, or changing any database byte.

use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::control_store::{LoadedMutableControlV1, discover_mutable_control};
use super::database_header::{ReadOnlyDatabaseHeader, read_database_header_read_only};
use super::durability_recovery::{PersistentDurabilityRecoveryState, classify_persistent_durability_recovery};
use super::system_control::{SystemControlKindV1, SystemControlSlotV1, system_control_path};
use crate::engine::compression::CompressionAlgorithm;
use crate::engine::config_resolver::{
  ConfigDocumentInput, ConfigFallback, ConfigResolver, ConfigurationFamily, MAX_CONFIG_DOCUMENT_BYTES, RUNTIME_CONFIG_PATH,
  configuration_history_required,
};
use crate::engine::directory_ops::{chunk_content_hash, file_path_hash};
use crate::engine::disk_kv_store::DiskKVStore;
use crate::engine::entry_header::{EntryHeader, FLAG_SYSTEM};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_header::{FileHeader, HEADER_REGION_SIZE};
use crate::engine::file_record::FileRecord;
use crate::engine::hot_tail::{HOT_TAIL_FORMAT_VERSION, HOT_TAIL_MAGIC, WRITE_RECORD_VERSION};
use crate::engine::kv_pages::{bucket_page_offset, find_entry_in_page_data, page_size};
use crate::engine::kv_stages::{KV_STAGE_SIZES, stage_params};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::scalar_converter::HashConverter;

pub const DEPLOYMENT_CAPABILITY_PROTOCOL_V1: u16 = 1;
pub const TRANSITION_RECOVERY_CAPABILITY_V1: &str = "aeordb.v3-transition-recovery.v1";

const HOT_TAIL_HEADER_SIZE: usize = 18;
const TRANSITION_FILE_RECORD_VALUE_CAP: u32 = 4_096;
const TRANSITION_CONTENT_CAP: usize = 4_096;
const CONFIG_FILE_RECORD_VALUE_CAP: u32 = 64 * 1024;

pub(crate) struct ReadOnlyRuntimeConfigurationInputs {
  pub current: ConfigDocumentInput,
  pub last_known_good: Option<ConfigFallback>,
  pub history: Vec<ConfigFallback>,
  pub history_issues: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentCapabilitiesV1 {
  pub protocol_version: u16,
  pub product: String,
  pub binary_version: String,
  pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeploymentTransitionStateV1 {
  pub database_header_version: u8,
  pub persistent_recovery: Option<PersistentDurabilityRecoveryState>,
  pub external_spill_count: usize,
  pub requires_transition_capability: bool,
  pub reasons: Vec<String>,
}

impl DeploymentTransitionStateV1 {
  pub fn inactive_v3() -> Self {
    Self {
      database_header_version: 3,
      persistent_recovery: None,
      external_spill_count: 0,
      requires_transition_capability: false,
      reasons: Vec::new(),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeploymentDecisionV1 {
  pub allowed: bool,
  pub candidate_supports_transition_recovery: bool,
  pub message: String,
}

pub struct DeploymentInspectionLock {
  _file: File,
}

pub fn current_deployment_capabilities() -> DeploymentCapabilitiesV1 {
  DeploymentCapabilitiesV1 {
    protocol_version: DEPLOYMENT_CAPABILITY_PROTOCOL_V1,
    product: "aeordb".to_string(),
    binary_version: env!("CARGO_PKG_VERSION").to_string(),
    capabilities: vec![TRANSITION_RECOVERY_CAPABILITY_V1.to_string()],
  }
}

pub fn deployment_capabilities_support_transition_recovery(report: &DeploymentCapabilitiesV1) -> bool {
  report.protocol_version == DEPLOYMENT_CAPABILITY_PROTOCOL_V1
    && report.product == "aeordb"
    && report.capabilities.iter().any(|capability| capability == TRANSITION_RECOVERY_CAPABILITY_V1)
}

pub fn evaluate_deployment_candidate(state: &DeploymentTransitionStateV1, candidate_capability: Option<&str>) -> DeploymentDecisionV1 {
  let compatible = candidate_capability == Some(TRANSITION_RECOVERY_CAPABILITY_V1);
  if compatible {
    return DeploymentDecisionV1 {
      allowed: true,
      candidate_supports_transition_recovery: true,
      message: "candidate understands v3 transition recovery authority".to_string(),
    };
  }
  if !state.requires_transition_capability {
    return DeploymentDecisionV1 {
      allowed: true,
      candidate_supports_transition_recovery: false,
      message: "no active transition recovery authority blocks this downgrade".to_string(),
    };
  }
  let detail = if state.reasons.is_empty() { "active transition recovery authority".to_string() } else { state.reasons.join("; ") };
  DeploymentDecisionV1 {
    allowed: false,
    candidate_supports_transition_recovery: false,
    message: format!("candidate lacks {TRANSITION_RECOVERY_CAPABILITY_V1} while {detail}"),
  }
}

pub fn inspect_deployment_transition_state_read_only(path: impl AsRef<Path>) -> EngineResult<DeploymentTransitionStateV1> {
  let path = path.as_ref();
  let locations = crate::engine::config_resolver::preopen_emergency_spill_locations(
    path,
    &crate::engine::config_resolver::CommandLineConfigOverrides::default(),
  )?;
  let external_spills = crate::engine::emergency_spill::scan_for_database_with_locations(path, &locations)?;
  inspect_deployment_transition_state_with_external_count(path, external_spills.len())
}

pub(crate) fn read_runtime_configuration_inputs_read_only(
  path: impl AsRef<Path>,
  resolver: &ConfigResolver,
) -> EngineResult<ReadOnlyRuntimeConfigurationInputs> {
  let path = path.as_ref();
  if !path.exists() {
    return Ok(ReadOnlyRuntimeConfigurationInputs {
      current: ConfigDocumentInput::Missing,
      last_known_good: None,
      history: Vec::new(),
      history_issues: Vec::new(),
    });
  }
  let mut store = ReadOnlyV3TransitionControlStore::open_for_configuration(path)?;
  let current = match store.read_file_bounded(RUNTIME_CONFIG_PATH, MAX_CONFIG_DOCUMENT_BYTES) {
    Ok(Some(bytes)) => ConfigDocumentInput::Bytes(bytes),
    Ok(None) => ConfigDocumentInput::Missing,
    Err(error) => ConfigDocumentInput::Unreadable(error.to_string()),
  };
  let hash_algo = store.header.hash_algo;
  let mut history_issues = Vec::new();
  let last_known_good = match store.discover_mutable(SystemControlKindV1::RuntimeLastKnownGood) {
    Ok(Some(selected)) => match crate::engine::v4::configuration_controls::decode_lkg_fallback_read_only(
      hash_algo,
      ConfigurationFamily::Runtime,
      &selected.bytes,
    ) {
      Ok(fallback) => Some(fallback),
      Err(error) => {
        history_issues.push(format!("runtime last-known-good is unusable during pre-open recovery: {error}"));
        None
      }
    },
    Ok(None) => None,
    Err(error) => {
      history_issues.push(format!("runtime last-known-good discovery failed during pre-open recovery: {error}"));
      None
    }
  };
  let mut history = Vec::new();
  if configuration_history_required(resolver, ConfigurationFamily::Runtime, &current, last_known_good.as_ref()) {
    let recovered = store.load_configuration_history(ConfigurationFamily::Runtime);
    history = recovered.candidates;
    history_issues.extend(recovered.issues);
  }
  Ok(ReadOnlyRuntimeConfigurationInputs { current, last_known_good, history, history_issues })
}

pub fn acquire_deployment_inspection_lock(path: impl AsRef<Path>) -> EngineResult<DeploymentInspectionLock> {
  let path = path.as_ref();
  let mut lock_name = path.as_os_str().to_os_string();
  lock_name.push(".lock");
  let lock_path = std::path::PathBuf::from(lock_name);
  let file = OpenOptions::new().write(true).create(true).truncate(false).open(&lock_path)?;
  file.try_lock_exclusive().map_err(|_| {
    EngineError::InvalidInput(format!(
      "database {} is still open; stop its AeorDB process before checking an incompatible downgrade",
      path.display()
    ))
  })?;
  Ok(DeploymentInspectionLock { _file: file })
}

pub fn inspect_deployment_transition_state_with_spill_dirs_read_only(
  path: impl AsRef<Path>,
  spill_directories: &[std::path::PathBuf],
) -> EngineResult<DeploymentTransitionStateV1> {
  let path = path.as_ref();
  let external_spills = crate::engine::emergency_spill::scan_for_database_with_dirs(path, spill_directories)?;
  inspect_deployment_transition_state_with_external_count(path, external_spills.len())
}

fn inspect_deployment_transition_state_with_external_count(
  path: &Path,
  external_spill_count: usize,
) -> EngineResult<DeploymentTransitionStateV1> {
  let persistent_recovery = ReadOnlyV3TransitionControlStore::open(path)?.inspect_persistent_recovery()?;
  let mut reasons = Vec::new();
  if let Some(recovery) = persistent_recovery.as_ref().filter(|recovery| recovery.blocks_writes) {
    reasons.push(recovery.reason.clone());
  }
  if external_spill_count != 0 {
    reasons.push(format!("{external_spill_count} unapplied external emergency spill artifact(s) require repair"));
  }
  Ok(DeploymentTransitionStateV1 {
    database_header_version: 3,
    persistent_recovery,
    external_spill_count,
    requires_transition_capability: !reasons.is_empty(),
    reasons,
  })
}

struct HotTailLayout {
  offset: u64,
  write_count: usize,
  write_record_size: usize,
}

struct ReadOnlyV3TransitionControlStore {
  file: File,
  header: FileHeader,
  file_length: u64,
  bucket_count: usize,
  hot_tail: Option<HotTailLayout>,
}

impl ReadOnlyV3TransitionControlStore {
  fn open(path: &Path) -> EngineResult<Self> {
    Self::open_with_hot_tail_policy(path, false)
  }

  fn open_for_configuration(path: &Path) -> EngineResult<Self> {
    Self::open_with_hot_tail_policy(path, true)
  }

  fn open_with_hot_tail_policy(path: &Path, tolerate_invalid_hot_tail: bool) -> EngineResult<Self> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    let header = match read_database_header_read_only(&mut file)
      .map_err(|error| EngineError::InvalidInput(format!("deployment database header inspection failed: {error}")))?
    {
      ReadOnlyDatabaseHeader::V3 { header, .. } => header,
      ReadOnlyDatabaseHeader::V4(_) => {
        return Err(EngineError::InvalidInput(
          "v4 deployment state requires the v4 capability gate; refusing to infer downgrade safety".to_string(),
        ));
      }
    };
    if header.header_version != 3 {
      return Err(EngineError::InvalidInput(format!("expected v3 database header, found version {}", header.header_version)));
    }
    if header.kv_block_version != DiskKVStore::CURRENT_KV_BLOCK_VERSION {
      return Err(EngineError::InvalidEntryVersion(header.kv_block_version));
    }
    let stage = header.kv_block_stage as usize;
    if stage >= KV_STAGE_SIZES.len() {
      return Err(EngineError::InvalidInput(format!("unsupported v3 KV stage {stage} during deployment inspection")));
    }
    let hash_length = header.hash_algo.hash_length();
    let page_length = page_size(hash_length);
    let (expected_kv_length, bucket_count) = stage_params(stage, page_length);
    if header.kv_block_offset < HEADER_REGION_SIZE as u64 || header.kv_block_length != expected_kv_length || bucket_count == 0 {
      return Err(EngineError::InvalidInput("v3 KV layout is inconsistent with its selected header".to_string()));
    }
    let kv_end = header
      .kv_block_offset
      .checked_add(header.kv_block_length)
      .ok_or_else(|| EngineError::InvalidInput("v3 KV layout overflows the file address space".to_string()))?;
    if header.hot_tail_offset < kv_end {
      return Err(EngineError::InvalidInput("v3 hot tail overlaps the KV block".to_string()));
    }
    let file_length = file.metadata()?.len();
    let hot_tail = match Self::read_hot_tail_layout(&mut file, header.hot_tail_offset, hash_length, file_length) {
      Ok(hot_tail) => Some(hot_tail),
      Err(error) if tolerate_invalid_hot_tail => {
        tracing::warn!(%error, "Pre-open configuration recovery is ignoring an invalid hot tail and reading only durable KV pages");
        None
      }
      Err(error) => return Err(error),
    };
    Ok(Self { file, header, file_length, bucket_count, hot_tail })
  }

  fn read_hot_tail_layout(file: &mut File, offset: u64, hash_length: usize, file_length: u64) -> EngineResult<HotTailLayout> {
    let header_end = offset
      .checked_add(HOT_TAIL_HEADER_SIZE as u64)
      .ok_or_else(|| EngineError::InvalidInput("hot tail header offset overflow".to_string()))?;
    if header_end > file_length {
      return Err(EngineError::InvalidInput("hot tail header is missing or truncated".to_string()));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0u8; HOT_TAIL_HEADER_SIZE];
    file.read_exact(&mut bytes)?;
    if bytes[..5] != HOT_TAIL_MAGIC || bytes[5] != HOT_TAIL_FORMAT_VERSION {
      return Err(EngineError::InvalidInput("hot tail magic or format version is invalid".to_string()));
    }
    if crc32fast::hash(&bytes[..14]) != u32::from_le_bytes(bytes[14..18].try_into().expect("fixed CRC field")) {
      return Err(EngineError::InvalidInput("hot tail header checksum is invalid".to_string()));
    }
    let write_count = u32::from_le_bytes(bytes[6..10].try_into().expect("fixed write count")) as usize;
    let void_count = u32::from_le_bytes(bytes[10..14].try_into().expect("fixed void count")) as usize;
    let write_record_size = 1usize
      .checked_add(hash_length)
      .and_then(|value| value.checked_add(1 + 8 + 4))
      .ok_or_else(|| EngineError::InvalidInput("hot tail write-record size overflow".to_string()))?;
    let body_length = write_count
      .checked_mul(write_record_size)
      .and_then(|value| void_count.checked_mul(13).and_then(|void_bytes| value.checked_add(void_bytes)))
      .ok_or_else(|| EngineError::InvalidInput("hot tail record counts overflow".to_string()))?;
    let end = header_end
      .checked_add(body_length as u64)
      .ok_or_else(|| EngineError::InvalidInput("hot tail length overflows the file address space".to_string()))?;
    if end > file_length {
      return Err(EngineError::InvalidInput("hot tail body is truncated".to_string()));
    }
    Ok(HotTailLayout { offset, write_count, write_record_size })
  }

  fn inspect_persistent_recovery(&mut self) -> EngineResult<Option<PersistentDurabilityRecoveryState>> {
    let latch = self.discover_mutable(SystemControlKindV1::DurabilityLatch)?;
    let catalog = self.discover_mutable(SystemControlKindV1::EmergencySpillCatalog)?;
    classify_persistent_durability_recovery(self.header.hash_algo, latch, catalog)
  }

  fn read_file_bounded(&mut self, path: &str, maximum_content_length: usize) -> EngineResult<Option<Vec<u8>>> {
    let key = file_path_hash(path, &self.header.hash_algo)?;
    let Some(entry) = self.lookup_kv_entry(&key)? else {
      return Ok(None);
    };
    let (header, entry_key, value) = self.read_entry_bounded(&entry, CONFIG_FILE_RECORD_VALUE_CAP)?;
    if header.entry_type != EntryType::FileRecord
      || header.flags & FLAG_SYSTEM == 0
      || header.compression_algo != CompressionAlgorithm::None
      || entry_key != key
    {
      return Err(EngineError::InvalidInput(format!("configuration path {path} is not a canonical system FileRecord")));
    }
    let record = FileRecord::deserialize(&value, self.header.hash_algo.hash_length(), header.entry_version)?;
    if record.path != path {
      return Err(EngineError::InvalidInput(format!("configuration FileRecord path {} does not match {path}", record.path)));
    }
    let content_length = usize::try_from(record.total_size)
      .map_err(|_| EngineError::InvalidInput(format!("configuration document {path} length does not fit this platform")))?;
    if content_length > maximum_content_length {
      return Err(EngineError::InvalidInput(format!(
        "configuration document {path} length {content_length} exceeds {maximum_content_length} bytes"
      )));
    }

    let mut content = Vec::with_capacity(content_length);
    for chunk_hash in record.chunk_hashes {
      let remaining = content_length
        .checked_sub(content.len())
        .ok_or_else(|| EngineError::InvalidInput(format!("configuration document {path} exceeded its declared length")))?;
      if remaining == 0 {
        return Err(EngineError::InvalidInput(format!("configuration document {path} has chunks beyond its declared length")));
      }
      let chunk_entry = self
        .lookup_kv_entry(&chunk_hash)?
        .ok_or_else(|| EngineError::NotFound(format!("configuration chunk {}", hex::encode(&chunk_hash))))?;
      let maximum_stored_length = u32::try_from(remaining)
        .map_err(|_| EngineError::InvalidInput(format!("configuration document {path} remaining length exceeds u32")))?;
      let (chunk_header, chunk_key, stored) = self.read_entry_bounded(&chunk_entry, maximum_stored_length)?;
      if chunk_header.entry_type != EntryType::Chunk || chunk_key != chunk_hash {
        return Err(EngineError::InvalidInput(format!("configuration document {path} references a noncanonical chunk")));
      }
      let decoded = crate::engine::compression::decompress_bounded(&stored, chunk_header.compression_algo, remaining)?;
      if chunk_content_hash(&decoded, &self.header.hash_algo)? != chunk_hash {
        return Err(EngineError::InvalidInput(format!("configuration document {path} chunk content hash mismatch")));
      }
      content.extend_from_slice(&decoded);
    }
    if content.len() != content_length {
      return Err(EngineError::InvalidInput(format!(
        "configuration document {path} expected {content_length} bytes but read {}",
        content.len()
      )));
    }
    Ok(Some(content))
  }

  fn load_configuration_history(&mut self, family: ConfigurationFamily) -> crate::engine::configuration_history::ConfigurationHistoryLoad {
    let wal_start = match self.header.kv_block_offset.checked_add(self.header.kv_block_length) {
      Some(wal_start) => wal_start,
      None => {
        return crate::engine::configuration_history::ConfigurationHistoryLoad {
          candidates: Vec::new(),
          issues: vec![format!("{} append-history WAL start overflows u64", family.name())],
        };
      }
    };
    let scan = crate::engine::configuration_history::scan_configuration_history_records(
      &self.file,
      wal_start,
      self.header.hot_tail_offset,
      self.header.hash_algo,
      family.path(),
      crate::engine::configuration_history::MAX_HISTORY_SCAN_BYTES,
      crate::engine::configuration_history::MAX_HISTORY_CANDIDATES,
    );
    let scan = match scan {
      Ok(scan) => scan,
      Err(error) => {
        return crate::engine::configuration_history::ConfigurationHistoryLoad {
          candidates: Vec::new(),
          issues: vec![format!("{} append-history scan failed during pre-open recovery: {error}", family.name())],
        };
      }
    };
    let hash_algorithm = self.header.hash_algo;
    crate::engine::configuration_history::materialize_configuration_history(scan, family, hash_algorithm, |chunk_hash, maximum_length| {
      let Some(chunk_entry) = self.lookup_kv_entry(chunk_hash)? else {
        return Ok(None);
      };
      let maximum_stored_length = u32::try_from(maximum_length)
        .map_err(|_| EngineError::ResourceExhausted("historical configuration chunk bound exceeds u32".to_string()))?;
      let (chunk_header, chunk_key, stored) = self.read_entry_bounded(&chunk_entry, maximum_stored_length)?;
      if chunk_header.entry_type != EntryType::Chunk || chunk_key != chunk_hash {
        return Err(EngineError::InvalidInput("historical configuration references a noncanonical chunk".to_string()));
      }
      let decoded = crate::engine::compression::decompress_bounded(&stored, chunk_header.compression_algo, maximum_length)?;
      if chunk_content_hash(&decoded, &hash_algorithm)? != chunk_hash {
        return Err(EngineError::InvalidInput("historical configuration chunk content hash mismatch".to_string()));
      }
      Ok(Some(decoded))
    })
  }

  fn discover_mutable(&mut self, kind: SystemControlKindV1) -> EngineResult<Option<LoadedMutableControlV1>> {
    let a_path = system_control_path(kind, &[], SystemControlSlotV1::A)
      .map_err(|error| EngineError::InvalidInput(format!("invalid deployment ControlStore path: {error}")))?;
    let b_path = system_control_path(kind, &[], SystemControlSlotV1::B)
      .map_err(|error| EngineError::InvalidInput(format!("invalid deployment ControlStore path: {error}")))?;
    let a = self.load_slot(kind, &a_path)?;
    let b = self.load_slot(kind, &b_path)?;
    discover_mutable_control(self.header.hash_algo, kind, &[], a, b)
      .map_err(|error| EngineError::InvalidInput(format!("invalid deployment ControlStore record: {error}")))
  }

  fn load_slot(&mut self, kind: SystemControlKindV1, path: &str) -> EngineResult<Option<Vec<u8>>> {
    let key = file_path_hash(path, &self.header.hash_algo)?;
    let Some(entry) = self.lookup_kv_entry(&key)? else {
      return Ok(None);
    };
    let (entry_header, entry_key, value) = self.read_entry_bounded(&entry, TRANSITION_FILE_RECORD_VALUE_CAP)?;
    if entry_header.entry_type != EntryType::FileRecord
      || entry_header.flags & FLAG_SYSTEM == 0
      || entry_header.entry_version != 0
      || entry_header.compression_algo != CompressionAlgorithm::None
      || entry_key != key
    {
      return Err(EngineError::InvalidInput(format!("transition control {path} is not a canonical system FileRecord v0")));
    }
    let record = FileRecord::deserialize(&value, self.header.hash_algo.hash_length(), entry_header.entry_version)?;
    if record.path != path || record.total_size > kind.encoded_cap() as u64 {
      return Err(EngineError::InvalidInput(format!("transition control {path} FileRecord identity or size is invalid")));
    }
    let expected_chunks = usize::from(record.total_size != 0);
    if record.chunk_hashes.len() != expected_chunks {
      return Err(EngineError::InvalidInput(format!("transition control {path} must use zero or one bounded chunk")));
    }
    let mut content = Vec::with_capacity(record.total_size as usize);
    for chunk_hash in &record.chunk_hashes {
      let chunk_entry = self
        .lookup_kv_entry(chunk_hash)?
        .ok_or_else(|| EngineError::NotFound(format!("transition control chunk {}", hex::encode(chunk_hash))))?;
      let (chunk_header, chunk_key, chunk_value) = self.read_entry_bounded(&chunk_entry, TRANSITION_CONTENT_CAP as u32)?;
      if chunk_header.entry_type != EntryType::Chunk
        || chunk_header.flags & FLAG_SYSTEM == 0
        || chunk_header.entry_version != 0
        || chunk_header.compression_algo != CompressionAlgorithm::None
        || chunk_key != *chunk_hash
        || chunk_content_hash(&chunk_value, &self.header.hash_algo)? != *chunk_hash
      {
        return Err(EngineError::InvalidInput(format!("transition control {path} references a noncanonical system chunk")));
      }
      content.extend_from_slice(&chunk_value);
    }
    if content.len() as u64 != record.total_size {
      return Err(EngineError::InvalidInput(format!("transition control {path} content length does not match its FileRecord")));
    }
    Ok(Some(content))
  }

  fn lookup_kv_entry(&mut self, key: &[u8]) -> EngineResult<Option<KVEntry>> {
    if let Some(entry) = self.lookup_hot_entry(key)? {
      return Ok((entry.type_flags & KV_FLAG_DELETED == 0).then_some(entry));
    }
    let nvt = NormalizedVectorTable::new(Box::new(HashConverter), self.bucket_count);
    let bucket = nvt.bucket_for_value(key);
    let page_offset = self
      .header
      .kv_block_offset
      .checked_add(bucket_page_offset(bucket, self.header.hash_algo.hash_length()))
      .ok_or_else(|| EngineError::InvalidInput("KV bucket offset overflow".to_string()))?;
    let page_length = page_size(self.header.hash_algo.hash_length());
    let page_end =
      page_offset.checked_add(page_length as u64).ok_or_else(|| EngineError::InvalidInput("KV bucket length overflow".to_string()))?;
    if page_end > self.header.kv_block_offset + self.header.kv_block_length || page_end > self.file_length {
      return Err(EngineError::InvalidInput("KV bucket falls outside the selected v3 layout".to_string()));
    }
    self.file.seek(SeekFrom::Start(page_offset))?;
    let mut page = vec![0u8; page_length];
    self.file.read_exact(&mut page)?;
    find_entry_in_page_data(&page, self.header.hash_algo.hash_length(), key, false)
  }

  fn lookup_hot_entry(&mut self, key: &[u8]) -> EngineResult<Option<KVEntry>> {
    let Some(hot_tail) = self.hot_tail.as_ref() else {
      return Ok(None);
    };
    let offset = hot_tail.offset;
    let write_record_size = hot_tail.write_record_size;
    let write_count = hot_tail.write_count;
    let mut matched = None;
    self.file.seek(SeekFrom::Start(offset + HOT_TAIL_HEADER_SIZE as u64))?;
    let mut record = vec![0u8; write_record_size];
    for _ in 0..write_count {
      self.file.read_exact(&mut record)?;
      if record[0] != WRITE_RECORD_VERSION {
        return Err(EngineError::InvalidInput(format!("unsupported hot tail write-record version {}", record[0])));
      }
      let hash_end = 1 + self.header.hash_algo.hash_length();
      if &record[1..hash_end] != key {
        continue;
      }
      let type_flags = record[hash_end];
      let offset = u64::from_le_bytes(record[hash_end + 1..hash_end + 9].try_into().expect("fixed hot offset"));
      let total_length = u32::from_le_bytes(record[hash_end + 9..hash_end + 13].try_into().expect("fixed hot length"));
      matched = Some(KVEntry { hash: key.to_vec(), type_flags, offset, total_length });
    }
    Ok(matched)
  }

  fn read_entry_bounded(&mut self, entry: &KVEntry, maximum_value_length: u32) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    let maximum_total = EntryHeader::FIXED_HEADER_SIZE
      + self.header.hash_algo.hash_length()
      + self.header.hash_algo.hash_length()
      + maximum_value_length as usize;
    if entry.total_length as usize > maximum_total {
      return Err(EngineError::InvalidInput(format!("transition KV entry length {} exceeds bounded inspection cap", entry.total_length)));
    }
    let end = entry
      .offset
      .checked_add(entry.total_length as u64)
      .ok_or_else(|| EngineError::InvalidInput("transition KV entry range overflow".to_string()))?;
    let wal_start = self.header.kv_block_offset + self.header.kv_block_length;
    if entry.offset < wal_start || end > self.header.hot_tail_offset || end > self.file_length {
      return Err(EngineError::InvalidInput("transition KV entry points outside the selected v3 WAL".to_string()));
    }
    self.file.seek(SeekFrom::Start(entry.offset))?;
    let mut bytes = vec![0u8; entry.total_length as usize];
    self.file.read_exact(&mut bytes)?;
    let mut cursor = Cursor::new(&bytes);
    let header = EntryHeader::deserialize(&mut cursor)?;
    let exact_total = EntryHeader::compute_total_length(header.hash_algo, header.key_length as usize, header.value_length as usize)?;
    if header.hash_algo != self.header.hash_algo
      || header.total_length != exact_total
      || header.total_length != entry.total_length
      || header.value_length > maximum_value_length
      || header.encryption_algo != 0
    {
      return Err(EngineError::InvalidInput("transition KV entry header is inconsistent with its bounded index record".to_string()));
    }
    let key_start = header.header_size();
    let key_end = key_start
      .checked_add(header.key_length as usize)
      .ok_or_else(|| EngineError::InvalidInput("transition entry key length overflow".to_string()))?;
    let value_end = key_end
      .checked_add(header.value_length as usize)
      .ok_or_else(|| EngineError::InvalidInput("transition entry value length overflow".to_string()))?;
    if value_end != bytes.len() {
      return Err(EngineError::InvalidInput("transition entry payload does not close at total_length".to_string()));
    }
    let key = bytes[key_start..key_end].to_vec();
    let value = bytes[key_end..value_end].to_vec();
    if key != entry.hash || !header.verify(&key, &value) {
      return Err(EngineError::InvalidInput("transition entry key or integrity hash does not match the KV record".to_string()));
    }
    Ok((header, key, value))
  }
}
