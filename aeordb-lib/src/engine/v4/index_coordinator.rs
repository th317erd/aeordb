use std::collections::TryReserveError;
use std::mem::size_of;

use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{
  AdmissionClass, CriticalMemoryPurpose, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryPressure, MemoryReservation,
};

use super::index_page::{OrderedIndexRoleV1, checked_ordered_record_order_key_length, decode_ordered_record, ordered_record_order_key};
use super::reader::MalformedInputClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCoordinatorOptionsV1 {
  pub mutation_buffer_max_bytes: u64,
  pub flush_after_mutations: u64,
  pub flush_after_ms: u64,
  pub publication_batch_max_bytes: u64,
}

fn validate_membership_states(
  owner_class: IndexMembershipOwnerClassV1,
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
) -> Result<(), IndexCoordinatorErrorV1> {
  if (before.live && before.unindexable)
    || (after.live && after.unindexable)
    || (owner_class == IndexMembershipOwnerClassV1::ScopeCatalog && (before.unindexable || after.unindexable))
  {
    return Err(IndexCoordinatorErrorV1::InvalidMutation("membership state is contradictory for its owner class".to_string()));
  }
  Ok(())
}

fn membership_position(groups: &[RetainedIndexGroupV1], owner_id: &[u8], document_ordinal: u64) -> (usize, bool) {
  let position = groups
    .partition_point(|group| group.key.owner_id.as_slice().cmp(owner_id).then(group.key.document_ordinal.cmp(&document_ordinal)).is_lt());
  let exists = groups.get(position).is_some_and(|group| group.key.owner_id == owner_id && group.key.document_ordinal == document_ordinal);
  (position, exists)
}

fn group_mutation_position(
  records: &[RetainedGroupMutationV1],
  index_id: &[u8],
  role: OrderedIndexRoleV1,
  order_key: &[u8],
) -> (usize, bool) {
  let position = records.partition_point(|record| compare_key(&record.key, index_id, role, order_key).is_lt());
  let exists = records.get(position).is_some_and(|record| compare_key(&record.key, index_id, role, order_key).is_eq());
  (position, exists)
}

fn group_request_is_duplicate(
  memory: &MemoryCoordinator,
  mutation_buffer_max_bytes: u64,
  retained: &RetainedIndexGroupV1,
  request: &IndexMutationGroupRequestV1<'_>,
  hash_algorithm: HashAlgorithm,
) -> Result<bool, IndexCoordinatorErrorV1> {
  let transition = request.transition;
  if transition.owner_class != retained.owner_class
    || transition.operation_id != retained.operation_id
    || transition.before != retained.before
    || transition.after != retained.after
    || request.mutations.len() != retained.records.len()
  {
    return Ok(false);
  }
  if request.mutations.is_empty() {
    return Ok(true);
  }
  let seen_words =
    request.mutations.len().checked_add(63).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("duplicate group bitset words"))? / 64;
  let seen_bytes = checked_footprint(seen_words.checked_mul(size_of::<u64>()), "duplicate group bitset bytes")?;
  let _seen_reservation = memory
    .reserve(MemoryOwner::IndexDirtyBuffers, seen_bytes, AdmissionClass::Workload)
    .map_err(|error| map_reservation_error(error, seen_bytes, mutation_buffer_max_bytes, "duplicate group validation"))?;
  let mut seen = Vec::new();
  seen.try_reserve_exact(seen_words).map_err(|error| allocation_error("duplicate group bitset", error))?;
  seen.resize(seen_words, 0u64);
  for grouped in request.mutations {
    let decoded = decode_ordered_record(grouped.mutation.encoded_record, hash_algorithm, grouped.mutation.role)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    let order_key = ordered_record_order_key(&decoded)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    let (position, exists) = group_mutation_position(&retained.records, grouped.mutation.index_id, grouped.mutation.role, &order_key);
    let Some(record) = exists.then(|| &retained.records[position]) else {
      return Ok(false);
    };
    let word = position / 64;
    let mask = 1u64 << (position % 64);
    if seen[word] & mask != 0 {
      return Ok(false);
    }
    seen[word] |= mask;
    if record.operation != grouped.operation
      || record.publication_sequence != grouped.mutation.publication_sequence
      || record.operation_id != grouped.mutation.operation_id
      || record.encoded_record != grouped.mutation.encoded_record
    {
      return Ok(false);
    }
  }
  Ok(true)
}

fn build_retained_group(
  memory: &MemoryCoordinator,
  mutation_buffer_max_bytes: u64,
  publication_batch_max_bytes: u64,
  hash_algorithm: HashAlgorithm,
  existing: Option<&RetainedIndexGroupV1>,
  request: &IndexMutationGroupRequestV1<'_>,
  now_ms: u64,
) -> Result<RetainedIndexGroupV1, IndexCoordinatorErrorV1> {
  let mut requested_bytes =
    checked_footprint(group_base_retained_bytes(request.transition.owner_id.len(), 1), "semantic group requested base bytes")?;
  if let Some(existing) = existing {
    requested_bytes = requested_bytes
      .checked_add(existing.retained_bytes)
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group replacement bytes"))?;
  }
  for grouped in request.mutations {
    let decoded = decode_ordered_record(grouped.mutation.encoded_record, hash_algorithm, grouped.mutation.role)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    let order_key_length = checked_ordered_record_order_key_length(&decoded)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    requested_bytes = requested_bytes
      .checked_add(group_retained_mutation_bytes(grouped.mutation.index_id.len(), order_key_length, grouped.mutation.encoded_record.len())?)
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group requested bytes"))?;
  }
  let mut reservation = memory
    .reserve(MemoryOwner::IndexDirtyBuffers, requested_bytes, AdmissionClass::Workload)
    .map_err(|error| map_reservation_error(error, requested_bytes, mutation_buffer_max_bytes, "semantic group admission"))?;

  let mut incoming = Vec::new();
  incoming.try_reserve_exact(request.mutations.len()).map_err(|error| allocation_error("semantic group incoming records", error))?;
  for grouped in request.mutations {
    let mutation = grouped.mutation;
    let decoded = decode_ordered_record(mutation.encoded_record, hash_algorithm, mutation.role)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    let order_key = ordered_record_order_key(&decoded)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    incoming.push(RetainedGroupMutationV1 {
      key: MutationKeyV1 { index_id: copy_bytes(mutation.index_id)?, role_id: mutation.role.id(), order_key },
      role: mutation.role,
      operation: grouped.operation,
      publication_sequence: mutation.publication_sequence,
      operation_id: mutation.operation_id,
      encoded_record: copy_bytes(mutation.encoded_record)?,
      observed_mutations: 1,
    });
  }
  incoming.sort_unstable_by(|left, right| left.key.cmp(&right.key));
  if incoming.windows(2).any(|pair| pair[0].key == pair[1].key) {
    return Err(IndexCoordinatorErrorV1::InvalidMutation("semantic group contains duplicate mutation keys".to_string()));
  }

  let existing_count = existing.map_or(0, |group| group.records.len());
  let mut records = Vec::new();
  let merged_record_count =
    existing_count.checked_add(incoming.len()).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group merged record count"))?;
  records.try_reserve_exact(merged_record_count).map_err(|error| allocation_error("semantic group merged records", error))?;
  if let Some(existing) = existing {
    for record in &existing.records {
      records.push(clone_group_mutation(record)?);
    }
  }
  for mutation in incoming {
    let position = records.partition_point(|record| record.key < mutation.key);
    if records.get(position).is_some_and(|record| record.key == mutation.key) {
      let retained = &records[position];
      if mutation.publication_sequence < retained.publication_sequence {
        return Err(IndexCoordinatorErrorV1::StaleMutation {
          received_sequence: mutation.publication_sequence,
          retained_sequence: retained.publication_sequence,
        });
      }
      if mutation.publication_sequence == retained.publication_sequence {
        if mutation.operation != retained.operation
          || mutation.operation_id != retained.operation_id
          || mutation.encoded_record != retained.encoded_record
        {
          return Err(IndexCoordinatorErrorV1::ConflictingMutation { publication_sequence: mutation.publication_sequence });
        }
        records[position].observed_mutations = records[position]
          .observed_mutations
          .checked_add(1)
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group duplicate observations"))?;
      } else {
        let observed_mutations = records[position]
          .observed_mutations
          .checked_add(1)
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group replacement observations"))?;
        records[position] = RetainedGroupMutationV1 { observed_mutations, ..mutation };
      }
    } else {
      records.insert(position, mutation);
    }
  }

  let transition_publication_bytes = membership_publication_bytes(request.transition.owner_id.len())?;
  let publication_bytes = records.iter().try_fold(transition_publication_bytes, |bytes, record| {
    bytes
      .checked_add(publication_bytes(record.key.index_id.len(), record.key.order_key.len(), record.encoded_record.len())?)
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group publication bytes"))
  })?;
  if publication_bytes > publication_batch_max_bytes {
    return Err(IndexCoordinatorErrorV1::SpillRequired {
      context: "one semantic group exceeds the publication batch limit".to_string(),
      requested_bytes: publication_bytes,
      limit_bytes: publication_batch_max_bytes,
    });
  }
  const { assert!(usize::BITS <= u64::BITS) };
  let observed_mutations = existing
    .map_or(0, |group| group.observed_mutations)
    .checked_add(request.mutations.len().max(1) as u64)
    .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group observations"))?;
  let actual_retained_bytes = records.iter().try_fold(
    checked_footprint(group_base_retained_bytes(request.transition.owner_id.len(), 1), "semantic group actual base bytes")?,
    |bytes, record| {
      bytes
        .checked_add(group_retained_mutation_bytes(record.key.index_id.len(), record.key.order_key.len(), record.encoded_record.len())?)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic group actual retained bytes"))
    },
  )?;
  if actual_retained_bytes > requested_bytes {
    reservation
      .grow(actual_retained_bytes - requested_bytes)
      .map_err(|error| map_reservation_error(error, actual_retained_bytes, mutation_buffer_max_bytes, "semantic group exact growth"))?;
  } else if requested_bytes > actual_retained_bytes {
    reservation.shrink(requested_bytes - actual_retained_bytes).map_err(memory_authority_error)?;
  }
  let mut reservations = Vec::new();
  reservations.try_reserve_exact(1).map_err(|error| allocation_error("semantic group reservation slot", error))?;
  reservations.push(reservation);
  Ok(RetainedIndexGroupV1 {
    key: MembershipKeyV1 { owner_id: copy_bytes(request.transition.owner_id)?, document_ordinal: request.transition.document_ordinal },
    owner_class: request.transition.owner_class,
    publication_sequence: request.transition.publication_sequence,
    operation_id: request.transition.operation_id,
    before: existing.map_or(request.transition.before, |group| group.before),
    after: request.transition.after,
    records,
    observed_mutations,
    first_seen_ms: existing.map_or(now_ms, |group| group.first_seen_ms),
    retained_bytes: actual_retained_bytes,
    publication_bytes,
    _reservations: reservations,
  })
}

