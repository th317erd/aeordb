//! Bounded, disconnected execution of a preflight-admitted v3 base clone.
//!
//! Retained roots are supplied as a stream and source entries are read through
//! short-lived verified lookups. This owner publishes immutable content only;
//! root mapping, capture replay, final freeze, and service activation remain
//! separate migration phases.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::mem::size_of;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::first_authority::{
  ImmutableEntityBatchPublicationErrorV1, ImmutableEntityBatchPublicationRequestV1, ImmutableEntityWriteV1, V4FirstAuthorityPublisher,
};
use super::hash::IncrementalDigestV1;
use super::migration_clone::{
  MigrationBaseCloneErrorV1, MigrationBaseCloneItemV1, MigrationBaseClonePlanSummaryV1, MigrationBaseClonePlannerV1,
  MigrationBaseCloneSourceClosureV1, MigrationCloneDecisionV1,
};
use super::migration_preflight::{AuthorityInventoryCountsV1, MigrationPreflightPermitV1};
use super::system_family::SystemFamilySubjectV1;
use crate::engine::btree::{BTREE_CONVERSION_THRESHOLD, BTREE_MAX_INTERNAL_KEYS, BTREE_MAX_LEAF_ENTRIES, BTreeNode};
use crate::engine::compression::{CompressionAlgorithm, decompress_bounded};
use crate::engine::directory_entry::{ChildEntry, visit_bounded_child_entries};
use crate::engine::entry_header::{EntryHeader, FLAG_SYSTEM};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::PlatformFileIdentityDescriptorV1;
use crate::engine::path_utils::normalize_path;
use crate::engine::storage_engine::EntryData;
use crate::engine::symlink_record::SymlinkRecord;
use crate::engine::v4::entity::{EntryTypeV4, checked_whole_entity_encoded_length};
use crate::engine::HashAlgorithm;

const MAX_BATCH_ENTITIES: usize = 511;
const MAX_BATCH_ENCODED_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIRECTORY_DEPTH: usize = 1_000;
const MAX_BTREE_DEPTH: usize = 128;
const MAX_SEED_PATH_BYTES: usize = u16::MAX as usize;
const OWNED_ALLOCATION_OVERHEAD: u64 = 128;
const CANCELLATION_QUANTUM: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MigrationBaseCloneSeedKindV1 {
  CurrentHead,
  Snapshot,
  Fork,
  SyncPin,
  Maintenance,
  DetachedProtectedPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationBaseCloneSeedV1 {
  pub kind: MigrationBaseCloneSeedKindV1,
  pub path: String,
  pub entry_type: EntryType,
  pub hash: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationBaseCloneStreamClosureV1 {
  pub database_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub source_header_sequence: u64,
  pub source_capture_head: Vec<u8>,
  pub source_authority_digest: [u8; 32],
  pub source_authority_counts: AuthorityInventoryCountsV1,
}

/// One bounded caller-owned stream of the exact preflight root inventory.
pub trait MigrationBaseCloneSeedSourceV1 {
  fn next_seed(&mut self) -> EngineResult<Option<MigrationBaseCloneSeedV1>>;
  fn finish(&mut self) -> EngineResult<MigrationBaseCloneStreamClosureV1>;
}

/// Caller-owned streaming handoff from each legacy seed to its rebuilt v4 root.
pub trait MigrationBaseCloneSeedResultSinkV1 {
  fn record_seed_result(&mut self, seed: &MigrationBaseCloneSeedV1, destination_hash: Option<&[u8]>) -> EngineResult<()>;
}

/// Short-lived exact historical reads used by the clone executor.
pub trait MigrationBaseCloneEntrySourceV1 {
  fn hash_algorithm(&self) -> HashAlgorithm;
  fn physical_identity(&self) -> EngineResult<PlatformFileIdentityDescriptorV1>;
  fn historical_entry_header(&self, hash: &[u8]) -> EngineResult<Option<EntryHeader>>;
  fn historical_entry_verified_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<EntryData>>;
}

pub struct MigrationBaseCloneExecutionRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub source: &'a dyn MigrationBaseCloneEntrySourceV1,
  pub seeds: &'a mut dyn MigrationBaseCloneSeedSourceV1,
  pub seed_results: &'a mut dyn MigrationBaseCloneSeedResultSinkV1,
  pub destination: &'a V4FirstAuthorityPublisher,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub publication_timestamp_ms: u64,
  pub maximum_work_items: u64,
  pub maximum_memory_bytes: u64,
  pub maximum_decoded_chunk_bytes: usize,
  pub maximum_directory_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationBaseCloneExecutionReceiptV1 {
  pub plan: MigrationBaseClonePlanSummaryV1,
  pub processed_seeds: u64,
  pub loaded_entities: u64,
  pub published_entities: u64,
  pub idempotent_entities: u64,
  pub duplicate_batch_entities: u64,
  pub copied_chunk_bytes: u64,
  pub maximum_batch_entities: u16,
  pub maximum_batch_encoded_bytes: u64,
  pub maximum_frontier_items: u64,
  pub maximum_directory_depth: u16,
  pub maximum_btree_depth: u16,
  pub peak_accounted_memory_bytes: u64,
  pub destination_header_sequence: u64,
  pub destination_write_sequence: u64,
  pub destination_head_tree: Vec<u8>,
}

pub(crate) struct MigrationSubtreeCloneRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub source: &'a dyn MigrationBaseCloneEntrySourceV1,
  pub destination: &'a V4FirstAuthorityPublisher,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub publication_timestamp_ms: u64,
  pub maximum_work_items: u64,
  pub maximum_memory_bytes: u64,
  pub maximum_decoded_chunk_bytes: usize,
  pub maximum_directory_depth: usize,
  pub path: &'a str,
  pub hash: &'a [u8],
  pub entry_type: EntryType,
  pub logical_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MigrationTranslatedSubtreeV1 {
  pub hash: Vec<u8>,
  pub total_size: u64,
  pub content_type: Option<String>,
  pub created_at: Option<i64>,
  pub updated_at: Option<i64>,
}

#[derive(Debug)]
pub enum MigrationBaseCloneExecutionErrorV1 {
  Invalid { code: &'static str, message: String },
  Source(EngineError),
  SeedResult(EngineError),
  Planner(MigrationBaseCloneErrorV1),
  Publication(ImmutableEntityBatchPublicationErrorV1),
  Memory(MemoryCoordinatorError),
}

impl MigrationBaseCloneExecutionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Source(_) => "migration_clone_source_error",
      Self::SeedResult(_) => "migration_clone_seed_result_error",
      Self::Planner(source) => source.code(),
      Self::Publication(source) => source.code(),
      Self::Memory(_) => "migration_clone_memory_admission",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationBaseCloneExecutionErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Source(source) => Display::fmt(source, formatter),
      Self::SeedResult(source) => Display::fmt(source, formatter),
      Self::Planner(source) => Display::fmt(source, formatter),
      Self::Publication(source) => Display::fmt(source, formatter),
      Self::Memory(source) => Display::fmt(source, formatter),
    }
  }
}

impl Error for MigrationBaseCloneExecutionErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Source(source) => Some(source),
      Self::SeedResult(source) => Some(source),
      Self::Planner(source) => Some(source),
      Self::Publication(source) => Some(source),
      Self::Memory(source) => Some(source),
      Self::Invalid { .. } => None,
    }
  }
}

impl From<EngineError> for MigrationBaseCloneExecutionErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Source(source)
  }
}

impl From<MigrationBaseCloneErrorV1> for MigrationBaseCloneExecutionErrorV1 {
  fn from(source: MigrationBaseCloneErrorV1) -> Self {
    Self::Planner(source)
  }
}

impl From<ImmutableEntityBatchPublicationErrorV1> for MigrationBaseCloneExecutionErrorV1 {
  fn from(source: ImmutableEntityBatchPublicationErrorV1) -> Self {
    Self::Publication(source)
  }
}

impl From<MemoryCoordinatorError> for MigrationBaseCloneExecutionErrorV1 {
  fn from(source: MemoryCoordinatorError) -> Self {
    Self::Memory(source)
  }
}

struct MigrationMemoryBudgetV1 {
  reservation: MemoryReservation,
  maximum_bytes: u64,
  peak_bytes: u64,
}

impl MigrationMemoryBudgetV1 {
  fn new(memory: &MemoryCoordinator, maximum_bytes: u64) -> Result<Self, MigrationBaseCloneExecutionErrorV1> {
    if maximum_bytes == 0 {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_limit", "memory limit must be nonzero"));
    }
    let reservation = memory.reserve(MemoryOwner::Migration, 1, AdmissionClass::Maintenance)?;
    Ok(Self { reservation, maximum_bytes, peak_bytes: 1 })
  }

  fn reserve(&mut self, bytes: u64) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let projected = self
      .reservation
      .bytes()
      .checked_add(bytes)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "memory accounting overflow"))?;
    if projected > self.maximum_bytes {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_memory_limit",
        format!("memory requirement {projected} exceeds limit {}", self.maximum_bytes),
      ));
    }
    self.reservation.grow(bytes)?;
    self.peak_bytes = self.peak_bytes.max(projected);
    Ok(())
  }

  fn release(&mut self, bytes: u64) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    self.reservation.shrink(bytes)?;
    Ok(())
  }
}

struct LoadedSourceEntityV1 {
  header: EntryHeader,
  key: Vec<u8>,
  value: Vec<u8>,
  memory_charge: u64,
}

struct PendingEntityV1 {
  entity_version: u8,
  entry_type: EntryTypeV4,
  key: Vec<u8>,
  value: Vec<u8>,
  memory_charge: u64,
}

struct DestinationBatchV1<'a> {
  destination: &'a V4FirstAuthorityPublisher,
  database_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
  next_publication_timestamp_ms: u64,
  entities: Vec<PendingEntityV1>,
  encoded_bytes: usize,
  published_entities: u64,
  idempotent_entities: u64,
  duplicate_entities: u64,
  maximum_entities: usize,
  maximum_encoded_bytes: usize,
  destination_header_sequence: u64,
  destination_write_sequence: u64,
}

