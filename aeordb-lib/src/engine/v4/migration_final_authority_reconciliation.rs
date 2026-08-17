//! Bounded final retained-root and SystemFamily reconciliation.
//!
//! This owner runs only while the nonconstructible source-write freeze from
//! final namespace reconciliation remains alive. It stages every final source
//! mapping through a caller-owned sink, but never publishes AMPR or a root-map
//! control itself.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use super::entity::EntryTypeV4;
use super::first_authority::{FirstAuthorityPublicationErrorV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use super::migration_base_clone_execution::{
  MAX_DIRECTORY_DEPTH, MigrationBaseCloneExecutionErrorV1, MigrationBaseCloneSeedKindV1, MigrationBaseCloneSeedV1,
  MigrationSubtreeCloneRequestV1, translate_migration_subtree_v1,
};
use super::migration_capture_replay::{MigrationCaptureReplayAuthorityTemplateV1, namespace_root_for_tree};
use super::migration_final_reconciliation::{
  MigrationFinalNamespaceReconciliationReceiptV1, MigrationFinalReconciliationErrorV1, MigrationSourceWriteFreezeV1,
};
use super::migration_preflight::{AuthorityInventoryCountsV1, MigrationPreflightPermitV1};
use super::reader::FormatError;
use super::system_family::{MigrationPolicyV1, SystemFamilyPolicyDecisionV1, SystemFamilyPolicyResolverV1, SystemFamilySubjectV1};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::PlatformFileIdentityDescriptorV1;
use crate::engine::{EngineError, EngineResult, EntryType};

const INVENTORY_DIGEST_DOMAIN: &[u8] = b"aeordb.migration-final-authority-inventory.v1\0";
const INVENTORY_COUNTS_DOMAIN: &[u8] = b"authority-counts\0";
const MAPPING_DIGEST_DOMAIN: &[u8] = b"aeordb.migration-final-root-mappings.v1\0";
const MAXIMUM_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const MAXIMUM_WORK_ITEMS: u64 = 1 << 40;
const MAXIMUM_ENTITY_BYTES: usize = 64 * 1024 * 1024;
const MAXIMUM_IDENTITY_BYTES: usize = u16::MAX as usize;
const MAXIMUM_PATH_BYTES: usize = u16::MAX as usize;
const OWNED_ALLOCATION_OVERHEAD: u64 = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationFinalAuthoritySeedV1 {
  pub authority_identity: Vec<u8>,
  pub source_write_sequence: u64,
  pub system_family_id: Option<u16>,
  pub logical_bytes: u64,
  pub seed: MigrationBaseCloneSeedV1,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrationFinalAuthoritySeedCountsV1 {
  pub current_heads: u64,
  pub snapshots: u64,
  pub forks: u64,
  pub sync_pins: u64,
  pub maintenance: u64,
  pub detached_protected: u64,
}

impl MigrationFinalAuthoritySeedCountsV1 {
  pub fn root_count(self) -> EngineResult<u64> {
    self
      .current_heads
      .checked_add(self.snapshots)
      .and_then(|value| value.checked_add(self.forks))
      .and_then(|value| value.checked_add(self.sync_pins))
      .and_then(|value| value.checked_add(self.maintenance))
      .ok_or_else(|| EngineError::InvalidInput("final authority root count overflowed".to_string()))
  }

  fn increment(&mut self, kind: MigrationBaseCloneSeedKindV1) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
    let value = match kind {
      MigrationBaseCloneSeedKindV1::CurrentHead => &mut self.current_heads,
      MigrationBaseCloneSeedKindV1::Snapshot => &mut self.snapshots,
      MigrationBaseCloneSeedKindV1::Fork => &mut self.forks,
      MigrationBaseCloneSeedKindV1::SyncPin => &mut self.sync_pins,
      MigrationBaseCloneSeedKindV1::Maintenance => &mut self.maintenance,
      MigrationBaseCloneSeedKindV1::DetachedProtectedPath => &mut self.detached_protected,
    };
    *value = value.checked_add(1).ok_or_else(|| {
      MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_count_overflow",
        "final authority seed count overflowed",
      )
    })?;
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationFinalAuthorityInventoryClosureV1 {
  pub complete: bool,
  pub database_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub source_physical_identity: PlatformFileIdentityDescriptorV1,
  pub source_header_sequence: u64,
  pub frozen_source_root: Vec<u8>,
  pub frozen_source_publication_sequence: u64,
  pub unresolved_family_count: u64,
  pub source_authority_counts: AuthorityInventoryCountsV1,
  pub seed_counts: MigrationFinalAuthoritySeedCountsV1,
  pub seed_count: u64,
  pub authority_digest: [u8; 32],
  pub system_family_registry_fingerprint: Vec<u8>,
}

/// Caller-owned canonical stream captured while the source freeze is held.
pub trait MigrationFinalAuthorityInventorySourceV1 {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationFinalAuthoritySeedV1>>;
  fn finish(&mut self) -> EngineResult<MigrationFinalAuthorityInventoryClosureV1>;
}

/// Bounded lookup into mappings already staged by base clone or replay.
pub trait MigrationFinalPriorRootMappingLookupV1 {
  fn lookup_destination_entity(&mut self, seed: &MigrationFinalAuthoritySeedV1) -> EngineResult<Option<Vec<u8>>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationFinalRootMappingV1 {
  pub kind: MigrationBaseCloneSeedKindV1,
  pub authority_identity: Vec<u8>,
  pub source_write_sequence: u64,
  pub source_path: String,
  pub source_entry_type: EntryType,
  pub source_root: Vec<u8>,
  pub system_family_id: Option<u16>,
  pub destination_entity: Option<Vec<u8>>,
  pub destination_namespace_root: Option<Vec<u8>>,
  pub destination_tree_root: Option<Vec<u8>>,
  pub reused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationFinalRootMappingClosureV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub source_header_sequence: u64,
  pub frozen_source_root: Vec<u8>,
  pub frozen_source_publication_sequence: u64,
  pub destination_header_sequence: u64,
  pub destination_namespace_root: Vec<u8>,
  pub destination_tree_root: Vec<u8>,
  pub source_authority_counts: AuthorityInventoryCountsV1,
  pub seed_counts: MigrationFinalAuthoritySeedCountsV1,
  pub mapping_count: u64,
  pub omitted_mapping_count: u64,
  pub authority_digest: [u8; 32],
  pub mapping_digest: [u8; 32],
  pub system_family_registry_fingerprint: Vec<u8>,
}

/// Staging sink for the later persistent root-map owner.
///
/// Exact duplicate rows and closures must be accepted idempotently. A failed
/// or absent `finish_root_mappings` call leaves the staging set unpublished.
pub trait MigrationFinalRootMappingSinkV1 {
  fn record_root_mapping(&mut self, mapping: &MigrationFinalRootMappingV1) -> EngineResult<()>;
  fn finish_root_mappings(&mut self, closure: &MigrationFinalRootMappingClosureV1) -> EngineResult<()>;
}

pub struct MigrationFinalAuthorityReconciliationRequestV1<'request, 'freeze, 'source> {
  pub permit: &'request MigrationPreflightPermitV1,
  pub namespace: &'request MigrationFinalNamespaceReconciliationReceiptV1<'freeze, 'source>,
  pub inventory: &'request mut dyn MigrationFinalAuthorityInventorySourceV1,
  pub prior_mappings: &'request mut dyn MigrationFinalPriorRootMappingLookupV1,
  pub root_sink: &'request mut dyn MigrationFinalRootMappingSinkV1,
  pub destination: &'request V4FirstAuthorityPublisher,
  pub authority: &'request MigrationCaptureReplayAuthorityTemplateV1,
  pub memory: &'request MemoryCoordinator,
  pub cancellation: &'request CancellationToken,
  pub publication_timestamp_ms: u64,
  pub maximum_memory_bytes: u64,
  pub maximum_work_items: u64,
  pub maximum_subtree_memory_bytes: u64,
  pub maximum_subtree_work_items: u64,
  pub maximum_total_subtree_work_items: u64,
  pub maximum_decoded_chunk_bytes: usize,
  pub maximum_destination_entity_bytes: usize,
  pub maximum_directory_depth: usize,
}

pub struct MigrationFinalAuthorityReconciliationProofV1<'freeze, 'source> {
  freeze: &'freeze MigrationSourceWriteFreezeV1<'source>,
  permit_evidence_fingerprint: [u8; 32],
  closure: MigrationFinalRootMappingClosureV1,
}

impl MigrationFinalAuthorityReconciliationProofV1<'_, '_> {
  pub fn validate_live(&self, destination: &V4FirstAuthorityPublisher) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
    self.freeze.validate_unchanged()?;
    let observation = destination.observe()?;
    if observation.selected.header.database_id != self.closure.database_id
      || observation.selected.header.physical_instance_id != self.closure.destination_physical_instance_id
      || observation.selected.header.slot_sequence != self.closure.destination_header_sequence
      || observation.selected.header.head_hash != self.closure.destination_namespace_root
    {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_destination_changed",
        "destination header or HEAD changed after final authority closure",
      ));
    }
    Ok(())
  }

  pub(crate) fn validate_for_completion(
    &self,
    permit: &MigrationPreflightPermitV1,
    destination: &V4FirstAuthorityPublisher,
  ) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
    if permit.evidence_fingerprint() != self.permit_evidence_fingerprint {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_proof_binding",
        "final authority proof belongs to another permit or destination owner",
      ));
    }
    self.validate_live(destination)
  }

  pub(crate) fn validate_for_destination_verification(
    &self,
    permit: &MigrationPreflightPermitV1,
    destination: &V4FirstAuthorityPublisher,
    expected_destination_header_sequence: u64,
    expected_destination_head: &[u8],
  ) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
    if permit.evidence_fingerprint() != self.permit_evidence_fingerprint {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_proof_binding",
        "final authority proof belongs to another permit or destination owner",
      ));
    }
    self.freeze.validate_unchanged()?;
    if expected_destination_header_sequence < self.closure.destination_header_sequence
      || expected_destination_head != self.closure.destination_namespace_root
    {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_destination_changed",
        "selected root-map authority precedes or changes the frozen reconciliation destination",
      ));
    }
    let observation = destination.observe()?;
    if observation.selected.redundancy_degraded
      || observation.selected.header.database_id != self.closure.database_id
      || observation.selected.header.physical_instance_id != self.closure.destination_physical_instance_id
      || observation.selected.header.slot_sequence != expected_destination_header_sequence
      || observation.selected.header.head_hash != expected_destination_head
    {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_destination_changed",
        "destination identity, header sequence, redundancy, or HEAD changed after selected root-map verification",
      ));
    }
    Ok(())
  }

  pub(crate) const fn closure(&self) -> &MigrationFinalRootMappingClosureV1 {
    &self.closure
  }
}

