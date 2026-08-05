use std::cmp::Ordering;

use crate::core::HashProfile;
use crate::definitions;
use crate::index::{
  build_immutable_value, decode_immutable_value, fill_sequence, put_u16, put_u32, put_u64, read_u16, read_u32, read_u64, IndexFixtureCase,
  IndexFormat,
};

const MUTATION_JOURNAL_KIND: u16 = 0x0040;
const INDEX_TASK_CHECKPOINT_KIND: u16 = 0x0041;
const MAX_JOURNAL_LENGTH: usize = 16 * 1_024 * 1_024;
const MAX_CHECKPOINT_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_JOURNAL_RECORDS: usize = 10_000;
const MAX_RESUME_KEY_LENGTH: usize = 1_024 * 1_024;
const MAX_ATTACHMENTS: usize = 4_096;
const MAX_EXTERNAL_DESCRIPTOR_LENGTH: usize = 64 * 1_024;
const SYSTEM_INDEX_JOURNAL_ID: [u8; 16] = *b"AEORIDXJOURNALV1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JournalOwnerKind {
  Task = 1,
  System = 2,
}

impl JournalOwnerKind {
  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::Task),
      2 => Some(Self::System),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::Task => "task",
      Self::System => "system",
    }
  }
}

#[derive(Clone)]
struct MutationRecordSpec<'a> {
  kind: u16,
  sequence: u64,
  mutation_id: Vec<u8>,
  batch_ordinal: u32,
  batch_count: u32,
  root_before: Vec<u8>,
  root_after: Vec<u8>,
  before_path: Option<&'a str>,
  before_revision: Vec<u8>,
  after_path: Option<&'a str>,
  after_revision: Vec<u8>,
  committed_at_ms: u64,
}

struct JournalSpec<'a> {
  owner_id: [u8; 16],
  owner_kind: JournalOwnerKind,
  generation: u64,
  segment_ordinal: u64,
  chain_reset: bool,
  previous_segment: Vec<u8>,
  semantic_state_root: Vec<u8>,
  runtime_boot_id: [u8; 16],
  records: &'a [MutationRecordSpec<'a>],
}

#[derive(Debug)]
struct DecodedMutationRecord {
  sequence: u64,
  mutation_id: Vec<u8>,
  batch_ordinal: u32,
  batch_count: u32,
  root_before: Vec<u8>,
  root_after: Vec<u8>,
  committed_at_ms: u64,
}

#[derive(Debug)]
struct DecodedJournal {
  owner_kind: JournalOwnerKind,
  #[cfg(test)]
  owner_id: [u8; 16],
  generation: u64,
  segment_ordinal: u64,
  chain_reset: bool,
  #[cfg(test)]
  previous_segment: Vec<u8>,
  #[cfg(test)]
  source_root_before: Vec<u8>,
  #[cfg(test)]
  source_root_after: Vec<u8>,
  record_count: u32,
  first_sequence: u64,
  last_sequence: u64,
  key: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskKind {
  ScopeBuild = 1,
  ValueBuild = 2,
  FieldBuild = 3,
  NvtBuild = 4,
  Reconcile = 5,
  V0Migration = 6,
  Compaction = 7,
  IndexRepair = 8,
}

impl TaskKind {
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

  fn name(self) -> &'static str {
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

  fn phase_name(self, phase: u16) -> Option<&'static str> {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskState {
  Running = 1,
  CancelRequested = 2,
  Canceled = 3,
  FailedRetryable = 4,
  FailedTerminal = 5,
  CompleteUnpublished = 6,
  Published = 7,
}

impl TaskState {
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

  fn name(self) -> &'static str {
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
}

#[derive(Clone)]
struct AttachmentSpec {
  role: u16,
  owner_id: Vec<u8>,
  artifact_hash: Vec<u8>,
  birth_generation: u64,
}

struct ExternalDescriptorSpec<'a> {
  workspace_id: [u8; 16],
  manifest_digest: [u8; 32],
  durable_sequence: u64,
  durable_bytes: u64,
  path: &'a str,
}

struct CheckpointSpec<'a> {
  task_id: [u8; 16],
  checkpoint_sequence: u64,
  generation: u64,
  task_kind: TaskKind,
  state: TaskState,
  phase: u16,
  required_capability_bits: &'a [u8],
  started_at_ms: u64,
  updated_at_ms: u64,
  source_root: Vec<u8>,
  target_root: Vec<u8>,
  primary_id: Vec<u8>,
  journal_head: Vec<u8>,
  journal_floor_sequence: u64,
  journal_audited_through: u64,
  next_document_ordinal: u64,
  completed_work: u64,
  total_work_hint: u64,
  resume_key: &'a [u8],
  attachments: &'a [AttachmentSpec],
  external: Option<ExternalDescriptorSpec<'a>>,
}

#[derive(Debug)]
struct DecodedCheckpoint {
  task_kind: TaskKind,
  state: TaskState,
  phase_name: &'static str,
  checkpoint_sequence: u64,
  attachment_count: u32,
  external: bool,
  key: Vec<u8>,
}

pub(crate) fn fixture_cases() -> Vec<IndexFixtureCase> {
  let mut cases = Vec::with_capacity(8);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for owner_kind in [JournalOwnerKind::Task, JournalOwnerKind::System] {
      let bytes = build_sample_journal(profile, owner_kind);
      let decoded = decode_journal(profile, &bytes).expect("sample journal must decode");
      cases.push(IndexFixtureCase {
        id: leak(format!("aidx-{}-{}-mutation-journal-valid", profile.label(), owner_kind.name())),
        format: IndexFormat::IndexArtifactV1,
        profile,
        expected: journal_expected(&decoded),
        relation: Some("roots:journal-chain-and-unpublished-mutation-coverage"),
        canonical_key: Some(hex::encode(decoded.key)),
        bytes,
      });
    }
    for external in [false, true] {
      let bytes = build_sample_checkpoint(profile, external);
      let decoded = decode_checkpoint(profile, &bytes).expect("sample checkpoint must decode");
      cases.push(IndexFixtureCase {
        id: leak(format!("aidx-{}-index-task-checkpoint-{}-valid", profile.label(), if external { "external" } else { "embedded" })),
        format: IndexFormat::IndexArtifactV1,
        profile,
        expected: checkpoint_expected(&decoded),
        relation: Some(if external {
          "node-local-external-state:not-transfer-authority"
        } else {
          "roots:typed-unpublished-index-artifacts"
        }),
        canonical_key: Some(hex::encode(decoded.key)),
        bytes,
      });
    }
  }
  cases
}