impl<'a> DestinationBatchV1<'a> {
  fn new(
    destination: &'a V4FirstAuthorityPublisher,
    database_id: [u8; 16],
    publication_timestamp_ms: u64,
  ) -> Result<Self, MigrationBaseCloneExecutionErrorV1> {
    let observation = destination.observe().map_err(ImmutableEntityBatchPublicationErrorV1::from)?;
    let selected = &observation.selected.header;
    if selected.database_id != database_id {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_destination_database",
        "destination database identity differs from preflight",
      ));
    }
    let selected_successor_time = selected.updated_at_ms.checked_add(1).ok_or_else(|| {
      MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_publication_time", "destination publication timestamp exhausted")
    })?;
    let next_publication_timestamp_ms = publication_timestamp_ms.max(selected_successor_time);
    if next_publication_timestamp_ms == 0 || next_publication_timestamp_ms > i64::MAX as u64 {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_publication_time",
        "publication timestamp is outside the signed millisecond range",
      ));
    }
    Ok(Self {
      destination,
      database_id,
      hash_algorithm: selected.hash_algorithm,
      next_publication_timestamp_ms,
      entities: Vec::new(),
      encoded_bytes: 0,
      published_entities: 0,
      idempotent_entities: 0,
      duplicate_entities: 0,
      maximum_entities: 0,
      maximum_encoded_bytes: 0,
      destination_header_sequence: selected.slot_sequence,
      destination_write_sequence: selected.write_sequence_high_water,
    })
  }

  fn add(
    &mut self,
    entity_version: u8,
    entry_type: EntryTypeV4,
    key: Vec<u8>,
    value: Vec<u8>,
    budget: &mut MigrationMemoryBudgetV1,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    if let Some(existing) = self.entities.iter().find(|existing| existing.key == key) {
      if existing.entity_version != entity_version || existing.entry_type != entry_type || existing.value != value {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_batch_identity_collision",
          format!("batch identity {} has conflicting bytes", hex::encode(key)),
        ));
      }
      self.duplicate_entities = self
        .duplicate_entities
        .checked_add(1)
        .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "duplicate count overflow"))?;
      return Ok(());
    }
    let encoded_length = checked_whole_entity_encoded_length(self.hash_algorithm, key.len(), value.len())
      .map_err(|source| MigrationBaseCloneExecutionErrorV1::invalid(source.code(), source.to_string()))?;
    if encoded_length > MAX_BATCH_ENCODED_BYTES {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_entity_too_large",
        format!("entity {} encodes to {encoded_length} bytes", hex::encode(&key)),
      ));
    }
    if !self.entities.is_empty()
      && (self.entities.len() == MAX_BATCH_ENTITIES
        || self.encoded_bytes.checked_add(encoded_length).is_none_or(|bytes| bytes > MAX_BATCH_ENCODED_BYTES))
    {
      self.flush(budget)?;
    }
    let memory_charge = allocation_charge(key.len(), value.len(), encoded_length)?;
    budget.reserve(memory_charge)?;
    self.encoded_bytes = self
      .encoded_bytes
      .checked_add(encoded_length)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_batch_overflow", "batch byte count overflow"))?;
    self.entities.push(PendingEntityV1 { entity_version, entry_type, key, value, memory_charge });
    self.maximum_entities = self.maximum_entities.max(self.entities.len());
    self.maximum_encoded_bytes = self.maximum_encoded_bytes.max(self.encoded_bytes);
    Ok(())
  }

  fn flush(&mut self, budget: &mut MigrationMemoryBudgetV1) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    if self.entities.is_empty() {
      return Ok(());
    }
    let writes_bytes = self
      .entities
      .len()
      .checked_mul(size_of::<ImmutableEntityWriteV1<'_>>())
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "batch descriptor size overflow"))?;
    let writes_charge = usize_to_u64(writes_bytes, "batch descriptor size")?;
    budget.reserve(writes_charge)?;
    let mut writes = Vec::new();
    if let Err(error) = writes.try_reserve_exact(self.entities.len()) {
      budget.release(writes_charge)?;
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_memory_allocation",
        format!("batch descriptor allocation failed: {error}"),
      ));
    }
    for entity in &self.entities {
      writes.push(ImmutableEntityWriteV1 {
        entity_version: entity.entity_version,
        entry_type: entity.entry_type,
        flags: 0,
        key: entity.key.as_slice(),
        stored_value: entity.value.as_slice(),
      });
    }
    let publication = self.destination.publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &self.database_id,
      entities: &writes,
      publication_timestamp_ms: self.next_publication_timestamp_ms,
    });
    budget.release(writes_charge)?;
    let receipt = match publication {
      Ok(receipt) => receipt,
      Err(error) => match error.committed_receipt() {
        Some(receipt) => receipt.clone(),
        None => return Err(MigrationBaseCloneExecutionErrorV1::Publication(error)),
      },
    };
    self.published_entities = self
      .published_entities
      .checked_add(usize_to_u64(receipt.entities.len(), "published entity count")?)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "published count overflow"))?;
    self.idempotent_entities = self
      .idempotent_entities
      .checked_add(usize_to_u64(receipt.entities.iter().filter(|entity| entity.idempotent).count(), "idempotent entity count")?)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "idempotent count overflow"))?;
    self.destination_header_sequence = receipt.observation.selected.header.slot_sequence;
    self.destination_write_sequence = receipt.observation.selected.header.write_sequence_high_water;
    self.next_publication_timestamp_ms =
      receipt.observation.selected.header.updated_at_ms.checked_add(1).ok_or_else(|| {
        MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_publication_time", "publication timestamp exhausted")
      })?;
    let released = self
      .entities
      .iter()
      .try_fold(0u64, |total, entity| total.checked_add(entity.memory_charge))
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "batch release overflow"))?;
    self.entities.clear();
    self.encoded_bytes = 0;
    budget.release(released)?;
    Ok(())
  }
}

enum CloneWorkV1 {
  Entry { path: Arc<str>, hash: Vec<u8>, entry_type: EntryType, logical_bytes: u64, directory_depth: usize },
  FlatDirectoryFinalize { path: Arc<str>, loaded: LoadedSourceEntityV1, children: Vec<ChildEntry>, retain_empty: bool, parse_charge: u64 },
  BtreeDirectoryFinalize { path: Arc<str>, retain_empty: bool, entry_version: u8 },
  BtreeLeafFinalize { entries: Vec<ChildEntry>, entry_version: u8, parse_charge: u64 },
  BtreeInternalFinalize { child_count: usize, entry_version: u8, parse_charge: u64 },
  DirectoryExit { hash: Vec<u8>, memory_charge: u64 },
  BtreeNode { path: Arc<str>, hash: Vec<u8>, depth: usize, lower_bound: Option<Arc<str>>, upper_bound: Option<Arc<str>> },
  BtreeExit { hash: Vec<u8>, memory_charge: u64 },
}

struct ExecutorV1<'a> {
  request: MigrationBaseCloneExecutionRequestV1<'a>,
  planner: Option<MigrationBaseClonePlannerV1<'a>>,
  budget: MigrationMemoryBudgetV1,
  batch: DestinationBatchV1<'a>,
  work: Vec<(CloneWorkV1, u64)>,
  active_directories: Vec<Vec<u8>>,
  active_btree_nodes: Vec<Vec<u8>>,
  entry_results: Vec<Option<TranslatedEntryV1>>,
  btree_results: Vec<Option<TranslatedBtreeNodeV1>>,
  pending_seed_results: Vec<PendingSeedResultV1>,
  work_items: u64,
  processed_seeds: u64,
  loaded_entities: u64,
  copied_chunk_bytes: u64,
  maximum_frontier_items: usize,
  maximum_directory_depth: usize,
  maximum_btree_depth: usize,
  work_since_cancellation: u64,
  saw_head: bool,
  destination_head_tree: Option<(Vec<u8>, u64)>,
}

struct TranslatedBtreeNodeV1 {
  hash: Vec<u8>,
  first_name: String,
  total_size: u64,
  memory_charge: u64,
}

struct TranslatedEntryV1 {
  hash: Vec<u8>,
  total_size: u64,
  content_type: Option<String>,
  created_at: Option<i64>,
  updated_at: Option<i64>,
  memory_charge: u64,
}

struct BtreeRangeV1 {
  depth: usize,
  lower_bound: Option<Arc<str>>,
  upper_bound: Option<Arc<str>>,
}

struct PendingSeedResultV1 {
  seed: MigrationBaseCloneSeedV1,
  destination: Option<TranslatedEntryV1>,
  memory_charge: u64,
}

pub fn execute_migration_base_clone_v1(
  request: MigrationBaseCloneExecutionRequestV1<'_>,
) -> Result<MigrationBaseCloneExecutionReceiptV1, MigrationBaseCloneExecutionErrorV1> {
  validate_request(&request)?;
  let planner = MigrationBaseClonePlannerV1::new(request.permit, request.cancellation, request.maximum_work_items)?;
  let budget = MigrationMemoryBudgetV1::new(request.memory, request.maximum_memory_bytes)?;
  let batch = DestinationBatchV1::new(request.destination, request.permit.database_id(), request.publication_timestamp_ms)?;
  let executor = ExecutorV1 {
    request,
    planner: Some(planner),
    budget,
    batch,
    work: Vec::new(),
    active_directories: Vec::new(),
    active_btree_nodes: Vec::new(),
    entry_results: Vec::new(),
    btree_results: Vec::new(),
    pending_seed_results: Vec::new(),
    work_items: 0,
    processed_seeds: 0,
    loaded_entities: 0,
    copied_chunk_bytes: 0,
    maximum_frontier_items: 0,
    maximum_directory_depth: 0,
    maximum_btree_depth: 0,
    work_since_cancellation: 0,
    saw_head: false,
    destination_head_tree: None,
  };
  executor.execute()
}

