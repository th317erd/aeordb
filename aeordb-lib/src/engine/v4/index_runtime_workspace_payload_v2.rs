//! AIRB v2 frozen runtime batches.
//!
//! V2 preserves the ordered-record stream while adding the two semantics that
//! exact manifest publication cannot reconstruct from sparse pages: explicit
//! remove-by-key operations and per-owner/document membership transitions.

use std::cmp::Ordering;

use crate::engine::HashAlgorithm;

use super::index_coordinator::IndexFlushReasonV1;
use super::index_page::{OrderedIndexRoleV1, decode_ordered_record, ordered_record_order_key};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const RUNTIME_BATCH_MAGIC: &[u8; 4] = b"AIRB";
const RUNTIME_BATCH_SCHEMA_VERSION_V2: u16 = 2;
pub const RUNTIME_BATCH_HEADER_LENGTH_V2: usize = 64;
pub const RUNTIME_MUTATION_FRAME_LENGTH_V2: usize = 40;
pub const RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2: usize = 48;
const RUNTIME_LOGICAL_RECORD_LIMIT_V2: usize = 1_048_576;
const RUNTIME_ORDER_KEY_LIMIT_V2: usize = 1_024 * 1_024;
const RUNTIME_ENCODED_RECORD_LIMIT_V2: usize = 4 * 1_024 * 1_024;
const RUNTIME_PAYLOAD_LIMIT_V2: usize = 512 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexWorkspaceMutationOperationV2 {
  Upsert = 1,
  RemoveExisting = 2,
}

impl IndexWorkspaceMutationOperationV2 {
  pub const fn id(self) -> u8 {
    self as u8
  }