pub(crate) fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match read_u16(bytes, 6) {
    Ok(MUTATION_JOURNAL_KIND) => match decode_journal(profile, bytes) {
      Ok(journal) => (journal_expected(&journal).to_string(), Some(hex::encode(journal.key))),
      Err(error) => (format!("error:{error}"), None),
    },
    Ok(INDEX_TASK_CHECKPOINT_KIND) => match decode_checkpoint(profile, bytes) {
      Ok(checkpoint) => (checkpoint_expected(&checkpoint).to_string(), Some(hex::encode(checkpoint.key))),
      Err(error) => (format!("error:{error}"), None),
    },
    Ok(_) => ("error:index_task_artifact_kind".to_string(), None),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub(crate) fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let kind = read_u16(bytes, 6).unwrap_or(0);
  let identity_length = read_u16(bytes, 16).unwrap_or(0);
  let body_length = read_u32(bytes, 20).unwrap_or(0);
  let identity = match kind {
    MUTATION_JOURNAL_KIND => "journal_owner_id[16] || segment_ordinal u64 LE",
    INDEX_TASK_CHECKPOINT_KIND => "TaskId[16] || checkpoint_sequence u64 LE",
    _ => "unknown",
  };
  let body = match kind {
    MUTATION_JOURNAL_KIND => "56 + 4H header followed by canonical mutation records",
    INDEX_TASK_CHECKPOINT_KIND => "120 + 4H header followed by resume key, typed attachments, and optional external descriptor",
    _ => "unknown",
  };
  vec![
    "envelope +0x000 len 32: AIDX common envelope".to_string(),
    format!("envelope artifact_kind: 0x{kind:04x}"),
    format!("identity +0x000 len {identity_length}: {identity}"),
    format!("body +0x000 len {body_length}: {body}; H={}", profile.width()),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

fn sample_hash(profile: HashProfile, start: u8) -> Vec<u8> {
  let mut value = vec![0u8; profile.width()];
  fill_sequence(&mut value, start);
  value
}

fn build_sample_journal(profile: HashProfile, owner_kind: JournalOwnerKind) -> Vec<u8> {
  let owner_id = if owner_kind == JournalOwnerKind::System { SYSTEM_INDEX_JOURNAL_ID } else { [0x31; 16] };
  let root_before = sample_hash(profile, 0x41);
  let root_after = sample_hash(profile, 0x51);
  let mutation_id = sample_hash(profile, 0x61);
  let records = if owner_kind == JournalOwnerKind::Task {
    vec![
      MutationRecordSpec {
        kind: 1,
        sequence: 900,
        mutation_id: mutation_id.clone(),
        batch_ordinal: 0,
        batch_count: 2,
        root_before: root_before.clone(),
        root_after: root_after.clone(),
        before_path: None,
        before_revision: vec![0; profile.width()],
        after_path: Some("/docs/a.md"),
        after_revision: sample_hash(profile, 0x71),
        committed_at_ms: 1_800_000_000_001,
      },
      MutationRecordSpec {
        kind: 1,
        sequence: 900,
        mutation_id,
        batch_ordinal: 1,
        batch_count: 2,
        root_before: root_before.clone(),
        root_after: root_after.clone(),
        before_path: None,
        before_revision: vec![0; profile.width()],
        after_path: Some("/docs/b.md"),
        after_revision: sample_hash(profile, 0x81),
        committed_at_ms: 1_800_000_000_001,
      },
    ]
  } else {
    vec![MutationRecordSpec {
      kind: 2,
      sequence: 901,
      mutation_id,
      batch_ordinal: 0,
      batch_count: 1,
      root_before: root_before.clone(),
      root_after: root_after.clone(),
      before_path: Some("/docs/system.json"),
      before_revision: sample_hash(profile, 0x72),
      after_path: Some("/docs/system.json"),
      after_revision: sample_hash(profile, 0x82),
      committed_at_ms: 1_800_000_000_002,
    }]
  };
  build_journal(
    profile,
    JournalSpec {
      owner_id,
      owner_kind,
      generation: if owner_kind == JournalOwnerKind::Task { 40 } else { 41 },
      segment_ordinal: if owner_kind == JournalOwnerKind::Task { 0 } else { 8 },
      chain_reset: owner_kind == JournalOwnerKind::Task,
      previous_segment: if owner_kind == JournalOwnerKind::Task { vec![0; profile.width()] } else { sample_hash(profile, 0x21) },
      semantic_state_root: sample_hash(profile, 0x91),
      runtime_boot_id: [0xa1; 16],
      records: &records,
    },
  )
}

fn build_journal(profile: HashProfile, spec: JournalSpec<'_>) -> Vec<u8> {
  let h = profile.width();
  let encoded_records: Vec<Vec<u8>> = spec.records.iter().map(|record| build_mutation_record(profile, record)).collect();
  let records_length: usize = encoded_records.iter().map(Vec::len).sum();
  let mut body = vec![0u8; 56 + 4 * h + records_length];
  put_u32(&mut body, 0, u32::from(spec.chain_reset));
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, spec.owner_kind as u16);
  put_u64(&mut body, 8, spec.segment_ordinal);
  put_u64(&mut body, 16, spec.records.first().map_or(0, |record| record.sequence));
  put_u64(&mut body, 24, spec.records.last().map_or(0, |record| record.sequence));
  put_u32(&mut body, 32, spec.records.len() as u32);
  put_u32(&mut body, 36, records_length as u32);
  body[40..40 + h].copy_from_slice(&spec.previous_segment);
  if let Some(first) = spec.records.first() {
    body[40 + h..40 + 2 * h].copy_from_slice(&first.root_before);
  }
  if let Some(last) = spec.records.last() {
    body[40 + 2 * h..40 + 3 * h].copy_from_slice(&last.root_after);
  }
  body[40 + 3 * h..40 + 4 * h].copy_from_slice(&spec.semantic_state_root);
  body[40 + 4 * h..56 + 4 * h].copy_from_slice(&spec.runtime_boot_id);
  let mut offset = 56 + 4 * h;
  for record in encoded_records {
    body[offset..offset + record.len()].copy_from_slice(&record);
    offset += record.len();
  }
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&spec.owner_id);
  identity.extend_from_slice(&spec.segment_ordinal.to_le_bytes());
  build_immutable_value(MUTATION_JOURNAL_KIND, spec.generation, &identity, &body)
}

