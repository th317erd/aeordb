//! Selector-last publication of one exact frozen runtime batch.
//!
//! Large dirty-overlay bytes remain in the private node-local workspace. One
//! immutable task checkpoint names the cumulative workspace head, and the
//! existing IndexOperationControl authority selects that checkpoint last.

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::{HashAlgorithm, VirtualClock};
use crate::engine::memory_coordinator::MemoryReservation;

use super::index_artifact::EncodedImmutableIndexArtifactV1;
use super::index_coordinator::FrozenIndexBatchV1;
use super::index_coordinator_recovery::{IndexCheckpointRootV1, IndexRecoveryOwnerV1, IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1};
use super::index_producer_coordinator::{
  IndexProducerDurableTaskStoreV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1,
  IndexProducerSpillStoreV1, IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
};
use super::index_runtime_owner::{
  IndexRuntimeBatchPublisherV1, IndexRuntimePublicationErrorClassV1, IndexRuntimePublicationErrorV1, IndexRuntimePublicationReceiptV1,
};
use super::index_recovery_store::NativeIndexRecoveryStoreV1;
use super::index_runtime_dirty_overlay_recovery::{
  INDEX_RUNTIME_DIRTY_OVERLAY_PHASE_V1, RecoveredIndexRuntimeDirtyOverlayV1, dirty_overlay_capabilities_v1,
};
use super::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeWorkspaceHeadV1, IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceSelectedHeadV1,
  IndexRuntimeWorkspaceRotationSuccessorV1, IndexRuntimeWorkspaceStoreErrorV1,
};
use super::index_runtime_workspace_rotation::IndexRuntimeImmutableCoverageProofV1;
use super::index_task::{
  ExternalWorkspaceDescriptorWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, decode_mutation_journal,
  encode_index_task_checkpoint,
};

pub use super::index_runtime_dirty_overlay_recovery::INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1;
const MAX_PUBLICATION_CONTEXT_BYTES: usize = 4 * 1024;

pub type NativeIndexRuntimeBatchPublisherV1 = DurableIndexRuntimeBatchPublisherV1<NativeIndexRecoveryStoreV1>;

pub trait IndexRuntimeCheckpointStoreV1: IndexRecoveryStoreV1 {
  fn hash_algorithm(&self) -> HashAlgorithm;
  fn database_id(&self) -> [u8; 16];
  fn destination_physical_instance_id(&self) -> [u8; 16];
}

pub trait IndexRuntimeMutationJournalStoreV1 {
  fn persist_mutation_journal(&mut self, journal: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRuntimePublicationErrorV1>;
}

impl IndexRuntimeCheckpointStoreV1 for NativeIndexRecoveryStoreV1 {
  fn hash_algorithm(&self) -> HashAlgorithm {
    NativeIndexRecoveryStoreV1::hash_algorithm(self)
  }

  fn database_id(&self) -> [u8; 16] {
    NativeIndexRecoveryStoreV1::database_id(self)
  }

