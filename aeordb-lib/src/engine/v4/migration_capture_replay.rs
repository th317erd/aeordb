//! Ordered, idempotent replay of one AMPR-selected migration capture chain.
//!
//! Replay reconstructs destination state from the exact base-clone root on
//! every invocation. It never trusts an unselected capture tail and never
//! advances AMPR until the corresponding destination root is durable.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use tokio_util::sync::CancellationToken;

use super::entity::EntryTypeV4;
use super::first_authority::{
  FirstAuthorityPublicationErrorV1, ImmutableEntityBatchPublicationErrorV1, ImmutableEntityBatchPublicationRequestV1,
  ImmutableEntityWriteV1, PreparedNamespaceTreeV0, SuccessorAuthorityPublicationRequestV1, V4FirstAuthorityPublisher,
};
use super::gc_retirement::RetirementJournalOwnerV1;
use super::hash::digest_parts;
use super::index_task::{MutationJournalV1, MutationKindV1, MutationRecordV1};
use super::migration_base_clone_execution::{
  MigrationBaseCloneEntrySourceV1, MigrationBaseCloneExecutionErrorV1, MigrationSubtreeCloneRequestV1, MigrationTranslatedSubtreeV1,
  translate_migration_subtree_v1,
};
use super::migration_capture_workspace::{
  MIGRATION_CAPTURE_SEGMENT_MAX_BYTES_V1, MigrationCaptureWorkspaceErrorV1, ReopenedMigrationCaptureWorkspaceV1,
};
use super::migration_owner::{MigrationReplayCheckpointPublicationRequestV1, MigrationStateOwnerErrorV1, MigrationStateOwnerV1};
use super::migration_preflight::MigrationPreflightPermitV1;
use super::namespace::{EncodedSemanticObjectV1, NamespaceRootWriteV1, encode_namespace_root};
use crate::engine::btree::{
  BTREE_CONVERSION_THRESHOLD, BTreeMutationDelta, BTreeNodeRead, btree_lookup_with_reader, btree_plan_apply_with_reader,
  btree_plan_from_entries, is_btree_format,
};
use crate::engine::directory_entry::{ChildEntry, serialize_child_entries, visit_bounded_child_entries};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::path_utils::normalize_path;
use crate::engine::{CompressionAlgorithm, EntryHeader, EntryType, HashAlgorithm};

const MAX_ENTITY_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_REPLAY_RECORDS: u64 = 16 * 1024 * 1024;
const MAX_REPLAY_PUBLICATIONS: u64 = 16 * 1024 * 1024;
const MAX_PATH_DEPTH: usize = 1_000;
const IMMUTABLE_BATCH_ENTITIES: usize = 511;
const MINIMUM_REPLAY_MEMORY_BYTES: u64 = (MIGRATION_CAPTURE_SEGMENT_MAX_BYTES_V1 as u64) * 2;
const REPLAY_TRANSACTION_DOMAIN: &[u8] = b"aeordb.migration-capture-replay.transaction.v1\0";
const REPLAY_CLOSURE_DOMAIN: &[u8] = b"aeordb.migration-capture-replay.closure.v1\0";

/// Exact historical source reads used by capture replay.
///
/// The live-source adapter implements the inherited base-clone interface in
/// its separately reviewed module; replay remains coupled only to this bounded
/// reader contract.
pub trait MigrationCaptureReplaySourceV1: MigrationBaseCloneEntrySourceV1 {}

impl<T: MigrationBaseCloneEntrySourceV1 + ?Sized> MigrationCaptureReplaySourceV1 for T {}

/// Stable semantic/witness material shared by every replayed successor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureReplayAuthorityTemplateV1 {
  pub base_predecessor_head: Vec<u8>,
  pub semantic_state: EncodedSemanticObjectV1,
  pub required_capabilities: [u8; 32],
  pub typed_closure_context: Vec<u8>,
  pub authority_identity: Vec<u8>,
  pub publication_timestamp_floor_ms: u64,
  pub monotonic_timestamp_floor_ms: u64,
}

/// Caller-owned streaming handoff to the later legacy-root map owner.
/// Implementations must accept exact duplicate rows idempotently.
pub trait MigrationCaptureReplayRootSinkV1 {
  fn record_root_mapping(
    &mut self,
    source_publication_sequence: u64,
    source_root: &[u8],
    destination_namespace_root: &[u8],
    destination_tree_root: &[u8],
  ) -> EngineResult<()>;
}

pub struct MigrationCaptureReplayRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub capture: &'a ReopenedMigrationCaptureWorkspaceV1,
  pub source: &'a dyn MigrationCaptureReplaySourceV1,
  pub destination: &'a V4FirstAuthorityPublisher,
  pub state_owner: &'a MigrationStateOwnerV1,
  pub retirement_owner: &'a mut RetirementJournalOwnerV1,
  pub root_sink: &'a mut dyn MigrationCaptureReplayRootSinkV1,
  pub base_destination_tree_root: &'a [u8],
  pub authority: &'a MigrationCaptureReplayAuthorityTemplateV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub maximum_replay_memory_bytes: u64,
  pub maximum_subtree_memory_bytes: u64,
  pub maximum_subtree_work_items: u64,
  pub maximum_decoded_chunk_bytes: usize,
  pub maximum_directory_depth: usize,
  pub maximum_records: u64,
  pub maximum_publications: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureReplayReceiptV1 {
  pub replayed_through_publication_sequence: u64,
  pub capture_segment_count: u64,
  pub publication_count: u64,
  pub record_count: u64,
  pub collapsed_path_count: u64,
  pub destination_successor_count: u64,
  pub unchanged_destination_count: u64,
  pub destination_header_sequence: u64,
  pub destination_namespace_root: Vec<u8>,
  pub destination_tree_root: Vec<u8>,
}

#[derive(Debug)]
pub enum MigrationCaptureReplayErrorV1 {
  Invalid { code: &'static str, message: String },
  Source(EngineError),
  Workspace(MigrationCaptureWorkspaceErrorV1),
  Clone(MigrationBaseCloneExecutionErrorV1),
  Publication(FirstAuthorityPublicationErrorV1),
  ImmutablePublication(ImmutableEntityBatchPublicationErrorV1),
  State(MigrationStateOwnerErrorV1),
  Memory(MemoryCoordinatorError),
  RootSink(EngineError),
}

impl MigrationCaptureReplayErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Source(_) => "migration_replay_source",
      Self::Workspace(source) => source.code(),
      Self::Clone(source) => source.code(),
      Self::Publication(source) => source.code(),
      Self::ImmutablePublication(source) => source.code(),
      Self::State(source) => source.code(),
      Self::Memory(_) => "migration_replay_memory",
      Self::RootSink(_) => "migration_replay_root_sink",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MigrationCaptureReplayErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Source(source) | Self::RootSink(source) => Display::fmt(source, formatter),
      Self::Workspace(source) => Display::fmt(source, formatter),
      Self::Clone(source) => Display::fmt(source, formatter),
      Self::Publication(source) => Display::fmt(source, formatter),
      Self::ImmutablePublication(source) => Display::fmt(source, formatter),
      Self::State(source) => Display::fmt(source, formatter),
      Self::Memory(source) => Display::fmt(source, formatter),
    }
  }
}

