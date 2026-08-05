use std::cmp::Ordering;

use crate::engine::HashAlgorithm;

use super::hash::digest_parts;
use super::index_artifact::{decode_immutable_index_artifact, u16_at, u32_at, u64_at};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTaskAttachmentV1<'a> {
  pub role: u16,
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
  let role = u16_at(record, 0)?;
  if !(1..=12).contains(&role) {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "index_checkpoint_attachment_role", "attachment role is unknown"));
  }
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
