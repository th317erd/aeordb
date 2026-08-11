use std::cmp::Ordering;

use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, PhysicalIncarnationV1, decode_gc_artifact_envelope,
  decode_physical_incarnation, encode_immutable_gc_artifact, immutable_gc_artifact_key, u16_at, u32_at, u64_at,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

pub const MARK_CHECKPOINT_VALUE_MAX: usize = 32 + 40 + 256 * 1024 + 4;
const MARK_JOURNAL_MAX: usize = 16 * 1024 * 1024;
pub const WORKSPACE_MANIFEST_MAX: usize = 8 * 1024 * 1024;
pub const WORKSPACE_OBJECT_MAX: usize = 64 * 1024 * 1024;
pub const WORKSPACE_OBJECT_HEADER: usize = 80;
const MAX_WORKSPACE_RECORD: usize = 1024 * 1024;
const MAX_WORKSPACE_NAME: usize = 4 * 1024;
const MARK_REQUIRED_CAPABILITY_BITS: &[usize] = &[12, 13, 14, 15, 17];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum MarkWorkspaceObjectKindV1 {
  Bitmap = 1,
  Frontier = 2,
  PathVisit = 3,
  Mutation = 4,
  Candidate = 5,
  Diagnostic = 6,
}

impl MarkWorkspaceObjectKindV1 {
  pub const ALL: [Self; 6] = [Self::Bitmap, Self::Frontier, Self::PathVisit, Self::Mutation, Self::Candidate, Self::Diagnostic];

  pub fn from_u16(value: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| *kind as u16 == value)
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::Bitmap => "bitmap",
      Self::Frontier => "frontier",
      Self::PathVisit => "path-visit",
      Self::Mutation => "mutation",
      Self::Candidate => "candidate",
      Self::Diagnostic => "diagnostic",
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum MarkMutationOperationV1 {
  Create = 1,
  Replace = 2,
  Delete = 3,
  Activate = 4,
  Deactivate = 5,
  Promote = 6,
  Restore = 7,
  Reconcile = 8,
  Retire = 9,
  Repair = 10,
}

impl MarkMutationOperationV1 {
  pub const ALL: [Self; 10] = [
    Self::Create,
    Self::Replace,
    Self::Delete,
    Self::Activate,
    Self::Deactivate,
    Self::Promote,
    Self::Restore,
    Self::Reconcile,
    Self::Retire,
    Self::Repair,
  ];

  pub fn from_u16(value: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|operation| *operation as u16 == value)
  }
}

#[derive(Debug, Clone)]
pub struct MarkRunCheckpointV1<'a> {
  pub database_id: &'a [u8],
  pub run_id: &'a [u8],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub state: u16,
  pub phase: u16,
  pub resumable: bool,
  pub canceled: bool,
  pub capabilities: &'a [u8],
  pub started_at_ms: u64,
  pub updated_at_ms: u64,
  pub authority_root_set_digest: &'a [u8],
  pub semantic_state_digest: &'a [u8],
  pub kv_layout_fingerprint: &'a [u8],
  pub effective_policy_fingerprint: &'a [u8],
  pub system_family_registry_fingerprint: &'a [u8],
  pub captured_header_sequence: u64,
  pub captured_write_high_water: u64,
  pub reconciled_through_sequence: u64,
  pub active_bitmap_bit_count: u64,
  pub kv_bucket_count: u64,
  pub kv_slots_per_bucket: u32,
  pub workspace_path: &'a str,
  pub workspace_id: &'a [u8],
  pub workspace_manifest_digest: &'a [u8],
  pub mutation_journal_head: &'a [u8],
  pub checkpoint_logical_work: u64,
  pub total_logical_work_hint: u64,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkRunCheckpointWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub run_id: &'a [u8; 16],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub state: u16,
  pub phase: u16,
  pub resumable: bool,
  pub canceled: bool,
  pub capabilities: [u8; 32],
  pub started_at_ms: u64,
  pub updated_at_ms: u64,
  pub authority_root_set_digest: &'a [u8],
  pub semantic_state_digest: &'a [u8],
  pub kv_layout_fingerprint: &'a [u8],
  pub effective_policy_fingerprint: [u8; 32],
  pub system_family_registry_fingerprint: [u8; 32],
  pub captured_header_sequence: u64,
  pub captured_write_high_water: u64,
  pub reconciled_through_sequence: u64,
  pub active_bitmap_bit_count: u64,
  pub kv_bucket_count: u64,
  pub kv_slots_per_bucket: u32,
  pub workspace_path: &'a str,
  pub workspace_id: [u8; 16],
  pub workspace_manifest_digest: [u8; 32],
  pub mutation_journal_head: &'a [u8],
  pub checkpoint_logical_work: u64,
  pub total_logical_work_hint: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct MarkResumeContextV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8],
  pub run_id: &'a [u8],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub workspace_path: &'a str,
  pub workspace_id: &'a [u8],
  pub authority_root_set_digest: &'a [u8],
  pub semantic_state_digest: &'a [u8],
  pub kv_layout_fingerprint: &'a [u8],
  pub effective_policy_fingerprint: &'a [u8],
  pub system_family_registry_fingerprint: &'a [u8],
  pub captured_header_sequence: u64,
  pub captured_write_high_water: u64,
  pub reconciled_through_sequence: u64,
  pub active_bitmap_bit_count: u64,
  pub kv_bucket_count: u64,
  pub kv_slots_per_bucket: u32,
}

#[derive(Debug, Clone)]
pub struct MarkMutationJournalV1<'a> {
  pub database_id: &'a [u8],
  pub run_id: &'a [u8],
  pub generation: u64,
  pub segment_sequence: u64,
  pub reset: bool,
  pub predecessor: &'a [u8],
  pub record_count: u32,
  pub first_sequence: u64,
  pub last_sequence: u64,
  pub first_mutation_id: &'a [u8],
  pub last_mutation_id: &'a [u8],
  records: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkMutationRecordV1<'a> {
  pub encoded: &'a [u8],
  pub publication_sequence: u64,
  pub mutation_id: &'a [u8],
  pub root_before: &'a [u8],
  pub root_after: &'a [u8],
  pub published_logical_key: &'a [u8],
  pub new_incarnation_bytes: &'a [u8],
  pub new_incarnation: PhysicalIncarnationV1<'a>,
  pub operation: MarkMutationOperationV1,
}

#[derive(Debug, Clone, Copy)]
pub struct MarkMutationRecordWriteV1<'a> {
  pub publication_sequence: u64,
  pub mutation_id: &'a [u8],
  pub root_before: &'a [u8],
  pub root_after: &'a [u8],
  pub published_logical_key: &'a [u8],
  pub new_incarnation: &'a [u8],
  pub operation: MarkMutationOperationV1,
}

#[derive(Debug, Clone, Copy)]
pub struct MarkMutationJournalSegmentWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: &'a [u8; 16],
  pub run_id: &'a [u8; 16],
  pub generation: u64,
  pub segment_ordinal: u64,
  pub previous_segment_hash: Option<&'a [u8]>,
  pub records: &'a [MarkMutationRecordWriteV1<'a>],
}

#[derive(Debug)]
pub struct MarkMutationJournalRecordsV1<'a> {
  records: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
}

impl<'a> Iterator for MarkMutationJournalRecordsV1<'a> {
  type Item = FormatResult<MarkMutationRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.records.next().map(|record| decode_mark_mutation_record(record, self.algorithm))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.records.size_hint()
  }
}

impl ExactSizeIterator for MarkMutationJournalRecordsV1<'_> {}

#[derive(Debug, Clone)]
pub enum GcMarkArtifactV1<'a> {
  Checkpoint(Box<MarkRunCheckpointV1<'a>>),
  MutationJournal(MarkMutationJournalV1<'a>),
}

impl GcMarkArtifactV1<'_> {
  pub fn key(&self) -> &[u8] {
    match self {
      Self::Checkpoint(value) => &value.key,
      Self::MutationJournal(value) => &value.key,
    }
  }

  pub fn summary(&self) -> String {
    match self {
      Self::Checkpoint(value) => {
        let label = if value.workspace_path.as_bytes().get(1) == Some(&b':') { "external-canceled" } else { "embedded" };
        format!("gc:checkpoint:mark-run:{label}:state={}:phase={}", value.state, value.phase)
      }
      Self::MutationJournal(value) => format!(
        "gc:journal:mark-mutation:{}:records={}:first={}:last={}",
        if value.reset { "reset" } else { "linked" },
        value.record_count,
        value.first_sequence,
        value.last_sequence
      ),
    }
  }
}

