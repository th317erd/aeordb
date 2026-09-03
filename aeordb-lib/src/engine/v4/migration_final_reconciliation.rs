//! Live final-write freeze and exact source-authority capture for v3-to-v4 migration.
//!
//! A durable migration flag records history; it is never treated as a live
//! process lock after restart. The nonconstructible owner below must remain
//! alive through reconciliation and later destination verification.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::mem::size_of;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::hash::digest_parts;
use super::migration_base_clone_execution::MigrationBaseCloneEntrySourceV1;
use super::migration_capture_replay::{
  MigrationCaptureReplayAuthorityTemplateV1, MigrationCaptureReplayErrorV1, MigrationPathProjectionModeV1,
  MigrationPathProjectionRequestV1, MigrationSuccessorProjectionRequestV1, load_destination_tree, namespace_root_for_tree,
  project_migration_authoritative_path_v1, publish_migration_successor_v1,
};
use super::migration_preflight::MigrationPreflightPermitV1;
use super::reader::FormatError;
use super::system_family::embedded_system_family_registry;
use crate::engine::btree::{BTREE_CONVERSION_THRESHOLD, BTREE_MAX_INTERNAL_KEYS, BTREE_MAX_LEAF_ENTRIES, BTreeNode, is_btree_format};
use crate::engine::directory_entry::ChildEntry;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use crate::engine::storage_engine::{ExclusiveReadOnlyEngineMaintenanceGuard, FrozenSourceAuthoritySnapshot, StorageEngine};
use crate::engine::v4::first_authority::V4FirstAuthorityPublisher;
use crate::engine::{CompressionAlgorithm, EngineError, EngineResult, EntryHeader, EntryType, HashAlgorithm};

const MAXIMUM_FREEZE_ACQUISITION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const MAXIMUM_DIFF_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const MAXIMUM_DIFF_WORK_ITEMS: u64 = 1 << 40;
const MAXIMUM_DIFF_DIRECTORY_DEPTH: usize = 1_000;
const MAXIMUM_DIFF_BTREE_DEPTH: usize = 128;
const MAXIMUM_DIFF_ENTITY_BYTES: usize = 64 * 1024 * 1024;
const OWNED_ALLOCATION_OVERHEAD: u64 = 128;
const FINAL_RECONCILIATION_TRANSACTION_DOMAIN: &[u8] = b"aeordb.migration-final-reconciliation.transaction.v1\0";
const FINAL_RECONCILIATION_CLOSURE_DOMAIN: &[u8] = b"aeordb.migration-final-reconciliation.closure.v1\0";

pub struct MigrationSourceWriteFreezeRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub source: &'a StorageEngine,
  pub cancellation: &'a CancellationToken,
  pub acquisition_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenMigrationSourceAuthorityV1 {
  pub physical_identity: PlatformFileIdentityDescriptorV1,
  pub header_sequence: u64,
  pub namespace_root: Vec<u8>,
  pub hard_publication_frontier: u64,
  pub hash_algorithm: HashAlgorithm,
  pub system_family_registry_fingerprint: Vec<u8>,
}

pub struct MigrationSourceWriteFreezeV1<'a> {
  source: &'a StorageEngine,
  authority: FrozenMigrationSourceAuthorityV1,
  _exclusive: ExclusiveReadOnlyEngineMaintenanceGuard<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationMerkleChangeKindV1 {
  Added,
  Removed,
  Replaced,
  MetadataOnly,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MigrationMerkleChangeV1 {
  pub path: String,
  pub kind: MigrationMerkleChangeKindV1,
  pub basis: Option<ChildEntry>,
  pub target: Option<ChildEntry>,
}

pub trait MigrationMerkleDiffSinkV1 {
  fn record_change(&mut self, change: &MigrationMerkleChangeV1) -> EngineResult<()>;
}

pub struct MigrationMerkleDiffRequestV1<'a> {
  pub source: &'a dyn MigrationBaseCloneEntrySourceV1,
  pub basis_root: &'a [u8],
  pub target_root: &'a [u8],
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub maximum_memory_bytes: u64,
  pub maximum_work_items: u64,
  pub maximum_directory_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationMerkleDiffReceiptV1 {
  pub basis_root: Vec<u8>,
  pub target_root: Vec<u8>,
  pub changed_path_count: u64,
  pub metadata_only_count: u64,
  pub visited_directory_count: u64,
  pub visited_entity_count: u64,
  pub visited_btree_node_count: u64,
  pub compared_child_count: u64,
  pub maximum_memory_used_bytes: u64,
  pub maximum_frontier_items: u64,
}

pub struct MigrationFinalNamespaceReconciliationRequestV1<'request, 'source> {
  pub permit: &'request MigrationPreflightPermitV1,
  pub freeze: &'request MigrationSourceWriteFreezeV1<'source>,
  pub destination: &'request V4FirstAuthorityPublisher,
  pub last_reconciled_source_root: &'request [u8],
  pub current_destination_tree_root: &'request [u8],
  pub authority: &'request MigrationCaptureReplayAuthorityTemplateV1,
  pub memory: &'request MemoryCoordinator,
  pub cancellation: &'request CancellationToken,
  pub publication_timestamp_ms: u64,
  pub maximum_diff_memory_bytes: u64,
  pub maximum_diff_work_items: u64,
  pub maximum_subtree_memory_bytes: u64,
  pub maximum_subtree_work_items: u64,
  pub maximum_total_subtree_work_items: u64,
  pub maximum_decoded_chunk_bytes: usize,
  pub maximum_directory_depth: usize,
}

pub struct MigrationFinalNamespaceReconciliationReceiptV1<'freeze, 'source> {
  pub frozen_source_root: Vec<u8>,
  pub frozen_source_publication_sequence: u64,
  pub diff: MigrationMerkleDiffReceiptV1,
  pub translated_subtree_count: u64,
  pub translated_subtree_work_items: u64,
  pub reused_destination_entity_count: u64,
  pub destination_successor_count: u64,
  pub idempotent: bool,
  pub destination_header_sequence: u64,
  pub destination_namespace_root: Vec<u8>,
  pub destination_tree_root: Vec<u8>,
  freeze: &'freeze MigrationSourceWriteFreezeV1<'source>,
}

impl fmt::Debug for MigrationFinalNamespaceReconciliationReceiptV1<'_, '_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("MigrationFinalNamespaceReconciliationReceiptV1")
      .field("frozen_source_root", &self.frozen_source_root)
      .field("frozen_source_publication_sequence", &self.frozen_source_publication_sequence)
      .field("diff", &self.diff)
      .field("translated_subtree_count", &self.translated_subtree_count)
      .field("translated_subtree_work_items", &self.translated_subtree_work_items)
      .field("reused_destination_entity_count", &self.reused_destination_entity_count)
      .field("destination_successor_count", &self.destination_successor_count)
      .field("idempotent", &self.idempotent)
      .field("destination_header_sequence", &self.destination_header_sequence)
      .field("destination_namespace_root", &self.destination_namespace_root)
      .field("destination_tree_root", &self.destination_tree_root)
      .finish_non_exhaustive()
  }
}

impl<'freeze, 'source> MigrationFinalNamespaceReconciliationReceiptV1<'freeze, 'source> {
  pub(crate) const fn live_freeze(&self) -> &'freeze MigrationSourceWriteFreezeV1<'source> {
    self.freeze
  }
}

impl MigrationSourceWriteFreezeV1<'_> {
  pub const fn authority(&self) -> &FrozenMigrationSourceAuthorityV1 {
    &self.authority
  }

  pub fn validate_unchanged(&self) -> Result<(), MigrationFinalReconciliationErrorV1> {
    let identity = source_identity(self.source)?;
    let snapshot = self.source.frozen_source_authority_snapshot()?;
    if identity != self.authority.physical_identity || !snapshot_matches(&snapshot, &self.authority) {
      return Err(MigrationFinalReconciliationErrorV1::invalid(
        "migration_final_freeze_authority_changed",
        "source physical identity, header, HEAD, or hard-publication frontier changed while final freeze was held",
      ));
    }
    Ok(())
  }

  pub(crate) fn source(&self) -> &StorageEngine {
    self.source
  }
}

