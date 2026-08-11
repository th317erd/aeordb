use std::cmp::Ordering;

use super::gc::{GcArtifactKindV1, decode_gc_artifact_envelope, decode_physical_incarnation, immutable_gc_artifact_key, u16_at, u32_at, u64_at};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MARK_CHECKPOINT_VALUE_MAX: usize = 32 + 40 + 256 * 1024 + 4;
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
  pub workspace_path: &'a str,
  pub workspace_manifest_digest: &'a [u8],
  pub mutation_journal_head: &'a [u8],
  pub key: Vec<u8>,
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
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum GcMarkArtifactV1<'a> {
  Checkpoint(MarkRunCheckpointV1<'a>),
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
    || current.first_sequence <= previous.last_sequence
  {
    return Err(closure_error(
      "mark_mutation_journal_chain",
      "mark mutation journal identity, predecessor, ordinal, or sequence range is discontinuous",
    ));
  }
  Ok(())
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
  pub stored_bytes: u64,
  pub logical_record_count: u64,
  pub descriptors: Vec<MarkWorkspaceDescriptorV1<'a>>,
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
}

impl MarkWorkspaceObjectV1<'_> {
  pub fn summary(&self) -> String {
    format!("gc:workspace-object:{}:ordinal={}:records={}", self.kind.name(), self.ordinal, self.logical_record_count)
  }
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
    GcArtifactKindV1::MarkRunCheckpoint => decode_mark_checkpoint(bytes, algorithm, key).map(GcMarkArtifactV1::Checkpoint),
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
  validate_exact_capabilities(&body[12..44])?;
  let created_at = u64_at(body, 44)?;
  let updated_at = u64_at(body, 52)?;
  if created_at == 0 || updated_at < created_at {
    return Err(closure_error("mark_checkpoint_timestamps", "mark checkpoint timestamps are invalid"));
  }
  let three_hashes_end = checked_add(60, checked_mul(3, hash_width, "mark checkpoint identity hashes")?, "mark checkpoint hashes")?;
  if body[60..three_hashes_end].chunks(hash_width).any(all_zero) {
    return Err(identity_error("mark_checkpoint_hashes", "mark checkpoint identity hashes must be nonzero"));
  }
  let policy_start = three_hashes_end;
  if body[policy_start..policy_start + 64].chunks(32).any(all_zero) {
    return Err(identity_error("mark_checkpoint_policy", "mark checkpoint policy fingerprints must be nonzero"));
  }
  let scalar = 124 + 3 * hash_width;
  let authority_root_count = u64_at(body, scalar)?;
  let journal_observed = u64_at(body, scalar + 8)?;
  let journal_applied = u64_at(body, scalar + 16)?;
  let bitmap_bytes = u64_at(body, scalar + 24)?;
  let bitmap_units = u64_at(body, scalar + 32)?;
  let bitmap_unit_bytes = u32_at(body, scalar + 40)?;
  let path_length = usize::try_from(u32_at(body, scalar + 44)?).map_err(|_| overflow_error("mark checkpoint path length"))?;
  if authority_root_count == 0
    || journal_applied > journal_observed
    || bitmap_bytes == 0
    || bitmap_units == 0
    || bitmap_unit_bytes == 0
    || bitmap_units.checked_mul(u64::from(bitmap_unit_bytes)) != Some(bitmap_bytes)
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
  let completed_work = u64_at(body, journal_head_start + hash_width)?;
  let total_work_hint = u64_at(body, journal_head_start + hash_width + 8)?;
  if completed_work > total_work_hint {
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
    workspace_path,
    workspace_manifest_digest: manifest_digest,
    mutation_journal_head,
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
  let payload_length = checked_add(36, checked_mul(6, hash_width, "mark mutation payload hashes")?, "mark mutation payload")?;
  let mut cursor = 32 + hash_width;
  let mut first_observed = None;
  let mut previous_sequence = 0;
  let mut previous_id: Option<&[u8]> = None;
  for _ in 0..record_count {
    let declared = usize::try_from(u32_at(body, cursor)?).map_err(|_| overflow_error("mark mutation payload length"))?;
    cursor = checked_add(cursor, 4, "mark mutation frame")?;
    if declared != payload_length {
      return Err(trailing_error("mark_mutation_record_length", "mark mutation payload has the wrong fixed length"));
    }
    let end = checked_add(cursor, declared, "mark mutation record")?;
    let payload =
      body.get(cursor..end).ok_or_else(|| trailing_error("mark_mutation_record_length", "mark mutation record is truncated"))?;
    let sequence = validate_mutation_payload(payload, algorithm)?;
    let mutation_id = &payload[8..8 + hash_width];
    if previous_id.is_some_and(|id| previous_sequence > sequence || previous_sequence == sequence && id >= mutation_id) {
      return Err(order_error("mark_mutation_record_order", "mark mutation records are duplicate or out of order"));
    }
    first_observed.get_or_insert(sequence);
    previous_sequence = sequence;
    previous_id = Some(mutation_id);
    cursor = end;
  }
  if cursor != body.len() || first_observed != Some(first_sequence) || previous_sequence != last_sequence {
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
    key,
  })
}

fn validate_mutation_payload(payload: &[u8], algorithm: HashAlgorithm) -> FormatResult<u64> {
  let hash_width = algorithm.hash_length();
  let expected = checked_add(36, checked_mul(6, hash_width, "mark mutation hashes")?, "mark mutation payload")?;
  if payload.len() != expected {
    return Err(trailing_error("mark_mutation_payload_length", "mark mutation payload has wrong length"));
  }
  if payload[8..8 + 4 * hash_width].chunks(hash_width).any(all_zero) {
    return Err(identity_error("mark_mutation_payload_hashes", "mark mutation identity hashes must be nonzero"));
  }
  let sequence = u64_at(payload, 0)?;
  let physical_end = 32 + 6 * hash_width;
  decode_physical_incarnation(&payload[8 + 4 * hash_width..physical_end], algorithm)?;
  let operation = u16_at(payload, physical_end)?;
  if sequence == 0 {
    return Err(identity_error("mark_mutation_sequence", "mark mutation sequence is zero"));
  }
  if !(1..=10).contains(&operation) {
    return Err(kind_error("mark_mutation_operation", "unknown mark mutation operation"));
  }
  if u16_at(payload, physical_end + 2)? != 0 {
    return Err(reserved_error("mark_mutation_reserved", "mark mutation reserve is nonzero"));
  }
  Ok(sequence)
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
  if u64_at(bytes, 64)? == 0 || u64_at(bytes, 72)? < u64_at(bytes, 64)? {
    return Err(closure_error("mark_workspace_manifest_timestamps", "workspace timestamps are invalid"));
  }
  if u16_at(bytes, 80)? != algorithm.to_u16() {
    return Err(closure_error("mark_workspace_manifest_hash_algorithm", "workspace hash algorithm does not match database"));
  }
  if u32_at(bytes, 84)? != 0 {
    return Err(reserved_error("mark_workspace_manifest_flags", "workspace manifest flags must be zero"));
  }
  if all_zero(&bytes[88..88 + hash_width])
    || all_zero(&bytes[88 + hash_width..88 + 2 * hash_width])
    || all_zero(&bytes[88 + 2 * hash_width..fixed_end])
  {
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
  Ok(MarkWorkspaceObjectV1 { kind, database_id, run_id, generation, checkpoint_sequence, ordinal, logical_record_count })
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