/// Reuse the bounded base-clone traversal for one authoritative after-subtree.
///
/// Capture replay owns root ordering and ancestor copy-on-write. This adapter
/// owns only source validation, SystemFamily classification, immutable subtree
/// translation, and bounded destination publication.
pub(crate) fn translate_migration_subtree_v1(
  request: MigrationSubtreeCloneRequestV1<'_>,
) -> Result<Option<MigrationTranslatedSubtreeV1>, MigrationBaseCloneExecutionErrorV1> {
  struct EmptySeedSource;
  impl MigrationBaseCloneSeedSourceV1 for EmptySeedSource {
    fn next_seed(&mut self) -> EngineResult<Option<MigrationBaseCloneSeedV1>> {
      Ok(None)
    }

    fn finish(&mut self) -> EngineResult<MigrationBaseCloneStreamClosureV1> {
      Err(EngineError::InvalidInput("subtree translator has no seed-stream closure".to_string()))
    }
  }
  struct EmptySeedSink;
  impl MigrationBaseCloneSeedResultSinkV1 for EmptySeedSink {
    fn record_seed_result(&mut self, _seed: &MigrationBaseCloneSeedV1, _destination_hash: Option<&[u8]>) -> EngineResult<()> {
      Err(EngineError::InvalidInput("subtree translator has no seed-result handoff".to_string()))
    }
  }

  let mut seeds = EmptySeedSource;
  let mut seed_results = EmptySeedSink;
  let base_request = MigrationBaseCloneExecutionRequestV1 {
    permit: request.permit,
    source: request.source,
    seeds: &mut seeds,
    seed_results: &mut seed_results,
    destination: request.destination,
    memory: request.memory,
    cancellation: request.cancellation,
    publication_timestamp_ms: request.publication_timestamp_ms,
    maximum_work_items: request.maximum_work_items,
    maximum_memory_bytes: request.maximum_memory_bytes,
    maximum_decoded_chunk_bytes: request.maximum_decoded_chunk_bytes,
    maximum_directory_depth: request.maximum_directory_depth,
  };
  validate_request(&base_request)?;
  validate_path(request.path)?;
  if request.hash.len() != request.permit.hash_algorithm().hash_length() || request.hash.iter().all(|byte| *byte == 0) {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_subtree_hash",
      "subtree identity must be one nonzero database-width hash",
    ));
  }
  let planner = MigrationBaseClonePlannerV1::new(request.permit, request.cancellation, request.maximum_work_items)?;
  let budget = MigrationMemoryBudgetV1::new(request.memory, request.maximum_memory_bytes)?;
  let batch = DestinationBatchV1::new(request.destination, request.permit.database_id(), request.publication_timestamp_ms)?;
  let mut executor = ExecutorV1 {
    request: base_request,
    planner: Some(planner),
    budget,
    batch,
    work: Vec::new(),
    active_directories: Vec::new(),
    active_btree_nodes: Vec::new(),
    entry_results: Vec::new(),
    btree_results: Vec::new(),
    pending_seed_results: Vec::new(),
    work_items: 0,
    processed_seeds: 0,
    loaded_entities: 0,
    copied_chunk_bytes: 0,
    maximum_frontier_items: 0,
    maximum_directory_depth: 0,
    maximum_btree_depth: 0,
    work_since_cancellation: 0,
    saw_head: true,
    destination_head_tree: None,
  };
  executor.push_work(CloneWorkV1::Entry {
    path: Arc::from(request.path),
    hash: request.hash.to_vec(),
    entry_type: request.entry_type,
    logical_bytes: request.logical_bytes,
    directory_depth: request.path.split('/').filter(|segment| !segment.is_empty()).count(),
  })?;
  executor.drain_work()?;
  let translated = match executor.entry_results.pop() {
    Some(translated) if executor.entry_results.is_empty() && executor.btree_results.is_empty() => translated,
    _ => {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_result_unbalanced",
        "subtree traversal did not produce exactly one destination result",
      ));
    }
  };
  executor.batch.flush(&mut executor.budget)?;
  translated
    .map(|translated| {
      executor.budget.release(translated.memory_charge)?;
      Ok(MigrationTranslatedSubtreeV1 {
        hash: translated.hash,
        total_size: translated.total_size,
        content_type: translated.content_type,
        created_at: translated.created_at,
        updated_at: translated.updated_at,
      })
    })
    .transpose()
}