pub struct MigrationFinalAuthorityReconciliationReceiptV1<'freeze, 'source> {
  pub processed_seed_count: u64,
  pub reused_mapping_count: u64,
  pub translated_seed_count: u64,
  pub omitted_seed_count: u64,
  pub translated_subtree_work_items: u64,
  pub peak_accounted_memory_bytes: u64,
  pub mapping_closure: MigrationFinalRootMappingClosureV1,
  proof: MigrationFinalAuthorityReconciliationProofV1<'freeze, 'source>,
}

impl<'freeze, 'source> MigrationFinalAuthorityReconciliationReceiptV1<'freeze, 'source> {
  pub const fn proof(&self) -> &MigrationFinalAuthorityReconciliationProofV1<'freeze, 'source> {
    &self.proof
  }
}

#[derive(Debug)]
pub enum MigrationFinalAuthorityReconciliationErrorV1 {
  Invalid { code: &'static str, message: String },
  InventorySource(EngineError),
  PriorMapping(EngineError),
  RootSink(EngineError),
  Clone(MigrationBaseCloneExecutionErrorV1),
  Namespace(MigrationFinalReconciliationErrorV1),
  Publication(FirstAuthorityPublicationErrorV1),
  Format(FormatError),
  Memory(MemoryCoordinatorError),
}

impl MigrationFinalAuthorityReconciliationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::InventorySource(_) => "migration_final_authority_inventory_source",
      Self::PriorMapping(_) => "migration_final_authority_prior_mapping",
      Self::RootSink(_) => "migration_final_authority_root_sink",
      Self::Clone(source) => source.code(),
      Self::Namespace(source) => source.code(),
      Self::Publication(source) => source.code(),
      Self::Format(source) => source.code(),
      Self::Memory(_) => "migration_final_authority_memory_admission",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationFinalAuthorityReconciliationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::InventorySource(source) | Self::PriorMapping(source) | Self::RootSink(source) => Display::fmt(source, formatter),
      Self::Clone(source) => Display::fmt(source, formatter),
      Self::Namespace(source) => Display::fmt(source, formatter),
      Self::Publication(source) => Display::fmt(source, formatter),
      Self::Format(source) => Display::fmt(source, formatter),
      Self::Memory(source) => Display::fmt(source, formatter),
    }
  }
}

