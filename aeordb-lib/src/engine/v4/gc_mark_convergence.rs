//! Bounded recoverable-soft mutation ownership for v4 mark convergence.
//!
//! A committed writer only offers evidence to this pre-reserved owner. Durable
//! segment publication remains background GC work and cannot turn journal I/O
//! into a user-write failure. Any missing evidence makes the run incomplete.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use thiserror::Error as ThisError;
use tokio_util::sync::CancellationToken;

use super::gc_mark::{
  GcMarkArtifactV1, MarkMutationRecordWriteV1, decode_gc_mark_artifact, encode_mark_mutation_journal_segment_records_v1,
  encode_mark_mutation_record,
};
use super::reader::FormatError;
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

pub const MARK_MUTATION_JOURNAL_TARGET_SEGMENT_BYTES_V1: usize = 1024 * 1024;
pub const MARK_MUTATION_JOURNAL_MAX_SEGMENT_BYTES_V1: usize = 16 * 1024 * 1024;
pub const MARK_MUTATION_JOURNAL_DEFAULT_MAX_BUFFER_BYTES_V1: usize = 16 * 1024 * 1024;
pub const MARK_MUTATION_JOURNAL_DEFAULT_FLUSH_RECORDS_V1: u32 = 4_096;
pub const MARK_MUTATION_JOURNAL_DEFAULT_FLUSH_AFTER_MS_V1: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkMutationJournalBufferOptionsV1 {
  pub flush_after_records: u32,
  pub target_segment_bytes: usize,
  pub maximum_buffer_bytes: usize,
  pub flush_after_ms: u64,
}

impl MarkMutationJournalBufferOptionsV1 {
  pub fn new(
    flush_after_records: u32,
    target_segment_bytes: usize,
    maximum_buffer_bytes: usize,
    flush_after_ms: u64,
  ) -> Result<Self, MarkMutationJournalOwnerErrorV1> {
    if flush_after_records == 0 || flush_after_ms == 0 {
      return Err(MarkMutationJournalOwnerErrorV1::InvalidOptions("flush record count and interval must be nonzero"));
    }
    if target_segment_bytes == 0 || target_segment_bytes > MARK_MUTATION_JOURNAL_MAX_SEGMENT_BYTES_V1 {
      return Err(MarkMutationJournalOwnerErrorV1::InvalidOptions("target segment bytes exceed the frozen 16 MiB cap"));
    }
    if maximum_buffer_bytes < target_segment_bytes || maximum_buffer_bytes > MARK_MUTATION_JOURNAL_MAX_SEGMENT_BYTES_V1 {
      return Err(MarkMutationJournalOwnerErrorV1::InvalidOptions(
        "maximum buffer bytes must cover the target and stay within the 16 MiB cap",
      ));
    }
    Ok(Self { flush_after_records, target_segment_bytes, maximum_buffer_bytes, flush_after_ms })
  }
}

impl Default for MarkMutationJournalBufferOptionsV1 {
  fn default() -> Self {
    Self {
      flush_after_records: MARK_MUTATION_JOURNAL_DEFAULT_FLUSH_RECORDS_V1,
      target_segment_bytes: MARK_MUTATION_JOURNAL_TARGET_SEGMENT_BYTES_V1,
      maximum_buffer_bytes: MARK_MUTATION_JOURNAL_DEFAULT_MAX_BUFFER_BYTES_V1,
      flush_after_ms: MARK_MUTATION_JOURNAL_DEFAULT_FLUSH_AFTER_MS_V1,
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct PreparedMarkMutationJournalSegmentV1<'a> {
  pub segment_ordinal: u64,
  pub generation: u64,
  pub first_publication_sequence: u64,
  pub last_publication_sequence: u64,
  pub record_count: u32,
  pub artifact_key: &'a [u8],
  pub value: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationJournalDurabilityReceiptV1 {
  pub artifact_key: Vec<u8>,
  pub stored_value_length: u32,
  pub hard_publication_sequence: u64,
}

pub struct MarkMutationJournalSinkErrorV1 {
  code: &'static str,
  source: Box<dyn Error + Send + Sync>,
}

impl MarkMutationJournalSinkErrorV1 {
  pub fn new(code: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
    Self { code, source: Box::new(source) }
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }
}

impl fmt::Debug for MarkMutationJournalSinkErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("MarkMutationJournalSinkErrorV1").field("code", &self.code).field("source", &self.source.to_string()).finish()
  }
}

impl Display for MarkMutationJournalSinkErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.source)
  }
}

impl Error for MarkMutationJournalSinkErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    Some(self.source.as_ref())
  }
}

pub trait MarkMutationJournalDurableSinkV1 {
  fn publish_mark_mutation_segment_synced(
    &mut self,
    segment: &PreparedMarkMutationJournalSegmentV1<'_>,
  ) -> Result<MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalSinkErrorV1>;
}

#[must_use = "a run-incomplete observation must not be treated as captured mutation evidence"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkMutationObservationV1 {
  Buffered { flush_due: bool },
  RunIncomplete { code: &'static str, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationJournalOwnerStatusV1 {
  pub pending_records: u32,
  pub pending_record_bytes: usize,
  pub durable_segments: u64,
  pub durable_records: u64,
  pub durable_through_publication_sequence: u64,
  pub last_segment_ordinal: u64,
  pub last_segment_hash: Vec<u8>,
  pub last_hard_publication_sequence: u64,
  pub incomplete: bool,
  pub incomplete_code: Option<&'static str>,
  pub failed: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct MarkMutationJournalChainStartV1<'a> {
  pub algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub run_id: [u8; 16],
  pub generation: u64,
  pub captured_publication_sequence: u64,
  pub options: MarkMutationJournalBufferOptionsV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Debug, ThisError)]
pub enum MarkMutationJournalOwnerErrorV1 {
  #[error("mark mutation journal options are invalid: {0}")]
  InvalidOptions(&'static str),
  #[error("mark mutation journal operation was canceled")]
  Canceled,
  #[error("mark mutation journal monotonic time regressed")]
  ClockRegression,
  #[error("mark mutation journal counters or sizes overflowed")]
  ArithmeticOverflow,
  #[error("mark mutation journal allocation failed: {0}")]
  Allocation(String),
  #[error("mark mutation journal durability receipt does not bind the published segment")]
  ReceiptMismatch,
  #[error("mark mutation journal durable sink failed: {source}")]
  Sink {
    #[source]
    source: MarkMutationJournalSinkErrorV1,
  },
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error(transparent)]
  Memory(#[from] MemoryCoordinatorError),
  #[error("mark mutation journal owner has latched a terminal failure")]
  Failed,
}

impl MarkMutationJournalOwnerErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidOptions(_) => "mark_mutation_options",
      Self::Canceled => "mark_mutation_cancelled",
      Self::ClockRegression => "mark_mutation_clock_regression",
      Self::ArithmeticOverflow => "mark_mutation_arithmetic",
      Self::Allocation(_) => "mark_mutation_allocation",
      Self::ReceiptMismatch => "mark_mutation_receipt",
      Self::Sink { .. } => "mark_mutation_sink",
      Self::Format(error) => error.code(),
      Self::Memory(_) => "mark_mutation_memory",
      Self::Failed => "mark_mutation_owner_failed",
    }
  }
}

pub struct MarkMutationJournalOwnerV1<'a> {
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  run_id: [u8; 16],
  generation: u64,
  options: MarkMutationJournalBufferOptionsV1,
  cancellation: &'a CancellationToken,
  _memory: MemoryReservation,
  records: Vec<u8>,
  pending_records: u32,
  pending_started_at_ms: Option<u64>,
  last_observed_at_ms: Option<u64>,
  last_record_sequence: u64,
  last_mutation_id: Vec<u8>,
  next_segment_ordinal: u64,
  previous_segment_hash: Vec<u8>,
  durable_segments: u64,
  durable_records: u64,
  durable_through_publication_sequence: u64,
  last_hard_publication_sequence: u64,
  incomplete: Option<(&'static str, String)>,
  failed: bool,
}

