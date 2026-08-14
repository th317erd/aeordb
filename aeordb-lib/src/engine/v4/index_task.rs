use std::cmp::Ordering;

use crate::engine::HashAlgorithm;

use super::hash::digest_parts;
use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, ImmutableIndexArtifactWriteV1, IndexManifestKindV1,
  checked_immutable_index_artifact_encoded_length, decode_immutable_index_artifact, decode_index_manifest, encode_immutable_index_artifact,
  u16_at, u32_at, u64_at,
};
use super::index_page::{OrderedIndexRoleV1, decode_artifact_directory};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::scope::validate_canonical_absolute_path;

const MUTATION_JOURNAL_KIND: u16 = 0x0040;
const INDEX_TASK_CHECKPOINT_KIND: u16 = 0x0041;
const MAX_JOURNAL_LENGTH: usize = 16 * 1_024 * 1_024;
const MAX_CHECKPOINT_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_JOURNAL_RECORDS: u32 = 10_000;
const MAX_RESUME_KEY_LENGTH: usize = 1_024 * 1_024;
const MAX_ATTACHMENTS: u32 = 4_096;
const MAX_EXTERNAL_DESCRIPTOR_LENGTH: usize = 64 * 1_024;
const SYSTEM_INDEX_JOURNAL_ID: [u8; 16] = *b"AEORIDXJOURNALV1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalOwnerKindV1 {
  Task,
  System,
}

impl JournalOwnerKindV1 {
  pub fn name(self) -> &'static str {
    match self {
      Self::Task => "task",
      Self::System => "system",
    }
  }

  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::Task),
      2 => Some(Self::System),
      _ => None,
    }
  }

  fn id(self) -> u16 {
    match self {
      Self::Task => 1,
      Self::System => 2,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKindV1 {
  Create,
  Update,
  Delete,
  Move,
  Copy,
  Restore,
  Transition,
}

impl MutationKindV1 {
  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::Create),
      2 => Some(Self::Update),
      3 => Some(Self::Delete),
      4 => Some(Self::Move),
      5 => Some(Self::Copy),
      6 => Some(Self::Restore),
      7 => Some(Self::Transition),
      _ => None,
    }
  }

  fn id(self) -> u16 {
    match self {
      Self::Create => 1,
      Self::Update => 2,
      Self::Delete => 3,
      Self::Move => 4,
      Self::Copy => 5,
      Self::Restore => 6,
      Self::Transition => 7,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationSideWriteV1<'a> {
  pub path: &'a str,
  pub revision: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationRecordWriteV1<'a> {
  pub kind: MutationKindV1,
  pub sequence: u64,
  pub mutation_id: &'a [u8],
  pub batch_ordinal: u32,
  pub batch_count: u32,
  pub root_before: &'a [u8],
  pub root_after: &'a [u8],
  pub before: Option<MutationSideWriteV1<'a>>,
  pub after: Option<MutationSideWriteV1<'a>>,
  pub committed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationJournalWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub owner_id: [u8; 16],
  pub owner_kind: JournalOwnerKindV1,
  pub generation: u64,
  pub segment_ordinal: u64,
  pub chain_reset: bool,
  pub previous_segment: &'a [u8],
  pub semantic_state_root: &'a [u8],
  pub runtime_boot_id: [u8; 16],
  pub records: &'a [MutationRecordWriteV1<'a>],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationRecordV1<'a> {
  pub kind: MutationKindV1,
  pub sequence: u64,
  pub mutation_id: &'a [u8],
  pub batch_ordinal: u32,
  pub batch_count: u32,
  pub root_before: &'a [u8],
  pub root_after: &'a [u8],
  pub before_path: Option<&'a str>,
  pub before_file_key: Option<&'a [u8]>,
  pub before_revision: Option<&'a [u8]>,
  pub after_path: Option<&'a str>,
  pub after_file_key: Option<&'a [u8]>,
  pub after_revision: Option<&'a [u8]>,
  pub committed_at_ms: u64,
  pub encoded: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationRecordsV1<'a> {
  hash_algorithm: HashAlgorithm,
  bytes: &'a [u8],
  count: u32,
}

impl<'a> MutationRecordsV1<'a> {
  pub fn len(&self) -> usize {
    self.count as usize
  }

  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  pub fn iter(&self) -> MutationRecordIteratorV1<'a> {
    MutationRecordIteratorV1 { records: self.clone(), offset: 0, remaining: self.count, failed: false }
  }
}

pub struct MutationRecordIteratorV1<'a> {
  records: MutationRecordsV1<'a>,
  offset: usize,
  remaining: u32,
  failed: bool,
}

impl<'a> Iterator for MutationRecordIteratorV1<'a> {
  type Item = FormatResult<MutationRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed || self.remaining == 0 {
      return None;
    }
    self.remaining -= 1;
    let decoded = decode_mutation_record(self.records.hash_algorithm, self.records.bytes, self.offset);
    match decoded {
      Ok((record, next)) => {
        self.offset = next;
        Some(Ok(record))
      }
      Err(error) => {
        self.failed = true;
        Some(Err(error))
      }
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let remaining = if self.failed { 0 } else { self.remaining as usize };
    (remaining, Some(remaining))
  }
}

impl ExactSizeIterator for MutationRecordIteratorV1<'_> {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationJournalV1<'a> {
  pub owner_id: [u8; 16],
  pub owner_kind: JournalOwnerKindV1,
  pub generation: u64,
  pub segment_ordinal: u64,
  pub chain_reset: bool,
  pub previous_segment: &'a [u8],
  pub source_root_before: &'a [u8],
  pub source_root_after: &'a [u8],
  pub semantic_state_root: &'a [u8],
  pub runtime_boot_id: [u8; 16],
  pub first_sequence: u64,
  pub last_sequence: u64,
  pub records: MutationRecordsV1<'a>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexTaskKindV1 {
  ScopeBuild,
  ValueBuild,
  FieldBuild,
  NvtBuild,
  Reconcile,
  V0Migration,
  Compaction,
  IndexRepair,
}