impl Error for MigrationCaptureReplayErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Invalid { .. } => None,
      Self::Source(source) | Self::RootSink(source) => Some(source),
      Self::Workspace(source) => Some(source),
      Self::Clone(source) => Some(source),
      Self::Publication(source) => Some(source),
      Self::ImmutablePublication(source) => Some(source),
      Self::State(source) => Some(source),
      Self::Memory(source) => Some(source),
    }
  }
}

impl From<MigrationCaptureWorkspaceErrorV1> for MigrationCaptureReplayErrorV1 {
  fn from(source: MigrationCaptureWorkspaceErrorV1) -> Self {
    Self::Workspace(source)
  }
}

impl From<MigrationBaseCloneExecutionErrorV1> for MigrationCaptureReplayErrorV1 {
  fn from(source: MigrationBaseCloneExecutionErrorV1) -> Self {
    Self::Clone(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for MigrationCaptureReplayErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Publication(source)
  }
}

impl From<ImmutableEntityBatchPublicationErrorV1> for MigrationCaptureReplayErrorV1 {
  fn from(source: ImmutableEntityBatchPublicationErrorV1) -> Self {
    Self::ImmutablePublication(source)
  }
}

impl From<MigrationStateOwnerErrorV1> for MigrationCaptureReplayErrorV1 {
  fn from(source: MigrationStateOwnerErrorV1) -> Self {
    Self::State(source)
  }
}

impl From<MemoryCoordinatorError> for MigrationCaptureReplayErrorV1 {
  fn from(source: MemoryCoordinatorError) -> Self {
    Self::Memory(source)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MigrationPathProjectionModeV1 {
  TranslateSubtree,
  ReuseDestinationEntity,
}

pub(super) struct MigrationPathProjectionRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub source: &'a dyn MigrationCaptureReplaySourceV1,
  pub destination: &'a V4FirstAuthorityPublisher,
  pub source_root_after: &'a [u8],
  pub current_tree: &'a PreparedNamespaceTreeV0,
  pub path: &'a str,
  pub mode: MigrationPathProjectionModeV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub timestamp_ms: u64,
  pub maximum_subtree_memory_bytes: u64,
  pub maximum_subtree_work_items: u64,
  pub maximum_decoded_chunk_bytes: usize,
  pub maximum_directory_depth: usize,
}

pub(super) struct MigrationSuccessorProjectionRequestV1<'a> {
  pub permit: &'a MigrationPreflightPermitV1,
  pub destination: &'a V4FirstAuthorityPublisher,
  pub authority: &'a MigrationCaptureReplayAuthorityTemplateV1,
  pub source_sequence: u64,
  pub source_root: &'a [u8],
  pub expected_head_hash: &'a [u8],
  pub tree: PreparedNamespaceTreeV0,
  pub semantic_timestamp_ms: u64,
  pub transaction_domain: &'static [u8],
  pub closure_domain: &'static [u8],
}

pub(super) struct MigrationPathProjectionReceiptV1 {
  pub tree: PreparedNamespaceTreeV0,
  pub translated_subtree_work_items: u64,
}

pub(super) fn project_migration_authoritative_path_v1(
  request: MigrationPathProjectionRequestV1<'_>,
) -> Result<MigrationPathProjectionReceiptV1, MigrationCaptureReplayErrorV1> {
  let algorithm = request.permit.hash_algorithm();
  let source_entry = if request.path == "/" && request.source_root_after.iter().all(|byte| *byte == 0) {
    None
  } else {
    resolve_source_path(request.source, algorithm, request.source_root_after, request.path, request.cancellation)?
  };
  let source_entry_exists = source_entry.is_some();
  let mut translated_subtree_work_items = 0;
  let replacement = match (source_entry, request.mode) {
    (Some(source_entry), MigrationPathProjectionModeV1::TranslateSubtree) => {
      let entry_type = EntryType::from_u8(source_entry.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)?;
      let translated = translate_migration_subtree_v1(MigrationSubtreeCloneRequestV1 {
        permit: request.permit,
        source: request.source,
        destination: request.destination,
        memory: request.memory,
        cancellation: request.cancellation,
        publication_timestamp_ms: request.timestamp_ms,
        maximum_work_items: request.maximum_subtree_work_items,
        maximum_memory_bytes: request.maximum_subtree_memory_bytes,
        maximum_decoded_chunk_bytes: request.maximum_decoded_chunk_bytes,
        maximum_directory_depth: request.maximum_directory_depth,
        path: request.path,
        hash: &source_entry.hash,
        entry_type,
        logical_bytes: source_entry.total_size,
      })?;
      translated.map(|translated| {
        translated_subtree_work_items = translated.work_items;
        apply_translation(source_entry, translated)
      })
    }
    (Some(source_entry), MigrationPathProjectionModeV1::ReuseDestinationEntity) => {
      let destination_entry =
        resolve_destination_path(request.destination, algorithm, request.current_tree, request.path, request.cancellation)?.ok_or_else(
          || {
            MigrationCaptureReplayErrorV1::invalid(
              "migration_replay_destination_reuse_missing",
              format!("destination path {} has no translated entity to reuse", request.path),
            )
          },
        )?;
      Some(reuse_destination_identity(source_entry, destination_entry)?)
    }
    (None, _) => None,
  };
  let tree = if request.path == "/" {
    match replacement {
      Some(replacement) => {
        if EntryType::from_u8(replacement.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)? != EntryType::DirectoryIndex {
          return Err(MigrationCaptureReplayErrorV1::invalid(
            "migration_replay_root_type",
            "source root projection did not produce a directory",
          ));
        }
        load_destination_tree(request.destination, &replacement.hash)
      }
      None if !source_entry_exists => {
        publish_directory_values(request.destination, algorithm, request.permit.database_id(), request.timestamp_ms, vec![Vec::new()], None)
      }
      None => Ok(request.current_tree.clone()),
    }?
  } else if replacement.is_none() && source_entry_exists {
    request.current_tree.clone()
  } else {
    patch_destination_path(
      DestinationPathPatchContextV1 {
        destination: request.destination,
        algorithm,
        source_root_after: request.source_root_after,
        source: request.source,
        timestamp_ms: request.timestamp_ms,
        database_id: request.permit.database_id(),
        cancellation: request.cancellation,
      },
      request.current_tree,
      request.path,
      replacement,
    )?
  };
  Ok(MigrationPathProjectionReceiptV1 { tree, translated_subtree_work_items })
}

pub(super) fn publish_migration_successor_v1(
  request: MigrationSuccessorProjectionRequestV1<'_>,
) -> Result<super::first_authority::FirstAuthorityPublicationReceiptV1, MigrationCaptureReplayErrorV1> {
  let algorithm = request.permit.hash_algorithm();
  let timestamp = timestamp_for(request.authority.publication_timestamp_floor_ms, request.semantic_timestamp_ms, request.source_sequence)?;
  let transaction_id = migration_projection_transaction_id(
    algorithm,
    request.transaction_domain,
    request.permit.migration_id(),
    request.source_sequence,
    request.source_root,
    &request.tree.root_hash,
  )?;
  let closure = digest_parts(
    algorithm,
    &[
      request.closure_domain,
      &request.authority.typed_closure_context,
      &request.source_sequence.to_le_bytes(),
      request.source_root,
      &request.tree.root_hash,
      &request.authority.semantic_state.object_id,
    ],
  );
  let publication_request = SuccessorAuthorityPublicationRequestV1 {
    database_id: request.permit.database_id(),
    transaction_id,
    created_at_ms: timestamp,
    expected_head_hash: request.expected_head_hash.to_vec(),
    namespace_tree: request.tree,
    semantic_state: request.authority.semantic_state.clone(),
    required_capabilities: request.authority.required_capabilities,
    typed_closure_digest: closure,
    authority_identity: request.authority.authority_identity.clone(),
  };
  match request.destination.publish_successor_authority(&publication_request) {
    Ok(receipt) => Ok(receipt),
    Err(error) => match error.committed_receipt() {
      Some(receipt) => Ok(receipt.clone()),
      None => Err(MigrationCaptureReplayErrorV1::Publication(error)),
    },
  }
}

#[derive(Clone)]
struct OwnedMutationRecordV1 {
  kind: MutationKindV1,
  before_path: Option<String>,
  before_revision: Option<Vec<u8>>,
  after_path: Option<String>,
  after_revision: Option<Vec<u8>>,
}

struct ReplayBatchV1 {
  sequence: u64,
  root_before: Vec<u8>,
  root_after: Vec<u8>,
  committed_at_ms: u64,
  records: Vec<OwnedMutationRecordV1>,
}

struct ReplayExecutorV1<'a> {
  request: MigrationCaptureReplayRequestV1<'a>,
  _memory: MemoryReservation,
  algorithm: HashAlgorithm,
  reconciled_through: u64,
  current_source_root: Vec<u8>,
  current_tree: PreparedNamespaceTreeV0,
  publication_count: u64,
  record_count: u64,
  collapsed_path_count: u64,
  successor_count: u64,
  unchanged_count: u64,
}