fn clone_group_mutation(record: &RetainedGroupMutationV1) -> Result<RetainedGroupMutationV1, IndexCoordinatorErrorV1> {
  Ok(RetainedGroupMutationV1 {
    key: MutationKeyV1 {
      index_id: copy_bytes(&record.key.index_id)?,
      role_id: record.key.role_id,
      order_key: copy_bytes(&record.key.order_key)?,
    },
    role: record.role,
    operation: record.operation,
    publication_sequence: record.publication_sequence,
    operation_id: record.operation_id,
    encoded_record: copy_bytes(&record.encoded_record)?,
    observed_mutations: record.observed_mutations,
  })
}

fn group_retained_mutation_bytes(
  index_id_length: usize,
  order_key_length: usize,
  record_length: usize,
) -> Result<u64, IndexCoordinatorErrorV1> {
  checked_footprint(
    size_of::<RetainedGroupMutationV1>()
      .checked_add(size_of::<MutationKeyV1>())
      .and_then(|bytes| bytes.checked_add(index_id_length))
      .and_then(|bytes| bytes.checked_add(order_key_length))
      .and_then(|bytes| bytes.checked_add(record_length)),
    "retained semantic-group mutation bytes",
  )
}

fn group_base_retained_bytes(owner_id_length: usize, reservation_count: usize) -> Option<usize> {
  size_of::<RetainedIndexGroupV1>()
    .checked_add(size_of::<MembershipKeyV1>())
    .and_then(|bytes| bytes.checked_add(owner_id_length))
    .and_then(|bytes| bytes.checked_add(size_of::<MemoryReservation>().checked_mul(reservation_count)?))
}

fn membership_publication_bytes(owner_id_length: usize) -> Result<u64, IndexCoordinatorErrorV1> {
  checked_footprint(
    size_of::<PublishedIndexMembershipTransitionV1>().checked_add(owner_id_length),
    "membership transition publication bytes",
  )
}

fn published_group_mutation(record: &RetainedGroupMutationV1) -> Result<PublishedIndexMutationV1, IndexCoordinatorErrorV1> {
  Ok(PublishedIndexMutationV1 {
    index_id: copy_bytes(&record.key.index_id)?,
    role: record.role,
    operation: record.operation,
    publication_sequence: record.publication_sequence,
    operation_id: record.operation_id,
    order_key: copy_bytes(&record.key.order_key)?,
    encoded_record: copy_bytes(&record.encoded_record)?,
  })
}

fn published_transition(group: &RetainedIndexGroupV1) -> Result<PublishedIndexMembershipTransitionV1, IndexCoordinatorErrorV1> {
  Ok(PublishedIndexMembershipTransitionV1 {
    owner_id: copy_bytes(&group.key.owner_id)?,
    owner_class: group.owner_class,
    publication_sequence: group.publication_sequence,
    operation_id: group.operation_id,
    document_ordinal: group.key.document_ordinal,
    before: group.before,
    after: group.after,
  })
}

fn merge_restored_group(active: &mut RetainedIndexGroupV1, mut restored: RetainedIndexGroupV1) -> Result<(), IndexCoordinatorErrorV1> {
  let target_retained_bytes = merged_group_retained_bytes(active, &restored)?;
  let current_retained_bytes = active
    .retained_bytes
    .checked_add(restored.retained_bytes)
    .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic retained bytes"))?;
  let surplus = current_retained_bytes
    .checked_sub(target_retained_bytes)
    .ok_or_else(|| IndexCoordinatorErrorV1::Invariant("restored semantic composition exceeds its existing reservations".to_string()))?;
  shrink_reservations_by(&mut restored._reservations, surplus)?;
  active.before = restored.before;
  active.first_seen_ms = active.first_seen_ms.min(restored.first_seen_ms);
  active.observed_mutations = active
    .observed_mutations
    .checked_add(restored.observed_mutations)
    .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic observations"))?;
  for record in restored.records.drain(..) {
    let position = active.records.partition_point(|candidate| candidate.key < record.key);
    if active.records.get(position).is_some_and(|candidate| candidate.key == record.key) {
      if active.records[position].publication_sequence < record.publication_sequence {
        return Err(IndexCoordinatorErrorV1::Invariant("restored semantic mutation is newer than its active successor".to_string()));
      }
      active.records[position].observed_mutations = active.records[position]
        .observed_mutations
        .checked_add(record.observed_mutations)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic mutation observations"))?;
    } else {
      active.records.insert(position, record);
    }
  }
  active._reservations.append(&mut restored._reservations);
  active.retained_bytes = target_retained_bytes;
  active.publication_bytes =
    active.records.iter().try_fold(membership_publication_bytes(active.key.owner_id.len())?, |bytes, record| {
      bytes
        .checked_add(publication_bytes(record.key.index_id.len(), record.key.order_key.len(), record.encoded_record.len())?)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic publication bytes"))
    })?;
  Ok(())
}

fn merged_group_retained_bytes(active: &RetainedIndexGroupV1, restored: &RetainedIndexGroupV1) -> Result<u64, IndexCoordinatorErrorV1> {
  let reservation_count = active
    ._reservations
    .len()
    .checked_add(restored._reservations.len())
    .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic reservation count"))?;
  let base =
    checked_footprint(group_base_retained_bytes(active.key.owner_id.len(), reservation_count), "restored semantic group base bytes")?;
  let active_bytes = active.records.iter().try_fold(base, |bytes, record| {
    bytes
      .checked_add(group_retained_mutation_bytes(record.key.index_id.len(), record.key.order_key.len(), record.encoded_record.len())?)
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored active semantic record bytes"))
  })?;
  restored.records.iter().try_fold(active_bytes, |bytes, record| {
    let (position, exists) = group_mutation_position(&active.records, &record.key.index_id, record.role, &record.key.order_key);
    if exists {
      let active_record = &active.records[position];
      if active_record.publication_sequence < record.publication_sequence {
        return Err(IndexCoordinatorErrorV1::Invariant("restored semantic mutation is newer than its active successor".to_string()));
      }
      Ok(bytes)
    } else {
      bytes
        .checked_add(group_retained_mutation_bytes(record.key.index_id.len(), record.key.order_key.len(), record.encoded_record.len())?)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored unique semantic record bytes"))
    }
  })
}