impl IndexTaskKindV1 {
  pub fn name(self) -> &'static str {
    match self {
      Self::ScopeBuild => "scope-build",
      Self::ValueBuild => "value-build",
      Self::FieldBuild => "field-build",
      Self::NvtBuild => "nvt-build",
      Self::Reconcile => "reconcile",
      Self::V0Migration => "v0-migration",
      Self::Compaction => "compaction",
      Self::IndexRepair => "index-repair",
    }
  }

  pub fn phase_name(self, phase: u16) -> Option<&'static str> {
    let phases: &[&str] = match self {
      Self::ScopeBuild => &["capture-source", "scan-scope", "build-ordinal", "build-reverse", "validate", "publish"],
      Self::ValueBuild => &["capture-source", "scan-documents", "parse-select", "build-values", "validate", "publish"],
      Self::FieldBuild => &["capture-source", "scan-values", "convert-expand", "build-postings", "validate", "publish"],
      Self::NvtBuild => &["pin-posting-manifest", "scan-postings", "build-tiles", "validate", "publish"],
      Self::Reconcile => &["capture-heads", "replay-journal", "diff-roots", "apply", "validate", "publish"],
      Self::V0Migration => &["capture-source", "scan-legacy", "convert", "replay-mutations", "validate", "publish"],
      Self::Compaction => &["select-generation", "rewrite-pages", "rebuild-directory", "validate", "publish"],
      Self::IndexRepair => &["diagnose", "select-salvage", "rebuild", "validate", "publish"],
    };
    phase.checked_sub(1).and_then(|index| phases.get(index as usize)).copied()
  }

  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::ScopeBuild),
      2 => Some(Self::ValueBuild),
      3 => Some(Self::FieldBuild),
      4 => Some(Self::NvtBuild),
      5 => Some(Self::Reconcile),
      6 => Some(Self::V0Migration),
      7 => Some(Self::Compaction),
      8 => Some(Self::IndexRepair),
      _ => None,
    }
  }

  fn id(self) -> u16 {
    match self {
      Self::ScopeBuild => 1,
      Self::ValueBuild => 2,
      Self::FieldBuild => 3,
      Self::NvtBuild => 4,
      Self::Reconcile => 5,
      Self::V0Migration => 6,
      Self::Compaction => 7,
      Self::IndexRepair => 8,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexTaskStateV1 {
  Running,
  CancelRequested,
  Canceled,
  FailedRetryable,
  FailedTerminal,
  CompleteUnpublished,
  Published,
}

impl IndexTaskStateV1 {
  pub fn name(self) -> &'static str {
    match self {
      Self::Running => "running",
      Self::CancelRequested => "cancel-requested",
      Self::Canceled => "canceled",
      Self::FailedRetryable => "failed-retryable",
      Self::FailedTerminal => "failed-terminal",
      Self::CompleteUnpublished => "complete-unpublished",
      Self::Published => "published",
    }
  }

  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::Running),
      2 => Some(Self::CancelRequested),
      3 => Some(Self::Canceled),
      4 => Some(Self::FailedRetryable),
      5 => Some(Self::FailedTerminal),
      6 => Some(Self::CompleteUnpublished),
      7 => Some(Self::Published),
      _ => None,
    }
  }

  fn id(self) -> u16 {
    match self {
      Self::Running => 1,
      Self::CancelRequested => 2,
      Self::Canceled => 3,
      Self::FailedRetryable => 4,
      Self::FailedTerminal => 5,
      Self::CompleteUnpublished => 6,
      Self::Published => 7,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexTaskAttachmentRoleV1 {
  ScopeOrdinalDirectoryRoot,
  ScopeReverseDirectoryRoot,
  ValueDirectoryRoot,
  ValueStateDirectoryRoot,
  PostingDirectoryRoot,
  IndexStateDirectoryRoot,
  NvtTileDirectoryRoot,
  CandidateScopeManifest,
  CandidateValueManifest,
  CandidateFieldManifest,
  CandidateNvtManifest,
  MutationJournalHead,
}

impl IndexTaskAttachmentRoleV1 {
  pub fn id(self) -> u16 {
    match self {
      Self::ScopeOrdinalDirectoryRoot => 1,
      Self::ScopeReverseDirectoryRoot => 2,
      Self::ValueDirectoryRoot => 3,
      Self::ValueStateDirectoryRoot => 4,
      Self::PostingDirectoryRoot => 5,
      Self::IndexStateDirectoryRoot => 6,
      Self::NvtTileDirectoryRoot => 7,
      Self::CandidateScopeManifest => 8,
      Self::CandidateValueManifest => 9,
      Self::CandidateFieldManifest => 10,
      Self::CandidateNvtManifest => 11,
      Self::MutationJournalHead => 12,
    }
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::ScopeOrdinalDirectoryRoot => "scope-ordinal-directory-root",
      Self::ScopeReverseDirectoryRoot => "scope-reverse-directory-root",
      Self::ValueDirectoryRoot => "value-directory-root",
      Self::ValueStateDirectoryRoot => "value-state-directory-root",
      Self::PostingDirectoryRoot => "posting-directory-root",
      Self::IndexStateDirectoryRoot => "index-state-directory-root",
      Self::NvtTileDirectoryRoot => "nvt-tile-directory-root",
      Self::CandidateScopeManifest => "candidate-scope-manifest",
      Self::CandidateValueManifest => "candidate-value-manifest",
      Self::CandidateFieldManifest => "candidate-field-manifest",
      Self::CandidateNvtManifest => "candidate-nvt-manifest",
      Self::MutationJournalHead => "mutation-journal-head",
    }
  }

  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::ScopeOrdinalDirectoryRoot),
      2 => Some(Self::ScopeReverseDirectoryRoot),
      3 => Some(Self::ValueDirectoryRoot),
      4 => Some(Self::ValueStateDirectoryRoot),
      5 => Some(Self::PostingDirectoryRoot),
      6 => Some(Self::IndexStateDirectoryRoot),
      7 => Some(Self::NvtTileDirectoryRoot),
      8 => Some(Self::CandidateScopeManifest),
      9 => Some(Self::CandidateValueManifest),
      10 => Some(Self::CandidateFieldManifest),
      11 => Some(Self::CandidateNvtManifest),
      12 => Some(Self::MutationJournalHead),
      _ => None,
    }
  }

  fn directory_role(self) -> Option<OrderedIndexRoleV1> {
    match self {
      Self::ScopeOrdinalDirectoryRoot => Some(OrderedIndexRoleV1::ScopeOrdinal),
      Self::ScopeReverseDirectoryRoot => Some(OrderedIndexRoleV1::ScopeReverse),
      Self::ValueDirectoryRoot => Some(OrderedIndexRoleV1::Value),
      Self::ValueStateDirectoryRoot => Some(OrderedIndexRoleV1::ValueDocumentState),
      Self::PostingDirectoryRoot => Some(OrderedIndexRoleV1::Posting),
      Self::IndexStateDirectoryRoot => Some(OrderedIndexRoleV1::IndexDocumentState),
      Self::NvtTileDirectoryRoot => Some(OrderedIndexRoleV1::NvtTile),
      _ => None,
    }
  }

  fn manifest_kind(self) -> Option<IndexManifestKindV1> {
    match self {
      Self::CandidateScopeManifest => Some(IndexManifestKindV1::ScopeCatalog),
      Self::CandidateValueManifest => Some(IndexManifestKindV1::ValueStore),
      Self::CandidateFieldManifest => Some(IndexManifestKindV1::FieldIndex),
      Self::CandidateNvtManifest => Some(IndexManifestKindV1::FieldNvt),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTaskAttachmentWriteV1<'a> {
  pub role: IndexTaskAttachmentRoleV1,
  pub owner_id: &'a [u8],
  pub artifact_hash: &'a [u8],
  pub birth_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTaskAttachmentV1<'a> {
  pub role: IndexTaskAttachmentRoleV1,
  pub owner_id: &'a [u8],
  pub artifact_hash: &'a [u8],
  pub birth_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexTaskAttachmentsV1<'a> {
  hash_width: usize,
  bytes: &'a [u8],
  count: u32,
}

impl<'a> IndexTaskAttachmentsV1<'a> {
  pub fn len(&self) -> usize {
    self.count as usize
  }

  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  pub fn entry_at(&self, index: usize) -> FormatResult<IndexTaskAttachmentV1<'a>> {
    if index >= self.len() {
      return Err(truncated_error("checkpoint attachment index is outside the declared count"));
    }
    decode_attachment(self.hash_width, self.bytes, index)
  }

  pub fn iter(&self) -> IndexTaskAttachmentIteratorV1<'a> {
    IndexTaskAttachmentIteratorV1 { attachments: self.clone(), index: 0, failed: false }
  }
}

pub struct IndexTaskAttachmentIteratorV1<'a> {
  attachments: IndexTaskAttachmentsV1<'a>,
  index: usize,
  failed: bool,
}

