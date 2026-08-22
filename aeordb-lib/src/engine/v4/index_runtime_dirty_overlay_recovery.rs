//! Bounded recovery of one selected no-journal runtime dirty overlay.
//!
//! This is deliberately distinct from journal-backed checkpoint recovery: an
//! external workspace proves recoverable dirty state, not query coverage.

use std::mem::size_of;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::contract_generated::capability_bit;
use super::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryErrorV1, IndexRecoveryOwnerV1, IndexRecoveryReasonV1, IndexRecoveryStoreErrorV1,
  IndexRecoveryStoreV1, LoadedIndexCheckpointOutcomeV1, load_index_checkpoint_from_key_v1,
};
use super::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeRecoveredTaskSinkV1, IndexRuntimeWorkspaceHeadV1, IndexRuntimeWorkspaceOptionsV1,
  IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceSelectedHeadV1, IndexRuntimeWorkspaceStoreErrorV1,
};
use super::index_task::{IndexTaskKindV1, IndexTaskStateV1, decode_index_task_checkpoint};

pub const INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1: &[u8] = b"aeordb.index-runtime-dirty-overlay.v1";
pub(crate) const INDEX_RUNTIME_DIRTY_OVERLAY_PHASE_V1: u16 = 4;
const MAX_RECOVERY_EVIDENCE_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexRuntimeDirtyOverlayRecoveryReasonV1 {
  Checkpoint(IndexRecoveryReasonV1),
  CheckpointContractMismatch,
  WorkspaceMissing,
  WorkspaceCorrupt,
  RecoveryLimitExceeded,
  SelectionChanged,
}

pub struct RecoveredIndexRuntimeDirtyOverlayV1 {
  selected: IndexCheckpointRootV1,
  owner: IndexRecoveryOwnerV1,
  source_root: Vec<u8>,
  generation: u64,
  started_at_ms: u64,
  updated_at_ms: u64,
  workspace: DurableIndexRuntimeWorkspaceV1,
  cancellation: CancellationToken,
  reservation: MemoryReservation,
}

impl RecoveredIndexRuntimeDirtyOverlayV1 {
  pub fn selected(&self) -> &IndexCheckpointRootV1 {
    &self.selected
  }

