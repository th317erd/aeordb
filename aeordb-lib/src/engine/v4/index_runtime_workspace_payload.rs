use std::cmp::Ordering;

use crate::engine::HashAlgorithm;
use crate::engine::path_utils::normalize_path;

use super::index_coordinator::{FrozenIndexBatchV1, IndexFlushReasonV1, PublishedIndexMutationV1};
use super::index_page::{OrderedIndexRoleV1, decode_ordered_record, ordered_record_order_key};
use super::index_producer_coordinator::{IndexProducerTaskKindV1, IndexProducerTaskRequestV1};
use super::index_runtime_workspace::{
  IndexWorkspaceObjectKindV1, IndexWorkspaceRuntimeBatchPayload, decode_index_workspace_runtime_batch_payload,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const RUNTIME_BATCH_MAGIC: &[u8; 4] = b"AIRB";
const RUNTIME_BATCH_SCHEMA_VERSION: u16 = 1;
pub(super) const RUNTIME_BATCH_HEADER_LENGTH: usize = 64;
pub(super) const RUNTIME_MUTATION_FRAME_LENGTH: usize = 40;
const RUNTIME_RECORD_LIMIT: usize = 1_048_576;
const RUNTIME_ORDER_KEY_LIMIT: usize = 1_024 * 1_024;
const RUNTIME_ENCODED_RECORD_LIMIT: usize = 4 * 1_024 * 1_024;
const RUNTIME_PAYLOAD_LIMIT: usize = 512 * 1_024 * 1_024;

const PRODUCER_TASK_MAGIC: &[u8; 4] = b"AITK";
const PRODUCER_TASK_SCHEMA_VERSION: u16 = 1;
const PRODUCER_TASK_FIXED_HEADER_LENGTH: usize = 56;
const PRODUCER_TASK_SCOPE_LIMIT: usize = 16 * 1_024;
const PRODUCER_TASK_FLAG_JOURNAL: u16 = 1;
const PRODUCER_TASK_FLAG_SCOPE: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceRuntimeMutationV1<'a> {
  pub index_id: &'a [u8],
  pub role: OrderedIndexRoleV1,
  pub publication_sequence: u64,
  pub operation_id: [u8; 16],
  pub order_key: &'a [u8],
  pub encoded_record: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexWorkspaceRuntimeBatchPayloadV1<'a> {
  pub coordinator_id: [u8; 16],
  pub batch_id: u64,
  pub reason: IndexFlushReasonV1,
  pub records: Vec<IndexWorkspaceRuntimeMutationV1<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceProducerTaskPayloadV1<'a> {
  pub operation_id: [u8; 16],
  pub kind: IndexProducerTaskKindV1,
  pub publication_sequence: u64,
  pub namespace_root_before: &'a [u8],
  pub namespace_root_after: &'a [u8],
  pub semantic_state_root: &'a [u8],
  pub journal_head: Option<&'a [u8]>,
  pub scope: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IndexWorkspaceProducerTaskPlanV1 {
  header_length: usize,
  scope_length: usize,
  payload_length: usize,
}

impl IndexWorkspaceProducerTaskPlanV1 {
  pub(super) const fn payload_length(self) -> usize {
    self.payload_length
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IndexWorkspaceRuntimeBatchPlanV1 {
  header: [u8; RUNTIME_BATCH_HEADER_LENGTH],
  payload_length: usize,
  logical_record_count: u64,
  minimum_publication_sequence: u64,
  maximum_publication_sequence: u64,
  payload_digest: [u8; 32],
}

impl IndexWorkspaceRuntimeBatchPlanV1 {
  pub(super) const fn payload_length(self) -> usize {
    self.payload_length
  }

  pub(super) const fn logical_record_count(self) -> u64 {
    self.logical_record_count
  }

  pub(super) const fn minimum_publication_sequence(self) -> u64 {
    self.minimum_publication_sequence
  }

  pub(super) const fn maximum_publication_sequence(self) -> u64 {
    self.maximum_publication_sequence
  }

  pub(super) const fn payload_digest(self) -> [u8; 32] {
    self.payload_digest
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IndexWorkspaceRuntimeBatchStreamHeaderV1 {
  pub coordinator_id: [u8; 16],
  pub batch_id: u64,
  pub reason: IndexFlushReasonV1,
  pub record_count: usize,
}

pub fn encode_index_workspace_runtime_batch_payload_v1(batch: &FrozenIndexBatchV1, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let plan = plan_index_workspace_runtime_batch_payload_v1(batch, hash_algorithm)?;
  let mut encoded = Vec::new();
  encoded.try_reserve_exact(plan.payload_length).map_err(|error| allocation_error(format!("runtime batch allocation failed: {error}")))?;
  stream_index_workspace_runtime_batch_payload_v1::<FormatError>(batch, plan, |chunk| {
    encoded.extend_from_slice(chunk);
    Ok(())
  })?;
  if encoded.len() != plan.payload_length {
    return Err(length_error("runtime batch encoder did not emit its planned length"));
  }
  Ok(encoded)
}

pub(super) fn plan_index_workspace_runtime_batch_payload_v1(
  batch: &FrozenIndexBatchV1,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceRuntimeBatchPlanV1> {
  if batch.coordinator_id() == [0; 16]
    || batch.batch_id() == 0
    || batch.records().is_empty()
    || !batch.transitions().is_empty()
    || batch.records().len() > RUNTIME_RECORD_LIMIT
  {
    return Err(closure_error("runtime batch identity or record count is invalid"));
  }
  let mut total_length = RUNTIME_BATCH_HEADER_LENGTH;
  let mut previous: Option<&PublishedIndexMutationV1> = None;
  let mut minimum_publication_sequence = u64::MAX;
  let mut maximum_publication_sequence = 0u64;
  for record in batch.records() {
    if record.operation() != super::index_coordinator::IndexMutationOperationV1::Upsert {
      return Err(closure_error("AIRB v1 cannot encode explicit mutation operations"));
    }
    validate_runtime_mutation(record, hash_algorithm)?;
    if previous.is_some_and(|prior| compare_runtime_mutations(prior, record) != Ordering::Less) {
      return Err(ordering_error("runtime batch records are not in strict canonical order"));
    }
    let frame_length = checked_runtime_frame_length(record.index_id().len(), record.order_key().len(), record.encoded_record().len())?;
    total_length = total_length.checked_add(frame_length).ok_or_else(|| length_error("runtime batch payload length overflowed"))?;
    if total_length > RUNTIME_PAYLOAD_LIMIT {
      return Err(allocation_error("runtime batch payload exceeds its fixed cap"));
    }
    minimum_publication_sequence = minimum_publication_sequence.min(record.publication_sequence());
    maximum_publication_sequence = maximum_publication_sequence.max(record.publication_sequence());
    previous = Some(record);
  }

  let mut header = [0u8; RUNTIME_BATCH_HEADER_LENGTH];
  header[..4].copy_from_slice(RUNTIME_BATCH_MAGIC);
  put_u16(&mut header, 4, RUNTIME_BATCH_SCHEMA_VERSION);
  put_u16(&mut header, 6, RUNTIME_BATCH_HEADER_LENGTH as u16);
  put_u64(&mut header, 8, total_length as u64);
  header[16..32].copy_from_slice(&batch.coordinator_id());
  put_u64(&mut header, 32, batch.batch_id());
  put_u16(&mut header, 40, batch.reason().id());
  put_u32(&mut header, 44, batch.records().len() as u32);

  let mut digest = blake3::Hasher::new();
  digest.update(&header);
  for record in batch.records() {
    let frame = runtime_mutation_frame_header(record)?;
    digest.update(&frame);
    digest.update(record.index_id());
    digest.update(record.order_key());
    digest.update(record.encoded_record());
  }
  Ok(IndexWorkspaceRuntimeBatchPlanV1 {
    header,
    payload_length: total_length,
    logical_record_count: batch.records().len() as u64,
    minimum_publication_sequence,
    maximum_publication_sequence,
    payload_digest: *digest.finalize().as_bytes(),
  })
}

pub(super) fn stream_index_workspace_runtime_batch_payload_v1<E>(
  batch: &FrozenIndexBatchV1,
  plan: IndexWorkspaceRuntimeBatchPlanV1,
  mut emit: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E>
where
  E: From<FormatError>,
{
  emit(&plan.header)?;
  for record in batch.records() {
    let frame = runtime_mutation_frame_header(record).map_err(E::from)?;
    emit(&frame)?;
    emit(record.index_id())?;
    emit(record.order_key())?;
    emit(record.encoded_record())?;
  }
  Ok(())
}

pub fn decode_index_workspace_runtime_batch_payload_v1(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceRuntimeBatchPayloadV1<'_>> {
  if bytes.len() < RUNTIME_BATCH_HEADER_LENGTH {
    return Err(length_error("runtime batch payload is shorter than its header"));
  }
  let header = decode_index_workspace_runtime_batch_stream_header_v1(&bytes[..RUNTIME_BATCH_HEADER_LENGTH], hash_algorithm, bytes.len())?;

  let mut records = Vec::new();
  records.try_reserve_exact(header.record_count).map_err(|error| allocation_error(format!("runtime record allocation failed: {error}")))?;
  let mut cursor = RUNTIME_BATCH_HEADER_LENGTH;
  for _ in 0..header.record_count {
    let fixed_end = cursor.checked_add(RUNTIME_MUTATION_FRAME_LENGTH).ok_or_else(|| length_error("runtime frame header overflowed"))?;
    if fixed_end > bytes.len() {
      return Err(length_error("runtime frame header is truncated"));
    }
    let frame_length = u32_at(bytes, cursor) as usize;
    let frame_end = cursor.checked_add(frame_length).ok_or_else(|| length_error("runtime frame length overflowed"))?;
    if frame_end > bytes.len() {
      return Err(length_error("runtime mutation frame is truncated"));
    }
    let record = decode_index_workspace_runtime_mutation_frame_v1(&bytes[cursor..frame_end], hash_algorithm)?;
    if records.last().is_some_and(|previous| compare_decoded_runtime_mutations(previous, &record) != Ordering::Less) {
      return Err(ordering_error("runtime mutations are not in strict canonical order"));
    }
    records.push(record);
    cursor = frame_end;
  }
  if cursor != bytes.len() {
    return Err(length_error("runtime batch payload contains trailing bytes"));
  }
  Ok(IndexWorkspaceRuntimeBatchPayloadV1 {
    coordinator_id: header.coordinator_id,
    batch_id: header.batch_id,
    reason: header.reason,
    records,
  })
}

pub(super) fn decode_index_workspace_runtime_batch_stream_header_v1(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
  payload_length: usize,
) -> FormatResult<IndexWorkspaceRuntimeBatchStreamHeaderV1> {
  if bytes.len() != RUNTIME_BATCH_HEADER_LENGTH
    || !(RUNTIME_BATCH_HEADER_LENGTH + RUNTIME_MUTATION_FRAME_LENGTH..=RUNTIME_PAYLOAD_LIMIT).contains(&payload_length)
  {
    return Err(length_error("runtime batch payload is outside its fixed bounds"));
  }
  if &bytes[..4] != RUNTIME_BATCH_MAGIC || u16_at(bytes, 4) != RUNTIME_BATCH_SCHEMA_VERSION {
    return Err(version_error("runtime batch payload magic or version is unknown"));
  }
  if usize::from(u16_at(bytes, 6)) != RUNTIME_BATCH_HEADER_LENGTH
    || usize_from_u64(u64_at(bytes, 8), "runtime batch total length")? != payload_length
  {
    return Err(length_error("runtime batch header or total length is not canonical"));
  }
  if bytes[42..44].iter().chain(bytes[48..64].iter()).any(|byte| *byte != 0) {
    return Err(reserve_error("runtime batch reserved bytes are nonzero"));
  }
  let coordinator_id = array_at(bytes, 16);
  let batch_id = u64_at(bytes, 32);
  let reason = IndexFlushReasonV1::from_id(u16_at(bytes, 40)).ok_or_else(|| kind_error("runtime batch flush reason is unknown"))?;
  let record_count = u32_at(bytes, 44) as usize;
  let minimum_frame_length = RUNTIME_MUTATION_FRAME_LENGTH
    .checked_add(hash_algorithm.hash_length())
    .and_then(|length| length.checked_add(2))
    .ok_or_else(|| length_error("minimum runtime frame length overflowed"))?;
  let maximum_framed_records = (payload_length - RUNTIME_BATCH_HEADER_LENGTH) / minimum_frame_length;
  if coordinator_id == [0; 16]
    || batch_id == 0
    || record_count == 0
    || record_count > RUNTIME_RECORD_LIMIT
    || record_count > maximum_framed_records
  {
    return Err(closure_error("runtime batch identity or record count is invalid"));
  }
  Ok(IndexWorkspaceRuntimeBatchStreamHeaderV1 { coordinator_id, batch_id, reason, record_count })
}

pub(super) fn decode_index_workspace_runtime_mutation_frame_v1(
  frame: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceRuntimeMutationV1<'_>> {
  if frame.len() < RUNTIME_MUTATION_FRAME_LENGTH {
    return Err(length_error("runtime mutation frame header is truncated"));
  }
  let (frame_length, _order_length) =
    validate_index_workspace_runtime_mutation_frame_header_v1(&frame[..RUNTIME_MUTATION_FRAME_LENGTH], hash_algorithm)?;
  let role = OrderedIndexRoleV1::from_id(frame[4]).ok_or_else(|| kind_error("runtime mutation role is unknown"))?;
  let index_length = hash_algorithm.hash_length();
  let publication_sequence = u64_at(frame, 8);
  let operation_id = array_at(frame, 16);
  let order_length = u32_at(frame, 32) as usize;
  if frame_length != frame.len() {
    return Err(closure_error("runtime mutation frame length disagrees with its bytes"));
  }
  let index_start = RUNTIME_MUTATION_FRAME_LENGTH;
  let index_end = index_start + index_length;
  let order_end = index_end + order_length;
  let index_id = &frame[index_start..index_end];
  let order_key = &frame[index_end..order_end];
  let encoded_record = &frame[order_end..];
  if index_id.iter().all(|byte| *byte == 0) {
    return Err(closure_error("runtime mutation index identity is all zeroes"));
  }
  let decoded_record = decode_ordered_record(encoded_record, hash_algorithm, role)?;
  if ordered_record_order_key(&decoded_record)?.as_slice() != order_key {
    return Err(closure_error("runtime mutation order key does not match its encoded record"));
  }
  Ok(IndexWorkspaceRuntimeMutationV1 { index_id, role, publication_sequence, operation_id, order_key, encoded_record })
}

pub(super) fn validate_index_workspace_runtime_mutation_frame_header_v1(
  frame: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(usize, usize)> {
  if frame.len() != RUNTIME_MUTATION_FRAME_LENGTH {
    return Err(length_error("runtime mutation fixed header has the wrong length"));
  }
  let frame_length = u32_at(frame, 0) as usize;
  let role = OrderedIndexRoleV1::from_id(frame[4]).ok_or_else(|| kind_error("runtime mutation role is unknown"))?;
  let index_length = usize::from(u16_at(frame, 6));
  let publication_sequence = u64_at(frame, 8);
  let operation_id = array_at::<16>(frame, 16);
  let order_length = u32_at(frame, 32) as usize;
  let record_length = u32_at(frame, 36) as usize;
  if frame[5] != 0 || role == OrderedIndexRoleV1::NvtTile {
    return Err(reserve_error("runtime mutation flags are nonzero or its role is not mutable"));
  }
  if index_length != hash_algorithm.hash_length()
    || order_length == 0
    || order_length > RUNTIME_ORDER_KEY_LIMIT
    || record_length == 0
    || record_length > RUNTIME_ENCODED_RECORD_LIMIT
    || publication_sequence == 0
    || operation_id == [0; 16]
    || checked_runtime_frame_length(index_length, order_length, record_length)? != frame_length
  {
    return Err(closure_error("runtime mutation lengths, identity, or publication sequence are invalid"));
  }
  Ok((frame_length, order_length))
}

pub fn encode_index_workspace_producer_task_payload_v1(
  task: &IndexProducerTaskRequestV1<'_>,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  let plan = plan_index_workspace_producer_task_payload_v1(task, hash_algorithm)?;
  encode_index_workspace_producer_task_payload_with_plan_v1(task, hash_algorithm, plan)
}

pub(super) fn plan_index_workspace_producer_task_payload_v1(
  task: &IndexProducerTaskRequestV1<'_>,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceProducerTaskPlanV1> {
  validate_producer_task_fields(
    task.operation_id,
    task.kind,
    task.publication_sequence,
    task.namespace_root_before,
    task.namespace_root_after,
    task.semantic_state_root,
    task.journal_head,
    task.scope,
    hash_algorithm,
  )?;
  let hash_width = hash_algorithm.hash_length();
  let header_length = PRODUCER_TASK_FIXED_HEADER_LENGTH
    .checked_add(4usize.checked_mul(hash_width).ok_or_else(|| length_error("producer task hash area overflowed"))?)
    .ok_or_else(|| length_error("producer task header length overflowed"))?;
  let scope_length = task.scope.map_or(0, str::len);
  let total_length = header_length.checked_add(scope_length).ok_or_else(|| length_error("producer task length overflowed"))?;
  Ok(IndexWorkspaceProducerTaskPlanV1 { header_length, scope_length, payload_length: total_length })
}

fn encode_index_workspace_producer_task_payload_with_plan_v1(
  task: &IndexProducerTaskRequestV1<'_>,
  hash_algorithm: HashAlgorithm,
  plan: IndexWorkspaceProducerTaskPlanV1,
) -> FormatResult<Vec<u8>> {
  let hash_width = hash_algorithm.hash_length();
  let mut encoded = Vec::new();
  encoded.try_reserve_exact(plan.payload_length).map_err(|error| allocation_error(format!("producer task allocation failed: {error}")))?;
  encoded.resize(plan.payload_length, 0);
  encoded[..4].copy_from_slice(PRODUCER_TASK_MAGIC);
  put_u16(&mut encoded, 4, PRODUCER_TASK_SCHEMA_VERSION);
  put_u16(&mut encoded, 6, plan.header_length as u16);
  put_u64(&mut encoded, 8, plan.payload_length as u64);
  encoded[16..32].copy_from_slice(&task.operation_id);
  put_u16(&mut encoded, 32, task.kind.id());
  let flags = if task.journal_head.is_some() { PRODUCER_TASK_FLAG_JOURNAL } else { PRODUCER_TASK_FLAG_SCOPE };
  put_u16(&mut encoded, 34, flags);
  put_u64(&mut encoded, 36, task.publication_sequence);
  put_u16(&mut encoded, 44, hash_width as u16);
  put_u32(&mut encoded, 48, plan.scope_length as u32);
  let mut cursor = PRODUCER_TASK_FIXED_HEADER_LENGTH;
  for hash in [task.namespace_root_before, task.namespace_root_after, task.semantic_state_root] {
    encoded[cursor..cursor + hash_width].copy_from_slice(hash);
    cursor += hash_width;
  }
  if let Some(journal) = task.journal_head {
    encoded[cursor..cursor + hash_width].copy_from_slice(journal);
  }
  cursor += hash_width;
  if let Some(scope) = task.scope {
    encoded[cursor..].copy_from_slice(scope.as_bytes());
  }
  Ok(encoded)
}

pub fn decode_index_workspace_producer_task_payload_v1(
  bytes: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexWorkspaceProducerTaskPayloadV1<'_>> {
  let (header_length, maximum_length) = index_workspace_producer_task_payload_bounds_v1(hash_algorithm)?;
  let hash_width = hash_algorithm.hash_length();
  if bytes.len() < header_length || bytes.len() > maximum_length {
    return Err(length_error("producer task payload is outside its fixed bounds"));
  }
  if &bytes[..4] != PRODUCER_TASK_MAGIC || u16_at(bytes, 4) != PRODUCER_TASK_SCHEMA_VERSION {
    return Err(version_error("producer task payload magic or version is unknown"));
  }
  let scope_length = u32_at(bytes, 48) as usize;
  if usize::from(u16_at(bytes, 6)) != header_length
    || usize_from_u64(u64_at(bytes, 8), "producer task total length")? != bytes.len()
    || u16_at(bytes, 44) as usize != hash_width
    || header_length.checked_add(scope_length) != Some(bytes.len())
  {
    return Err(length_error("producer task header, hash width, scope, or total length is not canonical"));
  }
  if bytes[46..48].iter().chain(bytes[52..56].iter()).any(|byte| *byte != 0) {
    return Err(reserve_error("producer task reserved bytes are nonzero"));
  }
  let operation_id = array_at(bytes, 16);
  let kind = IndexProducerTaskKindV1::from_id(u16_at(bytes, 32)).ok_or_else(|| kind_error("producer task kind is unknown"))?;
  let flags = u16_at(bytes, 34);
  let publication_sequence = u64_at(bytes, 36);
  let mut cursor = PRODUCER_TASK_FIXED_HEADER_LENGTH;
  let namespace_root_before = &bytes[cursor..cursor + hash_width];
  cursor += hash_width;
  let namespace_root_after = &bytes[cursor..cursor + hash_width];
  cursor += hash_width;
  let semantic_state_root = &bytes[cursor..cursor + hash_width];
  cursor += hash_width;
  let journal_bytes = &bytes[cursor..cursor + hash_width];
  cursor += hash_width;
  let journal_head = (flags == PRODUCER_TASK_FLAG_JOURNAL).then_some(journal_bytes);
  let scope = if flags == PRODUCER_TASK_FLAG_SCOPE {
    match std::str::from_utf8(&bytes[cursor..]) {
      Ok(scope) => Some(scope),
      Err(error) => return Err(closure_error(format!("producer task scope is not UTF-8: {error}"))),
    }
  } else {
    None
  };
  if !matches!(flags, PRODUCER_TASK_FLAG_JOURNAL | PRODUCER_TASK_FLAG_SCOPE)
    || (flags == PRODUCER_TASK_FLAG_JOURNAL && scope_length != 0)
    || (flags == PRODUCER_TASK_FLAG_SCOPE && (!journal_bytes.iter().all(|byte| *byte == 0) || scope_length == 0))
  {
    return Err(closure_error("producer task flags, journal, and scope closure is invalid"));
  }
  validate_producer_task_fields(
    operation_id,
    kind,
    publication_sequence,
    namespace_root_before,
    namespace_root_after,
    semantic_state_root,
    journal_head,
    scope,
    hash_algorithm,
  )?;
  Ok(IndexWorkspaceProducerTaskPayloadV1 {
    operation_id,
    kind,
    publication_sequence,
    namespace_root_before,
    namespace_root_after,
    semantic_state_root,
    journal_head,
    scope,
  })
}

pub(super) fn index_workspace_producer_task_payload_bounds_v1(hash_algorithm: HashAlgorithm) -> FormatResult<(usize, usize)> {
  let hash_width = hash_algorithm.hash_length();
  let header_length = PRODUCER_TASK_FIXED_HEADER_LENGTH
    .checked_add(4usize.checked_mul(hash_width).ok_or_else(|| length_error("producer task hash area overflowed"))?)
    .ok_or_else(|| length_error("producer task header length overflowed"))?;
  let maximum_length =
    header_length.checked_add(PRODUCER_TASK_SCOPE_LIMIT).ok_or_else(|| length_error("producer task maximum length overflowed"))?;
  Ok((header_length, maximum_length))
}

pub(super) fn validate_index_workspace_object_payload_v1(
  kind: IndexWorkspaceObjectKindV1,
  payload: &[u8],
  hash_algorithm: HashAlgorithm,
  logical_record_count: u64,
  minimum_publication_sequence: u64,
  maximum_publication_sequence: u64,
) -> FormatResult<()> {
  match kind {
    IndexWorkspaceObjectKindV1::RuntimeBatch => {
      let decoded = decode_index_workspace_runtime_batch_payload(payload, hash_algorithm)?;
      let (decoded_count, decoded_minimum, decoded_maximum) = match decoded {
        IndexWorkspaceRuntimeBatchPayload::V1(decoded) => {
          let Some(first) = decoded.records.first() else {
            return Err(closure_error("runtime object payload is empty"));
          };
          let (minimum, maximum) =
            decoded.records.iter().skip(1).fold((first.publication_sequence, first.publication_sequence), |(minimum, maximum), record| {
              (minimum.min(record.publication_sequence), maximum.max(record.publication_sequence))
            });
          (decoded.records.len() as u64, minimum, maximum)
        }
        IndexWorkspaceRuntimeBatchPayload::V2(decoded) => {
          let count = decoded
            .mutations
            .len()
            .checked_add(decoded.transitions.len())
            .ok_or_else(|| closure_error("runtime v2 object logical record count overflowed"))? as u64;
          let mut sequences = decoded
            .mutations
            .iter()
            .map(|mutation| mutation.publication_sequence)
            .chain(decoded.transitions.iter().map(|transition| transition.publication_sequence));
          let Some(first) = sequences.next() else {
            return Err(closure_error("runtime v2 object payload is empty"));
          };
          let (minimum, maximum) =
            sequences.fold((first, first), |(minimum, maximum), sequence| (minimum.min(sequence), maximum.max(sequence)));
          (count, minimum, maximum)
        }
      };
      if decoded_count != logical_record_count
        || decoded_minimum != minimum_publication_sequence
        || decoded_maximum != maximum_publication_sequence
      {
        return Err(closure_error("runtime object counters do not close over its payload"));
      }
    }
    IndexWorkspaceObjectKindV1::ProducerTask => {
      let decoded = decode_index_workspace_producer_task_payload_v1(payload, hash_algorithm)?;
      if logical_record_count != 1
        || decoded.publication_sequence != minimum_publication_sequence
        || decoded.publication_sequence != maximum_publication_sequence
      {
        return Err(closure_error("producer task object counters do not close over its payload"));
      }
    }
  }
  Ok(())
}

fn validate_runtime_mutation(record: &PublishedIndexMutationV1, hash_algorithm: HashAlgorithm) -> FormatResult<()> {
  if record.index_id().len() != hash_algorithm.hash_length()
    || record.index_id().iter().all(|byte| *byte == 0)
    || record.role() == OrderedIndexRoleV1::NvtTile
    || record.publication_sequence() == 0
    || record.operation_id() == [0; 16]
    || record.order_key().is_empty()
    || record.order_key().len() > RUNTIME_ORDER_KEY_LIMIT
    || record.encoded_record().is_empty()
    || record.encoded_record().len() > RUNTIME_ENCODED_RECORD_LIMIT
  {
    return Err(closure_error("runtime mutation identity, role, publication sequence, or lengths are invalid"));
  }
  let decoded = decode_ordered_record(record.encoded_record(), hash_algorithm, record.role())?;
  if ordered_record_order_key(&decoded)?.as_slice() != record.order_key() {
    return Err(closure_error("runtime mutation order key does not match its encoded record"));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_producer_task_fields(
  operation_id: [u8; 16],
  kind: IndexProducerTaskKindV1,
  publication_sequence: u64,
  namespace_root_before: &[u8],
  namespace_root_after: &[u8],
  semantic_state_root: &[u8],
  journal_head: Option<&[u8]>,
  scope: Option<&str>,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<()> {
  let hash_width = hash_algorithm.hash_length();
  if operation_id == [0; 16] || publication_sequence == 0 {
    return Err(closure_error("producer task operation identity or publication sequence is zero"));
  }
  for hash in [namespace_root_before, namespace_root_after, semantic_state_root] {
    if hash.len() != hash_width || hash.iter().all(|byte| *byte == 0) {
      return Err(closure_error("producer task root has the wrong width or is all zeroes"));
    }
  }
  if kind.requires_journal() {
    let journal = journal_head.ok_or_else(|| closure_error("journal producer task lacks its journal head"))?;
    if namespace_root_before == namespace_root_after
      || journal.len() != hash_width
      || journal.iter().all(|byte| *byte == 0)
      || scope.is_some()
    {
      return Err(closure_error("journal producer task has invalid root, journal, or scope closure"));
    }
  } else {
    let scope = scope.ok_or_else(|| closure_error("root-pinned producer task lacks its scope"))?;
    if namespace_root_before != namespace_root_after
      || journal_head.is_some()
      || scope.is_empty()
      || !scope.starts_with('/')
      || scope.len() > PRODUCER_TASK_SCOPE_LIMIT
      || normalize_path(scope) != scope
    {
      return Err(closure_error("root-pinned producer task has invalid root, journal, or scope closure"));
    }
  }
  Ok(())
}

fn compare_runtime_mutations(left: &PublishedIndexMutationV1, right: &PublishedIndexMutationV1) -> Ordering {
  left.index_id().cmp(right.index_id()).then(left.role().id().cmp(&right.role().id())).then(left.order_key().cmp(right.order_key()))
}

pub(super) fn compare_decoded_runtime_mutations(
  left: &IndexWorkspaceRuntimeMutationV1<'_>,
  right: &IndexWorkspaceRuntimeMutationV1<'_>,
) -> Ordering {
  left.index_id.cmp(right.index_id).then(left.role.id().cmp(&right.role.id())).then(left.order_key.cmp(right.order_key))
}

fn runtime_mutation_frame_header(record: &PublishedIndexMutationV1) -> FormatResult<[u8; RUNTIME_MUTATION_FRAME_LENGTH]> {
  let frame_length = checked_runtime_frame_length(record.index_id().len(), record.order_key().len(), record.encoded_record().len())?;
  let frame_length = u32::try_from(frame_length).map_err(|error| length_error(format!("runtime mutation frame exceeds u32: {error}")))?;
  let index_length =
    u16::try_from(record.index_id().len()).map_err(|error| length_error(format!("runtime mutation index ID exceeds u16: {error}")))?;
  let order_length =
    u32::try_from(record.order_key().len()).map_err(|error| length_error(format!("runtime mutation order key exceeds u32: {error}")))?;
  let record_length = u32::try_from(record.encoded_record().len())
    .map_err(|error| length_error(format!("runtime mutation encoded record exceeds u32: {error}")))?;
  let mut frame = [0u8; RUNTIME_MUTATION_FRAME_LENGTH];
  put_u32(&mut frame, 0, frame_length);
  frame[4] = record.role().id();
  put_u16(&mut frame, 6, index_length);
  put_u64(&mut frame, 8, record.publication_sequence());
  frame[16..32].copy_from_slice(&record.operation_id());
  put_u32(&mut frame, 32, order_length);
  put_u32(&mut frame, 36, record_length);
  Ok(frame)
}

fn checked_runtime_frame_length(index_length: usize, order_length: usize, record_length: usize) -> FormatResult<usize> {
  RUNTIME_MUTATION_FRAME_LENGTH
    .checked_add(index_length)
    .and_then(|length| length.checked_add(order_length))
    .and_then(|length| length.checked_add(record_length))
    .ok_or_else(|| length_error("runtime mutation frame length overflowed"))
}

fn usize_from_u64(value: u64, context: &'static str) -> FormatResult<usize> {
  usize::try_from(value).map_err(|error| length_error(format!("{context} exceeds this platform: {error}")))
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
  let mut encoded = [0; 2];
  encoded.copy_from_slice(&bytes[offset..offset + 2]);
  u16::from_le_bytes(encoded)
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
  let mut encoded = [0; 4];
  encoded.copy_from_slice(&bytes[offset..offset + 4]);
  u32::from_le_bytes(encoded)
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
  let mut encoded = [0; 8];
  encoded.copy_from_slice(&bytes[offset..offset + 8]);
  u64::from_le_bytes(encoded)
}

fn array_at<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
  let mut encoded = [0; N];
  encoded.copy_from_slice(&bytes[offset..offset + N]);
  encoded
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
  FormatError::new(MalformedInputClass::UnknownMagicOrVersion, "index_workspace_payload_version", context)
}

fn kind_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, "index_workspace_payload_kind", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::TruncationOrTrailingBytes, "index_workspace_payload_length", context)
}

fn reserve_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NonzeroReservedOrPadding, "index_workspace_payload_reserved", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, "index_workspace_payload_closure", context)
}

fn ordering_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NoncanonicalOrderOrDuplicate, "index_workspace_payload_order", context)
}

fn allocation_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::AllocationAmplification, "index_workspace_payload_allocation", context)
}