impl Error for MigrationFinalAuthorityReconciliationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Invalid { .. } => None,
      Self::InventorySource(source) | Self::PriorMapping(source) | Self::RootSink(source) => Some(source),
      Self::Clone(source) => Some(source),
      Self::Namespace(source) => Some(source),
      Self::Publication(source) => Some(source),
      Self::Format(source) => Some(source),
      Self::Memory(source) => Some(source),
    }
  }
}

impl From<MigrationBaseCloneExecutionErrorV1> for MigrationFinalAuthorityReconciliationErrorV1 {
  fn from(source: MigrationBaseCloneExecutionErrorV1) -> Self {
    Self::Clone(source)
  }
}

impl From<MigrationFinalReconciliationErrorV1> for MigrationFinalAuthorityReconciliationErrorV1 {
  fn from(source: MigrationFinalReconciliationErrorV1) -> Self {
    Self::Namespace(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for MigrationFinalAuthorityReconciliationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Publication(source)
  }
}

impl From<FormatError> for MigrationFinalAuthorityReconciliationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<MemoryCoordinatorError> for MigrationFinalAuthorityReconciliationErrorV1 {
  fn from(source: MemoryCoordinatorError) -> Self {
    Self::Memory(source)
  }
}

pub fn execute_final_authority_reconciliation_v1<'request, 'freeze, 'source>(
  request: MigrationFinalAuthorityReconciliationRequestV1<'request, 'freeze, 'source>,
) -> Result<MigrationFinalAuthorityReconciliationReceiptV1<'freeze, 'source>, MigrationFinalAuthorityReconciliationErrorV1> {
  validate_request(&request)?;
  let freeze = request.namespace.live_freeze();
  freeze.validate_unchanged()?;
  validate_destination_authority(&request, request.namespace.destination_header_sequence)?;
  let registry = SystemFamilyPolicyResolverV1::embedded(request.permit.hash_algorithm())?;
  if registry.registry().operational_fingerprint != request.permit.system_family_registry_fingerprint() {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_system_family",
      "selected SystemFamily registry differs from the migration permit",
    ));
  }
  let mut budget = InventoryMemoryBudgetV1::new(request.memory, request.maximum_memory_bytes)?;
  let mut work = WorkBudgetV1::new(request.maximum_work_items);
  let mut inventory_digest = blake3::Hasher::new();
  inventory_digest.update(INVENTORY_DIGEST_DOMAIN);
  let mut mapping_digest = blake3::Hasher::new();
  mapping_digest.update(MAPPING_DIGEST_DOMAIN);
  let mut previous_order: Option<(u8, Vec<u8>, u64)> = None;
  let mut seed_counts = MigrationFinalAuthoritySeedCountsV1::default();
  let mut processed_seed_count = 0u64;
  let mut reused_mapping_count = 0u64;
  let mut translated_seed_count = 0u64;
  let mut omitted_seed_count = 0u64;
  let mut translated_subtree_work_items = 0u64;
  let mut remaining_subtree_work_items = request.maximum_total_subtree_work_items;

  while let Some(seed) = request.inventory.next_seed().map_err(MigrationFinalAuthorityReconciliationErrorV1::InventorySource)? {
    check_cancelled(request.cancellation)?;
    work.consume("authority seed")?;
    let row_charge = seed_memory_charge(&seed)?;
    budget.reserve(row_charge)?;
    validate_seed(&seed, freeze, request.permit.hash_algorithm().hash_length(), registry, &previous_order)?;
    update_inventory_digest(&mut inventory_digest, &seed)?;
    seed_counts.increment(seed.seed.kind)?;
    processed_seed_count = checked_add(processed_seed_count, 1, "processed seed count")?;

    let order_charge = order_memory_charge(&seed.authority_identity)?;
    budget.reserve(order_charge)?;
    let old_order = previous_order.replace((seed_kind_tag(seed.seed.kind), seed.authority_identity.clone(), order_charge));
    if let Some((_, _, old_charge)) = old_order {
      budget.release(old_charge)?;
    }

    let disposition = seed_disposition(&seed, registry)?;
    let (destination_entity, reused, translated_work) = if seed.seed.kind == MigrationBaseCloneSeedKindV1::CurrentHead {
      let destination_tree_root = request.namespace.destination_tree_root.clone();
      validate_destination_entity(&request, &seed, &destination_tree_root)?;
      (Some(destination_tree_root), false, 0)
    } else if disposition == SeedDispositionV1::Omit {
      (None, false, 0)
    } else if let Some(mapped) =
      request.prior_mappings.lookup_destination_entity(&seed).map_err(MigrationFinalAuthorityReconciliationErrorV1::PriorMapping)?
    {
      validate_destination_entity(&request, &seed, &mapped)?;
      (Some(mapped), true, 0)
    } else {
      let available = request.maximum_subtree_work_items.min(remaining_subtree_work_items);
      if available == 0 {
        return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
          "migration_final_authority_subtree_work_limit",
          "final authority reconciliation exhausted its aggregate subtree work bound",
        ));
      }
      let translated = translate_migration_subtree_v1(MigrationSubtreeCloneRequestV1 {
        permit: request.permit,
        source: freeze.source(),
        destination: request.destination,
        memory: request.memory,
        cancellation: request.cancellation,
        publication_timestamp_ms: request.publication_timestamp_ms,
        maximum_work_items: available,
        maximum_memory_bytes: request.maximum_subtree_memory_bytes,
        maximum_decoded_chunk_bytes: request.maximum_decoded_chunk_bytes,
        maximum_directory_depth: request.maximum_directory_depth,
        path: &seed.seed.path,
        hash: &seed.seed.hash,
        entry_type: seed.seed.entry_type,
        logical_bytes: seed.logical_bytes,
      })?;
      let translated = translated.ok_or_else(|| {
        MigrationFinalAuthorityReconciliationErrorV1::invalid(
          "migration_final_authority_required_mapping_omitted",
          "a required-copy final authority seed was omitted by translation",
        )
      })?;
      remaining_subtree_work_items = remaining_subtree_work_items.checked_sub(translated.work_items).ok_or_else(|| {
        MigrationFinalAuthorityReconciliationErrorV1::invalid(
          "migration_final_authority_subtree_work_accounting",
          "translated subtree work exceeded the aggregate bound",
        )
      })?;
      (Some(translated.hash), false, translated.work_items)
    };

    if reused {
      reused_mapping_count = checked_add(reused_mapping_count, 1, "reused mapping count")?;
    } else if translated_work != 0 {
      translated_seed_count = checked_add(translated_seed_count, 1, "translated seed count")?;
      translated_subtree_work_items = checked_add(translated_subtree_work_items, translated_work, "translated subtree work count")?;
    } else if destination_entity.is_none() {
      omitted_seed_count = checked_add(omitted_seed_count, 1, "omitted seed count")?;
    }

    let (destination_namespace_root, destination_tree_root) = if is_root_kind(seed.seed.kind) {
      if seed.seed.kind == MigrationBaseCloneSeedKindV1::CurrentHead {
        (Some(request.namespace.destination_namespace_root.clone()), Some(request.namespace.destination_tree_root.clone()))
      } else {
        let tree_hash = destination_entity.as_ref().ok_or_else(|| {
          MigrationFinalAuthorityReconciliationErrorV1::invalid(
            "migration_final_authority_root_omitted",
            "a retained namespace root cannot be omitted",
          )
        })?;
        let loaded = validate_destination_entity(&request, &seed, tree_hash)?;
        let tree = PreparedNamespaceTreeV0 { root_hash: tree_hash.clone(), stored_value: loaded.stored_value };
        let namespace = namespace_root_for_tree(request.permit.hash_algorithm(), &tree, request.authority)
          .map_err(|error| MigrationFinalAuthorityReconciliationErrorV1::invalid(error.code(), error.to_string()))?;
        (Some(namespace), Some(tree_hash.clone()))
      }
    } else {
      (None, None)
    };
    let mapping = MigrationFinalRootMappingV1 {
      kind: seed.seed.kind,
      authority_identity: seed.authority_identity.clone(),
      source_write_sequence: seed.source_write_sequence,
      source_path: seed.seed.path.clone(),
      source_entry_type: seed.seed.entry_type,
      source_root: seed.seed.hash.clone(),
      system_family_id: seed.system_family_id,
      destination_entity,
      destination_namespace_root,
      destination_tree_root,
      reused,
    };
    let mapping_charge = mapping_memory_charge(&mapping)?;
    budget.reserve(mapping_charge)?;
    update_mapping_digest(&mut mapping_digest, &mapping)?;
    request.root_sink.record_root_mapping(&mapping).map_err(MigrationFinalAuthorityReconciliationErrorV1::RootSink)?;
    budget.release(mapping_charge)?;
    budget.release(row_charge)?;
  }

  check_cancelled(request.cancellation)?;
  let closure = request.inventory.finish().map_err(MigrationFinalAuthorityReconciliationErrorV1::InventorySource)?;
  check_cancelled(request.cancellation)?;
  let closure_charge = inventory_closure_memory_charge(&closure)?;
  budget.reserve(closure_charge)?;
  update_authority_counts_digest(&mut inventory_digest, closure.source_authority_counts);
  let authority_digest = *inventory_digest.finalize().as_bytes();
  validate_inventory_closure(request.permit, freeze, registry, &closure, seed_counts, processed_seed_count, authority_digest)?;
  if let Some((_, _, charge)) = previous_order.take() {
    budget.release(charge)?;
  }
  freeze.validate_unchanged()?;
  let observation = validate_destination_authority(&request, request.namespace.destination_header_sequence)?;
  let mapping_closure = MigrationFinalRootMappingClosureV1 {
    database_id: request.permit.database_id(),
    migration_id: request.permit.migration_id(),
    source_physical_instance_id: request.permit.source_physical_instance_id(),
    destination_physical_instance_id: request.permit.destination_physical_instance_id(),
    source_header_sequence: freeze.authority().header_sequence,
    frozen_source_root: freeze.authority().namespace_root.clone(),
    frozen_source_publication_sequence: freeze.authority().hard_publication_frontier,
    destination_header_sequence: observation.selected.header.slot_sequence,
    destination_namespace_root: request.namespace.destination_namespace_root.clone(),
    destination_tree_root: request.namespace.destination_tree_root.clone(),
    source_authority_counts: closure.source_authority_counts,
    seed_counts,
    mapping_count: processed_seed_count,
    omitted_mapping_count: omitted_seed_count,
    authority_digest,
    mapping_digest: *mapping_digest.finalize().as_bytes(),
    system_family_registry_fingerprint: registry.registry().operational_fingerprint.to_vec(),
  };
  budget.release(closure_charge)?;
  drop(closure);
  check_cancelled(request.cancellation)?;
  request.root_sink.finish_root_mappings(&mapping_closure).map_err(MigrationFinalAuthorityReconciliationErrorV1::RootSink)?;
  check_cancelled(request.cancellation)?;
  freeze.validate_unchanged()?;
  let after_sink = validate_destination_authority(&request, mapping_closure.destination_header_sequence)?;
  if after_sink.selected.header.slot_sequence != mapping_closure.destination_header_sequence
    || after_sink.selected.header.head_hash != mapping_closure.destination_namespace_root
  {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_sink_mutation",
      "final mapping sink changed destination authority while finalizing staging",
    ));
  }
  let peak_accounted_memory_bytes = budget.peak;
  let proof = MigrationFinalAuthorityReconciliationProofV1 {
    freeze,
    permit_evidence_fingerprint: request.permit.evidence_fingerprint(),
    closure: mapping_closure.clone(),
  };
  Ok(MigrationFinalAuthorityReconciliationReceiptV1 {
    processed_seed_count,
    reused_mapping_count,
    translated_seed_count,
    omitted_seed_count,
    translated_subtree_work_items,
    peak_accounted_memory_bytes,
    mapping_closure,
    proof,
  })
}