struct PreparedMarkMutationJournalSegmentOwnedV1 {
  segment_ordinal: u64,
  generation: u64,
  first_publication_sequence: u64,
  last_publication_sequence: u64,
  record_count: u32,
  artifact_key: Vec<u8>,
  value: Vec<u8>,
  stored_value_length: u32,
  selected_record_bytes: usize,
  remaining_records: u32,
  next_durable_segments: u64,
  next_durable_records: u64,
  next_segment_ordinal: u64,
}

impl fmt::Debug for MarkMutationJournalOwnerV1<'_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("MarkMutationJournalOwnerV1")
      .field("generation", &self.generation)
      .field("pending_records", &self.pending_records)
      .field("pending_record_bytes", &self.records.len())
      .field("durable_segments", &self.durable_segments)
      .field("durable_records", &self.durable_records)
      .field("incomplete", &self.incomplete.as_ref().map(|value| value.0))
      .field("failed", &self.failed)
      .finish_non_exhaustive()
  }
}

impl<'a> MarkMutationJournalOwnerV1<'a> {
  pub fn new_chain(
    start: MarkMutationJournalChainStartV1<'a>,
    memory: &MemoryCoordinator,
  ) -> Result<Self, MarkMutationJournalOwnerErrorV1> {
    validate_options(start.algorithm, start.options)?;
    if start.database_id.iter().all(|byte| *byte == 0)
      || start.run_id.iter().all(|byte| *byte == 0)
      || start.generation == 0
      || start.captured_publication_sequence == 0
    {
      return Err(MarkMutationJournalOwnerErrorV1::InvalidOptions("run identity and captured sequence must be nonzero"));
    }
    if start.cancellation.is_cancelled() {
      return Err(MarkMutationJournalOwnerErrorV1::Canceled);
    }
    let reservation = memory.reserve(MemoryOwner::GarbageCollection, required_memory_bytes(start.options)?, AdmissionClass::Maintenance)?;
    let mut records = Vec::new();
    records
      .try_reserve_exact(start.options.maximum_buffer_bytes)
      .map_err(|error| MarkMutationJournalOwnerErrorV1::Allocation(error.to_string()))?;
    Ok(Self {
      algorithm: start.algorithm,
      database_id: start.database_id,
      run_id: start.run_id,
      generation: start.generation,
      options: start.options,
      cancellation: start.cancellation,
      _memory: reservation,
      records,
      pending_records: 0,
      pending_started_at_ms: None,
      last_observed_at_ms: None,
      last_record_sequence: start.captured_publication_sequence,
      last_mutation_id: Vec::new(),
      next_segment_ordinal: 1,
      previous_segment_hash: Vec::new(),
      durable_segments: 0,
      durable_records: 0,
      durable_through_publication_sequence: start.captured_publication_sequence,
      last_hard_publication_sequence: 0,
      incomplete: None,
      failed: false,
    })
  }