  fn destination_physical_instance_id(&self) -> [u8; 16] {
    NativeIndexRecoveryStoreV1::destination_physical_instance_id(self)
  }
}

#[derive(Debug, Error)]
pub enum IndexRuntimeBatchPublisherBuildErrorV1 {
  #[error("index runtime batch publisher identity is invalid: {0}")]
  Invalid(String),
  #[error("index runtime batch publisher selected-state observation failed: {0}")]
  Store(#[source] IndexRecoveryStoreErrorV1),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspacePublicationIdentityV1 {
  RuntimeBatch { batch_id: u64 },
  ProducerTask { operation_id: [u8; 16] },
  Rotation { rotation_sequence: u64 },
}

#[derive(Clone)]
struct PendingWorkspacePublicationV1 {
  identity: WorkspacePublicationIdentityV1,
  timestamp_ms: u64,
  next: IndexCheckpointRootV1,
  checkpoint: EncodedImmutableIndexArtifactV1,
  selected_workspace: Option<IndexRuntimeWorkspaceSelectedHeadV1>,
}

#[derive(Clone, Copy)]
struct PreparedWorkspacePublicationV1 {
  identity: WorkspacePublicationIdentityV1,
  object_id: [u8; 16],
  timestamp_ms: u64,
}

struct SelectedWorkspacePublicationV1 {
  next: IndexCheckpointRootV1,
}

pub struct DurableIndexRuntimeBatchPublisherV1<Store> {
  hash_algorithm: HashAlgorithm,
  owner: IndexRecoveryOwnerV1,
  source_root: Vec<u8>,
  generation: u64,
  started_at_ms: u64,
  selected_updated_at_ms: u64,
  selected: Option<IndexCheckpointRootV1>,
  selected_workspace: Option<IndexRuntimeWorkspaceSelectedHeadV1>,
  prepared: Option<PreparedWorkspacePublicationV1>,
  pending: Option<PendingWorkspacePublicationV1>,
  pending_replacement_workspace: Option<DurableIndexRuntimeWorkspaceV1>,
  workspace: DurableIndexRuntimeWorkspaceV1,
  store: Store,
  cancellation: CancellationToken,
  clock: Arc<dyn VirtualClock>,
  _recovered_reservation: Option<MemoryReservation>,
}

impl<Store: IndexRuntimeCheckpointStoreV1> DurableIndexRuntimeBatchPublisherV1<Store> {
  #[allow(clippy::too_many_arguments)]
  pub fn new_unselected(
    hash_algorithm: HashAlgorithm,
    owner: IndexRecoveryOwnerV1,
    source_root: Vec<u8>,
    generation: u64,
    started_at_ms: u64,
    workspace: DurableIndexRuntimeWorkspaceV1,
    mut store: Store,
    cancellation: CancellationToken,
    clock: Arc<dyn VirtualClock>,
  ) -> Result<Self, IndexRuntimeBatchPublisherBuildErrorV1> {
    let workspace_identity = workspace.identity();
    if generation == 0
      || started_at_ms == 0
      || cancellation.is_cancelled()
      || owner.database_id() != workspace_identity.database_id()
      || hash_algorithm != workspace_identity.hash_algorithm()
      || store.database_id() != workspace_identity.database_id()
      || store.destination_physical_instance_id() != workspace_identity.destination_physical_instance_id()
      || store.hash_algorithm() != workspace_identity.hash_algorithm()
      || owner.index_id().len() != hash_algorithm.hash_length()
      || source_root.len() != hash_algorithm.hash_length()
      || source_root.iter().all(|byte| *byte == 0)
      || workspace.head().is_some()
    {
      return Err(IndexRuntimeBatchPublisherBuildErrorV1::Invalid(
        "fresh publisher identity, source root, generation, cancellation, or workspace head is invalid".to_string(),
      ));
    }
    if store.load_selected(&owner).map_err(IndexRuntimeBatchPublisherBuildErrorV1::Store)?.is_some() {
      return Err(IndexRuntimeBatchPublisherBuildErrorV1::Invalid(
        "fresh publisher cannot replace an existing selected checkpoint without bounded recovery".to_string(),
      ));
    }
    Ok(Self {
      hash_algorithm,
      owner,
      source_root,
      generation,
      started_at_ms,
      selected_updated_at_ms: started_at_ms,
      selected: None,
      selected_workspace: None,
      prepared: None,
      pending: None,
      pending_replacement_workspace: None,
      workspace,
      store,
      cancellation,
      clock,
      _recovered_reservation: None,
    })
  }

  pub fn new_resumed(
    recovered: Box<RecoveredIndexRuntimeDirtyOverlayV1>,
    mut store: Store,
    clock: Arc<dyn VirtualClock>,
  ) -> Result<Self, IndexRuntimeBatchPublisherBuildErrorV1> {
    let parts = (*recovered).into_parts();
    let workspace_identity = parts.workspace.identity();
    if parts.cancellation.is_cancelled()
      || parts.owner.database_id() != workspace_identity.database_id()
      || store.database_id() != workspace_identity.database_id()
      || store.destination_physical_instance_id() != workspace_identity.destination_physical_instance_id()
      || store.hash_algorithm() != workspace_identity.hash_algorithm()
      || parts.owner.index_id().len() != workspace_identity.hash_algorithm().hash_length()
      || parts.source_root.len() != workspace_identity.hash_algorithm().hash_length()
      || parts.source_root.iter().all(|byte| *byte == 0)
      || parts.generation == 0
      || parts.started_at_ms == 0
      || parts.updated_at_ms < parts.started_at_ms
    {
      return Err(IndexRuntimeBatchPublisherBuildErrorV1::Invalid(
        "resumed publisher identity, source root, generation, cancellation, or selected state is invalid".to_string(),
      ));
    }
    let observed = store.load_selected(&parts.owner).map_err(IndexRuntimeBatchPublisherBuildErrorV1::Store)?;
    if parts.cancellation.is_cancelled() {
      return Err(IndexRuntimeBatchPublisherBuildErrorV1::Invalid(
        "resumed publisher was canceled while rechecking selected checkpoint authority".to_string(),
      ));
    }
    if observed.as_ref() != Some(&parts.selected) {
      return Err(IndexRuntimeBatchPublisherBuildErrorV1::Invalid(
        "resumed publisher selected checkpoint changed after bounded recovery".to_string(),
      ));
    }
    let selected_workspace = parts.workspace.head().map(IndexRuntimeWorkspaceHeadV1::selected_descriptor);
    Ok(Self {
      hash_algorithm: workspace_identity.hash_algorithm(),
      owner: parts.owner,
      source_root: parts.source_root,
      generation: parts.generation,
      started_at_ms: parts.started_at_ms,
      selected_updated_at_ms: parts.updated_at_ms,
      selected: Some(parts.selected),
      selected_workspace,
      prepared: None,
      pending: None,
      pending_replacement_workspace: None,
      workspace: parts.workspace,
      store,
      cancellation: parts.cancellation,
      clock,
      _recovered_reservation: Some(parts.reservation),
    })
  }