fn validate_destination_authority(
  request: &MigrationFinalAuthorityReconciliationRequestV1<'_, '_, '_>,
  minimum_header_sequence: u64,
) -> Result<super::header_publication::DatabaseHeaderObservationV4, MigrationFinalAuthorityReconciliationErrorV1> {
  let observation = request.destination.observe()?;
  let header = &observation.selected.header;
  if observation.selected.redundancy_degraded
    || header.database_id != request.permit.database_id()
    || header.physical_instance_id != request.permit.destination_physical_instance_id()
    || header.slot_sequence < minimum_header_sequence
    || header.head_hash != request.namespace.destination_namespace_root
  {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_destination_binding",
      "destination identity, header lineage, redundancy, or HEAD differs from final namespace reconciliation",
    ));
  }
  Ok(observation)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeedDispositionV1 {
  Copy,
  Omit,
}

fn validate_request(
  request: &MigrationFinalAuthorityReconciliationRequestV1<'_, '_, '_>,
) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
  check_cancelled(request.cancellation)?;
  let width = request.permit.hash_algorithm().hash_length();
  let freeze = request.namespace.live_freeze();
  if freeze.authority().hash_algorithm != request.permit.hash_algorithm()
    || freeze.authority().physical_identity != request.permit.source_file_identity()
    || request.namespace.frozen_source_root != freeze.authority().namespace_root
    || request.namespace.frozen_source_publication_sequence != freeze.authority().hard_publication_frontier
    || request.namespace.destination_namespace_root.len() != width
    || request.namespace.destination_tree_root.len() != width
  {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_namespace_binding",
      "final namespace receipt, live freeze, or permit binding differs",
    ));
  }
  if request.publication_timestamp_ms == 0
    || request.publication_timestamp_ms > i64::MAX as u64
    || request.maximum_memory_bytes == 0
    || request.maximum_memory_bytes > MAXIMUM_MEMORY_BYTES
    || request.maximum_work_items == 0
    || request.maximum_work_items > MAXIMUM_WORK_ITEMS
    || request.maximum_subtree_memory_bytes == 0
    || request.maximum_subtree_memory_bytes > MAXIMUM_MEMORY_BYTES
    || request.maximum_subtree_work_items == 0
    || request.maximum_subtree_work_items > MAXIMUM_WORK_ITEMS
    || request.maximum_total_subtree_work_items == 0
    || request.maximum_total_subtree_work_items > MAXIMUM_WORK_ITEMS
    || request.maximum_decoded_chunk_bytes == 0
    || request.maximum_decoded_chunk_bytes > MAXIMUM_ENTITY_BYTES
    || request.maximum_destination_entity_bytes == 0
    || request.maximum_destination_entity_bytes > MAXIMUM_ENTITY_BYTES
    || request.maximum_directory_depth == 0
    || request.maximum_directory_depth > MAX_DIRECTORY_DEPTH
  {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_bounds",
      "final authority reconciliation time or resource bounds are invalid",
    ));
  }
  Ok(())
}

