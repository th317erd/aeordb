use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::emergency_spill::{EmergencySpillArtifact, native_path_bytes};
use crate::engine::storage_engine::{DurabilityRepairAuthorityGuard, StorageEngine};

use super::control_store::{LoadedMutableControlV1, V3TransitionControlStore};
use super::hash::digest_parts;
use super::system_control::{
  DurabilityLatchBodyV1, EmergencySpillCatalogBodyV1, SystemControlKindV1, decode_durability_latch_body,
  decode_emergency_spill_catalog_body, decode_system_control, encode_durability_latch_control, encode_emergency_spill_catalog_control,
};

const LATCH_REPAIR_VERIFYING: u16 = 2;
const LATCH_CLEARED: u16 = 3;
const LATCH_READ_ONLY: u16 = 1;
const CATALOG_DISCOVERED: u16 = 1;
const CATALOG_REPLAYING: u16 = 2;
const CATALOG_COMPLETE: u16 = 3;
const SPILL_CATALOG_PRESENT: u32 = 1;
const SPILL_CATALOG_PAYLOAD_DOMAIN: &[u8] = b"aeordb.emergency-spill-catalog-payload.v1\0";
const REPAIR_RECEIPT_DOMAIN: &[u8] = b"aeordb.durability-repair-receipt.v1\0";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PersistentDurabilityRecoveryState {
  pub database_id: [u8; 16],
  pub blocks_writes: bool,
  pub latch_state: Option<u16>,
  pub catalog_state: Option<u16>,
  pub latch_sequence: Option<u64>,
  pub catalog_sequence: Option<u64>,
  pub redundancy_degraded: bool,
  pub reason: String,
}

impl PersistentDurabilityRecoveryState {
  pub(crate) fn is_repair_verifying(&self) -> bool {
    self.latch_state == Some(LATCH_REPAIR_VERIFYING)
  }