fn build_mutation_record(profile: HashProfile, spec: &MutationRecordSpec<'_>) -> Vec<u8> {
  let h = profile.width();
  let before = spec.before_path.unwrap_or("").as_bytes();
  let after = spec.after_path.unwrap_or("").as_bytes();
  let fixed = 40 + 7 * h;
  let mut record = vec![0u8; fixed + before.len() + after.len()];
  let record_length = record.len() as u32;
  put_u32(&mut record, 0, record_length);
  put_u16(&mut record, 4, spec.kind);
  put_u16(&mut record, 6, u16::from(spec.before_path.is_some()) | (u16::from(spec.after_path.is_some()) << 1));
  put_u64(&mut record, 8, spec.sequence);
  put_u32(&mut record, 16, spec.batch_ordinal);
  put_u32(&mut record, 20, spec.batch_count);
  record[24..24 + h].copy_from_slice(&spec.mutation_id);
  record[24 + h..24 + 2 * h].copy_from_slice(&spec.root_before);
  record[24 + 2 * h..24 + 3 * h].copy_from_slice(&spec.root_after);
  let before_key = spec.before_path.map_or(vec![0; h], |path| definitions::file_key(profile, path).expect("sample before path"));
  let after_key = spec.after_path.map_or(vec![0; h], |path| definitions::file_key(profile, path).expect("sample after path"));
  record[24 + 3 * h..24 + 4 * h].copy_from_slice(&before_key);
  record[24 + 4 * h..24 + 5 * h].copy_from_slice(&spec.before_revision);
  record[24 + 5 * h..24 + 6 * h].copy_from_slice(&after_key);
  record[24 + 6 * h..24 + 7 * h].copy_from_slice(&spec.after_revision);
  put_u32(&mut record, 24 + 7 * h, before.len() as u32);
  put_u32(&mut record, 28 + 7 * h, after.len() as u32);
  put_u64(&mut record, 32 + 7 * h, spec.committed_at_ms);
  record[fixed..fixed + before.len()].copy_from_slice(before);
  record[fixed + before.len()..].copy_from_slice(after);
  record
}

fn decode_journal(profile: HashProfile, bytes: &[u8]) -> Result<DecodedJournal, &'static str> {
  let artifact = decode_immutable_value(profile, bytes, MAX_JOURNAL_LENGTH)?;
  let h = profile.width();
  if artifact.kind != MUTATION_JOURNAL_KIND || artifact.identity.len() != 24 {
    return Err("mutation_journal_identity_length");
  }
  let owner_id: [u8; 16] = artifact.identity[..16].try_into().map_err(|_| "mutation_journal_owner")?;
  let segment_ordinal = read_u64(artifact.identity, 16)?;
  let body = artifact.body;
  if body.len() < 56 + 4 * h {
    return Err("mutation_journal_body_length");
  }
  let flags = read_u32(body, 0)?;
  let owner_kind = JournalOwnerKind::from_id(read_u16(body, 6)?).ok_or("mutation_journal_owner_kind")?;
  let first_sequence = read_u64(body, 16)?;
  let last_sequence = read_u64(body, 24)?;
  let record_count = read_u32(body, 32)?;
  let records_length = read_u32(body, 36)? as usize;
  let previous_segment = body[40..40 + h].to_vec();
  let source_root_before = body[40 + h..40 + 2 * h].to_vec();
  let source_root_after = body[40 + 2 * h..40 + 3 * h].to_vec();
  let semantic_state_root = &body[40 + 3 * h..40 + 4 * h];
  let runtime_boot_id = &body[40 + 4 * h..56 + 4 * h];
  let chain_reset = flags & 1 != 0;
  if flags & !1 != 0
    || read_u16(body, 4)? != 1
    || read_u64(body, 8)? != segment_ordinal
    || record_count == 0
    || record_count as usize > MAX_JOURNAL_RECORDS
    || 56usize.checked_add(4 * h).and_then(|length| length.checked_add(records_length)) != Some(body.len())
    || (chain_reset != previous_segment.iter().all(|byte| *byte == 0))
    || source_root_before.iter().all(|byte| *byte == 0)
    || source_root_after.iter().all(|byte| *byte == 0)
    || semantic_state_root.iter().all(|byte| *byte == 0)
    || runtime_boot_id.iter().all(|byte| *byte == 0)
  {
    return Err("mutation_journal_header");
  }
  match owner_kind {
    JournalOwnerKind::Task if owner_id.iter().all(|byte| *byte == 0) || owner_id == SYSTEM_INDEX_JOURNAL_ID => {
      return Err("mutation_journal_task_owner")
    }
    JournalOwnerKind::System if owner_id != SYSTEM_INDEX_JOURNAL_ID => return Err("mutation_journal_system_owner"),
    _ => {}
  }

  let mut records = Vec::with_capacity(record_count as usize);
  let mut offset = 56 + 4 * h;
  for _ in 0..record_count {
    let record = decode_mutation_record(profile, body, offset)?;
    let length = read_u32(body, offset)? as usize;
    offset = offset.checked_add(length).ok_or("mutation_record_overflow")?;
    records.push(record);
  }
  if offset != body.len()
    || records.first().map(|record| record.sequence) != Some(first_sequence)
    || records.last().map(|record| record.sequence) != Some(last_sequence)
    || records.first().is_none_or(|record| record.root_before != source_root_before)
    || records.last().is_none_or(|record| record.root_after != source_root_after)
  {
    return Err("mutation_journal_record_boundary");
  }
  validate_record_order_and_batches(&records)?;
  Ok(DecodedJournal {
    owner_kind,
    #[cfg(test)]
    owner_id,
    generation: artifact.generation,
    segment_ordinal,
    chain_reset,
    #[cfg(test)]
    previous_segment,
    #[cfg(test)]
    source_root_before,
    #[cfg(test)]
    source_root_after,
    record_count,
    first_sequence,
    last_sequence,
    key: artifact.key,
  })
}