fn validate_seed(
  row: &MigrationFinalAuthoritySeedV1,
  freeze: &MigrationSourceWriteFreezeV1<'_>,
  hash_width: usize,
  resolver: SystemFamilyPolicyResolverV1,
  previous_order: &Option<(u8, Vec<u8>, u64)>,
) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
  if row.authority_identity.len() > MAXIMUM_IDENTITY_BYTES
    || row.seed.path.len() > MAXIMUM_PATH_BYTES
    || !row.seed.path.starts_with('/')
    || row.seed.hash.len() != hash_width
    || row.seed.hash.iter().all(|byte| *byte == 0)
    || row.source_write_sequence > freeze.authority().hard_publication_frontier
  {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_seed",
      "final authority seed has an invalid identity, path, hash, or source write sequence",
    ));
  }
  let kind = seed_kind_tag(row.seed.kind);
  if let Some((previous_kind, previous_identity, _)) = previous_order {
    if (*previous_kind, previous_identity.as_slice()) >= (kind, row.authority_identity.as_slice()) {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_order",
        "final authority seeds are not in strict canonical kind/identity order",
      ));
    }
  }
  match row.seed.kind {
    MigrationBaseCloneSeedKindV1::CurrentHead => {
      if previous_order.is_some()
        || !row.authority_identity.is_empty()
        || row.source_write_sequence != freeze.authority().hard_publication_frontier
        || row.system_family_id.is_some()
        || row.seed.path != "/"
        || row.seed.entry_type != EntryType::DirectoryIndex
        || row.seed.hash != freeze.authority().namespace_root
      {
        return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
          "migration_final_authority_head",
          "first final authority seed must be the exact frozen HEAD",
        ));
      }
    }
    MigrationBaseCloneSeedKindV1::Snapshot
    | MigrationBaseCloneSeedKindV1::Fork
    | MigrationBaseCloneSeedKindV1::SyncPin
    | MigrationBaseCloneSeedKindV1::Maintenance => {
      if previous_order.is_none()
        || row.authority_identity.is_empty()
        || row.system_family_id.is_some()
        || row.seed.path != "/"
        || row.seed.entry_type != EntryType::DirectoryIndex
      {
        return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
          "migration_final_authority_retained_root",
          "retained roots must follow HEAD with a unique identity and root DirectoryIndex",
        ));
      }
    }
    MigrationBaseCloneSeedKindV1::DetachedProtectedPath => {
      if previous_order.is_none() || row.authority_identity.is_empty() || row.seed.path == "/" || row.system_family_id == Some(0) {
        return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
          "migration_final_authority_detached",
          "detached protected seeds require a unique identity, family, and non-root path",
        ));
      }
      let family = resolver.policy(SystemFamilySubjectV1::Path(&row.seed.path), "final migration authority inventory")?;
      match family {
        SystemFamilyPolicyDecisionV1::Known { family_id, .. } if Some(family_id) == row.system_family_id => {}
        _ => {
          return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
            "migration_final_authority_family_binding",
            "detached protected seed does not match its selected SystemFamily",
          ));
        }
      }
    }
  }
  Ok(())
}