pub fn execute_selected_migration_capture_replay_v1(
  request: MigrationCaptureReplayRequestV1<'_>,
) -> Result<MigrationCaptureReplayReceiptV1, MigrationCaptureReplayErrorV1> {
  let capture = request.capture;
  let mut executor = ReplayExecutorV1::new(request)?;
  executor.publish_or_recover_base()?;
  capture.try_for_each_segment(|journal| executor.replay_journal(journal))?;
  executor.finish()
}

impl<'a> ReplayExecutorV1<'a> {
  fn new(request: MigrationCaptureReplayRequestV1<'a>) -> Result<Self, MigrationCaptureReplayErrorV1> {
    validate_request(&request)?;
    let algorithm = request.permit.hash_algorithm();
    let observation = request.state_owner.observe_capture_state(
      i64::try_from(request.authority.publication_timestamp_floor_ms)
        .map_err(|error| MigrationCaptureReplayErrorV1::invalid("migration_replay_timestamp", error.to_string()))?,
      request.authority.publication_timestamp_floor_ms,
      request.authority.monotonic_timestamp_floor_ms,
    )?;
    if observation.checkpoint_artifact != request.capture.selected_manifest_identity()
      || observation.captured_through_publication_sequence != request.capture.captured_through_publication_sequence()
    {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_checkpoint_selection",
        "AMPR does not select the reopened capture checkpoint and watermark",
      ));
    }
    let basis_sequence = request.capture.starting_publication_sequence();
    if observation.reconciled_through_publication_sequence != 0 && observation.reconciled_through_publication_sequence < basis_sequence {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_checkpoint_before_basis",
        "AMPR replay watermark is between zero and the selected capture basis",
      ));
    }
    if observation.reconciled_through_publication_sequence > observation.captured_through_publication_sequence {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_checkpoint_beyond_capture",
        "AMPR replay watermark exceeds selected capture",
      ));
    }
    let memory = request.memory.reserve(MemoryOwner::Migration, request.maximum_replay_memory_bytes, AdmissionClass::Maintenance)?;
    let current_tree = load_destination_tree(request.destination, request.base_destination_tree_root)?;
    let current_source_root = request.capture.starting_source_root().to_vec();
    Ok(Self {
      request,
      _memory: memory,
      algorithm,
      reconciled_through: observation.reconciled_through_publication_sequence,
      current_source_root,
      current_tree,
      publication_count: 0,
      record_count: 0,
      collapsed_path_count: 0,
      successor_count: 0,
      unchanged_count: 0,
    })
  }

  fn publish_or_recover_base(&mut self) -> Result<(), MigrationCaptureReplayErrorV1> {
    let basis_sequence = self.request.capture.starting_publication_sequence();
    let destination_root = namespace_root_for_tree(self.algorithm, &self.current_tree, self.request.authority)?;
    if self.reconciled_through == 0 {
      let selected = self.request.destination.observe()?.selected.header;
      if selected.head_hash == self.request.authority.base_predecessor_head && selected.head_hash == destination_root {
        self
          .request
          .root_sink
          .record_root_mapping(basis_sequence, self.request.capture.starting_source_root(), &destination_root, &self.current_tree.root_hash)
          .map_err(MigrationCaptureReplayErrorV1::RootSink)?;
        if basis_sequence != 0 {
          self.publish_checkpoint(basis_sequence, selected.slot_sequence)?;
        }
        return Ok(());
      }
      let publication = self.publish_successor(
        basis_sequence,
        self.request.capture.starting_source_root(),
        &self.request.authority.base_predecessor_head,
        self.current_tree.clone(),
        self.request.authority.publication_timestamp_floor_ms,
      )?;
      if !publication.idempotent {
        self.successor_count = checked_add(self.successor_count, 1, "successor count")?;
      }
      self
        .request
        .root_sink
        .record_root_mapping(
          basis_sequence,
          self.request.capture.starting_source_root(),
          &publication.namespace_root.root_hash,
          &self.current_tree.root_hash,
        )
        .map_err(MigrationCaptureReplayErrorV1::RootSink)?;
      if basis_sequence != 0 {
        self.publish_checkpoint(basis_sequence, publication.observation.selected.header.slot_sequence)?;
      }
    } else {
      self
        .request
        .root_sink
        .record_root_mapping(basis_sequence, self.request.capture.starting_source_root(), &destination_root, &self.current_tree.root_hash)
        .map_err(MigrationCaptureReplayErrorV1::RootSink)?;
    }
    Ok(())
  }

  fn replay_journal(&mut self, journal: &MutationJournalV1<'_>) -> Result<(), MigrationCaptureReplayErrorV1> {
    let mut records = journal.records.iter();
    let mut pending_publication: Option<ReplayBatchV1> = None;
    while let Some(record) = records.next() {
      self.check_cancelled()?;
      let first = record.map_err(|error| MigrationCaptureReplayErrorV1::invalid(error.code(), error.to_string()))?;
      let batch_count = usize::try_from(first.batch_count)
        .map_err(|error| MigrationCaptureReplayErrorV1::invalid("migration_replay_batch_count", error.to_string()))?;
      let mut batch = ReplayBatchV1 {
        sequence: first.sequence,
        root_before: first.root_before.to_vec(),
        root_after: first.root_after.to_vec(),
        committed_at_ms: first.committed_at_ms,
        records: Vec::new(),
      };
      batch.records.try_reserve_exact(batch_count).map_err(|error| {
        MigrationCaptureReplayErrorV1::invalid("migration_replay_batch_allocation", format!("batch allocation failed: {error}"))
      })?;
      batch.records.push(own_record(&first));
      for _ in 1..batch_count {
        let record = records
          .next()
          .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_batch_truncated", "journal batch ended early"))?
          .map_err(|error| MigrationCaptureReplayErrorV1::invalid(error.code(), error.to_string()))?;
        if record.sequence != batch.sequence || record.root_before != batch.root_before || record.root_after != batch.root_after {
          return Err(MigrationCaptureReplayErrorV1::invalid(
            "migration_replay_batch_divergence",
            "journal batch members disagree after selected-workspace validation",
          ));
        }
        batch.records.push(own_record(&record));
      }
      match pending_publication.take() {
        Some(mut publication) if publication.sequence == batch.sequence => {
          if publication.root_before != batch.root_before
            || publication.root_after != batch.root_after
            || publication.committed_at_ms != batch.committed_at_ms
          {
            return Err(MigrationCaptureReplayErrorV1::invalid(
              "migration_replay_publication_divergence",
              "atomic batches from one source publication disagree on roots or commit time",
            ));
          }
          publication.records.try_reserve(batch.records.len()).map_err(|error| {
            MigrationCaptureReplayErrorV1::invalid(
              "migration_replay_publication_allocation",
              format!("publication record allocation failed: {error}"),
            )
          })?;
          publication.records.append(&mut batch.records);
          pending_publication = Some(publication);
        }
        Some(publication) => {
          self.replay_batch(publication)?;
          pending_publication = Some(batch);
        }
        None => pending_publication = Some(batch),
      }
    }
    if let Some(publication) = pending_publication {
      self.replay_batch(publication)?;
    }
    Ok(())
  }

  fn replay_batch(&mut self, batch: ReplayBatchV1) -> Result<(), MigrationCaptureReplayErrorV1> {
    self.publication_count = checked_add(self.publication_count, 1, "publication count")?;
    self.record_count = checked_add(
      self.record_count,
      u64::try_from(batch.records.len())
        .map_err(|error| MigrationCaptureReplayErrorV1::invalid("migration_replay_record_count", error.to_string()))?,
      "record count",
    )?;
    if self.publication_count > self.request.maximum_publications || self.record_count > self.request.maximum_records {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_work_limit",
        "selected capture exceeds caller replay publication or record bounds",
      ));
    }
    let expected_sequence = self
      .request
      .capture
      .starting_publication_sequence()
      .checked_add(self.publication_count)
      .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_sequence_overflow", "publication sequence overflow"))?;
    if batch.sequence != expected_sequence || batch.root_before != self.current_source_root {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_source_order",
        "capture publication does not continue the exact replay source sequence/root",
      ));
    }

    let mut changed_paths = BTreeSet::new();
    for record in &batch.records {
      verify_record(self.request.source, self.algorithm, &batch.root_before, &batch.root_after, record, self.request.cancellation)?;
      if let Some(path) = &record.before_path {
        changed_paths.insert(path.clone());
      }
      if let Some(path) = &record.after_path {
        changed_paths.insert(path.clone());
      }
    }
    if changed_paths.is_empty() && batch.root_before != batch.root_after {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_unrepresented_transition",
        "source namespace root changed without any path identity to replay",
      ));
    }
    let paths = collapse_changed_paths(changed_paths);
    self.collapsed_path_count = checked_add(
      self.collapsed_path_count,
      u64::try_from(paths.len())
        .map_err(|error| MigrationCaptureReplayErrorV1::invalid("migration_replay_path_count", error.to_string()))?,
      "collapsed path count",
    )?;

    let predecessor_tree = self.current_tree.clone();
    let predecessor_namespace_root = namespace_root_for_tree(self.algorithm, &predecessor_tree, self.request.authority)?;
    for path in paths {
      self.apply_authoritative_path(&batch.root_after, &path, batch.committed_at_ms)?;
    }
    let destination_namespace_root = namespace_root_for_tree(self.algorithm, &self.current_tree, self.request.authority)?;
    let destination_header_sequence = if self.current_tree.root_hash == predecessor_tree.root_hash {
      self.unchanged_count = checked_add(self.unchanged_count, 1, "unchanged destination count")?;
      self.request.destination.observe()?.selected.header.slot_sequence
    } else if batch.sequence <= self.reconciled_through {
      self.request.destination.observe()?.selected.header.slot_sequence
    } else {
      let publication = self.publish_successor(
        batch.sequence,
        &batch.root_after,
        &predecessor_namespace_root,
        self.current_tree.clone(),
        batch.committed_at_ms,
      )?;
      if !publication.idempotent {
        self.successor_count = checked_add(self.successor_count, 1, "successor count")?;
      }
      publication.observation.selected.header.slot_sequence
    };
    self
      .request
      .root_sink
      .record_root_mapping(batch.sequence, &batch.root_after, &destination_namespace_root, &self.current_tree.root_hash)
      .map_err(MigrationCaptureReplayErrorV1::RootSink)?;
    if batch.sequence > self.reconciled_through {
      self.publish_checkpoint(batch.sequence, destination_header_sequence)?;
    }
    self.current_source_root = batch.root_after;
    Ok(())
  }

  fn apply_authoritative_path(
    &mut self,
    source_root_after: &[u8],
    path: &str,
    committed_at_ms: u64,
  ) -> Result<(), MigrationCaptureReplayErrorV1> {
    self.current_tree = project_migration_authoritative_path_v1(MigrationPathProjectionRequestV1 {
      permit: self.request.permit,
      source: self.request.source,
      destination: self.request.destination,
      source_root_after,
      current_tree: &self.current_tree,
      path,
      mode: MigrationPathProjectionModeV1::TranslateSubtree,
      memory: self.request.memory,
      cancellation: self.request.cancellation,
      timestamp_ms: timestamp_for(self.request.authority.publication_timestamp_floor_ms, committed_at_ms, 0)?,
      maximum_subtree_memory_bytes: self.request.maximum_subtree_memory_bytes,
      maximum_subtree_work_items: self.request.maximum_subtree_work_items,
      maximum_decoded_chunk_bytes: self.request.maximum_decoded_chunk_bytes,
      maximum_directory_depth: self.request.maximum_directory_depth,
    })?
    .tree;
    Ok(())
  }

  fn publish_successor(
    &self,
    source_sequence: u64,
    source_root: &[u8],
    expected_head_hash: &[u8],
    tree: PreparedNamespaceTreeV0,
    semantic_timestamp_ms: u64,
  ) -> Result<super::first_authority::FirstAuthorityPublicationReceiptV1, MigrationCaptureReplayErrorV1> {
    publish_migration_successor_v1(MigrationSuccessorProjectionRequestV1 {
      permit: self.request.permit,
      destination: self.request.destination,
      authority: self.request.authority,
      source_sequence,
      source_root,
      expected_head_hash,
      tree,
      semantic_timestamp_ms,
      transaction_domain: REPLAY_TRANSACTION_DOMAIN,
      closure_domain: REPLAY_CLOSURE_DOMAIN,
    })
  }

  fn publish_checkpoint(&mut self, sequence: u64, destination_header_sequence: u64) -> Result<(), MigrationCaptureReplayErrorV1> {
    let timestamp = timestamp_for(self.request.authority.publication_timestamp_floor_ms, 0, sequence)?;
    let updated_at_ms =
      i64::try_from(timestamp).map_err(|error| MigrationCaptureReplayErrorV1::invalid("migration_replay_timestamp", error.to_string()))?;
    self.request.state_owner.publish_replay_checkpoint(
      MigrationReplayCheckpointPublicationRequestV1 {
        reconciled_through_publication_sequence: sequence,
        destination_header_sequence,
        updated_at_ms,
        publication_timestamp_ms: timestamp,
        monotonic_now_ms: monotonic_for(
          self.request.authority.monotonic_timestamp_floor_ms,
          self.request.capture.starting_publication_sequence(),
          sequence,
        )?,
      },
      self.request.retirement_owner,
    )?;
    self.reconciled_through = sequence;
    Ok(())
  }

  fn finish(self) -> Result<MigrationCaptureReplayReceiptV1, MigrationCaptureReplayErrorV1> {
    if self.current_source_root != self.request.capture.selected_source_root_after()
      || self.reconciled_through != self.request.capture.captured_through_publication_sequence()
    {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_incomplete",
        "replay did not reach the exact selected source root and publication watermark",
      ));
    }
    let destination_namespace_root = namespace_root_for_tree(self.algorithm, &self.current_tree, self.request.authority)?;
    let observation = self.request.destination.observe()?;
    if observation.selected.header.head_hash != destination_namespace_root {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_destination_divergence",
        "selected destination HEAD differs from the replayed destination root",
      ));
    }
    Ok(MigrationCaptureReplayReceiptV1 {
      replayed_through_publication_sequence: self.reconciled_through,
      capture_segment_count: self.request.capture.segment_count(),
      publication_count: self.publication_count,
      record_count: self.record_count,
      collapsed_path_count: self.collapsed_path_count,
      destination_successor_count: self.successor_count,
      unchanged_destination_count: self.unchanged_count,
      destination_header_sequence: observation.selected.header.slot_sequence,
      destination_namespace_root,
      destination_tree_root: self.current_tree.root_hash,
    })
  }

  fn check_cancelled(&self) -> Result<(), MigrationCaptureReplayErrorV1> {
    if self.request.cancellation.is_cancelled() {
      return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "capture replay was canceled"));
    }
    Ok(())
  }
}