#[derive(Debug)]
pub enum MigrationFinalReconciliationErrorV1 {
  Invalid { code: &'static str, message: String },
  Engine(EngineError),
  Format(FormatError),
  DiffSource(EngineError),
  DiffSink(EngineError),
  Memory(MemoryCoordinatorError),
  Replay(MigrationCaptureReplayErrorV1),
}

impl MigrationFinalReconciliationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Engine(_) => "migration_final_freeze_engine",
      Self::Format(source) => source.code(),
      Self::DiffSource(_) => "migration_final_diff_source",
      Self::DiffSink(_) => "migration_final_diff_sink",
      Self::Memory(_) => "migration_final_diff_memory",
      Self::Replay(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationFinalReconciliationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Engine(source) => Display::fmt(source, formatter),
      Self::Format(source) => Display::fmt(source, formatter),
      Self::DiffSource(source) | Self::DiffSink(source) => Display::fmt(source, formatter),
      Self::Memory(source) => Display::fmt(source, formatter),
      Self::Replay(source) => Display::fmt(source, formatter),
    }
  }
}

impl Error for MigrationFinalReconciliationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Invalid { .. } => None,
      Self::Engine(source) => Some(source),
      Self::Format(source) => Some(source),
      Self::DiffSource(source) | Self::DiffSink(source) => Some(source),
      Self::Memory(source) => Some(source),
      Self::Replay(source) => Some(source),
    }
  }
}

impl From<EngineError> for MigrationFinalReconciliationErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Engine(source)
  }
}

impl From<FormatError> for MigrationFinalReconciliationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<MemoryCoordinatorError> for MigrationFinalReconciliationErrorV1 {
  fn from(source: MemoryCoordinatorError) -> Self {
    Self::Memory(source)
  }
}

impl From<MigrationCaptureReplayErrorV1> for MigrationFinalReconciliationErrorV1 {
  fn from(source: MigrationCaptureReplayErrorV1) -> Self {
    Self::Replay(source)
  }
}

pub fn acquire_migration_source_write_freeze_v1<'a>(
  request: MigrationSourceWriteFreezeRequestV1<'a>,
) -> Result<MigrationSourceWriteFreezeV1<'a>, MigrationFinalReconciliationErrorV1> {
  validate_request(&request)?;
  let before = source_identity(request.source)?;
  if before != request.permit.source_file_identity() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_source_identity",
      "live source file identity differs from the preflight permit",
    ));
  }
  let acquisition_started = Instant::now();
  let exclusive = request
    .source
    .acquire_exclusive_read_only_engine_maintenance("migration_final_write_freeze", request.acquisition_timeout, Some(request.cancellation))
    .map_err(|error| map_acquisition_error(error, acquisition_started, request.acquisition_timeout, request.cancellation))?;
  if request.cancellation.is_cancelled() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_canceled",
      "final source write freeze was canceled after admission",
    ));
  }
  let after = source_identity(request.source)?;
  let snapshot = request.source.frozen_source_authority_snapshot()?;
  if after != before {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_source_replaced",
      "source file identity changed while final write admission was closing",
    ));
  }
  if snapshot.hash_algorithm != request.permit.hash_algorithm()
    || snapshot.header_sequence < request.permit.source_header_sequence()
    || snapshot.namespace_root.len() != request.permit.hash_algorithm().hash_length()
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_source_frontier",
      "frozen source header, hash profile, or HEAD is inconsistent with preflight",
    ));
  }
  let registry = embedded_system_family_registry(snapshot.hash_algorithm)?;
  if registry.operational_fingerprint != request.permit.system_family_registry_fingerprint() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_system_family",
      "frozen source SystemFamily registry differs from preflight",
    ));
  }
  Ok(MigrationSourceWriteFreezeV1 {
    source: request.source,
    authority: FrozenMigrationSourceAuthorityV1 {
      physical_identity: after,
      header_sequence: snapshot.header_sequence,
      namespace_root: snapshot.namespace_root,
      hard_publication_frontier: snapshot.hard_publication_frontier,
      hash_algorithm: snapshot.hash_algorithm,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    _exclusive: exclusive,
  })
}

pub fn execute_final_namespace_reconciliation_v1<'request, 'source>(
  request: MigrationFinalNamespaceReconciliationRequestV1<'request, 'source>,
) -> Result<MigrationFinalNamespaceReconciliationReceiptV1<'request, 'source>, MigrationFinalReconciliationErrorV1> {
  validate_final_reconciliation_request(&request)?;
  request.freeze.validate_unchanged()?;
  let source = request.freeze.source();
  let current_tree = load_destination_tree(request.destination, request.current_destination_tree_root)?;
  let predecessor_namespace_root = namespace_root_for_tree(request.permit.hash_algorithm(), &current_tree, request.authority)?;
  let mut sink = FinalProjectionSinkV1 {
    permit: request.permit,
    source,
    destination: request.destination,
    source_root_after: &request.freeze.authority().namespace_root,
    current_tree,
    memory: request.memory,
    cancellation: request.cancellation,
    publication_timestamp_ms: request.publication_timestamp_ms,
    maximum_subtree_memory_bytes: request.maximum_subtree_memory_bytes,
    maximum_subtree_work_items_per_path: request.maximum_subtree_work_items,
    remaining_subtree_work_items: request.maximum_total_subtree_work_items,
    translated_subtree_work_items: 0,
    maximum_decoded_chunk_bytes: request.maximum_decoded_chunk_bytes,
    maximum_directory_depth: request.maximum_directory_depth,
    translated_subtree_count: 0,
    reused_destination_entity_count: 0,
    failure: None,
  };
  let diff_result = stream_strict_migration_merkle_diff_v1(
    MigrationMerkleDiffRequestV1 {
      source,
      basis_root: request.last_reconciled_source_root,
      target_root: &request.freeze.authority().namespace_root,
      memory: request.memory,
      cancellation: request.cancellation,
      maximum_memory_bytes: request.maximum_diff_memory_bytes,
      maximum_work_items: request.maximum_diff_work_items,
      maximum_directory_depth: request.maximum_directory_depth,
    },
    &mut sink,
  );
  if let Some(error) = sink.failure.take() {
    return Err(error);
  }
  let diff = diff_result?;
  request.freeze.validate_unchanged()?;
  let destination_tree = sink.current_tree;
  let destination_namespace_root = namespace_root_for_tree(request.permit.hash_algorithm(), &destination_tree, request.authority)?;
  let (destination_successor_count, idempotent, destination_header_sequence) =
    if destination_tree.root_hash == request.current_destination_tree_root {
      let observation = request.destination.observe().map_err(MigrationCaptureReplayErrorV1::Publication)?;
      if observation.selected.header.head_hash != destination_namespace_root {
        return Err(MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_destination_divergence",
          "destination HEAD differs from an unchanged final reconciliation tree",
        ));
      }
      (0, true, observation.selected.header.slot_sequence)
    } else {
      let publication = publish_migration_successor_v1(MigrationSuccessorProjectionRequestV1 {
        permit: request.permit,
        destination: request.destination,
        authority: request.authority,
        source_sequence: request.freeze.authority().hard_publication_frontier,
        source_root: &request.freeze.authority().namespace_root,
        expected_head_hash: &predecessor_namespace_root,
        tree: destination_tree.clone(),
        semantic_timestamp_ms: request.publication_timestamp_ms,
        transaction_domain: FINAL_RECONCILIATION_TRANSACTION_DOMAIN,
        closure_domain: FINAL_RECONCILIATION_CLOSURE_DOMAIN,
      })?;
      (u64::from(!publication.idempotent), publication.idempotent, publication.observation.selected.header.slot_sequence)
    };
  request.freeze.validate_unchanged()?;
  let final_observation = request.destination.observe().map_err(MigrationCaptureReplayErrorV1::Publication)?;
  if final_observation.selected.header.head_hash != destination_namespace_root {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_destination_postcondition",
      "selected destination HEAD differs from the reconciled namespace root",
    ));
  }
  Ok(MigrationFinalNamespaceReconciliationReceiptV1 {
    frozen_source_root: request.freeze.authority().namespace_root.clone(),
    frozen_source_publication_sequence: request.freeze.authority().hard_publication_frontier,
    diff,
    translated_subtree_count: sink.translated_subtree_count,
    translated_subtree_work_items: sink.translated_subtree_work_items,
    reused_destination_entity_count: sink.reused_destination_entity_count,
    destination_successor_count,
    idempotent,
    destination_header_sequence,
    destination_namespace_root,
    destination_tree_root: destination_tree.root_hash,
    freeze: request.freeze,
  })
}