impl<'a> Iterator for IndexTaskAttachmentIteratorV1<'a> {
  type Item = FormatResult<IndexTaskAttachmentV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed || self.index == self.attachments.len() {
      return None;
    }
    let decoded = self.attachments.entry_at(self.index);
    self.index += 1;
    if decoded.is_err() {
      self.failed = true;
    }
    Some(decoded)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalWorkspaceDescriptorV1<'a> {
  pub workspace_id: [u8; 16],
  pub manifest_digest: [u8; 32],
  pub durable_sequence: u64,
  pub durable_bytes: u64,
  pub path: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalWorkspaceDescriptorWriteV1<'a> {
  pub workspace_id: [u8; 16],
  pub manifest_digest: [u8; 32],
  pub durable_sequence: u64,
  pub durable_bytes: u64,
  pub path: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexTaskCheckpointWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub task_id: [u8; 16],
  pub checkpoint_sequence: u64,
  pub generation: u64,
  pub task_kind: IndexTaskKindV1,
  pub state: IndexTaskStateV1,
  pub phase: u16,
  pub required_capabilities: &'a [u8; 32],
  pub started_at_ms: u64,
  pub updated_at_ms: u64,
  pub source_root: &'a [u8],
  pub target_root: Option<&'a [u8]>,
  pub primary_id: Option<&'a [u8]>,
  pub journal_head: Option<&'a [u8]>,
  pub journal_floor_sequence: u64,
  pub journal_audited_through: u64,
  pub next_document_ordinal: u64,
  pub completed_work: u64,
  pub total_work_hint: u64,
  pub resume_key: &'a [u8],
  pub attachments: &'a [IndexTaskAttachmentWriteV1<'a>],
  pub external: Option<ExternalWorkspaceDescriptorWriteV1<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexTaskCheckpointV1<'a> {
  pub task_id: [u8; 16],
  pub checkpoint_sequence: u64,
  pub generation: u64,
  pub task_kind: IndexTaskKindV1,
  pub state: IndexTaskStateV1,
  pub phase: u16,
  pub phase_name: &'static str,
  pub required_capabilities: &'a [u8],
  pub started_at_ms: u64,
  pub updated_at_ms: u64,
  pub source_root: &'a [u8],
  pub target_root: &'a [u8],
  pub primary_id: &'a [u8],
  pub journal_head: &'a [u8],
  pub journal_floor_sequence: u64,
  pub journal_audited_through: u64,
  pub next_document_ordinal: u64,
  pub completed_work: u64,
  pub total_work_hint: u64,
  pub resume_key: &'a [u8],
  pub attachments: IndexTaskAttachmentsV1<'a>,
  pub external: Option<ExternalWorkspaceDescriptorV1<'a>>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexTaskArtifactV1<'a> {
  Journal(MutationJournalV1<'a>),
  Checkpoint(IndexTaskCheckpointV1<'a>),
}

pub fn encode_mutation_journal(request: &MutationJournalWriteV1<'_>) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  validate_journal_write_header(request)?;
  if request.records.is_empty() || request.records.len() > MAX_JOURNAL_RECORDS as usize {
    return Err(amplification_error("journal record count is outside 1..=10000"));
  }

  let records_length = request.records.iter().try_fold(0usize, |length, record| {
    let record_length = checked_mutation_record_length(request.hash_algorithm, record)?;
    length.checked_add(record_length).ok_or_else(|| length_error("journal record byte length overflow"))
  })?;
  let records_length_u32 = checked_task_u32(records_length, "journal records exceed u32")?;
  let hash_width = request.hash_algorithm.hash_length();
  let fixed = 56usize.checked_add(4 * hash_width).ok_or_else(|| length_error("journal fixed length overflow"))?;
  let body_length = fixed.checked_add(records_length).ok_or_else(|| length_error("journal body length overflow"))?;
  checked_immutable_index_artifact_encoded_length(ImmutableIndexArtifactKindV1::MutationJournalSegment, 24, body_length)?;
  let mut body = vec![0u8; body_length];
  let first = request.records.first().ok_or_else(|| closure_error("journal has no first record"))?;
  let last = request.records.last().ok_or_else(|| closure_error("journal has no last record"))?;
  body[..4].copy_from_slice(&u32::from(request.chain_reset).to_le_bytes());
  body[4..6].copy_from_slice(&1u16.to_le_bytes());
  body[6..8].copy_from_slice(&request.owner_kind.id().to_le_bytes());
  body[8..16].copy_from_slice(&request.segment_ordinal.to_le_bytes());
  body[16..24].copy_from_slice(&first.sequence.to_le_bytes());
  body[24..32].copy_from_slice(&last.sequence.to_le_bytes());
  body[32..36].copy_from_slice(&(request.records.len() as u32).to_le_bytes());
  body[36..40].copy_from_slice(&records_length_u32.to_le_bytes());
  body[40..40 + hash_width].copy_from_slice(request.previous_segment);
  body[40 + hash_width..40 + 2 * hash_width].copy_from_slice(first.root_before);
  body[40 + 2 * hash_width..40 + 3 * hash_width].copy_from_slice(last.root_after);
  body[40 + 3 * hash_width..40 + 4 * hash_width].copy_from_slice(request.semantic_state_root);
  body[40 + 4 * hash_width..fixed].copy_from_slice(&request.runtime_boot_id);
  let mut offset = fixed;
  for record in request.records {
    let encoded = encode_mutation_record(request.hash_algorithm, record)?;
    let end = offset.checked_add(encoded.len()).ok_or_else(|| length_error("journal record offset overflow"))?;
    body[offset..end].copy_from_slice(&encoded);
    offset = end;
  }

  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&request.owner_id);
  identity.extend_from_slice(&request.segment_ordinal.to_le_bytes());
  let encoded = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind: ImmutableIndexArtifactKindV1::MutationJournalSegment,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  decode_mutation_journal(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

fn validate_journal_write_header(request: &MutationJournalWriteV1<'_>) -> FormatResult<()> {
  let hash_width = request.hash_algorithm.hash_length();
  validate_hash(request.previous_segment, hash_width, true, "journal previous segment")?;
  validate_hash(request.semantic_state_root, hash_width, false, "journal semantic-state root")?;
  if request.generation == 0 || request.runtime_boot_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("journal generation or runtime boot ID is zero"));
  }
  if request.chain_reset != request.previous_segment.iter().all(|byte| *byte == 0) {
    return Err(closure_error("journal reset flag disagrees with previous-segment presence"));
  }
  match request.owner_kind {
    JournalOwnerKindV1::Task if request.owner_id.iter().all(|byte| *byte == 0) || request.owner_id == SYSTEM_INDEX_JOURNAL_ID => {
      Err(identity_error("task journal owner is zero or uses the reserved system owner"))
    }
    JournalOwnerKindV1::System if request.owner_id != SYSTEM_INDEX_JOURNAL_ID => {
      Err(identity_error("system journal does not use its reserved owner ID"))
    }
    _ => Ok(()),
  }
}

fn encode_mutation_record(hash_algorithm: HashAlgorithm, request: &MutationRecordWriteV1<'_>) -> FormatResult<Vec<u8>> {
  let record_length = checked_mutation_record_length(hash_algorithm, request)?;
  let hash_width = hash_algorithm.hash_length();
  let before_path = request.before.map_or(&[][..], |side| side.path.as_bytes());
  let after_path = request.after.map_or(&[][..], |side| side.path.as_bytes());
  let fixed = 40usize.checked_add(7 * hash_width).ok_or_else(|| length_error("mutation record fixed length overflow"))?;
  let record_length_u32 = checked_task_u32(record_length, "mutation record length exceeds u32")?;
  let before_length = checked_task_u32(before_path.len(), "before path length exceeds u32")?;
  let after_length = checked_task_u32(after_path.len(), "after path length exceeds u32")?;
  let mut record = vec![0u8; record_length];
  record[..4].copy_from_slice(&record_length_u32.to_le_bytes());
  record[4..6].copy_from_slice(&request.kind.id().to_le_bytes());
  let presence = u16::from(request.before.is_some()) | (u16::from(request.after.is_some()) << 1);
  record[6..8].copy_from_slice(&presence.to_le_bytes());
  record[8..16].copy_from_slice(&request.sequence.to_le_bytes());
  record[16..20].copy_from_slice(&request.batch_ordinal.to_le_bytes());
  record[20..24].copy_from_slice(&request.batch_count.to_le_bytes());
  record[24..24 + hash_width].copy_from_slice(request.mutation_id);
  record[24 + hash_width..24 + 2 * hash_width].copy_from_slice(request.root_before);
  record[24 + 2 * hash_width..24 + 3 * hash_width].copy_from_slice(request.root_after);
  encode_mutation_side(hash_algorithm, &mut record, 24 + 3 * hash_width, request.before)?;
  encode_mutation_side(hash_algorithm, &mut record, 24 + 5 * hash_width, request.after)?;
  record[24 + 7 * hash_width..28 + 7 * hash_width].copy_from_slice(&before_length.to_le_bytes());
  record[28 + 7 * hash_width..32 + 7 * hash_width].copy_from_slice(&after_length.to_le_bytes());
  record[32 + 7 * hash_width..40 + 7 * hash_width].copy_from_slice(&request.committed_at_ms.to_le_bytes());
  record[fixed..fixed + before_path.len()].copy_from_slice(before_path);
  record[fixed + before_path.len()..].copy_from_slice(after_path);
  Ok(record)
}

fn checked_mutation_record_length(hash_algorithm: HashAlgorithm, request: &MutationRecordWriteV1<'_>) -> FormatResult<usize> {
  let hash_width = hash_algorithm.hash_length();
  validate_hash(request.mutation_id, hash_width, false, "mutation ID")?;
  validate_hash(request.root_before, hash_width, false, "mutation root before")?;
  validate_hash(request.root_after, hash_width, false, "mutation root after")?;
  if request.sequence == 0 || request.batch_count == 0 || request.batch_ordinal >= request.batch_count {
    return Err(closure_error("mutation sequence or batch coordinates are invalid"));
  }
  validate_mutation_presence(request)?;
  validate_mutation_side_write(hash_width, request.before)?;
  validate_mutation_side_write(hash_width, request.after)?;
  let before_length = request.before.map_or(0, |side| side.path.len());
  let after_length = request.after.map_or(0, |side| side.path.len());
  let _before_length_u32 = checked_task_u32(before_length, "before path length exceeds u32")?;
  let _after_length_u32 = checked_task_u32(after_length, "after path length exceeds u32")?;
  40usize
    .checked_add(7 * hash_width)
    .and_then(|length| length.checked_add(before_length))
    .and_then(|length| length.checked_add(after_length))
    .ok_or_else(|| length_error("mutation record length overflow"))
}

fn validate_mutation_presence(request: &MutationRecordWriteV1<'_>) -> FormatResult<()> {
  let valid = match request.kind {
    MutationKindV1::Create | MutationKindV1::Copy | MutationKindV1::Restore => request.before.is_none() && request.after.is_some(),
    MutationKindV1::Update => request.before.is_some() && request.after.is_some(),
    MutationKindV1::Delete => request.before.is_some() && request.after.is_none(),
    MutationKindV1::Move => {
      request.before.is_some() && request.after.is_some() && request.before.map(|side| side.path) != request.after.map(|side| side.path)
    }
    MutationKindV1::Transition => request.before.is_some() || request.after.is_some(),
  };
  if valid {
    Ok(())
  } else {
    Err(closure_error("mutation kind and before/after presence are inconsistent"))
  }
}

fn validate_mutation_side_write(hash_width: usize, side: Option<MutationSideWriteV1<'_>>) -> FormatResult<()> {
  let Some(side) = side else {
    return Ok(());
  };
  validate_canonical_absolute_path(side.path)?;
  validate_hash(side.revision, hash_width, false, "mutation revision")
}

fn encode_mutation_side(
  hash_algorithm: HashAlgorithm,
  output: &mut [u8],
  offset: usize,
  side: Option<MutationSideWriteV1<'_>>,
) -> FormatResult<()> {
  let Some(side) = side else {
    return Ok(());
  };
  let hash_width = hash_algorithm.hash_length();
  let file_key = digest_parts(hash_algorithm, &[b"file:", side.path.as_bytes()]);
  output[offset..offset + hash_width].copy_from_slice(&file_key);
  output[offset + hash_width..offset + 2 * hash_width].copy_from_slice(side.revision);
  Ok(())
}

pub fn encode_index_task_checkpoint(request: &IndexTaskCheckpointWriteV1<'_>) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  validate_checkpoint_write(request)?;
  let hash_width = request.hash_algorithm.hash_length();
  let attachment_length = 12usize.checked_add(2 * hash_width).ok_or_else(|| length_error("attachment fixed length overflow"))?;
  let attachment_bytes =
    request.attachments.len().checked_mul(attachment_length).ok_or_else(|| length_error("checkpoint attachment length overflow"))?;
  let external = match request.external {
    Some(external) => encode_external_descriptor(external)?,
    None => Vec::new(),
  };
  let fixed = 120usize.checked_add(4 * hash_width).ok_or_else(|| length_error("checkpoint fixed length overflow"))?;
  let body_length = fixed
    .checked_add(request.resume_key.len())
    .and_then(|length| length.checked_add(attachment_bytes))
    .and_then(|length| length.checked_add(external.len()))
    .ok_or_else(|| length_error("checkpoint body length overflow"))?;
  let resume_length_u32 = checked_task_u32(request.resume_key.len(), "checkpoint resume key exceeds u32")?;
  let attachment_count_u32 = checked_task_u32(request.attachments.len(), "checkpoint attachment count exceeds u32")?;
  let attachment_bytes_u32 = checked_task_u32(attachment_bytes, "checkpoint attachment bytes exceed u32")?;
  let external_length_u32 = checked_task_u32(external.len(), "checkpoint external descriptor exceeds u32")?;
  let mut body = vec![0u8; body_length];
  body[4..6].copy_from_slice(&1u16.to_le_bytes());
  body[6..8].copy_from_slice(&request.task_kind.id().to_le_bytes());
  body[8..10].copy_from_slice(&request.state.id().to_le_bytes());
  body[10..12].copy_from_slice(&request.phase.to_le_bytes());
  body[12..44].copy_from_slice(request.required_capabilities);
  body[44..52].copy_from_slice(&request.started_at_ms.to_le_bytes());
  body[52..60].copy_from_slice(&request.updated_at_ms.to_le_bytes());
  body[60..60 + hash_width].copy_from_slice(request.source_root);
  write_optional_hash(&mut body[60 + hash_width..60 + 2 * hash_width], request.target_root);
  write_optional_hash(&mut body[60 + 2 * hash_width..60 + 3 * hash_width], request.primary_id);
  write_optional_hash(&mut body[60 + 3 * hash_width..60 + 4 * hash_width], request.journal_head);
  body[60 + 4 * hash_width..68 + 4 * hash_width].copy_from_slice(&request.journal_floor_sequence.to_le_bytes());
  body[68 + 4 * hash_width..76 + 4 * hash_width].copy_from_slice(&request.journal_audited_through.to_le_bytes());
  body[76 + 4 * hash_width..84 + 4 * hash_width].copy_from_slice(&request.next_document_ordinal.to_le_bytes());
  body[84 + 4 * hash_width..92 + 4 * hash_width].copy_from_slice(&request.completed_work.to_le_bytes());
  body[92 + 4 * hash_width..100 + 4 * hash_width].copy_from_slice(&request.total_work_hint.to_le_bytes());
  body[100 + 4 * hash_width..104 + 4 * hash_width].copy_from_slice(&resume_length_u32.to_le_bytes());
  body[104 + 4 * hash_width..108 + 4 * hash_width].copy_from_slice(&attachment_count_u32.to_le_bytes());
  body[108 + 4 * hash_width..112 + 4 * hash_width].copy_from_slice(&attachment_bytes_u32.to_le_bytes());
  body[112 + 4 * hash_width..116 + 4 * hash_width].copy_from_slice(&external_length_u32.to_le_bytes());
  body[fixed..fixed + request.resume_key.len()].copy_from_slice(request.resume_key);
  let mut offset = fixed + request.resume_key.len();
  for attachment in request.attachments {
    body[offset..offset + 2].copy_from_slice(&attachment.role.id().to_le_bytes());
    body[offset + 4..offset + 4 + hash_width].copy_from_slice(attachment.owner_id);
    body[offset + 4 + hash_width..offset + 4 + 2 * hash_width].copy_from_slice(attachment.artifact_hash);
    body[offset + 4 + 2 * hash_width..offset + 12 + 2 * hash_width].copy_from_slice(&attachment.birth_generation.to_le_bytes());
    offset += attachment_length;
  }
  body[offset..].copy_from_slice(&external);

  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&request.task_id);
  identity.extend_from_slice(&request.checkpoint_sequence.to_le_bytes());
  let encoded = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind: ImmutableIndexArtifactKindV1::IndexTaskCheckpoint,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  decode_index_task_checkpoint(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

fn validate_checkpoint_write(request: &IndexTaskCheckpointWriteV1<'_>) -> FormatResult<()> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.task_id.iter().all(|byte| *byte == 0) || request.checkpoint_sequence == 0 || request.generation == 0 {
    return Err(identity_error("checkpoint TaskId, sequence, or generation is zero"));
  }
  if request.task_kind.phase_name(request.phase).is_none() {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "index_checkpoint_phase", "checkpoint phase is unknown for task kind"));
  }
  if request.required_capabilities[3..].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::UnknownRequiredCapability,
      "index_checkpoint_capabilities",
      "checkpoint requires an unknown capability bit",
    ));
  }
  validate_hash(request.source_root, hash_width, false, "checkpoint source root")?;
  validate_optional_hash(request.target_root, hash_width, "checkpoint target root")?;
  validate_optional_hash(request.primary_id, hash_width, "checkpoint primary ID")?;
  validate_optional_hash(request.journal_head, hash_width, "checkpoint journal head")?;
  if request.updated_at_ms < request.started_at_ms
    || (request.journal_head.is_none() && (request.journal_floor_sequence != 0 || request.journal_audited_through != 0))
    || (request.journal_head.is_some()
      && (request.journal_audited_through == 0 || request.journal_floor_sequence > request.journal_audited_through))
    || (request.total_work_hint != 0 && request.completed_work > request.total_work_hint)
  {
    return Err(closure_error("checkpoint time, journal, or progress semantics are invalid"));
  }
  if request.resume_key.len() > MAX_RESUME_KEY_LENGTH || request.attachments.len() > MAX_ATTACHMENTS as usize {
    return Err(amplification_error("checkpoint component exceeds its hard cap"));
  }
  let mut previous = None;
  for attachment in request.attachments {
    validate_hash(attachment.owner_id, hash_width, false, "checkpoint attachment owner")?;
    validate_hash(attachment.artifact_hash, hash_width, false, "checkpoint attachment hash")?;
    if attachment.birth_generation == 0 {
      return Err(identity_error("checkpoint attachment generation is zero"));
    }
    let key = (attachment.role, attachment.owner_id, attachment.artifact_hash);
    if previous.is_some_and(|previous| previous >= key) {
      return Err(order_error("checkpoint attachments are not strictly ordered"));
    }
    previous = Some(key);
  }
  if let Some(external) = request.external {
    let _encoded = encode_external_descriptor(external)?;
  }
  Ok(())
}