fn validate_request(request: &MigrationCaptureReplayRequestV1<'_>) -> Result<(), MigrationCaptureReplayErrorV1> {
  if request.cancellation.is_cancelled() {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "capture replay was canceled"));
  }
  let width = request.permit.hash_algorithm().hash_length();
  if request.source.hash_algorithm() != request.permit.hash_algorithm()
    || request.base_destination_tree_root.len() != width
    || request.base_destination_tree_root.iter().all(|byte| *byte == 0)
    || request.authority.base_predecessor_head.len() != width
    || request.authority.base_predecessor_head.iter().all(|byte| *byte == 0)
    || request.authority.semantic_state.object_id.len() != width
    || request.authority.typed_closure_context.is_empty()
    || request.authority.authority_identity.is_empty()
    || request.authority.publication_timestamp_floor_ms == 0
    || request.authority.publication_timestamp_floor_ms > i64::MAX as u64
    || request.authority.monotonic_timestamp_floor_ms == 0
  {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_request",
      "source, roots, authority template, or timestamp floor is invalid",
    ));
  }
  if request.capture.starting_source_root() != request.permit.source_capture_head() {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_basis_divergence",
      "selected capture basis differs from the preflight/base-clone source root; final reconciliation is required",
    ));
  }
  monotonic_for(
    request.authority.monotonic_timestamp_floor_ms,
    request.capture.starting_publication_sequence(),
    request.capture.captured_through_publication_sequence(),
  )?;
  timestamp_for(request.authority.publication_timestamp_floor_ms, 0, request.capture.captured_through_publication_sequence())?;
  if request.maximum_replay_memory_bytes < MINIMUM_REPLAY_MEMORY_BYTES
    || request.maximum_subtree_memory_bytes == 0
    || request.maximum_subtree_work_items == 0
    || request.maximum_decoded_chunk_bytes == 0
    || request.maximum_decoded_chunk_bytes > MAX_ENTITY_TOTAL_BYTES
    || request.maximum_directory_depth == 0
    || request.maximum_directory_depth > MAX_PATH_DEPTH
    || request.maximum_records == 0
    || request.maximum_records > MAX_REPLAY_RECORDS
    || request.maximum_publications == 0
    || request.maximum_publications > MAX_REPLAY_PUBLICATIONS
  {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_bounds",
      "capture replay resource bounds are zero or exceed engine ceilings",
    ));
  }
  Ok(())
}