  pub fn workspace_head(&self) -> Option<&IndexRuntimeWorkspaceHeadV1> {
    self.workspace.head()
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub const fn started_at_ms(&self) -> u64 {
    self.started_at_ms
  }

  pub const fn updated_at_ms(&self) -> u64 {
    self.updated_at_ms
  }

  pub fn source_root(&self) -> &[u8] {
    &self.source_root
  }

  pub(super) fn into_parts(self) -> IndexRuntimeDirtyOverlayResumePartsV1 {
    IndexRuntimeDirtyOverlayResumePartsV1 {
      selected: self.selected,
      owner: self.owner,
      source_root: self.source_root,
      generation: self.generation,
      started_at_ms: self.started_at_ms,
      updated_at_ms: self.updated_at_ms,
      workspace: self.workspace,
      cancellation: self.cancellation,
      reservation: self.reservation,
    }
  }
}

pub(super) struct IndexRuntimeDirtyOverlayResumePartsV1 {
  pub(super) selected: IndexCheckpointRootV1,
  pub(super) owner: IndexRecoveryOwnerV1,
  pub(super) source_root: Vec<u8>,
  pub(super) generation: u64,
  pub(super) started_at_ms: u64,
  pub(super) updated_at_ms: u64,
  pub(super) workspace: DurableIndexRuntimeWorkspaceV1,
  pub(super) cancellation: CancellationToken,
  pub(super) reservation: MemoryReservation,
}

pub enum IndexRuntimeDirtyOverlayRecoveryOutcomeV1 {
  Resumable(Box<RecoveredIndexRuntimeDirtyOverlayV1>),
  ReconciliationRequired { reason: IndexRuntimeDirtyOverlayRecoveryReasonV1, evidence: Option<String> },
  Canceled,
}

#[derive(Debug, Error)]
pub enum IndexRuntimeDirtyOverlayRecoveryErrorV1 {
  #[error("index runtime dirty-overlay recovery identity is invalid: {0}")]
  Invalid(&'static str),
  #[error("index runtime dirty-overlay recovery arithmetic conversion failed: {0}")]
  Arithmetic(String),
  #[error(transparent)]
  Checkpoint(#[from] IndexRecoveryErrorV1),
  #[error("index runtime dirty-overlay workspace recovery failed: {0}")]
  Workspace(#[source] IndexRuntimeWorkspaceStoreErrorV1),
}

#[allow(clippy::too_many_arguments)]
pub fn recover_index_runtime_dirty_overlay_v1(
  store: &mut dyn IndexRecoveryStoreV1,
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  owner: &IndexRecoveryOwnerV1,
  workspace_options: IndexRuntimeWorkspaceOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<IndexRuntimeDirtyOverlayRecoveryOutcomeV1, IndexRuntimeDirtyOverlayRecoveryErrorV1> {
  recover_index_runtime_dirty_overlay_inner_v1(
    store,
    hash_algorithm,
    database_id,
    destination_physical_instance_id,
    owner,
    workspace_options,
    memory,
    cancellation,
    None,
    None,
  )
}

#[allow(clippy::too_many_arguments)]
pub fn recover_index_runtime_dirty_overlay_for_workspace_v1(
  store: &mut dyn IndexRecoveryStoreV1,
  database_path: &Path,
  workspace_identity: IndexRuntimeWorkspaceIdentityV1,
  owner: &IndexRecoveryOwnerV1,
  workspace_options: IndexRuntimeWorkspaceOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<IndexRuntimeDirtyOverlayRecoveryOutcomeV1, IndexRuntimeDirtyOverlayRecoveryErrorV1> {
  recover_index_runtime_dirty_overlay_inner_v1(
    store,
    workspace_identity.hash_algorithm(),
    workspace_identity.database_id(),
    workspace_identity.destination_physical_instance_id(),
    owner,
    workspace_options,
    memory,
    cancellation,
    Some(IndexRuntimeDirtyOverlayWorkspaceContextV1 { database_path, identity: workspace_identity }),
    None,
  )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn recover_index_runtime_dirty_overlay_with_task_sink_v1(
  store: &mut dyn IndexRecoveryStoreV1,
  database_path: &Path,
  workspace_identity: IndexRuntimeWorkspaceIdentityV1,
  owner: &IndexRecoveryOwnerV1,
  workspace_options: IndexRuntimeWorkspaceOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  recovered_task_sink: &mut dyn IndexRuntimeRecoveredTaskSinkV1,
) -> Result<IndexRuntimeDirtyOverlayRecoveryOutcomeV1, IndexRuntimeDirtyOverlayRecoveryErrorV1> {
  recover_index_runtime_dirty_overlay_inner_v1(
    store,
    workspace_identity.hash_algorithm(),
    workspace_identity.database_id(),
    workspace_identity.destination_physical_instance_id(),
    owner,
    workspace_options,
    memory,
    cancellation,
    Some(IndexRuntimeDirtyOverlayWorkspaceContextV1 { database_path, identity: workspace_identity }),
    Some(recovered_task_sink),
  )
}

#[derive(Clone, Copy)]
struct IndexRuntimeDirtyOverlayWorkspaceContextV1<'a> {
  database_path: &'a Path,
  identity: IndexRuntimeWorkspaceIdentityV1,
}

#[allow(clippy::too_many_arguments)]
fn recover_index_runtime_dirty_overlay_inner_v1(
  store: &mut dyn IndexRecoveryStoreV1,
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  owner: &IndexRecoveryOwnerV1,
  workspace_options: IndexRuntimeWorkspaceOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  workspace_context: Option<IndexRuntimeDirtyOverlayWorkspaceContextV1<'_>>,
  recovered_task_sink: Option<&mut dyn IndexRuntimeRecoveredTaskSinkV1>,
) -> Result<IndexRuntimeDirtyOverlayRecoveryOutcomeV1, IndexRuntimeDirtyOverlayRecoveryErrorV1> {
  if cancellation.is_cancelled() {
    return Ok(IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled);
  }
  if database_id != owner.database_id() || destination_physical_instance_id.iter().all(|byte| *byte == 0) {
    return Err(IndexRuntimeDirtyOverlayRecoveryErrorV1::Invalid(
      "database and destination identities must match the selected operation owner and be nonzero",
    ));
  }
  let selected = match store.load_selected(owner).map_err(IndexRecoveryErrorV1::Store)? {
    Some(selected) => selected,
    None => return Ok(checkpoint_reconciliation(IndexRecoveryReasonV1::CheckpointSelectionMissing, None)),
  };
  let loaded = match load_index_checkpoint_from_key_v1(store, hash_algorithm, owner, &selected, memory, cancellation) {
    Ok(LoadedIndexCheckpointOutcomeV1::Loaded(loaded)) => loaded,
    Ok(LoadedIndexCheckpointOutcomeV1::ReconciliationRequired { reason, evidence }) => {
      return Ok(checkpoint_reconciliation(reason, evidence));
    }
    Err(IndexRecoveryErrorV1::Canceled) => return Ok(IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled),
    Err(error) => return Err(error.into()),
  };
  let (checkpoint_sequence, workspace_object_count, source_root, generation, started_at_ms, updated_at_ms, selected_workspace) = {
    let checkpoint = match decode_index_task_checkpoint(&loaded.bytes, hash_algorithm) {
      Ok(checkpoint) => checkpoint,
      Err(error) => return Ok(contract_reconciliation(error)),
    };
    let expected_capabilities = dirty_overlay_capabilities_v1();
    if checkpoint.task_kind != IndexTaskKindV1::Reconcile
      || checkpoint.state != IndexTaskStateV1::Running
      || checkpoint.phase != INDEX_RUNTIME_DIRTY_OVERLAY_PHASE_V1
      || checkpoint.required_capabilities != expected_capabilities
      || checkpoint.target_root.iter().any(|byte| *byte != 0)
      || checkpoint.journal_head.iter().any(|byte| *byte != 0)
      || checkpoint.journal_floor_sequence != 0
      || checkpoint.journal_audited_through != 0
      || checkpoint.next_document_ordinal != 0
      || checkpoint.started_at_ms == 0
      || checkpoint.completed_work != checkpoint.total_work_hint
      || checkpoint.completed_work != checkpoint.checkpoint_sequence
      || checkpoint.resume_key != INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1
      || !checkpoint.attachments.is_empty()
    {
      return Ok(contract_reconciliation("selected checkpoint is not the exact no-journal runtime dirty-overlay contract"));
    }
    let selected_workspace = match checkpoint.external {
      Some(external) => {
        if external.durable_sequence == 0
          || external.durable_bytes == 0
          || workspace_context.is_some_and(|context| external.workspace_id != context.identity.workspace_id())
        {
          return Ok(contract_reconciliation(
            "selected nonempty dirty overlay has invalid local progress or belongs to another configured workspace",
          ));
        }
        match IndexRuntimeWorkspaceSelectedHeadV1::new(
          PathBuf::from(external.path),
          external.workspace_id,
          external.manifest_digest,
          external.durable_sequence,
          external.durable_bytes,
        ) {
          Ok(selected_workspace) => Some(selected_workspace),
          Err(error) => return Ok(workspace_reconciliation(IndexRuntimeDirtyOverlayRecoveryReasonV1::WorkspaceCorrupt, error)),
        }
      }
      None => {
        if workspace_context.is_none() {
          return Ok(contract_reconciliation("selected clean dirty overlay requires an exact configured workspace identity"));
        }
        None
      }
    };
    let workspace_object_count = selected_workspace.as_ref().map_or(0, IndexRuntimeWorkspaceSelectedHeadV1::durable_sequence);
    (
      checkpoint.checkpoint_sequence,
      workspace_object_count,
      checkpoint.source_root.to_vec(),
      checkpoint.generation,
      checkpoint.started_at_ms,
      checkpoint.updated_at_ms,
      selected_workspace,
    )
  };
  drop(loaded);
  let resumed = match selected_workspace {
    Some(selected_workspace) => match recovered_task_sink {
      Some(sink) => DurableIndexRuntimeWorkspaceV1::resume_with_recovered_task_sink(
        database_id,
        destination_physical_instance_id,
        hash_algorithm,
        selected_workspace,
        workspace_options,
        cancellation.clone(),
        memory,
        sink,
      ),
      None => DurableIndexRuntimeWorkspaceV1::resume(
        database_id,
        destination_physical_instance_id,
        hash_algorithm,
        selected_workspace,
        workspace_options,
        cancellation.clone(),
        memory,
      ),
    },
    None => {
      let context = workspace_context
        .ok_or(IndexRuntimeDirtyOverlayRecoveryErrorV1::Invalid("clean dirty-overlay recovery requires an exact workspace context"))?;
      DurableIndexRuntimeWorkspaceV1::resume_empty_rotation(
        context.database_path,
        context.identity,
        checkpoint_sequence,
        workspace_options,
        cancellation.clone(),
        memory,
      )
    }
  };
  let workspace = match resumed {
    Ok(workspace) => workspace,
    Err(IndexRuntimeWorkspaceStoreErrorV1::Canceled) => return Ok(IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled),
    Err(error @ IndexRuntimeWorkspaceStoreErrorV1::Capacity(_)) => {
      return Ok(workspace_reconciliation(IndexRuntimeDirtyOverlayRecoveryReasonV1::RecoveryLimitExceeded, error));
    }
    Err(error @ IndexRuntimeWorkspaceStoreErrorV1::Invalid(_))
    | Err(error @ IndexRuntimeWorkspaceStoreErrorV1::Path(_))
    | Err(error @ IndexRuntimeWorkspaceStoreErrorV1::State(_))
    | Err(error @ IndexRuntimeWorkspaceStoreErrorV1::Format(_))
    | Err(error @ IndexRuntimeWorkspaceStoreErrorV1::Workspace(_)) => {
      return Ok(workspace_reconciliation(IndexRuntimeDirtyOverlayRecoveryReasonV1::WorkspaceCorrupt, error));
    }
    Err(error) if matches!(&error, IndexRuntimeWorkspaceStoreErrorV1::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound) =>
    {
      return Ok(workspace_reconciliation(IndexRuntimeDirtyOverlayRecoveryReasonV1::WorkspaceMissing, error));
    }
    Err(error) => return Err(IndexRuntimeDirtyOverlayRecoveryErrorV1::Workspace(error)),
  };
  if workspace_context.is_some_and(|context| workspace.identity() != context.identity) {
    return Ok(contract_reconciliation("recovered workspace identity disagrees with the configured runtime owner"));
  }
  match workspace.head() {
    Some(head) => {
      let object_count = match head.runtime_batch_count().checked_add(head.producer_task_count()) {
        Some(object_count) => object_count,
        None => return Ok(contract_reconciliation("recovered workspace object counters overflow")),
      };
      if head.manifest_sequence() != workspace_object_count
        || head.cumulative_object_count() != workspace_object_count
        || object_count != workspace_object_count
      {
        return Ok(contract_reconciliation("recovered workspace closure disagrees with local checkpoint progress"));
      }
    }
    None if workspace_object_count == 0 => {}
    None => return Ok(contract_reconciliation("nonempty checkpoint resumed without a selected workspace head")),
  }
  if cancellation.is_cancelled() {
    return Ok(IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled);
  }
  let observed = store.load_selected(owner).map_err(IndexRecoveryErrorV1::Store)?;
  if cancellation.is_cancelled() {
    return Ok(IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled);
  }
  if observed.as_ref() != Some(&selected) {
    return Ok(reconciliation(IndexRuntimeDirtyOverlayRecoveryReasonV1::SelectionChanged, None));
  }
  let owner = owner.clone();
  let retained_heap_capacity = selected
    .checkpoint_key
    .capacity()
    .checked_add(owner.retained_heap_capacity())
    .and_then(|bytes| bytes.checked_add(source_root.capacity()))
    .and_then(|bytes| workspace.retained_heap_capacity().and_then(|workspace_bytes| bytes.checked_add(workspace_bytes)))
    .ok_or_else(|| IndexRuntimeDirtyOverlayRecoveryErrorV1::Arithmetic("recovered dirty-overlay heap capacity overflowed".to_string()))?;
  let retained_capacity = size_of::<RecoveredIndexRuntimeDirtyOverlayV1>()
    .checked_add(retained_heap_capacity)
    .ok_or_else(|| IndexRuntimeDirtyOverlayRecoveryErrorV1::Arithmetic("recovered dirty-overlay retained size overflowed".to_string()))?;
  let retained_bytes =
    u64::try_from(retained_capacity).map_err(|error| IndexRuntimeDirtyOverlayRecoveryErrorV1::Arithmetic(error.to_string()))?;
  let reservation =
    memory.reserve(MemoryOwner::IndexDirtyBuffers, retained_bytes, AdmissionClass::Maintenance).map_err(IndexRecoveryErrorV1::Memory)?;
  Ok(IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(Box::new(RecoveredIndexRuntimeDirtyOverlayV1 {
    selected,
    owner,
    source_root,
    generation,
    started_at_ms,
    updated_at_ms,
    workspace,
    cancellation: cancellation.clone(),
    reservation,
  })))
}

pub(crate) fn dirty_overlay_capabilities_v1() -> [u8; 32] {
  let mut capabilities = [0u8; 32];
  for bit in [capability_bit::INDEX_ARTIFACT_V1, capability_bit::DURABLE_TASK_PIN_V1] {
    capabilities[usize::from(bit / 8)] |= 1 << (bit % 8);
  }
  capabilities
}

fn checkpoint_reconciliation(reason: IndexRecoveryReasonV1, evidence: Option<String>) -> IndexRuntimeDirtyOverlayRecoveryOutcomeV1 {
  reconciliation(IndexRuntimeDirtyOverlayRecoveryReasonV1::Checkpoint(reason), evidence)
}

fn contract_reconciliation(evidence: impl std::fmt::Display) -> IndexRuntimeDirtyOverlayRecoveryOutcomeV1 {
  reconciliation(IndexRuntimeDirtyOverlayRecoveryReasonV1::CheckpointContractMismatch, Some(evidence.to_string()))
}

fn workspace_reconciliation(
  reason: IndexRuntimeDirtyOverlayRecoveryReasonV1,
  evidence: IndexRuntimeWorkspaceStoreErrorV1,
) -> IndexRuntimeDirtyOverlayRecoveryOutcomeV1 {
  reconciliation(reason, Some(evidence.to_string()))
}

fn reconciliation(reason: IndexRuntimeDirtyOverlayRecoveryReasonV1, evidence: Option<String>) -> IndexRuntimeDirtyOverlayRecoveryOutcomeV1 {
  IndexRuntimeDirtyOverlayRecoveryOutcomeV1::ReconciliationRequired { reason, evidence: evidence.map(bounded_recovery_evidence) }
}

fn bounded_recovery_evidence(mut evidence: String) -> String {
  if evidence.len() <= MAX_RECOVERY_EVIDENCE_BYTES {
    return evidence;
  }
  let mut boundary = MAX_RECOVERY_EVIDENCE_BYTES;
  while !evidence.is_char_boundary(boundary) {
    boundary -= 1;
  }
  evidence.truncate(boundary);
  evidence
}

impl From<IndexRecoveryStoreErrorV1> for IndexRuntimeDirtyOverlayRecoveryErrorV1 {
  fn from(error: IndexRecoveryStoreErrorV1) -> Self {
    Self::Checkpoint(IndexRecoveryErrorV1::Store(error))
  }
}