  pub const fn from_id(id: u8) -> Option<Self> {
    match id {
      1 => Some(Self::Upsert),
      2 => Some(Self::RemoveExisting),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexWorkspaceOwnerClassV2 {
  ScopeCatalog = 1,
  ValueStore = 2,
  FieldIndex = 3,
}

impl IndexWorkspaceOwnerClassV2 {
  pub const fn id(self) -> u8 {
    self as u8
  }

  pub const fn from_id(id: u8) -> Option<Self> {
    match id {
      1 => Some(Self::ScopeCatalog),
      2 => Some(Self::ValueStore),
      3 => Some(Self::FieldIndex),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceMembershipStateV2 {
  pub live: bool,
  pub unindexable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceRuntimeMutationWriteV2<'a> {
  pub index_id: &'a [u8],
  pub role: OrderedIndexRoleV1,
  pub operation: IndexWorkspaceMutationOperationV2,
  pub publication_sequence: u64,
  pub operation_id: [u8; 16],
  pub order_key: &'a [u8],
  pub encoded_record: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceMembershipTransitionWriteV2<'a> {
  pub owner_id: &'a [u8],
  pub owner_class: IndexWorkspaceOwnerClassV2,
  pub publication_sequence: u64,
  pub operation_id: [u8; 16],
  pub document_ordinal: u64,
  pub before: IndexWorkspaceMembershipStateV2,
  pub after: IndexWorkspaceMembershipStateV2,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexWorkspaceRuntimeBatchWriteV2<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub coordinator_id: [u8; 16],
  pub batch_id: u64,
  pub reason: IndexFlushReasonV1,
  pub mutations: &'a [IndexWorkspaceRuntimeMutationWriteV2<'a>],
  pub transitions: &'a [IndexWorkspaceMembershipTransitionWriteV2<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceRuntimeMutationV2<'a> {
  pub index_id: &'a [u8],
  pub role: OrderedIndexRoleV1,
  pub operation: IndexWorkspaceMutationOperationV2,
  pub publication_sequence: u64,
  pub operation_id: [u8; 16],
  pub order_key: &'a [u8],
  pub encoded_record: &'a [u8],
  pub document_ordinal: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceMembershipTransitionV2<'a> {
  pub owner_id: &'a [u8],
  pub owner_class: IndexWorkspaceOwnerClassV2,
  pub publication_sequence: u64,
  pub operation_id: [u8; 16],
  pub document_ordinal: u64,
  pub before: IndexWorkspaceMembershipStateV2,
  pub after: IndexWorkspaceMembershipStateV2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexWorkspaceRuntimeBatchPayloadV2<'a> {
  pub coordinator_id: [u8; 16],
  pub batch_id: u64,
  pub reason: IndexFlushReasonV1,
  pub mutations: Vec<IndexWorkspaceRuntimeMutationV2<'a>>,
  pub transitions: Vec<IndexWorkspaceMembershipTransitionV2<'a>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IndexWorkspaceRuntimeBatchPlanV2 {
  header: [u8; RUNTIME_BATCH_HEADER_LENGTH_V2],
  payload_length: usize,
  logical_record_count: u64,
  minimum_publication_sequence: u64,
  maximum_publication_sequence: u64,
  payload_digest: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexWorkspaceRuntimeBatchStreamHeaderV2 {
  pub coordinator_id: [u8; 16],
  pub batch_id: u64,
  pub reason: IndexFlushReasonV1,
  pub mutation_count: usize,
  pub transition_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexWorkspaceRuntimeMutationFrameHeaderV2 {
  pub frame_length: usize,
  pub order_length: usize,
}

impl IndexWorkspaceRuntimeBatchPlanV2 {
  pub const fn payload_length(self) -> usize {
    self.payload_length
  }

  pub const fn logical_record_count(self) -> u64 {
    self.logical_record_count
  }

  pub const fn minimum_publication_sequence(self) -> u64 {
    self.minimum_publication_sequence
  }

  pub const fn maximum_publication_sequence(self) -> u64 {
    self.maximum_publication_sequence
  }

  pub const fn payload_digest(self) -> [u8; 32] {
    self.payload_digest
  }
}

pub fn encode_index_workspace_runtime_batch_payload_v2(request: &IndexWorkspaceRuntimeBatchWriteV2<'_>) -> FormatResult<Vec<u8>> {
  let plan = plan_index_workspace_runtime_batch_payload_v2(request)?;
  let mut encoded = Vec::new();
  encoded.try_reserve_exact(plan.payload_length).map_err(|error| allocation_error(format!("AIRB v2 allocation failed: {error}")))?;
  stream_index_workspace_runtime_batch_payload_v2::<FormatError>(request, plan, |chunk| {
    encoded.extend_from_slice(chunk);
    Ok(())
  })?;
  if encoded.len() != plan.payload_length {
    return Err(length_error("AIRB v2 encoder did not emit its planned length"));
  }
  Ok(encoded)
}

pub(crate) fn plan_index_workspace_runtime_batch_payload_v2(
  request: &IndexWorkspaceRuntimeBatchWriteV2<'_>,
) -> FormatResult<IndexWorkspaceRuntimeBatchPlanV2> {
  validate_batch_identity(request)?;
  let hash_algorithm = request.hash_algorithm;
  let logical_count = request
    .mutations
    .len()
    .checked_add(request.transitions.len())
    .ok_or_else(|| length_error("AIRB v2 logical record count overflowed"))?;
  if logical_count > RUNTIME_LOGICAL_RECORD_LIMIT_V2 {
    return Err(amplification_error("AIRB v2 logical record count exceeds its fixed cap"));
  }

  let mut total_length = RUNTIME_BATCH_HEADER_LENGTH_V2;
  let mut minimum_publication_sequence = u64::MAX;
  let mut maximum_publication_sequence = 0u64;
  for (index, mutation) in request.mutations.iter().enumerate() {
    validate_mutation(mutation, hash_algorithm)?;
    if index > 0 && compare_mutations(&request.mutations[index - 1], mutation) != Ordering::Less {
      return Err(ordering_error("AIRB v2 mutations are not in strict canonical order"));
    }
    total_length = total_length
      .checked_add(checked_mutation_frame_length(mutation.index_id.len(), mutation.order_key.len(), mutation.encoded_record.len())?)
      .ok_or_else(|| length_error("AIRB v2 payload length overflowed"))?;
    ensure_payload_bound(total_length)?;
    minimum_publication_sequence = minimum_publication_sequence.min(mutation.publication_sequence);
    maximum_publication_sequence = maximum_publication_sequence.max(mutation.publication_sequence);
  }
  for (index, transition) in request.transitions.iter().enumerate() {
    validate_transition(transition, hash_algorithm)?;
    if index > 0 && compare_transitions(&request.transitions[index - 1], transition) != Ordering::Less {
      return Err(ordering_error("AIRB v2 membership transitions are not in strict canonical order"));
    }
    total_length = total_length
      .checked_add(checked_transition_frame_length(transition.owner_id.len())?)
      .ok_or_else(|| length_error("AIRB v2 payload length overflowed"))?;
    ensure_payload_bound(total_length)?;
    minimum_publication_sequence = minimum_publication_sequence.min(transition.publication_sequence);
    maximum_publication_sequence = maximum_publication_sequence.max(transition.publication_sequence);
  }
  validate_mutation_transition_closure_write(request)?;

  let mut header = [0u8; RUNTIME_BATCH_HEADER_LENGTH_V2];
  header[..4].copy_from_slice(RUNTIME_BATCH_MAGIC);
  put_u16(&mut header, 4, RUNTIME_BATCH_SCHEMA_VERSION_V2);
  put_u16(&mut header, 6, RUNTIME_BATCH_HEADER_LENGTH_V2 as u16);
  put_u64(&mut header, 8, total_length as u64);
  header[16..32].copy_from_slice(&request.coordinator_id);
  put_u64(&mut header, 32, request.batch_id);
  put_u16(&mut header, 40, request.reason.id());
  put_u32(&mut header, 44, checked_u32(request.mutations.len(), "AIRB v2 mutation count")?);
  put_u32(&mut header, 48, checked_u32(request.transitions.len(), "AIRB v2 transition count")?);

  let mut digest = blake3::Hasher::new();
  digest.update(&header);
  for mutation in request.mutations {
    let frame = mutation_frame_header(mutation)?;
    digest.update(&frame);
    digest.update(mutation.index_id);
    digest.update(mutation.order_key);
    digest.update(mutation.encoded_record);
  }
  for transition in request.transitions {
    let frame = transition_frame_header(transition)?;
    digest.update(&frame);
    digest.update(transition.owner_id);
  }
  Ok(IndexWorkspaceRuntimeBatchPlanV2 {
    header,
    payload_length: total_length,
    logical_record_count: logical_count as u64,
    minimum_publication_sequence,
    maximum_publication_sequence,
    payload_digest: *digest.finalize().as_bytes(),
  })
}

pub(crate) fn stream_index_workspace_runtime_batch_payload_v2<E>(
  request: &IndexWorkspaceRuntimeBatchWriteV2<'_>,
  plan: IndexWorkspaceRuntimeBatchPlanV2,
  mut emit: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E>
where
  E: From<FormatError>,
{
  emit(&plan.header)?;
  for mutation in request.mutations {
    let frame = mutation_frame_header(mutation).map_err(E::from)?;
    emit(&frame)?;
    emit(mutation.index_id)?;
    emit(mutation.order_key)?;
    emit(mutation.encoded_record)?;
  }
  for transition in request.transitions {
    let frame = transition_frame_header(transition).map_err(E::from)?;
    emit(&frame)?;
    emit(transition.owner_id)?;
  }
  Ok(())
}

pub fn decode_index_workspace_runtime_batch_payload_v2(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceRuntimeBatchPayloadV2<'_>> {
  let header = decode_index_workspace_runtime_batch_stream_header_v2(
    bytes.get(..RUNTIME_BATCH_HEADER_LENGTH_V2).ok_or_else(|| length_error("AIRB v2 payload is shorter than its header"))?,
    hash_algorithm,
    bytes.len(),
  )?;

  let mut mutations = Vec::new();
  mutations
    .try_reserve_exact(header.mutation_count)
    .map_err(|error| allocation_error(format!("AIRB v2 mutation allocation failed: {error}")))?;
  let mut cursor = RUNTIME_BATCH_HEADER_LENGTH_V2;
  for _ in 0..header.mutation_count {
    let end = checked_frame_end(bytes, cursor, RUNTIME_MUTATION_FRAME_LENGTH_V2, "AIRB v2 mutation")?;
    let mutation = decode_index_workspace_runtime_mutation_frame_v2(&bytes[cursor..end], hash_algorithm)?;
    if mutations.last().is_some_and(|previous| compare_decoded_mutations(previous, &mutation) != Ordering::Less) {
      return Err(ordering_error("AIRB v2 mutations are not in strict canonical order"));
    }
    mutations.push(mutation);
    cursor = end;
  }

  let mut transitions = Vec::new();
  transitions
    .try_reserve_exact(header.transition_count)
    .map_err(|error| allocation_error(format!("AIRB v2 transition allocation failed: {error}")))?;
  for _ in 0..header.transition_count {
    let end = checked_frame_end(bytes, cursor, RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2, "AIRB v2 membership transition")?;
    let transition = decode_index_workspace_runtime_transition_frame_v2(&bytes[cursor..end], hash_algorithm)?;
    if transitions.last().is_some_and(|previous| compare_decoded_transitions(previous, &transition) != Ordering::Less) {
      return Err(ordering_error("AIRB v2 membership transitions are not in strict canonical order"));
    }
    transitions.push(transition);
    cursor = end;
  }
  if cursor != bytes.len() {
    return Err(length_error("AIRB v2 payload contains trailing bytes"));
  }
  validate_mutation_transition_closure(&mutations, &transitions)?;
  Ok(IndexWorkspaceRuntimeBatchPayloadV2 {
    coordinator_id: header.coordinator_id,
    batch_id: header.batch_id,
    reason: header.reason,
    mutations,
    transitions,
  })
}

pub(crate) fn decode_index_workspace_runtime_batch_stream_header_v2(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
  payload_length: usize,
) -> FormatResult<IndexWorkspaceRuntimeBatchStreamHeaderV2> {
  if bytes.len() != RUNTIME_BATCH_HEADER_LENGTH_V2 || !(RUNTIME_BATCH_HEADER_LENGTH_V2..=RUNTIME_PAYLOAD_LIMIT_V2).contains(&payload_length)
  {
    return Err(length_error("AIRB v2 payload is outside its fixed bounds"));
  }
  if &bytes[..4] != RUNTIME_BATCH_MAGIC || u16_at(bytes, 4)? != RUNTIME_BATCH_SCHEMA_VERSION_V2 {
    return Err(version_error("AIRB v2 magic or version is unknown"));
  }
  if usize::from(u16_at(bytes, 6)?) != RUNTIME_BATCH_HEADER_LENGTH_V2
    || usize_from_u64(u64_at(bytes, 8)?, "AIRB v2 total length")? != payload_length
  {
    return Err(length_error("AIRB v2 header or total length is not canonical"));
  }
  if bytes[42..44].iter().chain(bytes[52..64].iter()).any(|byte| *byte != 0) {
    return Err(reserve_error("AIRB v2 reserved bytes are nonzero"));
  }
  let coordinator_id = array_at::<16>(bytes, 16)?;
  let batch_id = u64_at(bytes, 32)?;
  let reason = IndexFlushReasonV1::from_id(u16_at(bytes, 40)?).ok_or_else(|| kind_error("AIRB v2 flush reason is unknown"))?;
  let mutation_count = u32_at(bytes, 44)? as usize;
  let transition_count = u32_at(bytes, 48)? as usize;
  let logical_count =
    mutation_count.checked_add(transition_count).ok_or_else(|| length_error("AIRB v2 logical record count overflowed"))?;
  if coordinator_id == [0; 16] || batch_id == 0 || transition_count == 0 || logical_count > RUNTIME_LOGICAL_RECORD_LIMIT_V2 {
    return Err(closure_error("AIRB v2 identity or logical record count is invalid"));
  }
  validate_decode_count_bounds(payload_length, hash_algorithm.hash_length(), mutation_count, transition_count)?;
  Ok(IndexWorkspaceRuntimeBatchStreamHeaderV2 { coordinator_id, batch_id, reason, mutation_count, transition_count })
}

fn validate_batch_identity(request: &IndexWorkspaceRuntimeBatchWriteV2<'_>) -> FormatResult<()> {
  if request.coordinator_id == [0; 16] || request.batch_id == 0 || request.transitions.is_empty() {
    return Err(closure_error("AIRB v2 identity or transition count is invalid"));
  }
  Ok(())
}

fn validate_mutation(mutation: &IndexWorkspaceRuntimeMutationWriteV2<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<()> {
  let width = hash_algorithm.hash_length();
  if mutation.index_id.len() != width
    || mutation.index_id.iter().all(|byte| *byte == 0)
    || mutation.publication_sequence == 0
    || mutation.operation_id == [0; 16]
    || mutation.role == OrderedIndexRoleV1::NvtTile
    || mutation.order_key.is_empty()
    || mutation.order_key.len() > RUNTIME_ORDER_KEY_LIMIT_V2
    || mutation.encoded_record.is_empty()
    || mutation.encoded_record.len() > RUNTIME_ENCODED_RECORD_LIMIT_V2
  {
    return Err(closure_error("AIRB v2 mutation identity, role, or bounds are invalid"));
  }
  let decoded = decode_ordered_record(mutation.encoded_record, hash_algorithm, mutation.role)?;
  if ordered_record_order_key(&decoded)?.as_slice() != mutation.order_key {
    return Err(closure_error("AIRB v2 mutation order key disagrees with its encoded record"));
  }
  if mutation.operation == IndexWorkspaceMutationOperationV2::RemoveExisting
    && (mutation.role != OrderedIndexRoleV1::ScopeReverse || decoded.tombstone)
  {
    return Err(closure_error("AIRB v2 remove-existing is legal only for a live scope-reverse record"));
  }
  Ok(())
}

fn validate_transition(transition: &IndexWorkspaceMembershipTransitionWriteV2<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<()> {
  if transition.owner_id.len() != hash_algorithm.hash_length()
    || transition.owner_id.iter().all(|byte| *byte == 0)
    || transition.publication_sequence == 0
    || transition.operation_id == [0; 16]
    || transition.document_ordinal == 0
  {
    return Err(closure_error("AIRB v2 membership transition identity is invalid"));
  }
  validate_membership_states(transition.owner_class, transition.before, transition.after)
}

fn validate_membership_states(
  owner_class: IndexWorkspaceOwnerClassV2,
  before: IndexWorkspaceMembershipStateV2,
  after: IndexWorkspaceMembershipStateV2,
) -> FormatResult<()> {
  if (before.live && before.unindexable)
    || (after.live && after.unindexable)
    || (owner_class == IndexWorkspaceOwnerClassV2::ScopeCatalog && (before.unindexable || after.unindexable))
  {
    return Err(closure_error("AIRB v2 membership state is contradictory for its owner class"));
  }
  Ok(())
}

fn validate_mutation_transition_closure_write(request: &IndexWorkspaceRuntimeBatchWriteV2<'_>) -> FormatResult<()> {
  for mutation in request.mutations {
    let decoded = decode_ordered_record(mutation.encoded_record, request.hash_algorithm, mutation.role)?;
    let position = request.transitions.partition_point(|transition| {
      compare_transition_key(transition.owner_id, transition.document_ordinal, mutation.index_id, decoded.document_ordinal).is_lt()
    });
    let Some(transition) = request.transitions.get(position) else {
      return Err(closure_error("AIRB v2 mutation has no owner/document membership transition"));
    };
    if transition.owner_id != mutation.index_id
      || transition.document_ordinal != decoded.document_ordinal
      || transition.owner_class.id() != mutation.role.owner_class()
      || mutation.publication_sequence > transition.publication_sequence
    {
      return Err(closure_error("AIRB v2 mutation and membership transition disagree"));
    }
  }
  Ok(())
}

fn validate_mutation_transition_closure(
  mutations: &[IndexWorkspaceRuntimeMutationV2<'_>],
  transitions: &[IndexWorkspaceMembershipTransitionV2<'_>],
) -> FormatResult<()> {
  for mutation in mutations {
    let position = transitions.partition_point(|transition| {
      compare_transition_key(transition.owner_id, transition.document_ordinal, mutation.index_id, mutation.document_ordinal).is_lt()
    });
    let Some(transition) = transitions.get(position) else {
      return Err(closure_error("AIRB v2 mutation has no owner/document membership transition"));
    };
    validate_index_workspace_runtime_mutation_transition_v2(mutation, transition)?;
  }
  Ok(())
}

pub(crate) fn validate_index_workspace_runtime_mutation_transition_v2(
  mutation: &IndexWorkspaceRuntimeMutationV2<'_>,
  transition: &IndexWorkspaceMembershipTransitionV2<'_>,
) -> FormatResult<()> {
  if transition.owner_id != mutation.index_id
    || transition.document_ordinal != mutation.document_ordinal
    || transition.owner_class.id() != mutation.role.owner_class()
    || mutation.publication_sequence > transition.publication_sequence
  {
    return Err(closure_error("AIRB v2 mutation and membership transition disagree"));
  }
  Ok(())
}

fn mutation_frame_header(mutation: &IndexWorkspaceRuntimeMutationWriteV2<'_>) -> FormatResult<[u8; RUNTIME_MUTATION_FRAME_LENGTH_V2]> {
  let mut frame = [0u8; RUNTIME_MUTATION_FRAME_LENGTH_V2];
  put_u32(
    &mut frame,
    0,
    checked_u32(
      checked_mutation_frame_length(mutation.index_id.len(), mutation.order_key.len(), mutation.encoded_record.len())?,
      "AIRB v2 mutation frame length",
    )?,
  );
  frame[4] = mutation.role.id();
  frame[5] = mutation.operation.id();
  put_u16(&mut frame, 6, checked_u16(mutation.index_id.len(), "AIRB v2 mutation owner width")?);
  put_u64(&mut frame, 8, mutation.publication_sequence);
  frame[16..32].copy_from_slice(&mutation.operation_id);
  put_u32(&mut frame, 32, checked_u32(mutation.order_key.len(), "AIRB v2 order-key length")?);
  put_u32(&mut frame, 36, checked_u32(mutation.encoded_record.len(), "AIRB v2 encoded-record length")?);
  Ok(frame)
}

fn transition_frame_header(
  transition: &IndexWorkspaceMembershipTransitionWriteV2<'_>,
) -> FormatResult<[u8; RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2]> {
  let mut frame = [0u8; RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2];
  put_u32(&mut frame, 0, checked_u32(checked_transition_frame_length(transition.owner_id.len())?, "AIRB v2 transition frame length")?);
  frame[4] = transition.owner_class.id();
  frame[5] = membership_flags(transition.before, transition.after);
  put_u16(&mut frame, 6, checked_u16(transition.owner_id.len(), "AIRB v2 transition owner width")?);
  put_u64(&mut frame, 8, transition.publication_sequence);
  frame[16..32].copy_from_slice(&transition.operation_id);
  put_u64(&mut frame, 32, transition.document_ordinal);
  Ok(frame)
}

pub(crate) fn validate_index_workspace_runtime_mutation_frame_header_v2(
  frame: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceRuntimeMutationFrameHeaderV2> {
  if frame.len() != RUNTIME_MUTATION_FRAME_LENGTH_V2 {
    return Err(length_error("AIRB v2 mutation fixed header has the wrong length"));
  }
  let frame_length = u32_at(frame, 0)? as usize;
  let role = OrderedIndexRoleV1::from_id(frame[4]).ok_or_else(|| kind_error("AIRB v2 mutation role is unknown"))?;
  IndexWorkspaceMutationOperationV2::from_id(frame[5]).ok_or_else(|| kind_error("AIRB v2 mutation operation is unknown"))?;
  let index_length = usize::from(u16_at(frame, 6)?);
  let publication_sequence = u64_at(frame, 8)?;
  let operation_id = array_at::<16>(frame, 16)?;
  let order_length = u32_at(frame, 32)? as usize;
  let record_length = u32_at(frame, 36)? as usize;
  if index_length != hash_algorithm.hash_length()
    || order_length == 0
    || order_length > RUNTIME_ORDER_KEY_LIMIT_V2
    || record_length == 0
    || record_length > RUNTIME_ENCODED_RECORD_LIMIT_V2
    || publication_sequence == 0
    || operation_id == [0; 16]
    || role == OrderedIndexRoleV1::NvtTile
    || checked_mutation_frame_length(index_length, order_length, record_length)? != frame_length
  {
    return Err(closure_error("AIRB v2 mutation frame identity or lengths are invalid"));
  }
  Ok(IndexWorkspaceRuntimeMutationFrameHeaderV2 { frame_length, order_length })
}

pub(crate) fn decode_index_workspace_runtime_mutation_frame_v2(
  frame: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceRuntimeMutationV2<'_>> {
  if frame.len() < RUNTIME_MUTATION_FRAME_LENGTH_V2 {
    return Err(length_error("AIRB v2 mutation frame is truncated"));
  }
  let header = validate_index_workspace_runtime_mutation_frame_header_v2(&frame[..RUNTIME_MUTATION_FRAME_LENGTH_V2], hash_algorithm)?;
  let role = OrderedIndexRoleV1::from_id(frame[4]).ok_or_else(|| kind_error("AIRB v2 mutation role is unknown"))?;
  let operation =
    IndexWorkspaceMutationOperationV2::from_id(frame[5]).ok_or_else(|| kind_error("AIRB v2 mutation operation is unknown"))?;
  let index_length = usize::from(u16_at(frame, 6)?);
  let publication_sequence = u64_at(frame, 8)?;
  let operation_id = array_at::<16>(frame, 16)?;
  if header.frame_length != frame.len() {
    return Err(length_error("AIRB v2 mutation frame length disagrees with its bytes"));
  }
  let index_start = RUNTIME_MUTATION_FRAME_LENGTH_V2;
  let index_end = index_start + index_length;
  let order_end = index_end + header.order_length;
  let index_id = &frame[index_start..index_end];
  let order_key = &frame[index_end..order_end];
  let encoded_record = &frame[order_end..];
  if index_id.iter().all(|byte| *byte == 0) {
    return Err(closure_error("AIRB v2 mutation owner is all zeroes"));
  }
  let decoded = decode_ordered_record(encoded_record, hash_algorithm, role)?;
  if ordered_record_order_key(&decoded)?.as_slice() != order_key {
    return Err(closure_error("AIRB v2 mutation order key disagrees with its encoded record"));
  }
  if operation == IndexWorkspaceMutationOperationV2::RemoveExisting && (role != OrderedIndexRoleV1::ScopeReverse || decoded.tombstone) {
    return Err(closure_error("AIRB v2 remove-existing is legal only for a live scope-reverse record"));
  }
  Ok(IndexWorkspaceRuntimeMutationV2 {
    index_id,
    role,
    operation,
    publication_sequence,
    operation_id,
    order_key,
    encoded_record,
    document_ordinal: decoded.document_ordinal,
  })
}

pub(crate) fn validate_index_workspace_runtime_transition_frame_header_v2(
  frame: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<usize> {
  if frame.len() != RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2 {
    return Err(length_error("AIRB v2 membership fixed header has the wrong length"));
  }
  let frame_length = u32_at(frame, 0)? as usize;
  let owner_class = IndexWorkspaceOwnerClassV2::from_id(frame[4]).ok_or_else(|| kind_error("AIRB v2 owner class is unknown"))?;
  let flags = frame[5];
  let owner_length = usize::from(u16_at(frame, 6)?);
  let publication_sequence = u64_at(frame, 8)?;
  let operation_id = array_at::<16>(frame, 16)?;
  let document_ordinal = u64_at(frame, 32)?;
  if flags & !0x0f != 0 || frame[40..48].iter().any(|byte| *byte != 0) {
    return Err(reserve_error("AIRB v2 membership flags or reserved bytes are noncanonical"));
  }
  let before = IndexWorkspaceMembershipStateV2 { live: flags & 1 != 0, unindexable: flags & 4 != 0 };
  let after = IndexWorkspaceMembershipStateV2 { live: flags & 2 != 0, unindexable: flags & 8 != 0 };
  validate_membership_states(owner_class, before, after)?;
  if owner_length != hash_algorithm.hash_length()
    || publication_sequence == 0
    || operation_id == [0; 16]
    || document_ordinal == 0
    || checked_transition_frame_length(owner_length)? != frame_length
  {
    return Err(closure_error("AIRB v2 membership transition identity or lengths are invalid"));
  }
  Ok(frame_length)
}

pub(crate) fn decode_index_workspace_runtime_transition_frame_v2(
  frame: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceMembershipTransitionV2<'_>> {
  if frame.len() < RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2 {
    return Err(length_error("AIRB v2 membership transition frame is truncated"));
  }
  let frame_length =
    validate_index_workspace_runtime_transition_frame_header_v2(&frame[..RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2], hash_algorithm)?;
  let owner_class = IndexWorkspaceOwnerClassV2::from_id(frame[4]).ok_or_else(|| kind_error("AIRB v2 owner class is unknown"))?;
  let flags = frame[5];
  let publication_sequence = u64_at(frame, 8)?;
  let operation_id = array_at::<16>(frame, 16)?;
  let document_ordinal = u64_at(frame, 32)?;
  if frame_length != frame.len() {
    return Err(length_error("AIRB v2 membership frame length disagrees with its bytes"));
  }
  let owner_id = &frame[RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2..];
  if owner_id.iter().all(|byte| *byte == 0) {
    return Err(closure_error("AIRB v2 transition owner is all zeroes"));
  }
  let before = IndexWorkspaceMembershipStateV2 { live: flags & 1 != 0, unindexable: flags & 4 != 0 };
  let after = IndexWorkspaceMembershipStateV2 { live: flags & 2 != 0, unindexable: flags & 8 != 0 };
  validate_membership_states(owner_class, before, after)?;
  Ok(IndexWorkspaceMembershipTransitionV2 { owner_id, owner_class, publication_sequence, operation_id, document_ordinal, before, after })
}

fn validate_decode_count_bounds(
  payload_length: usize,
  hash_width: usize,
  mutation_count: usize,
  transition_count: usize,
) -> FormatResult<()> {
  let remaining = payload_length - RUNTIME_BATCH_HEADER_LENGTH_V2;
  let minimum_mutation = RUNTIME_MUTATION_FRAME_LENGTH_V2
    .checked_add(hash_width)
    .and_then(|length| length.checked_add(2))
    .ok_or_else(|| length_error("AIRB v2 minimum mutation length overflowed"))?;
  let mutation_bytes =
    mutation_count.checked_mul(minimum_mutation).ok_or_else(|| length_error("AIRB v2 minimum mutation bytes overflowed"))?;
  let minimum_transition = RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2
    .checked_add(hash_width)
    .ok_or_else(|| length_error("AIRB v2 minimum transition length overflowed"))?;
  let transition_bytes =
    transition_count.checked_mul(minimum_transition).ok_or_else(|| length_error("AIRB v2 minimum transition bytes overflowed"))?;
  if mutation_bytes.checked_add(transition_bytes).is_none_or(|minimum| minimum > remaining) {
    return Err(amplification_error("AIRB v2 frame counts exceed the available payload"));
  }
  Ok(())
}

fn checked_frame_end(bytes: &[u8], start: usize, fixed_length: usize, label: &'static str) -> FormatResult<usize> {
  let fixed_end = start.checked_add(fixed_length).ok_or_else(|| length_error(format!("{label} fixed header overflowed")))?;
  if fixed_end > bytes.len() {
    return Err(length_error(format!("{label} fixed header is truncated")));
  }
  let frame_length = u32_at(bytes, start)? as usize;
  let end = start.checked_add(frame_length).ok_or_else(|| length_error(format!("{label} length overflowed")))?;
  if end > bytes.len() {
    return Err(length_error(format!("{label} is truncated")));
  }
  Ok(end)
}

fn compare_mutations(left: &IndexWorkspaceRuntimeMutationWriteV2<'_>, right: &IndexWorkspaceRuntimeMutationWriteV2<'_>) -> Ordering {
  left.index_id.cmp(right.index_id).then(left.role.id().cmp(&right.role.id())).then(left.order_key.cmp(right.order_key))
}

fn compare_decoded_mutations(left: &IndexWorkspaceRuntimeMutationV2<'_>, right: &IndexWorkspaceRuntimeMutationV2<'_>) -> Ordering {
  left.index_id.cmp(right.index_id).then(left.role.id().cmp(&right.role.id())).then(left.order_key.cmp(right.order_key))
}

fn compare_transitions(
  left: &IndexWorkspaceMembershipTransitionWriteV2<'_>,
  right: &IndexWorkspaceMembershipTransitionWriteV2<'_>,
) -> Ordering {
  compare_transition_key(left.owner_id, left.document_ordinal, right.owner_id, right.document_ordinal)
}

fn compare_decoded_transitions(
  left: &IndexWorkspaceMembershipTransitionV2<'_>,
  right: &IndexWorkspaceMembershipTransitionV2<'_>,
) -> Ordering {
  compare_transition_key(left.owner_id, left.document_ordinal, right.owner_id, right.document_ordinal)
}

fn compare_transition_key(left_owner: &[u8], left_ordinal: u64, right_owner: &[u8], right_ordinal: u64) -> Ordering {
  left_owner.cmp(right_owner).then(left_ordinal.cmp(&right_ordinal))
}

fn membership_flags(before: IndexWorkspaceMembershipStateV2, after: IndexWorkspaceMembershipStateV2) -> u8 {
  u8::from(before.live) | (u8::from(after.live) << 1) | (u8::from(before.unindexable) << 2) | (u8::from(after.unindexable) << 3)
}

fn checked_mutation_frame_length(index_length: usize, order_length: usize, record_length: usize) -> FormatResult<usize> {
  RUNTIME_MUTATION_FRAME_LENGTH_V2
    .checked_add(index_length)
    .and_then(|length| length.checked_add(order_length))
    .and_then(|length| length.checked_add(record_length))
    .ok_or_else(|| length_error("AIRB v2 mutation frame length overflowed"))
}

fn checked_transition_frame_length(owner_length: usize) -> FormatResult<usize> {
  RUNTIME_MEMBERSHIP_FRAME_LENGTH_V2.checked_add(owner_length).ok_or_else(|| length_error("AIRB v2 membership frame length overflowed"))
}

fn ensure_payload_bound(length: usize) -> FormatResult<()> {
  if length > RUNTIME_PAYLOAD_LIMIT_V2 {
    return Err(amplification_error("AIRB v2 payload exceeds its fixed cap"));
  }
  Ok(())
}

fn checked_u16(value: usize, label: &'static str) -> FormatResult<u16> {
  u16::try_from(value).map_err(|_| length_error(format!("{label} exceeds u16")))
}

fn checked_u32(value: usize, label: &'static str) -> FormatResult<u32> {
  u32::try_from(value).map_err(|_| length_error(format!("{label} exceeds u32")))
}

fn usize_from_u64(value: u64, label: &'static str) -> FormatResult<usize> {
  usize::try_from(value).map_err(|_| length_error(format!("{label} exceeds usize")))
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let end = offset.checked_add(2).ok_or_else(|| length_error("u16 field offset overflowed"))?;
  let field = bytes.get(offset..end).ok_or_else(|| length_error("u16 field is truncated"))?;
  Ok(u16::from_le_bytes(field.try_into().map_err(|_| length_error("u16 field length changed"))?))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let end = offset.checked_add(4).ok_or_else(|| length_error("u32 field offset overflowed"))?;
  let field = bytes.get(offset..end).ok_or_else(|| length_error("u32 field is truncated"))?;
  Ok(u32::from_le_bytes(field.try_into().map_err(|_| length_error("u32 field length changed"))?))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let end = offset.checked_add(8).ok_or_else(|| length_error("u64 field offset overflowed"))?;
  let field = bytes.get(offset..end).ok_or_else(|| length_error("u64 field is truncated"))?;
  Ok(u64::from_le_bytes(field.try_into().map_err(|_| length_error("u64 field length changed"))?))
}

fn array_at<const N: usize>(bytes: &[u8], offset: usize) -> FormatResult<[u8; N]> {
  let end = offset.checked_add(N).ok_or_else(|| length_error("array field offset overflowed"))?;
  bytes
    .get(offset..end)
    .ok_or_else(|| length_error("array field is truncated"))?
    .try_into()
    .map_err(|_| length_error("array field length changed"))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn version_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownMagicOrVersion, "index_runtime_batch_v2_version", context)
}

fn kind_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, "index_runtime_batch_v2_kind", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::TruncationOrTrailingBytes, "index_runtime_batch_v2_length", context)
}

fn amplification_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::AllocationAmplification, "index_runtime_batch_v2_allocation", context)
}

fn reserve_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NonzeroReservedOrPadding, "index_runtime_batch_v2_reserved", context)
}

fn ordering_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NoncanonicalOrderOrDuplicate, "index_runtime_batch_v2_order", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, "index_runtime_batch_v2_closure", context)
}

fn allocation_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::AllocationAmplification, "index_runtime_batch_v2_allocation", context)
}