fn own_record(record: &MutationRecordV1<'_>) -> OwnedMutationRecordV1 {
  OwnedMutationRecordV1 {
    kind: record.kind,
    before_path: record.before_path.map(ToOwned::to_owned),
    before_revision: record.before_revision.map(ToOwned::to_owned),
    after_path: record.after_path.map(ToOwned::to_owned),
    after_revision: record.after_revision.map(ToOwned::to_owned),
  }
}

fn verify_record(
  source: &dyn MigrationCaptureReplaySourceV1,
  algorithm: HashAlgorithm,
  root_before: &[u8],
  root_after: &[u8],
  record: &OwnedMutationRecordV1,
  cancellation: &CancellationToken,
) -> Result<(), MigrationCaptureReplayErrorV1> {
  verify_record_side(source, algorithm, root_before, record.before_path.as_deref(), record.before_revision.as_deref(), cancellation)?;
  verify_record_side(source, algorithm, root_after, record.after_path.as_deref(), record.after_revision.as_deref(), cancellation)?;
  match record.kind {
    MutationKindV1::Create | MutationKindV1::Copy | MutationKindV1::Restore => {
      verify_path_absent(source, algorithm, root_before, record.after_path.as_deref(), cancellation)
    }
    MutationKindV1::Delete => verify_path_absent(source, algorithm, root_after, record.before_path.as_deref(), cancellation),
    MutationKindV1::Move => {
      verify_path_absent(source, algorithm, root_before, record.after_path.as_deref(), cancellation)?;
      verify_path_absent(source, algorithm, root_after, record.before_path.as_deref(), cancellation)
    }
    MutationKindV1::Update | MutationKindV1::Transition => Ok(()),
  }
}

fn verify_path_absent(
  source: &dyn MigrationCaptureReplaySourceV1,
  algorithm: HashAlgorithm,
  root: &[u8],
  path: Option<&str>,
  cancellation: &CancellationToken,
) -> Result<(), MigrationCaptureReplayErrorV1> {
  let path = path.ok_or_else(|| {
    MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_record_side",
      "mutation kind requires a path whose historical absence can be verified",
    )
  })?;
  if resolve_source_path(source, algorithm, root, path, cancellation)?.is_some() {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_presence_divergence",
      format!("historical path {path} exists where its mutation requires absence"),
    ));
  }
  Ok(())
}