fn shrink_reservations_by(reservations: &mut [MemoryReservation], mut bytes: u64) -> Result<(), IndexCoordinatorErrorV1> {
  for reservation in reservations.iter_mut().rev() {
    if bytes == 0 {
      return Ok(());
    }
    let released = bytes.min(reservation.bytes());
    reservation.shrink(released).map_err(memory_authority_error)?;
    bytes -= released;
  }
  if bytes == 0 {
    Ok(())
  } else {
    Err(IndexCoordinatorErrorV1::Invariant("restored semantic reservations cannot release their superseded bytes".to_string()))
  }
}

impl IndexCoordinatorOptionsV1 {
  pub fn new(
    mutation_buffer_max_bytes: u64,
    flush_after_mutations: u64,
    flush_after_ms: u64,
    publication_batch_max_bytes: u64,
  ) -> Result<Self, IndexCoordinatorErrorV1> {
    if mutation_buffer_max_bytes == 0 {
      return Err(IndexCoordinatorErrorV1::InvalidOptions("mutation buffer limit must be nonzero".to_string()));
    }
    if flush_after_mutations == 0 {
      return Err(IndexCoordinatorErrorV1::InvalidOptions("mutation flush count must be nonzero".to_string()));
    }
    if flush_after_ms == 0 {
      return Err(IndexCoordinatorErrorV1::InvalidOptions("mutation flush age must be nonzero".to_string()));
    }
    if publication_batch_max_bytes == 0 {
      return Err(IndexCoordinatorErrorV1::InvalidOptions("publication batch limit must be nonzero".to_string()));
    }
    Ok(Self { mutation_buffer_max_bytes, flush_after_mutations, flush_after_ms, publication_batch_max_bytes })
  }
}

#[derive(Debug, Clone, Copy)]
pub struct IndexMutationRequestV1<'a> {
  pub index_id: &'a [u8],
  pub role: OrderedIndexRoleV1,
  pub publication_sequence: u64,
  pub operation_id: [u8; 16],
  pub encoded_record: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexMutationOperationV1 {
  Upsert = 1,
  RemoveExisting = 2,
}