pub fn validate_mark_mutation_journal_chain(previous: &MarkMutationJournalV1<'_>, current: &MarkMutationJournalV1<'_>) -> FormatResult<()> {
  if current.reset
    || previous.database_id != current.database_id
    || previous.run_id != current.run_id
    || previous.generation != current.generation
    || previous.segment_sequence.checked_add(1) != Some(current.segment_sequence)
    || current.predecessor != previous.key
    || (current.first_sequence, current.first_mutation_id) <= (previous.last_sequence, previous.last_mutation_id)
  {
    return Err(closure_error(
      "mark_mutation_journal_chain",
      "mark mutation journal identity, predecessor, ordinal, or sequence range is discontinuous",
    ));
  }
  Ok(())
}

pub fn mark_mutation_journal_records_v1<'a>(
  segment: &MarkMutationJournalV1<'a>,
  algorithm: HashAlgorithm,
) -> FormatResult<MarkMutationJournalRecordsV1<'a>> {
  let record_length = checked_add(40, checked_mul(6, algorithm.hash_length(), "mark mutation record width")?, "mark mutation record")?;
  let expected_length = (segment.record_count as usize)
    .checked_mul(record_length)
    .ok_or_else(|| amplification_error("mark_mutation_record_count", segment.record_count as usize, MARK_JOURNAL_MAX / record_length))?;
  if segment.records.len() != expected_length {
    return Err(closure_error("mark_mutation_record_count", "mark mutation record count does not match its validated byte range"));
  }
  Ok(MarkMutationJournalRecordsV1 { records: segment.records.chunks_exact(record_length), algorithm })
}

pub fn encode_mark_mutation_journal_segment(request: &MarkMutationJournalSegmentWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let record_length =
    checked_add(40, checked_mul(6, request.hash_algorithm.hash_length(), "mark mutation record width")?, "mark mutation record")?;
  let records_length = checked_mul(request.records.len(), record_length, "mark mutation records")?;
  let mut records = Vec::with_capacity(records_length);
  for record in request.records {
    encode_mark_mutation_record(&mut records, *record, request.hash_algorithm)?;
  }
  encode_mark_mutation_journal_segment_records_v1(
    request.hash_algorithm,
    request.database_id,
    request.run_id,
    request.generation,
    request.segment_ordinal,
    request.previous_segment_hash,
    &records,
  )
}

pub(crate) fn encode_mark_mutation_journal_segment_records_v1(
  algorithm: HashAlgorithm,
  database_id: &[u8; 16],
  run_id: &[u8; 16],
  generation: u64,
  segment_ordinal: u64,
  previous_segment_hash: Option<&[u8]>,
  records: &[u8],
) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = algorithm.hash_length();
  if database_id.iter().all(|byte| *byte == 0)
    || run_id.iter().all(|byte| *byte == 0)
    || generation == 0
    || segment_ordinal == 0
    || records.is_empty()
  {
    return Err(identity_error("mark_mutation_journal_write_identity", "mark mutation journal identity and records must be nonzero"));
  }
  if previous_segment_hash.is_some_and(|hash| hash.len() != hash_width || all_zero(hash)) {
    return Err(identity_error(
      "mark_mutation_journal_write_predecessor",
      "mark mutation journal predecessor does not match the selected hash width",
    ));
  }
  let record_length = checked_add(40, checked_mul(6, hash_width, "mark mutation record width")?, "mark mutation record")?;
  if !records.len().is_multiple_of(record_length) {
    return Err(trailing_error("mark_mutation_records_length", "mark mutation records do not close on a fixed record boundary"));
  }
  let record_count_usize = records.len() / record_length;
  if record_count_usize > u32::MAX as usize {
    return Err(amplification_error("mark_mutation_record_count", record_count_usize, u32::MAX as usize));
  }
  let record_count = record_count_usize as u32;
  let mut first_cursor = None;
  let mut previous_cursor: Option<(u64, &[u8])> = None;
  for record in records.chunks_exact(record_length) {
    let decoded = decode_mark_mutation_record(record, algorithm)?;
    let cursor = (decoded.publication_sequence, decoded.mutation_id);
    if previous_cursor.is_some_and(|previous| cursor <= previous) {
      return Err(order_error("mark_mutation_record_order", "mark mutation records are duplicate or out of order"));
    }
    first_cursor.get_or_insert(cursor);
    previous_cursor = Some(cursor);
  }
  let Some((first_sequence, _)) = first_cursor else {
    return Err(closure_error("mark_mutation_record_count", "mark mutation segment contains no records"));
  };
  let Some((last_sequence, _)) = previous_cursor else {
    return Err(closure_error("mark_mutation_record_count", "mark mutation segment contains no records"));
  };
  let body_length = checked_add(32 + hash_width, records.len(), "mark mutation journal body")?;
  ensure_cap("mark_mutation_journal_length", checked_add(76, body_length, "mark mutation journal complete value")?, MARK_JOURNAL_MAX)?;
  if records.len() > u32::MAX as usize {
    return Err(amplification_error("mark_mutation_records_length", records.len(), u32::MAX as usize));
  }
  let records_length = records.len() as u32;
  let mut body = Vec::with_capacity(body_length);
  body.extend_from_slice(&u32::from(previous_segment_hash.is_none()).to_le_bytes());
  body.extend_from_slice(&1u16.to_le_bytes());
  body.extend_from_slice(&0u16.to_le_bytes());
  body.extend_from_slice(&first_sequence.to_le_bytes());
  body.extend_from_slice(&last_sequence.to_le_bytes());
  body.extend_from_slice(&record_count.to_le_bytes());
  body.extend_from_slice(&records_length.to_le_bytes());
  match previous_segment_hash {
    Some(hash) => body.extend_from_slice(hash),
    None => body.resize(body.len() + hash_width, 0),
  }
  body.extend_from_slice(records);

  let mut identity = [0u8; 40];
  identity[..16].copy_from_slice(database_id);
  identity[16..32].copy_from_slice(run_id);
  identity[32..].copy_from_slice(&segment_ordinal.to_le_bytes());
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::MarkMutationJournalSegment,
    hash_algorithm: algorithm,
    generation,
    identity: &identity,
    body: &body,
  })?;
  let GcMarkArtifactV1::MutationJournal(decoded) = decode_gc_mark_artifact(&encoded.value, algorithm)? else {
    return Err(closure_error("mark_mutation_writer_readback", "encoded mark mutation segment decoded as another artifact kind"));
  };
  if decoded.key != encoded.key
    || decoded.segment_sequence != segment_ordinal
    || decoded.generation != generation
    || decoded.record_count != record_count
  {
    return Err(closure_error("mark_mutation_writer_readback", "encoded mark mutation segment did not close against its request"));
  }
  Ok(encoded)
}

#[derive(Debug, Clone)]
pub struct MarkWorkspaceDescriptorV1<'a> {
  pub kind: MarkWorkspaceObjectKindV1,
  pub ordinal: u64,
  pub stored_length: u64,
  pub logical_record_count: u64,
  pub digest: [u8; 32],
  pub name: &'a str,
}

#[derive(Debug, Clone)]
pub struct MarkWorkspaceManifestV1<'a> {
  pub state: u16,
  pub database_id: &'a [u8],
  pub run_id: &'a [u8],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub created_at_ms: u64,
  pub updated_at_ms: u64,
  pub hash_algorithm: HashAlgorithm,
  pub kv_layout_fingerprint: &'a [u8],
  pub authority_root_set_digest: &'a [u8],
  pub effective_policy_fingerprint: &'a [u8],
  pub stored_bytes: u64,
  pub logical_record_count: u64,
  pub descriptors: Vec<MarkWorkspaceDescriptorV1<'a>>,
}