  pub fn workspace_head(&self) -> Option<&IndexRuntimeWorkspaceHeadV1> {
    self.workspace.head()
  }

  pub fn workspace_path(&self) -> &Path {
    self.workspace.workspace_path()
  }

  pub fn runtime_id(&self) -> [u8; 16] {
    self.workspace.identity().runtime_id()
  }

  pub(crate) fn workspace_identity(&self) -> IndexRuntimeWorkspaceIdentityV1 {
    self.workspace.identity()
  }

  pub(crate) fn selected_checkpoint(&self) -> Option<&IndexCheckpointRootV1> {
    self.selected.as_ref()
  }

  pub fn build_rotation_successor(
    &self,
    coverage: IndexRuntimeImmutableCoverageProofV1<'_>,
    pending_operation_ids: &[[u8; 16]],
  ) -> Result<IndexRuntimeWorkspaceRotationSuccessorV1, IndexRuntimePublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(cancelled("index_workspace_rotation_cancelled", "workspace rotation was canceled before successor construction"));
    }
    if self.prepared.is_some() {
      return Err(corrupt(
        "index_workspace_rotation_prepared",
        "workspace rotation cannot begin while an ordinary publication owns an unselected prefix",
      ));
    }
    let rotation_sequence = self.expected_global_sequence()?;
    if self.pending.as_ref().is_some_and(|pending| pending.identity != (WorkspacePublicationIdentityV1::Rotation { rotation_sequence })) {
      return Err(corrupt("index_workspace_rotation_pending", "workspace rotation cannot replace a different pending publication"));
    }
    let selected_workspace = self
      .selected_workspace
      .as_ref()
      .ok_or_else(|| corrupt("index_workspace_rotation_empty", "an unselected or clean workspace has no cumulative history to rotate"))?;
    if self.workspace.head().map(IndexRuntimeWorkspaceHeadV1::selected_descriptor).as_ref() != Some(selected_workspace) {
      return Err(corrupt(
        "index_workspace_rotation_head",
        "workspace rotation predecessor disagrees with the selected local workspace descriptor",
      ));
    }
    self
      .workspace
      .build_rotation_successor(rotation_sequence, self.generation, &self.source_root, coverage, pending_operation_ids)
      .map_err(map_workspace_before_selection)
  }

  pub fn publish_rotation_successor(
    &mut self,
    rotated: IndexRuntimeWorkspaceRotationSuccessorV1,
  ) -> Result<IndexCheckpointRootV1, IndexRuntimePublicationErrorV1> {
    let identity = WorkspacePublicationIdentityV1::Rotation { rotation_sequence: rotated.rotation_sequence() };
    if let Some(selected) = self.resolve_already_selected_pending(identity)? {
      return Ok(selected.next);
    }
    if self.cancellation.is_cancelled() {
      return Err(cancelled("index_workspace_rotation_cancelled", "workspace rotation was canceled before checkpoint publication"));
    }
    self.require_expected_selection()?;
    if self.prepared.is_some() || self.pending.is_some() || self.pending_replacement_workspace.is_some() {
      return Err(corrupt("index_workspace_rotation_state", "workspace rotation encountered another prepared or pending publication"));
    }
    let expected_sequence = self.expected_global_sequence()?;
    let selected_workspace = self
      .selected_workspace
      .as_ref()
      .ok_or_else(|| corrupt("index_workspace_rotation_empty", "an unselected or clean workspace has no selected predecessor"))?;
    if rotated.rotation_sequence() != expected_sequence || rotated.predecessor_selected() != selected_workspace {
      return Err(corrupt(
        "index_workspace_rotation_predecessor",
        "rotation successor does not bind the exact selected checkpoint succession and local predecessor",
      ));
    }
    let summary = rotated.summary();
    let successor_workspace = rotated.into_successor_workspace();
    if successor_workspace.identity() != self.workspace.identity() {
      return Err(corrupt("index_workspace_rotation_identity", "rotation successor belongs to another runtime workspace identity"));
    }
    let successor_selected = successor_workspace.head().map(IndexRuntimeWorkspaceHeadV1::selected_descriptor);
    if successor_selected.as_ref().map_or(0, IndexRuntimeWorkspaceSelectedHeadV1::durable_sequence) != summary.retained_objects() {
      return Err(corrupt(
        "index_workspace_rotation_summary",
        "rotation successor local sequence disagrees with the retained-work summary",
      ));
    }
    successor_workspace.validate_selected_state(successor_selected.as_ref()).map_err(map_workspace_before_selection)?;
    // Rotation adds no new logical work. Retaining the selected authority time
    // makes an unselected checkpoint byte-exact across process restart.
    let timestamp_ms = self.selected_updated_at_ms;
    if timestamp_ms == 0 {
      return Err(corrupt("index_workspace_clock", "workspace rotation clock returned zero"));
    }
    let checkpoint = self.encode_checkpoint(expected_sequence, successor_selected.as_ref(), timestamp_ms)?;
    let next = IndexCheckpointRootV1::new(expected_sequence, checkpoint.key.clone())
      .map_err(|error| corrupt("index_workspace_checkpoint_root", error.to_string()))?;
    self.pending = Some(PendingWorkspacePublicationV1 {
      identity,
      timestamp_ms,
      next: next.clone(),
      checkpoint,
      selected_workspace: successor_selected,
    });
    self.pending_replacement_workspace = Some(successor_workspace);
    self.persist_and_select_pending(identity)?;
    Ok(next)
  }