impl ExecutorV1<'_> {
  fn execute(mut self) -> Result<MigrationBaseCloneExecutionReceiptV1, MigrationBaseCloneExecutionErrorV1> {
    while let Some(seed) = self.request.seeds.next_seed()? {
      self.check_work()?;
      self.validate_seed(&seed)?;
      if !self.entry_results.is_empty() || !self.btree_results.is_empty() {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_result_unbalanced",
          "a seed began with unconsumed traversal results",
        ));
      }
      self.processed_seeds = self
        .processed_seeds
        .checked_add(1)
        .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "seed count overflow"))?;
      self.push_work(CloneWorkV1::Entry {
        path: Arc::from(seed.path.as_str()),
        hash: seed.hash.clone(),
        entry_type: seed.entry_type,
        logical_bytes: 0,
        directory_depth: 0,
      })?;
      self.drain_work()?;
      let destination_hash = match self.entry_results.pop() {
        Some(destination_hash) if self.entry_results.is_empty() && self.btree_results.is_empty() => destination_hash,
        _ => {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_result_unbalanced",
            "seed traversal did not produce exactly one destination result",
          ));
        }
      };
      if seed.kind == MigrationBaseCloneSeedKindV1::CurrentHead {
        let head = destination_hash.as_ref().ok_or_else(|| {
          MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_head_omitted", "current HEAD cannot be omitted from the destination")
        })?;
        let head_hash = head.hash.clone();
        let head_charge = allocation_charge(head_hash.len(), 0, 0)?;
        self.budget.reserve(head_charge)?;
        self.destination_head_tree = Some((head_hash, head_charge));
      }
      self.queue_seed_result(seed, destination_hash)?;
      if self.pending_seed_results.len() == MAX_BATCH_ENTITIES {
        self.flush_pending_seed_results()?;
      }
    }
    if !self.saw_head {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_head_missing", "seed stream omitted current HEAD"));
    }
    let closure = self.request.seeds.finish()?;
    let planner = self.planner.take().ok_or_else(|| {
      MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_planner_state", "base clone planner was already finalized")
    })?;
    let plan = planner.finish(MigrationBaseCloneSourceClosureV1 {
      database_id: closure.database_id,
      source_physical_instance_id: closure.source_physical_instance_id,
      source_header_sequence: closure.source_header_sequence,
      source_capture_head: &closure.source_capture_head,
      source_authority_digest: closure.source_authority_digest,
      source_authority_counts: closure.source_authority_counts,
    })?;
    self.flush_pending_seed_results()?;
    let observation = self.request.destination.observe().map_err(ImmutableEntityBatchPublicationErrorV1::from)?;
    let maximum_batch_entities = usize_to_u16(self.batch.maximum_entities, "maximum batch entity count")?;
    let maximum_batch_encoded_bytes = usize_to_u64(self.batch.maximum_encoded_bytes, "maximum batch byte count")?;
    let maximum_frontier_items = usize_to_u64(self.maximum_frontier_items, "maximum frontier item count")?;
    let maximum_directory_depth = usize_to_u16(self.maximum_directory_depth, "maximum directory depth")?;
    let maximum_btree_depth = usize_to_u16(self.maximum_btree_depth, "maximum B-tree depth")?;
    let (destination_head_tree, destination_head_charge) = self.destination_head_tree.take().ok_or_else(|| {
      MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_head_missing", "base clone completed without a translated HEAD")
    })?;
    self.budget.release(destination_head_charge)?;
    Ok(MigrationBaseCloneExecutionReceiptV1 {
      plan,
      processed_seeds: self.processed_seeds,
      loaded_entities: self.loaded_entities,
      published_entities: self.batch.published_entities,
      idempotent_entities: self.batch.idempotent_entities,
      duplicate_batch_entities: self.batch.duplicate_entities,
      copied_chunk_bytes: self.copied_chunk_bytes,
      maximum_batch_entities,
      maximum_batch_encoded_bytes,
      maximum_frontier_items,
      maximum_directory_depth,
      maximum_btree_depth,
      peak_accounted_memory_bytes: self.budget.peak_bytes,
      destination_header_sequence: observation.selected.header.slot_sequence,
      destination_write_sequence: observation.selected.header.write_sequence_high_water,
      destination_head_tree,
    })
  }

  fn validate_seed(&mut self, seed: &MigrationBaseCloneSeedV1) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    validate_path(&seed.path)?;
    if seed.hash.len() != self.request.permit.hash_algorithm().hash_length() {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_seed_hash_width", "seed hash width differs from database"));
    }
    match seed.kind {
      MigrationBaseCloneSeedKindV1::CurrentHead => {
        if self.saw_head {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_head_duplicate",
            "seed stream contains multiple HEAD rows",
          ));
        }
        if self.processed_seeds != 0
          || seed.path != "/"
          || seed.entry_type != EntryType::DirectoryIndex
          || seed.hash != self.request.permit.source_capture_head()
        {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_head_seed",
            "the first seed must be the exact preflight HEAD directory root",
          ));
        }
        self.saw_head = true;
      }
      MigrationBaseCloneSeedKindV1::Snapshot
      | MigrationBaseCloneSeedKindV1::Fork
      | MigrationBaseCloneSeedKindV1::SyncPin
      | MigrationBaseCloneSeedKindV1::Maintenance => {
        if !self.saw_head || seed.path != "/" || seed.entry_type != EntryType::DirectoryIndex {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_retained_root_seed",
            "retained roots must follow HEAD and identify a root DirectoryIndex",
          ));
        }
      }
      MigrationBaseCloneSeedKindV1::DetachedProtectedPath => {
        if !self.saw_head || seed.path == "/" {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_detached_seed",
            "detached protected seeds must follow HEAD and use a non-root absolute path",
          ));
        }
      }
    }
    Ok(())
  }

  fn drain_work(&mut self) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    while let Some((work, work_charge)) = self.work.pop() {
      self.check_work()?;
      let result = match work {
        CloneWorkV1::Entry { path, hash, entry_type, logical_bytes, directory_depth } => {
          self.process_path_entry(path, hash, entry_type, logical_bytes, directory_depth)
        }
        CloneWorkV1::FlatDirectoryFinalize { path, loaded, children, retain_empty, parse_charge } => {
          self.finalize_flat_directory(path, loaded, children, retain_empty, parse_charge)
        }
        CloneWorkV1::BtreeDirectoryFinalize { path, retain_empty, entry_version } => {
          self.finalize_btree_directory(path, retain_empty, entry_version)
        }
        CloneWorkV1::BtreeLeafFinalize { entries, entry_version, parse_charge } => {
          self.finalize_btree_leaf(entries, entry_version, parse_charge)
        }
        CloneWorkV1::BtreeInternalFinalize { child_count, entry_version, parse_charge } => {
          self.finalize_btree_internal(child_count, entry_version, parse_charge)
        }
        CloneWorkV1::DirectoryExit { hash, memory_charge } => self.exit_directory(hash, memory_charge),
        CloneWorkV1::BtreeNode { path, hash, depth, lower_bound, upper_bound } => {
          self.process_btree_hash(path, hash, depth, lower_bound, upper_bound)
        }
        CloneWorkV1::BtreeExit { hash, memory_charge } => self.exit_btree(hash, memory_charge),
      };
      self.budget.release(work_charge)?;
      result?;
    }
    if !self.active_directories.is_empty() || !self.active_btree_nodes.is_empty() {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_frontier_unbalanced",
        "clone traversal completed with active ancestry",
      ));
    }
    Ok(())
  }

  fn process_path_entry(
    &mut self,
    path: Arc<str>,
    hash: Vec<u8>,
    entry_type: EntryType,
    logical_bytes: u64,
    directory_depth: usize,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let planner = self.planner.as_mut().ok_or_else(|| {
      MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_planner_state", "base clone planner is unavailable during traversal")
    })?;
    let decision = planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path(path.as_ref()), logical_bytes })?;
    let copy = matches!(decision, MigrationCloneDecisionV1::CopyOrdinary | MigrationCloneDecisionV1::CopyKnown { .. });
    let traverse = copy || decision == MigrationCloneDecisionV1::TraverseStructuralContainer;
    match entry_type {
      EntryType::DirectoryIndex if traverse => self.process_directory(path, hash, copy, directory_depth),
      EntryType::DirectoryIndex => {
        self.entry_results.push(None);
        Ok(())
      }
      EntryType::FileRecord if copy => self.process_file(path, hash),
      EntryType::Symlink if copy => self.process_symlink(path, hash),
      EntryType::FileRecord | EntryType::Symlink => {
        if decision == MigrationCloneDecisionV1::TraverseStructuralContainer {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_structural_leaf",
            format!("structural path {path} resolved to {entry_type:?}"),
          ));
        }
        self.entry_results.push(None);
        Ok(())
      }
      other => Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_namespace_entry_type",
        format!("namespace path {path} references unsupported {other:?}"),
      )),
    }
  }

  fn process_directory(
    &mut self,
    path: Arc<str>,
    hash: Vec<u8>,
    retain_empty: bool,
    depth: usize,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    if depth > self.request.maximum_directory_depth {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_directory_depth",
        format!("directory depth {depth} exceeds configured bound"),
      ));
    }
    if self.active_directories.iter().any(|ancestor| ancestor == &hash) {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_directory_cycle",
        format!("directory cycle at {path} ({})", hex::encode(&hash)),
      ));
    }
    let ancestry_charge = allocation_charge(hash.len(), 0, 0)?;
    self.budget.reserve(ancestry_charge)?;
    self.active_directories.push(hash.clone());
    self.maximum_directory_depth = self.maximum_directory_depth.max(depth);
    self.push_work_unaccounted(CloneWorkV1::DirectoryExit { hash: hash.clone(), memory_charge: ancestry_charge })?;
    let loaded = self.load_entity(&hash, EntryType::DirectoryIndex, MAX_BATCH_ENCODED_BYTES as u32)?;
    validate_legacy_path_flags(path.as_ref(), loaded.header.flags, true)?;
    if loaded.header.entry_version != 0 {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_directory_version",
        format!("raw directory {path} uses unsupported entity version {}", loaded.header.entry_version),
      ));
    }
    if loaded.header.compression_algo != CompressionAlgorithm::None {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_directory_compression",
        format!("directory {path} uses unsupported compression"),
      ));
    }
    let is_btree = crate::engine::btree::is_btree_format(&loaded.value);
    canonical_directory_key(self.request.permit.hash_algorithm(), path.as_ref(), &loaded.key, &loaded.value, true)?;
    if is_btree {
      let entry_version = loaded.header.entry_version;
      self.push_work_unaccounted(CloneWorkV1::BtreeDirectoryFinalize { path: path.clone(), retain_empty, entry_version })?;
      self.process_loaded_btree_node(
        path,
        hash,
        entry_version,
        &loaded.value,
        BtreeRangeV1 { depth: 0, lower_bound: None, upper_bound: None },
      )?;
      self.release_loaded(loaded)?;
    } else {
      let parse_bytes = usize_to_u64(loaded.value.len(), "flat directory parse size")?;
      let parse_charge = parse_bytes.checked_mul(4).and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD)).ok_or_else(|| {
        MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "flat directory parse estimate overflow")
      })?;
      self.budget.reserve(parse_charge)?;
      let mut children = Vec::new();
      visit_bounded_child_entries(
        &loaded.value,
        self.request.permit.hash_algorithm().hash_length(),
        loaded.header.entry_version,
        BTREE_CONVERSION_THRESHOLD,
        |child| {
          children.push(child);
          Ok(true)
        },
      )?;
      if children.iter().enumerate().any(|(index, child)| children[index + 1..].iter().any(|candidate| candidate.name == child.name)) {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_flat_directory_duplicate",
          format!("flat directory {path} contains a duplicate child name"),
        ));
      }
      self.push_work_unaccounted(CloneWorkV1::FlatDirectoryFinalize {
        path: path.clone(),
        loaded,
        children: children.clone(),
        retain_empty,
        parse_charge,
      })?;
      for child in children.into_iter().rev() {
        self.push_child(path.clone(), child, depth + 1)?;
      }
    }
    Ok(())
  }

  fn finalize_flat_directory(
    &mut self,
    _path: Arc<str>,
    loaded: LoadedSourceEntityV1,
    children: Vec<ChildEntry>,
    retain_empty: bool,
    parse_charge: u64,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let results = self.take_entry_results(children.len())?;
    let mut retained = Vec::with_capacity(children.len());
    let mut result_charge = 0u64;
    for (mut child, result) in children.into_iter().zip(results) {
      if let Some(result) = result {
        result_charge = result_charge.checked_add(result.memory_charge).ok_or_else(|| {
          MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "flat child result charge overflow")
        })?;
        apply_translated_child(&mut child, result);
        retained.push(child);
      }
    }
    retained.sort_by(|left, right| left.name.cmp(&right.name));
    if retained.is_empty() && !retain_empty {
      self.release_loaded(loaded)?;
      self.budget.release(parse_charge)?;
      self.budget.release(result_charge)?;
      self.entry_results.push(None);
      return Ok(());
    }
    let value = crate::engine::directory_entry::serialize_child_entries(&retained, self.request.permit.hash_algorithm().hash_length())?;
    let (destination_hash, total_size) = self.publish_directory_value(loaded.header.entry_version, value)?;
    self.release_loaded(loaded)?;
    self.budget.release(parse_charge)?;
    self.budget.release(result_charge)?;
    self.push_entry_result(destination_hash, total_size, None, None, None)?;
    Ok(())
  }

  fn finalize_btree_directory(
    &mut self,
    path: Arc<str>,
    retain_empty: bool,
    entry_version: u8,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let result = self.btree_results.pop().ok_or_else(|| {
      MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_btree_result", format!("B-tree directory {path} has no root result"))
    })?;
    if let Some(result) = result {
      self.entry_results.push(Some(TranslatedEntryV1 {
        hash: result.hash,
        total_size: result.total_size,
        content_type: None,
        created_at: None,
        updated_at: None,
        memory_charge: result.memory_charge,
      }));
    } else if retain_empty {
      let (destination_hash, total_size) = self.publish_directory_value(entry_version, Vec::new())?;
      self.push_entry_result(destination_hash, total_size, None, None, None)?;
    } else {
      self.entry_results.push(None);
    }
    Ok(())
  }

  fn finalize_btree_leaf(
    &mut self,
    entries: Vec<ChildEntry>,
    entry_version: u8,
    parse_charge: u64,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let results = self.take_entry_results(entries.len())?;
    let mut retained = Vec::with_capacity(entries.len());
    let mut result_charge = 0u64;
    for (mut child, result) in entries.into_iter().zip(results) {
      if let Some(result) = result {
        result_charge = result_charge.checked_add(result.memory_charge).ok_or_else(|| {
          MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "B-tree leaf result charge overflow")
        })?;
        apply_translated_child(&mut child, result);
        retained.push(child);
      }
    }
    if retained.is_empty() {
      self.budget.release(parse_charge)?;
      self.budget.release(result_charge)?;
      self.btree_results.push(None);
      return Ok(());
    }
    let first_name = retained[0].name.clone();
    let value = BTreeNode::Leaf(crate::engine::btree::LeafNode { entries: retained })
      .serialize(self.request.permit.hash_algorithm().hash_length())?;
    let (destination_hash, total_size) = self.publish_directory_value(entry_version, value)?;
    self.budget.release(parse_charge)?;
    self.budget.release(result_charge)?;
    self.push_btree_result(destination_hash, first_name, total_size)
  }

  fn finalize_btree_internal(
    &mut self,
    child_count: usize,
    entry_version: u8,
    parse_charge: u64,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let results = self.take_btree_results(child_count)?;
    let mut retained = results.into_iter().flatten().collect::<Vec<_>>();
    if retained.is_empty() {
      self.budget.release(parse_charge)?;
      self.btree_results.push(None);
      return Ok(());
    }
    if retained.len() == 1 {
      self.budget.release(parse_charge)?;
      self.btree_results.push(Some(retained.remove(0)));
      return Ok(());
    }
    let retained_charge = retained.iter().try_fold(0u64, |total, child| total.checked_add(child.memory_charge)).ok_or_else(|| {
      MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "B-tree child result charge overflow")
    })?;
    let construction_bytes = retained
      .iter()
      .try_fold(0usize, |total, child| total.checked_add(child.hash.len()).and_then(|bytes| bytes.checked_add(child.first_name.len())));
    let construction_charge = allocation_charge(
      construction_bytes.ok_or_else(|| {
        MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "B-tree construction size overflow")
      })?,
      retained.len().checked_mul(size_of::<TranslatedBtreeNodeV1>()).ok_or_else(|| {
        MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "B-tree construction item overflow")
      })?,
      0,
    )?;
    self.budget.reserve(construction_charge)?;
    let first_name = retained[0].first_name.clone();
    let keys = retained.iter().skip(1).map(|child| child.first_name.clone()).collect::<Vec<_>>();
    let children = retained.drain(..).map(|child| child.hash).collect::<Vec<_>>();
    let value = BTreeNode::Internal(crate::engine::btree::InternalNode { keys, children })
      .serialize(self.request.permit.hash_algorithm().hash_length())?;
    let (destination_hash, total_size) = self.publish_directory_value(entry_version, value)?;
    self.budget.release(construction_charge)?;
    self.budget.release(parse_charge)?;
    self.budget.release(retained_charge)?;
    self.push_btree_result(destination_hash, first_name, total_size)
  }

  fn process_file(&mut self, path: Arc<str>, hash: Vec<u8>) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let mut loaded = self.load_entity(&hash, EntryType::FileRecord, MAX_BATCH_ENCODED_BYTES as u32)?;
    validate_legacy_path_flags(path.as_ref(), loaded.header.flags, false)?;
    if loaded.header.compression_algo != CompressionAlgorithm::None {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_file_record_compression",
        format!("FileRecord {path} uses unsupported compression"),
      ));
    }
    let (declared_total_size, declared_content_hash, content_type, created_at, updated_at, chunk_offset, chunk_count, hash_length) = {
      let record =
        BorrowedFileRecordV1::decode(&loaded.value, self.request.permit.hash_algorithm().hash_length(), loaded.header.entry_version)?;
      if record.path != path.as_ref() {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_file_path",
          format!("FileRecord path '{}' differs from traversed path '{path}'", record.path),
        ));
      }
      canonical_file_key(self.request.permit.hash_algorithm(), &loaded.key, &loaded.value, &record)?;
      (
        record.total_size,
        record.content_hash.map(<[u8]>::to_vec),
        if record.content_type.is_empty() { None } else { Some(record.content_type.to_owned()) },
        record.created_at,
        record.updated_at,
        loaded.value.len() - record.chunk_bytes.len(),
        record.chunk_bytes.len() / record.hash_length,
        record.hash_length,
      )
    };
    let mut total_size = 0u64;
    let mut content_hasher = IncrementalDigestV1::new(self.request.permit.hash_algorithm());
    for index in 0..chunk_count {
      self.check_work()?;
      let start = chunk_offset + index * hash_length;
      let end = start + hash_length;
      let source_chunk = loaded.value[start..end].to_vec();
      let (decoded_bytes, destination_chunk) = self.copy_chunk(&source_chunk, loaded.header.flags == FLAG_SYSTEM, &mut content_hasher)?;
      total_size = total_size
        .checked_add(decoded_bytes)
        .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_file_size_overflow", "file size overflow"))?;
      loaded.value[start..end].copy_from_slice(&destination_chunk);
    }
    if total_size != declared_total_size {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_file_size",
        format!("FileRecord {path} declares {declared_total_size} bytes but contains {total_size}"),
      ));
    }
    let content_hash = content_hasher.finalize();
    if declared_content_hash.is_some_and(|expected| expected != content_hash) {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_file_content_hash",
        format!("FileRecord {path} whole-file content hash differs from its chunks"),
      ));
    }
    loaded.key = super::hash::digest_parts(self.request.permit.hash_algorithm(), &[b"filec:", &loaded.value]);
    let destination_hash = self.add_loaded_to_batch(&mut loaded, EntryTypeV4::FileRecord)?;
    self.release_loaded(loaded)?;
    self.push_entry_result(destination_hash, declared_total_size, content_type, Some(created_at), Some(updated_at))?;
    Ok(())
  }

  fn process_symlink(&mut self, path: Arc<str>, hash: Vec<u8>) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let mut loaded = self.load_entity(&hash, EntryType::Symlink, MAX_BATCH_ENCODED_BYTES as u32)?;
    validate_legacy_path_flags(path.as_ref(), loaded.header.flags, false)?;
    if loaded.header.compression_algo != CompressionAlgorithm::None {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_symlink_compression",
        format!("symlink {path} uses unsupported compression"),
      ));
    }
    let record = SymlinkRecord::deserialize(&loaded.value, loaded.header.entry_version)?;
    if record.path != path.as_ref() {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_symlink_path",
        format!("symlink path '{}' differs from traversed path '{path}'", record.path),
      ));
    }
    if normalize_path(&record.target) != record.target || record.serialize()? != loaded.value {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_symlink_noncanonical",
        format!("symlink {path} has a noncanonical target or trailing bytes"),
      ));
    }
    let created_at = record.created_at;
    let updated_at = record.updated_at;
    loaded.key = canonical_symlink_key(self.request.permit.hash_algorithm(), &loaded.key, &loaded.value, &record)?;
    let destination_hash = self.add_loaded_to_batch(&mut loaded, EntryTypeV4::Symlink)?;
    self.release_loaded(loaded)?;
    self.push_entry_result(destination_hash, 0, None, Some(created_at), Some(updated_at))?;
    Ok(())
  }

  fn copy_chunk(
    &mut self,
    hash: &[u8],
    allow_legacy_system_identity: bool,
    content_hasher: &mut IncrementalDigestV1,
  ) -> Result<(u64, Vec<u8>), MigrationBaseCloneExecutionErrorV1> {
    let header = self
      .request
      .source
      .historical_entry_header(hash)?
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_missing_chunk", hex::encode(hash)))?;
    if header.entry_type != EntryType::Chunk {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_chunk_type",
        format!("chunk identity {} resolves to {:?}", hex::encode(hash), header.entry_type),
      ));
    }
    let stored_bound = zstd::zstd_safe::compress_bound(self.request.maximum_decoded_chunk_bytes)
      .max(self.request.maximum_decoded_chunk_bytes)
      .min(u32::MAX as usize) as u32;
    if header.value_length > stored_bound {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_chunk_stored_bound",
        format!("chunk {} stored length {} exceeds bound {stored_bound}", hex::encode(hash), header.value_length),
      ));
    }
    let transient_charge = u64::from(header.value_length)
      .checked_add(self.request.maximum_decoded_chunk_bytes as u64)
      .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "chunk memory estimate overflow"))?;
    self.budget.reserve(transient_charge)?;
    let result = self.request.source.historical_entry_verified_bounded(hash, stored_bound);
    let (actual, key, stored) = match result {
      Ok(Some(entry)) => entry,
      Ok(None) => {
        self.budget.release(transient_charge)?;
        return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_missing_chunk", hex::encode(hash)));
      }
      Err(error) => {
        self.budget.release(transient_charge)?;
        return Err(error.into());
      }
    };
    if !source_headers_match(&header, &actual) {
      self.budget.release(transient_charge)?;
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_source_changed",
        format!("chunk {} header changed between bounded reads", hex::encode(hash)),
      ));
    }
    validate_loaded_header(hash, &key, stored.len(), &actual, self.request.permit.hash_algorithm(), EntryType::Chunk)?;
    if actual.entry_version != 0 {
      self.budget.release(transient_charge)?;
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_chunk_version",
        format!("chunk {} uses unsupported entity version {}", hex::encode(hash), actual.entry_version),
      ));
    }
    let decoded = decompress_bounded(&stored, actual.compression_algo, self.request.maximum_decoded_chunk_bytes)?;
    let canonical_key = super::hash::digest_parts(self.request.permit.hash_algorithm(), &[b"chunk:", decoded.as_slice()]);
    let legacy_system_key = allow_legacy_system_identity
      .then(|| super::hash::digest_parts(self.request.permit.hash_algorithm(), &[b"system::", decoded.as_slice()]));
    if key != canonical_key && legacy_system_key.as_deref() != Some(key.as_slice()) {
      self.budget.release(transient_charge)?;
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_chunk_identity",
        format!("chunk {} is stored under a noncanonical identity", hex::encode(hash)),
      ));
    }
    content_hasher.update(&decoded);
    let decoded_length = usize_to_u64(decoded.len(), "decoded chunk length")?;
    self.loaded_entities = self
      .loaded_entities
      .checked_add(1)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "loaded entity count overflow"))?;
    self.copied_chunk_bytes = self
      .copied_chunk_bytes
      .checked_add(decoded_length)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "chunk byte count overflow"))?;
    self.batch.add(0, EntryTypeV4::Chunk, canonical_key.clone(), decoded, &mut self.budget)?;
    self.budget.release(transient_charge)?;
    Ok((decoded_length, canonical_key))
  }

  fn process_btree_hash(
    &mut self,
    path: Arc<str>,
    hash: Vec<u8>,
    depth: usize,
    lower_bound: Option<Arc<str>>,
    upper_bound: Option<Arc<str>>,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    if depth >= MAX_BTREE_DEPTH || self.active_btree_nodes.iter().any(|ancestor| ancestor == &hash) {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_btree_cycle_or_depth",
        format!("B-tree ancestry is invalid at {}", hex::encode(hash)),
      ));
    }
    let loaded = self.load_entity(&hash, EntryType::DirectoryIndex, MAX_BATCH_ENCODED_BYTES as u32)?;
    if loaded.header.compression_algo != CompressionAlgorithm::None {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_btree_compression",
        format!("B-tree node {} uses unsupported compression", hex::encode(&hash)),
      ));
    }
    if loaded.header.flags != 0 {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_btree_flags",
        format!("B-tree node {} has noncanonical flags", hex::encode(&hash)),
      ));
    }
    canonical_directory_key(self.request.permit.hash_algorithm(), "/", &loaded.key, &loaded.value, false)?;
    self.process_loaded_btree_node(
      path,
      hash,
      loaded.header.entry_version,
      &loaded.value,
      BtreeRangeV1 { depth, lower_bound, upper_bound },
    )?;
    self.release_loaded(loaded)
  }

  fn process_loaded_btree_node(
    &mut self,
    path: Arc<str>,
    hash: Vec<u8>,
    entry_version: u8,
    value: &[u8],
    range: BtreeRangeV1,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let BtreeRangeV1 { depth, lower_bound, upper_bound } = range;
    if depth >= MAX_BTREE_DEPTH || self.active_btree_nodes.iter().any(|ancestor| ancestor == &hash) {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_btree_cycle_or_depth",
        format!("B-tree ancestry is invalid at {}", hex::encode(hash)),
      ));
    }
    let ancestry_charge = allocation_charge(hash.len(), 0, 0)?;
    self.budget.reserve(ancestry_charge)?;
    self.active_btree_nodes.push(hash.clone());
    self.maximum_btree_depth = self.maximum_btree_depth.max(depth + 1);
    self.push_work_unaccounted(CloneWorkV1::BtreeExit { hash, memory_charge: ancestry_charge })?;
    let parse_bytes = usize_to_u64(value.len(), "B-tree parse size")?;
    let parse_charge = parse_bytes
      .checked_mul(4)
      .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "B-tree parse estimate overflow"))?;
    self.budget.reserve(parse_charge)?;
    let node = BTreeNode::deserialize(value, self.request.permit.hash_algorithm().hash_length(), entry_version)?;
    validate_btree_node(&node, lower_bound.as_deref(), upper_bound.as_deref())?;
    if node.serialize(self.request.permit.hash_algorithm().hash_length())? != value {
      self.budget.release(parse_charge)?;
      let active_hash = match self.active_btree_nodes.last() {
        Some(active_hash) => active_hash,
        None => {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_btree_ancestry",
            "B-tree parse completed without active ancestry",
          ));
        }
      };
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_btree_noncanonical",
        format!("B-tree node {} has trailing or noncanonical bytes", hex::encode(active_hash)),
      ));
    }
    match node {
      BTreeNode::Leaf(leaf) => {
        let entries = leaf.entries;
        self.push_work_unaccounted(CloneWorkV1::BtreeLeafFinalize { entries: entries.clone(), entry_version, parse_charge })?;
        for child in entries.into_iter().rev() {
          self.push_child(path.clone(), child, self.active_directories.len())?;
        }
      }
      BTreeNode::Internal(internal) => {
        let keys = internal.keys.into_iter().map(Arc::<str>::from).collect::<Vec<_>>();
        let child_count = internal.children.len();
        self.push_work_unaccounted(CloneWorkV1::BtreeInternalFinalize { child_count, entry_version, parse_charge })?;
        for (index, child_hash) in internal.children.into_iter().enumerate().rev() {
          let child_lower = if index == 0 { lower_bound.clone() } else { Some(keys[index - 1].clone()) };
          let child_upper = if index == keys.len() { upper_bound.clone() } else { Some(keys[index].clone()) };
          self.push_work(CloneWorkV1::BtreeNode {
            path: path.clone(),
            hash: child_hash,
            depth: depth + 1,
            lower_bound: child_lower,
            upper_bound: child_upper,
          })?;
        }
      }
    }
    Ok(())
  }

  fn push_child(&mut self, parent: Arc<str>, child: ChildEntry, depth: usize) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    validate_child_name(&child.name)?;
    if child.hash.len() != self.request.permit.hash_algorithm().hash_length() {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_child_hash_width",
        format!("child '{}' hash width differs from database", child.name),
      ));
    }
    let path = if parent.as_ref() == "/" { format!("/{}", child.name) } else { format!("{parent}/{}", child.name) };
    validate_path(&path)?;
    self.push_work(CloneWorkV1::Entry {
      path: Arc::from(path),
      hash: child.hash,
      entry_type: EntryType::from_u8(child.entry_type)?,
      logical_bytes: child.total_size,
      directory_depth: depth,
    })
  }

  fn load_entity(
    &mut self,
    hash: &[u8],
    expected_type: EntryType,
    maximum_value_length: u32,
  ) -> Result<LoadedSourceEntityV1, MigrationBaseCloneExecutionErrorV1> {
    let header = self
      .request
      .source
      .historical_entry_header(hash)?
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_missing_entity", hex::encode(hash)))?;
    if header.value_length > maximum_value_length {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_entity_too_large",
        format!("entity {} value length {} exceeds {maximum_value_length}", hex::encode(hash), header.value_length),
      ));
    }
    let memory_charge = u64::from(header.value_length)
      .checked_add(u64::from(header.key_length))
      .and_then(|bytes| bytes.checked_add(header.header_size() as u64))
      .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "source entry estimate overflow"))?;
    self.budget.reserve(memory_charge)?;
    let result = self.request.source.historical_entry_verified_bounded(hash, maximum_value_length);
    let (actual, key, value) = match result {
      Ok(Some(entry)) => entry,
      Ok(None) => {
        self.budget.release(memory_charge)?;
        return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_missing_entity", hex::encode(hash)));
      }
      Err(error) => {
        self.budget.release(memory_charge)?;
        return Err(error.into());
      }
    };
    if !source_headers_match(&header, &actual) {
      self.budget.release(memory_charge)?;
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_source_changed",
        format!("entity {} header changed between bounded reads", hex::encode(hash)),
      ));
    }
    validate_loaded_header(hash, &key, value.len(), &actual, self.request.permit.hash_algorithm(), expected_type)?;
    self.loaded_entities = self
      .loaded_entities
      .checked_add(1)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "loaded entity count overflow"))?;
    Ok(LoadedSourceEntityV1 { header: actual, key, value, memory_charge })
  }

  fn add_loaded_to_batch(
    &mut self,
    loaded: &mut LoadedSourceEntityV1,
    entry_type: EntryTypeV4,
  ) -> Result<Vec<u8>, MigrationBaseCloneExecutionErrorV1> {
    let value = std::mem::take(&mut loaded.value);
    if entry_type == EntryTypeV4::Chunk {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_entity_type", "chunks must use the decoded copy path"));
    }
    let expected_key =
      super::hash::digest_parts(self.request.permit.hash_algorithm(), &[content_domain(entry_type, &value)?, value.as_slice()]);
    if expected_key != loaded.key {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_content_identity",
        format!("source identity {} is not canonical for {entry_type:?}", hex::encode(&loaded.key)),
      ));
    }
    let key = std::mem::take(&mut loaded.key);
    let result = key.clone();
    self.batch.add(loaded.header.entry_version, entry_type, key, value, &mut self.budget)?;
    Ok(result)
  }

  fn release_loaded(&mut self, loaded: LoadedSourceEntityV1) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    self.budget.release(loaded.memory_charge)
  }

  fn publish_directory_value(&mut self, entry_version: u8, value: Vec<u8>) -> Result<(Vec<u8>, u64), MigrationBaseCloneExecutionErrorV1> {
    let total_size = usize_to_u64(value.len(), "directory body size")?;
    let domain = if crate::engine::btree::is_btree_format(&value) { b"btree:".as_slice() } else { b"dirc:".as_slice() };
    let key = super::hash::digest_parts(self.request.permit.hash_algorithm(), &[domain, &value]);
    let result = key.clone();
    self.batch.add(entry_version, EntryTypeV4::DirectoryIndex, key, value, &mut self.budget)?;
    Ok((result, total_size))
  }

  fn push_entry_result(
    &mut self,
    hash: Vec<u8>,
    total_size: u64,
    content_type: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let content_type_bytes = content_type.as_ref().map_or(0, String::len);
    let memory_charge = allocation_charge(hash.len(), content_type_bytes, 0)?
      .checked_add(if content_type.is_some() { OWNED_ALLOCATION_OVERHEAD } else { 0 })
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "entry result charge overflow"))?;
    self.budget.reserve(memory_charge)?;
    self.entry_results.push(Some(TranslatedEntryV1 { hash, total_size, content_type, created_at, updated_at, memory_charge }));
    Ok(())
  }

  fn push_btree_result(&mut self, hash: Vec<u8>, first_name: String, total_size: u64) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let memory_charge = allocation_charge(hash.len(), first_name.len(), 0)?;
    self.budget.reserve(memory_charge)?;
    self.btree_results.push(Some(TranslatedBtreeNodeV1 { hash, first_name, total_size, memory_charge }));
    Ok(())
  }

  fn queue_seed_result(
    &mut self,
    seed: MigrationBaseCloneSeedV1,
    destination: Option<TranslatedEntryV1>,
  ) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let memory_charge = allocation_charge(size_of::<MigrationBaseCloneSeedV1>(), seed.path.len(), seed.hash.len())?;
    self.budget.reserve(memory_charge)?;
    self.pending_seed_results.push(PendingSeedResultV1 { seed, destination, memory_charge });
    Ok(())
  }

  fn flush_pending_seed_results(&mut self) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    if self.pending_seed_results.is_empty() {
      return self.batch.flush(&mut self.budget);
    }
    self.batch.flush(&mut self.budget)?;
    let pending = std::mem::take(&mut self.pending_seed_results);
    for result in pending {
      let callback = self
        .request
        .seed_results
        .record_seed_result(&result.seed, result.destination.as_ref().map(|destination| destination.hash.as_slice()));
      let destination_charge = result.destination.as_ref().map_or(0, |destination| destination.memory_charge);
      let release_charge = result.memory_charge.checked_add(destination_charge).ok_or_else(|| {
        MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "seed result release charge overflow")
      })?;
      self.budget.release(release_charge)?;
      callback.map_err(MigrationBaseCloneExecutionErrorV1::SeedResult)?;
    }
    Ok(())
  }

  fn take_entry_results(&mut self, count: usize) -> Result<Vec<Option<TranslatedEntryV1>>, MigrationBaseCloneExecutionErrorV1> {
    let start = self.entry_results.len().checked_sub(count).ok_or_else(|| {
      MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_result_unbalanced", "directory child results are incomplete")
    })?;
    Ok(self.entry_results.split_off(start))
  }

  fn take_btree_results(&mut self, count: usize) -> Result<Vec<Option<TranslatedBtreeNodeV1>>, MigrationBaseCloneExecutionErrorV1> {
    let start =
      self.btree_results.len().checked_sub(count).ok_or_else(|| {
        MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_btree_result", "B-tree child results are incomplete")
      })?;
    Ok(self.btree_results.split_off(start))
  }

  fn push_work(&mut self, work: CloneWorkV1) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let charge = work_charge(&work)?;
    self.budget.reserve(charge)?;
    self.work.push((work, charge));
    self.maximum_frontier_items = self.maximum_frontier_items.max(self.work.len());
    Ok(())
  }

  fn push_work_unaccounted(&mut self, work: CloneWorkV1) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    let charge = usize_to_u64(size_of::<CloneWorkV1>(), "unaccounted work item")?
      .checked_add(OWNED_ALLOCATION_OVERHEAD)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "work item estimate overflow"))?;
    self.budget.reserve(charge)?;
    self.work.push((work, charge));
    self.maximum_frontier_items = self.maximum_frontier_items.max(self.work.len());
    Ok(())
  }

  fn exit_directory(&mut self, hash: Vec<u8>, memory_charge: u64) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    if self.active_directories.pop().as_deref() != Some(hash.as_slice()) {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_directory_ancestry",
        "directory exit does not match active ancestry",
      ));
    }
    self.budget.release(memory_charge)
  }

  fn exit_btree(&mut self, hash: Vec<u8>, memory_charge: u64) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    if self.active_btree_nodes.pop().as_deref() != Some(hash.as_slice()) {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_btree_ancestry",
        "B-tree exit does not match active ancestry",
      ));
    }
    self.budget.release(memory_charge)
  }

  fn check_work(&mut self) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
    self.work_items = self
      .work_items
      .checked_add(1)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "work count overflow"))?;
    if self.work_items > self.request.maximum_work_items {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_work_limit",
        format!("work count exceeds {}", self.request.maximum_work_items),
      ));
    }
    self.work_since_cancellation = self
      .work_since_cancellation
      .checked_add(1)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", "cancellation counter overflow"))?;
    if self.work_since_cancellation >= CANCELLATION_QUANTUM || self.request.cancellation.is_cancelled() {
      self.work_since_cancellation = 0;
      if self.request.cancellation.is_cancelled() {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_canceled", "base clone was canceled"));
      }
      self.request.memory.check_admission(MemoryOwner::Migration, AdmissionClass::Maintenance)?;
    }
    Ok(())
  }
}