pub fn encode_mark_run_checkpoint(request: &MarkRunCheckpointWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  let path = request.workspace_path.as_bytes();
  let body_length = checked_add(236, checked_mul(4, hash_width, "mark checkpoint hash width")?, "mark checkpoint body")?;
  let body_length = checked_add(body_length, path.len(), "mark checkpoint path")?;
  if body_length > 256 * 1024 {
    return Err(amplification_error("mark_checkpoint_length", body_length, 256 * 1024));
  }
  let mut body = vec![0u8; body_length];
  let flags = u32::from(request.resumable) | (u32::from(request.canceled) << 1);
  body[..4].copy_from_slice(&flags.to_le_bytes());
  body[4..6].copy_from_slice(&1u16.to_le_bytes());
  body[6..8].copy_from_slice(&request.state.to_le_bytes());
  body[8..10].copy_from_slice(&request.phase.to_le_bytes());
  body[12..44].copy_from_slice(&request.capabilities);
  body[44..52].copy_from_slice(&request.started_at_ms.to_le_bytes());
  body[52..60].copy_from_slice(&request.updated_at_ms.to_le_bytes());
  copy_exact_hash(&mut body[60..60 + hash_width], request.authority_root_set_digest, hash_width, "authority root-set digest")?;
  copy_exact_hash(&mut body[60 + hash_width..60 + 2 * hash_width], request.semantic_state_digest, hash_width, "semantic state digest")?;
  copy_exact_hash(&mut body[60 + 2 * hash_width..60 + 3 * hash_width], request.kv_layout_fingerprint, hash_width, "KV layout fingerprint")?;
  body[60 + 3 * hash_width..92 + 3 * hash_width].copy_from_slice(&request.effective_policy_fingerprint);
  body[92 + 3 * hash_width..124 + 3 * hash_width].copy_from_slice(&request.system_family_registry_fingerprint);
  let scalar = 124 + 3 * hash_width;
  body[scalar..scalar + 8].copy_from_slice(&request.captured_header_sequence.to_le_bytes());
  body[scalar + 8..scalar + 16].copy_from_slice(&request.captured_write_high_water.to_le_bytes());
  body[scalar + 16..scalar + 24].copy_from_slice(&request.reconciled_through_sequence.to_le_bytes());
  body[scalar + 24..scalar + 32].copy_from_slice(&request.active_bitmap_bit_count.to_le_bytes());
  body[scalar + 32..scalar + 40].copy_from_slice(&request.kv_bucket_count.to_le_bytes());
  body[scalar + 40..scalar + 44].copy_from_slice(&request.kv_slots_per_bucket.to_le_bytes());
  let path_length = path.len() as u32;
  body[scalar + 44..scalar + 48].copy_from_slice(&path_length.to_le_bytes());
  body[scalar + 48..scalar + 64].copy_from_slice(&request.workspace_id);
  body[scalar + 64..scalar + 96].copy_from_slice(&request.workspace_manifest_digest);
  copy_exact_hash(
    &mut body[scalar + 96..scalar + 96 + hash_width],
    request.mutation_journal_head,
    hash_width,
    "mark mutation journal head",
  )?;
  body[scalar + 96 + hash_width..scalar + 104 + hash_width].copy_from_slice(&request.checkpoint_logical_work.to_le_bytes());
  body[scalar + 104 + hash_width..scalar + 112 + hash_width].copy_from_slice(&request.total_logical_work_hint.to_le_bytes());
  body[236 + 4 * hash_width..].copy_from_slice(path);

  let mut identity = [0u8; 40];
  identity[..16].copy_from_slice(request.database_id);
  identity[16..32].copy_from_slice(request.run_id);
  identity[32..].copy_from_slice(&request.checkpoint_sequence.to_le_bytes());
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::MarkRunCheckpoint,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let _decoded = decode_gc_mark_artifact(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

impl MarkWorkspaceManifestV1<'_> {
  pub fn summary(&self) -> String {
    let mut closure = blake3::Hasher::new();
    for row in &self.descriptors {
      closure.update(&row.digest);
    }
    let closure = closure.finalize().to_hex();
    format!(
      "gc:workspace-manifest:state={}:objects={}:records={}:bytes={}:closure={}",
      self.state,
      self.descriptors.len(),
      self.logical_record_count,
      self.stored_bytes,
      &closure[..16]
    )
  }
}

#[derive(Debug, Clone)]
pub struct MarkWorkspaceObjectV1<'a> {
  pub kind: MarkWorkspaceObjectKindV1,
  pub database_id: &'a [u8],
  pub run_id: &'a [u8],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub ordinal: u64,
  pub logical_record_count: u64,
  body: &'a [u8],
}

impl MarkWorkspaceObjectV1<'_> {
  pub fn summary(&self) -> String {
    format!("gc:workspace-object:{}:ordinal={}:records={}", self.kind.name(), self.ordinal, self.logical_record_count)
  }
}

#[derive(Debug)]
pub struct MarkWorkspaceMutationRecordsV1<'a> {
  records: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
}

impl<'a> Iterator for MarkWorkspaceMutationRecordsV1<'a> {
  type Item = FormatResult<MarkMutationRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.records.next().map(|record| decode_mark_mutation_record(record, self.algorithm))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.records.size_hint()
  }
}

impl ExactSizeIterator for MarkWorkspaceMutationRecordsV1<'_> {}

pub fn mark_workspace_mutation_records_v1(
  complete_object: &[u8],
  algorithm: HashAlgorithm,
) -> FormatResult<MarkWorkspaceMutationRecordsV1<'_>> {
  let object = decode_mark_workspace_object(complete_object, algorithm)?;
  if object.kind != MarkWorkspaceObjectKindV1::Mutation {
    return Err(kind_error("mark_workspace_mutation_kind", "workspace object does not contain mutation records"));
  }
  let records = &object.body[24..];
  let record_length =
    checked_add(40, checked_mul(6, algorithm.hash_length(), "workspace mutation record width")?, "workspace mutation record")?;
  let chunks = records.chunks_exact(record_length);
  let expected_record_count = match usize::try_from(object.logical_record_count) {
    Ok(expected_record_count) => expected_record_count,
    Err(error) => {
      return Err(closure_error(
        "mark_workspace_mutation_records",
        format!("workspace mutation record count does not fit this platform: {error}"),
      ));
    }
  };
  if !chunks.remainder().is_empty() || chunks.len() != expected_record_count {
    return Err(closure_error(
      "mark_workspace_mutation_records",
      "workspace mutation records do not close against the validated object count",
    ));
  }
  Ok(MarkWorkspaceMutationRecordsV1 { records: chunks, algorithm })
}

pub fn decode_gc_mark_artifact(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<GcMarkArtifactV1<'_>> {
  let hinted_kind = u16_at(bytes, 6).ok().and_then(GcArtifactKindV1::from_u16);
  let cap = match hinted_kind {
    Some(GcArtifactKindV1::MarkRunCheckpoint) => MARK_CHECKPOINT_VALUE_MAX,
    Some(GcArtifactKindV1::MarkMutationJournalSegment) => MARK_JOURNAL_MAX,
    _ => super::gc::MAX_GC_ARTIFACT_LENGTH,
  };
  ensure_cap("gc_mark_artifact_length", bytes.len(), cap)?;
  let envelope = decode_gc_artifact_envelope(bytes)?;
  let key = immutable_gc_artifact_key(algorithm, envelope.kind, bytes);
  match envelope.kind {
    GcArtifactKindV1::MarkRunCheckpoint => decode_mark_checkpoint(bytes, algorithm, key).map(Box::new).map(GcMarkArtifactV1::Checkpoint),
    GcArtifactKindV1::MarkMutationJournalSegment => {
      decode_mark_mutation_journal(bytes, algorithm, key).map(GcMarkArtifactV1::MutationJournal)
    }
    _ => Err(kind_error("gc_mark_artifact_kind", format!("{} is not a mark artifact", envelope.kind.name()))),
  }
}