fn verify_record_side(
  source: &dyn MigrationCaptureReplaySourceV1,
  algorithm: HashAlgorithm,
  root: &[u8],
  path: Option<&str>,
  revision: Option<&[u8]>,
  cancellation: &CancellationToken,
) -> Result<(), MigrationCaptureReplayErrorV1> {
  match (path, revision) {
    (None, None) => Ok(()),
    (Some(path), Some(revision)) => {
      let actual = resolve_source_path(source, algorithm, root, path, cancellation)?;
      if actual.as_ref().map(|entry| entry.hash.as_slice()) != Some(revision) {
        return Err(MigrationCaptureReplayErrorV1::invalid(
          "migration_replay_revision_divergence",
          format!("historical path {path} does not match its captured revision"),
        ));
      }
      Ok(())
    }
    _ => Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_record_side", "captured path and revision presence disagree")),
  }
}

fn collapse_changed_paths(paths: BTreeSet<String>) -> Vec<String> {
  let mut paths = paths.into_iter().collect::<Vec<_>>();
  paths.sort_by(|left, right| path_depth(left).cmp(&path_depth(right)).then_with(|| left.cmp(right)));
  let mut retained: Vec<String> = Vec::new();
  for path in paths {
    if retained.iter().any(|ancestor| is_path_ancestor(ancestor, &path)) {
      continue;
    }
    retained.push(path);
  }
  retained
}

fn path_depth(path: &str) -> usize {
  path.split('/').filter(|segment| !segment.is_empty()).count()
}

fn is_path_ancestor(ancestor: &str, path: &str) -> bool {
  ancestor == "/" || path == ancestor || path.strip_prefix(ancestor).is_some_and(|suffix| suffix.starts_with('/'))
}

fn resolve_source_path(
  source: &dyn MigrationCaptureReplaySourceV1,
  algorithm: HashAlgorithm,
  root: &[u8],
  path: &str,
  cancellation: &CancellationToken,
) -> Result<Option<ChildEntry>, MigrationCaptureReplayErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "capture replay was canceled"));
  }
  if normalize_path(path) != path || !path.starts_with('/') || path_depth(path) > MAX_PATH_DEPTH {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_path", format!("captured path {path:?} is invalid")));
  }
  if path == "/" {
    let loaded = load_source_entity(source, algorithm, root, EntryType::DirectoryIndex)?;
    return Ok(Some(ChildEntry {
      entry_type: EntryType::DirectoryIndex.to_u8(),
      hash: root.to_vec(),
      total_size: loaded.2.len() as u64,
      created_at: 0,
      updated_at: 0,
      name: String::new(),
      content_type: None,
      virtual_time: 0,
      node_id: 0,
    }));
  }
  let reader = HistoricalBtreeReaderV1 { source, algorithm };
  let mut directory_hash = root.to_vec();
  let segments = path.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
  for (index, segment) in segments.iter().enumerate() {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "capture replay was canceled"));
    }
    let (header, _, value) = load_source_entity(source, algorithm, &directory_hash, EntryType::DirectoryIndex)?;
    let child = lookup_directory_child(&reader, &directory_hash, &value, header.entry_version, segment, algorithm.hash_length())?;
    let Some(child) = child else {
      return Ok(None);
    };
    if index + 1 == segments.len() {
      return Ok(Some(child));
    }
    if EntryType::from_u8(child.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)? != EntryType::DirectoryIndex {
      return Ok(None);
    }
    directory_hash = child.hash;
  }
  Ok(None)
}

fn resolve_destination_path(
  destination: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  tree: &PreparedNamespaceTreeV0,
  path: &str,
  cancellation: &CancellationToken,
) -> Result<Option<ChildEntry>, MigrationCaptureReplayErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "namespace projection was canceled"));
  }
  if normalize_path(path) != path || !path.starts_with('/') || path_depth(path) > MAX_PATH_DEPTH {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_path", format!("destination path {path:?} is invalid")));
  }
  if path == "/" {
    return Ok(Some(ChildEntry {
      entry_type: EntryType::DirectoryIndex.to_u8(),
      hash: tree.root_hash.clone(),
      total_size: tree.stored_value.len() as u64,
      created_at: 0,
      updated_at: 0,
      name: String::new(),
      content_type: None,
      virtual_time: 0,
      node_id: 0,
    }));
  }
  let reader = DestinationBtreeReaderV1 { destination };
  let mut directory_hash = tree.root_hash.clone();
  let segments = path.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
  for (index, segment) in segments.iter().enumerate() {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "namespace projection was canceled"));
    }
    let entity = load_destination_directory(destination, &directory_hash)?;
    let child =
      lookup_directory_child(&reader, &directory_hash, &entity.stored_value, entity.entity_version, segment, algorithm.hash_length())?;
    let Some(child) = child else {
      return Ok(None);
    };
    if index + 1 == segments.len() {
      return Ok(Some(child));
    }
    if EntryType::from_u8(child.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)? != EntryType::DirectoryIndex {
      return Ok(None);
    }
    directory_hash = child.hash;
  }
  Ok(None)
}

fn load_source_entity(
  source: &dyn MigrationCaptureReplaySourceV1,
  algorithm: HashAlgorithm,
  hash: &[u8],
  expected_type: EntryType,
) -> Result<(EntryHeader, Vec<u8>, Vec<u8>), MigrationCaptureReplayErrorV1> {
  let header = source
    .historical_entry_header(hash)
    .map_err(MigrationCaptureReplayErrorV1::Source)?
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_missing_source_entity", hex::encode(hash)))?;
  if header.entry_type != expected_type || header.value_length as usize > MAX_ENTITY_TOTAL_BYTES {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_source_entity",
      "historical source entity type or bounded length is invalid",
    ));
  }
  let (actual, key, value) = source
    .historical_entry_verified_bounded(hash, header.value_length)
    .map_err(MigrationCaptureReplayErrorV1::Source)?
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_missing_source_entity", hex::encode(hash)))?;
  if key != hash
    || actual.entry_type != header.entry_type
    || actual.entry_version != header.entry_version
    || actual.value_length != header.value_length
    || actual.key_length != header.key_length
    || value.len() != header.value_length as usize
    || algorithm != source.hash_algorithm()
  {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_source_changed",
      format!("historical source entity {} changed between bounded reads", hex::encode(hash)),
    ));
  }
  Ok((actual, key, value))
}

struct HistoricalBtreeReaderV1<'a> {
  source: &'a dyn MigrationCaptureReplaySourceV1,
  algorithm: HashAlgorithm,
}

impl BTreeNodeRead for HistoricalBtreeReaderV1<'_> {
  fn load_btree_node(&self, node_hash: &[u8]) -> EngineResult<(Vec<u8>, u8)> {
    load_source_entity(self.source, self.algorithm, node_hash, EntryType::DirectoryIndex)
      .map(|(header, _, value)| (value, header.entry_version))
      .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: error.to_string() })
  }
}

struct DestinationBtreeReaderV1<'a> {
  destination: &'a V4FirstAuthorityPublisher,
}

impl BTreeNodeRead for DestinationBtreeReaderV1<'_> {
  fn load_btree_node(&self, node_hash: &[u8]) -> EngineResult<(Vec<u8>, u8)> {
    let entity = load_destination_directory(self.destination, node_hash)
      .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: error.to_string() })?;
    Ok((entity.stored_value, entity.entity_version))
  }
}