fn decode_mutation_record(profile: HashProfile, bytes: &[u8], start: usize) -> Result<DecodedMutationRecord, &'static str> {
  let h = profile.width();
  let fixed = 40 + 7 * h;
  let record_length = read_u32(bytes, start)? as usize;
  if record_length < fixed {
    return Err("mutation_record_length");
  }
  let end = start.checked_add(record_length).ok_or("mutation_record_overflow")?;
  let record = bytes.get(start..end).ok_or("mutation_record_truncated")?;
  let kind = read_u16(record, 4)?;
  let presence = read_u16(record, 6)?;
  let sequence = read_u64(record, 8)?;
  let batch_ordinal = read_u32(record, 16)?;
  let batch_count = read_u32(record, 20)?;
  let before_length = read_u32(record, 24 + 7 * h)? as usize;
  let after_length = read_u32(record, 28 + 7 * h)? as usize;
  if !(1..=7).contains(&kind)
    || presence & !3 != 0
    || sequence == 0
    || batch_count == 0
    || batch_ordinal >= batch_count
    || fixed.checked_add(before_length).and_then(|length| length.checked_add(after_length)) != Some(record.len())
  {
    return Err("mutation_record_header");
  }
  let mutation_id = record[24..24 + h].to_vec();
  let root_before = record[24 + h..24 + 2 * h].to_vec();
  let root_after = record[24 + 2 * h..24 + 3 * h].to_vec();
  let before_key = &record[24 + 3 * h..24 + 4 * h];
  let before_revision = &record[24 + 4 * h..24 + 5 * h];
  let after_key = &record[24 + 5 * h..24 + 6 * h];
  let after_revision = &record[24 + 6 * h..24 + 7 * h];
  let committed_at_ms = read_u64(record, 32 + 7 * h)?;
  let before_path = &record[fixed..fixed + before_length];
  let after_path = &record[fixed + before_length..];
  if mutation_id.iter().all(|byte| *byte == 0) || root_before.iter().all(|byte| *byte == 0) || root_after.iter().all(|byte| *byte == 0) {
    return Err("mutation_record_identity");
  }
  validate_record_side(profile, presence & 1 != 0, before_path, before_key, before_revision)?;
  validate_record_side(profile, presence & 2 != 0, after_path, after_key, after_revision)?;
  match kind {
    1 | 5 | 6 if presence != 2 => return Err("mutation_record_kind_presence"),
    2 if presence != 3 => return Err("mutation_record_kind_presence"),
    3 if presence != 1 => return Err("mutation_record_kind_presence"),
    4 if presence != 3 || before_path == after_path => return Err("mutation_record_move_presence"),
    7 if presence == 0 => return Err("mutation_record_transition_presence"),
    _ => {}
  }
  Ok(DecodedMutationRecord { sequence, mutation_id, batch_ordinal, batch_count, root_before, root_after, committed_at_ms })
}

fn validate_record_side(
  profile: HashProfile,
  present: bool,
  path_bytes: &[u8],
  file_key: &[u8],
  revision: &[u8],
) -> Result<(), &'static str> {
  if !present {
    if !path_bytes.is_empty() || file_key.iter().any(|byte| *byte != 0) || revision.iter().any(|byte| *byte != 0) {
      return Err("mutation_record_absent_side");
    }
    return Ok(());
  }
  let path = std::str::from_utf8(path_bytes).map_err(|_| "mutation_record_path_utf8")?;
  if !definitions::is_canonical_absolute_path(path)
    || definitions::file_key(profile, path).map_err(|_| "mutation_record_path")? != file_key
    || revision.iter().all(|byte| *byte == 0)
  {
    return Err("mutation_record_present_side");
  }
  Ok(())
}

fn validate_record_order_and_batches(records: &[DecodedMutationRecord]) -> Result<(), &'static str> {
  for pair in records.windows(2) {
    let left = (&pair[0].sequence, &pair[0].mutation_id, &pair[0].batch_ordinal);
    let right = (&pair[1].sequence, &pair[1].mutation_id, &pair[1].batch_ordinal);
    if left.cmp(&right) != Ordering::Less {
      return Err("mutation_record_order");
    }
  }
  let mut start = 0;
  while start < records.len() {
    let first = &records[start];
    let count = first.batch_count as usize;
    let end = start.checked_add(count).ok_or("mutation_batch_overflow")?;
    let batch = records.get(start..end).ok_or("mutation_batch_incomplete")?;
    for (ordinal, record) in batch.iter().enumerate() {
      if record.sequence != first.sequence
        || record.mutation_id != first.mutation_id
        || record.root_before != first.root_before
        || record.root_after != first.root_after
        || record.batch_count != first.batch_count
        || record.batch_ordinal as usize != ordinal
        || record.committed_at_ms != first.committed_at_ms
      {
        return Err("mutation_batch_inconsistent");
      }
    }
    start = end;
  }
  Ok(())
}

#[cfg(test)]
fn validate_journal_chain(previous: &DecodedJournal, next: &DecodedJournal) -> Result<(), &'static str> {
  if previous.owner_id != next.owner_id
    || previous.owner_kind != next.owner_kind
    || previous.generation != next.generation
    || previous.segment_ordinal.checked_add(1) != Some(next.segment_ordinal)
    || next.chain_reset
    || next.previous_segment != previous.key
    || previous.source_root_after != next.source_root_before
    || previous.last_sequence >= next.first_sequence
  {
    return Err("mutation_journal_chain");
  }
  Ok(())
}

fn build_sample_checkpoint(profile: HashProfile, external: bool) -> Vec<u8> {
  let task_kind = if external { TaskKind::V0Migration } else { TaskKind::ScopeBuild };
  let state = if external { TaskState::Running } else { TaskState::CompleteUnpublished };
  let phase = if external { 3 } else { 6 };
  let mut capabilities = [0u8; 32];
  for bit in [7usize, 8, 9, 10, 11] {
    capabilities[bit / 8] |= 1 << (bit % 8);
  }
  let attachments = if external {
    vec![AttachmentSpec { role: 12, owner_id: sample_hash(profile, 0x21), artifact_hash: sample_hash(profile, 0x31), birth_generation: 17 }]
  } else {
    vec![
      AttachmentSpec { role: 1, owner_id: sample_hash(profile, 0x11), artifact_hash: sample_hash(profile, 0x21), birth_generation: 12 },
      AttachmentSpec { role: 8, owner_id: sample_hash(profile, 0x11), artifact_hash: sample_hash(profile, 0x31), birth_generation: 13 },
    ]
  };
  build_checkpoint(
    profile,
    CheckpointSpec {
      task_id: if external { [0xc2; 16] } else { [0xc1; 16] },
      checkpoint_sequence: if external { 18 } else { 17 },
      generation: 20,
      task_kind,
      state,
      phase,
      required_capability_bits: &capabilities,
      started_at_ms: 1_800_000_000_010,
      updated_at_ms: 1_800_000_000_020,
      source_root: sample_hash(profile, 0x41),
      target_root: if external { vec![0; profile.width()] } else { sample_hash(profile, 0x51) },
      primary_id: if external { vec![0; profile.width()] } else { sample_hash(profile, 0x11) },
      journal_head: sample_hash(profile, 0x61),
      journal_floor_sequence: 800,
      journal_audited_through: 900,
      next_document_ordinal: 1_024,
      completed_work: if external { 100 } else { 200 },
      total_work_hint: 1_000,
      resume_key: if external { b"legacy-offset:0000000000001000" } else { b"/docs/guide.md" },
      attachments: &attachments,
      external: external.then_some(ExternalDescriptorSpec {
        workspace_id: [0xd1; 16],
        manifest_digest: [0xe1; 32],
        durable_sequence: 77,
        durable_bytes: 8192,
        path: "/var/lib/aeordb/workspaces/migrate-01/run-0001",
      }),
    },
  )
}