fn encode_external_descriptor(request: ExternalWorkspaceDescriptorWriteV1<'_>) -> FormatResult<Vec<u8>> {
  validate_canonical_absolute_path(request.path)?;
  if request.workspace_id.iter().all(|byte| *byte == 0) || request.manifest_digest.iter().all(|byte| *byte == 0) {
    return Err(identity_error("external workspace identity or manifest digest is zero"));
  }
  let path = request.path.as_bytes();
  let length = 68usize.checked_add(path.len()).ok_or_else(|| length_error("external descriptor length overflow"))?;
  if path.is_empty() || length > MAX_EXTERNAL_DESCRIPTOR_LENGTH {
    return Err(amplification_error("external workspace descriptor exceeds its hard cap"));
  }
  let path_length = checked_task_u32(path.len(), "external path exceeds u32")?;
  let mut encoded = vec![0u8; length];
  encoded[..16].copy_from_slice(&request.workspace_id);
  encoded[16..20].copy_from_slice(&path_length.to_le_bytes());
  encoded[20..52].copy_from_slice(&request.manifest_digest);
  encoded[52..60].copy_from_slice(&request.durable_sequence.to_le_bytes());
  encoded[60..68].copy_from_slice(&request.durable_bytes.to_le_bytes());
  encoded[68..].copy_from_slice(path);
  Ok(encoded)
}