fn seed_disposition(
  row: &MigrationFinalAuthoritySeedV1,
  resolver: SystemFamilyPolicyResolverV1,
) -> Result<SeedDispositionV1, MigrationFinalAuthorityReconciliationErrorV1> {
  if row.seed.kind != MigrationBaseCloneSeedKindV1::DetachedProtectedPath {
    return Ok(SeedDispositionV1::Copy);
  }
  let policy = resolver.policy(SystemFamilySubjectV1::Path(&row.seed.path), "final migration authority mapping")?;
  Ok(match policy {
    SystemFamilyPolicyDecisionV1::Known { policy, .. } => match policy.migration_policy {
      MigrationPolicyV1::RequiredCopy => SeedDispositionV1::Copy,
      MigrationPolicyV1::DestinationLocal
      | MigrationPolicyV1::RebuildDestination
      | MigrationPolicyV1::OwnerConverter
      | MigrationPolicyV1::OmitDeclared => SeedDispositionV1::Omit,
      MigrationPolicyV1::FailUnknown => {
        return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
          "migration_final_authority_policy_refused",
          "selected SystemFamily refuses automatic migration",
        ));
      }
    },
    SystemFamilyPolicyDecisionV1::Ordinary | SystemFamilyPolicyDecisionV1::StructuralContainer => {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_family_binding",
        "detached protected seed resolved outside a known SystemFamily",
      ));
    }
  })
}

fn validate_inventory_closure(
  permit: &MigrationPreflightPermitV1,
  freeze: &MigrationSourceWriteFreezeV1<'_>,
  resolver: SystemFamilyPolicyResolverV1,
  closure: &MigrationFinalAuthorityInventoryClosureV1,
  seed_counts: MigrationFinalAuthoritySeedCountsV1,
  seed_count: u64,
  authority_digest: [u8; 32],
) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
  let root_count = seed_counts.root_count().map_err(MigrationFinalAuthorityReconciliationErrorV1::InventorySource)?;
  if !closure.complete
    || closure.unresolved_family_count != 0
    || closure.database_id != permit.database_id()
    || closure.source_physical_instance_id != permit.source_physical_instance_id()
    || closure.source_physical_identity != freeze.authority().physical_identity
    || closure.source_header_sequence != freeze.authority().header_sequence
    || closure.frozen_source_root != freeze.authority().namespace_root
    || closure.frozen_source_publication_sequence != freeze.authority().hard_publication_frontier
    || closure.seed_counts != seed_counts
    || closure.seed_count != seed_count
    || closure.authority_digest != authority_digest
    || closure.system_family_registry_fingerprint != freeze.authority().system_family_registry_fingerprint
    || closure.system_family_registry_fingerprint != resolver.registry().operational_fingerprint
    || closure.source_authority_counts.protected_families != u64::from(resolver.registry().family_count)
    || closure.source_authority_counts.roots != root_count
    || closure.source_authority_counts.snapshots != seed_counts.snapshots
    || closure.source_authority_counts.forks != seed_counts.forks
    || seed_counts.current_heads != 1
  {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_closure",
      "final authority stream closure is incomplete or differs from the live frozen inventory",
    ));
  }
  Ok(())
}