fn validate_request(request: &MigrationBaseCloneExecutionRequestV1<'_>) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
  if request.cancellation.is_cancelled() {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_canceled", "base clone was canceled"));
  }
  if request.maximum_work_items == 0
    || request.maximum_memory_bytes == 0
    || request.maximum_decoded_chunk_bytes == 0
    || request.maximum_decoded_chunk_bytes > MAX_BATCH_ENCODED_BYTES
    || request.maximum_directory_depth == 0
    || request.maximum_directory_depth > MAX_DIRECTORY_DEPTH
  {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_limits", "one or more execution bounds are invalid"));
  }
  if request.source.hash_algorithm() != request.permit.hash_algorithm() {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_hash_algorithm",
      "source, destination, and preflight hash algorithms differ",
    ));
  }
  if request.source.physical_identity()? != request.permit.source_file_identity() {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_source_identity",
      "source physical identity differs from preflight",
    ));
  }
  let destination = request.destination.observe().map_err(ImmutableEntityBatchPublicationErrorV1::from)?;
  if destination.selected.header.hash_algorithm != request.permit.hash_algorithm()
    || destination.selected.redundancy_degraded
    || destination.selected.header.database_id != request.permit.database_id()
    || destination.selected.header.physical_instance_id != request.permit.destination_physical_instance_id()
  {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_destination_identity",
      "destination authority differs from preflight or has degraded redundancy",
    ));
  }
  Ok(())
}