impl IndexMutationOperationV1 {
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
pub enum IndexMembershipOwnerClassV1 {
  ScopeCatalog = 1,
  ValueStore = 2,
  FieldIndex = 3,
}

impl IndexMembershipOwnerClassV1 {
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
pub struct IndexMembershipStateV1 {
  pub live: bool,
  pub unindexable: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexMembershipTransitionRequestV1<'a> {
  pub owner_id: &'a [u8],
  pub owner_class: IndexMembershipOwnerClassV1,
  pub publication_sequence: u64,
  pub operation_id: [u8; 16],
  pub document_ordinal: u64,
  pub before: IndexMembershipStateV1,
  pub after: IndexMembershipStateV1,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexGroupMutationRequestV1<'a> {
  pub operation: IndexMutationOperationV1,
  pub mutation: IndexMutationRequestV1<'a>,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexMutationGroupRequestV1<'a> {
  pub transition: IndexMembershipTransitionRequestV1<'a>,
  pub mutations: &'a [IndexGroupMutationRequestV1<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMutationGroupAdmissionV1 {
  Inserted,
  Replaced,
  Duplicate,
}

impl IndexMutationGroupAdmissionV1 {
  pub const fn is_duplicate(self) -> bool {
    matches!(self, Self::Duplicate)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMutationAdmissionV1 {
  Inserted,
  Replaced,
  Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexFlushReasonV1 {
  MutationCount,
  Age,
  MemoryPressure,
  Explicit,
  Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexCoordinatorLifecycleV1 {
  Running,
  Draining,
  Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCoordinatorSnapshotV1 {
  pub lifecycle: IndexCoordinatorLifecycleV1,
  pub active_records: u64,
  pub active_mutations: u64,
  pub active_bytes: u64,
  pub active_groups: u64,
  pub frozen_records: u64,
  pub frozen_mutations: u64,
  pub frozen_bytes: u64,
  pub frozen_groups: u64,
  pub successful_flushes: u64,
  pub restored_flushes: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexCoordinatorErrorV1 {
  #[error("invalid index coordinator options: {0}")]
  InvalidOptions(String),
  #[error("index coordinator identity is all zeroes")]
  InvalidCoordinatorIdentity,
  #[error("invalid index mutation: {0}")]
  InvalidMutation(String),
  #[error("malformed ordered index record ({class:?}): {message}")]
  MalformedRecord { class: MalformedInputClass, message: String },
  #[error("index mutation sequence {received_sequence} is older than retained sequence {retained_sequence}")]
  StaleMutation { received_sequence: u64, retained_sequence: u64 },
  #[error("index mutation sequence {publication_sequence} conflicts with retained operation identity or bytes")]
  ConflictingMutation { publication_sequence: u64 },
  #[error("grouped and legacy mutation admission cannot share one dirty coordinator generation")]
  MixedAdmissionMode,
  #[error("index owner/document transition conflicts at publication sequence {publication_sequence}")]
  ConflictingGroupTransition { publication_sequence: u64 },
  #[error("index state requires spill or reconciliation: {context}; requested={requested_bytes}, limit={limit_bytes}")]
  SpillRequired { context: String, requested_bytes: u64, limit_bytes: u64 },
  #[error("index coordinator memory authority failed: {0}")]
  MemoryAuthority(String),
  #[error("index coordinator allocation failed: {0}")]
  Allocation(String),
  #[error("index coordinator clock regressed from {previous_ms} to {received_ms}")]
  ClockRegressed { previous_ms: u64, received_ms: u64 },
  #[error("index flush is already in progress as batch {batch_id}")]
  FlushInProgress { batch_id: u64 },
  #[error("index flush has no frozen batch to retry")]
  NoFlushInProgress,
  #[error("index flush batch does not belong to this coordinator")]
  ForeignBatch,
  #[error("index flush batch does not match the in-flight batch")]
  StaleBatch,
  #[error("index flush was cancelled before state was frozen")]
  Cancelled,
  #[error("index coordinator is not running: {lifecycle:?}")]
  NotRunning { lifecycle: IndexCoordinatorLifecycleV1 },
  #[error("index coordinator cannot stop while dirty or frozen state remains")]
  DrainIncomplete,
  #[error("index coordinator accounting overflow: {0}")]
  AccountingOverflow(&'static str),
  #[error("index coordinator invariant failed: {0}")]
  Invariant(String),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MutationKeyV1 {
  index_id: Vec<u8>,
  role_id: u8,
  order_key: Vec<u8>,
}

struct RetainedMutationV1 {
  key: MutationKeyV1,
  role: OrderedIndexRoleV1,
  operation: IndexMutationOperationV1,
  publication_sequence: u64,
  operation_id: [u8; 16],
  encoded_record: Vec<u8>,
  observed_mutations: u64,
  first_seen_ms: u64,
  retained_bytes: u64,
  _reservation: MemoryReservation,
}

impl RetainedMutationV1 {
  fn publication_bytes(&self) -> Result<u64, IndexCoordinatorErrorV1> {
    publication_bytes(self.key.index_id.len(), self.key.order_key.len(), self.encoded_record.len())
  }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MembershipKeyV1 {
  owner_id: Vec<u8>,
  document_ordinal: u64,
}

struct RetainedGroupMutationV1 {
  key: MutationKeyV1,
  role: OrderedIndexRoleV1,
  operation: IndexMutationOperationV1,
  publication_sequence: u64,
  operation_id: [u8; 16],
  encoded_record: Vec<u8>,
  observed_mutations: u64,
}

struct RetainedIndexGroupV1 {
  key: MembershipKeyV1,
  owner_class: IndexMembershipOwnerClassV1,
  publication_sequence: u64,
  operation_id: [u8; 16],
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
  records: Vec<RetainedGroupMutationV1>,
  observed_mutations: u64,
  first_seen_ms: u64,
  retained_bytes: u64,
  publication_bytes: u64,
  _reservations: Vec<MemoryReservation>,
}

struct FrozenStateV1 {
  batch_id: u64,
  attempt_id: u64,
  reason: IndexFlushReasonV1,
  records: Vec<RetainedMutationV1>,
  groups: Vec<RetainedIndexGroupV1>,
  mutations: u64,
  bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedIndexMutationV1 {
  index_id: Vec<u8>,
  role: OrderedIndexRoleV1,
  operation: IndexMutationOperationV1,
  publication_sequence: u64,
  operation_id: [u8; 16],
  order_key: Vec<u8>,
  encoded_record: Vec<u8>,
}

impl PublishedIndexMutationV1 {
  pub fn index_id(&self) -> &[u8] {
    &self.index_id
  }

  pub const fn role(&self) -> OrderedIndexRoleV1 {
    self.role
  }

  pub const fn operation(&self) -> IndexMutationOperationV1 {
    self.operation
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }

  pub const fn operation_id(&self) -> [u8; 16] {
    self.operation_id
  }

  pub fn order_key(&self) -> &[u8] {
    &self.order_key
  }

  pub fn encoded_record(&self) -> &[u8] {
    &self.encoded_record
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedIndexMembershipTransitionV1 {
  owner_id: Vec<u8>,
  owner_class: IndexMembershipOwnerClassV1,
  publication_sequence: u64,
  operation_id: [u8; 16],
  document_ordinal: u64,
  before: IndexMembershipStateV1,
  after: IndexMembershipStateV1,
}

impl PublishedIndexMembershipTransitionV1 {
  pub fn owner_id(&self) -> &[u8] {
    &self.owner_id
  }

  pub const fn owner_class(&self) -> IndexMembershipOwnerClassV1 {
    self.owner_class
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }

  pub const fn operation_id(&self) -> [u8; 16] {
    self.operation_id
  }

  pub const fn document_ordinal(&self) -> u64 {
    self.document_ordinal
  }

  pub const fn before(&self) -> IndexMembershipStateV1 {
    self.before
  }

  pub const fn after(&self) -> IndexMembershipStateV1 {
    self.after
  }
}

pub struct FrozenIndexBatchV1 {
  coordinator_id: [u8; 16],
  batch_id: u64,
  attempt_id: u64,
  reason: IndexFlushReasonV1,
  records: Vec<PublishedIndexMutationV1>,
  transitions: Vec<PublishedIndexMembershipTransitionV1>,
  publication_bytes: u64,
  _publication_reservation: MemoryReservation,
}

impl FrozenIndexBatchV1 {
  pub const fn coordinator_id(&self) -> [u8; 16] {
    self.coordinator_id
  }

  pub const fn batch_id(&self) -> u64 {
    self.batch_id
  }

  pub const fn attempt_id(&self) -> u64 {
    self.attempt_id
  }

  pub const fn reason(&self) -> IndexFlushReasonV1 {
    self.reason
  }

  pub fn records(&self) -> &[PublishedIndexMutationV1] {
    &self.records
  }

  pub fn transitions(&self) -> &[PublishedIndexMembershipTransitionV1] {
    &self.transitions
  }

  pub const fn publication_bytes(&self) -> u64 {
    self.publication_bytes
  }
}

impl IndexFlushReasonV1 {
  pub const fn id(self) -> u16 {
    match self {
      Self::MutationCount => 1,
      Self::Age => 2,
      Self::MemoryPressure => 3,
      Self::Explicit => 4,
      Self::Shutdown => 5,
    }
  }

  pub const fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::MutationCount),
      2 => Some(Self::Age),
      3 => Some(Self::MemoryPressure),
      4 => Some(Self::Explicit),
      5 => Some(Self::Shutdown),
      _ => None,
    }
  }
}

pub struct IndexCoordinatorV1 {
  coordinator_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
  memory: MemoryCoordinator,
  options: IndexCoordinatorOptionsV1,
  lifecycle: IndexCoordinatorLifecycleV1,
  active: Vec<RetainedMutationV1>,
  active_groups: Vec<RetainedIndexGroupV1>,
  active_mutations: u64,
  active_bytes: u64,
  frozen: Option<FrozenStateV1>,
  next_batch_id: u64,
  next_attempt_id: u64,
  last_observed_ms: u64,
  successful_flushes: u64,
  restored_flushes: u64,
}

impl IndexCoordinatorV1 {
  pub fn new(
    coordinator_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
    memory: MemoryCoordinator,
    options: IndexCoordinatorOptionsV1,
    now_ms: u64,
  ) -> Result<Self, IndexCoordinatorErrorV1> {
    if coordinator_id == [0; 16] {
      return Err(IndexCoordinatorErrorV1::InvalidCoordinatorIdentity);
    }
    Ok(Self {
      coordinator_id,
      hash_algorithm,
      memory,
      options,
      lifecycle: IndexCoordinatorLifecycleV1::Running,
      active: Vec::new(),
      active_groups: Vec::new(),
      active_mutations: 0,
      active_bytes: 0,
      frozen: None,
      next_batch_id: 1,
      next_attempt_id: 1,
      last_observed_ms: now_ms,
      successful_flushes: 0,
      restored_flushes: 0,
    })
  }

  pub fn snapshot(&self) -> IndexCoordinatorSnapshotV1 {
    let active_group_records = self.active_groups.iter().map(|group| group.records.len() as u64).sum::<u64>();
    let (frozen_records, frozen_mutations, frozen_bytes, frozen_groups) = self.frozen.as_ref().map_or((0, 0, 0, 0), |frozen| {
      (
        frozen.records.len() as u64 + frozen.groups.iter().map(|group| group.records.len() as u64).sum::<u64>(),
        frozen.mutations,
        frozen.bytes,
        frozen.groups.len() as u64,
      )
    });
    IndexCoordinatorSnapshotV1 {
      lifecycle: self.lifecycle,
      active_records: self.active.len() as u64 + active_group_records,
      active_mutations: self.active_mutations,
      active_bytes: self.active_bytes,
      active_groups: self.active_groups.len() as u64,
      frozen_records,
      frozen_mutations,
      frozen_bytes,
      frozen_groups,
      successful_flushes: self.successful_flushes,
      restored_flushes: self.restored_flushes,
    }
  }

  pub fn admit(&mut self, request: IndexMutationRequestV1<'_>, now_ms: u64) -> Result<IndexMutationAdmissionV1, IndexCoordinatorErrorV1> {
    self.require_running()?;
    self.check_time(now_ms)?;
    if !self.active_groups.is_empty() || self.frozen.as_ref().is_some_and(|frozen| !frozen.groups.is_empty()) {
      return Err(IndexCoordinatorErrorV1::MixedAdmissionMode);
    }
    self.validate_request(&request)?;

    let decoded = decode_ordered_record(request.encoded_record, self.hash_algorithm, request.role)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    let order_key_length = checked_ordered_record_order_key_length(&decoded)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    let actual_bytes = retained_bytes(request.index_id.len(), order_key_length, request.encoded_record.len())?;
    let reservation = self.reserve_dirty(actual_bytes, "mutation admission")?;
    let order_key = ordered_record_order_key(&decoded)
      .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
    if order_key.len() != order_key_length {
      return Err(IndexCoordinatorErrorV1::Invariant("ordered record key length changed between sizing and materialization".to_string()));
    }

    let projected =
      self.retained_bytes()?.checked_add(actual_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("mutation buffer projection"))?;
    let (active_position, replacing_active) = mutation_position(&self.active, request.index_id, request.role, &order_key);
    let active_existing = if replacing_active { self.active.get(active_position) } else { None };
    let frozen_existing = if active_existing.is_none() { self.frozen_record(request.index_id, request.role, &order_key) } else { None };
    let existing = active_existing.or(frozen_existing);
    let admission = compare_existing(existing, &request)?;
    if admission == IndexMutationAdmissionV1::Duplicate {
      return Ok(admission);
    }

    let replaced_active_bytes = active_existing.map_or(0, |record| record.retained_bytes);
    let projected =
      projected.checked_sub(replaced_active_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("mutation replacement projection"))?;
    if projected > self.options.mutation_buffer_max_bytes {
      return Err(IndexCoordinatorErrorV1::SpillRequired {
        context: "mutation buffer limit".to_string(),
        requested_bytes: projected,
        limit_bytes: self.options.mutation_buffer_max_bytes,
      });
    }
    let one_publication = publication_bytes(request.index_id.len(), order_key.len(), request.encoded_record.len())?;
    if one_publication > self.options.publication_batch_max_bytes {
      return Err(IndexCoordinatorErrorV1::SpillRequired {
        context: "one mutation exceeds the publication batch limit".to_string(),
        requested_bytes: one_publication,
        limit_bytes: self.options.publication_batch_max_bytes,
      });
    }

    let previous_observations = match active_existing {
      Some(record) => record.observed_mutations,
      None => 0,
    };
    let observed_mutations =
      previous_observations.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("observed mutation count"))?;
    let first_seen_ms = active_existing.map_or(now_ms, |record| record.first_seen_ms);
    let (next_active_mutations, next_active_bytes) = if let Some(previous) = active_existing {
      (
        self
          .active_mutations
          .checked_sub(previous.observed_mutations)
          .and_then(|value| value.checked_add(observed_mutations))
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("active mutation replacement"))?,
        self
          .active_bytes
          .checked_sub(previous.retained_bytes)
          .and_then(|value| value.checked_add(actual_bytes))
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("active byte replacement"))?,
      )
    } else {
      (
        self
          .active_mutations
          .checked_add(observed_mutations)
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("active mutation insertion"))?,
        self.active_bytes.checked_add(actual_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("active byte insertion"))?,
      )
    };
    if !replacing_active {
      let restore_headroom = self.frozen.as_ref().map_or(0, |frozen| frozen.records.len());
      self.active.try_reserve(restore_headroom.saturating_add(1)).map_err(|error| allocation_error("active mutation slot", error))?;
    }

    let key = MutationKeyV1 { index_id: copy_bytes(request.index_id)?, role_id: request.role.id(), order_key };
    let encoded_record = copy_bytes(request.encoded_record)?;
    let retained = RetainedMutationV1 {
      key,
      role: request.role,
      operation: IndexMutationOperationV1::Upsert,
      publication_sequence: request.publication_sequence,
      operation_id: request.operation_id,
      encoded_record,
      observed_mutations,
      first_seen_ms,
      retained_bytes: actual_bytes,
      _reservation: reservation,
    };

    self.last_observed_ms = now_ms;
    if replacing_active {
      self.active[active_position] = retained;
    } else {
      self.active.insert(active_position, retained);
    }
    self.active_mutations = next_active_mutations;
    self.active_bytes = next_active_bytes;
    Ok(admission)
  }

  pub fn admit_group(
    &mut self,
    request: IndexMutationGroupRequestV1<'_>,
    now_ms: u64,
  ) -> Result<IndexMutationGroupAdmissionV1, IndexCoordinatorErrorV1> {
    self.require_running()?;
    self.check_time(now_ms)?;
    if !self.active.is_empty() || self.frozen.as_ref().is_some_and(|frozen| !frozen.records.is_empty()) {
      return Err(IndexCoordinatorErrorV1::MixedAdmissionMode);
    }
    self.validate_group_request(&request)?;

    let (active_position, active_exists) =
      membership_position(&self.active_groups, request.transition.owner_id, request.transition.document_ordinal);
    let active = active_exists.then(|| &self.active_groups[active_position]);
    let frozen = if active.is_none() { self.frozen_group(request.transition.owner_id, request.transition.document_ordinal) } else { None };
    if let Some(retained) = active.or(frozen) {
      if request.transition.publication_sequence < retained.publication_sequence {
        return Err(IndexCoordinatorErrorV1::StaleMutation {
          received_sequence: request.transition.publication_sequence,
          retained_sequence: retained.publication_sequence,
        });
      }
      if request.transition.publication_sequence == retained.publication_sequence {
        if group_request_is_duplicate(&self.memory, self.options.mutation_buffer_max_bytes, retained, &request, self.hash_algorithm)? {
          return Ok(IndexMutationGroupAdmissionV1::Duplicate);
        }
        return Err(IndexCoordinatorErrorV1::ConflictingGroupTransition { publication_sequence: request.transition.publication_sequence });
      }
      if request.transition.before != retained.after || request.transition.owner_class != retained.owner_class {
        return Err(IndexCoordinatorErrorV1::ConflictingGroupTransition { publication_sequence: request.transition.publication_sequence });
      }
    }

    let replacement = build_retained_group(
      &self.memory,
      self.options.mutation_buffer_max_bytes,
      self.options.publication_batch_max_bytes,
      self.hash_algorithm,
      active,
      &request,
      now_ms,
    )?;
    if let Some(frozen) = frozen {
      let recovery_bytes = replacement
        .publication_bytes
        .checked_add(frozen.publication_bytes)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic recovery publication bytes"))?;
      if recovery_bytes > self.options.publication_batch_max_bytes {
        return Err(IndexCoordinatorErrorV1::SpillRequired {
          context: "semantic successor would make failed-flush recovery exceed the publication batch limit".to_string(),
          requested_bytes: recovery_bytes,
          limit_bytes: self.options.publication_batch_max_bytes,
        });
      }
    }
    let replaced_bytes = active.map_or(0, |group| group.retained_bytes);
    let projected = self
      .retained_bytes()?
      .checked_sub(replaced_bytes)
      .and_then(|bytes| bytes.checked_add(replacement.retained_bytes))
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("group admission projection"))?;
    if projected > self.options.mutation_buffer_max_bytes {
      return Err(IndexCoordinatorErrorV1::SpillRequired {
        context: "semantic group buffer limit".to_string(),
        requested_bytes: projected,
        limit_bytes: self.options.mutation_buffer_max_bytes,
      });
    }
    let replaced_mutations = active.map_or(0, |group| group.observed_mutations);
    let next_active_mutations = self
      .active_mutations
      .checked_sub(replaced_mutations)
      .and_then(|count| count.checked_add(replacement.observed_mutations))
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("group mutation admission"))?;
    let next_active_bytes = self
      .active_bytes
      .checked_sub(replaced_bytes)
      .and_then(|bytes| bytes.checked_add(replacement.retained_bytes))
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("group byte admission"))?;
    if !active_exists {
      self.active_groups.try_reserve(1).map_err(|error| allocation_error("active semantic-group slot", error))?;
    }
    if active_exists {
      self.active_groups[active_position] = replacement;
    } else {
      self.active_groups.insert(active_position, replacement);
    }
    self.active_mutations = next_active_mutations;
    self.active_bytes = next_active_bytes;
    self.last_observed_ms = now_ms;
    Ok(if active_exists { IndexMutationGroupAdmissionV1::Replaced } else { IndexMutationGroupAdmissionV1::Inserted })
  }

  pub fn flush_reason(&mut self, now_ms: u64) -> Result<Option<IndexFlushReasonV1>, IndexCoordinatorErrorV1> {
    self.check_time(now_ms)?;
    if self.active.is_empty() && self.active_groups.is_empty() {
      self.last_observed_ms = now_ms;
      return Ok(None);
    }
    let pressure = self.memory.snapshot().map_err(memory_authority_error)?.pressure;
    let reason = self.compute_flush_reason(now_ms, pressure);
    self.last_observed_ms = now_ms;
    Ok(reason)
  }

  pub fn begin_flush(
    &mut self,
    now_ms: u64,
    requested_reason: Option<IndexFlushReasonV1>,
    cancelled: bool,
  ) -> Result<Option<FrozenIndexBatchV1>, IndexCoordinatorErrorV1> {
    if cancelled {
      return Err(IndexCoordinatorErrorV1::Cancelled);
    }
    self.check_time(now_ms)?;
    if self.lifecycle == IndexCoordinatorLifecycleV1::Stopped {
      return Err(IndexCoordinatorErrorV1::NotRunning { lifecycle: self.lifecycle });
    }
    if let Some(frozen) = &self.frozen {
      return Err(IndexCoordinatorErrorV1::FlushInProgress { batch_id: frozen.batch_id });
    }
    if self.active.is_empty() && self.active_groups.is_empty() {
      self.last_observed_ms = now_ms;
      return Ok(None);
    }
    let reason = match requested_reason {
      Some(reason) => reason,
      None if self.lifecycle == IndexCoordinatorLifecycleV1::Draining => IndexFlushReasonV1::Shutdown,
      None => match self.compute_flush_reason(now_ms, self.memory.snapshot().map_err(memory_authority_error)?.pressure) {
        Some(reason) => reason,
        None => {
          self.last_observed_ms = now_ms;
          return Ok(None);
        }
      },
    };
    if !self.active_groups.is_empty() {
      return self.begin_group_flush(now_ms, reason);
    }

    let mut publication_bytes = 0u64;
    let mut count = 0usize;
    let mut frozen_mutations = 0u64;
    let mut frozen_bytes = 0u64;
    for record in &self.active {
      let next = record.publication_bytes()?;
      let projected = publication_bytes.checked_add(next).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("publication batch bytes"))?;
      if projected > self.options.publication_batch_max_bytes {
        break;
      }
      publication_bytes = projected;
      frozen_mutations = frozen_mutations
        .checked_add(record.observed_mutations)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("frozen mutation count"))?;
      frozen_bytes =
        frozen_bytes.checked_add(record.retained_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("frozen byte count"))?;
      count += 1;
    }
    if count == 0 {
      return Err(IndexCoordinatorErrorV1::Invariant("admitted mutation does not fit an empty publication batch".to_string()));
    }

    let publication_reservation = self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, publication_bytes, AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite))
      .map_err(|error| map_reservation_error(error, publication_bytes, self.options.publication_batch_max_bytes, "publication snapshot"))?;
    let mut published = Vec::new();
    published.try_reserve_exact(count).map_err(|error| allocation_error("publication record slots", error))?;
    for record in self.active.iter().take(count) {
      published.push(PublishedIndexMutationV1 {
        index_id: copy_bytes(&record.key.index_id)?,
        role: record.role,
        operation: record.operation,
        publication_sequence: record.publication_sequence,
        operation_id: record.operation_id,
        order_key: copy_bytes(&record.key.order_key)?,
        encoded_record: copy_bytes(&record.encoded_record)?,
      });
    }