fn validate_hash(bytes: &[u8], hash_width: usize, allow_zero: bool, context: &'static str) -> FormatResult<()> {
  if bytes.len() != hash_width || (!allow_zero && bytes.iter().all(|byte| *byte == 0)) {
    return Err(identity_error(format!("{context} has the wrong width or is all zero")));
  }
  Ok(())
}

fn validate_optional_hash(bytes: Option<&[u8]>, hash_width: usize, context: &'static str) -> FormatResult<()> {
  match bytes {
    Some(bytes) => validate_hash(bytes, hash_width, false, context),
    None => Ok(()),
  }
}

fn write_optional_hash(output: &mut [u8], value: Option<&[u8]>) {
  if let Some(value) = value {
    output.copy_from_slice(value);
  }
}

fn checked_task_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  if value > u32::MAX as usize {
    return Err(length_error(context));
  }
  Ok(value as u32)
}

pub fn decode_index_task_artifact(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<IndexTaskArtifactV1<'_>> {
  if value.len() > MAX_JOURNAL_LENGTH {
    return Err(amplification_error("index task artifact exceeds the 16 MiB family cap"));
  }
  match u16_at(value, 6)? {
    MUTATION_JOURNAL_KIND => decode_mutation_journal(value, hash_algorithm).map(IndexTaskArtifactV1::Journal),
    INDEX_TASK_CHECKPOINT_KIND => decode_index_task_checkpoint(value, hash_algorithm).map(IndexTaskArtifactV1::Checkpoint),
    kind => Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "index_task_artifact_kind",
      format!("unsupported index-task artifact kind 0x{kind:04x}"),
    )),
  }
}

pub fn decode_mutation_journal(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<MutationJournalV1<'_>> {
  let artifact = decode_immutable_index_artifact(value, hash_algorithm, MAX_JOURNAL_LENGTH)?;
  if artifact.kind != MUTATION_JOURNAL_KIND || artifact.identity.len() != 24 {
    return Err(closure_error("mutation-journal kind or identity length is invalid"));
  }
  let owner_id: [u8; 16] = artifact.identity[..16].try_into().expect("validated journal owner width");
  let segment_ordinal = u64_at(artifact.identity, 16)?;
  let hash_width = hash_algorithm.hash_length();
  let fixed = 56usize.checked_add(4 * hash_width).ok_or_else(|| length_error("journal fixed length overflow"))?;
  let body = artifact.body;
  if body.len() < fixed {
    return Err(truncated_error("mutation-journal body is truncated"));
  }
  let flags = u32_at(body, 0)?;
  if flags & !1 != 0 {
    return Err(reserve_error("mutation-journal flags contain unknown bits"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "mutation_journal_version", "journal body version is not 1"));
  }
  let owner_kind = JournalOwnerKindV1::from_id(u16_at(body, 6)?)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "mutation_journal_owner_kind", "journal owner kind is unknown"))?;
  if u64_at(body, 8)? != segment_ordinal {
    return Err(identity_error("journal identity and body segment ordinals disagree"));
  }
  let first_sequence = u64_at(body, 16)?;
  let last_sequence = u64_at(body, 24)?;
  let record_count = u32_at(body, 32)?;
  let records_length = usize::try_from(u32_at(body, 36)?).map_err(|_| length_error("journal record length conversion"))?;
  if record_count == 0 || record_count > MAX_JOURNAL_RECORDS {
    return Err(amplification_error("journal record count is outside 1..=10000"));
  }
  if fixed.checked_add(records_length) != Some(body.len()) {
    return Err(truncated_error("journal record bytes do not consume the body"));
  }
  let previous_segment = &body[40..40 + hash_width];
  let source_root_before = &body[40 + hash_width..40 + 2 * hash_width];
  let source_root_after = &body[40 + 2 * hash_width..40 + 3 * hash_width];
  let semantic_state_root = &body[40 + 3 * hash_width..40 + 4 * hash_width];
  let runtime_boot_id: [u8; 16] = body[40 + 4 * hash_width..fixed].try_into().expect("validated runtime boot ID width");
  let chain_reset = flags & 1 != 0;
  if chain_reset != previous_segment.iter().all(|byte| *byte == 0) {
    return Err(closure_error("journal reset flag disagrees with previous-segment presence"));
  }
  if source_root_before.iter().all(|byte| *byte == 0)
    || source_root_after.iter().all(|byte| *byte == 0)
    || semantic_state_root.iter().all(|byte| *byte == 0)
    || runtime_boot_id.iter().all(|byte| *byte == 0)
  {
    return Err(identity_error("journal required root or runtime identity is all zero"));
  }
  match owner_kind {
    JournalOwnerKindV1::Task if owner_id.iter().all(|byte| *byte == 0) || owner_id == SYSTEM_INDEX_JOURNAL_ID => {
      return Err(identity_error("task journal owner is zero or uses the reserved system owner"));
    }
    JournalOwnerKindV1::System if owner_id != SYSTEM_INDEX_JOURNAL_ID => {
      return Err(identity_error("system journal does not use its reserved owner ID"));
    }
    _ => {}
  }

  let record_bytes = &body[fixed..];
  let scan = scan_mutation_records(hash_algorithm, record_bytes, record_count)?;
  if scan.first.sequence != first_sequence
    || scan.last.sequence != last_sequence
    || scan.first.root_before != source_root_before
    || scan.last.root_after != source_root_after
  {
    return Err(closure_error("journal record boundaries disagree with header sequences or roots"));
  }
  Ok(MutationJournalV1 {
    owner_id,
    owner_kind,
    generation: artifact.generation,
    segment_ordinal,
    chain_reset,
    previous_segment,
    source_root_before,
    source_root_after,
    semantic_state_root,
    runtime_boot_id,
    first_sequence,
    last_sequence,
    records: MutationRecordsV1 { hash_algorithm, bytes: record_bytes, count: record_count },
    key: artifact.key,
  })
}