fn validate_loaded_header(
  requested_hash: &[u8],
  key: &[u8],
  value_length: usize,
  header: &EntryHeader,
  algorithm: HashAlgorithm,
  expected_type: EntryType,
) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
  if key != requested_hash {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_source_key_mismatch",
      format!("requested {}, loaded {}", hex::encode(requested_hash), hex::encode(key)),
    ));
  }
  if header.entry_type != expected_type {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_source_type_mismatch",
      format!("identity {} is {:?}, expected {expected_type:?}", hex::encode(requested_hash), header.entry_type),
    ));
  }
  if header.hash_algo != algorithm || header.key_length as usize != algorithm.hash_length() {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_source_hash_profile",
      "source entry hash profile differs from preflight",
    ));
  }
  if header.value_length as usize != value_length {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_source_length",
      format!("source entry {} header/value lengths disagree", hex::encode(requested_hash)),
    ));
  }
  if header.flags & !FLAG_SYSTEM != 0 || header.encryption_algo != 0 {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_source_representation",
      "source entry uses unknown flags or encryption",
    ));
  }
  Ok(())
}

fn source_headers_match(expected: &EntryHeader, actual: &EntryHeader) -> bool {
  expected.entry_version == actual.entry_version
    && expected.entry_type == actual.entry_type
    && expected.flags == actual.flags
    && expected.hash_algo == actual.hash_algo
    && expected.compression_algo == actual.compression_algo
    && expected.encryption_algo == actual.encryption_algo
    && expected.key_length == actual.key_length
    && expected.value_length == actual.value_length
    && expected.timestamp == actual.timestamp
    && expected.total_length == actual.total_length
    && expected.hash == actual.hash
}