fn lookup_directory_child(
  reader: &(impl BTreeNodeRead + ?Sized),
  root_hash: &[u8],
  value: &[u8],
  entry_version: u8,
  name: &str,
  hash_width: usize,
) -> Result<Option<ChildEntry>, MigrationCaptureReplayErrorV1> {
  if is_btree_format(value) {
    return btree_lookup_with_reader(reader, root_hash, name, hash_width).map_err(MigrationCaptureReplayErrorV1::Source);
  }
  let mut found = None;
  visit_bounded_child_entries(value, hash_width, entry_version, BTREE_CONVERSION_THRESHOLD, |child| {
    if child.name == name {
      found = Some(child);
      return Ok(false);
    }
    Ok(true)
  })
  .map_err(MigrationCaptureReplayErrorV1::Source)?;
  Ok(found)
}

struct DestinationPathPatchContextV1<'a> {
  destination: &'a V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  source_root_after: &'a [u8],
  source: &'a dyn MigrationCaptureReplaySourceV1,
  timestamp_ms: u64,
  database_id: [u8; 16],
  cancellation: &'a CancellationToken,
}

fn patch_destination_path(
  context: DestinationPathPatchContextV1<'_>,
  tree: &PreparedNamespaceTreeV0,
  path: &str,
  mut replacement: Option<ChildEntry>,
) -> Result<PreparedNamespaceTreeV0, MigrationCaptureReplayErrorV1> {
  let segments = path.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
  if segments.is_empty() || segments.len() > MAX_PATH_DEPTH {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_patch_path", "destination patch path is invalid"));
  }
  let reader = DestinationBtreeReaderV1 { destination: context.destination };
  let mut ancestor_hashes = Vec::new();
  ancestor_hashes.try_reserve_exact(segments.len()).map_err(|error| {
    MigrationCaptureReplayErrorV1::invalid("migration_replay_ancestor_allocation", format!("ancestor allocation failed: {error}"))
  })?;
  ancestor_hashes.push(tree.root_hash.clone());
  let mut current_hash = tree.root_hash.clone();
  for segment in &segments[..segments.len() - 1] {
    if context.cancellation.is_cancelled() {
      return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "capture replay was canceled"));
    }
    let entity = load_destination_directory(context.destination, &current_hash)?;
    let child = lookup_directory_child(
      &reader,
      &current_hash,
      &entity.stored_value,
      entity.entity_version,
      segment,
      context.algorithm.hash_length(),
    )?
    .ok_or_else(|| {
      MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_destination_parent_missing",
        format!("destination parent segment {segment:?} is absent"),
      )
    })?;
    if EntryType::from_u8(child.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)? != EntryType::DirectoryIndex {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_destination_parent_type",
        format!("destination parent segment {segment:?} is not a directory"),
      ));
    }
    current_hash = child.hash;
    ancestor_hashes.push(current_hash.clone());
  }

  let mut final_tree = None;
  for level in (0..ancestor_hashes.len()).rev() {
    if context.cancellation.is_cancelled() {
      return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_cancelled", "capture replay was canceled"));
    }
    let parent_hash = &ancestor_hashes[level];
    let parent = load_destination_directory(context.destination, parent_hash)?;
    let rebuilt = apply_directory_delta(
      context.destination,
      context.algorithm,
      &parent.stored_value,
      parent.entity_version,
      segments[level],
      replacement,
      context.timestamp_ms,
      context.database_id,
    )?;
    if level == 0 {
      final_tree = Some(rebuilt);
      break;
    }
    let directory_path = format!("/{}", segments[..level].join("/"));
    let mut metadata =
      resolve_source_path(context.source, context.algorithm, context.source_root_after, &directory_path, context.cancellation)?
        .ok_or_else(|| {
          MigrationCaptureReplayErrorV1::invalid(
            "migration_replay_source_parent_missing",
            format!("source parent {directory_path} is absent after its captured publication"),
          )
        })?;
    if EntryType::from_u8(metadata.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)? != EntryType::DirectoryIndex {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_source_parent_type",
        format!("source parent {directory_path} is not a directory"),
      ));
    }
    metadata.hash = rebuilt.root_hash;
    metadata.total_size = rebuilt.stored_value.len() as u64;
    replacement = Some(metadata);
  }
  final_tree.ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_patch_result", "destination patch produced no root"))
}

#[allow(clippy::too_many_arguments)]
fn apply_directory_delta(
  destination: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  root_value: &[u8],
  entry_version: u8,
  name: &str,
  replacement: Option<ChildEntry>,
  timestamp_ms: u64,
  database_id: [u8; 16],
) -> Result<PreparedNamespaceTreeV0, MigrationCaptureReplayErrorV1> {
  if is_btree_format(root_value) {
    let reader = DestinationBtreeReaderV1 { destination };
    let delta = match replacement {
      Some(entry) => BTreeMutationDelta { upserts: vec![entry], removals: Vec::new() },
      None => BTreeMutationDelta { upserts: Vec::new(), removals: vec![name.to_string()] },
    };
    let plan = btree_plan_apply_with_reader(&reader, root_value, delta, algorithm.hash_length(), &algorithm)
      .map_err(MigrationCaptureReplayErrorV1::Source)?;
    let Some(plan) = plan else {
      return publish_directory_values(destination, algorithm, database_id, timestamp_ms, vec![Vec::new()], None);
    };
    let values = plan.node_writes().map(|write| write.value.clone()).collect::<Vec<_>>();
    return publish_directory_values(destination, algorithm, database_id, timestamp_ms, values, Some((plan.root_hash(), plan.root_data())));
  }

  let mut entries = Vec::new();
  visit_bounded_child_entries(root_value, algorithm.hash_length(), entry_version, BTREE_CONVERSION_THRESHOLD, |entry| {
    entries.push(entry);
    Ok(true)
  })
  .map_err(MigrationCaptureReplayErrorV1::Source)?;
  match entries.binary_search_by(|entry| entry.name.as_str().cmp(name)) {
    Ok(index) => match replacement {
      Some(entry) => entries[index] = entry,
      None => {
        entries.remove(index);
      }
    },
    Err(index) => {
      if let Some(entry) = replacement {
        entries.insert(index, entry);
      }
    }
  }
  if entries.len() > BTREE_CONVERSION_THRESHOLD {
    let plan = btree_plan_from_entries(entries, algorithm.hash_length(), &algorithm).map_err(MigrationCaptureReplayErrorV1::Source)?;
    let values = plan.node_writes().map(|write| write.value.clone()).collect::<Vec<_>>();
    publish_directory_values(destination, algorithm, database_id, timestamp_ms, values, Some((plan.root_hash(), plan.root_data())))
  } else {
    let value = serialize_child_entries(&entries, algorithm.hash_length()).map_err(MigrationCaptureReplayErrorV1::Source)?;
    publish_directory_values(destination, algorithm, database_id, timestamp_ms, vec![value], None)
  }
}