struct MutationRecordScanV1<'a> {
  first: MutationRecordV1<'a>,
  last: MutationRecordV1<'a>,
}

fn scan_mutation_records(hash_algorithm: HashAlgorithm, bytes: &[u8], count: u32) -> FormatResult<MutationRecordScanV1<'_>> {
  let mut offset = 0usize;
  let mut first = None;
  let mut previous: Option<MutationRecordV1<'_>> = None;
  let mut batch_anchor: Option<MutationRecordV1<'_>> = None;
  let mut expected_batch_ordinal = 0u32;
  for _ in 0..count {
    let (record, next) = decode_mutation_record(hash_algorithm, bytes, offset)?;
    if let Some(previous) = &previous {
      if compare_mutation_records(previous, &record) != Ordering::Less {
        return Err(order_error("journal records are not strictly ordered by sequence, mutation ID, and batch ordinal"));
      }
    }
    if record.batch_ordinal == 0 {
      if let Some(anchor) = &batch_anchor {
        if expected_batch_ordinal != anchor.batch_count {
          return Err(closure_error("journal batch ended before all members were present"));
        }
      }
      batch_anchor = Some(record.clone());
      expected_batch_ordinal = 0;
    }
    let anchor = batch_anchor.as_ref().ok_or_else(|| closure_error("journal batch does not begin at ordinal zero"))?;
    if record.sequence != anchor.sequence
      || record.mutation_id != anchor.mutation_id
      || record.root_before != anchor.root_before
      || record.root_after != anchor.root_after
      || record.batch_count != anchor.batch_count
      || record.batch_ordinal != expected_batch_ordinal
      || record.committed_at_ms != anchor.committed_at_ms
    {
      return Err(closure_error("journal batch members disagree"));
    }
    expected_batch_ordinal = expected_batch_ordinal.checked_add(1).ok_or_else(|| length_error("journal batch ordinal overflow"))?;
    if first.is_none() {
      first = Some(record.clone());
    }
    previous = Some(record);
    offset = next;
  }
  let anchor = batch_anchor.as_ref().ok_or_else(|| closure_error("journal has no batch"))?;
  if expected_batch_ordinal != anchor.batch_count {
    return Err(closure_error("journal final batch is incomplete"));
  }
  if offset != bytes.len() {
    return Err(truncated_error("journal record count does not consume the record area"));
  }
  Ok(MutationRecordScanV1 {
    first: first.ok_or_else(|| closure_error("journal has no first record"))?,
    last: previous.ok_or_else(|| closure_error("journal has no last record"))?,
  })
}

fn compare_mutation_records(left: &MutationRecordV1<'_>, right: &MutationRecordV1<'_>) -> Ordering {
  left
    .sequence
    .cmp(&right.sequence)
    .then_with(|| left.mutation_id.cmp(right.mutation_id))
    .then_with(|| left.batch_ordinal.cmp(&right.batch_ordinal))
}

fn decode_mutation_record(hash_algorithm: HashAlgorithm, bytes: &[u8], start: usize) -> FormatResult<(MutationRecordV1<'_>, usize)> {
  let hash_width = hash_algorithm.hash_length();
  let fixed = 40usize.checked_add(7 * hash_width).ok_or_else(|| length_error("mutation record fixed length overflow"))?;
  let record_length = usize::try_from(u32_at(bytes, start)?).map_err(|_| length_error("mutation record length conversion"))?;
  if record_length < fixed {
    return Err(truncated_error("mutation record is shorter than its fixed fields"));
  }
  let end = start.checked_add(record_length).ok_or_else(|| length_error("mutation record end overflow"))?;
  let record = bytes.get(start..end).ok_or_else(|| truncated_error("mutation record is truncated"))?;
  let kind = MutationKindV1::from_id(u16_at(record, 4)?)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "mutation_record_kind", "mutation kind is unknown"))?;
  let presence = u16_at(record, 6)?;
  if presence & !3 != 0 {
    return Err(reserve_error("mutation record presence contains unknown bits"));
  }
  let sequence = u64_at(record, 8)?;
  let batch_ordinal = u32_at(record, 16)?;
  let batch_count = u32_at(record, 20)?;
  if sequence == 0 || batch_count == 0 || batch_ordinal >= batch_count {
    return Err(closure_error("mutation record sequence or batch coordinates are invalid"));
  }
  let before_length = usize::try_from(u32_at(record, 24 + 7 * hash_width)?).map_err(|_| length_error("before-path length conversion"))?;
  let after_length = usize::try_from(u32_at(record, 28 + 7 * hash_width)?).map_err(|_| length_error("after-path length conversion"))?;
  if fixed.checked_add(before_length).and_then(|length| length.checked_add(after_length)) != Some(record.len()) {
    return Err(truncated_error("mutation path lengths do not consume the record"));
  }
  let mutation_id = &record[24..24 + hash_width];
  let root_before = &record[24 + hash_width..24 + 2 * hash_width];
  let root_after = &record[24 + 2 * hash_width..24 + 3 * hash_width];
  if mutation_id.iter().all(|byte| *byte == 0) || root_before.iter().all(|byte| *byte == 0) || root_after.iter().all(|byte| *byte == 0) {
    return Err(identity_error("mutation ID or source roots are all zero"));
  }
  let before_key = &record[24 + 3 * hash_width..24 + 4 * hash_width];
  let before_revision = &record[24 + 4 * hash_width..24 + 5 * hash_width];
  let after_key = &record[24 + 5 * hash_width..24 + 6 * hash_width];
  let after_revision = &record[24 + 6 * hash_width..24 + 7 * hash_width];
  let committed_at_ms = u64_at(record, 32 + 7 * hash_width)?;
  let before_bytes = &record[fixed..fixed + before_length];
  let after_bytes = &record[fixed + before_length..];
  let before = decode_mutation_side(hash_algorithm, presence & 1 != 0, before_bytes, before_key, before_revision)?;
  let after = decode_mutation_side(hash_algorithm, presence & 2 != 0, after_bytes, after_key, after_revision)?;
  match kind {
    MutationKindV1::Create | MutationKindV1::Copy | MutationKindV1::Restore if presence != 2 => {
      return Err(closure_error("create/copy/restore mutations require only an after side"));
    }
    MutationKindV1::Update if presence != 3 => return Err(closure_error("update mutations require before and after sides")),
    MutationKindV1::Delete if presence != 1 => return Err(closure_error("delete mutations require only a before side")),
    MutationKindV1::Move if presence != 3 || before.0 == after.0 => {
      return Err(closure_error("move mutations require distinct before and after paths"));
    }
    MutationKindV1::Transition if presence == 0 => return Err(closure_error("transition mutations require at least one side")),
    _ => {}
  }
  Ok((
    MutationRecordV1 {
      kind,
      sequence,
      mutation_id,
      batch_ordinal,
      batch_count,
      root_before,
      root_after,
      before_path: before.0,
      before_file_key: before.1,
      before_revision: before.2,
      after_path: after.0,
      after_file_key: after.1,
      after_revision: after.2,
      committed_at_ms,
      encoded: record,
    },
    end,
  ))
}