fn build_checkpoint(profile: HashProfile, spec: CheckpointSpec<'_>) -> Vec<u8> {
  let h = profile.width();
  let attachment_length = 12 + 2 * h;
  let attachments_length = attachment_length * spec.attachments.len();
  let external = spec.external.as_ref().map_or_else(Vec::new, build_external_descriptor);
  let fixed = 120 + 4 * h;
  let mut body = vec![0u8; fixed + spec.resume_key.len() + attachments_length + external.len()];
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, spec.task_kind as u16);
  put_u16(&mut body, 8, spec.state as u16);
  put_u16(&mut body, 10, spec.phase);
  body[12..44].copy_from_slice(spec.required_capability_bits);
  put_u64(&mut body, 44, spec.started_at_ms);
  put_u64(&mut body, 52, spec.updated_at_ms);
  body[60..60 + h].copy_from_slice(&spec.source_root);
  body[60 + h..60 + 2 * h].copy_from_slice(&spec.target_root);
  body[60 + 2 * h..60 + 3 * h].copy_from_slice(&spec.primary_id);
  body[60 + 3 * h..60 + 4 * h].copy_from_slice(&spec.journal_head);
  put_u64(&mut body, 60 + 4 * h, spec.journal_floor_sequence);
  put_u64(&mut body, 68 + 4 * h, spec.journal_audited_through);
  put_u64(&mut body, 76 + 4 * h, spec.next_document_ordinal);
  put_u64(&mut body, 84 + 4 * h, spec.completed_work);
  put_u64(&mut body, 92 + 4 * h, spec.total_work_hint);
  put_u32(&mut body, 100 + 4 * h, spec.resume_key.len() as u32);
  put_u32(&mut body, 104 + 4 * h, spec.attachments.len() as u32);
  put_u32(&mut body, 108 + 4 * h, attachments_length as u32);
  put_u32(&mut body, 112 + 4 * h, external.len() as u32);
  body[fixed..fixed + spec.resume_key.len()].copy_from_slice(spec.resume_key);
  let mut offset = fixed + spec.resume_key.len();
  for attachment in spec.attachments {
    put_u16(&mut body, offset, attachment.role);
    body[offset + 4..offset + 4 + h].copy_from_slice(&attachment.owner_id);
    body[offset + 4 + h..offset + 4 + 2 * h].copy_from_slice(&attachment.artifact_hash);
    put_u64(&mut body, offset + 4 + 2 * h, attachment.birth_generation);
    offset += attachment_length;
  }
  body[offset..].copy_from_slice(&external);
  let mut identity = Vec::with_capacity(24);
  identity.extend_from_slice(&spec.task_id);
  identity.extend_from_slice(&spec.checkpoint_sequence.to_le_bytes());
  build_immutable_value(INDEX_TASK_CHECKPOINT_KIND, spec.generation, &identity, &body)
}

fn build_external_descriptor(spec: &ExternalDescriptorSpec<'_>) -> Vec<u8> {
  let path = spec.path.as_bytes();
  let mut bytes = vec![0u8; 68 + path.len()];
  bytes[..16].copy_from_slice(&spec.workspace_id);
  put_u32(&mut bytes, 16, path.len() as u32);
  bytes[20..52].copy_from_slice(&spec.manifest_digest);
  put_u64(&mut bytes, 52, spec.durable_sequence);
  put_u64(&mut bytes, 60, spec.durable_bytes);
  bytes[68..].copy_from_slice(path);
  bytes
}

fn decode_checkpoint(profile: HashProfile, bytes: &[u8]) -> Result<DecodedCheckpoint, &'static str> {
  let artifact = decode_immutable_value(profile, bytes, MAX_CHECKPOINT_LENGTH)?;
  let h = profile.width();
  if artifact.kind != INDEX_TASK_CHECKPOINT_KIND || artifact.identity.len() != 24 || artifact.identity[..16].iter().all(|byte| *byte == 0) {
    return Err("index_checkpoint_identity");
  }
  let checkpoint_sequence = read_u64(artifact.identity, 16)?;
  if checkpoint_sequence == 0 {
    return Err("index_checkpoint_sequence");
  }
  let body = artifact.body;
  let fixed = 120 + 4 * h;
  if body.len() < fixed || read_u32(body, 0)? != 0 || read_u16(body, 4)? != 1 || read_u32(body, 116 + 4 * h)? != 0 {
    return Err("index_checkpoint_header");
  }
  let task_kind = TaskKind::from_id(read_u16(body, 6)?).ok_or("index_checkpoint_task_kind")?;
  let state = TaskState::from_id(read_u16(body, 8)?).ok_or("index_checkpoint_task_state")?;
  let phase = read_u16(body, 10)?;
  let phase_name = task_kind.phase_name(phase).ok_or("index_checkpoint_phase")?;
  validate_capabilities(&body[12..44])?;
  let started_at_ms = read_u64(body, 44)?;
  let updated_at_ms = read_u64(body, 52)?;
  let source_root = &body[60..60 + h];
  let _target_root = &body[60 + h..60 + 2 * h];
  let _primary_id = &body[60 + 2 * h..60 + 3 * h];
  let journal_head = &body[60 + 3 * h..60 + 4 * h];
  let journal_floor = read_u64(body, 60 + 4 * h)?;
  let journal_audited = read_u64(body, 68 + 4 * h)?;
  let completed_work = read_u64(body, 84 + 4 * h)?;
  let total_work_hint = read_u64(body, 92 + 4 * h)?;
  let resume_length = read_u32(body, 100 + 4 * h)? as usize;
  let attachment_count = read_u32(body, 104 + 4 * h)?;
  let attachment_bytes = read_u32(body, 108 + 4 * h)? as usize;
  let external_length = read_u32(body, 112 + 4 * h)? as usize;
  if updated_at_ms < started_at_ms
    || source_root.iter().all(|byte| *byte == 0)
    || (journal_head.iter().all(|byte| *byte == 0) && (journal_floor != 0 || journal_audited != 0))
    || (journal_head.iter().any(|byte| *byte != 0) && (journal_audited == 0 || journal_floor > journal_audited))
    || (total_work_hint != 0 && completed_work > total_work_hint)
    || resume_length > MAX_RESUME_KEY_LENGTH
    || attachment_count as usize > MAX_ATTACHMENTS
    || external_length > MAX_EXTERNAL_DESCRIPTOR_LENGTH
  {
    return Err("index_checkpoint_semantics");
  }
  let attachment_length = 12 + 2 * h;
  if (attachment_count as usize).checked_mul(attachment_length) != Some(attachment_bytes)
    || fixed
      .checked_add(resume_length)
      .and_then(|length| length.checked_add(attachment_bytes))
      .and_then(|length| length.checked_add(external_length))
      != Some(body.len())
  {
    return Err("index_checkpoint_lengths");
  }
  let attachments_start = fixed + resume_length;
  validate_attachments(profile, &body[attachments_start..attachments_start + attachment_bytes], attachment_count)?;
  let external_start = attachments_start + attachment_bytes;
  if external_length != 0 {
    validate_external_descriptor(&body[external_start..])?;
  }
  Ok(DecodedCheckpoint {
    task_kind,
    state,
    phase_name,
    checkpoint_sequence,
    attachment_count,
    external: external_length != 0,
    key: artifact.key,
  })
}

