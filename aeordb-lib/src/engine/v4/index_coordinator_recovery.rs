//! Bounded persistence and restart validation for v4 index checkpoints.
//!
//! The store remains an injected durability boundary. This module validates
//! immutable dependencies and their complete checkpoint closure before asking
//! that store to advance a selected checkpoint root.

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::coverage_journal::{
  CoverageJournalErrorV1, CoverageJournalReplayExpectationV1, CoverageJournalReplayOptionsV1, CoverageJournalReplayOutcomeV1,
  CoverageJournalReplaySummaryV1, CoverageRebuildReasonV1, replay_system_journal_chain,
};
use super::index_artifact::{EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, decode_immutable_index_artifact};
use super::index_task::{
  IndexTaskAttachmentClosureBuilderV1, IndexTaskAttachmentRoleV1, IndexTaskCheckpointV1, JournalOwnerKindV1, decode_index_task_checkpoint,
  decode_mutation_journal,
};
use super::reader::FormatError;

const SYSTEM_INDEX_JOURNAL_ID: [u8; 16] = *b"AEORIDXJOURNALV1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexRecoveryOptionsV1 {
  maximum_attachments: usize,
  maximum_attachment_bytes: u64,
  maximum_journal_segments: usize,
  maximum_journal_bytes: u64,
}