type MutationSideV1<'a> = (Option<&'a str>, Option<&'a [u8]>, Option<&'a [u8]>);

fn decode_mutation_side<'a>(
  hash_algorithm: HashAlgorithm,
  present: bool,
  path_bytes: &'a [u8],
  file_key: &'a [u8],
  revision: &'a [u8],
) -> FormatResult<MutationSideV1<'a>> {
  if !present {
    if !path_bytes.is_empty() || file_key.iter().any(|byte| *byte != 0) || revision.iter().any(|byte| *byte != 0) {
      return Err(closure_error("absent mutation side contains path or identity bytes"));
    }
    return Ok((None, None, None));
  }
  let path = std::str::from_utf8(path_bytes)
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "mutation_path_utf8", source.to_string()))?;
  validate_canonical_absolute_path(path)?;
  if digest_parts(hash_algorithm, &[b"file:", path.as_bytes()]) != file_key || revision.iter().all(|byte| *byte == 0) {
    return Err(identity_error("mutation side FileKey or revision identity is invalid"));
  }
  Ok((Some(path), Some(file_key), Some(revision)))
}

pub fn validate_journal_chain(previous: &MutationJournalV1<'_>, next: &MutationJournalV1<'_>) -> FormatResult<()> {
  if previous.owner_id != next.owner_id
    || previous.owner_kind != next.owner_kind
    || previous.generation != next.generation
    || previous.segment_ordinal.checked_add(1) != Some(next.segment_ordinal)
    || next.chain_reset
    || next.previous_segment != previous.key
    || previous.source_root_after != next.source_root_before
    || previous.last_sequence >= next.first_sequence
  {
    return Err(closure_error("mutation-journal chain continuity is invalid"));
  }
  Ok(())
}

pub fn decode_index_task_checkpoint(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<IndexTaskCheckpointV1<'_>> {
  let artifact = decode_immutable_index_artifact(value, hash_algorithm, MAX_CHECKPOINT_LENGTH)?;
  if artifact.kind != INDEX_TASK_CHECKPOINT_KIND || artifact.identity.len() != 24 || artifact.identity[..16].iter().all(|byte| *byte == 0) {
    return Err(identity_error("checkpoint kind, TaskId, or identity length is invalid"));
  }
  let task_id: [u8; 16] = artifact.identity[..16].try_into().expect("validated TaskId width");
  let checkpoint_sequence = u64_at(artifact.identity, 16)?;
  if checkpoint_sequence == 0 {
    return Err(identity_error("checkpoint sequence is zero"));
  }
  let hash_width = hash_algorithm.hash_length();
  let fixed = 120usize.checked_add(4 * hash_width).ok_or_else(|| length_error("checkpoint fixed length overflow"))?;
  let body = artifact.body;
  if body.len() < fixed {
    return Err(truncated_error("checkpoint body is truncated"));
  }
  if u32_at(body, 0)? != 0 || u32_at(body, 116 + 4 * hash_width)? != 0 {
    return Err(reserve_error("checkpoint flags or reserve are nonzero"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "index_checkpoint_version", "checkpoint body version is not 1"));
  }
  let task_kind = IndexTaskKindV1::from_id(u16_at(body, 6)?)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "index_checkpoint_task_kind", "checkpoint task kind is unknown"))?;
  let state = IndexTaskStateV1::from_id(u16_at(body, 8)?)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "index_checkpoint_state", "checkpoint state is unknown"))?;
  let phase = u16_at(body, 10)?;
  let phase_name = task_kind.phase_name(phase).ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "index_checkpoint_phase", "checkpoint phase is unknown for task kind")
  })?;
  let required_capabilities = &body[12..44];
  if required_capabilities[3..].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::UnknownRequiredCapability,
      "index_checkpoint_capabilities",
      "checkpoint requires an unknown capability bit",
    ));
  }
  let started_at_ms = u64_at(body, 44)?;
  let updated_at_ms = u64_at(body, 52)?;
  let source_root = &body[60..60 + hash_width];
  let target_root = &body[60 + hash_width..60 + 2 * hash_width];
  let primary_id = &body[60 + 2 * hash_width..60 + 3 * hash_width];
  let journal_head = &body[60 + 3 * hash_width..60 + 4 * hash_width];
  let journal_floor_sequence = u64_at(body, 60 + 4 * hash_width)?;
  let journal_audited_through = u64_at(body, 68 + 4 * hash_width)?;
  let next_document_ordinal = u64_at(body, 76 + 4 * hash_width)?;
  let completed_work = u64_at(body, 84 + 4 * hash_width)?;
  let total_work_hint = u64_at(body, 92 + 4 * hash_width)?;
  let resume_length = usize::try_from(u32_at(body, 100 + 4 * hash_width)?).map_err(|_| length_error("resume-key length conversion"))?;
  let attachment_count = u32_at(body, 104 + 4 * hash_width)?;
  let attachment_bytes = usize::try_from(u32_at(body, 108 + 4 * hash_width)?).map_err(|_| length_error("attachment length conversion"))?;
  let external_length = usize::try_from(u32_at(body, 112 + 4 * hash_width)?).map_err(|_| length_error("external length conversion"))?;
  if updated_at_ms < started_at_ms
    || source_root.iter().all(|byte| *byte == 0)
    || (journal_head.iter().all(|byte| *byte == 0) && (journal_floor_sequence != 0 || journal_audited_through != 0))
    || (journal_head.iter().any(|byte| *byte != 0) && (journal_audited_through == 0 || journal_floor_sequence > journal_audited_through))
    || (total_work_hint != 0 && completed_work > total_work_hint)
  {
    return Err(closure_error("checkpoint time, root, journal, or progress semantics are invalid"));
  }
  if resume_length > MAX_RESUME_KEY_LENGTH || attachment_count > MAX_ATTACHMENTS || external_length > MAX_EXTERNAL_DESCRIPTOR_LENGTH {
    return Err(amplification_error("checkpoint component exceeds its hard cap"));
  }
  let attachment_length = 12usize.checked_add(2 * hash_width).ok_or_else(|| length_error("attachment fixed length overflow"))?;
  let expected_attachment_bytes = usize::try_from(attachment_count)
    .ok()
    .and_then(|count| count.checked_mul(attachment_length))
    .ok_or_else(|| length_error("attachment-count multiplication overflow"))?;
  if expected_attachment_bytes != attachment_bytes {
    return Err(closure_error("checkpoint attachment count disagrees with attachment bytes"));
  }
  let expected_length = fixed
    .checked_add(resume_length)
    .and_then(|length| length.checked_add(attachment_bytes))
    .and_then(|length| length.checked_add(external_length))
    .ok_or_else(|| length_error("checkpoint body length overflow"))?;
  if expected_length != body.len() {
    return Err(truncated_error("checkpoint components do not consume the body"));
  }
  let resume_end = fixed + resume_length;
  let attachments_end = resume_end + attachment_bytes;
  let resume_key = &body[fixed..resume_end];
  let attachments = IndexTaskAttachmentsV1 { hash_width, bytes: &body[resume_end..attachments_end], count: attachment_count };
  validate_attachments(&attachments)?;
  let external = if external_length == 0 { None } else { Some(decode_external_descriptor(&body[attachments_end..])?) };
  Ok(IndexTaskCheckpointV1 {
    task_id,
    checkpoint_sequence,
    generation: artifact.generation,
    task_kind,
    state,
    phase,
    phase_name,
    required_capabilities,
    started_at_ms,
    updated_at_ms,
    source_root,
    target_root,
    primary_id,
    journal_head,
    journal_floor_sequence,
    journal_audited_through,
    next_document_ordinal,
    completed_work,
    total_work_hint,
    resume_key,
    attachments,
    external,
    key: artifact.key,
  })
}