fn validate_btree_node(
  node: &BTreeNode,
  lower_bound: Option<&str>,
  upper_bound: Option<&str>,
) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
  if lower_bound.zip(upper_bound).is_some_and(|(lower, upper)| lower >= upper) {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_btree_range",
      "B-tree inherited separator range is empty or reversed",
    ));
  }
  match node {
    BTreeNode::Leaf(leaf) => {
      if leaf.entries.len() > BTREE_MAX_LEAF_ENTRIES || !strictly_ordered(leaf.entries.iter().map(|entry| entry.name.as_str())) {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_btree_leaf", "B-tree leaf count or ordering is invalid"));
      }
      if leaf.entries.iter().any(|entry| !within_btree_range(&entry.name, lower_bound, upper_bound)) {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_btree_range",
          "B-tree leaf entry is outside its inherited separator range",
        ));
      }
    }
    BTreeNode::Internal(internal) => {
      let child_count_valid = internal.keys.len() <= BTREE_MAX_INTERNAL_KEYS && internal.children.len() == internal.keys.len() + 1;
      if !child_count_valid || !strictly_ordered(internal.keys.iter().map(String::as_str)) {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_btree_internal",
          "B-tree internal count or ordering is invalid",
        ));
      }
      if internal
        .children
        .iter()
        .enumerate()
        .any(|(index, child)| internal.children[index + 1..].iter().any(|candidate| candidate == child))
      {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_btree_duplicate_child",
          "B-tree internal node contains a duplicate child locator",
        ));
      }
      for key in &internal.keys {
        validate_child_name(key)?;
        if !within_btree_range(key, lower_bound, upper_bound) {
          return Err(MigrationBaseCloneExecutionErrorV1::invalid(
            "migration_clone_btree_range",
            "B-tree internal separator is outside its inherited range",
          ));
        }
      }
    }
  }
  Ok(())
}

fn within_btree_range(value: &str, lower_bound: Option<&str>, upper_bound: Option<&str>) -> bool {
  lower_bound.is_none_or(|lower| value >= lower) && upper_bound.is_none_or(|upper| value < upper)
}

fn strictly_ordered<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
  let Some(mut prior) = values.next() else {
    return true;
  };
  for value in values {
    if prior >= value {
      return false;
    }
    prior = value;
  }
  true
}

fn apply_translated_child(child: &mut ChildEntry, result: TranslatedEntryV1) {
  child.hash = result.hash;
  child.total_size = result.total_size;
  child.content_type = result.content_type;
  if let Some(created_at) = result.created_at {
    child.created_at = created_at;
  }
  if let Some(updated_at) = result.updated_at {
    child.updated_at = updated_at;
  }
}

fn validate_child_name(name: &str) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
  if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_child_name",
      format!("directory child name {name:?} is not one canonical path segment"),
    ));
  }
  Ok(())
}

fn validate_path(path: &str) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
  if path.is_empty() || !path.starts_with('/') || path.len() > MAX_SEED_PATH_BYTES || path.contains('\0') || normalize_path(path) != path {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_path",
      format!("path {path:?} is not canonical or exceeds the encoded bound"),
    ));
  }
  Ok(())
}