    let remaining_count = self.active.len() - count;
    let mut remaining = Vec::new();
    remaining.try_reserve_exact(remaining_count).map_err(|error| allocation_error("remaining active record slots", error))?;
    let next_active_mutations =
      self.active_mutations.checked_sub(frozen_mutations).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("active mutation freeze"))?;
    let next_active_bytes =
      self.active_bytes.checked_sub(frozen_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("active byte freeze"))?;

    let batch_id = self.next_batch_id;
    let next_batch_id = self.next_batch_id.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("flush batch identity"))?;
    let attempt_id = self.next_attempt_id;
    let next_attempt_id =
      self.next_attempt_id.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("flush attempt identity"))?;
    remaining.extend(self.active.drain(count..));
    let frozen_records = std::mem::replace(&mut self.active, remaining);
    self.active_mutations = next_active_mutations;
    self.active_bytes = next_active_bytes;
    self.next_batch_id = next_batch_id;
    self.next_attempt_id = next_attempt_id;
    self.last_observed_ms = now_ms;
    self.frozen = Some(FrozenStateV1 {
      batch_id,
      attempt_id,
      reason,
      records: frozen_records,
      groups: Vec::new(),
      mutations: frozen_mutations,
      bytes: frozen_bytes,
    });
    Ok(Some(FrozenIndexBatchV1 {
      coordinator_id: self.coordinator_id,
      batch_id,
      attempt_id,
      reason,
      records: published,
      transitions: Vec::new(),
      publication_bytes,
      _publication_reservation: publication_reservation,
    }))
  }

  fn begin_group_flush(&mut self, now_ms: u64, reason: IndexFlushReasonV1) -> Result<Option<FrozenIndexBatchV1>, IndexCoordinatorErrorV1> {
    let mut publication_bytes = 0u64;
    let mut group_count = 0usize;
    let mut record_count = 0usize;
    let mut frozen_mutations = 0u64;
    let mut frozen_bytes = 0u64;
    for group in &self.active_groups {
      let projected = publication_bytes
        .checked_add(group.publication_bytes)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic publication batch bytes"))?;
      if projected > self.options.publication_batch_max_bytes {
        break;
      }
      publication_bytes = projected;
      record_count = record_count
        .checked_add(group.records.len())
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic publication record count"))?;
      frozen_mutations = frozen_mutations
        .checked_add(group.observed_mutations)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic frozen mutation count"))?;
      frozen_bytes =
        frozen_bytes.checked_add(group.retained_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic frozen bytes"))?;
      group_count += 1;
    }
    if group_count == 0 || publication_bytes == 0 {
      return Err(IndexCoordinatorErrorV1::Invariant("admitted semantic group does not fit an empty publication batch".to_string()));
    }
    let publication_reservation = self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, publication_bytes, AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite))
      .map_err(|error| {
        map_reservation_error(error, publication_bytes, self.options.publication_batch_max_bytes, "semantic publication snapshot")
      })?;
    let mut published = Vec::new();
    published.try_reserve_exact(record_count).map_err(|error| allocation_error("semantic publication record slots", error))?;
    let mut transitions = Vec::new();
    transitions.try_reserve_exact(group_count).map_err(|error| allocation_error("semantic publication transition slots", error))?;
    for group in self.active_groups.iter().take(group_count) {
      transitions.push(published_transition(group)?);
      for record in &group.records {
        published.push(published_group_mutation(record)?);
      }
    }
    published.sort_unstable_by(|left, right| {
      left.index_id.cmp(&right.index_id).then(left.role.id().cmp(&right.role.id())).then(left.order_key.cmp(&right.order_key))
    });

    let next_active_mutations = self
      .active_mutations
      .checked_sub(frozen_mutations)
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic active mutation freeze"))?;
    let next_active_bytes =
      self.active_bytes.checked_sub(frozen_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("semantic active byte freeze"))?;
    let batch_id = self.next_batch_id;
    let next_batch_id = self.next_batch_id.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("flush batch identity"))?;
    let attempt_id = self.next_attempt_id;
    let next_attempt_id =
      self.next_attempt_id.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("flush attempt identity"))?;
    let remaining = self.active_groups.split_off(group_count);
    let frozen_groups = std::mem::replace(&mut self.active_groups, remaining);
    self.active_mutations = next_active_mutations;
    self.active_bytes = next_active_bytes;
    self.next_batch_id = next_batch_id;
    self.next_attempt_id = next_attempt_id;
    self.last_observed_ms = now_ms;
    self.frozen = Some(FrozenStateV1 {
      batch_id,
      attempt_id,
      reason,
      records: Vec::new(),
      groups: frozen_groups,
      mutations: frozen_mutations,
      bytes: frozen_bytes,
    });
    Ok(Some(FrozenIndexBatchV1 {
      coordinator_id: self.coordinator_id,
      batch_id,
      attempt_id,
      reason,
      records: published,
      transitions,
      publication_bytes,
      _publication_reservation: publication_reservation,
    }))
  }

  /// Reissue the current frozen batch after a worker failure or abandoned
  /// publication handle. The new attempt invalidates every older handle while
  /// retaining the exact frozen records and batch identity.
  pub fn retry_frozen(&mut self, cancelled: bool) -> Result<FrozenIndexBatchV1, IndexCoordinatorErrorV1> {
    if cancelled {
      return Err(IndexCoordinatorErrorV1::Cancelled);
    }
    let attempt_id = self.next_attempt_id;
    let next_attempt_id =
      self.next_attempt_id.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("flush attempt identity"))?;
    let batch = {
      let frozen = match self.frozen.as_ref() {
        Some(frozen) => frozen,
        None => return Err(IndexCoordinatorErrorV1::NoFlushInProgress),
      };
      self.clone_frozen_batch(frozen, attempt_id)?
    };
    let frozen = match self.frozen.as_mut() {
      Some(frozen) => frozen,
      None => return Err(IndexCoordinatorErrorV1::Invariant("frozen batch disappeared during retry".to_string())),
    };
    frozen.attempt_id = attempt_id;
    self.next_attempt_id = next_attempt_id;
    Ok(batch)
  }

  pub fn complete_success(&mut self, batch: &FrozenIndexBatchV1) -> Result<(), IndexCoordinatorErrorV1> {
    self.validate_batch(batch)?;
    let successful_flushes =
      self.successful_flushes.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("successful flush count"))?;
    self.frozen.take().ok_or_else(|| IndexCoordinatorErrorV1::Invariant("validated flush batch has no frozen state".to_string()))?;
    self.successful_flushes = successful_flushes;
    Ok(())
  }

  pub fn complete_failure(&mut self, batch: &FrozenIndexBatchV1, now_ms: u64) -> Result<(), IndexCoordinatorErrorV1> {
    self.check_time(now_ms)?;
    self.validate_batch(batch)?;
    if self.frozen.as_ref().is_some_and(|frozen| !frozen.groups.is_empty()) {
      return self.complete_group_failure(now_ms);
    }
    let (distinct_records, next_active_mutations, next_active_bytes) = {
      let frozen = self.frozen.as_ref().ok_or(IndexCoordinatorErrorV1::StaleBatch)?;
      let mut distinct_records = 0usize;
      let next_active_mutations = self
        .active_mutations
        .checked_add(frozen.mutations)
        .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("failed-flush mutation restore"))?;
      let mut next_active_bytes = self.active_bytes;
      for restored in &frozen.records {
        let (position, exists) = mutation_key_position(&self.active, &restored.key);
        if exists {
          let active = &self.active[position];
          if active.publication_sequence < restored.publication_sequence {
            return Err(IndexCoordinatorErrorV1::Invariant("frozen mutation is newer than active replacement".to_string()));
          }
          active
            .observed_mutations
            .checked_add(restored.observed_mutations)
            .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored mutation observations"))?;
        } else {
          distinct_records = distinct_records.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored record count"))?;
          next_active_bytes = next_active_bytes
            .checked_add(restored.retained_bytes)
            .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("failed-flush byte restore"))?;
        }
      }
      (distinct_records, next_active_mutations, next_active_bytes)
    };
    let restored_flushes =
      self.restored_flushes.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored flush count"))?;
    self.active.try_reserve_exact(distinct_records).map_err(|error| allocation_error("failed-flush restore slots", error))?;
    let frozen =
      self.frozen.take().ok_or_else(|| IndexCoordinatorErrorV1::Invariant("validated flush batch has no frozen state".to_string()))?;
    for restored in frozen.records {
      let (position, exists) = mutation_key_position(&self.active, &restored.key);
      if exists {
        let active = &mut self.active[position];
        active.observed_mutations += restored.observed_mutations;
        active.first_seen_ms = active.first_seen_ms.min(restored.first_seen_ms);
      } else {
        self.active.insert(position, restored);
      }
    }
    self.active_mutations = next_active_mutations;
    self.active_bytes = next_active_bytes;
    self.restored_flushes = restored_flushes;
    self.last_observed_ms = now_ms;
    Ok(())
  }

  fn complete_group_failure(&mut self, now_ms: u64) -> Result<(), IndexCoordinatorErrorV1> {
    let frozen = self.frozen.as_ref().ok_or(IndexCoordinatorErrorV1::StaleBatch)?;
    let next_active_mutations = self
      .active_mutations
      .checked_add(frozen.mutations)
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("failed semantic flush mutation restore"))?;
    let mut next_active_bytes = self.active_bytes;
    let mut distinct_groups = 0usize;
    for restored in &frozen.groups {
      let (position, exists) = membership_position(&self.active_groups, &restored.key.owner_id, restored.key.document_ordinal);
      if exists {
        let active = &mut self.active_groups[position];
        if active.publication_sequence <= restored.publication_sequence
          || active.before != restored.after
          || active.owner_class != restored.owner_class
        {
          return Err(IndexCoordinatorErrorV1::Invariant("active semantic successor does not continue its frozen predecessor".to_string()));
        }
        let merged_bytes = merged_group_retained_bytes(active, restored)?;
        next_active_bytes = next_active_bytes
          .checked_sub(active.retained_bytes)
          .and_then(|bytes| bytes.checked_add(merged_bytes))
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("failed semantic composition byte restore"))?;
        let missing = restored
          .records
          .iter()
          .filter(|record| {
            let position = active.records.partition_point(|candidate| candidate.key < record.key);
            active.records.get(position).is_none_or(|candidate| candidate.key != record.key)
          })
          .count();
        active.records.try_reserve_exact(missing).map_err(|error| allocation_error("failed semantic-flush record restore", error))?;
        active
          ._reservations
          .try_reserve_exact(restored._reservations.len())
          .map_err(|error| allocation_error("failed semantic-flush reservation restore", error))?;
        active
          .observed_mutations
          .checked_add(restored.observed_mutations)
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic observations"))?;
        active
          .retained_bytes
          .checked_add(restored.retained_bytes)
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic retained bytes"))?;
      } else {
        distinct_groups =
          distinct_groups.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored semantic group count"))?;
        next_active_bytes = next_active_bytes
          .checked_add(restored.retained_bytes)
          .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("failed semantic group byte restore"))?;
      }
    }
    self
      .active_groups
      .try_reserve_exact(distinct_groups)
      .map_err(|error| allocation_error("failed semantic-flush group restore", error))?;
    let restored_flushes =
      self.restored_flushes.checked_add(1).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("restored flush count"))?;
    let frozen =
      self.frozen.take().ok_or_else(|| IndexCoordinatorErrorV1::Invariant("validated semantic flush has no frozen state".to_string()))?;
    for restored in frozen.groups {
      let (position, exists) = membership_position(&self.active_groups, &restored.key.owner_id, restored.key.document_ordinal);
      if exists {
        merge_restored_group(&mut self.active_groups[position], restored)?;
      } else {
        self.active_groups.insert(position, restored);
      }
    }
    self.active_mutations = next_active_mutations;
    self.active_bytes = next_active_bytes;
    self.restored_flushes = restored_flushes;
    self.last_observed_ms = now_ms;
    Ok(())
  }

  pub fn begin_draining(&mut self) -> Result<(), IndexCoordinatorErrorV1> {
    if self.lifecycle != IndexCoordinatorLifecycleV1::Running {
      return Err(IndexCoordinatorErrorV1::NotRunning { lifecycle: self.lifecycle });
    }
    self.lifecycle = IndexCoordinatorLifecycleV1::Draining;
    Ok(())
  }

  pub fn finish_draining(&mut self) -> Result<(), IndexCoordinatorErrorV1> {
    if self.lifecycle != IndexCoordinatorLifecycleV1::Draining {
      return Err(IndexCoordinatorErrorV1::NotRunning { lifecycle: self.lifecycle });
    }
    if !self.active.is_empty() || !self.active_groups.is_empty() || self.frozen.is_some() {
      return Err(IndexCoordinatorErrorV1::DrainIncomplete);
    }
    self.lifecycle = IndexCoordinatorLifecycleV1::Stopped;
    Ok(())
  }

  fn require_running(&self) -> Result<(), IndexCoordinatorErrorV1> {
    if self.lifecycle == IndexCoordinatorLifecycleV1::Running {
      Ok(())
    } else {
      Err(IndexCoordinatorErrorV1::NotRunning { lifecycle: self.lifecycle })
    }
  }

  fn check_time(&self, now_ms: u64) -> Result<(), IndexCoordinatorErrorV1> {
    if now_ms < self.last_observed_ms {
      return Err(IndexCoordinatorErrorV1::ClockRegressed { previous_ms: self.last_observed_ms, received_ms: now_ms });
    }
    Ok(())
  }

  fn compute_flush_reason(&self, now_ms: u64, pressure: MemoryPressure) -> Option<IndexFlushReasonV1> {
    if matches!(pressure, MemoryPressure::Soft | MemoryPressure::Hard) {
      return Some(IndexFlushReasonV1::MemoryPressure);
    }
    if self.active_mutations >= self.options.flush_after_mutations {
      return Some(IndexFlushReasonV1::MutationCount);
    }
    let oldest =
      match self.active.iter().map(|record| record.first_seen_ms).chain(self.active_groups.iter().map(|group| group.first_seen_ms)).min() {
        Some(oldest) => oldest,
        None => now_ms,
      };
    (now_ms.saturating_sub(oldest) >= self.options.flush_after_ms).then_some(IndexFlushReasonV1::Age)
  }

  fn validate_request(&self, request: &IndexMutationRequestV1<'_>) -> Result<(), IndexCoordinatorErrorV1> {
    if request.index_id.len() != self.hash_algorithm.hash_length() || request.index_id.iter().all(|byte| *byte == 0) {
      return Err(IndexCoordinatorErrorV1::InvalidMutation("index identity has the wrong width or is all zeroes".to_string()));
    }
    if request.publication_sequence == 0 {
      return Err(IndexCoordinatorErrorV1::InvalidMutation("publication sequence is zero".to_string()));
    }
    if request.operation_id == [0; 16] {
      return Err(IndexCoordinatorErrorV1::InvalidMutation("operation identity is all zeroes".to_string()));
    }
    if request.role == OrderedIndexRoleV1::NvtTile {
      return Err(IndexCoordinatorErrorV1::InvalidMutation(
        "NVT tiles are derived publication artifacts, not ordered mutations".to_string(),
      ));
    }
    Ok(())
  }

  fn validate_group_request(&self, request: &IndexMutationGroupRequestV1<'_>) -> Result<(), IndexCoordinatorErrorV1> {
    let transition = request.transition;
    if transition.owner_id.len() != self.hash_algorithm.hash_length() || transition.owner_id.iter().all(|byte| *byte == 0) {
      return Err(IndexCoordinatorErrorV1::InvalidMutation("membership owner identity has the wrong width or is all zeroes".to_string()));
    }
    if transition.publication_sequence == 0 || transition.operation_id == [0; 16] || transition.document_ordinal == 0 {
      return Err(IndexCoordinatorErrorV1::InvalidMutation("membership transition identity is incomplete".to_string()));
    }
    validate_membership_states(transition.owner_class, transition.before, transition.after)?;
    for grouped in request.mutations {
      let mutation = grouped.mutation;
      self.validate_request(&mutation)?;
      if mutation.index_id != transition.owner_id
        || mutation.publication_sequence != transition.publication_sequence
        || mutation.operation_id != transition.operation_id
        || mutation.role.owner_class() != transition.owner_class.id()
      {
        return Err(IndexCoordinatorErrorV1::InvalidMutation("group mutation disagrees with its owner/document transition".to_string()));
      }
      let decoded = decode_ordered_record(mutation.encoded_record, self.hash_algorithm, mutation.role)
        .map_err(|error| IndexCoordinatorErrorV1::MalformedRecord { class: error.class(), message: error.to_string() })?;
      if decoded.document_ordinal != transition.document_ordinal {
        return Err(IndexCoordinatorErrorV1::InvalidMutation("group mutation document ordinal disagrees with its transition".to_string()));
      }
      if grouped.operation == IndexMutationOperationV1::RemoveExisting
        && (mutation.role != OrderedIndexRoleV1::ScopeReverse || decoded.tombstone)
      {
        return Err(IndexCoordinatorErrorV1::InvalidMutation("remove-existing is legal only for a live scope-reverse record".to_string()));
      }
    }
    Ok(())
  }

  fn frozen_record(&self, index_id: &[u8], role: OrderedIndexRoleV1, order_key: &[u8]) -> Option<&RetainedMutationV1> {
    let frozen = self.frozen.as_ref()?;
    let (position, exists) = mutation_position(&frozen.records, index_id, role, order_key);
    if exists {
      frozen.records.get(position)
    } else {
      None
    }
  }

  fn frozen_group(&self, owner_id: &[u8], document_ordinal: u64) -> Option<&RetainedIndexGroupV1> {
    let frozen = self.frozen.as_ref()?;
    let (position, exists) = membership_position(&frozen.groups, owner_id, document_ordinal);
    exists.then(|| &frozen.groups[position])
  }

  fn retained_bytes(&self) -> Result<u64, IndexCoordinatorErrorV1> {
    let frozen_bytes = match self.frozen.as_ref() {
      Some(frozen) => frozen.bytes,
      None => 0,
    };
    self.active_bytes.checked_add(frozen_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("total retained bytes"))
  }

  fn reserve_dirty(&self, bytes: u64, context: &str) -> Result<MemoryReservation, IndexCoordinatorErrorV1> {
    self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, bytes, AdmissionClass::Workload)
      .map_err(|error| map_reservation_error(error, bytes, self.options.mutation_buffer_max_bytes, context))
  }

  fn validate_batch(&self, batch: &FrozenIndexBatchV1) -> Result<(), IndexCoordinatorErrorV1> {
    if batch.coordinator_id != self.coordinator_id {
      return Err(IndexCoordinatorErrorV1::ForeignBatch);
    }
    let Some(frozen) = &self.frozen else {
      return Err(IndexCoordinatorErrorV1::StaleBatch);
    };
    if batch.batch_id != frozen.batch_id
      || batch.attempt_id != frozen.attempt_id
      || batch.reason != frozen.reason
      || batch.records.len() != frozen.records.len() + frozen.groups.iter().map(|group| group.records.len()).sum::<usize>()
      || batch.transitions.len() != frozen.groups.len()
      || batch.publication_bytes == 0
    {
      return Err(IndexCoordinatorErrorV1::StaleBatch);
    }
    Ok(())
  }

  fn clone_frozen_batch(&self, frozen: &FrozenStateV1, attempt_id: u64) -> Result<FrozenIndexBatchV1, IndexCoordinatorErrorV1> {
    let publication_bytes = frozen
      .records
      .iter()
      .try_fold(0u64, |total, record| {
        total.checked_add(record.publication_bytes()?).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("retried publication batch bytes"))
      })?
      .checked_add(frozen.groups.iter().try_fold(0u64, |total, group| {
        total.checked_add(group.publication_bytes).ok_or(IndexCoordinatorErrorV1::AccountingOverflow("retried semantic publication bytes"))
      })?)
      .ok_or(IndexCoordinatorErrorV1::AccountingOverflow("retried combined publication bytes"))?;
    if publication_bytes == 0 || publication_bytes > self.options.publication_batch_max_bytes {
      return Err(IndexCoordinatorErrorV1::Invariant("frozen batch violates its publication byte bound".to_string()));
    }
    let publication_reservation = self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, publication_bytes, AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite))
      .map_err(|error| {
        map_reservation_error(error, publication_bytes, self.options.publication_batch_max_bytes, "retried publication snapshot")
      })?;
    let mut published = Vec::new();
    let record_count = frozen.records.len() + frozen.groups.iter().map(|group| group.records.len()).sum::<usize>();
    published.try_reserve_exact(record_count).map_err(|error| allocation_error("retried publication record slots", error))?;
    for record in &frozen.records {
      published.push(PublishedIndexMutationV1 {
        index_id: copy_bytes(&record.key.index_id)?,
        role: record.role,
        operation: record.operation,
        publication_sequence: record.publication_sequence,
        operation_id: record.operation_id,
        order_key: copy_bytes(&record.key.order_key)?,
        encoded_record: copy_bytes(&record.encoded_record)?,
      });
    }
    let mut transitions = Vec::new();
    transitions.try_reserve_exact(frozen.groups.len()).map_err(|error| allocation_error("retried semantic transition slots", error))?;
    for group in &frozen.groups {
      transitions.push(published_transition(group)?);
      for record in &group.records {
        published.push(published_group_mutation(record)?);
      }
    }
    published.sort_unstable_by(|left, right| {
      left.index_id.cmp(&right.index_id).then(left.role.id().cmp(&right.role.id())).then(left.order_key.cmp(&right.order_key))
    });
    Ok(FrozenIndexBatchV1 {
      coordinator_id: self.coordinator_id,
      batch_id: frozen.batch_id,
      attempt_id,
      reason: frozen.reason,
      records: published,
      transitions,
      publication_bytes,
      _publication_reservation: publication_reservation,
    })
  }
}