struct FinalProjectionSinkV1<'a> {
  permit: &'a MigrationPreflightPermitV1,
  source: &'a StorageEngine,
  destination: &'a V4FirstAuthorityPublisher,
  source_root_after: &'a [u8],
  current_tree: super::first_authority::PreparedNamespaceTreeV0,
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
  publication_timestamp_ms: u64,
  maximum_subtree_memory_bytes: u64,
  maximum_subtree_work_items_per_path: u64,
  remaining_subtree_work_items: u64,
  maximum_decoded_chunk_bytes: usize,
  maximum_directory_depth: usize,
  translated_subtree_count: u64,
  translated_subtree_work_items: u64,
  reused_destination_entity_count: u64,
  failure: Option<MigrationFinalReconciliationErrorV1>,
}

impl MigrationMerkleDiffSinkV1 for FinalProjectionSinkV1<'_> {
  fn record_change(&mut self, change: &MigrationMerkleChangeV1) -> EngineResult<()> {
    let mode = if change.kind == MigrationMerkleChangeKindV1::MetadataOnly {
      MigrationPathProjectionModeV1::ReuseDestinationEntity
    } else {
      MigrationPathProjectionModeV1::TranslateSubtree
    };
    let maximum_subtree_work_items = if mode == MigrationPathProjectionModeV1::TranslateSubtree {
      let available = self.maximum_subtree_work_items_per_path.min(self.remaining_subtree_work_items);
      if available == 0 {
        let error = MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_projection_work_limit",
          "final reconciliation exhausted its aggregate subtree work budget",
        );
        return Err(self.record_failure(error));
      }
      available
    } else {
      self.maximum_subtree_work_items_per_path
    };
    let projected = project_migration_authoritative_path_v1(MigrationPathProjectionRequestV1 {
      permit: self.permit,
      source: self.source,
      destination: self.destination,
      source_root_after: self.source_root_after,
      current_tree: &self.current_tree,
      path: &change.path,
      mode,
      memory: self.memory,
      cancellation: self.cancellation,
      timestamp_ms: self.publication_timestamp_ms,
      maximum_subtree_memory_bytes: self.maximum_subtree_memory_bytes,
      maximum_subtree_work_items,
      maximum_decoded_chunk_bytes: self.maximum_decoded_chunk_bytes,
      maximum_directory_depth: self.maximum_directory_depth,
    });
    match projected {
      Ok(projected) => {
        let Some(remaining) = self.remaining_subtree_work_items.checked_sub(projected.translated_subtree_work_items) else {
          let error = MigrationFinalReconciliationErrorV1::invalid(
            "migration_final_projection_work_accounting",
            "final reconciliation subtree work exceeded its admitted aggregate budget",
          );
          return Err(self.record_failure(error));
        };
        let Some(consumed) = self.translated_subtree_work_items.checked_add(projected.translated_subtree_work_items) else {
          let error = MigrationFinalReconciliationErrorV1::invalid(
            "migration_final_projection_work_accounting",
            "final reconciliation subtree work counter overflowed",
          );
          return Err(self.record_failure(error));
        };
        self.remaining_subtree_work_items = remaining;
        self.translated_subtree_work_items = consumed;
        self.current_tree = projected.tree;
        if mode == MigrationPathProjectionModeV1::ReuseDestinationEntity {
          let Some(next) = self.reused_destination_entity_count.checked_add(1) else {
            let error = MigrationFinalReconciliationErrorV1::invalid(
              "migration_final_projection_counter_overflow",
              "final reconciliation reused-entity counter overflowed",
            );
            return Err(self.record_failure(error));
          };
          self.reused_destination_entity_count = next;
        } else {
          let Some(next) = self.translated_subtree_count.checked_add(1) else {
            let error = MigrationFinalReconciliationErrorV1::invalid(
              "migration_final_projection_counter_overflow",
              "final reconciliation translated-subtree counter overflowed",
            );
            return Err(self.record_failure(error));
          };
          self.translated_subtree_count = next;
        }
        Ok(())
      }
      Err(error) => {
        let message = error.to_string();
        self.failure = Some(MigrationFinalReconciliationErrorV1::Replay(error));
        Err(EngineError::InvalidInput(message))
      }
    }
  }
}

impl FinalProjectionSinkV1<'_> {
  fn record_failure(&mut self, error: MigrationFinalReconciliationErrorV1) -> EngineError {
    let message = error.to_string();
    self.failure = Some(error);
    EngineError::ResourceExhausted(message)
  }
}

fn validate_final_reconciliation_request(
  request: &MigrationFinalNamespaceReconciliationRequestV1<'_, '_>,
) -> Result<(), MigrationFinalReconciliationErrorV1> {
  check_diff_cancelled(request.cancellation)?;
  let authority = request.freeze.authority();
  let width = request.permit.hash_algorithm().hash_length();
  if authority.hash_algorithm != request.permit.hash_algorithm()
    || authority.physical_identity != request.permit.source_file_identity()
    || authority.system_family_registry_fingerprint != request.permit.system_family_registry_fingerprint()
    || request.last_reconciled_source_root.len() != width
    || request.current_destination_tree_root.len() != width
    || is_zero_hash(request.current_destination_tree_root)
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_reconciliation_authority",
      "freeze, permit, source basis, or destination tree identity is inconsistent",
    ));
  }
  if request.authority.base_predecessor_head.len() != width
    || is_zero_hash(&request.authority.base_predecessor_head)
    || request.authority.semantic_state.object_id.len() != width
    || request.authority.typed_closure_context.is_empty()
    || request.authority.authority_identity.is_empty()
    || request.publication_timestamp_ms == 0
    || request.publication_timestamp_ms > i64::MAX as u64
    || request.maximum_subtree_memory_bytes == 0
    || request.maximum_subtree_work_items == 0
    || request.maximum_subtree_work_items > MAXIMUM_DIFF_WORK_ITEMS
    || request.maximum_total_subtree_work_items == 0
    || request.maximum_total_subtree_work_items > MAXIMUM_DIFF_WORK_ITEMS
    || request.maximum_decoded_chunk_bytes == 0
    || request.maximum_decoded_chunk_bytes > MAXIMUM_DIFF_ENTITY_BYTES
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_reconciliation_request",
      "final reconciliation authority, time, or subtree bounds are invalid",
    ));
  }
  let destination = request.destination.observe().map_err(MigrationCaptureReplayErrorV1::Publication)?;
  if destination.selected.header.database_id != request.permit.database_id()
    || destination.selected.header.physical_instance_id != request.permit.destination_physical_instance_id()
    || destination.selected.header.hash_algorithm != request.permit.hash_algorithm()
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_reconciliation_destination",
      "destination first authority differs from the preflight permit",
    ));
  }
  Ok(())
}