  pub(crate) fn is_catalog_replaying(&self) -> bool {
    self.catalog_state == Some(CATALOG_REPLAYING)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityRepairReceipt {
  pub database_id: [u8; 16],
  pub repair_receipt_hash: Vec<u8>,
  pub selected_header_sequence: u64,
  pub durable_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityRecoverySeed {
  pub database_id: [u8; 16],
  pub requires_repair: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityRepairVerification {
  evidence_digest: Vec<u8>,
  selected_header_sequence: u64,
  durable_sequence: u64,
}

impl DurabilityRepairVerification {
  pub(crate) fn new(evidence_digest: Vec<u8>, selected_header_sequence: u64, durable_sequence: u64) -> Self {
    Self { evidence_digest, selected_header_sequence, durable_sequence }
  }
}

pub struct ExplicitDurabilityRepair<'a> {
  engine: &'a StorageEngine,
  _authority: DurabilityRepairAuthorityGuard<'a>,
}

impl<'a> ExplicitDurabilityRepair<'a> {
  pub(crate) fn begin(engine: &'a StorageEngine) -> EngineResult<Self> {
    let recovery = engine
      .persistent_durability_recovery()
      .filter(|recovery| recovery.blocks_writes)
      .ok_or_else(|| EngineError::InvalidInput("no active durability recovery requires explicit repair".to_string()))?;
    let authority = engine.acquire_durability_repair_authority()?;
    transition_to_repairing(engine, recovery.database_id)?;
    Ok(Self { engine, _authority: authority })
  }

  pub fn engine(&self) -> &StorageEngine {
    self.engine
  }

  pub fn complete(self, verification: DurabilityRepairVerification) -> EngineResult<DurabilityRepairReceipt> {
    let current_header_sequence = self.engine.writer_read_lock()?.file_header().sequence;
    let current_durable_sequence = self.engine.durability_snapshot()?.hard_frontier;
    if current_header_sequence != verification.selected_header_sequence || current_durable_sequence != verification.durable_sequence {
      return Err(EngineError::DurabilityFailure(
        "database changed after durability repair verification; run verification again".to_string(),
      ));
    }
    self.engine.force_hot_tail_flush()?;
    let selected_header_sequence = self.engine.writer_read_lock()?.file_header().sequence;
    let durable_sequence = self.engine.durability_snapshot()?.hard_frontier;
    if selected_header_sequence == 0 || durable_sequence == 0 {
      return Err(EngineError::DurabilityFailure(
        "durability repair probe did not establish nonzero header and hard-frontier evidence".to_string(),
      ));
    }
    let database_id = self
      .engine
      .persistent_durability_recovery()
      .map(|recovery| recovery.database_id)
      .ok_or_else(|| EngineError::DurabilityFailure("durability recovery authority disappeared during explicit repair".to_string()))?;
    let proposed_receipt = digest_parts(
      self.engine.hash_algo(),
      &[
        REPAIR_RECEIPT_DOMAIN,
        &database_id,
        &selected_header_sequence.to_le_bytes(),
        &durable_sequence.to_le_bytes(),
        &verification.evidence_digest,
      ],
    );
    let repair_receipt_hash = transition_to_complete(self.engine, database_id, &proposed_receipt)?;
    Ok(DurabilityRepairReceipt { database_id, repair_receipt_hash, selected_header_sequence, durable_sequence })
  }
}

pub fn inspect_persistent_durability_recovery(engine: &StorageEngine) -> EngineResult<Option<PersistentDurabilityRecoveryState>> {
  let store = V3TransitionControlStore::new(engine);
  let latch = store.discover_mutable(SystemControlKindV1::DurabilityLatch, &[])?;
  let catalog = store.discover_mutable(SystemControlKindV1::EmergencySpillCatalog, &[])?;
  classify_persistent_durability_recovery(engine.hash_algo(), latch, catalog)
}

pub fn classify_persistent_durability_recovery(
  algorithm: crate::engine::HashAlgorithm,
  latch: Option<LoadedMutableControlV1>,
  catalog: Option<LoadedMutableControlV1>,
) -> EngineResult<Option<PersistentDurabilityRecoveryState>> {
  if latch.is_none() && catalog.is_none() {
    return Ok(None);
  }

  let database_id = recovery_database_id(latch.as_ref(), catalog.as_ref())?;
  let latch_body = latch
    .as_ref()
    .map(|selected| {
      let control = decode_system_control(&selected.bytes, algorithm).map_err(format_error)?;
      decode_durability_latch_body(control.body, algorithm).map_err(format_error)
    })
    .transpose()?;
  let catalog_control =
    catalog.as_ref().map(|selected| decode_system_control(&selected.bytes, algorithm).map_err(format_error)).transpose()?;
  let catalog_body = catalog_control
    .as_ref()
    .map(|control| decode_emergency_spill_catalog_body(control.body, algorithm).map_err(format_error))
    .transpose()?;

  let catalog_presence_consistent =
    latch_body.as_ref().is_none_or(|body| (body.flags & SPILL_CATALOG_PRESENT != 0) == catalog_control.is_some());
  let mut catalog_reference_consistent = true;
  if let Some(latch_body) = latch_body.as_ref() {
    if latch_body.flags & SPILL_CATALOG_PRESENT != 0 {
      if let Some(catalog_control) = catalog_control.as_ref() {
        let expected = digest_parts(algorithm, &[SPILL_CATALOG_PAYLOAD_DOMAIN, catalog_control.body]);
        if latch_body.emergency_spill_catalog_payload_hash != expected {
          catalog_reference_consistent = false;
          if latch_body.state == LATCH_CLEARED && catalog_body.as_ref().is_none_or(|body| body.state == CATALOG_COMPLETE) {
            return Err(EngineError::InvalidInput(
              "cleared persistent durability latch references a different emergency spill catalog payload".to_string(),
            ));
          }
        }
      }
    }
  }

  let latch_state = latch_body.as_ref().map(|body| body.state);
  let catalog_state = catalog_body.as_ref().map(|body| body.state);
  let latch_cleared = latch_state == Some(LATCH_CLEARED);
  let catalog_complete = catalog_state.is_none_or(|state| state == CATALOG_COMPLETE);
  let blocks_writes = !latch_cleared || !catalog_complete || !catalog_presence_consistent || !catalog_reference_consistent;
  let reason = recovery_reason(latch_state, catalog_state, blocks_writes, catalog_presence_consistent, catalog_reference_consistent)?;

  Ok(Some(PersistentDurabilityRecoveryState {
    database_id,
    blocks_writes,
    latch_state,
    catalog_state,
    latch_sequence: latch.as_ref().map(|selected| selected.sequence),
    catalog_sequence: catalog.as_ref().map(|selected| selected.sequence),
    redundancy_degraded: latch.as_ref().is_some_and(|selected| selected.redundancy_degraded)
      || catalog.as_ref().is_some_and(|selected| selected.redundancy_degraded),
    reason,
  }))
}

pub fn seed_from_external_spills(engine: &StorageEngine, artifacts: &[EmergencySpillArtifact]) -> EngineResult<DurabilityRecoverySeed> {
  if artifacts.is_empty() {
    return Err(EngineError::InvalidInput("cannot seed durability recovery without external spill artifacts".to_string()));
  }
  let store = V3TransitionControlStore::new(engine);
  let existing_latch = store.discover_mutable(SystemControlKindV1::DurabilityLatch, &[])?;
  let existing_catalog = store.discover_mutable(SystemControlKindV1::EmergencySpillCatalog, &[])?;
  let existing_database_id = match (existing_latch.as_ref(), existing_catalog.as_ref()) {
    (Some(latch), Some(catalog)) if latch.database_id != catalog.database_id => {
      return Err(EngineError::InvalidInput(
        "persistent durability latch and emergency spill catalog have different database identities".to_string(),
      ));
    }
    (Some(latch), _) => Some(latch.database_id),
    (_, Some(catalog)) => Some(catalog.database_id),
    (None, None) => None,
  };
  let artifact_database_id = artifacts.iter().filter_map(|artifact| artifact.database_id).try_fold(None, |selected, candidate| {
    if candidate == [0; 16] {
      return Err(EngineError::InvalidInput("external spill artifact contains a zero database identity".to_string()));
    }
    match selected {
      Some(selected) if selected != candidate => {
        Err(EngineError::InvalidInput("external spill artifacts have different database identities".to_string()))
      }
      _ => Ok(Some(candidate)),
    }
  })?;
  if matches!((existing_database_id, artifact_database_id), (Some(existing), Some(artifact)) if existing != artifact) {
    return Err(EngineError::InvalidInput(
      "external spill artifacts and persistent recovery controls have different database identities".to_string(),
    ));
  }
  let database_id = existing_database_id.or(artifact_database_id).unwrap_or_else(|| uuid::Uuid::new_v4().into_bytes());
  if database_id == [0; 16] {
    return Err(EngineError::InvalidInput("durability recovery database identity is zero".to_string()));
  }
  let ordered_artifacts = validate_and_order_spill_artifacts(artifacts)?;
  let rows = spill_catalog_rows(&ordered_artifacts)?;

  let existing_catalog_body = existing_catalog.as_ref().map(|selected| decode_catalog(engine, selected)).transpose()?;
  if let Some(catalog) = existing_catalog_body.as_ref() {
    if catalog.database_id != database_id {
      return Err(EngineError::InvalidInput("spill catalog body has a different database identity".to_string()));
    }
    if catalog.state == CATALOG_COMPLETE && same_spill_rows(&catalog.rows, &rows) {
      let state = inspect_persistent_durability_recovery(engine)?;
      if state.as_ref().is_some_and(|state| !state.blocks_writes) {
        return Ok(DurabilityRecoverySeed { database_id, requires_repair: false });
      }
    } else if catalog.state != CATALOG_COMPLETE && !same_spill_rows(&catalog.rows, &rows) {
      return Err(EngineError::InvalidInput("active persistent spill catalog does not match the validated external spill set".to_string()));
    }
  }

  let _authority = engine.acquire_durability_repair_authority()?;
  let catalog = if existing_catalog_body
    .as_ref()
    .is_some_and(|catalog| catalog.state != CATALOG_COMPLETE && same_spill_rows(&catalog.rows, &rows))
  {
    existing_catalog.ok_or_else(|| {
      EngineError::DurabilityFailure("selected spill catalog disappeared after its body was decoded during recovery seeding".to_string())
    })?
  } else {
    let sequence = existing_catalog.as_ref().map(|selected| next_sequence(selected.sequence)).transpose()?.unwrap_or(1);
    let generation =
      existing_catalog_body.as_ref().map(|catalog| next_generation(catalog.catalog_generation, "spill catalog")).transpose()?.unwrap_or(1);
    let catalog_body = EmergencySpillCatalogBodyV1 {
      database_id,
      catalog_generation: generation,
      discovered_at_ms: chrono::Utc::now().timestamp_millis().max(0),
      state: CATALOG_DISCOVERED,
      flags: 0,
      repair_receipt_hash: vec![0; engine.hash_algo().hash_length()],
      rows,
    };
    let bytes = encode_emergency_spill_catalog_control(sequence, &catalog_body, engine.hash_algo()).map_err(format_error)?;
    store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &bytes)?
  };
  // The catalog is published before the latch so a crash cannot lose spill
  // discovery. Refresh immediately: any later error must leave this process
  // read-only, not merely the next process after reopen.
  engine.refresh_persistent_durability_recovery()?;
  let selected_catalog = decode_catalog(engine, &catalog)?;
  let catalog_hash = catalog_payload_hash(engine, &catalog.bytes)?;
  let existing_latch_body = existing_latch.as_ref().map(|selected| decode_latch(engine, selected)).transpose()?;
  let latch_already_matches = existing_latch_body.as_ref().is_some_and(|latch| {
    latch.state != LATCH_CLEARED && latch.emergency_spill_catalog_payload_hash == catalog_hash && latch.database_id == database_id
  });
  if !latch_already_matches {
    let sequence = existing_latch.as_ref().map(|selected| next_sequence(selected.sequence)).transpose()?.unwrap_or(1);
    let generation =
      existing_latch_body.as_ref().map(|latch| next_generation(latch.latch_generation, "durability latch")).transpose()?.unwrap_or(1);
    let fallback_sequence = engine.writer_read_lock()?.file_header().sequence.max(1);
    let first = ordered_artifacts[0];
    let latest_failure_at_ms = artifacts.iter().map(|artifact| artifact.latest_failure_at_ms).max().unwrap_or(first.latest_failure_at_ms);
    let failed_operation =
      first.failed_operation.unwrap_or(crate::engine::durability_coordinator::DurabilityOperation::EmergencySpill.stable_id());
    let os_error_class = first.os_error_class.unwrap_or(crate::engine::durability_coordinator::OsErrorClass::OtherPersistentIo.stable_id());
    let os_error_code = first.os_error_code.filter(|code| *code != 0).unwrap_or(-1);
    let catalog_control = decode_system_control(&catalog.bytes, engine.hash_algo()).map_err(format_error)?;
    let evidence_digest = digest_parts(
      engine.hash_algo(),
      &[
        b"aeordb.durability-latch-evidence.v1\0",
        &database_id,
        catalog_control.body,
        &first.first_failure_at_ms.to_le_bytes(),
        &latest_failure_at_ms.to_le_bytes(),
      ],
    );
    let latch = DurabilityLatchBodyV1 {
      database_id,
      latch_generation: generation,
      first_failure_at_ms: first.first_failure_at_ms.max(0),
      latest_failure_at_ms: latest_failure_at_ms.max(first.first_failure_at_ms).max(0),
      severity: 1,
      state: LATCH_READ_ONLY,
      failed_operation,
      os_error_class,
      os_error_code,
      flags: SPILL_CATALOG_PRESENT,
      last_selected_header_sequence: first.last_selected_header_sequence.unwrap_or(fallback_sequence).max(1),
      last_durable_write_sequence: first.last_durable_write_sequence.unwrap_or(fallback_sequence).max(1),
      last_durable_publication_sequence: first.last_durable_publication_sequence.unwrap_or(fallback_sequence).max(1),
      emergency_spill_catalog_payload_hash: catalog_hash,
      evidence_digest,
      diagnostic: canonical_utf8("external emergency spill requires explicit repair")?,
    };
    let bytes = encode_durability_latch_control(sequence, &latch, engine.hash_algo()).map_err(format_error)?;
    store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &bytes)?;
  } else if selected_catalog.state == CATALOG_COMPLETE {
    return Err(EngineError::InvalidInput("active durability latch cannot reference a completed replacement catalog".to_string()));
  }
  engine.refresh_persistent_durability_recovery()?;
  let state = engine
    .persistent_durability_recovery()
    .ok_or_else(|| EngineError::DurabilityFailure("spill seeding did not establish persistent recovery authority".to_string()))?;
  if !state.blocks_writes || state.database_id != database_id {
    return Err(EngineError::DurabilityFailure("spill seeding did not establish the expected read-only authority".to_string()));
  }
  Ok(DurabilityRecoverySeed { database_id, requires_repair: true })
}

fn validate_and_order_spill_artifacts(artifacts: &[EmergencySpillArtifact]) -> EngineResult<Vec<&EmergencySpillArtifact>> {
  let mut ordered: Vec<_> = artifacts.iter().collect();
  ordered.sort_by(|left, right| {
    left
      .first_failure_at_ms
      .cmp(&right.first_failure_at_ms)
      .then_with(|| left.creation_sequence.cmp(&right.creation_sequence))
      .then_with(|| left.manifest_digest.cmp(&right.manifest_digest))
      .then_with(|| native_path_bytes(&left.manifest_path).cmp(&native_path_bytes(&right.manifest_path)))
  });
  for artifact in &ordered {
    let typed_fields = [
      artifact.failed_operation.is_some(),
      artifact.os_error_class.is_some(),
      artifact.os_error_code.is_some(),
      artifact.last_selected_header_sequence.is_some(),
      artifact.last_durable_write_sequence.is_some(),
      artifact.last_durable_publication_sequence.is_some(),
    ];
    let typed_count = typed_fields.iter().filter(|present| **present).count();
    if typed_count != 0 && typed_count != typed_fields.len() {
      return Err(EngineError::InvalidInput("external spill typed failure evidence must be complete when present".to_string()));
    }
    if artifact
      .failed_operation
      .is_some_and(|operation| !crate::engine::durability_coordinator::DurabilityOperation::is_stable_id(operation))
    {
      return Err(EngineError::InvalidInput("external spill failed operation is invalid".to_string()));
    }
    if artifact.os_error_class.is_some_and(|error_class| !crate::engine::durability_coordinator::OsErrorClass::is_stable_id(error_class)) {
      return Err(EngineError::InvalidInput("external spill OS error class is invalid".to_string()));
    }
    if artifact.os_error_code == Some(0) {
      return Err(EngineError::InvalidInput("external spill OS error code must be nonzero".to_string()));
    }
    if [artifact.last_selected_header_sequence, artifact.last_durable_write_sequence, artifact.last_durable_publication_sequence]
      .into_iter()
      .flatten()
      .any(|sequence| sequence == 0)
    {
      return Err(EngineError::InvalidInput("external spill durability sequence must be nonzero".to_string()));
    }
    if artifact.latest_failure_at_ms < artifact.first_failure_at_ms {
      return Err(EngineError::InvalidInput("external spill latest failure precedes first failure".to_string()));
    }
    match artifact.format_version {
      crate::engine::emergency_spill::EmergencySpillFormatVersion::V1 => {}
      crate::engine::emergency_spill::EmergencySpillFormatVersion::V2 => {
        if artifact.database_id.is_none_or(|database_id| database_id == [0; 16])
          || artifact.incident_id.is_none_or(|incident_id| incident_id == [0; 16])
          || artifact.creation_sequence == 0
        {
          return Err(EngineError::InvalidInput("external v2 spill identity or creation sequence is invalid".to_string()));
        }
      }
    }
    let path = native_path_bytes(&artifact.manifest_path);
    if artifact.manifest_length == 0 || artifact.manifest_digest == [0; 32] || path.is_empty() {
      return Err(EngineError::InvalidInput("external spill manifest has incomplete catalog evidence".to_string()));
    }
  }
  Ok(ordered)
}

fn spill_catalog_rows(
  ordered_artifacts: &[&EmergencySpillArtifact],
) -> EngineResult<Vec<super::system_control::EmergencySpillCatalogRowV1>> {
  let mut rows = Vec::with_capacity(ordered_artifacts.len());
  for (index, artifact) in ordered_artifacts.iter().enumerate() {
    let path = native_path_bytes(&artifact.manifest_path);
    rows.push(super::system_control::EmergencySpillCatalogRowV1 {
      source_location_class: artifact.source_location_class as u16,
      replay_state: 1,
      path_encoding: artifact.path_encoding,
      flags: 0,
      created_at_ms: artifact.first_failure_at_ms.max(0),
      creation_sequence: artifact.creation_sequence.max(index as u64 + 1),
      file_length: artifact.manifest_length,
      complete_file_digest: artifact.manifest_digest.to_vec(),
      native_path: path,
    });
  }
  rows.sort_by(|left, right| {
    left
      .created_at_ms
      .cmp(&right.created_at_ms)
      .then_with(|| left.creation_sequence.cmp(&right.creation_sequence))
      .then_with(|| left.complete_file_digest.cmp(&right.complete_file_digest))
      .then_with(|| left.native_path.cmp(&right.native_path))
  });
  if rows.windows(2).any(|pair| pair[0] == pair[1]) {
    return Err(EngineError::InvalidInput("external spill set contains duplicate catalog rows".to_string()));
  }
  Ok(rows)
}

fn same_spill_rows(
  left: &[super::system_control::EmergencySpillCatalogRowV1],
  right: &[super::system_control::EmergencySpillCatalogRowV1],
) -> bool {
  left.len() == right.len()
    && left.iter().zip(right).all(|(left, right)| {
      left.source_location_class == right.source_location_class
        && left.path_encoding == right.path_encoding
        && left.flags == right.flags
        && left.created_at_ms == right.created_at_ms
        && left.creation_sequence == right.creation_sequence
        && left.file_length == right.file_length
        && left.complete_file_digest == right.complete_file_digest
        && left.native_path == right.native_path
    })
}

fn canonical_utf8(value: &str) -> EngineResult<Vec<u8>> {
  let bytes = value.as_bytes();
  let length = u32::try_from(bytes.len()).map_err(|_| EngineError::InvalidInput("durability diagnostic is too large".to_string()))?;
  let mut encoded = Vec::with_capacity(5 + bytes.len());
  encoded.push(0x07);
  encoded.extend_from_slice(&length.to_le_bytes());
  encoded.extend_from_slice(bytes);
  Ok(encoded)
}

fn decode_catalog(engine: &StorageEngine, selected: &LoadedMutableControlV1) -> EngineResult<EmergencySpillCatalogBodyV1> {
  let control = decode_system_control(&selected.bytes, engine.hash_algo()).map_err(format_error)?;
  decode_emergency_spill_catalog_body(control.body, engine.hash_algo()).map_err(format_error)
}

fn decode_latch(engine: &StorageEngine, selected: &LoadedMutableControlV1) -> EngineResult<DurabilityLatchBodyV1> {
  let control = decode_system_control(&selected.bytes, engine.hash_algo()).map_err(format_error)?;
  decode_durability_latch_body(control.body, engine.hash_algo()).map_err(format_error)
}

fn transition_to_repairing(engine: &StorageEngine, database_id: [u8; 16]) -> EngineResult<()> {
  let store = V3TransitionControlStore::new(engine);
  let mut catalog = selected_catalog(&store, engine, database_id)?;
  if catalog.body.state != CATALOG_COMPLETE {
    catalog.body.catalog_generation = next_generation(catalog.body.catalog_generation, "spill catalog")?;
    catalog.body.state = CATALOG_REPLAYING;
    catalog.body.repair_receipt_hash.fill(0);
    let bytes = encode_emergency_spill_catalog_control(next_sequence(catalog.selected.sequence)?, &catalog.body, engine.hash_algo())
      .map_err(format_error)?;
    store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &bytes)?;
    engine.refresh_persistent_durability_recovery()?;
  }