fn validate_capabilities(bytes: &[u8]) -> Result<(), &'static str> {
  if bytes.len() != 32 || bytes[3..].iter().any(|byte| *byte != 0) {
    return Err("index_checkpoint_capabilities");
  }
  Ok(())
}

fn validate_attachments(profile: HashProfile, bytes: &[u8], count: u32) -> Result<(), &'static str> {
  let h = profile.width();
  let length = 12 + 2 * h;
  let mut prior: Option<(u16, &[u8], &[u8])> = None;
  for index in 0..count as usize {
    let record = &bytes[index * length..(index + 1) * length];
    let role = read_u16(record, 0)?;
    let flags = read_u16(record, 2)?;
    let owner = &record[4..4 + h];
    let artifact = &record[4 + h..4 + 2 * h];
    let generation = read_u64(record, 4 + 2 * h)?;
    if !(1..=12).contains(&role)
      || flags != 0
      || owner.iter().all(|byte| *byte == 0)
      || artifact.iter().all(|byte| *byte == 0)
      || generation == 0
    {
      return Err("index_checkpoint_attachment");
    }
    if prior.is_some_and(|prior| prior.cmp(&(role, owner, artifact)) != Ordering::Less) {
      return Err("index_checkpoint_attachment_order");
    }
    prior = Some((role, owner, artifact));
  }
  Ok(())
}

fn validate_external_descriptor(bytes: &[u8]) -> Result<(), &'static str> {
  if bytes.len() < 69 || bytes.len() > MAX_EXTERNAL_DESCRIPTOR_LENGTH {
    return Err("index_checkpoint_external_length");
  }
  let path_length = read_u32(bytes, 16)? as usize;
  let path = bytes.get(68..).ok_or("index_checkpoint_external_truncated")?;
  if bytes[..16].iter().all(|byte| *byte == 0)
    || bytes[20..52].iter().all(|byte| *byte == 0)
    || path_length == 0
    || 68usize.checked_add(path_length) != Some(bytes.len())
  {
    return Err("index_checkpoint_external_metadata");
  }
  let path = std::str::from_utf8(path).map_err(|_| "index_checkpoint_external_utf8")?;
  if !definitions::is_canonical_absolute_path(path) {
    return Err("index_checkpoint_external_path");
  }
  Ok(())
}

fn journal_expected(journal: &DecodedJournal) -> &'static str {
  leak(format!(
    "index:journal:{}:generation={}:segment={}:reset={}:records={}:sequences={}/{}",
    journal.owner_kind.name(),
    journal.generation,
    journal.segment_ordinal,
    journal.chain_reset,
    journal.record_count,
    journal.first_sequence,
    journal.last_sequence
  ))
}