impl IndexRecoveryOptionsV1 {
  pub fn new(
    maximum_attachments: usize,
    maximum_attachment_bytes: u64,
    maximum_journal_segments: usize,
    maximum_journal_bytes: u64,
  ) -> Result<Self, IndexRecoveryErrorV1> {
    if maximum_attachments == 0 || maximum_attachment_bytes == 0 || maximum_journal_segments == 0 || maximum_journal_bytes == 0 {
      return Err(IndexRecoveryErrorV1::Invalid("recovery limits must be nonzero"));
    }
    Ok(Self { maximum_attachments, maximum_attachment_bytes, maximum_journal_segments, maximum_journal_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexRecoveryOwnerV1 {
  database_id: [u8; 16],
  index_id: Vec<u8>,
  operation_id: [u8; 16],
}

impl IndexRecoveryOwnerV1 {
  pub fn new(database_id: [u8; 16], index_id: Vec<u8>, operation_id: [u8; 16]) -> Result<Self, IndexRecoveryErrorV1> {
    if database_id.iter().all(|byte| *byte == 0)
      || index_id.is_empty()
      || index_id.iter().all(|byte| *byte == 0)
      || operation_id.iter().all(|byte| *byte == 0)
    {
      return Err(IndexRecoveryErrorV1::Invalid("database, index, and operation identities must be nonzero"));
    }
    Ok(Self { database_id, index_id, operation_id })
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub fn index_id(&self) -> &[u8] {
    &self.index_id
  }

  pub const fn operation_id(&self) -> [u8; 16] {
    self.operation_id
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexCheckpointRootV1 {
  pub checkpoint_sequence: u64,
  pub checkpoint_key: Vec<u8>,
}

impl IndexCheckpointRootV1 {
  pub fn new(checkpoint_sequence: u64, checkpoint_key: Vec<u8>) -> Result<Self, IndexRecoveryErrorV1> {
    if checkpoint_sequence == 0 || checkpoint_key.is_empty() || checkpoint_key.iter().all(|byte| *byte == 0) {
      return Err(IndexRecoveryErrorV1::Invalid("checkpoint root sequence and key must be nonzero"));
    }
    Ok(Self { checkpoint_sequence, checkpoint_key })
  }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("index recovery store failure {code}: {message}")]
pub struct IndexRecoveryStoreErrorV1 {
  code: &'static str,
  message: String,
}

impl IndexRecoveryStoreErrorV1 {
  pub fn new(code: &'static str, message: impl Into<String>) -> Self {
    Self { code, message: message.into() }
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }
}

/// Minimal durability boundary required by checkpoint publication and replay.
///
/// `publish_selected_synced` is selector-last: on success the supplied root is
/// durable. An error may be commit-unknown, but reopening must select either
/// the expected root or the exact next root, never partial or foreign state.
/// The caller resolves uncertainty by reloading and validating the selected
/// checkpoint graph before retrying.
pub trait IndexRecoveryStoreV1 {
  fn immutable_length(&mut self, key: &[u8]) -> Result<Option<u64>, IndexRecoveryStoreErrorV1>;
  fn load_immutable(&mut self, key: &[u8], expected_length: u64) -> Result<Option<Vec<u8>>, IndexRecoveryStoreErrorV1>;
  fn put_immutable(&mut self, artifact: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRecoveryStoreErrorV1>;
  fn sync_immutable(&mut self) -> Result<(), IndexRecoveryStoreErrorV1>;
  fn load_selected(&mut self, owner: &IndexRecoveryOwnerV1) -> Result<Option<IndexCheckpointRootV1>, IndexRecoveryStoreErrorV1>;
  fn publish_selected_synced(
    &mut self,
    owner: &IndexRecoveryOwnerV1,
    expected: Option<&IndexCheckpointRootV1>,
    next: &IndexCheckpointRootV1,
  ) -> Result<(), IndexRecoveryStoreErrorV1>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexRecoveryReasonV1 {
  CheckpointSelectionMissing,
  CheckpointMissing,
  CheckpointCorrupt,
  CheckpointDiscontinuous,
  AttachmentMissing,
  AttachmentCorrupt,
  JournalMissing,
  JournalCorrupt,
  JournalChainDiscontinuous,
  RecoveryLimitExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredIndexCheckpointV1 {
  pub checkpoint_sequence: u64,
  pub checkpoint_key: Vec<u8>,
  pub generation: u64,
  pub rooted_artifact_count: u32,
  pub journal: CoverageJournalReplaySummaryV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexRecoveryOutcomeV1 {
  Resumable(RecoveredIndexCheckpointV1),
  /// `evidence` is operator diagnostics, not a pre-sanitized public API error.
  ReconciliationRequired {
    reason: IndexRecoveryReasonV1,
    evidence: Option<String>,
  },
  Canceled,
}

#[derive(Debug, Error)]
pub enum IndexRecoveryErrorV1 {
  #[error("index recovery options or identity are invalid: {0}")]
  Invalid(&'static str),
  #[error("index recovery was canceled")]
  Canceled,
  #[error("index recovery arithmetic conversion failed: {0}")]
  Arithmetic(String),
  #[error("index recovery requires authoritative reconciliation ({reason:?}): {evidence:?}")]
  ReconciliationRequired { reason: IndexRecoveryReasonV1, evidence: Option<String> },
  #[error("index recovery memory admission failed: {0}")]
  Memory(#[source] MemoryCoordinatorError),
  #[error(transparent)]
  Store(#[from] IndexRecoveryStoreErrorV1),
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error(transparent)]
  Coverage(#[from] CoverageJournalErrorV1),
}

#[derive(Clone, Copy)]
pub struct IndexRecoveryPublicationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub owner: &'a IndexRecoveryOwnerV1,
  pub expected: Option<&'a IndexCheckpointRootV1>,
  pub checkpoint: &'a EncodedImmutableIndexArtifactV1,
  pub dependencies: &'a [&'a EncodedImmutableIndexArtifactV1],
  pub options: IndexRecoveryOptionsV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexRecoveryPublicationReceiptV1 {
  pub selected: IndexCheckpointRootV1,
  pub rooted_artifact_count: u32,
  pub journal_last_sequence: u64,
  pub idempotent: bool,
}

pub fn publish_index_recovery_checkpoint_v1(
  store: &mut dyn IndexRecoveryStoreV1,
  request: IndexRecoveryPublicationRequestV1<'_>,
) -> Result<IndexRecoveryPublicationReceiptV1, IndexRecoveryErrorV1> {
  check_cancellation(request.cancellation)?;
  validate_owner(request.hash_algorithm, request.owner)?;
  validate_dependency_inputs(request.hash_algorithm, request.dependencies, request.options)?;
  let checkpoint = decode_index_task_checkpoint(&request.checkpoint.value, request.hash_algorithm)?;
  if checkpoint.key != request.checkpoint.key {
    return Err(IndexRecoveryErrorV1::Invalid("checkpoint key disagrees with its encoded bytes"));
  }
  validate_checkpoint_owner(&checkpoint, request.owner, request.hash_algorithm)?;
  let selected = IndexCheckpointRootV1::new(checkpoint.checkpoint_sequence, checkpoint.key.clone())?;
  let current = store.load_selected(request.owner)?;
  if current.as_ref() != request.expected {
    return Err(IndexRecoveryErrorV1::Invalid("selected checkpoint changed before publication"));
  }
  validate_checkpoint_advance(current.as_ref(), &selected)?;

  for dependency in request.dependencies {
    check_cancellation(request.cancellation)?;
    store.put_immutable(dependency)?;
  }
  store.put_immutable(request.checkpoint)?;
  store.sync_immutable()?;
  check_cancellation(request.cancellation)?;

  let validation = validate_checkpoint_from_key(
    store,
    request.hash_algorithm,
    request.owner,
    &selected,
    request.options,
    request.memory,
    request.cancellation,
  )?;
  let state = match validation {
    IndexRecoveryOutcomeV1::Resumable(state) => state,
    IndexRecoveryOutcomeV1::ReconciliationRequired { reason, evidence } => {
      return Err(IndexRecoveryErrorV1::ReconciliationRequired { reason, evidence });
    }
    IndexRecoveryOutcomeV1::Canceled => return Err(IndexRecoveryErrorV1::Canceled),
  };
  check_cancellation(request.cancellation)?;
  let idempotent = current.as_ref() == Some(&selected);
  if !idempotent {
    store.publish_selected_synced(request.owner, current.as_ref(), &selected)?;
  }
  Ok(IndexRecoveryPublicationReceiptV1 {
    selected,
    rooted_artifact_count: state.rooted_artifact_count,
    journal_last_sequence: state.journal.last_sequence,
    idempotent,
  })
}

pub fn recover_index_checkpoint_v1(
  store: &mut dyn IndexRecoveryStoreV1,
  hash_algorithm: HashAlgorithm,
  owner: &IndexRecoveryOwnerV1,
  options: IndexRecoveryOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<IndexRecoveryOutcomeV1, IndexRecoveryErrorV1> {
  if cancellation.is_cancelled() {
    return Ok(IndexRecoveryOutcomeV1::Canceled);
  }
  validate_owner(hash_algorithm, owner)?;
  let selected = match store.load_selected(owner)? {
    Some(selected) => selected,
    None => return Ok(reconciliation(IndexRecoveryReasonV1::CheckpointSelectionMissing)),
  };
  match validate_checkpoint_from_key(store, hash_algorithm, owner, &selected, options, memory, cancellation) {
    Err(IndexRecoveryErrorV1::Canceled) => Ok(IndexRecoveryOutcomeV1::Canceled),
    outcome => outcome,
  }
}

fn validate_checkpoint_from_key(
  store: &mut dyn IndexRecoveryStoreV1,
  hash_algorithm: HashAlgorithm,
  owner: &IndexRecoveryOwnerV1,
  selected: &IndexCheckpointRootV1,
  options: IndexRecoveryOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<IndexRecoveryOutcomeV1, IndexRecoveryErrorV1> {
  if selected.checkpoint_key.len() != hash_algorithm.hash_length() || selected.checkpoint_key.iter().all(|byte| *byte == 0) {
    return Ok(reconciliation(IndexRecoveryReasonV1::CheckpointCorrupt));
  }
  let checkpoint_loaded = match load_reserved(store, &selected.checkpoint_key, 4 * 1_024 * 1_024, memory, cancellation)? {
    ArtifactLoadOutcomeV1::Loaded(loaded) => loaded,
    ArtifactLoadOutcomeV1::Missing => return Ok(reconciliation(IndexRecoveryReasonV1::CheckpointMissing)),
    ArtifactLoadOutcomeV1::Corrupt(evidence) => {
      return Ok(reconciliation_from(IndexRecoveryReasonV1::CheckpointCorrupt, evidence));
    }
    ArtifactLoadOutcomeV1::LimitExceeded => return Ok(reconciliation(IndexRecoveryReasonV1::RecoveryLimitExceeded)),
  };
  let checkpoint = match decode_index_task_checkpoint(&checkpoint_loaded.bytes, hash_algorithm) {
    Ok(checkpoint) => checkpoint,
    Err(source) => return Ok(reconciliation_from(IndexRecoveryReasonV1::CheckpointCorrupt, source)),
  };
  if checkpoint.key != selected.checkpoint_key || checkpoint.checkpoint_sequence != selected.checkpoint_sequence {
    return Ok(reconciliation(IndexRecoveryReasonV1::CheckpointDiscontinuous));
  }
  if let Err(source) = validate_checkpoint_owner(&checkpoint, owner, hash_algorithm) {
    return Ok(reconciliation_from(IndexRecoveryReasonV1::CheckpointDiscontinuous, source));
  }
  let attachments = match validate_attachment_closure(store, hash_algorithm, &checkpoint, options, memory, cancellation)? {
    AttachmentClosureOutcomeV1::Valid(attachments) => attachments,
    AttachmentClosureOutcomeV1::ReconciliationRequired { reason, evidence } => {
      return Ok(IndexRecoveryOutcomeV1::ReconciliationRequired { reason, evidence });
    }
  };
  let journal =
    match replay_checkpoint_journal(store, hash_algorithm, &checkpoint, attachments.journal_head, options, memory, cancellation)? {
      CoverageJournalReplayOutcomeV1::Verified(summary) => summary,
      CoverageJournalReplayOutcomeV1::RebuildRequired { reason, evidence } => {
        return Ok(match evidence {
          Some(evidence) => reconciliation_from(map_rebuild_reason(reason), evidence),
          None => reconciliation(map_rebuild_reason(reason)),
        });
      }
      CoverageJournalReplayOutcomeV1::Canceled => return Ok(IndexRecoveryOutcomeV1::Canceled),
    };
  if !journal_matches_checkpoint(&journal, &checkpoint) {
    return Ok(reconciliation(IndexRecoveryReasonV1::JournalChainDiscontinuous));
  }
  Ok(IndexRecoveryOutcomeV1::Resumable(RecoveredIndexCheckpointV1 {
    checkpoint_sequence: checkpoint.checkpoint_sequence,
    checkpoint_key: checkpoint.key,
    generation: checkpoint.generation,
    rooted_artifact_count: attachments.rooted_artifact_count,
    journal,
  }))
}

fn validate_attachment_closure(
  store: &mut dyn IndexRecoveryStoreV1,
  hash_algorithm: HashAlgorithm,
  checkpoint: &IndexTaskCheckpointV1<'_>,
  options: IndexRecoveryOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<AttachmentClosureOutcomeV1, IndexRecoveryErrorV1> {
  if checkpoint.attachments.len() > options.maximum_attachments {
    return Ok(attachment_reconciliation(IndexRecoveryReasonV1::RecoveryLimitExceeded));
  }
  let mut closure = match IndexTaskAttachmentClosureBuilderV1::new(checkpoint, hash_algorithm) {
    Ok(closure) => closure,
    Err(source) => {
      return Ok(AttachmentClosureOutcomeV1::ReconciliationRequired {
        reason: IndexRecoveryReasonV1::CheckpointCorrupt,
        evidence: Some(source.to_string()),
      });
    }
  };
  let mut loaded_bytes = 0u64;
  let mut journal_head = None;
  for attachment in checkpoint.attachments.iter() {
    check_cancellation(cancellation)?;
    let attachment = match attachment {
      Ok(attachment) => attachment,
      Err(source) => {
        return Ok(AttachmentClosureOutcomeV1::ReconciliationRequired {
          reason: IndexRecoveryReasonV1::CheckpointCorrupt,
          evidence: Some(source.to_string()),
        });
      }
    };
    let length = match store.immutable_length(attachment.artifact_hash)? {
      Some(length) => length,
      None => return Ok(attachment_reconciliation(missing_attachment_reason(attachment.role))),
    };
    loaded_bytes = match loaded_bytes.checked_add(length) {
      Some(total) if total <= options.maximum_attachment_bytes => total,
      Some(_) | None => {
        return Ok(attachment_reconciliation(IndexRecoveryReasonV1::RecoveryLimitExceeded));
      }
    };
    let loaded = match load_reserved_known_length(store, attachment.artifact_hash, length, memory, cancellation)? {
      ArtifactLoadOutcomeV1::Loaded(loaded) => loaded,
      ArtifactLoadOutcomeV1::Missing => {
        return Ok(attachment_reconciliation(missing_attachment_reason(attachment.role)));
      }
      ArtifactLoadOutcomeV1::Corrupt(evidence) => {
        return Ok(AttachmentClosureOutcomeV1::ReconciliationRequired {
          reason: corrupt_attachment_reason(attachment.role),
          evidence: Some(evidence.to_string()),
        });
      }
      ArtifactLoadOutcomeV1::LimitExceeded => {
        return Ok(attachment_reconciliation(IndexRecoveryReasonV1::RecoveryLimitExceeded));
      }
    };
    if let Err(source) = closure.observe_encoded(&loaded.bytes) {
      return Ok(AttachmentClosureOutcomeV1::ReconciliationRequired {
        reason: corrupt_attachment_reason(attachment.role),
        evidence: Some(source.to_string()),
      });
    }
    if attachment.role == IndexTaskAttachmentRoleV1::MutationJournalHead {
      journal_head = Some(loaded);
    }
  }
  let closed = match closure.finish() {
    Ok(closed) => closed,
    Err(source) => {
      return Ok(AttachmentClosureOutcomeV1::ReconciliationRequired {
        reason: IndexRecoveryReasonV1::AttachmentCorrupt,
        evidence: Some(source.to_string()),
      });
    }
  };
  Ok(AttachmentClosureOutcomeV1::Valid(ValidatedAttachmentClosureV1 {
    rooted_artifact_count: closed.rooted_artifact_count(),
    journal_head,
  }))
}

fn replay_checkpoint_journal(
  store: &mut dyn IndexRecoveryStoreV1,
  hash_algorithm: HashAlgorithm,
  checkpoint: &IndexTaskCheckpointV1<'_>,
  mut preloaded_journal_head: Option<LoadedArtifactV1>,
  options: IndexRecoveryOptionsV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<CoverageJournalReplayOutcomeV1, IndexRecoveryErrorV1> {
  if checkpoint.journal_head.iter().all(|byte| *byte == 0) {
    return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalMissing));
  }
  let mut next = checkpoint.journal_head.to_vec();
  let mut segments: Vec<Vec<u8>> = Vec::new();
  let mut reservations = Vec::new();
  let mut total_bytes = 0u64;
  let mut generation = None;
  let mut first_ordinal = 0u64;
  while next.iter().any(|byte| *byte != 0) {
    check_cancellation(cancellation)?;
    if segments.len() >= options.maximum_journal_segments {
      return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded));
    }
    let mut duplicate = false;
    for segment in &segments {
      let journal = decode_mutation_journal(segment, hash_algorithm)?;
      if journal.key == next {
        duplicate = true;
      }
    }
    if duplicate {
      return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalChainDiscontinuous));
    }
    let (length, loaded) = if next == checkpoint.journal_head {
      let Some(loaded) = preloaded_journal_head.take() else {
        return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalMissing));
      };
      let length = match u64::try_from(loaded.bytes.len()) {
        Ok(length) => length,
        Err(source) => return Err(IndexRecoveryErrorV1::Arithmetic(source.to_string())),
      };
      (length, loaded)
    } else {
      let length = match store.immutable_length(&next)? {
        Some(length) => length,
        None => return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalMissing)),
      };
      let loaded = match load_reserved_known_length(store, &next, length, memory, cancellation)? {
        ArtifactLoadOutcomeV1::Loaded(loaded) => loaded,
        ArtifactLoadOutcomeV1::Missing => return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalMissing)),
        ArtifactLoadOutcomeV1::Corrupt(evidence) => {
          return Ok(CoverageJournalReplayOutcomeV1::RebuildRequired {
            reason: CoverageRebuildReasonV1::JournalCorrupt,
            evidence: Some(CoverageJournalErrorV1::AuthorityClosure(evidence)),
          });
        }
        ArtifactLoadOutcomeV1::LimitExceeded => {
          return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded));
        }
      };
      (length, loaded)
    };
    total_bytes = match total_bytes.checked_add(length) {
      Some(total) if total <= options.maximum_journal_bytes => total,
      Some(_) | None => return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded)),
    };
    let journal = match decode_mutation_journal(&loaded.bytes, hash_algorithm) {
      Ok(journal) => journal,
      Err(source) => {
        return Ok(CoverageJournalReplayOutcomeV1::RebuildRequired {
          reason: CoverageRebuildReasonV1::JournalCorrupt,
          evidence: Some(CoverageJournalErrorV1::Format(source)),
        });
      }
    };
    if journal.key != next || journal.owner_kind != JournalOwnerKindV1::System || journal.owner_id != SYSTEM_INDEX_JOURNAL_ID {
      return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalChainDiscontinuous));
    }
    match generation {
      Some(expected) if expected != journal.generation => {
        return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalChainDiscontinuous));
      }
      Some(_) => {}
      None => generation = Some(journal.generation),
    }
    first_ordinal = journal.segment_ordinal;
    next = journal.previous_segment.to_vec();
    segments.push(loaded.bytes);
    reservations.push(loaded.reservation);
  }
  segments.reverse();
  reservations.reverse();
  let Some(generation) = generation else {
    return Ok(CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalMissing));
  };
  let maximum_encoded_bytes = match usize::try_from(options.maximum_journal_bytes) {
    Ok(value) => value,
    Err(source) => return Err(IndexRecoveryErrorV1::Arithmetic(source.to_string())),
  };
  let replay = replay_system_journal_chain(
    hash_algorithm,
    &segments,
    &CoverageJournalReplayExpectationV1 {
      generation,
      first_segment_ordinal: first_ordinal,
      previous_segment: vec![0; hash_algorithm.hash_length()],
      source_root_before: checkpoint.source_root.to_vec(),
    },
    CoverageJournalReplayOptionsV1::new(options.maximum_journal_segments, maximum_encoded_bytes)?,
    cancellation,
  );
  drop(reservations);
  Ok(replay)
}