  let selected_catalog = selected_catalog(&store, engine, database_id)?;
  let mut latch = selected_latch(&store, engine, database_id)?;
  if latch.body.state == LATCH_CLEARED {
    return Err(EngineError::InvalidInput("cleared durability latch cannot be reopened as the same repair incident".to_string()));
  }
  latch.body.latch_generation = next_generation(latch.body.latch_generation, "durability latch")?;
  latch.body.state = LATCH_REPAIR_VERIFYING;
  latch.body.flags |= SPILL_CATALOG_PRESENT;
  latch.body.emergency_spill_catalog_payload_hash = catalog_payload_hash(engine, &selected_catalog.selected.bytes)?;
  let bytes =
    encode_durability_latch_control(next_sequence(latch.selected.sequence)?, &latch.body, engine.hash_algo()).map_err(format_error)?;
  store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &bytes)?;
  engine.refresh_persistent_durability_recovery()
}

fn transition_to_complete(engine: &StorageEngine, database_id: [u8; 16], proposed_receipt: &[u8]) -> EngineResult<Vec<u8>> {
  let store = V3TransitionControlStore::new(engine);
  let mut catalog = selected_catalog(&store, engine, database_id)?;
  if catalog.body.state != CATALOG_COMPLETE {
    if catalog.body.rows.iter().any(|row| row.replay_state == 5) {
      return Err(EngineError::DurabilityFailure("failed spill rows prevent durability repair completion".to_string()));
    }
    for row in &mut catalog.body.rows {
      if row.replay_state == 1 {
        row.replay_state = 2;
      }
    }
    catalog.body.catalog_generation = next_generation(catalog.body.catalog_generation, "spill catalog")?;
    catalog.body.state = CATALOG_COMPLETE;
    catalog.body.repair_receipt_hash = proposed_receipt.to_vec();
    let bytes = encode_emergency_spill_catalog_control(next_sequence(catalog.selected.sequence)?, &catalog.body, engine.hash_algo())
      .map_err(format_error)?;
    store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &bytes)?;
    engine.refresh_persistent_durability_recovery()?;
  }

  let selected_catalog = selected_catalog(&store, engine, database_id)?;
  let repair_receipt_hash = selected_catalog.body.repair_receipt_hash.clone();
  let mut latch = selected_latch(&store, engine, database_id)?;
  latch.body.latch_generation = next_generation(latch.body.latch_generation, "durability latch")?;
  latch.body.state = LATCH_CLEARED;
  latch.body.flags |= SPILL_CATALOG_PRESENT;
  latch.body.emergency_spill_catalog_payload_hash = catalog_payload_hash(engine, &selected_catalog.selected.bytes)?;
  let bytes =
    encode_durability_latch_control(next_sequence(latch.selected.sequence)?, &latch.body, engine.hash_algo()).map_err(format_error)?;
  store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &bytes)?;
  engine.refresh_persistent_durability_recovery()?;
  if engine.persistent_durability_recovery().is_none_or(|recovery| recovery.blocks_writes) {
    return Err(EngineError::DurabilityFailure("hard-proven durability repair did not clear persistent write admission".to_string()));
  }
  Ok(repair_receipt_hash)
}