pub fn stream_strict_migration_merkle_diff_v1(
  request: MigrationMerkleDiffRequestV1<'_>,
  sink: &mut dyn MigrationMerkleDiffSinkV1,
) -> Result<MigrationMerkleDiffReceiptV1, MigrationFinalReconciliationErrorV1> {
  validate_diff_request(&request)?;
  let mut receipt = MigrationMerkleDiffReceiptV1 {
    basis_root: request.basis_root.to_vec(),
    target_root: request.target_root.to_vec(),
    changed_path_count: 0,
    metadata_only_count: 0,
    visited_directory_count: 0,
    visited_entity_count: 0,
    visited_btree_node_count: 0,
    compared_child_count: 0,
    maximum_memory_used_bytes: 0,
    maximum_frontier_items: 0,
  };
  if request.basis_root == request.target_root {
    return Ok(receipt);
  }

  let mut budget = DiffMemoryBudgetV1::new(request.memory, request.maximum_memory_bytes)?;
  let mut work = DiffWorkBudgetV1::new(request.maximum_work_items);
  let basis_empty = is_zero_hash(request.basis_root);
  let target_empty = is_zero_hash(request.target_root);
  if basis_empty || target_empty {
    work.consume("root transition")?;
    let change = MigrationMerkleChangeV1 {
      path: "/".to_string(),
      kind: if basis_empty { MigrationMerkleChangeKindV1::Added } else { MigrationMerkleChangeKindV1::Removed },
      basis: (!basis_empty).then(|| synthetic_root_entry(request.basis_root)),
      target: (!target_empty).then(|| synthetic_root_entry(request.target_root)),
    };
    emit_diff_change(sink, &mut receipt, &mut budget, change)?;
    receipt.maximum_memory_used_bytes = budget.peak;
    return Ok(receipt);
  }

  let root_charge = diff_directory_work_charge("/", request.basis_root, request.target_root)?;
  budget.reserve(root_charge)?;
  let mut directories = vec![DiffDirectoryWorkV1 {
    path: "/".to_string(),
    basis_hash: request.basis_root.to_vec(),
    target_hash: request.target_root.to_vec(),
    depth: 0,
    memory_charge: root_charge,
  }];
  receipt.maximum_frontier_items = 1;

  while let Some(directory) = directories.pop() {
    check_diff_cancelled(request.cancellation)?;
    work.consume("directory")?;
    receipt.visited_directory_count = increment(receipt.visited_directory_count, "visited directory count")?;
    if directory.depth >= request.maximum_directory_depth {
      return Err(MigrationFinalReconciliationErrorV1::invalid(
        "migration_final_diff_directory_depth",
        format!("directory {} exceeds the caller depth bound", directory.path),
      ));
    }
    if directory.basis_hash != directory.target_hash {
      let mut basis = DirectoryEntryCursorV1::open(
        request.source,
        &directory.basis_hash,
        request.source.hash_algorithm(),
        &mut budget,
        &mut work,
        &mut receipt,
      )?;
      let mut target = DirectoryEntryCursorV1::open(
        request.source,
        &directory.target_hash,
        request.source.hash_algorithm(),
        &mut budget,
        &mut work,
        &mut receipt,
      )?;
      let mut basis_child =
        basis.next(request.source, request.source.hash_algorithm(), request.cancellation, &mut budget, &mut work, &mut receipt)?;
      let mut target_child =
        target.next(request.source, request.source.hash_algorithm(), request.cancellation, &mut budget, &mut work, &mut receipt)?;

      while basis_child.is_some() || target_child.is_some() {
        check_diff_cancelled(request.cancellation)?;
        work.consume("directory child comparison")?;
        receipt.compared_child_count = increment(receipt.compared_child_count, "compared child count")?;
        match child_order(basis_child.as_ref(), target_child.as_ref()) {
          std::cmp::Ordering::Less => {
            let removed = take_diff_child(&mut basis_child, "basis")?;
            let path = join_diff_path(&directory.path, &removed.entry.name)?;
            emit_diff_change(
              sink,
              &mut receipt,
              &mut budget,
              MigrationMerkleChangeV1 { path, kind: MigrationMerkleChangeKindV1::Removed, basis: Some(removed.entry), target: None },
            )?;
            budget.release(removed.memory_charge)?;
            basis_child =
              basis.next(request.source, request.source.hash_algorithm(), request.cancellation, &mut budget, &mut work, &mut receipt)?;
          }
          std::cmp::Ordering::Greater => {
            let added = take_diff_child(&mut target_child, "target")?;
            let path = join_diff_path(&directory.path, &added.entry.name)?;
            emit_diff_change(
              sink,
              &mut receipt,
              &mut budget,
              MigrationMerkleChangeV1 { path, kind: MigrationMerkleChangeKindV1::Added, basis: None, target: Some(added.entry) },
            )?;
            budget.release(added.memory_charge)?;
            target_child =
              target.next(request.source, request.source.hash_algorithm(), request.cancellation, &mut budget, &mut work, &mut receipt)?;
          }
          std::cmp::Ordering::Equal => {
            let basis_value = take_diff_child(&mut basis_child, "basis")?;
            let target_value = take_diff_child(&mut target_child, "target")?;
            let path = join_diff_path(&directory.path, &basis_value.entry.name)?;
            compare_matching_diff_children(MatchingDiffChildrenV1 {
              sink,
              directories: &mut directories,
              receipt: &mut receipt,
              budget: &mut budget,
              path: &path,
              depth: directory.depth + 1,
              basis: &basis_value.entry,
              target: &target_value.entry,
            })?;
            budget.release(basis_value.memory_charge)?;
            budget.release(target_value.memory_charge)?;
            basis_child =
              basis.next(request.source, request.source.hash_algorithm(), request.cancellation, &mut budget, &mut work, &mut receipt)?;
            target_child =
              target.next(request.source, request.source.hash_algorithm(), request.cancellation, &mut budget, &mut work, &mut receipt)?;
          }
        }
        receipt.maximum_frontier_items = receipt.maximum_frontier_items.max(
          u64::try_from(directories.len())
            .map_err(|error| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_frontier", error.to_string()))?,
        );
      }
    }
    budget.release(directory.memory_charge)?;
  }

  receipt.maximum_memory_used_bytes = budget.peak;
  Ok(receipt)
}

pub(super) fn count_strict_migration_tree_symlinks_v1(
  source: &dyn MigrationBaseCloneEntrySourceV1,
  root: &[u8],
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  maximum_memory_bytes: u64,
  maximum_work_items: u64,
  maximum_directory_depth: usize,
) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  let algorithm = source.hash_algorithm();
  if root.len() != algorithm.hash_length()
    || is_zero_hash(root)
    || maximum_memory_bytes == 0
    || maximum_memory_bytes > MAXIMUM_DIFF_MEMORY_BYTES
    || maximum_work_items == 0
    || maximum_work_items > MAXIMUM_DIFF_WORK_ITEMS
    || maximum_directory_depth == 0
    || maximum_directory_depth > MAXIMUM_DIFF_DIRECTORY_DEPTH
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_v3_inventory_namespace_bounds",
      "v3 inventory namespace root or resource bounds are invalid",
    ));
  }
  check_diff_cancelled(cancellation)?;
  let mut budget = DiffMemoryBudgetV1::new(memory, maximum_memory_bytes)?;
  let mut work = DiffWorkBudgetV1::new(maximum_work_items);
  let mut receipt = MigrationMerkleDiffReceiptV1 {
    basis_root: Vec::new(),
    target_root: root.to_vec(),
    changed_path_count: 0,
    metadata_only_count: 0,
    visited_directory_count: 0,
    visited_entity_count: 0,
    visited_btree_node_count: 0,
    compared_child_count: 0,
    maximum_memory_used_bytes: 0,
    maximum_frontier_items: 0,
  };
  let initial_charge = diff_directory_frontier_charge(root.len())?;
  budget.reserve(initial_charge)?;
  let mut directories = vec![(root.to_vec(), 0usize, initial_charge)];
  let mut symlinks = 0u64;
  while let Some((directory_hash, depth, frontier_charge)) = directories.pop() {
    budget.release(frontier_charge)?;
    check_diff_cancelled(cancellation)?;
    if depth >= maximum_directory_depth {
      return Err(MigrationFinalReconciliationErrorV1::invalid(
        "migration_v3_inventory_namespace_depth",
        "v3 inventory namespace exceeds its configured directory-depth bound",
      ));
    }
    let mut cursor = DirectoryEntryCursorV1::open(source, &directory_hash, algorithm, &mut budget, &mut work, &mut receipt)?;
    while let Some(child) = cursor.next(source, algorithm, cancellation, &mut budget, &mut work, &mut receipt)? {
      work.consume("v3 inventory namespace child")?;
      match EntryType::from_u8(child.entry.entry_type).map_err(MigrationFinalReconciliationErrorV1::DiffSource)? {
        EntryType::Symlink => {
          symlinks = symlinks.checked_add(1).ok_or_else(|| {
            MigrationFinalReconciliationErrorV1::invalid("migration_v3_inventory_symlink_overflow", "v3 inventory symlink count overflowed")
          })?;
        }
        EntryType::DirectoryIndex => {
          let child_depth = depth.checked_add(1).ok_or_else(|| {
            MigrationFinalReconciliationErrorV1::invalid(
              "migration_v3_inventory_namespace_depth",
              "v3 inventory directory depth overflowed",
            )
          })?;
          if child_depth >= maximum_directory_depth {
            return Err(MigrationFinalReconciliationErrorV1::invalid(
              "migration_v3_inventory_namespace_depth",
              "v3 inventory namespace exceeds its configured directory-depth bound",
            ));
          }
          let charge = diff_directory_frontier_charge(child.entry.hash.len())?;
          budget.reserve(charge)?;
          directories.try_reserve(1).map_err(|error| {
            MigrationFinalReconciliationErrorV1::invalid(
              "migration_v3_inventory_namespace_allocation",
              format!("v3 inventory directory frontier allocation failed: {error}"),
            )
          })?;
          directories.push((child.entry.hash.clone(), child_depth, charge));
        }
        _ => {}
      }
      budget.release(child.memory_charge)?;
    }
  }
  Ok(symlinks)
}

