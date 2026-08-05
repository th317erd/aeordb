use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::storage_engine::StorageEngine;

use super::control_store::{LoadedMutableControlV1, V3TransitionControlStore};
use super::hash::digest_parts;
use super::system_control::{SystemControlKindV1, decode_durability_latch_body, decode_emergency_spill_catalog_body, decode_system_control};

const LATCH_CLEARED: u16 = 3;
const CATALOG_COMPLETE: u16 = 3;
const SPILL_CATALOG_PRESENT: u32 = 1;
const SPILL_CATALOG_PAYLOAD_DOMAIN: &[u8] = b"aeordb.emergency-spill-catalog-payload.v1\0";

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
    latch_body.as_ref().is_none_or(|latch_body| (latch_body.flags & SPILL_CATALOG_PRESENT != 0) == catalog_control.is_some());
  if let Some(latch_body) = latch_body.as_ref() {
    if latch_body.flags & SPILL_CATALOG_PRESENT != 0 {
      if let Some(catalog_control) = catalog_control.as_ref() {
        let expected = digest_parts(engine.hash_algo(), &[SPILL_CATALOG_PAYLOAD_DOMAIN, catalog_control.body]);
        if latch_body.emergency_spill_catalog_payload_hash != expected {
          return Err(EngineError::InvalidInput(
            "persistent durability latch references a different emergency spill catalog payload".to_string(),
          ));
        }
      }
    }
  }

  let latch_state = latch_body.as_ref().map(|body| body.state);
  let catalog_state = catalog_body.as_ref().map(|body| body.state);
  let latch_cleared = latch_state == Some(LATCH_CLEARED);
  let catalog_complete = catalog_state.is_none_or(|state| state == CATALOG_COMPLETE);
  let blocks_writes = !latch_cleared || !catalog_complete || !catalog_presence_consistent;
  let reason = if !catalog_presence_consistent {
    "durability recovery is incomplete (spill-catalog presence disagrees with selected authority); run explicit repair".to_string()
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