fn validate_destination_entity(
  request: &MigrationFinalAuthorityReconciliationRequestV1<'_, '_, '_>,
  seed: &MigrationFinalAuthoritySeedV1,
  destination_hash: &[u8],
) -> Result<super::first_authority::LoadedImmutableEntityV1, MigrationFinalAuthorityReconciliationErrorV1> {
  let width = request.permit.hash_algorithm().hash_length();
  if destination_hash.len() != width || destination_hash.iter().all(|byte| *byte == 0) {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_prior_mapping_hash",
      "prior destination mapping is not one nonzero database-width hash",
    ));
  }
  let loaded =
    request.destination.load_immutable_entity_bounded(destination_hash, request.maximum_destination_entity_bytes)?.ok_or_else(|| {
      MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_prior_mapping_missing",
        "prior destination mapping does not resolve to a selected immutable entity",
      )
    })?;
  let expected = EntryTypeV4::from_u8(seed.seed.entry_type.to_u8())?;
  if loaded.entry_type != expected {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_prior_mapping_type",
      "prior destination mapping resolves to the wrong entity type",
    ));
  }
  Ok(loaded)
}

struct InventoryMemoryBudgetV1 {
  _reservation: MemoryReservation,
  maximum: u64,
  used: u64,
  peak: u64,
}

impl InventoryMemoryBudgetV1 {
  fn new(memory: &MemoryCoordinator, maximum: u64) -> Result<Self, MigrationFinalAuthorityReconciliationErrorV1> {
    Ok(Self { _reservation: memory.reserve(MemoryOwner::Migration, maximum, AdmissionClass::Maintenance)?, maximum, used: 0, peak: 0 })
  }

  fn reserve(&mut self, bytes: u64) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
    let next = self.used.checked_add(bytes).ok_or_else(|| {
      MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_memory_overflow",
        "final authority memory accounting overflowed",
      )
    })?;
    if next > self.maximum {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_memory_limit",
        format!("final authority reconciliation requires {next} bytes but its bound is {}", self.maximum),
      ));
    }
    self.used = next;
    self.peak = self.peak.max(next);
    Ok(())
  }

  fn release(&mut self, bytes: u64) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
    self.used = self.used.checked_sub(bytes).ok_or_else(|| {
      MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_memory_underflow",
        "final authority memory accounting underflowed",
      )
    })?;
    Ok(())
  }
}

struct WorkBudgetV1 {
  maximum: u64,
  used: u64,
}

impl WorkBudgetV1 {
  const fn new(maximum: u64) -> Self {
    Self { maximum, used: 0 }
  }

  fn consume(&mut self, item: &'static str) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
    self.used = self.used.checked_add(1).ok_or_else(|| {
      MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_work_overflow",
        "final authority work accounting overflowed",
      )
    })?;
    if self.used > self.maximum {
      return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_work_limit",
        format!("final authority reconciliation exceeded its work bound while processing {item}"),
      ));
    }
    Ok(())
  }
}

fn update_inventory_digest(
  hasher: &mut blake3::Hasher,
  row: &MigrationFinalAuthoritySeedV1,
) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
  hasher.update(&[seed_kind_tag(row.seed.kind)]);
  update_len_bytes(hasher, &row.authority_identity, "authority identity")?;
  hasher.update(&row.source_write_sequence.to_le_bytes());
  update_optional_family_id(hasher, row.system_family_id);
  hasher.update(&row.logical_bytes.to_le_bytes());
  update_len_bytes(hasher, row.seed.path.as_bytes(), "seed path")?;
  hasher.update(&[row.seed.entry_type.to_u8()]);
  let hash_length = u16::try_from(row.seed.hash.len())
    .map_err(|error| MigrationFinalAuthorityReconciliationErrorV1::invalid("migration_final_authority_hash_length", error.to_string()))?;
  hasher.update(&hash_length.to_le_bytes());
  hasher.update(&row.seed.hash);
  Ok(())
}

fn update_mapping_digest(
  hasher: &mut blake3::Hasher,
  mapping: &MigrationFinalRootMappingV1,
) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
  hasher.update(&[seed_kind_tag(mapping.kind)]);
  update_len_bytes(hasher, &mapping.authority_identity, "mapping authority identity")?;
  hasher.update(&mapping.source_write_sequence.to_le_bytes());
  update_len_bytes(hasher, mapping.source_path.as_bytes(), "mapping source path")?;
  hasher.update(&[mapping.source_entry_type.to_u8()]);
  update_len_bytes(hasher, &mapping.source_root, "mapping source root")?;
  update_optional_family_id(hasher, mapping.system_family_id);
  for value in
    [mapping.destination_entity.as_deref(), mapping.destination_namespace_root.as_deref(), mapping.destination_tree_root.as_deref()]
  {
    match value {
      Some(value) => {
        hasher.update(&[1]);
        update_len_bytes(hasher, value, "mapping destination identity")?;
      }
      None => {
        hasher.update(&[0]);
      }
    }
  }
  hasher.update(&[u8::from(mapping.reused)]);
  Ok(())
}