  /// Persist one immutable mutation journal and establish its data barrier
  /// without advancing the dirty-overlay selector. A producer task references
  /// the journal only after this succeeds.
  pub fn persist_mutation_journal(&mut self, journal: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRuntimePublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(cancelled("mutation_journal_cancelled", "mutation journal persistence was canceled before immutable publication"));
    }
    let decoded = decode_mutation_journal(&journal.value, self.hash_algorithm)
      .map_err(|error| corrupt("mutation_journal_format", format!("mutation journal bytes are invalid: {error}")))?;
    if decoded.key != journal.key {
      return Err(corrupt(
        "mutation_journal_identity",
        "mutation journal key disagrees with the identity derived from its validated bytes",
      ));
    }
    self.store.put_immutable(journal).map_err(map_store_before_selection)?;
    self.store.sync_immutable().map_err(map_store_before_selection)?;
    if self.cancellation.is_cancelled() {
      return Err(cancelled(
        "mutation_journal_cancelled",
        "mutation journal persistence was canceled after immutable durability and before task admission",
      ));
    }
    Ok(())
  }

  fn publish_exact(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(cancelled("runtime_batch_cancelled", "runtime batch publication was canceled before workspace append"));
    }
    if batch.coordinator_id() != self.workspace.identity().runtime_id() || (batch.records().is_empty() && batch.transitions().is_empty()) {
      return Err(corrupt("runtime_batch_identity", "frozen batch does not belong to this workspace runtime"));
    }
    let identity = WorkspacePublicationIdentityV1::RuntimeBatch { batch_id: batch.batch_id() };
    if let Some(selected) = self.resolve_already_selected_pending(identity)? {
      return Ok(runtime_receipt(batch, &selected));
    }
    self.require_expected_selection()?;

    let prepared = self.prepare_publication(identity, runtime_batch_object_id(batch))?;
    let object_id = prepared.object_id;
    let head = if batch.transitions().is_empty() {
      self.workspace.append_runtime_batch(object_id, prepared.timestamp_ms, batch)
    } else {
      self.workspace.append_frozen_runtime_batch_v2(object_id, prepared.timestamp_ms, batch)
    }
    .map_err(map_workspace_before_selection)?;
    let selected = self.publish_workspace_head(identity, &head)?;
    Ok(runtime_receipt(batch, &selected))
  }

  fn spill_exact(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexRuntimePublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(cancelled("producer_spill_cancelled", "producer spill was canceled before workspace append"));
    }
    let identity = WorkspacePublicationIdentityV1::ProducerTask { operation_id: task.operation_id() };
    if let Some(selected) = self.resolve_already_selected_pending(identity)? {
      return producer_spill_receipt(task.operation_id(), &selected.next);
    }
    self.require_expected_selection()?;

    let object_id = producer_task_object_id(self.workspace.identity().runtime_id(), task.operation_id());
    let request = IndexProducerTaskRequestV1 {
      operation_id: task.operation_id(),
      kind: task.kind(),
      publication_sequence: task.publication_sequence(),
      namespace_root_before: task.namespace_root_before(),
      namespace_root_after: task.namespace_root_after(),
      semantic_state_root: task.semantic_state_root(),
      journal_head: task.journal_head(),
      scope: task.scope(),
    };
    if let Some(selected_workspace) = self.selected_workspace.clone() {
      let selected = self
        .selected
        .as_ref()
        .ok_or_else(|| corrupt("producer_spill_selected_missing", "selected workspace has no selected checkpoint root"))?;
      if self.workspace.selected_contains_producer_task(&selected_workspace, object_id, &request).map_err(map_workspace_before_selection)? {
        return IndexProducerSpillReceiptV1::new(task.operation_id(), selected.checkpoint_key.clone())
          .map_err(|error| corrupt(error.code(), error.context()));
      }
    }

    let prepared = self.prepare_publication(identity, object_id)?;
    let head =
      self.workspace.append_producer_task(prepared.object_id, prepared.timestamp_ms, &request).map_err(map_workspace_before_selection)?;
    let selected = self.publish_workspace_head(identity, &head)?;
    producer_spill_receipt(task.operation_id(), &selected.next)
  }

  fn publish_workspace_head(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
    head: &IndexRuntimeWorkspaceHeadV1,
  ) -> Result<SelectedWorkspacePublicationV1, IndexRuntimePublicationErrorV1> {
    let timestamp_ms = head.latest_object_created_at_ms();
    if timestamp_ms == 0 || timestamp_ms < self.selected_updated_at_ms {
      return Err(corrupt("index_workspace_timestamp", "durable workspace object timestamp is zero or predates the selected checkpoint"));
    }
    let expected_sequence = self.expected_global_sequence()?;
    let expected_local_sequence = self.expected_local_sequence()?;
    if head.manifest_sequence() != expected_local_sequence || head.cumulative_object_count() != expected_local_sequence {
      return Err(corrupt(
        "index_workspace_sequence",
        "cumulative workspace sequence or object count disagrees with local workspace succession",
      ));
    }
    let selected_workspace = head.selected_descriptor();
    let checkpoint = self.encode_checkpoint(expected_sequence, Some(&selected_workspace), timestamp_ms)?;
    let next = IndexCheckpointRootV1::new(expected_sequence, checkpoint.key.clone())
      .map_err(|error| corrupt("index_workspace_checkpoint_root", error.to_string()))?;
    self.pending =
      Some(PendingWorkspacePublicationV1 { identity, timestamp_ms, next, checkpoint, selected_workspace: Some(selected_workspace) });
    self.persist_and_select_pending(identity)
  }

  fn encode_checkpoint(
    &self,
    checkpoint_sequence: u64,
    selected_workspace: Option<&IndexRuntimeWorkspaceSelectedHeadV1>,
    timestamp_ms: u64,
  ) -> Result<super::index_artifact::EncodedImmutableIndexArtifactV1, IndexRuntimePublicationErrorV1> {
    let persisted_path = selected_workspace.map(|selected| persisted_workspace_path(selected.workspace_path())).transpose()?;
    let external = match (selected_workspace, persisted_path.as_deref()) {
      (Some(selected), Some(path)) => Some(ExternalWorkspaceDescriptorWriteV1 {
        workspace_id: selected.workspace_id(),
        manifest_digest: selected.manifest_digest(),
        durable_sequence: selected.durable_sequence(),
        durable_bytes: selected.durable_bytes(),
        path,
      }),
      (None, None) => None,
      _ => return Err(corrupt("runtime_batch_workspace_path", "workspace descriptor path preparation is inconsistent")),
    };
    let completed_work = checkpoint_sequence;
    let required_capabilities = dirty_overlay_capabilities_v1();
    encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
      hash_algorithm: self.hash_algorithm,
      task_id: self.owner.operation_id(),
      checkpoint_sequence,
      generation: self.generation,
      task_kind: IndexTaskKindV1::Reconcile,
      state: IndexTaskStateV1::Running,
      phase: INDEX_RUNTIME_DIRTY_OVERLAY_PHASE_V1,
      required_capabilities: &required_capabilities,
      started_at_ms: self.started_at_ms,
      updated_at_ms: timestamp_ms,
      source_root: &self.source_root,
      target_root: None,
      primary_id: Some(self.owner.index_id()),
      journal_head: None,
      journal_floor_sequence: 0,
      journal_audited_through: 0,
      next_document_ordinal: 0,
      completed_work,
      total_work_hint: completed_work,
      resume_key: INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1,
      attachments: &[],
      external,
    })
    .map_err(|error| corrupt("runtime_batch_checkpoint_encode", error.to_string()))
  }

  fn prepare_publication(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
    object_id: [u8; 16],
  ) -> Result<PreparedWorkspacePublicationV1, IndexRuntimePublicationErrorV1> {
    if let Some(prepared) = self.prepared {
      if prepared.identity == identity && prepared.object_id == object_id {
        return Ok(prepared);
      }
      return Err(corrupt(
        "index_workspace_prepared_identity",
        "another runtime batch or producer task already owns the prepared workspace prefix",
      ));
    }
    let timestamp_ms = self.clock.now_ms().max(self.selected_updated_at_ms);
    if timestamp_ms == 0 {
      return Err(corrupt("index_workspace_clock", "workspace publication clock returned zero"));
    }
    let prepared = PreparedWorkspacePublicationV1 { identity, object_id, timestamp_ms };
    self.prepared = Some(prepared);
    Ok(prepared)
  }

  fn expected_global_sequence(&self) -> Result<u64, IndexRuntimePublicationErrorV1> {
    self
      .selected
      .as_ref()
      .map_or(Some(1), |selected| selected.checkpoint_sequence.checked_add(1))
      .ok_or_else(|| corrupt("index_workspace_sequence", "selected checkpoint sequence is exhausted"))
  }

  fn expected_local_sequence(&self) -> Result<u64, IndexRuntimePublicationErrorV1> {
    self
      .selected_workspace
      .as_ref()
      .map_or(Some(1), |selected| selected.durable_sequence().checked_add(1))
      .ok_or_else(|| corrupt("index_workspace_local_sequence", "selected workspace sequence is exhausted"))
  }

  fn persist_and_select_pending(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
  ) -> Result<SelectedWorkspacePublicationV1, IndexRuntimePublicationErrorV1> {
    let pending = self
      .pending
      .clone()
      .ok_or_else(|| corrupt("index_workspace_pending_missing", "checkpoint selection has no retained pending publication"))?;
    if pending.identity != identity {
      return Err(corrupt("index_workspace_pending_identity", "checkpoint selection belongs to another pending publication"));
    }
    self.store.put_immutable(&pending.checkpoint).map_err(map_store_before_selection)?;
    self.store.sync_immutable().map_err(map_store_before_selection)?;
    if self.cancellation.is_cancelled() {
      return Err(cancelled(
        "index_workspace_cancelled",
        "workspace publication was canceled after immutable checkpoint durability and before selection",
      ));
    }
    match self.store.publish_selected_synced(&self.owner, self.selected.as_ref(), &pending.next) {
      Ok(()) => self.complete_selected(identity, pending.next, pending.timestamp_ms),
      Err(selection_error) => self.resolve_selection_error(identity, pending.next, pending.timestamp_ms, selection_error),
    }
  }

  fn resolve_already_selected_pending(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
  ) -> Result<Option<SelectedWorkspacePublicationV1>, IndexRuntimePublicationErrorV1> {
    let Some(pending) = self.pending.clone() else {
      return Ok(None);
    };
    if pending.identity != identity {
      return Err(corrupt("index_workspace_pending_identity", "pending checkpoint belongs to another workspace publication"));
    }
    let observed = self
      .store
      .load_selected(&self.owner)
      .map_err(|error| commit_unknown("index_workspace_pending_reopen", format!("pending selector cannot be reopened: {error}")))?;
    if observed.as_ref() == Some(&pending.next) {
      self.validate_selected_successor(&pending)?;
      return self.complete_selected(identity, pending.next, pending.timestamp_ms).map(Some);
    }
    if observed == self.selected {
      self.persist_and_select_pending(identity).map(Some)
    } else {
      Err(commit_unknown(
        "index_workspace_pending_foreign_selector",
        "pending selector resolved to neither the prior nor exact successor checkpoint",
      ))
    }
  }

  fn require_expected_selection(&mut self) -> Result<(), IndexRuntimePublicationErrorV1> {
    let observed = self.store.load_selected(&self.owner).map_err(map_store_before_selection)?;
    if observed == self.selected {
      Ok(())
    } else {
      Err(corrupt("index_workspace_selection_changed", "selected checkpoint changed outside the cumulative workspace publisher"))
    }
  }

  fn resolve_selection_error(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
    next: IndexCheckpointRootV1,
    timestamp_ms: u64,
    selection_error: IndexRecoveryStoreErrorV1,
  ) -> Result<SelectedWorkspacePublicationV1, IndexRuntimePublicationErrorV1> {
    let observed = self.store.load_selected(&self.owner).map_err(|reopen_error| {
      commit_unknown(
        "index_workspace_selector_unreadable",
        format!("selector publication failed ({selection_error}); exact reopen also failed ({reopen_error})"),
      )
    })?;
    if observed == self.selected {
      return Err(retryable(
        "index_workspace_selector_unselected",
        format!("selector publication was definitely unselected: {selection_error}"),
      ));
    }
    if observed.as_ref() == Some(&next) {
      let pending = self
        .pending
        .clone()
        .ok_or_else(|| corrupt("index_workspace_pending_missing", "selected successor has no retained checkpoint evidence"))?;
      self.validate_selected_successor(&pending)?;
      return self.complete_selected(identity, next, timestamp_ms);
    }
    Err(commit_unknown(
      "index_workspace_selector_foreign",
      format!("selector publication failed and reopened to a foreign root: {selection_error}"),
    ))
  }

  fn validate_selected_successor(&mut self, pending: &PendingWorkspacePublicationV1) -> Result<(), IndexRuntimePublicationErrorV1> {
    let expected_length = u64::try_from(pending.checkpoint.value.len())
      .map_err(|error| corrupt("index_workspace_checkpoint_length", format!("checkpoint length exceeds u64: {error}")))?;
    let observed_length = self.store.immutable_length(&pending.next.checkpoint_key).map_err(|error| {
      commit_unknown("index_workspace_checkpoint_length", format!("selected successor length cannot be reopened: {error}"))
    })?;
    if observed_length != Some(expected_length) {
      return Err(commit_unknown(
        "index_workspace_checkpoint_missing",
        "selected successor checkpoint is absent or has a different length",
      ));
    }
    let observed = self
      .store
      .load_immutable(&pending.next.checkpoint_key, expected_length)
      .map_err(|error| commit_unknown("index_workspace_checkpoint_reopen", format!("selected successor cannot be reopened: {error}")))?;
    if observed.as_deref() != Some(pending.checkpoint.value.as_slice()) {
      return Err(commit_unknown(
        "index_workspace_checkpoint_changed",
        "selected successor checkpoint bytes disagree with the exact pending artifact",
      ));
    }
    let workspace = match self.pending_replacement_workspace.as_ref() {
      Some(replacement) => replacement,
      None => &self.workspace,
    };
    workspace.validate_selected_state(pending.selected_workspace.as_ref()).map_err(|error| {
      commit_unknown("index_workspace_head", format!("selected successor workspace closure cannot be reopened exactly: {error}"))
    })?;
    Ok(())
  }

  fn complete_selected(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
    next: IndexCheckpointRootV1,
    timestamp_ms: u64,
  ) -> Result<SelectedWorkspacePublicationV1, IndexRuntimePublicationErrorV1> {
    if self.pending.as_ref().is_none_or(|pending| pending.identity != identity || pending.next != next) {
      return Err(corrupt("index_workspace_pending_mismatch", "selected successor does not match the retained workspace publication"));
    }
    let pending = self
      .pending
      .as_ref()
      .ok_or_else(|| corrupt("index_workspace_pending_missing", "selected successor has no retained publication evidence"))?
      .clone();
    let candidate_workspace = match self.pending_replacement_workspace.as_ref() {
      Some(replacement) => replacement,
      None => &self.workspace,
    };
    if candidate_workspace.head().map(IndexRuntimeWorkspaceHeadV1::selected_descriptor).as_ref() != pending.selected_workspace.as_ref() {
      return Err(corrupt(
        "index_workspace_head",
        "selected successor in-memory workspace state disagrees with its retained checkpoint descriptor",
      ));
    }
    let is_rotation = matches!(identity, WorkspacePublicationIdentityV1::Rotation { .. });
    if is_rotation != self.pending_replacement_workspace.is_some() {
      return Err(corrupt(
        "index_workspace_replacement_mismatch",
        "selected successor replacement ownership disagrees with its publication kind",
      ));
    }
    if let Some(replacement) = self.pending_replacement_workspace.take() {
      self.workspace = replacement;
    }
    self.selected = Some(next.clone());
    self.selected_workspace = pending.selected_workspace;
    self.selected_updated_at_ms = timestamp_ms;
    self.prepared = None;
    self.pending = None;
    Ok(SelectedWorkspacePublicationV1 { next })
  }
}