struct SelectedLatch {
  selected: LoadedMutableControlV1,
  body: DurabilityLatchBodyV1,
}

struct SelectedCatalog {
  selected: LoadedMutableControlV1,
  body: EmergencySpillCatalogBodyV1,
}

fn selected_latch(store: &V3TransitionControlStore<'_>, engine: &StorageEngine, database_id: [u8; 16]) -> EngineResult<SelectedLatch> {
  let selected = store
    .load_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[])?
    .ok_or_else(|| EngineError::DurabilityFailure("explicit repair requires a selected durability latch".to_string()))?;
  let control = decode_system_control(&selected.bytes, engine.hash_algo()).map_err(format_error)?;
  let body = decode_durability_latch_body(control.body, engine.hash_algo()).map_err(format_error)?;
  Ok(SelectedLatch { selected, body })
}

fn selected_catalog(store: &V3TransitionControlStore<'_>, engine: &StorageEngine, database_id: [u8; 16]) -> EngineResult<SelectedCatalog> {
  let selected = store
    .load_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[])?
    .ok_or_else(|| EngineError::DurabilityFailure("explicit repair requires a selected emergency spill catalog".to_string()))?;
  let control = decode_system_control(&selected.bytes, engine.hash_algo()).map_err(format_error)?;
  let body = decode_emergency_spill_catalog_body(control.body, engine.hash_algo()).map_err(format_error)?;
  Ok(SelectedCatalog { selected, body })
}