fn diff_directory_frontier_charge(hash_bytes: usize) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  let bytes = size_of::<(Vec<u8>, usize, u64)>()
    .checked_add(hash_bytes)
    .and_then(|value| value.checked_add(OWNED_ALLOCATION_OVERHEAD as usize))
    .ok_or_else(|| {
      MigrationFinalReconciliationErrorV1::invalid(
        "migration_v3_inventory_namespace_memory_overflow",
        "v3 inventory directory frontier memory charge overflowed",
      )
    })?;
  diff_usize_to_u64(bytes, "v3 inventory directory frontier charge")
}

struct DiffMemoryBudgetV1 {
  _reservation: MemoryReservation,
  maximum: u64,
  used: u64,
  peak: u64,
}

impl DiffMemoryBudgetV1 {
  fn new(memory: &MemoryCoordinator, maximum: u64) -> Result<Self, MigrationFinalReconciliationErrorV1> {
    let reservation = memory.reserve(MemoryOwner::Migration, maximum, AdmissionClass::Maintenance)?;
    Ok(Self { _reservation: reservation, maximum, used: 0, peak: 0 })
  }

  fn reserve(&mut self, bytes: u64) -> Result<(), MigrationFinalReconciliationErrorV1> {
    let next = self.used.checked_add(bytes).ok_or_else(|| {
      MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "diff memory accounting overflow")
    })?;
    if next > self.maximum {
      return Err(MigrationFinalReconciliationErrorV1::invalid(
        "migration_final_diff_memory_limit",
        format!("strict Merkle diff requires {next} bytes but its bound is {}", self.maximum),
      ));
    }
    self.used = next;
    self.peak = self.peak.max(next);
    Ok(())
  }

  fn release(&mut self, bytes: u64) -> Result<(), MigrationFinalReconciliationErrorV1> {
    self.used = self.used.checked_sub(bytes).ok_or_else(|| {
      MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_underflow", "diff memory accounting underflow")
    })?;
    Ok(())
  }
}

struct DiffWorkBudgetV1 {
  maximum: u64,
  used: u64,
}

impl DiffWorkBudgetV1 {
  const fn new(maximum: u64) -> Self {
    Self { maximum, used: 0 }
  }

  fn consume(&mut self, item: &'static str) -> Result<(), MigrationFinalReconciliationErrorV1> {
    self.used = self
      .used
      .checked_add(1)
      .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_work_overflow", "diff work counter overflow"))?;
    if self.used > self.maximum {
      return Err(MigrationFinalReconciliationErrorV1::invalid(
        "migration_final_diff_work_limit",
        format!("strict Merkle diff exceeded its work bound while processing {item}"),
      ));
    }
    Ok(())
  }
}

struct DiffDirectoryWorkV1 {
  path: String,
  basis_hash: Vec<u8>,
  target_hash: Vec<u8>,
  depth: usize,
  memory_charge: u64,
}

struct BudgetedChildEntryV1 {
  entry: ChildEntry,
  memory_charge: u64,
}

enum DirectoryEntryCursorV1 {
  Empty,
  Flat {
    value: Vec<u8>,
    offset: usize,
    entry_version: u8,
    entry_count: usize,
    previous_name: Option<String>,
    previous_name_memory_charge: u64,
    memory_charge: u64,
  },
  BTree {
    frames: Vec<DiffBtreeFrameV1>,
  },
}

enum DiffBtreeFrameV1 {
  Leaf {
    hash: Vec<u8>,
    entries: Vec<ChildEntry>,
    index: usize,
    memory_charge: u64,
  },
  Internal {
    hash: Vec<u8>,
    keys: Vec<String>,
    children: Vec<Vec<u8>>,
    next_child: usize,
    lower_bound: Option<String>,
    upper_bound: Option<String>,
    memory_charge: u64,
  },
}

impl DirectoryEntryCursorV1 {
  fn open(
    source: &dyn MigrationBaseCloneEntrySourceV1,
    root: &[u8],
    algorithm: HashAlgorithm,
    budget: &mut DiffMemoryBudgetV1,
    work: &mut DiffWorkBudgetV1,
    receipt: &mut MigrationMerkleDiffReceiptV1,
  ) -> Result<Self, MigrationFinalReconciliationErrorV1> {
    let loaded = load_diff_directory(source, root, algorithm, budget, work, receipt)?;
    if !is_btree_format(&loaded.value) {
      return Ok(Self::Flat {
        value: loaded.value,
        offset: 0,
        entry_version: loaded.header.entry_version,
        entry_count: 0,
        previous_name: None,
        previous_name_memory_charge: 0,
        memory_charge: loaded.memory_charge,
      });
    }
    let mut cursor = Self::BTree { frames: Vec::new() };
    cursor.descend_loaded_btree(source, root.to_vec(), loaded, None, None, algorithm, budget, work, receipt)?;
    Ok(cursor)
  }

