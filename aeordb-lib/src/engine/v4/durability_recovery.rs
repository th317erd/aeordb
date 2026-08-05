use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::storage_engine::{DurabilityRepairAuthorityGuard, StorageEngine};

use super::control_store::{LoadedMutableControlV1, V3TransitionControlStore};
use super::hash::digest_parts;
use super::system_control::{
  DurabilityLatchBodyV1, EmergencySpillCatalogBodyV1, SystemControlKindV1, decode_durability_latch_body,
  decode_emergency_spill_catalog_body, decode_system_control, encode_durability_latch_control, encode_emergency_spill_catalog_control,
};

const LATCH_REPAIR_VERIFYING: u16 = 2;
const LATCH_CLEARED: u16 = 3;
const CATALOG_REPLAYING: u16 = 2;
const CATALOG_COMPLETE: u16 = 3;
const SPILL_CATALOG_PRESENT: u32 = 1;
const SPILL_CATALOG_PAYLOAD_DOMAIN: &[u8] = b"aeordb.emergency-spill-catalog-payload.v1\0";
const REPAIR_RECEIPT_DOMAIN: &[u8] = b"aeordb.durability-repair-receipt.v1\0";

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurabilityRepairReceipt {
  pub database_id: [u8; 16],
  pub repair_receipt_hash: Vec<u8>,
  pub selected_header_sequence: u64,
  pub durable_sequence: u64,
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
  if latch.is_none() && catalog.is_none() {
    return Ok(None);
  }

  let database_id = recovery_database_id(latch.as_ref(), catalog.as_ref())?;
  let latch_body = latch
    .as_ref()
    .map(|selected| {
      let control = decode_system_control(&selected.bytes, engine.hash_algo()).map_err(format_error)?;
      decode_durability_latch_body(control.body, engine.hash_algo()).map_err(format_error)
    })
    .transpose()?;
  let catalog_control =
    catalog.as_ref().map(|selected| decode_system_control(&selected.bytes, engine.hash_algo()).map_err(format_error)).transpose()?;
  let catalog_body = catalog_control
    .as_ref()
    .map(|control| decode_emergency_spill_catalog_body(control.body, engine.hash_algo()).map_err(format_error))
    .transpose()?;

  let catalog_presence_consistent =
    latch_body.as_ref().is_none_or(|body| (body.flags & SPILL_CATALOG_PRESENT != 0) == catalog_control.is_some());
  let mut catalog_reference_consistent = true;
  if let Some(latch_body) = latch_body.as_ref() {
    if latch_body.flags & SPILL_CATALOG_PRESENT != 0 {
      if let Some(catalog_control) = catalog_control.as_ref() {
        let expected = digest_parts(engine.hash_algo(), &[SPILL_CATALOG_PAYLOAD_DOMAIN, catalog_control.body]);
        if latch_body.emergency_spill_catalog_payload_hash != expected {
          catalog_reference_consistent = false;
          if latch_body.state == LATCH_CLEARED {
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
  let reason = if !catalog_presence_consistent {
    "durability recovery is incomplete (spill-catalog presence disagrees with selected authority); run explicit repair".to_string()
  } else if !catalog_reference_consistent {
    "durability recovery is incomplete (latch/catalog publication is partially advanced); run explicit repair".to_string()
  } else {
    match (latch_state, catalog_state, blocks_writes) {
      (Some(state), Some(catalog_state), true) => {
        format!("durability recovery is incomplete (latch state {state}, spill catalog state {catalog_state}); run explicit repair")
      }
      (Some(state), None, true) => format!("durability recovery is incomplete (latch state {state}); run explicit repair"),
      (None, Some(state), true) => {
        format!("durability recovery is incomplete (spill catalog state {state}, clear latch missing); run explicit repair")
      }
      (None, None, true) => unreachable!("recovery controls disappeared while building admission state"),
      (_, _, false) => "durability recovery is complete".to_string(),
    }
  };

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

fn recovery_database_id(latch: Option<&LoadedMutableControlV1>, catalog: Option<&LoadedMutableControlV1>) -> EngineResult<[u8; 16]> {
  match (latch, catalog) {
    (Some(latch), Some(catalog)) if latch.database_id != catalog.database_id => Err(EngineError::InvalidInput(
      "persistent durability latch and emergency spill catalog have different database identities".to_string(),
    )),
    (Some(latch), _) => Ok(latch.database_id),
    (_, Some(catalog)) => Ok(catalog.database_id),
    (None, None) => unreachable!("recovery identity requested without controls"),
  }
}

fn format_error(error: super::reader::FormatError) -> EngineError {
  EngineError::InvalidInput(format!("malformed persistent durability recovery control: {error}"))
}
