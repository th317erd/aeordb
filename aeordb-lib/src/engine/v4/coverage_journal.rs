use crate::engine::HashAlgorithm;
use crate::engine::namespace_mutation::{NamespaceMutationKind, NamespaceMutationSourceIdentity};

use super::coverage_runtime::{
  CoverageAuthorityV1, CoverageBoundaryV1, CoverageControlIdentityV1, CoverageReconciliationV1, CoverageTrackerV1, SoftMutationNoticeV1,
};
use super::hash::digest_parts;
use super::index_artifact::EncodedImmutableIndexArtifactV1;
use super::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal, validate_journal_chain,
};
use super::namespace::{NamespaceRootV1, SemanticStateV1};
use super::system_family::SystemFamilyRegistryV1;

const SYSTEM_INDEX_JOURNAL_ID: [u8; 16] = *b"AEORIDXJOURNALV1";
const MUTATION_ID_DOMAIN: &[u8] = b"aeordb.coverage-mutation.v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum CoverageControlDomainV1 {
  SemanticStateRoot = 1,
  SystemFamilySemanticProjection = 2,
}

pub fn build_coverage_authority(
  hash_algorithm: HashAlgorithm,
  root: &NamespaceRootV1,
  semantic_state: &SemanticStateV1,
  registry: &SystemFamilyRegistryV1<'_>,
) -> Result<CoverageAuthorityV1, CoverageJournalErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  require_hash(&root.root_hash, hash_width, "namespace root")?;
  require_hash(&root.namespace_tree_root, hash_width, "namespace tree root")?;
  require_hash(&root.semantic_state_root, hash_width, "semantic-state edge")?;
  require_hash(&semantic_state.object_id, hash_width, "semantic-state object")?;
  require_hash(&registry.semantic_projection_fingerprint, hash_width, "SystemFamily semantic projection")?;
  if root.semantic_state_root != semantic_state.object_id {
    return Err(CoverageJournalErrorV1::AuthorityClosure("NamespaceRoot semantic-state edge does not match the verified semantic object"));
  }

  CoverageAuthorityV1::new(
    hash_algorithm,
    root.root_hash.clone(),
    vec![
      CoverageControlIdentityV1 { domain: CoverageControlDomainV1::SemanticStateRoot as u16, identity: semantic_state.object_id.clone() },
      CoverageControlIdentityV1 {
        domain: CoverageControlDomainV1::SystemFamilySemanticProjection as u16,
        identity: registry.semantic_projection_fingerprint.clone(),
      },
    ],
  )
  .map_err(CoverageJournalErrorV1::Runtime)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageAuthoritySelectionV1 {
  Selected(CoverageBoundaryV1),
  Missing,
  Ambiguous,
  Corrupt,
  Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageRebuildReasonV1 {
  AuthorityMissing,
  AuthorityAmbiguous,
  AuthorityCorrupt,
  AuthorityDiscontinuity,
  ConflictingMutation,
  InvalidNotice,
  WindowLimitExceeded,
  WholeRootTransition,
  JournalMissing,
  JournalCorrupt,
  JournalChainDiscontinuous,
  JournalLimitExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageRecoveryOutcomeV1<T> {
  Verified(T),
  RebuildRequired { reason: CoverageRebuildReasonV1, evidence: Option<CoverageJournalErrorV1> },
  Canceled,
}

impl<T> CoverageRecoveryOutcomeV1<T> {
  pub fn rebuild(reason: CoverageRebuildReasonV1) -> Self {
    Self::RebuildRequired { reason, evidence: None }
  }

  fn rebuild_from(reason: CoverageRebuildReasonV1, evidence: CoverageJournalErrorV1) -> Self {
    Self::RebuildRequired { reason, evidence: Some(evidence) }
  }
}

pub type CoverageAuthorityReconciliationOutcomeV1 = CoverageRecoveryOutcomeV1<CoverageReconciliationV1>;
pub type CoverageJournalReplayOutcomeV1 = CoverageRecoveryOutcomeV1<CoverageJournalReplaySummaryV1>;

pub fn reconcile_authority_selection(
  tracker: &CoverageTrackerV1,
  selection: CoverageAuthoritySelectionV1,
) -> CoverageAuthorityReconciliationOutcomeV1 {
  match selection {
    CoverageAuthoritySelectionV1::Selected(boundary) => {
      match tracker.reconcile_against(&boundary.authority, boundary.publication_sequence) {
        Ok(reconciliation) => CoverageAuthorityReconciliationOutcomeV1::Verified(reconciliation),
        Err(error) => CoverageAuthorityReconciliationOutcomeV1::rebuild_from(
          CoverageRebuildReasonV1::AuthorityCorrupt,
          CoverageJournalErrorV1::Runtime(error),
        ),
      }
    }
    CoverageAuthoritySelectionV1::Missing => CoverageAuthorityReconciliationOutcomeV1::rebuild(CoverageRebuildReasonV1::AuthorityMissing),
    CoverageAuthoritySelectionV1::Ambiguous => {
      CoverageAuthorityReconciliationOutcomeV1::rebuild(CoverageRebuildReasonV1::AuthorityAmbiguous)
    }
    CoverageAuthoritySelectionV1::Corrupt => CoverageAuthorityReconciliationOutcomeV1::rebuild(CoverageRebuildReasonV1::AuthorityCorrupt),
    CoverageAuthoritySelectionV1::Canceled => CoverageAuthorityReconciliationOutcomeV1::Canceled,
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverageJournalWindowOptionsV1 {
  maximum_notices: usize,
  maximum_retained_bytes: usize,
}

impl CoverageJournalWindowOptionsV1 {
  pub fn new(maximum_notices: usize, maximum_retained_bytes: usize) -> Result<Self, CoverageJournalErrorV1> {
    if maximum_notices == 0 || maximum_retained_bytes == 0 {
      return Err(CoverageJournalErrorV1::InvalidOptions("journal window limits must be nonzero"));
    }
    Ok(Self { maximum_notices, maximum_retained_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageJournalWindowV1 {
  notices: Vec<SoftMutationNoticeV1>,
  retained_bytes: usize,
  root_before: Vec<u8>,
  root_after: Vec<u8>,
  selected_authority: CoverageAuthorityV1,
}

impl CoverageJournalWindowV1 {
  pub fn notices(&self) -> &[SoftMutationNoticeV1] {
    &self.notices
  }

  pub fn retained_bytes(&self) -> usize {
    self.retained_bytes
  }

  pub fn root_before(&self) -> &[u8] {
    &self.root_before
  }

  pub fn root_after(&self) -> &[u8] {
    &self.root_after
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoverageJournalWindowOutcomeV1 {
  Exact(CoverageJournalWindowV1),
  BoundedDiffRequired { reason: CoverageRebuildReasonV1 },
  RebuildRequired(CoverageRebuildReasonV1),
}

pub fn order_soft_mutation_window(
  hash_algorithm: HashAlgorithm,
  mut notices: Vec<SoftMutationNoticeV1>,
  covered: &CoverageBoundaryV1,
  selected: &CoverageBoundaryV1,
  options: CoverageJournalWindowOptionsV1,
) -> CoverageJournalWindowOutcomeV1 {
  if notices.len() > options.maximum_notices {
    return CoverageJournalWindowOutcomeV1::RebuildRequired(CoverageRebuildReasonV1::WindowLimitExceeded);
  }
  let Some(retained_bytes) = notices.iter().try_fold(0usize, |sum, notice| sum.checked_add(notice.retained_bytes())) else {
    return CoverageJournalWindowOutcomeV1::RebuildRequired(CoverageRebuildReasonV1::WindowLimitExceeded);
  };
  if retained_bytes > options.maximum_retained_bytes {
    return CoverageJournalWindowOutcomeV1::RebuildRequired(CoverageRebuildReasonV1::WindowLimitExceeded);
  }

  let hash_width = hash_algorithm.hash_length();
  if covered.authority.source_namespace_root.len() != hash_width
    || selected.authority.source_namespace_root.len() != hash_width
    || selected.publication_sequence < covered.publication_sequence
  {
    return CoverageJournalWindowOutcomeV1::RebuildRequired(CoverageRebuildReasonV1::AuthorityCorrupt);
  }
  if covered.authority.control_identities != selected.authority.control_identities {
    return CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::AuthorityDiscontinuity };
  }
  if notices.iter().any(|notice| !valid_notice(notice, hash_width)) {
    return CoverageJournalWindowOutcomeV1::RebuildRequired(CoverageRebuildReasonV1::InvalidNotice);
  }
  if notices.iter().any(|notice| notice.source_identities.is_empty()) {
    return CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::WholeRootTransition };
  }

  notices.sort_unstable_by(|left, right| {
    left.operation_id.cmp(&right.operation_id).then_with(|| left.committed_at_ms.cmp(&right.committed_at_ms))
  });
  let mut conflicting_operation = false;
  notices.dedup_by(|later, earlier| {
    if later.operation_id != earlier.operation_id {
      return false;
    }
    if same_soft_mutation(later, earlier) {
      earlier.committed_at_ms = earlier.committed_at_ms.min(later.committed_at_ms);
      true
    } else {
      conflicting_operation = true;
      false
    }
  });
  if conflicting_operation {
    return CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::ConflictingMutation };
  }
  notices.sort_unstable_by(|left, right| {
    left.publication_sequence.cmp(&right.publication_sequence).then_with(|| left.operation_id.cmp(&right.operation_id))
  });

  let mut expected_root = covered.authority.source_namespace_root.as_slice();
  let mut previous_sequence = covered.publication_sequence;
  for notice in &notices {
    if notice.publication_sequence <= previous_sequence || notice.previous_namespace_root != expected_root {
      return CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::AuthorityDiscontinuity };
    }
    expected_root = &notice.namespace_root;
    previous_sequence = notice.publication_sequence;
  }

  if previous_sequence > selected.publication_sequence || expected_root != selected.authority.source_namespace_root {
    return CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::AuthorityDiscontinuity };
  }
  if notices.is_empty() && covered.authority != selected.authority {
    return CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason: CoverageRebuildReasonV1::AuthorityDiscontinuity };
  }

  CoverageJournalWindowOutcomeV1::Exact(CoverageJournalWindowV1 {
    notices,
    retained_bytes,
    root_before: covered.authority.source_namespace_root.clone(),
    root_after: selected.authority.source_namespace_root.clone(),
    selected_authority: selected.authority.clone(),
  })
}

fn same_soft_mutation(left: &SoftMutationNoticeV1, right: &SoftMutationNoticeV1) -> bool {
  left.kind == right.kind
    && left.publication_sequence == right.publication_sequence
    && left.previous_namespace_root == right.previous_namespace_root
    && left.namespace_root == right.namespace_root
    && left.source_identities == right.source_identities
}

fn valid_notice(notice: &SoftMutationNoticeV1, hash_width: usize) -> bool {
  if notice.operation_id.iter().all(|byte| *byte == 0)
    || notice.publication_sequence == 0
    || notice.committed_at_ms == 0
    || !valid_hash(&notice.previous_namespace_root, hash_width)
    || !valid_hash(&notice.namespace_root, hash_width)
    || notice.previous_namespace_root == notice.namespace_root
  {
    return false;
  }
  notice.source_identities.iter().all(|source| {
    (source.previous_identity.is_some() || source.new_identity.is_some())
      && source.previous_identity.as_ref().is_none_or(|identity| valid_hash(identity, hash_width))
      && source.new_identity.as_ref().is_none_or(|identity| valid_hash(identity, hash_width))
  })
}

fn valid_hash(value: &[u8], hash_width: usize) -> bool {
  value.len() == hash_width && value.iter().any(|byte| *byte != 0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageJournalEncodeOptionsV1 {
  pub generation: u64,
  pub segment_ordinal: u64,
  pub previous_segment: Vec<u8>,
  pub runtime_boot_id: [u8; 16],
}

pub fn encode_soft_mutation_journal_segment(
  hash_algorithm: HashAlgorithm,
  window: &CoverageJournalWindowV1,
  options: CoverageJournalEncodeOptionsV1,
) -> Result<EncodedImmutableIndexArtifactV1, CoverageJournalErrorV1> {
  if window.notices.is_empty() {
    return Err(CoverageJournalErrorV1::EmptyWindow);
  }
  let semantic_state_root = window
    .selected_authority
    .control_identities
    .iter()
    .find(|control| control.domain == CoverageControlDomainV1::SemanticStateRoot as u16)
    .map(|control| control.identity.as_slice())
    .ok_or(CoverageJournalErrorV1::AuthorityClosure("selected coverage authority has no semantic-state identity"))?;
  encode_owned_soft_mutation_journal_segment(
    hash_algorithm,
    window,
    SYSTEM_INDEX_JOURNAL_ID,
    JournalOwnerKindV1::System,
    semantic_state_root,
    options,
  )
}

pub fn encode_owned_soft_mutation_journal_segment(
  hash_algorithm: HashAlgorithm,
  window: &CoverageJournalWindowV1,
  owner_id: [u8; 16],
  owner_kind: JournalOwnerKindV1,
  semantic_state_root: &[u8],
  options: CoverageJournalEncodeOptionsV1,
) -> Result<EncodedImmutableIndexArtifactV1, CoverageJournalErrorV1> {
  if window.notices.is_empty() {
    return Err(CoverageJournalErrorV1::EmptyWindow);
  }
  let record_count = window.notices.iter().try_fold(0usize, |sum, notice| sum.checked_add(notice.source_identities.len()));
  let Some(record_count) = record_count else {
    return Err(CoverageJournalErrorV1::RecordLimitExceeded);
  };
  if record_count == 0 || record_count > 10_000 {
    return Err(CoverageJournalErrorV1::RecordLimitExceeded);
  }

  let mut mutation_ids = Vec::new();
  mutation_ids.try_reserve_exact(window.notices.len()).map_err(|error| CoverageJournalErrorV1::Allocation(error.to_string()))?;
  for notice in &window.notices {
    mutation_ids.push(digest_parts(hash_algorithm, &[MUTATION_ID_DOMAIN, &notice.operation_id]));
  }
  let mut records = Vec::new();
  records.try_reserve_exact(record_count).map_err(|error| CoverageJournalErrorV1::Allocation(error.to_string()))?;
  for (notice_index, notice) in window.notices.iter().enumerate() {
    let batch_count = notice.source_identities.len() as u32;
    for (batch_ordinal, source) in notice.source_identities.iter().enumerate() {
      records.push(MutationRecordWriteV1 {
        kind: journal_mutation_kind(notice.kind, source),
        sequence: notice.publication_sequence,
        mutation_id: &mutation_ids[notice_index],
        batch_ordinal: batch_ordinal as u32,
        batch_count,
        root_before: &notice.previous_namespace_root,
        root_after: &notice.namespace_root,
        before: source.previous_identity.as_deref().map(|revision| MutationSideWriteV1 { path: &source.path, revision }),
        after: source.new_identity.as_deref().map(|revision| MutationSideWriteV1 { path: &source.path, revision }),
        committed_at_ms: notice.committed_at_ms,
      });
    }
  }

  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm,
    owner_id,
    owner_kind,
    generation: options.generation,
    segment_ordinal: options.segment_ordinal,
    chain_reset: options.previous_segment.iter().all(|byte| *byte == 0),
    previous_segment: &options.previous_segment,
    semantic_state_root,
    runtime_boot_id: options.runtime_boot_id,
    records: &records,
  })
  .map_err(CoverageJournalErrorV1::Format)
}

fn journal_mutation_kind(kind: NamespaceMutationKind, source: &NamespaceMutationSourceIdentity) -> MutationKindV1 {
  match (source.previous_identity.is_some(), source.new_identity.is_some()) {
    (false, true) if kind == NamespaceMutationKind::Copy => MutationKindV1::Copy,
    (false, true) if kind == NamespaceMutationKind::Restore => MutationKindV1::Restore,
    (false, true) => MutationKindV1::Create,
    (true, false) => MutationKindV1::Delete,
    (true, true) => MutationKindV1::Update,
    (false, false) => MutationKindV1::Transition,
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageJournalReplayExpectationV1 {
  pub generation: u64,
  pub first_segment_ordinal: u64,
  pub previous_segment: Vec<u8>,
  pub source_root_before: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverageJournalReplayOptionsV1 {
  maximum_segments: usize,
  maximum_encoded_bytes: usize,
}

impl CoverageJournalReplayOptionsV1 {
  pub fn new(maximum_segments: usize, maximum_encoded_bytes: usize) -> Result<Self, CoverageJournalErrorV1> {
    if maximum_segments == 0 || maximum_encoded_bytes == 0 {
      return Err(CoverageJournalErrorV1::InvalidOptions("journal replay limits must be nonzero"));
    }
    Ok(Self { maximum_segments, maximum_encoded_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageJournalReplaySummaryV1 {
  pub segment_count: usize,
  pub record_count: usize,
  pub first_sequence: u64,
  pub last_sequence: u64,
  pub source_root_before: Vec<u8>,
  pub source_root_after: Vec<u8>,
  pub semantic_state_root: Vec<u8>,
  pub head: Vec<u8>,
}

pub fn replay_system_journal_chain(
  hash_algorithm: HashAlgorithm,
  segments: &[Vec<u8>],
  expectation: &CoverageJournalReplayExpectationV1,
  options: CoverageJournalReplayOptionsV1,
  cancellation: &tokio_util::sync::CancellationToken,
) -> CoverageJournalReplayOutcomeV1 {
  if cancellation.is_cancelled() {
    return CoverageJournalReplayOutcomeV1::Canceled;
  }
  if segments.is_empty() {
    return CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalMissing);
  }
  if segments.len() > options.maximum_segments {
    return CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded);
  }
  let Some(total_bytes) = segments.iter().try_fold(0usize, |sum, segment| sum.checked_add(segment.len())) else {
    return CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded);
  };
  if total_bytes > options.maximum_encoded_bytes {
    return CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded);
  }

  let mut previous = None;
  let mut record_count = 0usize;
  let mut summary = None;
  for (index, bytes) in segments.iter().enumerate() {
    if cancellation.is_cancelled() {
      return CoverageJournalReplayOutcomeV1::Canceled;
    }
    let journal = match decode_mutation_journal(bytes, hash_algorithm) {
      Ok(journal) => journal,
      Err(error) => {
        return CoverageJournalReplayOutcomeV1::rebuild_from(
          CoverageRebuildReasonV1::JournalCorrupt,
          CoverageJournalErrorV1::Format(error),
        );
      }
    };
    if journal.owner_kind != JournalOwnerKindV1::System
      || journal.owner_id != SYSTEM_INDEX_JOURNAL_ID
      || journal.generation != expectation.generation
    {
      return CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalChainDiscontinuous);
    }
    if index == 0 {
      if journal.segment_ordinal != expectation.first_segment_ordinal
        || journal.previous_segment != expectation.previous_segment
        || journal.source_root_before != expectation.source_root_before
      {
        return CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalChainDiscontinuous);
      }
      summary = Some(CoverageJournalReplaySummaryV1 {
        segment_count: segments.len(),
        record_count: 0,
        first_sequence: journal.first_sequence,
        last_sequence: journal.last_sequence,
        source_root_before: journal.source_root_before.to_vec(),
        source_root_after: journal.source_root_after.to_vec(),
        semantic_state_root: journal.semantic_state_root.to_vec(),
        head: journal.key.clone(),
      });
    } else {
      let Some(prior) = previous.as_ref() else {
        return CoverageJournalReplayOutcomeV1::rebuild_from(
          CoverageRebuildReasonV1::JournalChainDiscontinuous,
          CoverageJournalErrorV1::AuthorityClosure("noninitial journal segment has no prior decoded segment"),
        );
      };
      if let Err(error) = validate_journal_chain(prior, &journal) {
        return CoverageJournalReplayOutcomeV1::rebuild_from(
          CoverageRebuildReasonV1::JournalChainDiscontinuous,
          CoverageJournalErrorV1::Format(error),
        );
      }
    }

    for record in journal.records.iter() {
      if cancellation.is_cancelled() {
        return CoverageJournalReplayOutcomeV1::Canceled;
      }
      if let Err(error) = record {
        return CoverageJournalReplayOutcomeV1::rebuild_from(
          CoverageRebuildReasonV1::JournalCorrupt,
          CoverageJournalErrorV1::Format(error),
        );
      }
      let Some(next) = record_count.checked_add(1) else {
        return CoverageJournalReplayOutcomeV1::rebuild(CoverageRebuildReasonV1::JournalLimitExceeded);
      };
      record_count = next;
    }
    if let Some(current) = summary.as_mut() {
      current.record_count = record_count;
      current.last_sequence = journal.last_sequence;
      current.source_root_after = journal.source_root_after.to_vec();
      current.semantic_state_root = journal.semantic_state_root.to_vec();
      current.head = journal.key.clone();
    }
    previous = Some(journal);
  }

  match summary {
    Some(summary) => CoverageJournalReplayOutcomeV1::Verified(summary),
    None => CoverageJournalReplayOutcomeV1::rebuild_from(
      CoverageRebuildReasonV1::JournalCorrupt,
      CoverageJournalErrorV1::AuthorityClosure("nonempty journal replay produced no summary"),
    ),
  }
}

fn require_hash(value: &[u8], expected: usize, role: &'static str) -> Result<(), CoverageJournalErrorV1> {
  if valid_hash(value, expected) {
    Ok(())
  } else {
    Err(CoverageJournalErrorV1::InvalidHash { role, expected, actual: value.len() })
  }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoverageJournalErrorV1 {
  #[error("invalid coverage journal options: {0}")]
  InvalidOptions(&'static str),
  #[error("coverage authority {role} has invalid hash width/content: expected {expected} bytes, got {actual}")]
  InvalidHash { role: &'static str, expected: usize, actual: usize },
  #[error("coverage authority closure is invalid: {0}")]
  AuthorityClosure(&'static str),
  #[error("coverage runtime authority is invalid: {0}")]
  Runtime(#[from] super::coverage_runtime::CoverageRuntimeErrorV1),
  #[error("coverage journal cannot encode an empty mutation window")]
  EmptyWindow,
  #[error("coverage journal record count exceeds the frozen segment bound")]
  RecordLimitExceeded,
  #[error("coverage journal allocation failed: {0}")]
  Allocation(String),
  #[error(transparent)]
  Format(#[from] super::reader::FormatError),
}