  #[allow(clippy::too_many_arguments)]
  fn next(
    &mut self,
    source: &dyn MigrationBaseCloneEntrySourceV1,
    algorithm: HashAlgorithm,
    cancellation: &CancellationToken,
    budget: &mut DiffMemoryBudgetV1,
    work: &mut DiffWorkBudgetV1,
    receipt: &mut MigrationMerkleDiffReceiptV1,
  ) -> Result<Option<BudgetedChildEntryV1>, MigrationFinalReconciliationErrorV1> {
    loop {
      check_diff_cancelled(cancellation)?;
      match self {
        Self::Empty => return Ok(None),
        Self::Flat { value, offset, entry_version, entry_count, previous_name, previous_name_memory_charge, memory_charge } => {
          if *offset == value.len() {
            let retained = memory_charge.checked_add(*previous_name_memory_charge).ok_or_else(|| {
              MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "flat cursor release charge overflow")
            })?;
            budget.release(retained)?;
            *self = Self::Empty;
            return Ok(None);
          }
          if *entry_count >= BTREE_CONVERSION_THRESHOLD {
            return Err(MigrationFinalReconciliationErrorV1::invalid(
              "migration_final_diff_flat_count",
              format!("flat directory exceeds the bounded {BTREE_CONVERSION_THRESHOLD}-entry compatibility limit"),
            ));
          }
          let (entry, consumed) = ChildEntry::deserialize(&value[*offset..], algorithm.hash_length(), *entry_version)
            .map_err(MigrationFinalReconciliationErrorV1::DiffSource)?;
          if consumed == 0 {
            return Err(MigrationFinalReconciliationErrorV1::invalid(
              "migration_final_diff_flat_zero_progress",
              "flat directory child consumed zero bytes",
            ));
          }
          *offset = offset.checked_add(consumed).ok_or_else(|| {
            MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_flat_offset", "flat directory offset overflow")
          })?;
          *entry_count = entry_count.checked_add(1).ok_or_else(|| {
            MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_flat_count", "flat directory entry count overflow")
          })?;
          validate_diff_child(&entry, algorithm.hash_length())?;
          if previous_name.as_deref().is_some_and(|previous| previous >= entry.name.as_str()) {
            return Err(MigrationFinalReconciliationErrorV1::invalid(
              "migration_final_diff_flat_order",
              "flat directory entries are not in strict name order",
            ));
          }
          let next_previous_charge = diff_string_memory_charge(&entry.name, "flat previous name")?;
          budget.reserve(next_previous_charge)?;
          let prior_charge = std::mem::replace(previous_name_memory_charge, next_previous_charge);
          *previous_name = Some(entry.name.clone());
          budget.release(prior_charge)?;
          return budget_child(entry, budget);
        }
        Self::BTree { frames } => {
          enum CursorAction {
            Yield(ChildEntry),
            Descend { hash: Vec<u8>, lower: Option<String>, upper: Option<String> },
            Pop(u64),
          }
          let action = match frames.last_mut() {
            None => return Ok(None),
            Some(DiffBtreeFrameV1::Leaf { entries, index, memory_charge, .. }) => {
              if *index < entries.len() {
                let entry = entries[*index].clone();
                *index += 1;
                CursorAction::Yield(entry)
              } else {
                CursorAction::Pop(*memory_charge)
              }
            }
            Some(DiffBtreeFrameV1::Internal { keys, children, next_child, lower_bound, upper_bound, memory_charge, .. }) => {
              if *next_child < children.len() {
                let index = *next_child;
                *next_child += 1;
                CursorAction::Descend {
                  hash: children[index].clone(),
                  lower: if index == 0 { lower_bound.clone() } else { Some(keys[index - 1].clone()) },
                  upper: if index == keys.len() { upper_bound.clone() } else { Some(keys[index].clone()) },
                }
              } else {
                CursorAction::Pop(*memory_charge)
              }
            }
          };
          match action {
            CursorAction::Yield(entry) => return budget_child(entry, budget),
            CursorAction::Pop(charge) => {
              frames.pop();
              budget.release(charge)?;
            }
            CursorAction::Descend { hash, lower, upper } => {
              self.descend_btree(source, hash, lower, upper, algorithm, budget, work, receipt)?;
            }
          }
        }
      }
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn descend_btree(
    &mut self,
    source: &dyn MigrationBaseCloneEntrySourceV1,
    hash: Vec<u8>,
    lower: Option<String>,
    upper: Option<String>,
    algorithm: HashAlgorithm,
    budget: &mut DiffMemoryBudgetV1,
    work: &mut DiffWorkBudgetV1,
    receipt: &mut MigrationMerkleDiffReceiptV1,
  ) -> Result<(), MigrationFinalReconciliationErrorV1> {
    let loaded = load_diff_directory(source, &hash, algorithm, budget, work, receipt)?;
    self.descend_loaded_btree(source, hash, loaded, lower, upper, algorithm, budget, work, receipt)
  }

  #[allow(clippy::too_many_arguments)]
  fn descend_loaded_btree(
    &mut self,
    _source: &dyn MigrationBaseCloneEntrySourceV1,
    mut hash: Vec<u8>,
    mut loaded: LoadedDiffDirectoryV1,
    mut lower: Option<String>,
    mut upper: Option<String>,
    algorithm: HashAlgorithm,
    budget: &mut DiffMemoryBudgetV1,
    work: &mut DiffWorkBudgetV1,
    receipt: &mut MigrationMerkleDiffReceiptV1,
  ) -> Result<(), MigrationFinalReconciliationErrorV1> {
    loop {
      let Self::BTree { frames } = self else {
        return Err(MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_diff_cursor_state",
          "B-tree descent used a flat directory cursor",
        ));
      };
      if frames.len() >= MAXIMUM_DIFF_BTREE_DEPTH || frames.iter().any(|frame| frame.hash() == hash) {
        return Err(MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_diff_btree_cycle_or_depth",
          format!("B-tree ancestry is invalid at {}", hex::encode(&hash)),
        ));
      }
      work.consume("B-tree node")?;
      receipt.visited_btree_node_count = increment(receipt.visited_btree_node_count, "visited B-tree node count")?;
      let loaded_bytes = diff_usize_to_u64(loaded.value.len(), "B-tree value length")?;
      let frame_bytes = diff_usize_to_u64(size_of::<DiffBtreeFrameV1>(), "B-tree frame size")?;
      let parse_charge = loaded_bytes
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
        .and_then(|bytes| bytes.checked_add(frame_bytes))
        .ok_or_else(|| {
          MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "B-tree parse charge overflow")
        })?;
      budget.reserve(parse_charge)?;
      let node = BTreeNode::deserialize(&loaded.value, algorithm.hash_length(), loaded.header.entry_version)
        .map_err(MigrationFinalReconciliationErrorV1::DiffSource)?;
      validate_diff_btree_node(&node, lower.as_deref(), upper.as_deref(), algorithm.hash_length())?;
      if node.serialize(algorithm.hash_length()).map_err(MigrationFinalReconciliationErrorV1::DiffSource)? != loaded.value {
        return Err(MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_diff_btree_noncanonical",
          format!("B-tree node {} contains trailing or noncanonical bytes", hex::encode(&hash)),
        ));
      }
      budget.release(loaded.memory_charge)?;
      match node {
        BTreeNode::Leaf(leaf) => {
          frames.push(DiffBtreeFrameV1::Leaf { hash, entries: leaf.entries, index: 0, memory_charge: parse_charge });
          return Ok(());
        }
        BTreeNode::Internal(internal) => {
          let child = internal.children.first().cloned().ok_or_else(|| {
            MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_btree_internal", "B-tree internal node has no first child")
          })?;
          let child_lower = lower.clone();
          let child_upper = match internal.keys.first() {
            Some(key) => Some(key.clone()),
            None => upper.clone(),
          };
          frames.push(DiffBtreeFrameV1::Internal {
            hash,
            keys: internal.keys,
            children: internal.children,
            next_child: 1,
            lower_bound: lower,
            upper_bound: upper,
            memory_charge: parse_charge,
          });
          hash = child;
          lower = child_lower;
          upper = child_upper;
          loaded = load_diff_directory(_source, &hash, algorithm, budget, work, receipt)?;
        }
      }
    }
  }
}

impl DiffBtreeFrameV1 {
  fn hash(&self) -> &[u8] {
    match self {
      Self::Leaf { hash, .. } | Self::Internal { hash, .. } => hash,
    }
  }
}

struct LoadedDiffDirectoryV1 {
  header: EntryHeader,
  value: Vec<u8>,
  memory_charge: u64,
}

fn load_diff_directory(
  source: &dyn MigrationBaseCloneEntrySourceV1,
  hash: &[u8],
  algorithm: HashAlgorithm,
  budget: &mut DiffMemoryBudgetV1,
  work: &mut DiffWorkBudgetV1,
  receipt: &mut MigrationMerkleDiffReceiptV1,
) -> Result<LoadedDiffDirectoryV1, MigrationFinalReconciliationErrorV1> {
  work.consume("directory entity")?;
  receipt.visited_entity_count = increment(receipt.visited_entity_count, "visited entity count")?;
  let header = source
    .historical_entry_header(hash)
    .map_err(MigrationFinalReconciliationErrorV1::DiffSource)?
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_missing_entity", hex::encode(hash)))?;
  if header.entry_type != EntryType::DirectoryIndex
    || header.flags != 0
    || header.hash_algo != algorithm
    || header.compression_algo != CompressionAlgorithm::None
    || header.encryption_algo != 0
    || header.key_length as usize != algorithm.hash_length()
    || header.value_length as usize > MAXIMUM_DIFF_ENTITY_BYTES
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_directory_header",
      format!("directory entity {} has a noncanonical header", hex::encode(hash)),
    ));
  }
  let hash_bytes = diff_usize_to_u64(algorithm.hash_length(), "hash length")?;
  let memory_charge = u64::from(header.value_length)
    .checked_add(hash_bytes)
    .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "loaded entity charge overflow"))?;
  budget.reserve(memory_charge)?;
  let (actual, key, value) = source
    .historical_entry_verified_bounded(hash, header.value_length)
    .map_err(MigrationFinalReconciliationErrorV1::DiffSource)?
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_missing_entity", hex::encode(hash)))?;
  if key != hash
    || actual.entry_version != header.entry_version
    || actual.entry_type != header.entry_type
    || actual.flags != header.flags
    || actual.hash_algo != header.hash_algo
    || actual.compression_algo != header.compression_algo
    || actual.encryption_algo != header.encryption_algo
    || actual.key_length != header.key_length
    || actual.value_length != header.value_length
    || actual.total_length != header.total_length
    || actual.hash != header.hash
    || value.len() != header.value_length as usize
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_source_changed",
      format!("directory entity {} changed between bounded reads", hex::encode(hash)),
    ));
  }
  let domain = if is_btree_format(&value) { b"btree:".as_slice() } else { b"dirc:".as_slice() };
  if digest_parts(algorithm, &[domain, &value]) != hash {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_directory_identity",
      format!("directory entity {} does not match its canonical content identity", hex::encode(hash)),
    ));
  }
  Ok(LoadedDiffDirectoryV1 { header: actual, value, memory_charge })
}