impl<Store: IndexRuntimeCheckpointStoreV1> IndexRuntimeBatchPublisherV1 for DurableIndexRuntimeBatchPublisherV1<Store> {
  fn publish(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    self.publish_exact(batch)
  }
}

impl<Store: IndexRuntimeCheckpointStoreV1> IndexProducerSpillStoreV1 for DurableIndexRuntimeBatchPublisherV1<Store> {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    self.spill_exact(task, reason).map_err(|error| IndexProducerSpillErrorV1::new(error.code(), error.to_string()))
  }
}

impl<Store: IndexRuntimeCheckpointStoreV1> IndexProducerDurableTaskStoreV1 for DurableIndexRuntimeBatchPublisherV1<Store> {
  fn persist_task(&mut self, task: IndexProducerTaskViewV1<'_>) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    self
      .spill_exact(task, IndexProducerSpillReasonV1::AdmissionPressure)
      .map_err(|error| IndexProducerSpillErrorV1::new(error.code(), error.to_string()))
  }
}

impl<Store: IndexRuntimeCheckpointStoreV1> IndexRuntimeMutationJournalStoreV1 for DurableIndexRuntimeBatchPublisherV1<Store> {
  fn persist_mutation_journal(&mut self, journal: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRuntimePublicationErrorV1> {
    DurableIndexRuntimeBatchPublisherV1::persist_mutation_journal(self, journal)
  }
}

fn runtime_receipt(batch: &FrozenIndexBatchV1, selected: &SelectedWorkspacePublicationV1) -> IndexRuntimePublicationReceiptV1 {
  IndexRuntimePublicationReceiptV1 {
    batch_id: batch.batch_id(),
    attempt_id: batch.attempt_id(),
    published_records: batch.records().len() as u64,
    publication_bytes: batch.publication_bytes(),
    checkpoint_sequence: selected.next.checkpoint_sequence,
  }
}

fn producer_spill_receipt(
  operation_id: [u8; 16],
  selected: &IndexCheckpointRootV1,
) -> Result<IndexProducerSpillReceiptV1, IndexRuntimePublicationErrorV1> {
  IndexProducerSpillReceiptV1::new(operation_id, selected.checkpoint_key.clone()).map_err(|error| corrupt(error.code(), error.context()))
}

fn runtime_batch_object_id(batch: &FrozenIndexBatchV1) -> [u8; 16] {
  let mut digest = blake3::Hasher::new();
  digest.update(b"aeordb-v4-index-runtime-batch-object-v1\0");
  digest.update(&batch.coordinator_id());
  digest.update(&batch.batch_id().to_le_bytes());
  let mut object_id = [0; 16];
  object_id.copy_from_slice(&digest.finalize().as_bytes()[..16]);
  object_id
}

fn producer_task_object_id(runtime_id: [u8; 16], operation_id: [u8; 16]) -> [u8; 16] {
  let mut digest = blake3::Hasher::new();
  digest.update(b"aeordb-v4-index-producer-task-object-v1\0");
  digest.update(&runtime_id);
  digest.update(&operation_id);
  let mut object_id = [0; 16];
  object_id.copy_from_slice(&digest.finalize().as_bytes()[..16]);
  object_id
}

fn persisted_workspace_path(path: &Path) -> Result<String, IndexRuntimePublicationErrorV1> {
  let native = path.to_str().ok_or_else(|| corrupt("runtime_batch_workspace_path", "workspace path is not canonical UTF-8"))?;
  #[cfg(windows)]
  let native = native.replace('\\', "/");
  #[cfg(not(windows))]
  let native = native.to_string();
  if super::native_path::canonical_persisted_native_path(&native) {
    Ok(native)
  } else {
    Err(corrupt("runtime_batch_workspace_path", "workspace path cannot be represented by the frozen external descriptor"))
  }
}

fn map_workspace_before_selection(error: IndexRuntimeWorkspaceStoreErrorV1) -> IndexRuntimePublicationErrorV1 {
  match error {
    IndexRuntimeWorkspaceStoreErrorV1::Canceled => cancelled("runtime_batch_workspace_cancelled", error.to_string()),
    IndexRuntimeWorkspaceStoreErrorV1::Invalid(_)
    | IndexRuntimeWorkspaceStoreErrorV1::Path(_)
    | IndexRuntimeWorkspaceStoreErrorV1::State(_)
    | IndexRuntimeWorkspaceStoreErrorV1::Format(_) => corrupt("runtime_batch_workspace_invalid", error.to_string()),
    other => retryable("runtime_batch_workspace_retry", other.to_string()),
  }
}

fn map_store_before_selection(error: IndexRecoveryStoreErrorV1) -> IndexRuntimePublicationErrorV1 {
  retryable("runtime_batch_store_retry", error.to_string())
}

fn retryable(code: &'static str, context: impl Into<String>) -> IndexRuntimePublicationErrorV1 {
  IndexRuntimePublicationErrorV1::new(
    IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection,
    code,
    bounded_publication_context(context),
  )
}

fn cancelled(code: &'static str, context: impl Into<String>) -> IndexRuntimePublicationErrorV1 {
  IndexRuntimePublicationErrorV1::new(
    IndexRuntimePublicationErrorClassV1::CancelledBeforeSelection,
    code,
    bounded_publication_context(context),
  )
}

fn commit_unknown(code: &'static str, context: impl Into<String>) -> IndexRuntimePublicationErrorV1 {
  IndexRuntimePublicationErrorV1::new(IndexRuntimePublicationErrorClassV1::CommitUnknown, code, bounded_publication_context(context))
}

fn corrupt(code: &'static str, context: impl Into<String>) -> IndexRuntimePublicationErrorV1 {
  IndexRuntimePublicationErrorV1::new(IndexRuntimePublicationErrorClassV1::Corrupt, code, bounded_publication_context(context))
}

fn bounded_publication_context(context: impl Into<String>) -> String {
  let mut context = context.into();
  if context.is_empty() {
    return "index runtime publication failed without diagnostic context".to_string();
  }
  if context.len() <= MAX_PUBLICATION_CONTEXT_BYTES {
    return context;
  }
  let mut boundary = MAX_PUBLICATION_CONTEXT_BYTES;
  while !context.is_char_boundary(boundary) {
    boundary -= 1;
  }
  context.truncate(boundary);
  context
}