struct LoadedArtifactV1 {
  bytes: Vec<u8>,
  reservation: MemoryReservation,
}

enum ArtifactLoadOutcomeV1 {
  Loaded(LoadedArtifactV1),
  Missing,
  Corrupt(&'static str),
  LimitExceeded,
}

enum AttachmentClosureOutcomeV1 {
  Valid(ValidatedAttachmentClosureV1),
  ReconciliationRequired { reason: IndexRecoveryReasonV1, evidence: Option<String> },
}

struct ValidatedAttachmentClosureV1 {
  rooted_artifact_count: u32,
  journal_head: Option<LoadedArtifactV1>,
}

fn load_reserved(
  store: &mut dyn IndexRecoveryStoreV1,
  key: &[u8],
  maximum_length: u64,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<ArtifactLoadOutcomeV1, IndexRecoveryErrorV1> {
  let length = match store.immutable_length(key)? {
    Some(length) if length <= maximum_length => length,
    Some(_) => return Ok(ArtifactLoadOutcomeV1::LimitExceeded),
    None => return Ok(ArtifactLoadOutcomeV1::Missing),
  };
  load_reserved_known_length(store, key, length, memory, cancellation)
}

fn load_reserved_known_length(
  store: &mut dyn IndexRecoveryStoreV1,
  key: &[u8],
  length: u64,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<ArtifactLoadOutcomeV1, IndexRecoveryErrorV1> {
  check_cancellation(cancellation)?;
  if length == 0 {
    return Ok(ArtifactLoadOutcomeV1::Corrupt("artifact length probe returned zero bytes"));
  }
  let reservation =
    memory.reserve(MemoryOwner::IndexDirtyBuffers, length, AdmissionClass::Maintenance).map_err(IndexRecoveryErrorV1::Memory)?;
  let bytes = match store.load_immutable(key, length)? {
    Some(bytes) => bytes,
    None => return Ok(ArtifactLoadOutcomeV1::Corrupt("artifact disappeared or changed after its successful length probe")),
  };
  let actual = match u64::try_from(bytes.len()) {
    Ok(actual) => actual,
    Err(source) => return Err(IndexRecoveryErrorV1::Arithmetic(source.to_string())),
  };
  if actual != length {
    return Ok(ArtifactLoadOutcomeV1::Corrupt("artifact bytes differed from the probed length"));
  }
  Ok(ArtifactLoadOutcomeV1::Loaded(LoadedArtifactV1 { bytes, reservation }))
}

fn validate_owner(hash_algorithm: HashAlgorithm, owner: &IndexRecoveryOwnerV1) -> Result<(), IndexRecoveryErrorV1> {
  if owner.index_id.len() != hash_algorithm.hash_length() {
    return Err(IndexRecoveryErrorV1::Invalid("index identity width differs from the database hash profile"));
  }
  Ok(())
}

fn validate_checkpoint_owner(
  checkpoint: &IndexTaskCheckpointV1<'_>,
  owner: &IndexRecoveryOwnerV1,
  hash_algorithm: HashAlgorithm,
) -> Result<(), IndexRecoveryErrorV1> {
  validate_owner(hash_algorithm, owner)?;
  if checkpoint.task_id != owner.operation_id
    || checkpoint.primary_id != owner.index_id
    || checkpoint.source_root.len() != hash_algorithm.hash_length()
    || checkpoint.target_root.len() != hash_algorithm.hash_length()
    || checkpoint.journal_head.len() != hash_algorithm.hash_length()
  {
    return Err(IndexRecoveryErrorV1::Invalid("checkpoint identity or required recovery edges disagree with the operation owner"));
  }
  Ok(())
}

fn validate_dependency_inputs(
  hash_algorithm: HashAlgorithm,
  dependencies: &[&EncodedImmutableIndexArtifactV1],
  options: IndexRecoveryOptionsV1,
) -> Result<(), IndexRecoveryErrorV1> {
  let maximum_dependencies = options
    .maximum_attachments
    .checked_add(options.maximum_journal_segments)
    .ok_or(IndexRecoveryErrorV1::Invalid("dependency count limit overflowed"))?;
  if dependencies.len() > maximum_dependencies {
    return Err(IndexRecoveryErrorV1::Invalid("dependency count exceeds the recovery limit"));
  }
  let mut bytes = 0u64;
  for (index, dependency) in dependencies.iter().enumerate() {
    let decoded = decode_immutable_index_artifact(
      &dependency.value,
      hash_algorithm,
      ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length(),
    )?;
    let kind = ImmutableIndexArtifactKindV1::from_u16(decoded.kind)
      .ok_or(IndexRecoveryErrorV1::Invalid("dependency uses an unknown immutable artifact kind"))?;
    if decoded.key != dependency.key
      || kind == ImmutableIndexArtifactKindV1::IndexTaskCheckpoint
      || dependency.value.len() > kind.maximum_encoded_length()
    {
      return Err(IndexRecoveryErrorV1::Invalid("dependency bytes have the wrong immutable identity or kind"));
    }
    if dependencies[..index].iter().any(|prior| prior.key == dependency.key) {
      return Err(IndexRecoveryErrorV1::Invalid("dependency list contains a duplicate immutable identity"));
    }
    let length = match u64::try_from(dependency.value.len()) {
      Ok(length) => length,
      Err(source) => return Err(IndexRecoveryErrorV1::Arithmetic(source.to_string())),
    };
    bytes = bytes.checked_add(length).ok_or(IndexRecoveryErrorV1::Invalid("dependency byte count overflowed"))?;
  }
  let total_limit = options
    .maximum_attachment_bytes
    .checked_add(options.maximum_journal_bytes)
    .ok_or(IndexRecoveryErrorV1::Invalid("combined dependency limit overflowed"))?;
  if bytes > total_limit {
    return Err(IndexRecoveryErrorV1::Invalid("dependency bytes exceed the recovery limit"));
  }
  Ok(())
}

fn validate_checkpoint_advance(current: Option<&IndexCheckpointRootV1>, next: &IndexCheckpointRootV1) -> Result<(), IndexRecoveryErrorV1> {
  match current {
    Some(current) if current == next => Ok(()),
    Some(current) => {
      let expected = current.checkpoint_sequence.checked_add(1).ok_or(IndexRecoveryErrorV1::Invalid("checkpoint sequence exhausted"))?;
      if next.checkpoint_sequence != expected {
        return Err(IndexRecoveryErrorV1::Invalid("checkpoint sequence is not the exact successor"));
      }
      Ok(())
    }
    None if next.checkpoint_sequence == 1 => Ok(()),
    None => Err(IndexRecoveryErrorV1::Invalid("first selected checkpoint sequence must be one")),
  }
}

fn journal_matches_checkpoint(summary: &CoverageJournalReplaySummaryV1, checkpoint: &IndexTaskCheckpointV1<'_>) -> bool {
  summary.head == checkpoint.journal_head
    && summary.source_root_before == checkpoint.source_root
    && summary.source_root_after == checkpoint.target_root
    && checkpoint.journal_floor_sequence <= summary.first_sequence
    && checkpoint.journal_audited_through == summary.last_sequence
}

fn missing_attachment_reason(role: IndexTaskAttachmentRoleV1) -> IndexRecoveryReasonV1 {
  if role == IndexTaskAttachmentRoleV1::MutationJournalHead {
    IndexRecoveryReasonV1::JournalMissing
  } else {
    IndexRecoveryReasonV1::AttachmentMissing
  }
}

fn corrupt_attachment_reason(role: IndexTaskAttachmentRoleV1) -> IndexRecoveryReasonV1 {
  if role == IndexTaskAttachmentRoleV1::MutationJournalHead {
    IndexRecoveryReasonV1::JournalCorrupt
  } else {
    IndexRecoveryReasonV1::AttachmentCorrupt
  }
}

fn map_rebuild_reason(reason: CoverageRebuildReasonV1) -> IndexRecoveryReasonV1 {
  match reason {
    CoverageRebuildReasonV1::JournalMissing => IndexRecoveryReasonV1::JournalMissing,
    CoverageRebuildReasonV1::JournalCorrupt => IndexRecoveryReasonV1::JournalCorrupt,
    CoverageRebuildReasonV1::JournalChainDiscontinuous => IndexRecoveryReasonV1::JournalChainDiscontinuous,
    CoverageRebuildReasonV1::JournalLimitExceeded => IndexRecoveryReasonV1::RecoveryLimitExceeded,
    CoverageRebuildReasonV1::AuthorityMissing
    | CoverageRebuildReasonV1::AuthorityAmbiguous
    | CoverageRebuildReasonV1::AuthorityCorrupt
    | CoverageRebuildReasonV1::AuthorityDiscontinuity
    | CoverageRebuildReasonV1::ConflictingMutation
    | CoverageRebuildReasonV1::InvalidNotice
    | CoverageRebuildReasonV1::WindowLimitExceeded
    | CoverageRebuildReasonV1::WholeRootTransition => IndexRecoveryReasonV1::JournalChainDiscontinuous,
  }
}

fn reconciliation(reason: IndexRecoveryReasonV1) -> IndexRecoveryOutcomeV1 {
  IndexRecoveryOutcomeV1::ReconciliationRequired { reason, evidence: None }
}

fn reconciliation_from(reason: IndexRecoveryReasonV1, evidence: impl std::fmt::Display) -> IndexRecoveryOutcomeV1 {
  IndexRecoveryOutcomeV1::ReconciliationRequired { reason, evidence: Some(evidence.to_string()) }
}

fn attachment_reconciliation(reason: IndexRecoveryReasonV1) -> AttachmentClosureOutcomeV1 {
  AttachmentClosureOutcomeV1::ReconciliationRequired { reason, evidence: None }
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), IndexRecoveryErrorV1> {
  if cancellation.is_cancelled() {
    Err(IndexRecoveryErrorV1::Canceled)
  } else {
    Ok(())
  }
}