fn validate_attachments(attachments: &IndexTaskAttachmentsV1<'_>) -> FormatResult<()> {
  let mut previous: Option<IndexTaskAttachmentV1<'_>> = None;
  for attachment in attachments.iter() {
    let attachment = attachment?;
    if let Some(previous) = previous {
      if (previous.role, previous.owner_id, previous.artifact_hash).cmp(&(attachment.role, attachment.owner_id, attachment.artifact_hash))
        != Ordering::Less
      {
        return Err(order_error("checkpoint attachments are not strictly ordered"));
      }
    }
    previous = Some(attachment);
  }
  Ok(())
}

fn decode_attachment(hash_width: usize, bytes: &[u8], index: usize) -> FormatResult<IndexTaskAttachmentV1<'_>> {
  let length = 12usize.checked_add(2 * hash_width).ok_or_else(|| length_error("attachment length overflow"))?;
  let start = index.checked_mul(length).ok_or_else(|| length_error("attachment offset overflow"))?;
  let record = bytes.get(start..start + length).ok_or_else(|| truncated_error("checkpoint attachment is truncated"))?;
  let role = IndexTaskAttachmentRoleV1::from_id(u16_at(record, 0)?)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "index_checkpoint_attachment_role", "attachment role is unknown"))?;
  if u16_at(record, 2)? != 0 {
    return Err(reserve_error("checkpoint attachment flags are nonzero"));
  }
  let owner_id = &record[4..4 + hash_width];
  let artifact_hash = &record[4 + hash_width..4 + 2 * hash_width];
  let birth_generation = u64_at(record, 4 + 2 * hash_width)?;
  if owner_id.iter().all(|byte| *byte == 0) || artifact_hash.iter().all(|byte| *byte == 0) || birth_generation == 0 {
    return Err(identity_error("checkpoint attachment identity or generation is zero"));
  }
  Ok(IndexTaskAttachmentV1 { role, owner_id, artifact_hash, birth_generation })
}

fn decode_external_descriptor(bytes: &[u8]) -> FormatResult<ExternalWorkspaceDescriptorV1<'_>> {
  if bytes.len() < 69 || bytes.len() > MAX_EXTERNAL_DESCRIPTOR_LENGTH {
    return Err(amplification_error("external workspace descriptor length is outside 69..=65536"));
  }
  let workspace_id: [u8; 16] = bytes[..16].try_into().expect("validated workspace ID width");
  let path_length = usize::try_from(u32_at(bytes, 16)?).map_err(|_| length_error("external path length conversion"))?;
  let manifest_digest: [u8; 32] = bytes[20..52].try_into().expect("validated manifest digest width");
  if workspace_id.iter().all(|byte| *byte == 0)
    || manifest_digest.iter().all(|byte| *byte == 0)
    || path_length == 0
    || 68usize.checked_add(path_length) != Some(bytes.len())
  {
    return Err(identity_error("external workspace metadata or path length is invalid"));
  }
  let path = std::str::from_utf8(&bytes[68..])
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "index_checkpoint_external_utf8", source.to_string()))?;
  validate_canonical_absolute_path(path)?;
  Ok(ExternalWorkspaceDescriptorV1 {
    workspace_id,
    manifest_digest,
    durable_sequence: u64_at(bytes, 52)?,
    durable_bytes: u64_at(bytes, 60)?,
    path,
  })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexTaskAttachmentClosureV1 {
  checkpoint_hash: Vec<u8>,
  rooted_artifact_count: u32,
  journal_head_validated: bool,
}

impl IndexTaskAttachmentClosureV1 {
  pub fn checkpoint_hash(&self) -> &[u8] {
    &self.checkpoint_hash
  }

  pub fn rooted_artifact_count(&self) -> u32 {
    self.rooted_artifact_count
  }

  pub fn journal_head_validated(&self) -> bool {
    self.journal_head_validated
  }
}

#[derive(Debug)]
pub struct IndexTaskAttachmentClosureBuilderV1<'checkpoint, 'artifact> {
  checkpoint: &'checkpoint IndexTaskCheckpointV1<'artifact>,
  hash_algorithm: HashAlgorithm,
  next_attachment: usize,
  journal_head_validated: bool,
  failed: bool,
}

impl<'checkpoint, 'artifact> IndexTaskAttachmentClosureBuilderV1<'checkpoint, 'artifact> {
  pub fn new(checkpoint: &'checkpoint IndexTaskCheckpointV1<'artifact>, hash_algorithm: HashAlgorithm) -> FormatResult<Self> {
    let hash_width = hash_algorithm.hash_length();
    if checkpoint.key.len() != hash_width || checkpoint.source_root.len() != hash_width || checkpoint.attachments.hash_width != hash_width {
      return Err(identity_error("checkpoint attachment closure uses a different hash profile"));
    }
    Ok(Self { checkpoint, hash_algorithm, next_attachment: 0, journal_head_validated: false, failed: false })
  }

  pub fn observe_encoded(&mut self, value: &[u8]) -> FormatResult<()> {
    if self.failed {
      return Err(closure_error("checkpoint attachment closure is already failed"));
    }
    match self.observe_encoded_inner(value) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> FormatResult<IndexTaskAttachmentClosureV1> {
    let expects_journal = self.checkpoint.journal_head.iter().any(|byte| *byte != 0);
    if self.failed || self.next_attachment != self.checkpoint.attachments.len() || expects_journal != self.journal_head_validated {
      return Err(closure_error("checkpoint attachment closure is incomplete or failed"));
    }
    Ok(IndexTaskAttachmentClosureV1 {
      checkpoint_hash: self.checkpoint.key.clone(),
      rooted_artifact_count: self.next_attachment as u32,
      journal_head_validated: self.journal_head_validated,
    })
  }

  fn observe_encoded_inner(&mut self, value: &[u8]) -> FormatResult<()> {
    if self.next_attachment >= self.checkpoint.attachments.len() {
      return Err(closure_error("checkpoint attachment closure received more artifacts than the checkpoint declares"));
    }
    let attachment = self.checkpoint.attachments.entry_at(self.next_attachment)?;
    match attachment.role {
      IndexTaskAttachmentRoleV1::MutationJournalHead => {
        let journal = decode_mutation_journal(value, self.hash_algorithm)?;
        validate_attachment_identity(&attachment, &journal.key, journal.generation)?;
        if self.checkpoint.journal_head != journal.key {
          return Err(closure_error("journal attachment does not name the checkpoint journal head"));
        }
        if journal.owner_kind == JournalOwnerKindV1::Task && journal.owner_id != self.checkpoint.task_id {
          return Err(closure_error("task-owned journal attachment belongs to another task"));
        }
        self.journal_head_validated = true;
      }
      role if role.directory_role().is_some() => {
        let directory = decode_artifact_directory(value, self.hash_algorithm)?;
        validate_attachment_identity(&attachment, &directory.key, directory.generation)?;
        if directory.owner_id != attachment.owner_id || Some(directory.role) != role.directory_role() {
          return Err(closure_error("directory attachment owner or role disagrees with its artifact"));
        }
      }
      role if role.manifest_kind().is_some() => {
        let manifest = decode_index_manifest(value, self.hash_algorithm)?;
        validate_attachment_identity(&attachment, &manifest.key, manifest.generation)?;
        if manifest.owner_id != attachment.owner_id || Some(manifest.kind) != role.manifest_kind() {
          return Err(closure_error("manifest attachment owner or kind disagrees with its artifact"));
        }
      }
      _ => return Err(closure_error("checkpoint attachment role has no registered immutable artifact decoder")),
    }
    self.next_attachment += 1;
    Ok(())
  }
}

fn validate_attachment_identity(attachment: &IndexTaskAttachmentV1<'_>, artifact_hash: &[u8], generation: u64) -> FormatResult<()> {
  if attachment.artifact_hash != artifact_hash || attachment.birth_generation != generation {
    return Err(closure_error("checkpoint attachment hash or birth generation disagrees with its artifact"));
  }
  Ok(())
}

fn truncated_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "index_task_length", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "index_task_arithmetic", context)
}

fn amplification_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, "index_task_bound", context)
}

fn reserve_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, "index_task_reserved", context)
}

fn identity_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "index_task_identity", context)
}

fn order_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "index_task_order", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "index_task_closure", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