struct MatchingDiffChildrenV1<'a> {
  sink: &'a mut dyn MigrationMerkleDiffSinkV1,
  directories: &'a mut Vec<DiffDirectoryWorkV1>,
  receipt: &'a mut MigrationMerkleDiffReceiptV1,
  budget: &'a mut DiffMemoryBudgetV1,
  path: &'a str,
  depth: usize,
  basis: &'a ChildEntry,
  target: &'a ChildEntry,
}

fn compare_matching_diff_children(request: MatchingDiffChildrenV1<'_>) -> Result<(), MigrationFinalReconciliationErrorV1> {
  let MatchingDiffChildrenV1 { sink, directories, receipt, budget, path, depth, basis, target } = request;
  if basis == target {
    return Ok(());
  }
  let basis_type = EntryType::from_u8(basis.entry_type).map_err(MigrationFinalReconciliationErrorV1::DiffSource)?;
  let target_type = EntryType::from_u8(target.entry_type).map_err(MigrationFinalReconciliationErrorV1::DiffSource)?;
  let same_identity = basis_type == target_type && basis.hash == target.hash;
  if basis_type == EntryType::DirectoryIndex && target_type == EntryType::DirectoryIndex {
    if !diff_metadata_equal(basis, target) {
      emit_diff_change(
        sink,
        receipt,
        budget,
        MigrationMerkleChangeV1 {
          path: path.to_string(),
          kind: MigrationMerkleChangeKindV1::MetadataOnly,
          basis: Some(basis.clone()),
          target: Some(target.clone()),
        },
      )?;
    }
    if !same_identity {
      let charge = diff_directory_work_charge(path, &basis.hash, &target.hash)?;
      budget.reserve(charge)?;
      directories.push(DiffDirectoryWorkV1 {
        path: path.to_string(),
        basis_hash: basis.hash.clone(),
        target_hash: target.hash.clone(),
        depth,
        memory_charge: charge,
      });
    }
    return Ok(());
  }
  emit_diff_change(
    sink,
    receipt,
    budget,
    MigrationMerkleChangeV1 {
      path: path.to_string(),
      kind: if same_identity { MigrationMerkleChangeKindV1::MetadataOnly } else { MigrationMerkleChangeKindV1::Replaced },
      basis: Some(basis.clone()),
      target: Some(target.clone()),
    },
  )
}

fn emit_diff_change(
  sink: &mut dyn MigrationMerkleDiffSinkV1,
  receipt: &mut MigrationMerkleDiffReceiptV1,
  budget: &mut DiffMemoryBudgetV1,
  change: MigrationMerkleChangeV1,
) -> Result<(), MigrationFinalReconciliationErrorV1> {
  let change_charge = diff_change_memory_charge(&change)?;
  budget.reserve(change_charge)?;
  let metadata_only = change.kind == MigrationMerkleChangeKindV1::MetadataOnly;
  sink.record_change(&change).map_err(MigrationFinalReconciliationErrorV1::DiffSink)?;
  drop(change);
  budget.release(change_charge)?;
  receipt.changed_path_count = increment(receipt.changed_path_count, "changed path count")?;
  if metadata_only {
    receipt.metadata_only_count = increment(receipt.metadata_only_count, "metadata-only count")?;
  }
  Ok(())
}

fn child_order(left: Option<&BudgetedChildEntryV1>, right: Option<&BudgetedChildEntryV1>) -> std::cmp::Ordering {
  match (left, right) {
    (Some(left), Some(right)) => left.entry.name.cmp(&right.entry.name),
    (Some(_), None) => std::cmp::Ordering::Less,
    (None, Some(_)) => std::cmp::Ordering::Greater,
    (None, None) => std::cmp::Ordering::Equal,
  }
}

fn budget_child(
  entry: ChildEntry,
  budget: &mut DiffMemoryBudgetV1,
) -> Result<Option<BudgetedChildEntryV1>, MigrationFinalReconciliationErrorV1> {
  let memory_charge = diff_child_memory_charge(&entry)?;
  budget.reserve(memory_charge)?;
  Ok(Some(BudgetedChildEntryV1 { entry, memory_charge }))
}

fn diff_child_memory_charge(entry: &ChildEntry) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  let child_bytes = diff_usize_to_u64(size_of::<ChildEntry>(), "child entry size")?;
  let hash_bytes = diff_usize_to_u64(entry.hash.len(), "child hash length")?;
  let name_bytes = diff_usize_to_u64(entry.name.len(), "child name length")?;
  let content_type_bytes = diff_usize_to_u64(entry.content_type.as_ref().map_or(0, String::len), "child content-type length")?;
  child_bytes
    .checked_add(hash_bytes)
    .and_then(|bytes| bytes.checked_add(name_bytes))
    .and_then(|bytes| bytes.checked_add(content_type_bytes))
    .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "child charge overflow"))
}

fn diff_string_memory_charge(value: &str, name: &'static str) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  diff_usize_to_u64(size_of::<String>(), "string descriptor size")?
    .checked_add(diff_usize_to_u64(value.len(), name)?)
    .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", format!("{name} charge overflow")))
}

fn diff_change_memory_charge(change: &MigrationMerkleChangeV1) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  let mut charge = diff_usize_to_u64(size_of::<MigrationMerkleChangeV1>(), "Merkle change size")?
    .checked_add(diff_usize_to_u64(change.path.len(), "Merkle change path length")?)
    .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "Merkle change charge overflow"))?;
  for entry in [change.basis.as_ref(), change.target.as_ref()].into_iter().flatten() {
    charge = charge.checked_add(diff_child_memory_charge(entry)?).ok_or_else(|| {
      MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "Merkle change charge overflow")
    })?;
  }
  Ok(charge)
}

fn validate_diff_request(request: &MigrationMerkleDiffRequestV1<'_>) -> Result<(), MigrationFinalReconciliationErrorV1> {
  let width = request.source.hash_algorithm().hash_length();
  if request.basis_root.len() != width || request.target_root.len() != width {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_root_width",
      "basis and target roots must match the source hash width",
    ));
  }
  if request.maximum_memory_bytes == 0
    || request.maximum_memory_bytes > MAXIMUM_DIFF_MEMORY_BYTES
    || request.maximum_work_items == 0
    || request.maximum_work_items > MAXIMUM_DIFF_WORK_ITEMS
    || request.maximum_directory_depth == 0
    || request.maximum_directory_depth > MAXIMUM_DIFF_DIRECTORY_DEPTH
  {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_bounds",
      "strict Merkle diff memory, work, or directory-depth bound is invalid",
    ));
  }
  check_diff_cancelled(request.cancellation)
}

fn validate_diff_child(entry: &ChildEntry, hash_width: usize) -> Result<(), MigrationFinalReconciliationErrorV1> {
  if entry.name.is_empty() || matches!(entry.name.as_str(), "." | "..") || entry.name.contains('/') || entry.name.contains('\0') {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_child_name",
      format!("directory child name {:?} is not canonical", entry.name),
    ));
  }
  if entry.hash.len() != hash_width || entry.hash.iter().all(|byte| *byte == 0) {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_child_hash",
      format!("directory child {:?} has an invalid content identity", entry.name),
    ));
  }
  EntryType::from_u8(entry.entry_type).map_err(MigrationFinalReconciliationErrorV1::DiffSource)?;
  Ok(())
}