fn validate_legacy_path_flags(
  path: &str,
  flags: u8,
  allow_unflagged_content_identity: bool,
) -> Result<(), MigrationBaseCloneExecutionErrorV1> {
  let system_path =
    path == "/.aeordb-system" || path.starts_with("/.aeordb-system/") || path == "/.aeordb-config" || path.starts_with("/.aeordb-config/");
  let valid = if system_path { flags == FLAG_SYSTEM || (allow_unflagged_content_identity && flags == 0) } else { flags == 0 };
  if !valid {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_source_flags",
      format!("legacy flags {flags:#04x} do not match path {path}"),
    ));
  }
  Ok(())
}

fn content_domain(entry_type: EntryTypeV4, value: &[u8]) -> Result<&'static [u8], MigrationBaseCloneExecutionErrorV1> {
  match entry_type {
    EntryTypeV4::FileRecord => Ok(b"filec:"),
    EntryTypeV4::DirectoryIndex if crate::engine::btree::is_btree_format(value) => Ok(b"btree:"),
    EntryTypeV4::DirectoryIndex => Ok(b"dirc:"),
    EntryTypeV4::Symlink => Ok(b"symlinkc:"),
    _ => Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_entity_type",
      format!("{entry_type:?} is not ordinary immutable migration content"),
    )),
  }
}

fn canonical_directory_key(
  algorithm: HashAlgorithm,
  path: &str,
  source_key: &[u8],
  value: &[u8],
  allow_path_locator: bool,
) -> Result<Vec<u8>, MigrationBaseCloneExecutionErrorV1> {
  let domain = if crate::engine::btree::is_btree_format(value) { b"btree:".as_slice() } else { b"dirc:".as_slice() };
  let content_key = super::hash::digest_parts(algorithm, &[domain, value]);
  let path_key = super::hash::digest_parts(algorithm, &[b"dir:", path.as_bytes()]);
  if source_key != content_key && (!allow_path_locator || source_key != path_key) {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_directory_identity",
      format!("directory {path} is not stored under its content identity or admitted path locator"),
    ));
  }
  Ok(content_key)
}

fn canonical_file_key(
  algorithm: HashAlgorithm,
  source_key: &[u8],
  value: &[u8],
  record: &BorrowedFileRecordV1<'_>,
) -> Result<Vec<u8>, MigrationBaseCloneExecutionErrorV1> {
  let content_key = super::hash::digest_parts(algorithm, &[b"filec:", value]);
  let identity_key = super::hash::digest_parts(
    algorithm,
    &[b"fileid:", record.path.as_bytes(), &[0], record.content_type.as_bytes(), &[0], record.chunk_bytes],
  );
  let path_key = super::hash::digest_parts(algorithm, &[b"file:", record.path.as_bytes()]);
  if source_key != content_key && source_key != identity_key && source_key != path_key {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_file_identity",
      format!("FileRecord {} is not stored under a canonical v3 identity", record.path),
    ));
  }
  Ok(content_key)
}

fn canonical_symlink_key(
  algorithm: HashAlgorithm,
  source_key: &[u8],
  value: &[u8],
  record: &SymlinkRecord,
) -> Result<Vec<u8>, MigrationBaseCloneExecutionErrorV1> {
  let content_key = super::hash::digest_parts(algorithm, &[b"symlinkc:", value]);
  let identity_key = super::hash::digest_parts(algorithm, &[b"symlinkid:", record.path.as_bytes(), &[0], record.target.as_bytes()]);
  let path_key = super::hash::digest_parts(algorithm, &[b"symlink:", record.path.as_bytes()]);
  if source_key != content_key && source_key != identity_key && source_key != path_key {
    return Err(MigrationBaseCloneExecutionErrorV1::invalid(
      "migration_clone_symlink_identity",
      format!("symlink {} is not stored under a canonical v3 identity", record.path),
    ));
  }
  Ok(content_key)
}

fn usize_to_u64(value: usize, context: &'static str) -> Result<u64, MigrationBaseCloneExecutionErrorV1> {
  match u64::try_from(value) {
    Ok(value) => Ok(value),
    Err(error) => {
      Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", format!("{context} exceeds u64: {error}")))
    }
  }
}

fn usize_to_u16(value: usize, context: &'static str) -> Result<u16, MigrationBaseCloneExecutionErrorV1> {
  match u16::try_from(value) {
    Ok(value) => Ok(value),
    Err(error) => {
      Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_counter_overflow", format!("{context} exceeds u16: {error}")))
    }
  }
}

fn allocation_charge(first: usize, second: usize, third: usize) -> Result<u64, MigrationBaseCloneExecutionErrorV1> {
  let first = usize_to_u64(first, "allocation first component")?;
  let second = usize_to_u64(second, "allocation second component")?;
  let third = usize_to_u64(third, "allocation third component")?;
  first
    .checked_add(second)
    .and_then(|value| value.checked_add(third))
    .and_then(|value| value.checked_add(OWNED_ALLOCATION_OVERHEAD))
    .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "allocation estimate overflow"))
}

fn work_charge(work: &CloneWorkV1) -> Result<u64, MigrationBaseCloneExecutionErrorV1> {
  let dynamic = match work {
    CloneWorkV1::Entry { path, hash, .. } => path
      .len()
      .checked_add(hash.len())
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "work item size overflow"))?,
    CloneWorkV1::BtreeNode { path, hash, lower_bound, upper_bound, .. } => path
      .len()
      .checked_add(hash.len())
      .and_then(|bytes| bytes.checked_add(lower_bound.as_deref().map_or(0, str::len)))
      .and_then(|bytes| bytes.checked_add(upper_bound.as_deref().map_or(0, str::len)))
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_memory_overflow", "B-tree work item size overflow"))?,
    CloneWorkV1::FlatDirectoryFinalize { path, .. } | CloneWorkV1::BtreeDirectoryFinalize { path, .. } => path.len(),
    CloneWorkV1::BtreeLeafFinalize { .. } | CloneWorkV1::BtreeInternalFinalize { .. } => 0,
    CloneWorkV1::DirectoryExit { hash, .. } | CloneWorkV1::BtreeExit { hash, .. } => hash.len(),
  };
  allocation_charge(size_of::<CloneWorkV1>(), dynamic, 0)
}

struct BorrowedFileRecordV1<'a> {
  path: &'a str,
  content_type: &'a str,
  total_size: u64,
  created_at: i64,
  updated_at: i64,
  content_hash: Option<&'a [u8]>,
  chunk_bytes: &'a [u8],
  hash_length: usize,
}

impl<'a> BorrowedFileRecordV1<'a> {
  fn decode(data: &'a [u8], hash_length: usize, version: u8) -> Result<Self, MigrationBaseCloneExecutionErrorV1> {
    if !matches!(version, 0 | 1) || hash_length == 0 {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_file_record_version", format!("version {version}")));
    }
    let mut cursor = 0usize;
    let path_length = read_u16(data, &mut cursor)?;
    let path_bytes = read_slice(data, &mut cursor, path_length)?;
    let path = match std::str::from_utf8(path_bytes) {
      Ok(path) => path,
      Err(error) => {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_file_record_utf8",
          format!("FileRecord path is not UTF-8: {error}"),
        ));
      }
    };
    let content_type_length = read_u16(data, &mut cursor)?;
    let content_type = read_slice(data, &mut cursor, content_type_length)?;
    let content_type = match std::str::from_utf8(content_type) {
      Ok(content_type) => content_type,
      Err(error) => {
        return Err(MigrationBaseCloneExecutionErrorV1::invalid(
          "migration_clone_file_record_utf8",
          format!("FileRecord content type is not UTF-8: {error}"),
        ));
      }
    };
    let total_size = read_u64(data, &mut cursor)?;
    let created_at = read_i64(data, &mut cursor)?;
    let updated_at = read_i64(data, &mut cursor)?;
    let content_hash = if version == 1 { Some(read_slice(data, &mut cursor, hash_length)?) } else { None };
    let metadata_length = read_u32(data, &mut cursor)?;
    read_slice(data, &mut cursor, metadata_length)?;
    let chunk_count = read_u32(data, &mut cursor)?;
    let chunk_length = chunk_count
      .checked_mul(hash_length)
      .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_file_record_overflow", "chunk bytes overflow"))?;
    let chunk_bytes = read_slice(data, &mut cursor, chunk_length)?;
    if cursor != data.len() {
      return Err(MigrationBaseCloneExecutionErrorV1::invalid(
        "migration_clone_file_record_trailing",
        format!("FileRecord has {} trailing bytes", data.len() - cursor),
      ));
    }
    Ok(Self { path, content_type, total_size, created_at, updated_at, content_hash, chunk_bytes, hash_length })
  }
}

fn read_u16(data: &[u8], cursor: &mut usize) -> Result<usize, MigrationBaseCloneExecutionErrorV1> {
  let bytes = read_slice(data, cursor, 2)?;
  Ok(u16::from_le_bytes([bytes[0], bytes[1]]) as usize)
}

fn read_u32(data: &[u8], cursor: &mut usize) -> Result<usize, MigrationBaseCloneExecutionErrorV1> {
  let bytes = read_slice(data, cursor, 4)?;
  Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
}

fn read_u64(data: &[u8], cursor: &mut usize) -> Result<u64, MigrationBaseCloneExecutionErrorV1> {
  let bytes = read_slice(data, cursor, 8)?;
  Ok(u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]))
}

fn read_i64(data: &[u8], cursor: &mut usize) -> Result<i64, MigrationBaseCloneExecutionErrorV1> {
  let bytes = read_slice(data, cursor, 8)?;
  Ok(i64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]))
}

fn read_slice<'a>(data: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], MigrationBaseCloneExecutionErrorV1> {
  let end = cursor
    .checked_add(length)
    .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_file_record_overflow", "FileRecord offset overflow"))?;
  let bytes = data
    .get(*cursor..end)
    .ok_or_else(|| MigrationBaseCloneExecutionErrorV1::invalid("migration_clone_file_record_truncated", "FileRecord is truncated"))?;
  *cursor = end;
  Ok(bytes)
}