  pub fn observe_committed(&mut self, record: MarkMutationRecordWriteV1<'_>, monotonic_now_ms: u64) -> MarkMutationObservationV1 {
    if let Some(incomplete) = &self.incomplete {
      return MarkMutationObservationV1::RunIncomplete { code: incomplete.0, message: incomplete.1.clone() };
    }
    if self.failed {
      return self.latch_incomplete("mark_mutation_owner_failed", "mark mutation owner has a terminal failure");
    }
    if self.cancellation.is_cancelled() {
      return self.latch_incomplete("mark_mutation_cancelled", "mark mutation run was canceled before evidence admission");
    }
    if self.last_observed_at_ms.is_some_and(|previous| monotonic_now_ms < previous) {
      return self.latch_incomplete("mark_mutation_clock_regression", "mark mutation monotonic clock regressed");
    }
    let mut encoded = Vec::with_capacity(mark_mutation_record_length(self.algorithm));
    if let Err(error) = encode_mark_mutation_record(&mut encoded, record, self.algorithm) {
      return self.latch_incomplete(error.code(), error.to_string());
    }
    let expected_next = match self.last_record_sequence.checked_add(1) {
      Some(sequence) => sequence,
      None => return self.latch_incomplete("mark_mutation_sequence_exhausted", "publication sequence is exhausted"),
    };
    let same_sequence = record.publication_sequence == self.last_record_sequence && !self.last_mutation_id.is_empty();
    if same_sequence && record.mutation_id <= self.last_mutation_id.as_slice() {
      return self.latch_incomplete("mark_mutation_order", "mutation ID did not advance within one publication sequence");
    }
    if !same_sequence && record.publication_sequence != expected_next {
      return self.latch_incomplete("mark_mutation_gap", "mutation journal did not observe the next global publication sequence");
    }
    let prospective = match self.records.len().checked_add(encoded.len()) {
      Some(length) => length,
      None => return self.latch_incomplete("mark_mutation_arithmetic", "mutation buffer length overflowed"),
    };
    if prospective > self.options.maximum_buffer_bytes {
      return self.latch_incomplete("mark_mutation_capacity", "mutation evidence exceeded its pre-reserved buffer");
    }
    let pending_records = match self.pending_records.checked_add(1) {
      Some(count) => count,
      None => return self.latch_incomplete("mark_mutation_arithmetic", "mutation record count overflowed"),
    };
    if self.pending_records == 0 {
      self.pending_started_at_ms = Some(monotonic_now_ms);
    }
    self.records.extend_from_slice(&encoded);
    self.pending_records = pending_records;
    self.last_record_sequence = record.publication_sequence;
    self.last_mutation_id.clear();
    self.last_mutation_id.extend_from_slice(record.mutation_id);
    self.last_observed_at_ms = Some(monotonic_now_ms);
    MarkMutationObservationV1::Buffered { flush_due: self.flush_due(monotonic_now_ms) }
  }

  pub fn poll(
    &mut self,
    monotonic_now_ms: u64,
    sink: &mut dyn MarkMutationJournalDurableSinkV1,
  ) -> Result<bool, MarkMutationJournalOwnerErrorV1> {
    self.preflight_background(monotonic_now_ms)?;
    if !self.flush_due(monotonic_now_ms) {
      return Ok(false);
    }
    self.flush_one(sink)
  }

  pub fn flush(&mut self, sink: &mut dyn MarkMutationJournalDurableSinkV1) -> Result<bool, MarkMutationJournalOwnerErrorV1> {
    self.ensure_background_operable()?;
    self.flush_one(sink)
  }

  pub fn status(&self) -> MarkMutationJournalOwnerStatusV1 {
    MarkMutationJournalOwnerStatusV1 {
      pending_records: self.pending_records,
      pending_record_bytes: self.records.len(),
      durable_segments: self.durable_segments,
      durable_records: self.durable_records,
      durable_through_publication_sequence: self.durable_through_publication_sequence,
      last_segment_ordinal: self.next_segment_ordinal.saturating_sub(1),
      last_segment_hash: self.previous_segment_hash.clone(),
      last_hard_publication_sequence: self.last_hard_publication_sequence,
      incomplete: self.incomplete.is_some(),
      incomplete_code: self.incomplete.as_ref().map(|value| value.0),
      failed: self.failed,
    }
  }

  fn preflight_background(&mut self, monotonic_now_ms: u64) -> Result<(), MarkMutationJournalOwnerErrorV1> {
    self.ensure_background_operable()?;
    if self.last_observed_at_ms.is_some_and(|previous| monotonic_now_ms < previous) {
      self.mark_incomplete("mark_mutation_clock_regression", "mark mutation monotonic clock regressed");
      return Err(MarkMutationJournalOwnerErrorV1::ClockRegression);
    }
    self.last_observed_at_ms = Some(monotonic_now_ms);
    Ok(())
  }