fn publish_directory_values(
  destination: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  timestamp_ms: u64,
  values: Vec<Vec<u8>>,
  expected_root: Option<(&[u8], &[u8])>,
) -> Result<PreparedNamespaceTreeV0, MigrationCaptureReplayErrorV1> {
  if values.is_empty() {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_directory_batch", "directory batch is empty"));
  }
  let mut entities = Vec::new();
  entities.try_reserve_exact(values.len()).map_err(|error| {
    MigrationCaptureReplayErrorV1::invalid("migration_replay_directory_allocation", format!("directory allocation failed: {error}"))
  })?;
  for value in values {
    let domain = if is_btree_format(&value) { b"btree:".as_slice() } else { b"dirc:".as_slice() };
    entities.push((digest_parts(algorithm, &[domain, &value]), value));
  }
  if let Some((expected_hash, expected_value)) = expected_root {
    let root = entities
      .last()
      .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_directory_batch", "directory batch lost its root"))?;
    if root.0 != expected_hash || root.1 != expected_value {
      return Err(MigrationCaptureReplayErrorV1::invalid(
        "migration_replay_btree_root_divergence",
        "shared B-tree plan root is not the final immutable publication dependency",
      ));
    }
  }
  for chunk in entities.chunks(IMMUTABLE_BATCH_ENTITIES) {
    let writes = chunk
      .iter()
      .map(|(key, value)| ImmutableEntityWriteV1 {
        entity_version: 0,
        entry_type: EntryTypeV4::DirectoryIndex,
        flags: 0,
        key,
        stored_value: value,
      })
      .collect::<Vec<_>>();
    match destination.publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &database_id,
      entities: &writes,
      publication_timestamp_ms: timestamp_ms,
    }) {
      Ok(_) => {}
      Err(error) if error.committed_receipt().is_some() => {}
      Err(error) => return Err(MigrationCaptureReplayErrorV1::ImmutablePublication(error)),
    }
  }
  let (root_hash, stored_value) = entities
    .last()
    .cloned()
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_directory_batch", "directory batch lost its root"))?;
  Ok(PreparedNamespaceTreeV0 { root_hash, stored_value })
}

pub(super) fn load_destination_tree(
  destination: &V4FirstAuthorityPublisher,
  root_hash: &[u8],
) -> Result<PreparedNamespaceTreeV0, MigrationCaptureReplayErrorV1> {
  let entity = load_destination_directory(destination, root_hash)?;
  Ok(PreparedNamespaceTreeV0 { root_hash: root_hash.to_vec(), stored_value: entity.stored_value })
}

fn load_destination_directory(
  destination: &V4FirstAuthorityPublisher,
  root_hash: &[u8],
) -> Result<super::first_authority::LoadedImmutableEntityV1, MigrationCaptureReplayErrorV1> {
  let entity = destination
    .load_immutable_entity_bounded(root_hash, MAX_ENTITY_TOTAL_BYTES)?
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_destination_entity_missing", hex::encode(root_hash)))?;
  if entity.entry_type != EntryTypeV4::DirectoryIndex
    || entity.entity_version != 0
    || entity.flags != 0
    || entity.compression_algorithm != CompressionAlgorithm::None
    || entity.key != root_hash
  {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_destination_directory",
      "destination directory entity is not canonical immutable v0 content",
    ));
  }
  Ok(entity)
}

fn apply_translation(mut source_entry: ChildEntry, translated: MigrationTranslatedSubtreeV1) -> ChildEntry {
  source_entry.hash = translated.hash;
  source_entry.total_size = translated.total_size;
  if let Some(content_type) = translated.content_type {
    source_entry.content_type = Some(content_type);
  }
  if let Some(created_at) = translated.created_at {
    source_entry.created_at = created_at;
  }
  if let Some(updated_at) = translated.updated_at {
    source_entry.updated_at = updated_at;
  }
  source_entry
}

fn reuse_destination_identity(
  mut source_entry: ChildEntry,
  destination_entry: ChildEntry,
) -> Result<ChildEntry, MigrationCaptureReplayErrorV1> {
  let source_type = EntryType::from_u8(source_entry.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)?;
  let destination_type = EntryType::from_u8(destination_entry.entry_type).map_err(MigrationCaptureReplayErrorV1::Source)?;
  if source_type != destination_type || source_entry.name != destination_entry.name {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_destination_reuse_identity",
      "metadata-only projection cannot reuse a different destination type or name",
    ));
  }
  source_entry.hash = destination_entry.hash;
  source_entry.total_size = destination_entry.total_size;
  if matches!(source_type, EntryType::FileRecord | EntryType::Symlink) {
    source_entry.content_type = destination_entry.content_type;
    source_entry.created_at = destination_entry.created_at;
    source_entry.updated_at = destination_entry.updated_at;
  }
  Ok(source_entry)
}

pub(super) fn namespace_root_for_tree(
  algorithm: HashAlgorithm,
  tree: &PreparedNamespaceTreeV0,
  authority: &MigrationCaptureReplayAuthorityTemplateV1,
) -> Result<Vec<u8>, MigrationCaptureReplayErrorV1> {
  encode_namespace_root(
    &NamespaceRootWriteV1 {
      required_capabilities: authority.required_capabilities,
      namespace_tree_root: tree.root_hash.clone(),
      semantic_state_root: authority.semantic_state.object_id.clone(),
    },
    algorithm,
  )
  .map(|root| root.root_hash)
  .map_err(|error| MigrationCaptureReplayErrorV1::invalid(error.code(), error.to_string()))
}

fn migration_projection_transaction_id(
  algorithm: HashAlgorithm,
  domain: &[u8],
  migration_id: [u8; 16],
  source_sequence: u64,
  source_root: &[u8],
  destination_tree_root: &[u8],
) -> Result<[u8; 16], MigrationCaptureReplayErrorV1> {
  let digest = digest_parts(algorithm, &[domain, &migration_id, &source_sequence.to_le_bytes(), source_root, destination_tree_root]);
  let mut transaction_id = [0u8; 16];
  transaction_id.copy_from_slice(&digest[..16]);
  if transaction_id == [0; 16] {
    return Err(MigrationCaptureReplayErrorV1::invalid("migration_replay_transaction_id", "deterministic replay transaction ID is zero"));
  }
  Ok(transaction_id)
}

fn timestamp_for(floor: u64, semantic: u64, sequence: u64) -> Result<u64, MigrationCaptureReplayErrorV1> {
  let timestamp = floor
    .checked_add(sequence)
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_timestamp", "replay timestamp overflow"))?
    .max(semantic);
  if timestamp == 0 || timestamp > i64::MAX as u64 {
    return Err(MigrationCaptureReplayErrorV1::invalid(
      "migration_replay_timestamp",
      "replay timestamp is outside the signed persistent range",
    ));
  }
  Ok(timestamp)
}

fn monotonic_for(floor: u64, basis_sequence: u64, sequence: u64) -> Result<u64, MigrationCaptureReplayErrorV1> {
  let operation_offset = sequence
    .checked_sub(basis_sequence)
    .and_then(|offset| offset.checked_add(1))
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_monotonic_time", "replay monotonic sequence is invalid"))?;
  floor
    .checked_add(operation_offset)
    .filter(|timestamp| *timestamp != 0)
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_monotonic_time", "replay monotonic timestamp overflow"))
}

fn checked_add(value: u64, increment: u64, name: &'static str) -> Result<u64, MigrationCaptureReplayErrorV1> {
  value
    .checked_add(increment)
    .ok_or_else(|| MigrationCaptureReplayErrorV1::invalid("migration_replay_counter_overflow", format!("{name} overflow")))
}