fn compare_key(retained: &MutationKeyV1, index_id: &[u8], role: OrderedIndexRoleV1, order_key: &[u8]) -> std::cmp::Ordering {
  retained
    .index_id
    .as_slice()
    .cmp(index_id)
    .then_with(|| retained.role_id.cmp(&role.id()))
    .then_with(|| retained.order_key.as_slice().cmp(order_key))
}

fn mutation_position(records: &[RetainedMutationV1], index_id: &[u8], role: OrderedIndexRoleV1, order_key: &[u8]) -> (usize, bool) {
  let position = records.partition_point(|record| compare_key(&record.key, index_id, role, order_key).is_lt());
  let exists = match records.get(position) {
    Some(record) => compare_key(&record.key, index_id, role, order_key).is_eq(),
    None => false,
  };
  (position, exists)
}

fn mutation_key_position(records: &[RetainedMutationV1], key: &MutationKeyV1) -> (usize, bool) {
  let position = records.partition_point(|record| record.key < *key);
  let exists = match records.get(position) {
    Some(record) => record.key == *key,
    None => false,
  };
  (position, exists)
}

fn compare_existing(
  existing: Option<&RetainedMutationV1>,
  request: &IndexMutationRequestV1<'_>,
) -> Result<IndexMutationAdmissionV1, IndexCoordinatorErrorV1> {
  let Some(existing) = existing else {
    return Ok(IndexMutationAdmissionV1::Inserted);
  };
  if request.publication_sequence < existing.publication_sequence {
    return Err(IndexCoordinatorErrorV1::StaleMutation {
      received_sequence: request.publication_sequence,
      retained_sequence: existing.publication_sequence,
    });
  }
  if request.publication_sequence == existing.publication_sequence {
    if request.operation_id == existing.operation_id && request.encoded_record == existing.encoded_record {
      return Ok(IndexMutationAdmissionV1::Duplicate);
    }
    return Err(IndexCoordinatorErrorV1::ConflictingMutation { publication_sequence: request.publication_sequence });
  }
  Ok(IndexMutationAdmissionV1::Replaced)
}

