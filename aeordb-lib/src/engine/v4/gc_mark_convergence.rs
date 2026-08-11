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
  GcMarkArtifactV1, MarkMutationRecordV1, MarkMutationRecordWriteV1, MarkWorkspaceObjectKindV1, decode_gc_mark_artifact,
  decode_mark_workspace_object, encode_mark_mutation_journal_segment_records_v1, encode_mark_mutation_record,
  mark_mutation_journal_records_v1, mark_workspace_mutation_records_v1,
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
    if receipt.hard_publication_sequence > self.last_record_sequence {
      self.last_record_sequence = receipt.hard_publication_sequence;
      self.last_mutation_id.clear();
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkMutationConvergenceOptionsV1 {
  pub maximum_records: u64,
  pub maximum_catch_up_rounds: u32,
}

impl MarkMutationConvergenceOptionsV1 {
  pub fn new(maximum_records: u64, maximum_catch_up_rounds: u32) -> Result<Self, MarkMutationConvergenceErrorV1> {
    if maximum_records == 0 || maximum_catch_up_rounds == 0 {
      return Err(MarkMutationConvergenceErrorV1::InvalidOptions("record and catch-up-round limits must be nonzero"));
    }
    Ok(Self { maximum_records, maximum_catch_up_rounds })
  }
}

#[derive(Debug, Clone, Copy)]
pub struct MarkMutationConvergenceBasisV1<'a> {
  pub algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub run_id: [u8; 16],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub kv_layout_generation: u64,
  pub kv_layout_fingerprint: &'a [u8],
  pub reconciled_root_hash: &'a [u8],
  pub reconciled_through_publication_sequence: u64,
  pub mutation_journal_head: Option<&'a [u8]>,
  pub mutation_journal_segment_ordinal: u64,
  pub options: MarkMutationConvergenceOptionsV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationBoundaryV1 {
  /// Empty only when `publication_sequence` is already a complete publication
  /// boundary for the supplied cursor; otherwise this is its terminal record.
  pub mutation_id: Vec<u8>,
  pub kv_layout_generation: u64,
  pub kv_layout_fingerprint: Vec<u8>,
  pub authority_root_hash: Vec<u8>,
  pub publication_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationCursorV1 {
  pub publication_sequence: u64,
  pub mutation_id: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationCatchUpV1 {
  pub rounds: u32,
  pub boundary: MarkMutationBoundaryV1,
}

#[derive(Debug, Clone, Copy)]
pub enum MarkMutationEncodedRunV1<'a> {
  WorkspaceObject(&'a [u8]),
  JournalSegment(&'a [u8]),
}

pub trait MarkMutationRunVisitorV1 {
  fn visit_run(&mut self, run: MarkMutationEncodedRunV1<'_>) -> Result<(), MarkMutationConvergenceErrorV1>;
}

pub trait MarkMutationDrainSourceV1 {
  fn capture_boundary(&mut self, after: &MarkMutationCursorV1) -> Result<MarkMutationBoundaryV1, MarkMutationConvergenceErrorV1>;

  fn visit_runs(
    &mut self,
    after: &MarkMutationCursorV1,
    through_publication_sequence: u64,
    visitor: &mut dyn MarkMutationRunVisitorV1,
  ) -> Result<(), MarkMutationConvergenceErrorV1>;
}

pub struct MarkMutationApplyErrorV1 {
  code: &'static str,
  source: Box<dyn Error + Send + Sync>,
}

impl MarkMutationApplyErrorV1 {
  pub fn new(code: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
    Self { code, source: Box::new(source) }
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }
}

impl fmt::Debug for MarkMutationApplyErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("MarkMutationApplyErrorV1").field("code", &self.code).field("source", &self.source.to_string()).finish()
  }
}

impl Display for MarkMutationApplyErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.source)
  }
}

impl Error for MarkMutationApplyErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    Some(self.source.as_ref())
  }
}