  fn ensure_background_operable(&mut self) -> Result<(), MarkMutationJournalOwnerErrorV1> {
    if self.failed {
      return Err(MarkMutationJournalOwnerErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      self.mark_incomplete("mark_mutation_cancelled", "mark mutation run was canceled");
      return Err(MarkMutationJournalOwnerErrorV1::Canceled);
    }
    if let Err(error) = self._memory.check_admission() {
      self.mark_incomplete("mark_mutation_memory", error.to_string());
      return Err(error.into());
    }
    Ok(())
  }

  fn flush_due(&self, monotonic_now_ms: u64) -> bool {
    if self.pending_records == 0 {
      return false;
    }
    self.pending_records >= self.options.flush_after_records
      || complete_segment_length(self.algorithm, self.records.len()) >= self.options.target_segment_bytes
      || self.pending_started_at_ms.is_some_and(|started| monotonic_now_ms.saturating_sub(started) >= self.options.flush_after_ms)
  }

  fn flush_one(&mut self, sink: &mut dyn MarkMutationJournalDurableSinkV1) -> Result<bool, MarkMutationJournalOwnerErrorV1> {
    if self.pending_records == 0 {
      return Ok(false);
    }
    let owned = match self.prepare_next_segment() {
      Ok(owned) => owned,
      Err(error) => {
        self.mark_incomplete(error.code(), error.to_string());
        return Err(error);
      }
    };
    let prepared = PreparedMarkMutationJournalSegmentV1 {
      segment_ordinal: owned.segment_ordinal,
      generation: owned.generation,
      first_publication_sequence: owned.first_publication_sequence,
      last_publication_sequence: owned.last_publication_sequence,
      record_count: owned.record_count,
      artifact_key: &owned.artifact_key,
      value: &owned.value,
    };
    let receipt = match sink.publish_mark_mutation_segment_synced(&prepared) {
      Ok(receipt) => receipt,
      Err(source) => {
        self.mark_incomplete("mark_mutation_sink", source.to_string());
        return Err(MarkMutationJournalOwnerErrorV1::Sink { source });
      }
    };
    if receipt.artifact_key != owned.artifact_key
      || receipt.stored_value_length != owned.stored_value_length
      || receipt.hard_publication_sequence <= self.last_hard_publication_sequence
    {
      self.failed = true;
      self.mark_incomplete("mark_mutation_receipt", "durability receipt did not bind the exact mutation segment");
      return Err(MarkMutationJournalOwnerErrorV1::ReceiptMismatch);
    }
    self.durable_segments = owned.next_durable_segments;
    self.durable_records = owned.next_durable_records;
    self.durable_through_publication_sequence = owned.last_publication_sequence;
    self.last_hard_publication_sequence = receipt.hard_publication_sequence;
    self.previous_segment_hash = owned.artifact_key;
    self.next_segment_ordinal = owned.next_segment_ordinal;
    self.records.drain(..owned.selected_record_bytes);
    self.pending_records = owned.remaining_records;
    if self.pending_records == 0 {
      self.pending_started_at_ms = None;
    }
    Ok(true)
  }

  fn prepare_next_segment(&self) -> Result<PreparedMarkMutationJournalSegmentOwnedV1, MarkMutationJournalOwnerErrorV1> {
    let record_length = mark_mutation_record_length(self.algorithm);
    let fixed = complete_segment_length(self.algorithm, 0);
    let maximum_records = self
      .options
      .target_segment_bytes
      .checked_sub(fixed)
      .map(|bytes| bytes / record_length)
      .filter(|count| *count != 0)
      .ok_or(MarkMutationJournalOwnerErrorV1::InvalidOptions("target segment cannot hold one mutation record"))?;
    let selected_records = (self.pending_records as usize).min(maximum_records);
    let selected_bytes = selected_records.checked_mul(record_length).ok_or(MarkMutationJournalOwnerErrorV1::ArithmeticOverflow)?;
    let encoded = encode_mark_mutation_journal_segment_records_v1(
      self.algorithm,
      &self.database_id,
      &self.run_id,
      self.generation,
      self.next_segment_ordinal,
      (!self.previous_segment_hash.is_empty()).then_some(self.previous_segment_hash.as_slice()),
      &self.records[..selected_bytes],
    )?;
    let GcMarkArtifactV1::MutationJournal(decoded) = decode_gc_mark_artifact(&encoded.value, self.algorithm)? else {
      return Err(MarkMutationJournalOwnerErrorV1::Format(FormatError::new(
        super::reader::MalformedInputClass::CrossRecordClosureMismatch,
        "mark_mutation_owner_readback",
        "prepared mutation segment decoded as another artifact kind",
      )));
    };
    let next_durable_segments = self.durable_segments.checked_add(1).ok_or(MarkMutationJournalOwnerErrorV1::ArithmeticOverflow)?;
    let next_durable_records =
      self.durable_records.checked_add(u64::from(decoded.record_count)).ok_or(MarkMutationJournalOwnerErrorV1::ArithmeticOverflow)?;
    let next_segment_ordinal = self.next_segment_ordinal.checked_add(1).ok_or(MarkMutationJournalOwnerErrorV1::ArithmeticOverflow)?;
    let remaining_records =
      self.pending_records.checked_sub(decoded.record_count).ok_or(MarkMutationJournalOwnerErrorV1::ArithmeticOverflow)?;
    let stored_value_length = encoded.value.len() as u32;
    Ok(PreparedMarkMutationJournalSegmentOwnedV1 {
      segment_ordinal: decoded.segment_sequence,
      generation: decoded.generation,
      first_publication_sequence: decoded.first_sequence,
      last_publication_sequence: decoded.last_sequence,
      record_count: decoded.record_count,
      artifact_key: encoded.key,
      value: encoded.value,
      stored_value_length,
      selected_record_bytes: selected_bytes,
      remaining_records,
      next_durable_segments,
      next_durable_records,
      next_segment_ordinal,
    })
  }

  fn latch_incomplete(&mut self, code: &'static str, message: impl Into<String>) -> MarkMutationObservationV1 {
    if let Some(incomplete) = &self.incomplete {
      return MarkMutationObservationV1::RunIncomplete { code: incomplete.0, message: incomplete.1.clone() };
    }
    let message = message.into();
    self.incomplete = Some((code, message.clone()));
    MarkMutationObservationV1::RunIncomplete { code, message }
  }

  fn mark_incomplete(&mut self, code: &'static str, message: impl Into<String>) {
    if self.incomplete.is_none() {
      self.incomplete = Some((code, message.into()));
    }
  }
}

fn validate_options(algorithm: HashAlgorithm, options: MarkMutationJournalBufferOptionsV1) -> Result<(), MarkMutationJournalOwnerErrorV1> {
  let minimum = complete_segment_length(algorithm, mark_mutation_record_length(algorithm));
  if options.flush_after_records == 0 || options.flush_after_ms == 0 {
    return Err(MarkMutationJournalOwnerErrorV1::InvalidOptions("flush record count and interval must be nonzero"));
  }
  if options.target_segment_bytes < minimum || options.target_segment_bytes > MARK_MUTATION_JOURNAL_MAX_SEGMENT_BYTES_V1 {
    return Err(MarkMutationJournalOwnerErrorV1::InvalidOptions("target segment must hold one record and stay within 16 MiB"));
  }
  if options.maximum_buffer_bytes < mark_mutation_record_length(algorithm)
    || options.maximum_buffer_bytes < options.target_segment_bytes
    || options.maximum_buffer_bytes > MARK_MUTATION_JOURNAL_MAX_SEGMENT_BYTES_V1
  {
    return Err(MarkMutationJournalOwnerErrorV1::InvalidOptions("maximum buffer is outside the admitted bounded range"));
  }
  Ok(())
}

fn required_memory_bytes(options: MarkMutationJournalBufferOptionsV1) -> Result<u64, MarkMutationJournalOwnerErrorV1> {
  let bytes = options
    .maximum_buffer_bytes
    .checked_add(options.target_segment_bytes.saturating_mul(2))
    .and_then(|bytes| bytes.checked_add(64 * 1024))
    .ok_or(MarkMutationJournalOwnerErrorV1::ArithmeticOverflow)?;
  Ok(bytes as u64)
}

fn mark_mutation_record_length(algorithm: HashAlgorithm) -> usize {
  40 + 6 * algorithm.hash_length()
}

fn complete_segment_length(algorithm: HashAlgorithm, record_bytes: usize) -> usize {
  108usize.saturating_add(algorithm.hash_length()).saturating_add(record_bytes)
}