fn retained_bytes(index_id_length: usize, order_key_length: usize, record_length: usize) -> Result<u64, IndexCoordinatorErrorV1> {
  checked_footprint(
    size_of::<MutationKeyV1>()
      .checked_add(size_of::<RetainedMutationV1>())
      .and_then(|value| value.checked_add(index_id_length))
      .and_then(|value| value.checked_add(order_key_length))
      .and_then(|value| value.checked_add(record_length)),
    "retained mutation bytes",
  )
}

fn publication_bytes(index_id_length: usize, order_key_length: usize, record_length: usize) -> Result<u64, IndexCoordinatorErrorV1> {
  checked_footprint(
    size_of::<PublishedIndexMutationV1>()
      .checked_add(index_id_length)
      .and_then(|value| value.checked_add(order_key_length))
      .and_then(|value| value.checked_add(record_length)),
    "publication mutation bytes",
  )
}

fn checked_footprint(value: Option<usize>, context: &'static str) -> Result<u64, IndexCoordinatorErrorV1> {
  let value = value.ok_or(IndexCoordinatorErrorV1::AccountingOverflow(context))?;
  match u64::try_from(value) {
    Ok(value) => Ok(value),
    Err(source) => Err(IndexCoordinatorErrorV1::Invariant(format!("{context} does not fit u64: {source}"))),
  }
}

fn copy_bytes(value: &[u8]) -> Result<Vec<u8>, IndexCoordinatorErrorV1> {
  let mut copy = Vec::new();
  copy.try_reserve_exact(value.len()).map_err(|error| allocation_error("mutation bytes", error))?;
  copy.extend_from_slice(value);
  Ok(copy)
}

fn allocation_error(context: &str, error: TryReserveError) -> IndexCoordinatorErrorV1 {
  IndexCoordinatorErrorV1::Allocation(format!("{context}: {error}"))
}

fn memory_authority_error(error: MemoryCoordinatorError) -> IndexCoordinatorErrorV1 {
  IndexCoordinatorErrorV1::MemoryAuthority(error.to_string())
}

fn map_reservation_error(error: MemoryCoordinatorError, requested: u64, limit: u64, context: &str) -> IndexCoordinatorErrorV1 {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => {
      IndexCoordinatorErrorV1::SpillRequired { context: format!("{context}: {error}"), requested_bytes: requested, limit_bytes: limit }
    }
    other => memory_authority_error(other),
  }
}