fn update_optional_family_id(hasher: &mut blake3::Hasher, family_id: Option<u16>) {
  match family_id {
    Some(family_id) => {
      hasher.update(&[1]);
      hasher.update(&family_id.to_le_bytes());
    }
    None => {
      hasher.update(&[0]);
    }
  }
}

fn update_authority_counts_digest(hasher: &mut blake3::Hasher, counts: AuthorityInventoryCountsV1) {
  hasher.update(INVENTORY_COUNTS_DOMAIN);
  for value in [
    counts.protected_families,
    counts.modules,
    counts.snapshots,
    counts.forks,
    counts.symlinks,
    counts.history_roots,
    counts.peers,
    counts.sync_states,
    counts.tasks,
    counts.plugins,
    counts.roots,
  ] {
    hasher.update(&value.to_le_bytes());
  }
}

fn update_len_bytes(
  hasher: &mut blake3::Hasher,
  bytes: &[u8],
  name: &'static str,
) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
  let length = u32::try_from(bytes.len()).map_err(|error| {
    MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_component_length",
      format!("{name} exceeds u32: {error}"),
    )
  })?;
  hasher.update(&length.to_le_bytes());
  hasher.update(bytes);
  Ok(())
}

fn seed_kind_tag(kind: MigrationBaseCloneSeedKindV1) -> u8 {
  match kind {
    MigrationBaseCloneSeedKindV1::CurrentHead => 0,
    MigrationBaseCloneSeedKindV1::Snapshot => 1,
    MigrationBaseCloneSeedKindV1::Fork => 2,
    MigrationBaseCloneSeedKindV1::SyncPin => 3,
    MigrationBaseCloneSeedKindV1::Maintenance => 4,
    MigrationBaseCloneSeedKindV1::DetachedProtectedPath => 5,
  }
}

fn is_root_kind(kind: MigrationBaseCloneSeedKindV1) -> bool {
  kind != MigrationBaseCloneSeedKindV1::DetachedProtectedPath
}

fn seed_memory_charge(seed: &MigrationFinalAuthoritySeedV1) -> Result<u64, MigrationFinalAuthorityReconciliationErrorV1> {
  allocation_charge(
    size_of::<MigrationFinalAuthoritySeedV1>(),
    seed.authority_identity.capacity(),
    seed.seed.path.capacity(),
    seed.seed.hash.capacity(),
  )
}

fn order_memory_charge(identity: &[u8]) -> Result<u64, MigrationFinalAuthorityReconciliationErrorV1> {
  allocation_charge(size_of::<(u8, Vec<u8>, u64)>(), identity.len(), 0, 0)
}

fn inventory_closure_memory_charge(
  closure: &MigrationFinalAuthorityInventoryClosureV1,
) -> Result<u64, MigrationFinalAuthorityReconciliationErrorV1> {
  allocation_charge(
    size_of::<MigrationFinalAuthorityInventoryClosureV1>(),
    closure.frozen_source_root.capacity(),
    closure.system_family_registry_fingerprint.capacity(),
    0,
  )
}

fn mapping_memory_charge(mapping: &MigrationFinalRootMappingV1) -> Result<u64, MigrationFinalAuthorityReconciliationErrorV1> {
  let mut bytes = mapping.authority_identity.capacity().checked_add(mapping.source_path.capacity()).ok_or_else(|| {
    MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_memory_overflow",
      "mapping allocation length overflowed",
    )
  })?;
  for capacity in [
    mapping.source_root.capacity(),
    mapping.destination_entity.as_ref().map_or(0, Vec::capacity),
    mapping.destination_namespace_root.as_ref().map_or(0, Vec::capacity),
    mapping.destination_tree_root.as_ref().map_or(0, Vec::capacity),
  ] {
    bytes = bytes.checked_add(capacity).ok_or_else(|| {
      MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_memory_overflow",
        "mapping allocation length overflowed",
      )
    })?;
  }
  allocation_charge(size_of::<MigrationFinalRootMappingV1>(), bytes, 0, 0)
}

fn allocation_charge(base: usize, first: usize, second: usize, third: usize) -> Result<u64, MigrationFinalAuthorityReconciliationErrorV1> {
  let bytes = base
    .checked_add(first)
    .and_then(|value| value.checked_add(second))
    .and_then(|value| value.checked_add(third))
    .and_then(|value| value.checked_add(OWNED_ALLOCATION_OVERHEAD as usize))
    .ok_or_else(|| {
      MigrationFinalAuthorityReconciliationErrorV1::invalid(
        "migration_final_authority_memory_overflow",
        "final authority allocation charge overflowed",
      )
    })?;
  u64::try_from(bytes)
    .map_err(|error| MigrationFinalAuthorityReconciliationErrorV1::invalid("migration_final_authority_memory_overflow", error.to_string()))
}

fn checked_add(value: u64, increment: u64, name: &'static str) -> Result<u64, MigrationFinalAuthorityReconciliationErrorV1> {
  value.checked_add(increment).ok_or_else(|| {
    MigrationFinalAuthorityReconciliationErrorV1::invalid("migration_final_authority_count_overflow", format!("{name} overflowed"))
  })
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), MigrationFinalAuthorityReconciliationErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MigrationFinalAuthorityReconciliationErrorV1::invalid(
      "migration_final_authority_cancelled",
      "final authority reconciliation was cancelled",
    ));
  }
  Ok(())
}