fn catalog_payload_hash(engine: &StorageEngine, encoded: &[u8]) -> EngineResult<Vec<u8>> {
  let control = decode_system_control(encoded, engine.hash_algo()).map_err(format_error)?;
  Ok(digest_parts(engine.hash_algo(), &[SPILL_CATALOG_PAYLOAD_DOMAIN, control.body]))
}

fn next_sequence(sequence: u64) -> EngineResult<u64> {
  sequence.checked_add(1).ok_or_else(|| EngineError::DurabilityFailure("recovery control sequence exhausted".to_string()))
}

fn next_generation(generation: u64, name: &str) -> EngineResult<u64> {
  generation.checked_add(1).ok_or_else(|| EngineError::DurabilityFailure(format!("{name} generation exhausted")))
}

fn recovery_reason(
  latch_state: Option<u16>,
  catalog_state: Option<u16>,
  blocks_writes: bool,
  catalog_presence_consistent: bool,
  catalog_reference_consistent: bool,
) -> EngineResult<String> {
  if !catalog_presence_consistent {
    return Ok(
      "durability recovery is incomplete (spill-catalog presence disagrees with selected authority); run explicit repair".to_string(),
    );
  }
  if !catalog_reference_consistent {
    return Ok("durability recovery is incomplete (latch/catalog publication is partially advanced); run explicit repair".to_string());
  }

  match (latch_state, catalog_state, blocks_writes) {
    (Some(state), Some(catalog_state), true) => {
      Ok(format!("durability recovery is incomplete (latch state {state}, spill catalog state {catalog_state}); run explicit repair"))
    }
    (Some(state), None, true) => Ok(format!("durability recovery is incomplete (latch state {state}); run explicit repair")),
    (None, Some(state), true) => {
      Ok(format!("durability recovery is incomplete (spill catalog state {state}, clear latch missing); run explicit repair"))
    }
    (None, None, true) => {
      Err(EngineError::DurabilityFailure("persistent recovery controls disappeared while building write-admission state".to_string()))
    }
    (_, _, false) => Ok("durability recovery is complete".to_string()),
  }
}

fn recovery_database_id(latch: Option<&LoadedMutableControlV1>, catalog: Option<&LoadedMutableControlV1>) -> EngineResult<[u8; 16]> {
  match (latch, catalog) {
    (Some(latch), Some(catalog)) if latch.database_id != catalog.database_id => Err(EngineError::InvalidInput(
      "persistent durability latch and emergency spill catalog have different database identities".to_string(),
    )),
    (Some(latch), _) => Ok(latch.database_id),
    (_, Some(catalog)) => Ok(catalog.database_id),
    (None, None) => Err(EngineError::DurabilityFailure(
      "persistent recovery controls disappeared before their database identity was selected".to_string(),
    )),
  }
}

fn format_error(error: super::reader::FormatError) -> EngineError {
  EngineError::InvalidInput(format!("malformed persistent durability recovery control: {error}"))
}

#[cfg(test)]
#[path = "../../../spec/engine/v4_durability_recovery_internal_spec.rs"]
mod v4_durability_recovery_internal_spec;