fn decode_mark_checkpoint(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<MarkRunCheckpointV1<'_>> {
  ensure_cap("mark_checkpoint_length", bytes.len(), MARK_CHECKPOINT_VALUE_MAX)?;
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let hash_width = algorithm.hash_length();
  let minimum_body = checked_add(236, checked_mul(4, hash_width, "mark checkpoint hash width")?, "mark checkpoint body")?;
  if artifact.kind != GcArtifactKindV1::MarkRunCheckpoint || artifact.identity.len() != 40 || artifact.body.len() < minimum_body {
    return Err(closure_error("mark_checkpoint_shape", "mark checkpoint identity or body shape is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let run_id = &artifact.identity[16..32];
  let checkpoint_sequence = u64_at(artifact.identity, 32)?;
  if all_zero(database_id) || all_zero(run_id) || checkpoint_sequence == 0 {
    return Err(identity_error("mark_checkpoint_identity", "mark checkpoint database, run, and sequence must be nonzero"));
  }

  let body = artifact.body;
  let flags = u32_at(body, 0)?;
  let state = u16_at(body, 6)?;
  let phase = u16_at(body, 8)?;
  if flags & !3 != 0 {
    return Err(reserved_error("mark_checkpoint_flags", "unknown mark checkpoint flag is set"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(kind_error("mark_checkpoint_version", "mark checkpoint payload version is not 1"));
  }
  if !(1..=5).contains(&state) || !(1..=8).contains(&phase) {
    return Err(kind_error("mark_checkpoint_state_or_phase", "unknown mark state or phase"));
  }
  if u16_at(body, 10)? != 0 {
    return Err(reserved_error("mark_checkpoint_reserved", "mark checkpoint reserve is nonzero"));
  }
  let resumable = flags & 1 != 0;
  let canceled = flags & 2 != 0;
  if canceled != (state == 4) || state == 5 && resumable {
    return Err(closure_error("mark_checkpoint_state_flags", "mark state and workspace/canceled flags disagree"));
  }
  let capabilities = &body[12..44];
  validate_exact_capabilities(capabilities)?;
  let created_at = u64_at(body, 44)?;
  let updated_at = u64_at(body, 52)?;
  if created_at == 0 || updated_at < created_at {
    return Err(closure_error("mark_checkpoint_timestamps", "mark checkpoint timestamps are invalid"));
  }
  let authority_root_set_digest = &body[60..60 + hash_width];
  let semantic_state_digest = &body[60 + hash_width..60 + 2 * hash_width];
  let kv_layout_fingerprint = &body[60 + 2 * hash_width..60 + 3 * hash_width];
  let three_hashes_end = checked_add(60, checked_mul(3, hash_width, "mark checkpoint identity hashes")?, "mark checkpoint hashes")?;
  if body[60..three_hashes_end].chunks(hash_width).any(all_zero) {
    return Err(identity_error("mark_checkpoint_hashes", "mark checkpoint identity hashes must be nonzero"));
  }
  let policy_start = three_hashes_end;
  if body[policy_start..policy_start + 64].chunks(32).any(all_zero) {
    return Err(identity_error("mark_checkpoint_policy", "mark checkpoint policy fingerprints must be nonzero"));
  }
  let effective_policy_fingerprint = &body[policy_start..policy_start + 32];
  let system_family_registry_fingerprint = &body[policy_start + 32..policy_start + 64];
  let scalar = 124 + 3 * hash_width;
  let captured_header_sequence = u64_at(body, scalar)?;
  let captured_write_high_water = u64_at(body, scalar + 8)?;
  let reconciled_through_sequence = u64_at(body, scalar + 16)?;
  let active_bitmap_bit_count = u64_at(body, scalar + 24)?;
  let kv_bucket_count = u64_at(body, scalar + 32)?;
  let kv_slots_per_bucket = u32_at(body, scalar + 40)?;
  let path_length = usize::try_from(u32_at(body, scalar + 44)?).map_err(|_| overflow_error("mark checkpoint path length"))?;
  if captured_header_sequence == 0
    || reconciled_through_sequence > captured_write_high_water
    || active_bitmap_bit_count == 0
    || kv_bucket_count == 0
    || kv_slots_per_bucket == 0
    || kv_bucket_count.checked_mul(u64::from(kv_slots_per_bucket)) != Some(active_bitmap_bit_count)
    || path_length == 0
  {
    return Err(closure_error("mark_checkpoint_counts", "mark checkpoint counts or bitmap geometry are invalid"));
  }
  let expected_body = checked_add(minimum_body, path_length, "mark checkpoint path")?;
  if expected_body != body.len() {
    return Err(trailing_error("mark_checkpoint_length", "mark checkpoint path length does not close the body"));
  }
  let workspace_id = &body[scalar + 48..scalar + 64];
  let manifest_digest = &body[scalar + 64..scalar + 96];
  let journal_head_start = scalar + 96;
  let mutation_journal_head = &body[journal_head_start..journal_head_start + hash_width];
  if all_zero(workspace_id) || all_zero(manifest_digest) {
    return Err(identity_error("mark_checkpoint_workspace", "workspace ID and manifest digest must be nonzero"));
  }
  let checkpoint_logical_work = u64_at(body, journal_head_start + hash_width)?;
  let total_logical_work_hint = u64_at(body, journal_head_start + hash_width + 8)?;
  if checkpoint_logical_work > total_logical_work_hint {
    return Err(closure_error("mark_checkpoint_work", "completed logical work exceeds its total hint"));
  }
  let path_bytes = &body[minimum_body..];
  let workspace_path = std::str::from_utf8(path_bytes).map_err(|_| path_error("mark_checkpoint_path", "workspace path is not UTF-8"))?;
  if !canonical_workspace_path(workspace_path) {
    return Err(path_error("mark_checkpoint_path", "workspace path is not a canonical absolute native path"));
  }
  Ok(MarkRunCheckpointV1 {
    database_id,
    run_id,
    generation: artifact.generation,
    checkpoint_sequence,
    state,
    phase,
    resumable,
    canceled,
    capabilities,
    started_at_ms: created_at,
    updated_at_ms: updated_at,
    authority_root_set_digest,
    semantic_state_digest,
    kv_layout_fingerprint,
    effective_policy_fingerprint,
    system_family_registry_fingerprint,
    captured_header_sequence,
    captured_write_high_water,
    reconciled_through_sequence,
    active_bitmap_bit_count,
    kv_bucket_count,
    kv_slots_per_bucket,
    workspace_path,
    workspace_id,
    workspace_manifest_digest: manifest_digest,
    mutation_journal_head,
    checkpoint_logical_work,
    total_logical_work_hint,
    key,
  })
}

fn decode_mark_mutation_journal(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<MarkMutationJournalV1<'_>> {
  ensure_cap("mark_mutation_journal_length", bytes.len(), MARK_JOURNAL_MAX)?;
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let hash_width = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::MarkMutationJournalSegment || artifact.identity.len() != 40 || artifact.body.len() < 32 + hash_width
  {
    return Err(closure_error("mark_mutation_journal_shape", "mark mutation journal identity or body shape is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let run_id = &artifact.identity[16..32];
  let segment_sequence = u64_at(artifact.identity, 32)?;
  if all_zero(database_id) || all_zero(run_id) || segment_sequence == 0 {
    return Err(identity_error("mark_mutation_journal_identity", "mark mutation journal identity must be nonzero"));
  }
  let body = artifact.body;
  let flags = u32_at(body, 0)?;
  if flags & !1 != 0 {
    return Err(reserved_error("mark_mutation_journal_flags", "unknown mark journal flag is set"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(kind_error("mark_mutation_journal_version", "mark mutation journal payload version is not 1"));
  }
  if u16_at(body, 6)? != 0 {
    return Err(reserved_error("mark_mutation_journal_reserved", "mark mutation journal reserve is nonzero"));
  }
  let reset = flags & 1 != 0;
  let first_sequence = u64_at(body, 8)?;
  let last_sequence = u64_at(body, 16)?;
  let record_count = u32_at(body, 24)?;
  let records_length = usize::try_from(u32_at(body, 28)?).map_err(|_| overflow_error("mark journal record length"))?;
  let predecessor = &body[32..32 + hash_width];
  if first_sequence == 0
    || first_sequence > last_sequence
    || record_count == 0
    || reset != all_zero(predecessor)
    || checked_add(32 + hash_width, records_length, "mark journal records")? != body.len()
  {
    return Err(closure_error("mark_mutation_journal_header", "mark journal range, predecessor, count, or length is invalid"));
  }
  let record_length = checked_add(40, checked_mul(6, hash_width, "mark mutation record hashes")?, "mark mutation record")?;
  let records = &body[32 + hash_width..];
  if (record_count as usize).checked_mul(record_length) != Some(records.len()) {
    return Err(trailing_error("mark_mutation_record_count", "mark mutation record count does not match its byte range"));
  }
  let mut first_observed = None;
  let mut first_mutation_id = None;
  let mut previous_cursor: Option<(u64, &[u8])> = None;
  for encoded in records.chunks_exact(record_length) {
    let record = decode_mark_mutation_record(encoded, algorithm)?;
    let cursor = (record.publication_sequence, record.mutation_id);
    if previous_cursor.is_some_and(|previous| cursor <= previous) {
      return Err(order_error("mark_mutation_record_order", "mark mutation records are duplicate or out of order"));
    }
    first_observed.get_or_insert(record.publication_sequence);
    first_mutation_id.get_or_insert(record.mutation_id);
    previous_cursor = Some(cursor);
  }
  let Some((last_observed, last_mutation_id)) = previous_cursor else {
    return Err(closure_error("mark_mutation_journal_bounds", "mark mutation journal contains no records"));
  };
  if first_observed != Some(first_sequence) || last_observed != last_sequence {
    return Err(closure_error("mark_mutation_journal_bounds", "declared mutation range does not match records"));
  }
  Ok(MarkMutationJournalV1 {
    database_id,
    run_id,
    generation: artifact.generation,
    segment_sequence,
    reset,
    predecessor,
    record_count,
    first_sequence,
    last_sequence,
    first_mutation_id: first_mutation_id.ok_or_else(|| closure_error("mark_mutation_journal_bounds", "first mutation ID is absent"))?,
    last_mutation_id,
    records,
    key,
  })
}

fn validate_mutation_payload(payload: &[u8], algorithm: HashAlgorithm) -> FormatResult<u64> {
  Ok(decode_mark_mutation_payload(payload, payload, algorithm)?.publication_sequence)
}

fn decode_mark_mutation_record(record: &[u8], algorithm: HashAlgorithm) -> FormatResult<MarkMutationRecordV1<'_>> {
  let payload_length = u32_at(record, 0)? as usize;
  if payload_length.checked_add(4) != Some(record.len()) {
    return Err(trailing_error("mark_mutation_record_length", "mark mutation record framing is invalid"));
  }
  decode_mark_mutation_payload(&record[4..], record, algorithm)
}

fn decode_mark_mutation_payload<'a>(
  payload: &'a [u8],
  encoded: &'a [u8],
  algorithm: HashAlgorithm,
) -> FormatResult<MarkMutationRecordV1<'a>> {
  let hash_width = algorithm.hash_length();
  let expected = checked_add(36, checked_mul(6, hash_width, "mark mutation hashes")?, "mark mutation payload")?;
  if payload.len() != expected {
    return Err(trailing_error("mark_mutation_payload_length", "mark mutation payload has wrong length"));
  }
  if payload[8..8 + 4 * hash_width].chunks(hash_width).any(all_zero) {
    return Err(identity_error("mark_mutation_payload_hashes", "mark mutation identity hashes must be nonzero"));
  }
  let publication_sequence = u64_at(payload, 0)?;
  let mutation_id = &payload[8..8 + hash_width];
  let root_before = &payload[8 + hash_width..8 + 2 * hash_width];
  let root_after = &payload[8 + 2 * hash_width..8 + 3 * hash_width];
  let published_logical_key = &payload[8 + 3 * hash_width..8 + 4 * hash_width];
  let physical_end = 32 + 6 * hash_width;
  let new_incarnation_bytes = &payload[8 + 4 * hash_width..physical_end];
  let new_incarnation = decode_physical_incarnation(new_incarnation_bytes, algorithm)?;
  let operation = MarkMutationOperationV1::from_u16(u16_at(payload, physical_end)?)
    .ok_or_else(|| kind_error("mark_mutation_operation", "unknown mark mutation operation"))?;
  if publication_sequence == 0 {
    return Err(identity_error("mark_mutation_sequence", "mark mutation sequence is zero"));
  }
  if u16_at(payload, physical_end + 2)? != 0 {
    return Err(reserved_error("mark_mutation_reserved", "mark mutation reserve is nonzero"));
  }
  Ok(MarkMutationRecordV1 {
    encoded,
    publication_sequence,
    mutation_id,
    root_before,
    root_after,
    published_logical_key,
    new_incarnation_bytes,
    new_incarnation,
    operation,
  })
}

pub(crate) fn encode_mark_mutation_record(
  destination: &mut Vec<u8>,
  record: MarkMutationRecordWriteV1<'_>,
  algorithm: HashAlgorithm,
) -> FormatResult<()> {
  let hash_width = algorithm.hash_length();
  if record.publication_sequence == 0
    || [record.mutation_id, record.root_before, record.root_after, record.published_logical_key]
      .iter()
      .any(|hash| hash.len() != hash_width || all_zero(hash))
  {
    return Err(identity_error(
      "mark_mutation_record_write_identity",
      "mark mutation sequence and hash identities must be nonzero and match the selected width",
    ));
  }
  decode_physical_incarnation(record.new_incarnation, algorithm)?;
  let payload_length = checked_add(36, checked_mul(6, hash_width, "mark mutation hashes")?, "mark mutation payload")?;
  let payload_length_u32 = payload_length as u32;
  destination.extend_from_slice(&payload_length_u32.to_le_bytes());
  destination.extend_from_slice(&record.publication_sequence.to_le_bytes());
  destination.extend_from_slice(record.mutation_id);
  destination.extend_from_slice(record.root_before);
  destination.extend_from_slice(record.root_after);
  destination.extend_from_slice(record.published_logical_key);
  destination.extend_from_slice(record.new_incarnation);
  destination.extend_from_slice(&(record.operation as u16).to_le_bytes());
  destination.extend_from_slice(&0u16.to_le_bytes());
  Ok(())
}

pub fn decode_mark_workspace_manifest(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<MarkWorkspaceManifestV1<'_>> {
  ensure_cap("mark_workspace_manifest_length", bytes.len(), WORKSPACE_MANIFEST_MAX)?;
  let hash_width = algorithm.hash_length();
  let fixed_end = checked_add(120, checked_mul(2, hash_width, "workspace manifest hash width")?, "workspace manifest header")?;
  if bytes.len() < fixed_end + 4 {
    return Err(trailing_error("mark_workspace_manifest_length", "workspace manifest is shorter than fixed framing"));
  }
  verify_workspace_crc(bytes)?;
  if &bytes[..4] != b"AGCW" || u16_at(bytes, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "mark_workspace_manifest_envelope", "expected AGCW schema version 1"));
  }
  if usize::try_from(u64_at(bytes, 8)?).map_err(|_| overflow_error("workspace complete length"))? != bytes.len() {
    return Err(trailing_error("mark_workspace_manifest_complete_length", "workspace complete length mismatch"));
  }
  let state = u16_at(bytes, 6)?;
  if !(1..=5).contains(&state) {
    return Err(kind_error("mark_workspace_manifest_state", "unknown workspace checkpoint state"));
  }
  let database_id = &bytes[16..32];
  let run_id = &bytes[32..48];
  let generation = u64_at(bytes, 48)?;
  let checkpoint_sequence = u64_at(bytes, 56)?;
  if all_zero(database_id) || all_zero(run_id) || generation == 0 || checkpoint_sequence == 0 {
    return Err(identity_error("mark_workspace_manifest_identity", "workspace identity must be nonzero"));
  }
  let created_at_ms = u64_at(bytes, 64)?;
  let updated_at_ms = u64_at(bytes, 72)?;
  if created_at_ms == 0 || updated_at_ms < created_at_ms {
    return Err(closure_error("mark_workspace_manifest_timestamps", "workspace timestamps are invalid"));
  }
  let encoded_algorithm = HashAlgorithm::from_u16(u16_at(bytes, 80)?)
    .ok_or_else(|| kind_error("mark_workspace_manifest_hash_algorithm", "workspace hash algorithm is unknown"))?;
  if encoded_algorithm != algorithm {
    return Err(closure_error("mark_workspace_manifest_hash_algorithm", "workspace hash algorithm does not match database"));
  }
  if u32_at(bytes, 84)? != 0 {
    return Err(reserved_error("mark_workspace_manifest_flags", "workspace manifest flags must be zero"));
  }
  let kv_layout_fingerprint = &bytes[88..88 + hash_width];
  let authority_root_set_digest = &bytes[88 + hash_width..88 + 2 * hash_width];
  let effective_policy_fingerprint = &bytes[88 + 2 * hash_width..fixed_end];
  if all_zero(kv_layout_fingerprint) || all_zero(authority_root_set_digest) || all_zero(effective_policy_fingerprint) {
    return Err(identity_error("mark_workspace_manifest_fingerprints", "workspace resume identities must be nonzero"));
  }

  let count = usize::from(u16_at(bytes, 82)?);
  let minimum_descriptor_bytes = checked_mul(count, 68, "workspace descriptors")?;
  if minimum_descriptor_bytes > bytes.len() - fixed_end - 4 {
    return Err(trailing_error("mark_workspace_descriptor_count", "workspace descriptor count exceeds remaining bytes"));
  }
  let mut descriptors = Vec::with_capacity(count);
  let mut stored_bytes = 0u64;
  let mut total_logical_record_count = 0u64;
  let end = bytes.len() - 4;
  let mut cursor = fixed_end;
  for _ in 0..count {
    let fixed_descriptor_end = checked_add(cursor, 68, "workspace descriptor header")?;
    if fixed_descriptor_end > end {
      return Err(trailing_error("mark_workspace_descriptor_length", "workspace descriptor header is truncated"));
    }
    let kind = MarkWorkspaceObjectKindV1::from_u16(u16_at(bytes, cursor)?)
      .ok_or_else(|| kind_error("mark_workspace_descriptor_kind", "unknown workspace object kind"))?;
    if u16_at(bytes, cursor + 2)? != 0 || u32_at(bytes, cursor + 64)? != 0 {
      return Err(reserved_error("mark_workspace_descriptor_reserved", "workspace descriptor reserve is nonzero"));
    }
    let ordinal = u64_at(bytes, cursor + 4)?;
    let stored_length = u64_at(bytes, cursor + 12)?;
    let logical_record_count = u64_at(bytes, cursor + 20)?;
    let digest: [u8; 32] = bytes[cursor + 28..cursor + 60]
      .try_into()
      .map_err(|_| trailing_error("mark_workspace_descriptor_digest", "workspace descriptor digest is truncated"))?;
    let name_length = usize::try_from(u32_at(bytes, cursor + 60)?).map_err(|_| overflow_error("workspace descriptor name"))?;
    if ordinal == 0
      || stored_length < (WORKSPACE_OBJECT_HEADER + 4) as u64
      || logical_record_count == 0
      || all_zero(&digest)
      || name_length == 0
    {
      return Err(identity_error("mark_workspace_descriptor_fields", "workspace descriptor identity/count fields are invalid"));
    }
    if name_length > MAX_WORKSPACE_NAME {
      return Err(amplification_error("mark_workspace_descriptor_name", name_length, MAX_WORKSPACE_NAME));
    }
    let descriptor_end = checked_add(fixed_descriptor_end, name_length, "workspace descriptor name")?;
    let name_bytes = bytes
      .get(fixed_descriptor_end..descriptor_end)
      .ok_or_else(|| trailing_error("mark_workspace_descriptor_name", "workspace descriptor name is truncated"))?;
    let name = std::str::from_utf8(name_bytes)
      .map_err(|_| path_error("mark_workspace_descriptor_name", "workspace descriptor name is not UTF-8"))?;
    if !canonical_relative_name(name) {
      return Err(path_error("mark_workspace_descriptor_name", "workspace descriptor name is not canonical and relative"));
    }
    let descriptor = MarkWorkspaceDescriptorV1 { kind, ordinal, stored_length, logical_record_count, digest, name };
    if descriptors.last().is_some_and(|prior| descriptor_cmp(prior, &descriptor) != Ordering::Less) {
      return Err(order_error("mark_workspace_descriptor_order", "workspace descriptors are duplicate or out of order"));
    }
    stored_bytes = stored_bytes.checked_add(stored_length).ok_or_else(|| overflow_error("workspace stored byte total"))?;
    total_logical_record_count = total_logical_record_count
      .checked_add(descriptor.logical_record_count)
      .ok_or_else(|| overflow_error("workspace logical record total"))?;
    descriptors.push(descriptor);
    cursor = descriptor_end;
  }
  if cursor != end {
    return Err(trailing_error("mark_workspace_manifest_trailing", "workspace descriptors do not consume the manifest"));
  }
  Ok(MarkWorkspaceManifestV1 {
    state,
    database_id,
    run_id,
    generation,
    checkpoint_sequence,
    created_at_ms,
    updated_at_ms,
    hash_algorithm: encoded_algorithm,
    kv_layout_fingerprint,
    authority_root_set_digest,
    effective_policy_fingerprint,
    stored_bytes,
    logical_record_count: total_logical_record_count,
    descriptors,
  })
}

pub fn decode_mark_workspace_object(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<MarkWorkspaceObjectV1<'_>> {
  ensure_cap("mark_workspace_object_length", bytes.len(), WORKSPACE_OBJECT_MAX)?;
  if bytes.len() < WORKSPACE_OBJECT_HEADER + 4 {
    return Err(trailing_error("mark_workspace_object_length", "workspace object is shorter than fixed framing"));
  }
  verify_workspace_crc(bytes)?;
  if &bytes[..4] != b"AGWO" || u16_at(bytes, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "mark_workspace_object_envelope", "expected AGWO schema version 1"));
  }
  let kind = MarkWorkspaceObjectKindV1::from_u16(u16_at(bytes, 6)?)
    .ok_or_else(|| kind_error("mark_workspace_object_kind", "unknown workspace object kind"))?;
  if usize::try_from(u64_at(bytes, 8)?).map_err(|_| overflow_error("workspace object complete length"))? != bytes.len() {
    return Err(trailing_error("mark_workspace_object_complete_length", "workspace object complete length mismatch"));
  }
  let database_id = &bytes[16..32];
  let run_id = &bytes[32..48];
  let generation = u64_at(bytes, 48)?;
  let checkpoint_sequence = u64_at(bytes, 56)?;
  let ordinal = u64_at(bytes, 64)?;
  if all_zero(database_id) || all_zero(run_id) || generation == 0 || checkpoint_sequence == 0 || ordinal == 0 {
    return Err(identity_error("mark_workspace_object_identity", "workspace object identity must be nonzero"));
  }
  let body_length = usize::try_from(u64_at(bytes, 72)?).map_err(|_| overflow_error("workspace object body length"))?;
  if checked_add(WORKSPACE_OBJECT_HEADER, checked_add(body_length, 4, "workspace object CRC")?, "workspace object body")? != bytes.len() {
    return Err(trailing_error("mark_workspace_object_body_length", "workspace object body length does not close"));
  }
  let body = &bytes[WORKSPACE_OBJECT_HEADER..WORKSPACE_OBJECT_HEADER + body_length];
  let logical_record_count = validate_mark_workspace_body(body, kind, generation, algorithm)?;
  Ok(MarkWorkspaceObjectV1 { kind, database_id, run_id, generation, checkpoint_sequence, ordinal, logical_record_count, body })
}

pub fn validate_mark_workspace_object(
  manifest: &MarkWorkspaceManifestV1<'_>,
  descriptor: &MarkWorkspaceDescriptorV1<'_>,
  object: &MarkWorkspaceObjectV1<'_>,
  complete_object: &[u8],
) -> FormatResult<()> {
  let stored_length = u64::try_from(complete_object.len()).map_err(|_| overflow_error("workspace object stored length"))?;
  if manifest.database_id != object.database_id
    || manifest.run_id != object.run_id
    || manifest.generation != object.generation
    || manifest.checkpoint_sequence != object.checkpoint_sequence
    || descriptor.kind != object.kind
    || descriptor.ordinal != object.ordinal
    || descriptor.stored_length != stored_length
    || descriptor.logical_record_count != object.logical_record_count
    || descriptor.digest != *blake3::hash(complete_object).as_bytes()
  {
    return Err(closure_error("mark_workspace_object_closure", "workspace object does not match its manifest descriptor"));
  }
  Ok(())
}

pub fn validate_mark_checkpoint_workspace(
  checkpoint: &MarkRunCheckpointV1<'_>,
  manifest: &MarkWorkspaceManifestV1<'_>,
  complete_manifest: &[u8],
) -> FormatResult<()> {
  if checkpoint.database_id != manifest.database_id
    || checkpoint.run_id != manifest.run_id
    || checkpoint.generation != manifest.generation
    || checkpoint.checkpoint_sequence != manifest.checkpoint_sequence
    || checkpoint.state != manifest.state
    || checkpoint.workspace_manifest_digest != blake3::hash(complete_manifest).as_bytes()
  {
    return Err(closure_error(
      "mark_checkpoint_workspace_closure",
      "mark checkpoint identity/state/digest does not match its workspace manifest",
    ));
  }
  Ok(())
}

pub fn validate_mark_resume_context(
  checkpoint: &MarkRunCheckpointV1<'_>,
  manifest: &MarkWorkspaceManifestV1<'_>,
  complete_manifest: &[u8],
  context: &MarkResumeContextV1<'_>,
) -> FormatResult<()> {
  validate_mark_checkpoint_resume_context(checkpoint, context)?;
  validate_mark_checkpoint_workspace(checkpoint, manifest, complete_manifest)?;
  if context.hash_algorithm != manifest.hash_algorithm
    || manifest.created_at_ms != checkpoint.started_at_ms
    || manifest.updated_at_ms > checkpoint.updated_at_ms
    || manifest.kv_layout_fingerprint != checkpoint.kv_layout_fingerprint
    || manifest.authority_root_set_digest != checkpoint.authority_root_set_digest
    || manifest.effective_policy_fingerprint != checkpoint.effective_policy_fingerprint
  {
    return Err(closure_error(
      "mark_resume_manifest_basis",
      "workspace manifest timestamps or resume fingerprints do not close against the selected checkpoint",
    ));
  }
  Ok(())
}

pub fn validate_mark_checkpoint_resume_context(
  checkpoint: &MarkRunCheckpointV1<'_>,
  context: &MarkResumeContextV1<'_>,
) -> FormatResult<()> {
  if !checkpoint.resumable || checkpoint.canceled || checkpoint.state > 3 {
    return Err(closure_error("mark_resume_state", "selected mark checkpoint is not resumable"));
  }
  if checkpoint.database_id != context.database_id
    || checkpoint.run_id != context.run_id
    || checkpoint.generation != context.generation
    || checkpoint.checkpoint_sequence != context.checkpoint_sequence
    || checkpoint.workspace_path != context.workspace_path
    || checkpoint.workspace_id != context.workspace_id
    || checkpoint.authority_root_set_digest != context.authority_root_set_digest
    || checkpoint.semantic_state_digest != context.semantic_state_digest
    || checkpoint.kv_layout_fingerprint != context.kv_layout_fingerprint
    || checkpoint.effective_policy_fingerprint != context.effective_policy_fingerprint
    || checkpoint.system_family_registry_fingerprint != context.system_family_registry_fingerprint
    || checkpoint.captured_header_sequence != context.captured_header_sequence
    || checkpoint.captured_write_high_water != context.captured_write_high_water
    || checkpoint.reconciled_through_sequence != context.reconciled_through_sequence
    || checkpoint.active_bitmap_bit_count != context.active_bitmap_bit_count
    || checkpoint.kv_bucket_count != context.kv_bucket_count
    || checkpoint.kv_slots_per_bucket != context.kv_slots_per_bucket
  {
    return Err(closure_error("mark_resume_context", "mark checkpoint does not match the exact captured resume context"));
  }
  Ok(())
}

pub fn validate_mark_workspace_body(
  body: &[u8],
  kind: MarkWorkspaceObjectKindV1,
  generation: u64,
  algorithm: HashAlgorithm,
) -> FormatResult<u64> {
  if kind == MarkWorkspaceObjectKindV1::Bitmap {
    if body.len() < 32 {
      return Err(trailing_error("mark_workspace_bitmap_header", "bitmap body is truncated"));
    }
    if u32_at(body, 0)? != 0 || u16_at(body, 6)? != 0 {
      return Err(reserved_error("mark_workspace_bitmap_reserved", "bitmap flags/reserve must be zero"));
    }
    if u16_at(body, 4)? != 1 {
      return Err(kind_error("mark_workspace_bitmap_codec", "bitmap codec is not 1"));
    }
    let start = u64_at(body, 8)?;
    let bit_count = u64_at(body, 16)?;
    let byte_count_u64 = u64_at(body, 24)?;
    let expected_u64 = bit_count.checked_add(7).ok_or_else(|| overflow_error("workspace bitmap bit range"))? / 8;
    let byte_count = usize::try_from(byte_count_u64).map_err(|_| overflow_error("workspace bitmap byte count"))?;
    if bit_count == 0
      || start.checked_add(bit_count).is_none()
      || byte_count_u64 != expected_u64
      || checked_add(32, byte_count, "workspace bitmap bytes")? != body.len()
    {
      return Err(closure_error("mark_workspace_bitmap_fields", "bitmap range, count, or length is invalid"));
    }
    let remainder = bit_count % 8;
    if remainder != 0 && body.last().is_some_and(|last| last & !((1u8 << remainder) - 1) != 0) {
      return Err(closure_error("mark_workspace_bitmap_unused_bits", "unused bitmap bits must be zero"));
    }
    return Ok(bit_count);
  }

  if body.len() < 24 {
    return Err(trailing_error("mark_workspace_run_header", "workspace run body is truncated"));
  }
  if u32_at(body, 0)? != 0 || u16_at(body, 6)? != 0 {
    return Err(reserved_error("mark_workspace_run_reserved", "workspace run flags/reserve must be zero"));
  }
  if u16_at(body, 4)? != kind as u16 - 1 {
    return Err(kind_error("mark_workspace_run_codec", "workspace run codec does not match object kind"));
  }
  let count = u32_at(body, 8)?;
  let records_length = usize::try_from(u32_at(body, 12)?).map_err(|_| overflow_error("workspace records length"))?;
  if u64_at(body, 16)? != generation {
    return Err(identity_error("mark_workspace_run_generation", "workspace body generation does not match object"));
  }
  if count == 0 || checked_add(24, records_length, "workspace records")? != body.len() {
    return Err(closure_error("mark_workspace_run_length", "workspace record count or length is invalid"));
  }
  let minimum_record = minimum_workspace_record_length(kind, algorithm)?;
  if usize::try_from(count).map_err(|_| overflow_error("workspace record count"))? > records_length / minimum_record.max(1) {
    return Err(trailing_error("mark_workspace_record_count", "workspace record count exceeds available bytes"));
  }
  let mut cursor = 24;
  let mut previous: Option<RecordOrderKey<'_>> = None;
  for _ in 0..count {
    let framed_length = usize::try_from(u32_at(body, cursor)?).map_err(|_| overflow_error("workspace record length"))?;
    let record_length =
      if kind == MarkWorkspaceObjectKindV1::Mutation { checked_add(framed_length, 4, "workspace mutation frame")? } else { framed_length };
    if framed_length == 0 || framed_length > MAX_WORKSPACE_RECORD || record_length < 4 {
      return Err(amplification_error("mark_workspace_record_length", framed_length, MAX_WORKSPACE_RECORD));
    }
    let end = checked_add(cursor, record_length, "workspace record")?;
    let record = body.get(cursor..end).ok_or_else(|| trailing_error("mark_workspace_record_length", "workspace record is truncated"))?;
    let key = validate_workspace_record(record, kind, algorithm)?;
    if previous.as_ref().is_some_and(|prior| prior.cmp(&key) != Ordering::Less) {
      return Err(order_error("mark_workspace_record_order", "workspace records are duplicate or out of order"));
    }
    previous = Some(key);
    cursor = end;
  }
  if cursor != body.len() {
    return Err(trailing_error("mark_workspace_record_trailing", "workspace records do not consume the body"));
  }
  Ok(u64::from(count))
}

#[derive(Clone, Copy)]
enum RecordOrderKey<'a> {
  Bytes(&'a [u8]),
  Mutation(u64, &'a [u8]),
  Diagnostic(&'a [u8], &'a [u8], &'a [u8], &'a [u8]),
}

impl RecordOrderKey<'_> {
  fn cmp(&self, other: &Self) -> Ordering {
    match (self, other) {
      (Self::Bytes(left), Self::Bytes(right)) => left.cmp(right),
      (Self::Mutation(left_sequence, left_id), Self::Mutation(right_sequence, right_id)) => {
        (left_sequence, left_id).cmp(&(right_sequence, right_id))
      }
      (
        Self::Diagnostic(left_time, left_class, left_key, left_context),
        Self::Diagnostic(right_time, right_class, right_key, right_context),
      ) => (left_time, left_class, left_key, left_context).cmp(&(right_time, right_class, right_key, right_context)),
      (left, right) => left.rank().cmp(&right.rank()),
    }
  }

  fn rank(self) -> u8 {
    match self {
      Self::Bytes(_) => 1,
      Self::Mutation(_, _) => 2,
      Self::Diagnostic(_, _, _, _) => 3,
    }
  }
}

fn validate_workspace_record<'a>(
  record: &'a [u8],
  kind: MarkWorkspaceObjectKindV1,
  algorithm: HashAlgorithm,
) -> FormatResult<RecordOrderKey<'a>> {
  let hash_width = algorithm.hash_length();
  match kind {
    MarkWorkspaceObjectKindV1::Bitmap => Err(kind_error("mark_workspace_record_kind", "bitmap objects do not contain framed records")),
    MarkWorkspaceObjectKindV1::Frontier => {
      let expected = checked_add(36, checked_mul(4, hash_width, "workspace frontier hashes")?, "workspace frontier record")?;
      let declared_length = usize::try_from(u32_at(record, 0)?).map_err(|_| overflow_error("frontier record length"))?;
      if record.len() != expected || declared_length != record.len() {
        return Err(trailing_error("mark_workspace_frontier_length", "frontier record has wrong fixed length"));
      }
      let record_kind = u16_at(record, 4)?;
      let flags = u16_at(record, 6)?;
      let family = u16_at(record, 8)?;
      if !(1..=4).contains(&record_kind) {
        return Err(kind_error("mark_workspace_frontier_kind", "unknown frontier record kind"));
      }
      if flags & !3 != 0 || u16_at(record, 10)? != 0 {
        return Err(reserved_error("mark_workspace_frontier_flags", "frontier flags/reserve are invalid"));
      }
      let object_hash = &record[12..12 + hash_width];
      let path_hash = &record[12 + hash_width..12 + 2 * hash_width];
      let physical = &record[12 + 2 * hash_width..];
      if (record_kind == 3) != (family != 0)
        || all_zero(object_hash)
        || (flags & 1 != 0) != !all_zero(path_hash)
        || (flags & 2 != 0) != !all_zero(physical)
      {
        return Err(closure_error("mark_workspace_frontier_fields", "frontier identity and presence fields disagree"));
      }
      if flags & 2 != 0 {
        decode_physical_incarnation(physical, algorithm)?;
      }
      Ok(RecordOrderKey::Bytes(&record[4..]))
    }
    MarkWorkspaceObjectKindV1::PathVisit => {
      let expected = checked_add(8, checked_mul(2, hash_width, "workspace path hashes")?, "workspace path record")?;
      let declared_length = usize::try_from(u32_at(record, 0)?).map_err(|_| overflow_error("path-visit record length"))?;
      if record.len() != expected
        || declared_length != record.len()
        || u32_at(record, 4)? != 0
        || all_zero(&record[8..8 + hash_width])
        || all_zero(&record[8 + hash_width..])
      {
        return Err(closure_error("mark_workspace_path_visit_fields", "path-visit record fields are invalid"));
      }
      Ok(RecordOrderKey::Bytes(&record[8..]))
    }
    MarkWorkspaceObjectKindV1::Mutation => {
      let payload_length = usize::try_from(u32_at(record, 0)?).map_err(|_| overflow_error("workspace mutation record length"))?;
      if record.len() < 4 || payload_length.checked_add(4) != Some(record.len()) {
        return Err(trailing_error("mark_workspace_mutation_length", "mutation record framing is invalid"));
      }
      let payload = &record[4..];
      let sequence = validate_mutation_payload(payload, algorithm)?;
      Ok(RecordOrderKey::Mutation(sequence, &payload[8..8 + hash_width]))
    }
    MarkWorkspaceObjectKindV1::Candidate => {
      let expected = checked_add(32, checked_mul(2, hash_width, "workspace candidate hashes")?, "workspace candidate record")?;
      let declared_length = usize::try_from(u32_at(record, 0)?).map_err(|_| overflow_error("candidate record length"))?;
      if record.len() != expected || declared_length != record.len() {
        return Err(trailing_error("mark_workspace_candidate_length", "candidate record has wrong fixed length"));
      }
      if u16_at(record, 4)? != 0 {
        return Err(reserved_error("mark_workspace_candidate_reserved", "candidate reserve is nonzero"));
      }
      if !(1..=7).contains(&u16_at(record, 6)?) {
        return Err(kind_error("mark_workspace_candidate_class", "unknown candidate class"));
      }
      decode_physical_incarnation(&record[8..], algorithm)?;
      Ok(RecordOrderKey::Bytes(&record[8..]))
    }
    MarkWorkspaceObjectKindV1::Diagnostic => {
      let fixed = checked_add(32, hash_width, "workspace diagnostic record")?;
      let declared_length = usize::try_from(u32_at(record, 0)?).map_err(|_| overflow_error("diagnostic record length"))?;
      if record.len() < fixed || declared_length != record.len() {
        return Err(trailing_error("mark_workspace_diagnostic_length", "diagnostic record is truncated"));
      }
      let error_class = u16_at(record, 4)?;
      if !(1..=10).contains(&error_class) || !(1..=3).contains(&record[6]) {
        return Err(kind_error("mark_workspace_diagnostic_kind", "unknown diagnostic class or severity"));
      }
      if record[7] != 0 || u32_at(record, 28 + hash_width)? != 0 {
        return Err(reserved_error("mark_workspace_diagnostic_reserved", "diagnostic reserve is nonzero"));
      }
      if i64_at(record, 8)? <= 0 || u64_at(record, 16 + hash_width)? == 0 {
        return Err(identity_error("mark_workspace_diagnostic_identity", "diagnostic time or physical offset is invalid"));
      }
      let context_length = usize::try_from(u32_at(record, 24 + hash_width)?).map_err(|_| overflow_error("diagnostic context length"))?;
      if context_length == 0 || context_length > 4_096 {
        return Err(amplification_error("mark_workspace_diagnostic_context", context_length, 4_096));
      }
      if checked_add(fixed, context_length, "diagnostic context")? != record.len() {
        return Err(trailing_error("mark_workspace_diagnostic_context", "diagnostic context length does not close"));
      }
      let context = &record[fixed..];
      let context_text =
        std::str::from_utf8(context).map_err(|_| path_error("mark_workspace_diagnostic_context", "diagnostic context is not UTF-8"))?;
      if context_text.as_bytes().contains(&0) {
        return Err(path_error("mark_workspace_diagnostic_context", "diagnostic context contains NUL"));
      }
      Ok(RecordOrderKey::Diagnostic(&record[8..16], &record[4..6], &record[16..16 + hash_width], context))
    }
  }
}

fn minimum_workspace_record_length(kind: MarkWorkspaceObjectKindV1, algorithm: HashAlgorithm) -> FormatResult<usize> {
  let hash_width = algorithm.hash_length();
  match kind {
    MarkWorkspaceObjectKindV1::Bitmap => Ok(1),
    MarkWorkspaceObjectKindV1::Frontier => checked_add(36, checked_mul(4, hash_width, "frontier width")?, "frontier record"),
    MarkWorkspaceObjectKindV1::PathVisit => checked_add(8, checked_mul(2, hash_width, "path width")?, "path record"),
    MarkWorkspaceObjectKindV1::Mutation => checked_add(40, checked_mul(6, hash_width, "mutation width")?, "mutation record"),
    MarkWorkspaceObjectKindV1::Candidate => checked_add(32, checked_mul(2, hash_width, "candidate width")?, "candidate record"),
    MarkWorkspaceObjectKindV1::Diagnostic => checked_add(33, hash_width, "diagnostic record"),
  }
}

fn validate_exact_capabilities(bytes: &[u8]) -> FormatResult<()> {
  let mut expected = [0u8; 32];
  for bit in MARK_REQUIRED_CAPABILITY_BITS {
    expected[bit / 8] |= 1 << (bit % 8);
  }
  if bytes != expected {
    return Err(closure_error("mark_checkpoint_capabilities", "mark checkpoint capabilities do not match the frozen set"));
  }
  Ok(())
}

fn copy_exact_hash(destination: &mut [u8], source: &[u8], expected_length: usize, field: &'static str) -> FormatResult<()> {
  if destination.len() != expected_length || source.len() != expected_length {
    return Err(identity_error("mark_checkpoint_hash_width", format!("{field} does not match the selected hash width")));
  }
  destination.copy_from_slice(source);
  Ok(())
}

fn descriptor_cmp(left: &MarkWorkspaceDescriptorV1<'_>, right: &MarkWorkspaceDescriptorV1<'_>) -> Ordering {
  (left.kind, left.ordinal, left.name.as_bytes()).cmp(&(right.kind, right.ordinal, right.name.as_bytes()))
}

fn canonical_relative_name(name: &str) -> bool {
  !name.is_empty()
    && !name.starts_with('/')
    && !name.ends_with('/')
    && !name.contains('\\')
    && !name.as_bytes().contains(&0)
    && name.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn canonical_workspace_path(path: &str) -> bool {
  if path.is_empty() || path.contains('\\') || path.as_bytes().contains(&0) || path.len() > 1 && path.ends_with('/') {
    return false;
  }
  let remainder = if let Some(rest) = path.strip_prefix('/') {
    rest
  } else if path.len() >= 3 && path.as_bytes()[0].is_ascii_uppercase() && path.as_bytes()[1] == b':' && path.as_bytes()[2] == b'/' {
    &path[3..]
  } else {
    return false;
  };
  !remainder.is_empty() && remainder.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn verify_workspace_crc(bytes: &[u8]) -> FormatResult<()> {
  let crc_offset = bytes.len().checked_sub(4).ok_or_else(|| trailing_error("mark_workspace_crc", "workspace CRC is truncated"))?;
  if u32_at(bytes, crc_offset)? != crc32fast::hash(&bytes[..crc_offset]) {
    return Err(error(MalformedInputClass::ChecksumOrIntegrityMismatch, "mark_workspace_crc", "workspace CRC does not match"));
  }
  Ok(())
}

fn i64_at(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let raw = bytes.get(offset..offset + 8).ok_or_else(|| trailing_error("mark_workspace_truncated", format!("i64 at offset {offset}")))?;
  Ok(i64::from_le_bytes(raw.try_into().expect("checked mark workspace i64 width")))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn ensure_cap(code: &'static str, actual: usize, cap: usize) -> FormatResult<()> {
  if actual > cap {
    return Err(amplification_error(code, actual, cap));
  }
  Ok(())
}

fn checked_add(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_add(right).ok_or_else(|| overflow_error(context))
}

fn checked_mul(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_mul(right).ok_or_else(|| overflow_error(context))
}

fn amplification_error(code: &'static str, actual: usize, cap: usize) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, code, format!("{actual} exceeds cap {cap}"))
}

fn overflow_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "gc_mark_overflow", context)
}

fn trailing_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, code, context)
}

fn reserved_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, code, context)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}

fn order_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn path_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, code, context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