fn checkpoint_expected(checkpoint: &DecodedCheckpoint) -> &'static str {
  leak(format!(
    "index:checkpoint:{}:task={}:state={}:phase={}:sequence={}:attachments={}",
    if checkpoint.external { "external" } else { "embedded" },
    checkpoint.task_kind.name(),
    checkpoint.state.name(),
    checkpoint.phase_name,
    checkpoint.checkpoint_sequence,
    checkpoint.attachment_count
  ))
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::index::write_trailing_crc;

  fn body_start() -> usize {
    32 + 24
  }

  fn test_mutation<'a>(
    profile: HashProfile,
    kind: u16,
    before_path: Option<&'a str>,
    after_path: Option<&'a str>,
  ) -> MutationRecordSpec<'a> {
    MutationRecordSpec {
      kind,
      sequence: 1,
      mutation_id: sample_hash(profile, 0x11),
      batch_ordinal: 0,
      batch_count: 1,
      root_before: sample_hash(profile, 0x21),
      root_after: sample_hash(profile, 0x31),
      before_path,
      before_revision: before_path.map_or(vec![0; profile.width()], |_| sample_hash(profile, 0x41)),
      after_path,
      after_revision: after_path.map_or(vec![0; profile.width()], |_| sample_hash(profile, 0x51)),
      committed_at_ms: 0,
    }
  }

  fn test_checkpoint(
    profile: HashProfile,
    resume_key: &[u8],
    attachments: &[AttachmentSpec],
    external: Option<ExternalDescriptorSpec<'_>>,
  ) -> Vec<u8> {
    let capabilities = [0u8; 32];
    build_checkpoint(
      profile,
      CheckpointSpec {
        task_id: [0x11; 16],
        checkpoint_sequence: 1,
        generation: 1,
        task_kind: TaskKind::V0Migration,
        state: TaskState::Running,
        phase: 1,
        required_capability_bits: &capabilities,
        started_at_ms: 0,
        updated_at_ms: 0,
        source_root: sample_hash(profile, 0x21),
        target_root: vec![0; profile.width()],
        primary_id: vec![0; profile.width()],
        journal_head: vec![0; profile.width()],
        journal_floor_sequence: 0,
        journal_audited_through: 0,
        next_document_ordinal: 0,
        completed_work: 0,
        total_work_hint: 0,
        resume_key,
        attachments,
        external,
      },
    )
  }

  #[test]
  fn task_artifact_fixtures_decode_with_exact_keys() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn journals_reject_bad_owners_headers_records_and_batches_after_crc_repair() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let baseline = build_sample_journal(profile, JournalOwnerKind::Task);
      let body = body_start();
      let record = body + 56 + 4 * h;
      for offset in [body, body + 4, body + 6, body + 8, body + 16, body + 32, body + 36] {
        let mut changed = baseline.clone();
        changed[offset] ^= 0x80;
        write_trailing_crc(&mut changed);
        assert!(decode_journal(profile, &changed).is_err(), "journal header offset {offset} accepted");
      }
      for range in [body + 40 + 3 * h..body + 40 + 4 * h, body + 40 + 4 * h..body + 56 + 4 * h] {
        let mut changed = baseline.clone();
        changed[range].fill(0);
        write_trailing_crc(&mut changed);
        assert!(decode_journal(profile, &changed).is_err(), "journal required identity accepted as zero");
      }
      for offset in [record, record + 4, record + 6, record + 8, record + 16, record + 20, record + 24, record + 32 + 7 * h] {
        let mut changed = baseline.clone();
        changed[offset] ^= 0x80;
        write_trailing_crc(&mut changed);
        assert!(decode_journal(profile, &changed).is_err(), "journal record offset {offset} accepted");
      }

      let system = build_sample_journal(profile, JournalOwnerKind::System);
      let mut wrong_system_owner = system.clone();
      wrong_system_owner[32] ^= 1;
      write_trailing_crc(&mut wrong_system_owner);
      assert_eq!(decode_journal(profile, &wrong_system_owner).err(), Some("mutation_journal_system_owner"));
    }
  }

  #[test]
  fn every_mutation_kind_has_exact_before_after_presence_semantics() {
    let profile = HashProfile::Blake3_256;
    let valid = [
      (1, None, Some("/after")),
      (2, Some("/same"), Some("/same")),
      (3, Some("/before"), None),
      (4, Some("/before"), Some("/after")),
      (5, None, Some("/copy")),
      (6, None, Some("/restore")),
      (7, Some("/transition"), None),
    ];
    for (kind, before, after) in valid {
      let encoded = build_mutation_record(profile, &test_mutation(profile, kind, before, after));
      assert!(decode_mutation_record(profile, &encoded, 0).is_ok(), "valid mutation kind {kind} rejected");
    }

    for kind in 1..=7 {
      let mut encoded = build_mutation_record(profile, &test_mutation(profile, kind, None, Some("/after")));
      put_u16(&mut encoded, 6, 0);
      assert!(decode_mutation_record(profile, &encoded, 0).is_err(), "mutation kind {kind} accepted no sides");
    }
    let encoded = build_mutation_record(profile, &test_mutation(profile, 8, None, Some("/after")));
    assert_eq!(decode_mutation_record(profile, &encoded, 0).err(), Some("mutation_record_header"));
  }

  #[test]
  fn journal_record_and_artifact_bounds_are_exact_and_checked_first() {
    let profile = HashProfile::Blake3_256;
    let mut records = Vec::with_capacity(MAX_JOURNAL_RECORDS);
    for sequence in 1..=MAX_JOURNAL_RECORDS as u64 {
      let mut record = test_mutation(profile, 1, None, Some("/bounded"));
      record.sequence = sequence;
      records.push(record);
    }
    let maximum = build_journal(
      profile,
      JournalSpec {
        owner_id: [0x11; 16],
        owner_kind: JournalOwnerKind::Task,
        generation: 1,
        segment_ordinal: 0,
        chain_reset: true,
        previous_segment: vec![0; profile.width()],
        semantic_state_root: sample_hash(profile, 0x31),
        runtime_boot_id: [0x41; 16],
        records: &records,
      },
    );
    assert!(decode_journal(profile, &maximum).is_ok());

    let mut too_many = maximum;
    put_u32(&mut too_many, body_start() + 32, (MAX_JOURNAL_RECORDS + 1) as u32);
    write_trailing_crc(&mut too_many);
    assert_eq!(decode_journal(profile, &too_many).err(), Some("mutation_journal_header"));
    assert_eq!(decode_journal(profile, &vec![0; MAX_JOURNAL_LENGTH + 1]).err(), Some("index_immutable_length"));
  }

  #[test]
  fn journal_chain_requires_exact_owner_generation_hash_root_and_sequence_continuity() {
    let profile = HashProfile::Blake3_256;
    let first_bytes = build_sample_journal(profile, JournalOwnerKind::Task);
    let first = decode_journal(profile, &first_bytes).unwrap();
    let root_after = first.source_root_after.clone();
    let next_record = [MutationRecordSpec {
      kind: 3,
      sequence: 901,
      mutation_id: sample_hash(profile, 0xb1),
      batch_ordinal: 0,
      batch_count: 1,
      root_before: root_after,
      root_after: sample_hash(profile, 0xc1),
      before_path: Some("/docs/a.md"),
      before_revision: sample_hash(profile, 0xd1),
      after_path: None,
      after_revision: vec![0; profile.width()],
      committed_at_ms: 1_800_000_000_003,
    }];
    let next_bytes = build_journal(
      profile,
      JournalSpec {
        owner_id: first.owner_id,
        owner_kind: first.owner_kind,
        generation: first.generation,
        segment_ordinal: first.segment_ordinal + 1,
        chain_reset: false,
        previous_segment: first.key.clone(),
        semantic_state_root: sample_hash(profile, 0xe1),
        runtime_boot_id: [0xf1; 16],
        records: &next_record,
      },
    );
    let next = decode_journal(profile, &next_bytes).unwrap();
    assert_eq!(validate_journal_chain(&first, &next), Ok(()));

    let mut stale = next;
    stale.previous_segment[0] ^= 1;
    assert_eq!(validate_journal_chain(&first, &stale), Err("mutation_journal_chain"));
  }

  #[test]
  fn checkpoints_reject_unknown_registry_values_and_incoherent_lengths_or_state() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      let baseline = build_sample_checkpoint(profile, false);
      let body = body_start();
      for offset in [
        body,
        body + 4,
        body + 6,
        body + 8,
        body + 10,
        body + 12 + 3,
        body + 100 + 4 * h,
        body + 104 + 4 * h,
        body + 108 + 4 * h,
        body + 116 + 4 * h,
      ] {
        let mut changed = baseline.clone();
        changed[offset] ^= 0x80;
        write_trailing_crc(&mut changed);
        assert!(decode_checkpoint(profile, &changed).is_err(), "checkpoint offset {offset} accepted");
      }

      let mut reversed_time = baseline.clone();
      put_u64(&mut reversed_time, body + 52, 1);
      write_trailing_crc(&mut reversed_time);
      assert_eq!(decode_checkpoint(profile, &reversed_time).err(), Some("index_checkpoint_semantics"));

      let mut zero_source = baseline.clone();
      zero_source[body + 60..body + 60 + h].fill(0);
      write_trailing_crc(&mut zero_source);
      assert_eq!(decode_checkpoint(profile, &zero_source).err(), Some("index_checkpoint_semantics"));

      let mut impossible_progress = baseline.clone();
      put_u64(&mut impossible_progress, body + 84 + 4 * h, 1_001);
      write_trailing_crc(&mut impossible_progress);
      assert_eq!(decode_checkpoint(profile, &impossible_progress).err(), Some("index_checkpoint_semantics"));

      let external = build_sample_checkpoint(profile, true);
      let external_length = read_u32(&external, body + 112 + 4 * h).unwrap() as usize;
      let external_start = external.len() - 4 - external_length;
      let mut zero_workspace = external.clone();
      zero_workspace[external_start..external_start + 16].fill(0);
      write_trailing_crc(&mut zero_workspace);
      assert!(decode_checkpoint(profile, &zero_workspace).is_err());

      let mut wrong_path_length = external.clone();
      put_u32(&mut wrong_path_length, external_start + 16, 1);
      write_trailing_crc(&mut wrong_path_length);
      assert!(decode_checkpoint(profile, &wrong_path_length).is_err());

      let mut zero_manifest = external.clone();
      zero_manifest[external_start + 20..external_start + 52].fill(0);
      write_trailing_crc(&mut zero_manifest);
      assert!(decode_checkpoint(profile, &zero_manifest).is_err());

      let mut relative_path = external.clone();
      relative_path[external_start + 68] = b'x';
      write_trailing_crc(&mut relative_path);
      assert!(decode_checkpoint(profile, &relative_path).is_err());
    }
  }

  #[test]
  fn every_task_kind_has_a_closed_permanent_phase_registry() {
    for id in 1..=8 {
      let task = TaskKind::from_id(id).unwrap();
      assert!(task.phase_name(1).is_some());
      let first_unknown = (1..=16).find(|phase| task.phase_name(*phase).is_none()).unwrap();
      assert!(task.phase_name(first_unknown).is_none());
      assert!(task.phase_name(0).is_none());
    }
    assert!(TaskKind::from_id(9).is_none());
    for id in 1..=7 {
      assert!(TaskState::from_id(id).is_some());
    }
    assert!(TaskState::from_id(0).is_none());
    assert!(TaskState::from_id(8).is_none());
  }

  #[test]
  fn attachment_role_registry_is_closed_and_spill_metadata_is_not_an_untyped_gc_edge() {
    let profile = HashProfile::Blake3_256;
    let h = profile.width();
    let length = 12 + 2 * h;
    let mut record = vec![0u8; length];
    put_u16(&mut record, 0, 12);
    fill_sequence(&mut record[4..4 + h], 0x11);
    fill_sequence(&mut record[4 + h..4 + 2 * h], 0x21);
    put_u64(&mut record, 4 + 2 * h, 1);
    assert_eq!(validate_attachments(profile, &record, 1), Ok(()));

    put_u16(&mut record, 0, 13);
    assert_eq!(validate_attachments(profile, &record, 1), Err("index_checkpoint_attachment"));
  }

  #[test]
  fn checkpoint_component_limits_accept_the_boundary_and_reject_one_more() {
    let profile = HashProfile::Blake3_256;
    let maximum_resume = vec![0x61; MAX_RESUME_KEY_LENGTH];
    assert!(decode_checkpoint(profile, &test_checkpoint(profile, &maximum_resume, &[], None)).is_ok());
    let oversized_resume = vec![0x61; MAX_RESUME_KEY_LENGTH + 1];
    assert_eq!(
      decode_checkpoint(profile, &test_checkpoint(profile, &oversized_resume, &[], None)).err(),
      Some("index_checkpoint_semantics")
    );

    let mut attachments = Vec::with_capacity(MAX_ATTACHMENTS + 1);
    for index in 0..=MAX_ATTACHMENTS {
      let mut owner = vec![0u8; profile.width()];
      owner[0] = 1;
      owner[profile.width() - 4..].copy_from_slice(&(index as u32).to_be_bytes());
      attachments.push(AttachmentSpec { role: 1, owner_id: owner, artifact_hash: sample_hash(profile, 0x31), birth_generation: 1 });
    }
    assert!(decode_checkpoint(profile, &test_checkpoint(profile, &[], &attachments[..MAX_ATTACHMENTS], None)).is_ok());
    assert_eq!(decode_checkpoint(profile, &test_checkpoint(profile, &[], &attachments, None)).err(), Some("index_checkpoint_semantics"));

    let exact_path = format!("/{}", "a".repeat(MAX_EXTERNAL_DESCRIPTOR_LENGTH - 69));
    let exact = ExternalDescriptorSpec {
      workspace_id: [0x11; 16],
      manifest_digest: [0x21; 32],
      durable_sequence: 0,
      durable_bytes: 0,
      path: &exact_path,
    };
    assert_eq!(build_external_descriptor(&exact).len(), MAX_EXTERNAL_DESCRIPTOR_LENGTH);
    assert_eq!(validate_external_descriptor(&build_external_descriptor(&exact)), Ok(()));

    let oversized_path = format!("{exact_path}a");
    let oversized = ExternalDescriptorSpec { path: &oversized_path, ..exact };
    assert_eq!(validate_external_descriptor(&build_external_descriptor(&oversized)), Err("index_checkpoint_external_length"));
    assert_eq!(decode_checkpoint(profile, &vec![0; MAX_CHECKPOINT_LENGTH + 1]).err(), Some("index_immutable_length"));
  }
}
