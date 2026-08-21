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
  IndexProducerSpillErrorV1, IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1,
  IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
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
  IndexRuntimeWorkspaceStoreErrorV1,
};
use super::index_task::{
  ExternalWorkspaceDescriptorWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, encode_index_task_checkpoint,
};

pub use super::index_runtime_dirty_overlay_recovery::INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1;
const MAX_PUBLICATION_CONTEXT_BYTES: usize = 4 * 1024;

pub type NativeIndexRuntimeBatchPublisherV1 = DurableIndexRuntimeBatchPublisherV1<NativeIndexRecoveryStoreV1>;

pub trait IndexRuntimeCheckpointStoreV1: IndexRecoveryStoreV1 {
  fn hash_algorithm(&self) -> HashAlgorithm;
  fn database_id(&self) -> [u8; 16];
  fn destination_physical_instance_id(&self) -> [u8; 16];
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
}

#[derive(Clone)]
struct PendingWorkspacePublicationV1 {
  identity: WorkspacePublicationIdentityV1,
  timestamp_ms: u64,
  next: IndexCheckpointRootV1,
  checkpoint: EncodedImmutableIndexArtifactV1,
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
      || parts.workspace.head().is_none()
    {
      return Err(IndexRuntimeBatchPublisherBuildErrorV1::Invalid(
        "resumed publisher identity, source root, generation, cancellation, or workspace head is invalid".to_string(),
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
    let selected_workspace = parts.workspace.head().map(IndexRuntimeWorkspaceHeadV1::selected_descriptor).ok_or_else(|| {
      IndexRuntimeBatchPublisherBuildErrorV1::Invalid("resumed publisher has no selected workspace descriptor".to_string())
    })?;
    if selected_workspace.durable_sequence() != parts.selected.checkpoint_sequence {
      return Err(IndexRuntimeBatchPublisherBuildErrorV1::Invalid(
        "resumed publisher checkpoint and workspace sequences disagree".to_string(),
      ));
    }
    Ok(Self {
      hash_algorithm: workspace_identity.hash_algorithm(),
      owner: parts.owner,
      source_root: parts.source_root,
      generation: parts.generation,
      started_at_ms: parts.started_at_ms,
      selected_updated_at_ms: parts.updated_at_ms,
      selected: Some(parts.selected),
      selected_workspace: Some(selected_workspace),
      prepared: None,
      pending: None,
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

  fn publish_exact(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(cancelled("runtime_batch_cancelled", "runtime batch publication was canceled before workspace append"));
    }
    if batch.coordinator_id() != self.workspace.identity().runtime_id() || batch.records().is_empty() {
      return Err(corrupt("runtime_batch_identity", "frozen batch does not belong to this workspace runtime"));
    }
    let identity = WorkspacePublicationIdentityV1::RuntimeBatch { batch_id: batch.batch_id() };
    if let Some(selected) = self.resolve_already_selected_pending(identity)? {
      return Ok(runtime_receipt(batch, &selected));
    }
    self.require_expected_selection()?;

    let prepared = self.prepare_publication(identity, runtime_batch_object_id(batch))?;
    let object_id = prepared.object_id;
    let head = self.workspace.append_runtime_batch(object_id, prepared.timestamp_ms, batch).map_err(map_workspace_before_selection)?;
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
    let expected_sequence = self
      .selected
      .as_ref()
      .map_or(Some(1), |selected| selected.checkpoint_sequence.checked_add(1))
      .ok_or_else(|| corrupt("index_workspace_sequence", "selected checkpoint sequence is exhausted"))?;
    if head.manifest_sequence() != expected_sequence || head.cumulative_object_count() != expected_sequence {
      return Err(corrupt("index_workspace_sequence", "cumulative workspace sequence or object count disagrees with selector succession"));
    }
    let checkpoint = self.encode_checkpoint(head, timestamp_ms)?;
    let next = IndexCheckpointRootV1::new(expected_sequence, checkpoint.key.clone())
      .map_err(|error| corrupt("index_workspace_checkpoint_root", error.to_string()))?;
    self.pending = Some(PendingWorkspacePublicationV1 { identity, timestamp_ms, next: next.clone(), checkpoint: checkpoint.clone() });

    self.store.put_immutable(&checkpoint).map_err(map_store_before_selection)?;
    self.store.sync_immutable().map_err(map_store_before_selection)?;
    if self.cancellation.is_cancelled() {
      return Err(cancelled(
        "index_workspace_cancelled",
        "workspace publication was canceled after immutable checkpoint durability and before selection",
      ));
    }
    match self.store.publish_selected_synced(&self.owner, self.selected.as_ref(), &next) {
      Ok(()) => self.complete_selected(identity, next, timestamp_ms),
      Err(selection_error) => self.resolve_selection_error(identity, next, timestamp_ms, selection_error),
    }
  }

  fn encode_checkpoint(
    &self,
    head: &IndexRuntimeWorkspaceHeadV1,
    timestamp_ms: u64,
  ) -> Result<super::index_artifact::EncodedImmutableIndexArtifactV1, IndexRuntimePublicationErrorV1> {
    let selected = head.selected_descriptor();
    let path = persisted_workspace_path(selected.workspace_path())?;
    let required_capabilities = dirty_overlay_capabilities_v1();
    encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
      hash_algorithm: self.hash_algorithm,
      task_id: self.owner.operation_id(),
      checkpoint_sequence: selected.durable_sequence(),
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
      completed_work: head.cumulative_object_count(),
      total_work_hint: head.cumulative_object_count(),
      resume_key: INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1,
      attachments: &[],
      external: Some(ExternalWorkspaceDescriptorWriteV1 {
        workspace_id: selected.workspace_id(),
        manifest_digest: selected.manifest_digest(),
        durable_sequence: selected.durable_sequence(),
        durable_bytes: selected.durable_bytes(),
        path: &path,
      }),
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

  fn resolve_already_selected_pending(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
  ) -> Result<Option<SelectedWorkspacePublicationV1>, IndexRuntimePublicationErrorV1> {
    let Some(pending) = self.pending.clone() else {
      return Ok(None);
    };
    if pending.identity != identity {
      return Err(corrupt("index_workspace_pending_identity", "pending checkpoint belongs to another runtime batch or producer task"));
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
      Ok(None)
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
    let head =
      self.workspace.head().ok_or_else(|| commit_unknown("index_workspace_head", "selected successor has no cumulative workspace head"))?;
    if head.manifest_sequence() != pending.next.checkpoint_sequence {
      return Err(commit_unknown("index_workspace_head", "selected successor sequence disagrees with the cumulative workspace head"));
    }
    Ok(())
  }

  fn complete_selected(
    &mut self,
    identity: WorkspacePublicationIdentityV1,
    next: IndexCheckpointRootV1,
    timestamp_ms: u64,
  ) -> Result<SelectedWorkspacePublicationV1, IndexRuntimePublicationErrorV1> {
    if self.pending.as_ref().is_none_or(|pending| pending.identity != identity || pending.next != next) {
      return Err(corrupt(
        "index_workspace_pending_mismatch",
        "selected successor does not match the retained runtime batch or producer task",
      ));
    }
    let selected_workspace = self
      .workspace
      .head()
      .filter(|head| head.manifest_sequence() == next.checkpoint_sequence)
      .map(IndexRuntimeWorkspaceHeadV1::selected_descriptor)
      .ok_or_else(|| corrupt("index_workspace_head", "selected successor has no exact cumulative workspace head"))?;
    self.selected = Some(next.clone());
    self.selected_workspace = Some(selected_workspace);
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