fn validate_diff_btree_node(
  node: &BTreeNode,
  lower_bound: Option<&str>,
  upper_bound: Option<&str>,
  hash_width: usize,
) -> Result<(), MigrationFinalReconciliationErrorV1> {
  if lower_bound.zip(upper_bound).is_some_and(|(lower, upper)| lower >= upper) {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_btree_range",
      "B-tree inherited separator range is empty or reversed",
    ));
  }
  match node {
    BTreeNode::Leaf(leaf) => {
      if leaf.entries.len() > BTREE_MAX_LEAF_ENTRIES || !strict_diff_order(leaf.entries.iter().map(|entry| entry.name.as_str())) {
        return Err(MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_diff_btree_leaf",
          "B-tree leaf count or ordering is invalid",
        ));
      }
      for entry in &leaf.entries {
        validate_diff_child(entry, hash_width)?;
        if !within_diff_btree_range(&entry.name, lower_bound, upper_bound) {
          return Err(MigrationFinalReconciliationErrorV1::invalid(
            "migration_final_diff_btree_range",
            "B-tree leaf entry is outside its inherited range",
          ));
        }
      }
    }
    BTreeNode::Internal(internal) => {
      if internal.keys.len() > BTREE_MAX_INTERNAL_KEYS
        || internal.children.len() != internal.keys.len() + 1
        || !strict_diff_order(internal.keys.iter().map(String::as_str))
      {
        return Err(MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_diff_btree_internal",
          "B-tree internal count or ordering is invalid",
        ));
      }
      if internal.children.iter().enumerate().any(|(index, child)| {
        child.len() != hash_width
          || child.iter().all(|byte| *byte == 0)
          || internal.children[index + 1..].iter().any(|candidate| candidate == child)
      }) {
        return Err(MigrationFinalReconciliationErrorV1::invalid(
          "migration_final_diff_btree_child",
          "B-tree internal node contains an invalid or duplicate child locator",
        ));
      }
      for key in &internal.keys {
        if key.is_empty() || matches!(key.as_str(), "." | "..") || key.contains('/') || key.contains('\0') {
          return Err(MigrationFinalReconciliationErrorV1::invalid(
            "migration_final_diff_btree_separator",
            "B-tree separator is not one canonical path segment",
          ));
        }
        if !within_diff_btree_range(key, lower_bound, upper_bound) {
          return Err(MigrationFinalReconciliationErrorV1::invalid(
            "migration_final_diff_btree_range",
            "B-tree separator is outside its inherited range",
          ));
        }
      }
    }
  }
  Ok(())
}

fn strict_diff_order<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
  let Some(mut previous) = values.next() else {
    return true;
  };
  for value in values {
    if previous >= value {
      return false;
    }
    previous = value;
  }
  true
}

fn within_diff_btree_range(value: &str, lower: Option<&str>, upper: Option<&str>) -> bool {
  lower.is_none_or(|bound| value >= bound) && upper.is_none_or(|bound| value < bound)
}

fn diff_metadata_equal(left: &ChildEntry, right: &ChildEntry) -> bool {
  left.entry_type == right.entry_type
    && left.total_size == right.total_size
    && left.created_at == right.created_at
    && left.updated_at == right.updated_at
    && left.name == right.name
    && left.content_type == right.content_type
    && left.virtual_time == right.virtual_time
    && left.node_id == right.node_id
}

fn join_diff_path(parent: &str, name: &str) -> Result<String, MigrationFinalReconciliationErrorV1> {
  let path = if parent == "/" { format!("/{name}") } else { format!("{parent}/{name}") };
  if path.len() > 1024 * 1024 {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_path_length",
      "strict Merkle diff path exceeds the migration path bound",
    ));
  }
  Ok(path)
}

fn diff_directory_work_charge(path: &str, basis: &[u8], target: &[u8]) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  let work_bytes = diff_usize_to_u64(size_of::<DiffDirectoryWorkV1>(), "directory work size")?;
  let path_bytes = diff_usize_to_u64(path.len(), "directory path length")?;
  let basis_bytes = diff_usize_to_u64(basis.len(), "basis hash length")?;
  let target_bytes = diff_usize_to_u64(target.len(), "target hash length")?;
  work_bytes
    .checked_add(path_bytes)
    .and_then(|bytes| bytes.checked_add(basis_bytes))
    .and_then(|bytes| bytes.checked_add(target_bytes))
    .and_then(|bytes| bytes.checked_add(OWNED_ALLOCATION_OVERHEAD))
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", "directory work charge overflow"))
}

fn take_diff_child(
  slot: &mut Option<BudgetedChildEntryV1>,
  side: &'static str,
) -> Result<BudgetedChildEntryV1, MigrationFinalReconciliationErrorV1> {
  slot.take().ok_or_else(|| {
    MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_diff_cursor_state",
      format!("directory ordering selected an absent {side} child"),
    )
  })
}

fn diff_usize_to_u64(value: usize, name: &'static str) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  u64::try_from(value).map_err(|error| {
    MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_memory_overflow", format!("{name} does not fit u64: {error}"))
  })
}

fn synthetic_root_entry(hash: &[u8]) -> ChildEntry {
  ChildEntry {
    entry_type: EntryType::DirectoryIndex.to_u8(),
    hash: hash.to_vec(),
    total_size: 0,
    created_at: 0,
    updated_at: 0,
    name: String::new(),
    content_type: None,
    virtual_time: 0,
    node_id: 0,
  }
}

fn check_diff_cancelled(cancellation: &CancellationToken) -> Result<(), MigrationFinalReconciliationErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_cancelled", "strict Merkle diff was canceled"));
  }
  Ok(())
}

fn is_zero_hash(hash: &[u8]) -> bool {
  hash.iter().all(|byte| *byte == 0)
}

fn increment(value: u64, name: &'static str) -> Result<u64, MigrationFinalReconciliationErrorV1> {
  value
    .checked_add(1)
    .ok_or_else(|| MigrationFinalReconciliationErrorV1::invalid("migration_final_diff_counter_overflow", format!("{name} overflow")))
}

fn validate_request(request: &MigrationSourceWriteFreezeRequestV1<'_>) -> Result<(), MigrationFinalReconciliationErrorV1> {
  if request.acquisition_timeout.is_zero() || request.acquisition_timeout > MAXIMUM_FREEZE_ACQUISITION_TIMEOUT {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_timeout",
      "final source write-freeze acquisition timeout must be within (0, 24 hours]",
    ));
  }
  if request.cancellation.is_cancelled() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_canceled",
      "final source write freeze was canceled before admission",
    ));
  }
  if request.source.hash_algo() != request.permit.hash_algorithm() {
    return Err(MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_hash_profile",
      "live source hash profile differs from the preflight permit",
    ));
  }
  Ok(())
}

fn source_identity(source: &StorageEngine) -> Result<PlatformFileIdentityDescriptorV1, MigrationFinalReconciliationErrorV1> {
  platform_file_identity(source.database_path())
    .map_err(|error| MigrationFinalReconciliationErrorV1::Engine(EngineError::IoError(std::io::Error::other(error.to_string()))))
}

fn snapshot_matches(snapshot: &FrozenSourceAuthoritySnapshot, authority: &FrozenMigrationSourceAuthorityV1) -> bool {
  snapshot.header_sequence == authority.header_sequence
    && snapshot.namespace_root == authority.namespace_root
    && snapshot.hard_publication_frontier == authority.hard_publication_frontier
    && snapshot.hash_algorithm == authority.hash_algorithm
}

fn map_acquisition_error(
  error: EngineError,
  started: Instant,
  timeout: Duration,
  cancellation: &CancellationToken,
) -> MigrationFinalReconciliationErrorV1 {
  if cancellation.is_cancelled() {
    return MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_canceled",
      "final source write freeze was canceled while waiting for exclusive admission",
    );
  }
  if started.elapsed() >= timeout {
    return MigrationFinalReconciliationErrorV1::invalid(
      "migration_final_freeze_timeout",
      "final source write freeze timed out while waiting for exclusive admission",
    );
  }
  MigrationFinalReconciliationErrorV1::Engine(error)
}