pub trait MarkMutationApplierV1 {
  fn apply(&mut self, record: &MarkMutationRecordV1<'_>) -> Result<(), MarkMutationApplyErrorV1>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationFinalPublicationRequestV1 {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub run_id: [u8; 16],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub kv_layout_generation: u64,
  pub kv_layout_fingerprint: Vec<u8>,
  pub authority_root_hash: Vec<u8>,
  pub reconciled_through_publication_sequence: u64,
  pub reconciled_through_mutation_id: Vec<u8>,
  pub mutation_journal_head: Vec<u8>,
  pub applied_records: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationFinalPublicationReceiptV1 {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub run_id: [u8; 16],
  pub generation: u64,
  pub checkpoint_sequence: u64,
  pub kv_layout_generation: u64,
  pub kv_layout_fingerprint: Vec<u8>,
  pub authority_root_hash: Vec<u8>,
  pub reconciled_through_publication_sequence: u64,
  pub reconciled_through_mutation_id: Vec<u8>,
  pub mutation_journal_head: Vec<u8>,
  pub applied_records: u64,
  pub hard_publication_sequence: u64,
}

pub trait MarkMutationFinalGuardSessionV1: MarkMutationDrainSourceV1 {
  fn publish_final(
    &mut self,
    request: &MarkMutationFinalPublicationRequestV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1>;
}

pub trait MarkMutationFinalGuardOperationV1 {
  fn execute(
    &mut self,
    session: &mut dyn MarkMutationFinalGuardSessionV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1>;
}

pub trait MarkMutationFinalGuardAuthorityV1 {
  fn execute_exclusively(
    &mut self,
    operation: &mut dyn MarkMutationFinalGuardOperationV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkMutationRestartReasonV1 {
  JournalGap,
  RootMismatch,
  ContextMismatch,
  MalformedRun,
  RecordLimit,
  LayoutChanged,
  Starvation,
  ApplyFailure,
  Canceled,
  MemoryPressure,
  FinalDrainNotEmpty,
  SourceFailure,
  PublicationFailure,
}

impl MarkMutationRestartReasonV1 {
  pub const fn code(self) -> &'static str {
    match self {
      Self::JournalGap => "mark_convergence_journal_gap",
      Self::RootMismatch => "mark_convergence_root_mismatch",
      Self::ContextMismatch => "mark_convergence_context_mismatch",
      Self::MalformedRun => "mark_convergence_malformed_run",
      Self::RecordLimit => "mark_convergence_record_limit",
      Self::LayoutChanged => "mark_convergence_layout_changed",
      Self::Starvation => "mark_convergence_starvation",
      Self::ApplyFailure => "mark_convergence_apply_failure",
      Self::Canceled => "mark_convergence_cancelled",
      Self::MemoryPressure => "mark_convergence_memory_pressure",
      Self::FinalDrainNotEmpty => "mark_convergence_final_drain_not_empty",
      Self::SourceFailure => "mark_convergence_source_failure",
      Self::PublicationFailure => "mark_convergence_publication_failure",
    }
  }
}

#[derive(Debug, ThisError)]
pub enum MarkMutationConvergenceErrorV1 {
  #[error("mark mutation convergence options are invalid: {0}")]
  InvalidOptions(&'static str),
  #[error("mark mutation convergence was canceled")]
  Canceled,
  #[error("mark mutation convergence requires restart: {0:?}")]
  RestartRequired(MarkMutationRestartReasonV1),
  #[error("mark mutation application failed: {0}")]
  Apply(#[source] MarkMutationApplyErrorV1),
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error(transparent)]
  Memory(#[from] MemoryCoordinatorError),
}

impl MarkMutationConvergenceErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidOptions(_) => "mark_convergence_options",
      Self::Canceled => "mark_convergence_cancelled",
      Self::RestartRequired(reason) => reason.code(),
      Self::Apply(error) => error.code(),
      Self::Format(error) => error.code(),
      Self::Memory(_) => "mark_convergence_memory",
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkMutationReconcilerStatusV1 {
  pub reconciled_through_publication_sequence: u64,
  pub last_mutation_id: Vec<u8>,
  pub current_root_hash: Vec<u8>,
  pub mutation_journal_head: Vec<u8>,
  pub mutation_journal_segment_ordinal: u64,
  pub applied_records: u64,
  pub final_publication_sequence: Option<u64>,
  pub restart_required: Option<MarkMutationRestartReasonV1>,
}

pub struct MarkMutationReconcilerV1<'a> {
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  run_id: [u8; 16],
  generation: u64,
  checkpoint_sequence: u64,
  kv_layout_generation: u64,
  kv_layout_fingerprint: Vec<u8>,
  options: MarkMutationConvergenceOptionsV1,
  cancellation: &'a CancellationToken,
  _memory: MemoryReservation,
  current_root_hash: Vec<u8>,
  last_publication_sequence: u64,
  last_mutation_id: Vec<u8>,
  publication_root_before: Vec<u8>,
  publication_root_after: Vec<u8>,
  mutation_journal_head: Vec<u8>,
  mutation_journal_segment_ordinal: u64,
  applied_records: u64,
  final_publication_sequence: Option<u64>,
  restart_required: Option<MarkMutationRestartReasonV1>,
}

impl fmt::Debug for MarkMutationReconcilerV1<'_> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("MarkMutationReconcilerV1")
      .field("generation", &self.generation)
      .field("checkpoint_sequence", &self.checkpoint_sequence)
      .field("last_publication_sequence", &self.last_publication_sequence)
      .field("mutation_journal_segment_ordinal", &self.mutation_journal_segment_ordinal)
      .field("applied_records", &self.applied_records)
      .field("final_publication_sequence", &self.final_publication_sequence)
      .field("restart_required", &self.restart_required)
      .finish_non_exhaustive()
  }
}

impl<'a> MarkMutationReconcilerV1<'a> {
  pub fn new(basis: MarkMutationConvergenceBasisV1<'a>, memory: &MemoryCoordinator) -> Result<Self, MarkMutationConvergenceErrorV1> {
    let width = basis.algorithm.hash_length();
    if basis.database_id.iter().all(|byte| *byte == 0)
      || basis.run_id.iter().all(|byte| *byte == 0)
      || basis.generation == 0
      || basis.checkpoint_sequence == 0
      || basis.kv_layout_generation == 0
      || basis.reconciled_through_publication_sequence == 0
      || !valid_nonzero_width(basis.kv_layout_fingerprint, width)
      || !valid_nonzero_width(basis.reconciled_root_hash, width)
    {
      return Err(MarkMutationConvergenceErrorV1::InvalidOptions("basis identity, layout, root, and sequence must be canonical"));
    }
    match basis.mutation_journal_head {
      Some(head) if !valid_nonzero_width(head, width) || basis.mutation_journal_segment_ordinal == 0 => {
        return Err(MarkMutationConvergenceErrorV1::InvalidOptions("journal head and ordinal disagree"));
      }
      None if basis.mutation_journal_segment_ordinal != 0 => {
        return Err(MarkMutationConvergenceErrorV1::InvalidOptions("journal ordinal exists without a head"));
      }
      Some(_) | None => {}
    }
    if basis.options.maximum_records == 0 || basis.options.maximum_catch_up_rounds == 0 {
      return Err(MarkMutationConvergenceErrorV1::InvalidOptions("record and catch-up-round limits must be nonzero"));
    }
    if basis.cancellation.is_cancelled() {
      return Err(MarkMutationConvergenceErrorV1::Canceled);
    }
    let reservation = memory.reserve(MemoryOwner::GarbageCollection, 64 * 1024, AdmissionClass::Maintenance)?;
    Ok(Self {
      algorithm: basis.algorithm,
      database_id: basis.database_id,
      run_id: basis.run_id,
      generation: basis.generation,
      checkpoint_sequence: basis.checkpoint_sequence,
      kv_layout_generation: basis.kv_layout_generation,
      kv_layout_fingerprint: basis.kv_layout_fingerprint.to_vec(),
      options: basis.options,
      cancellation: basis.cancellation,
      _memory: reservation,
      current_root_hash: basis.reconciled_root_hash.to_vec(),
      last_publication_sequence: basis.reconciled_through_publication_sequence,
      last_mutation_id: Vec::with_capacity(width),
      publication_root_before: Vec::with_capacity(width),
      publication_root_after: Vec::with_capacity(width),
      mutation_journal_head: basis.mutation_journal_head.map_or_else(Vec::new, ToOwned::to_owned),
      mutation_journal_segment_ordinal: basis.mutation_journal_segment_ordinal,
      applied_records: 0,
      final_publication_sequence: None,
      restart_required: None,
    })
  }

  pub fn reconcile_run(
    &mut self,
    run: MarkMutationEncodedRunV1<'_>,
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<(), MarkMutationConvergenceErrorV1> {
    self.preflight()?;
    match run {
      MarkMutationEncodedRunV1::WorkspaceObject(bytes) => self.reconcile_workspace_run(bytes, applier),
      MarkMutationEncodedRunV1::JournalSegment(bytes) => self.reconcile_journal_segment(bytes, applier),
    }
  }

  pub fn status(&self) -> MarkMutationReconcilerStatusV1 {
    MarkMutationReconcilerStatusV1 {
      reconciled_through_publication_sequence: self.last_publication_sequence,
      last_mutation_id: self.last_mutation_id.clone(),
      current_root_hash: self.current_root_hash.clone(),
      mutation_journal_head: self.mutation_journal_head.clone(),
      mutation_journal_segment_ordinal: self.mutation_journal_segment_ordinal,
      applied_records: self.applied_records,
      final_publication_sequence: self.final_publication_sequence,
      restart_required: self.restart_required,
    }
  }

  pub fn catch_up(
    &mut self,
    source: &mut dyn MarkMutationDrainSourceV1,
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<MarkMutationCatchUpV1, MarkMutationConvergenceErrorV1> {
    for round in 1..=self.options.maximum_catch_up_rounds {
      let target = self.capture_boundary(source)?;
      self.drain_through(source, &target, applier)?;
      self.require_exact_boundary(&target)?;

      let observed = self.capture_boundary(source)?;
      if observed == target {
        return Ok(MarkMutationCatchUpV1 { rounds: round, boundary: target });
      }
      let advances = observed.publication_sequence > target.publication_sequence
        || (observed.publication_sequence == target.publication_sequence && observed.mutation_id > target.mutation_id);
      if !advances {
        return Err(self.restart(MarkMutationRestartReasonV1::JournalGap));
      }
    }
    Err(self.restart(MarkMutationRestartReasonV1::Starvation))
  }

  pub fn finalize_guarded(
    &mut self,
    authority: &mut dyn MarkMutationFinalGuardAuthorityV1,
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    self.preflight()?;
    if self.final_publication_sequence.is_some() {
      return Err(MarkMutationConvergenceErrorV1::InvalidOptions("mark mutation convergence is already finalized"));
    }

    let authority_result;
    let operation_receipt;
    {
      let mut operation = GuardedFinalPublicationOperationV1 { reconciler: self, applier, executed: false, receipt: None };
      authority_result = authority.execute_exclusively(&mut operation);
      operation_receipt = operation.receipt;
    }

    let authority_receipt = match authority_result {
      Ok(receipt) => receipt,
      Err(error) => return Err(self.external_failure(error, MarkMutationRestartReasonV1::PublicationFailure)),
    };
    if operation_receipt.as_ref() != Some(&authority_receipt) {
      return Err(self.restart(MarkMutationRestartReasonV1::PublicationFailure));
    }
    self.final_publication_sequence = Some(authority_receipt.hard_publication_sequence);
    Ok(authority_receipt)
  }

  fn capture_boundary<S: MarkMutationDrainSourceV1 + ?Sized>(
    &mut self,
    source: &mut S,
  ) -> Result<MarkMutationBoundaryV1, MarkMutationConvergenceErrorV1> {
    self.preflight()?;
    let boundary =
      source.capture_boundary(&self.cursor()).map_err(|error| self.external_failure(error, MarkMutationRestartReasonV1::SourceFailure))?;
    self.validate_boundary(&boundary)?;
    Ok(boundary)
  }

  fn validate_boundary(&mut self, boundary: &MarkMutationBoundaryV1) -> Result<(), MarkMutationConvergenceErrorV1> {
    let width = self.algorithm.hash_length();
    if boundary.publication_sequence == 0
      || boundary.kv_layout_generation == 0
      || !valid_nonzero_width(&boundary.kv_layout_fingerprint, width)
      || !valid_nonzero_width(&boundary.authority_root_hash, width)
      || (!boundary.mutation_id.is_empty() && !valid_nonzero_width(&boundary.mutation_id, width))
    {
      return Err(self.restart(MarkMutationRestartReasonV1::ContextMismatch));
    }
    if boundary.kv_layout_generation != self.kv_layout_generation || boundary.kv_layout_fingerprint != self.kv_layout_fingerprint {
      return Err(self.restart(MarkMutationRestartReasonV1::LayoutChanged));
    }
    if boundary.publication_sequence < self.last_publication_sequence {
      return Err(self.restart(MarkMutationRestartReasonV1::JournalGap));
    }
    if boundary.publication_sequence == self.last_publication_sequence {
      if boundary.authority_root_hash != self.current_root_hash {
        return Err(self.restart(MarkMutationRestartReasonV1::RootMismatch));
      }
      let is_same_cursor = boundary.mutation_id == self.last_mutation_id;
      let advances_within_publication = !self.last_mutation_id.is_empty() && boundary.mutation_id > self.last_mutation_id;
      if !is_same_cursor && !advances_within_publication {
        return Err(self.restart(MarkMutationRestartReasonV1::JournalGap));
      }
    } else if boundary.mutation_id.is_empty() {
      return Err(self.restart(MarkMutationRestartReasonV1::JournalGap));
    }
    Ok(())
  }

  fn drain_through<S: MarkMutationDrainSourceV1 + ?Sized>(
    &mut self,
    source: &mut S,
    boundary: &MarkMutationBoundaryV1,
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<u64, MarkMutationConvergenceErrorV1> {
    self.preflight()?;
    let cursor = self.cursor();
    let applied_before = self.applied_records;
    let visit_result = {
      let mut visitor = ReconcilerRunVisitorV1 { reconciler: self, applier };
      source.visit_runs(&cursor, boundary.publication_sequence, &mut visitor)
    };
    if let Err(error) = visit_result {
      return Err(self.external_failure(error, MarkMutationRestartReasonV1::SourceFailure));
    }
    self.applied_records.checked_sub(applied_before).ok_or_else(|| self.restart(MarkMutationRestartReasonV1::SourceFailure))
  }

  fn require_exact_boundary(&mut self, boundary: &MarkMutationBoundaryV1) -> Result<(), MarkMutationConvergenceErrorV1> {
    if self.last_publication_sequence != boundary.publication_sequence {
      return Err(self.restart(MarkMutationRestartReasonV1::JournalGap));
    }
    if self.current_root_hash != boundary.authority_root_hash {
      return Err(self.restart(MarkMutationRestartReasonV1::RootMismatch));
    }
    if self.last_mutation_id != boundary.mutation_id {
      return Err(self.restart(MarkMutationRestartReasonV1::JournalGap));
    }
    Ok(())
  }

  fn cursor(&self) -> MarkMutationCursorV1 {
    MarkMutationCursorV1 { publication_sequence: self.last_publication_sequence, mutation_id: self.last_mutation_id.clone() }
  }

  fn finalize_under_guard(
    &mut self,
    session: &mut dyn MarkMutationFinalGuardSessionV1,
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    let target = self.capture_boundary(session)?;
    self.drain_through(session, &target, applier)?;
    self.require_exact_boundary(&target)?;

    let stable = self.capture_boundary(session)?;
    if stable != target {
      return Err(self.restart(MarkMutationRestartReasonV1::FinalDrainNotEmpty));
    }
    let second_drain_records = self.drain_through(session, &stable, applier)?;
    if second_drain_records != 0 {
      return Err(self.restart(MarkMutationRestartReasonV1::FinalDrainNotEmpty));
    }
    self.require_exact_boundary(&stable)?;

    let confirmed = self.capture_boundary(session)?;
    if confirmed != stable {
      return Err(self.restart(MarkMutationRestartReasonV1::FinalDrainNotEmpty));
    }

    let request = MarkMutationFinalPublicationRequestV1 {
      hash_algorithm: self.algorithm,
      database_id: self.database_id,
      run_id: self.run_id,
      generation: self.generation,
      checkpoint_sequence: self.checkpoint_sequence,
      kv_layout_generation: self.kv_layout_generation,
      kv_layout_fingerprint: self.kv_layout_fingerprint.clone(),
      authority_root_hash: self.current_root_hash.clone(),
      reconciled_through_publication_sequence: self.last_publication_sequence,
      reconciled_through_mutation_id: self.last_mutation_id.clone(),
      mutation_journal_head: self.mutation_journal_head.clone(),
      applied_records: self.applied_records,
    };
    let receipt =
      session.publish_final(&request).map_err(|error| self.external_failure(error, MarkMutationRestartReasonV1::PublicationFailure))?;
    if !final_receipt_matches(&request, &receipt) {
      return Err(self.restart(MarkMutationRestartReasonV1::PublicationFailure));
    }
    Ok(receipt)
  }

  fn reconcile_workspace_run(
    &mut self,
    bytes: &[u8],
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<(), MarkMutationConvergenceErrorV1> {
    let object = decode_mark_workspace_object(bytes, self.algorithm).map_err(|error| self.format_failure(error))?;
    if object.kind != MarkWorkspaceObjectKindV1::Mutation
      || object.database_id != self.database_id
      || object.run_id != self.run_id
      || object.generation != self.generation
      || object.checkpoint_sequence != self.checkpoint_sequence
    {
      return Err(self.restart(MarkMutationRestartReasonV1::ContextMismatch));
    }
    let records = mark_workspace_mutation_records_v1(bytes, self.algorithm).map_err(|error| self.format_failure(error))?;
    for record in records {
      let record = record.map_err(|error| self.format_failure(error))?;
      self.reconcile_record(&record, applier)?;
    }
    Ok(())
  }

  fn reconcile_journal_segment(
    &mut self,
    bytes: &[u8],
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<(), MarkMutationConvergenceErrorV1> {
    let GcMarkArtifactV1::MutationJournal(segment) =
      decode_gc_mark_artifact(bytes, self.algorithm).map_err(|error| self.format_failure(error))?
    else {
      return Err(self.restart(MarkMutationRestartReasonV1::MalformedRun));
    };
    let expected_ordinal =
      self.mutation_journal_segment_ordinal.checked_add(1).ok_or_else(|| self.restart(MarkMutationRestartReasonV1::MalformedRun))?;
    let expected_reset = self.mutation_journal_head.is_empty();
    let predecessor_matches =
      if expected_reset { segment.predecessor.iter().all(|byte| *byte == 0) } else { segment.predecessor == self.mutation_journal_head };
    if segment.database_id != self.database_id
      || segment.run_id != self.run_id
      || segment.generation != self.generation
      || segment.segment_sequence != expected_ordinal
      || segment.reset != expected_reset
      || !predecessor_matches
    {
      return Err(self.restart(MarkMutationRestartReasonV1::ContextMismatch));
    }
    let records = mark_mutation_journal_records_v1(&segment, self.algorithm).map_err(|error| self.format_failure(error))?;
    for record in records {
      let record = record.map_err(|error| self.format_failure(error))?;
      self.reconcile_record(&record, applier)?;
    }
    self.mutation_journal_segment_ordinal = segment.segment_sequence;
    self.mutation_journal_head = segment.key;
    Ok(())
  }

  fn reconcile_record(
    &mut self,
    record: &MarkMutationRecordV1<'_>,
    applier: &mut dyn MarkMutationApplierV1,
  ) -> Result<(), MarkMutationConvergenceErrorV1> {
    self.preflight()?;
    let next_applied = self
      .applied_records
      .checked_add(1)
      .filter(|count| *count <= self.options.maximum_records)
      .ok_or_else(|| self.restart(MarkMutationRestartReasonV1::RecordLimit))?;
    let same_publication = record.publication_sequence == self.last_publication_sequence && !self.last_mutation_id.is_empty();
    if same_publication {
      if record.mutation_id <= self.last_mutation_id.as_slice()
        || record.root_before != self.publication_root_before
        || record.root_after != self.publication_root_after
      {
        return Err(self.restart(MarkMutationRestartReasonV1::RootMismatch));
      }
    } else {
      let expected = self.last_publication_sequence.checked_add(1).ok_or_else(|| self.restart(MarkMutationRestartReasonV1::JournalGap))?;
      if record.publication_sequence != expected {
        return Err(self.restart(MarkMutationRestartReasonV1::JournalGap));
      }
      if record.root_before != self.current_root_hash {
        return Err(self.restart(MarkMutationRestartReasonV1::RootMismatch));
      }
    }
    if let Err(error) = applier.apply(record) {
      self.restart_required.get_or_insert(MarkMutationRestartReasonV1::ApplyFailure);
      return Err(MarkMutationConvergenceErrorV1::Apply(error));
    }
    if !same_publication {
      replace_bytes(&mut self.publication_root_before, record.root_before);
      replace_bytes(&mut self.publication_root_after, record.root_after);
    }
    replace_bytes(&mut self.current_root_hash, record.root_after);
    replace_bytes(&mut self.last_mutation_id, record.mutation_id);
    self.last_publication_sequence = record.publication_sequence;
    self.applied_records = next_applied;
    Ok(())
  }

  fn preflight(&mut self) -> Result<(), MarkMutationConvergenceErrorV1> {
    if let Some(reason) = self.restart_required {
      return Err(MarkMutationConvergenceErrorV1::RestartRequired(reason));
    }
    if self.final_publication_sequence.is_some() {
      return Err(MarkMutationConvergenceErrorV1::InvalidOptions("mark mutation convergence is already finalized"));
    }
    if self.cancellation.is_cancelled() {
      self.restart_required.get_or_insert(MarkMutationRestartReasonV1::Canceled);
      return Err(MarkMutationConvergenceErrorV1::Canceled);
    }
    if let Err(error) = self._memory.check_admission() {
      self.restart_required.get_or_insert(MarkMutationRestartReasonV1::MemoryPressure);
      return Err(MarkMutationConvergenceErrorV1::Memory(error));
    }
    Ok(())
  }

  fn restart(&mut self, reason: MarkMutationRestartReasonV1) -> MarkMutationConvergenceErrorV1 {
    let selected = *self.restart_required.get_or_insert(reason);
    MarkMutationConvergenceErrorV1::RestartRequired(selected)
  }

  fn format_failure(&mut self, error: FormatError) -> MarkMutationConvergenceErrorV1 {
    self.restart_required.get_or_insert(MarkMutationRestartReasonV1::MalformedRun);
    MarkMutationConvergenceErrorV1::Format(error)
  }

  fn external_failure(
    &mut self,
    error: MarkMutationConvergenceErrorV1,
    fallback: MarkMutationRestartReasonV1,
  ) -> MarkMutationConvergenceErrorV1 {
    match error {
      MarkMutationConvergenceErrorV1::RestartRequired(reason) => self.restart(reason),
      MarkMutationConvergenceErrorV1::Canceled => {
        self.restart_required.get_or_insert(fallback);
        MarkMutationConvergenceErrorV1::Canceled
      }
      MarkMutationConvergenceErrorV1::Apply(error) => {
        self.restart_required.get_or_insert(fallback);
        MarkMutationConvergenceErrorV1::Apply(error)
      }
      MarkMutationConvergenceErrorV1::Format(error) => {
        self.restart_required.get_or_insert(fallback);
        MarkMutationConvergenceErrorV1::Format(error)
      }
      MarkMutationConvergenceErrorV1::Memory(error) => {
        self.restart_required.get_or_insert(fallback);
        MarkMutationConvergenceErrorV1::Memory(error)
      }
      MarkMutationConvergenceErrorV1::InvalidOptions(_) => self.restart(fallback),
    }
  }
}

struct ReconcilerRunVisitorV1<'operation, 'cancellation> {
  reconciler: &'operation mut MarkMutationReconcilerV1<'cancellation>,
  applier: &'operation mut dyn MarkMutationApplierV1,
}

impl MarkMutationRunVisitorV1 for ReconcilerRunVisitorV1<'_, '_> {
  fn visit_run(&mut self, run: MarkMutationEncodedRunV1<'_>) -> Result<(), MarkMutationConvergenceErrorV1> {
    self.reconciler.reconcile_run(run, self.applier)
  }
}

struct GuardedFinalPublicationOperationV1<'operation, 'cancellation> {
  reconciler: &'operation mut MarkMutationReconcilerV1<'cancellation>,
  applier: &'operation mut dyn MarkMutationApplierV1,
  executed: bool,
  receipt: Option<MarkMutationFinalPublicationReceiptV1>,
}

impl MarkMutationFinalGuardOperationV1 for GuardedFinalPublicationOperationV1<'_, '_> {
  fn execute(
    &mut self,
    session: &mut dyn MarkMutationFinalGuardSessionV1,
  ) -> Result<MarkMutationFinalPublicationReceiptV1, MarkMutationConvergenceErrorV1> {
    if self.executed {
      return Err(self.reconciler.restart(MarkMutationRestartReasonV1::PublicationFailure));
    }
    self.executed = true;
    let receipt = self.reconciler.finalize_under_guard(session, self.applier)?;
    self.receipt = Some(receipt.clone());
    Ok(receipt)
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

fn valid_nonzero_width(bytes: &[u8], width: usize) -> bool {
  bytes.len() == width && bytes.iter().any(|byte| *byte != 0)
}

fn replace_bytes(destination: &mut Vec<u8>, source: &[u8]) {
  destination.clear();
  destination.extend_from_slice(source);
}

fn final_receipt_matches(request: &MarkMutationFinalPublicationRequestV1, receipt: &MarkMutationFinalPublicationReceiptV1) -> bool {
  receipt.hash_algorithm == request.hash_algorithm
    && receipt.database_id == request.database_id
    && receipt.run_id == request.run_id
    && receipt.generation == request.generation
    && receipt.checkpoint_sequence == request.checkpoint_sequence
    && receipt.kv_layout_generation == request.kv_layout_generation
    && receipt.kv_layout_fingerprint == request.kv_layout_fingerprint
    && receipt.authority_root_hash == request.authority_root_hash
    && receipt.reconciled_through_publication_sequence == request.reconciled_through_publication_sequence
    && receipt.reconciled_through_mutation_id == request.reconciled_through_mutation_id
    && receipt.mutation_journal_head == request.mutation_journal_head
    && receipt.applied_records == request.applied_records
    && receipt.hard_publication_sequence > request.reconciled_through_publication_sequence
}
