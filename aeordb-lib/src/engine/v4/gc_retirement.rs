//! Bounded retirement-journal buffering and immutable segment publication.
//!
//! This owner deliberately does not publish a mutable journal-head control.
//! Durable retirement segments are acceleration evidence discovered from the
//! selected physical-inventory checkpoint and its monotonic write-sequence
//! interval. Only a reconciled PhysicalInventoryManifest may persistently
//! advance `retirement_journal_through_sequence`.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

use thiserror::Error as ThisError;
use tokio_util::sync::CancellationToken;

use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, compare_physical_incarnations_v1,
  decode_physical_incarnation, encode_immutable_gc_artifact,
};
use super::gc_state::{RetirementJournalModelSummaryV1, RetirementReasonV1, decode_retirement_journal_segment_v1};
use super::reader::{FormatError, MalformedInputClass};
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

pub const RETIREMENT_JOURNAL_TARGET_SEGMENT_BYTES_V1: usize = 1024 * 1024;
pub const RETIREMENT_JOURNAL_MAX_SEGMENT_BYTES_V1: usize = 16 * 1024 * 1024;
pub const RETIREMENT_JOURNAL_DEFAULT_FLUSH_RECORDS_V1: u32 = 4_096;
pub const RETIREMENT_JOURNAL_DEFAULT_FLUSH_AFTER_MS_V1: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetirementJournalBufferOptionsV1 {
  pub flush_after_records: u32,
  pub target_segment_bytes: usize,
  pub flush_after_ms: u64,
}

impl RetirementJournalBufferOptionsV1 {
  pub const fn new(flush_after_records: u32, target_segment_bytes: usize, flush_after_ms: u64) -> Self {
    Self { flush_after_records, target_segment_bytes, flush_after_ms }
  }
}

impl Default for RetirementJournalBufferOptionsV1 {
  fn default() -> Self {
    Self::new(
      RETIREMENT_JOURNAL_DEFAULT_FLUSH_RECORDS_V1,
      RETIREMENT_JOURNAL_TARGET_SEGMENT_BYTES_V1,
      RETIREMENT_JOURNAL_DEFAULT_FLUSH_AFTER_MS_V1,
    )
  }
}

#[derive(Debug, Clone, Copy)]
pub struct RetirementJournalRecordWriteV1<'a> {
  pub reason: RetirementReasonV1,
  pub replacement_publication_sequence: u64,
  pub retired_at_ms: u64,
  pub old_incarnation: &'a [u8],
  pub replacement_incarnation: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct PreparedRetirementJournalSegmentV1<'a> {
  pub segment_ordinal: u64,
  pub generation: u64,
  pub first_replacement_sequence: u64,
  pub last_replacement_sequence: u64,
  pub record_count: u32,
  pub artifact_key: &'a [u8],
  pub value: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementJournalDurabilityReceiptV1 {
  pub artifact_key: Vec<u8>,
  pub stored_value_length: u32,
  pub hard_publication_sequence: u64,
}

pub struct RetirementJournalSinkErrorV1 {
  code: &'static str,
  source: Box<dyn Error + Send + Sync>,
}

impl RetirementJournalSinkErrorV1 {
  pub fn new(error_code: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
    Self { code: error_code, source: Box::new(source) }
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }
}

impl fmt::Debug for RetirementJournalSinkErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("RetirementJournalSinkErrorV1").field("code", &self.code).field("source", &self.source.to_string()).finish()
  }
}

impl Display for RetirementJournalSinkErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.source)
  }
}

impl Error for RetirementJournalSinkErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    Some(self.source.as_ref())
  }
}

/// A successful return certifies that the exact immutable artifact has crossed
/// the shared hard-durability frontier. Implementations must make retrying the
/// same key/value idempotent after an uncertain completion.
pub trait RetirementJournalDurableSinkV1 {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementJournalOwnerStatusV1 {
  pub pending_records: u32,
  pub pending_segment_bytes: usize,
  pub durable_segments: u64,
  pub durable_records: u64,
  pub durable_through_replacement_sequence: u64,
  pub last_segment_ordinal: u64,
  pub last_segment_hash: Vec<u8>,
  pub last_hard_publication_sequence: u64,
  pub failed: bool,
}

#[derive(Debug, ThisError)]
pub enum RetirementJournalOwnerErrorV1 {
  #[error("retirement journal options are invalid: {0}")]
  InvalidOptions(&'static str),
  #[error("retirement journal operation was canceled")]
  Canceled,
  #[error("retirement journal monotonic time regressed")]
  ClockRegression,
  #[error("retirement journal records are not in canonical order")]
  RecordOrder,
  #[error("retirement journal counters or sizes overflowed")]
  ArithmeticOverflow,
  #[error("retirement journal durability receipt does not bind the published segment")]
  ReceiptMismatch { incoming_record_retained: bool },
  #[error("retirement journal durable sink failed: {source}")]
  Sink {
    #[source]
    source: RetirementJournalSinkErrorV1,
    incoming_record_retained: bool,
  },
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error(transparent)]
  Memory(#[from] MemoryCoordinatorError),
  #[error("retirement journal owner has latched a terminal failure")]
  Failed,
}

impl RetirementJournalOwnerErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidOptions(_) => "retirement_journal_options",
      Self::Canceled => "retirement_journal_cancelled",
      Self::ClockRegression => "retirement_journal_clock_regression",
      Self::RecordOrder => "retirement_journal_record_order",
      Self::ArithmeticOverflow => "retirement_journal_arithmetic",
      Self::ReceiptMismatch { .. } => "retirement_journal_receipt",
      Self::Sink { .. } => "retirement_journal_sink",
      Self::Format(error) => error.code(),
      Self::Memory(_) => "retirement_journal_memory",
      Self::Failed => "retirement_journal_owner_failed",
    }
  }

  pub const fn incoming_record_retained(&self) -> bool {
    match self {
      Self::ReceiptMismatch { incoming_record_retained } | Self::Sink { incoming_record_retained, .. } => *incoming_record_retained,
      _ => false,
    }
  }
}

pub struct RetirementJournalOwnerV1<'a> {
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  next_segment_ordinal: u64,
  next_generation: u64,
  options: RetirementJournalBufferOptionsV1,
  cancellation: &'a CancellationToken,
  _memory: MemoryReservation,
  records: Vec<u8>,
  pending_records: u32,
  pending_first_sequence: u64,
  pending_last_sequence: u64,
  pending_started_at_ms: Option<u64>,
  last_observed_at_ms: Option<u64>,
  last_record_sequence: u64,
  last_old_incarnation: Vec<u8>,
  previous_segment_hash: Vec<u8>,
  durable_segments: u64,
  durable_records: u64,
  durable_through_replacement_sequence: u64,
  last_durable_segment_ordinal: u64,
  last_hard_publication_sequence: u64,
  failed: bool,
}

struct RetirementJournalChainStateV1 {
  next_segment_ordinal: u64,
  next_generation: u64,
  previous_segment_hash: Vec<u8>,
  last_record_sequence: u64,
  last_old_incarnation: Vec<u8>,
  durable_segments: u64,
  durable_records: u64,
  last_durable_segment_ordinal: u64,
}

impl<'a> RetirementJournalOwnerV1<'a> {
  pub fn new_chain(
    algorithm: HashAlgorithm,
    database_id: [u8; 16],
    first_segment_ordinal: u64,
    first_generation: u64,
    options: RetirementJournalBufferOptionsV1,
    cancellation: &'a CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, RetirementJournalOwnerErrorV1> {
    if database_id.iter().all(|byte| *byte == 0)
      || first_segment_ordinal == 0
      || first_segment_ordinal == u64::MAX
      || first_generation == 0
      || first_generation == u64::MAX
    {
      return Err(Self::invalid_options("database, segment ordinal, and generation must have usable nonzero successors"));
    }
    Self::build(
      algorithm,
      database_id,
      RetirementJournalChainStateV1 {
        next_segment_ordinal: first_segment_ordinal,
        next_generation: first_generation,
        previous_segment_hash: Vec::new(),
        last_record_sequence: 0,
        last_old_incarnation: Vec::new(),
        durable_segments: 0,
        durable_records: 0,
        last_durable_segment_ordinal: 0,
      },
      options,
      cancellation,
      memory,
    )
  }

  pub fn resume_chain(
    algorithm: HashAlgorithm,
    database_id: [u8; 16],
    summary: &RetirementJournalModelSummaryV1,
    options: RetirementJournalBufferOptionsV1,
    cancellation: &'a CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, RetirementJournalOwnerErrorV1> {
    if database_id.iter().all(|byte| *byte == 0)
      || database_id != summary.database_id
      || summary.segment_count == 0
      || summary.record_count == 0
      || summary.first_replacement_sequence == 0
      || summary.first_replacement_sequence > summary.last_replacement_sequence
      || summary.last_segment_ordinal == 0
      || summary.last_segment_generation == 0
      || summary.segment_count == u64::MAX
      || summary.record_count == u64::MAX
      || summary.last_segment_hash.len() != algorithm.hash_length()
      || summary.last_segment_hash.iter().all(|byte| *byte == 0)
    {
      return Err(Self::invalid_options("resume summary is incomplete or belongs to an invalid chain"));
    }
    decode_physical_incarnation(&summary.last_old_incarnation, algorithm)?;
    let next_segment_ordinal = summary.last_segment_ordinal.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    let next_generation = summary.last_segment_generation.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    Self::build(
      algorithm,
      database_id,
      RetirementJournalChainStateV1 {
        next_segment_ordinal,
        next_generation,
        previous_segment_hash: summary.last_segment_hash.clone(),
        last_record_sequence: summary.last_replacement_sequence,
        last_old_incarnation: summary.last_old_incarnation.clone(),
        durable_segments: summary.segment_count,
        durable_records: summary.record_count,
        last_durable_segment_ordinal: summary.last_segment_ordinal,
      },
      options,
      cancellation,
      memory,
    )
  }

  fn build(
    algorithm: HashAlgorithm,
    database_id: [u8; 16],
    state: RetirementJournalChainStateV1,
    options: RetirementJournalBufferOptionsV1,
    cancellation: &'a CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, RetirementJournalOwnerErrorV1> {
    validate_options(algorithm, options)?;
    let required_memory = required_memory_bytes(options)?;
    let reservation = memory.reserve(MemoryOwner::GarbageCollection, required_memory, AdmissionClass::Maintenance)?;
    let record_capacity = options.target_segment_bytes.saturating_sub(complete_segment_fixed_length(algorithm));
    Ok(Self {
      algorithm,
      database_id,
      next_segment_ordinal: state.next_segment_ordinal,
      next_generation: state.next_generation,
      options,
      cancellation,
      _memory: reservation,
      records: Vec::with_capacity(record_capacity),
      pending_records: 0,
      pending_first_sequence: 0,
      pending_last_sequence: 0,
      pending_started_at_ms: None,
      last_observed_at_ms: None,
      last_record_sequence: state.last_record_sequence,
      last_old_incarnation: state.last_old_incarnation,
      previous_segment_hash: state.previous_segment_hash,
      durable_segments: state.durable_segments,
      durable_records: state.durable_records,
      durable_through_replacement_sequence: state.last_record_sequence,
      last_durable_segment_ordinal: state.last_durable_segment_ordinal,
      last_hard_publication_sequence: 0,
      failed: false,
    })
  }

  pub fn append(
    &mut self,
    record: RetirementJournalRecordWriteV1<'_>,
    monotonic_now_ms: u64,
    sink: &mut dyn RetirementJournalDurableSinkV1,
  ) -> Result<(), RetirementJournalOwnerErrorV1> {
    self.preflight(monotonic_now_ms)?;
    let encoded = encode_record(record, self.algorithm)?;
    self.validate_order(&encoded)?;

    let prospective_length =
      self.current_segment_length().checked_add(encoded.len()).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    let time_due =
      self.pending_started_at_ms.is_some_and(|started| monotonic_now_ms.saturating_sub(started) >= self.options.flush_after_ms);
    if self.pending_records > 0
      && (self.pending_records >= self.options.flush_after_records || prospective_length > self.options.target_segment_bytes || time_due)
    {
      self.flush_pending(sink, false)?;
    }

    if self.pending_records == 0 {
      self.pending_first_sequence = record.replacement_publication_sequence;
      self.pending_started_at_ms = Some(monotonic_now_ms);
    }
    self.records.extend_from_slice(&encoded);
    self.pending_records = self.pending_records.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    self.pending_last_sequence = record.replacement_publication_sequence;
    self.last_record_sequence = record.replacement_publication_sequence;
    self.last_old_incarnation.clear();
    self.last_old_incarnation.extend_from_slice(record.old_incarnation);

    if self.pending_records >= self.options.flush_after_records || self.current_segment_length() >= self.options.target_segment_bytes {
      self.flush_pending(sink, true)?;
    }
    Ok(())
  }

  pub fn poll(
    &mut self,
    monotonic_now_ms: u64,
    sink: &mut dyn RetirementJournalDurableSinkV1,
  ) -> Result<bool, RetirementJournalOwnerErrorV1> {
    self.preflight(monotonic_now_ms)?;
    let Some(started) = self.pending_started_at_ms else {
      return Ok(false);
    };
    if monotonic_now_ms.saturating_sub(started) < self.options.flush_after_ms {
      return Ok(false);
    }
    self.flush_pending(sink, false)
  }

  pub fn flush(&mut self, sink: &mut dyn RetirementJournalDurableSinkV1) -> Result<bool, RetirementJournalOwnerErrorV1> {
    self.ensure_operable()?;
    self.flush_pending(sink, false)
  }

  pub fn status(&self) -> RetirementJournalOwnerStatusV1 {
    let pending_segment_bytes = if self.pending_records != 0 { self.current_segment_length() } else { 0 };
    RetirementJournalOwnerStatusV1 {
      pending_records: self.pending_records,
      pending_segment_bytes,
      durable_segments: self.durable_segments,
      durable_records: self.durable_records,
      durable_through_replacement_sequence: self.durable_through_replacement_sequence,
      last_segment_ordinal: self.last_durable_segment_ordinal,
      last_segment_hash: self.previous_segment_hash.clone(),
      last_hard_publication_sequence: self.last_hard_publication_sequence,
      failed: self.failed,
    }
  }

  fn preflight(&mut self, monotonic_now_ms: u64) -> Result<(), RetirementJournalOwnerErrorV1> {
    self.ensure_operable()?;
    if self.last_observed_at_ms.is_some_and(|previous| monotonic_now_ms < previous) {
      return Err(RetirementJournalOwnerErrorV1::ClockRegression);
    }
    self.last_observed_at_ms = Some(monotonic_now_ms);
    Ok(())
  }

  fn ensure_operable(&self) -> Result<(), RetirementJournalOwnerErrorV1> {
    if self.failed {
      return Err(RetirementJournalOwnerErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(RetirementJournalOwnerErrorV1::Canceled);
    }
    self._memory.check_admission()?;
    Ok(())
  }

  fn validate_order(&self, encoded: &[u8]) -> Result<(), RetirementJournalOwnerErrorV1> {
    if self.last_record_sequence == 0 {
      return Ok(());
    }
    let sequence = u64::from_le_bytes(encoded[8..16].try_into().map_err(|_| RetirementJournalOwnerErrorV1::ArithmeticOverflow)?);
    if sequence < self.last_record_sequence {
      return Err(RetirementJournalOwnerErrorV1::RecordOrder);
    }
    if sequence == self.last_record_sequence {
      let physical_length = 24 + 2 * self.algorithm.hash_length();
      let old = decode_physical_incarnation(&encoded[24..24 + physical_length], self.algorithm)?;
      let previous = decode_physical_incarnation(&self.last_old_incarnation, self.algorithm)?;
      if compare_physical_incarnations_v1(&previous, &old) != Ordering::Less {
        return Err(RetirementJournalOwnerErrorV1::RecordOrder);
      }
    }
    Ok(())
  }

  fn current_segment_length(&self) -> usize {
    complete_segment_fixed_length(self.algorithm).saturating_add(self.records.len())
  }

  fn flush_pending(
    &mut self,
    sink: &mut dyn RetirementJournalDurableSinkV1,
    incoming_record_retained: bool,
  ) -> Result<bool, RetirementJournalOwnerErrorV1> {
    if self.pending_records == 0 {
      return Ok(false);
    }
    let next_durable_segments = self.durable_segments.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    let next_durable_records =
      self.durable_records.checked_add(u64::from(self.pending_records)).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    let next_segment_ordinal = self.next_segment_ordinal.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    let next_generation = self.next_generation.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    let encoded = self.encode_pending_segment()?;
    let prepared = PreparedRetirementJournalSegmentV1 {
      segment_ordinal: self.next_segment_ordinal,
      generation: self.next_generation,
      first_replacement_sequence: self.pending_first_sequence,
      last_replacement_sequence: self.pending_last_sequence,
      record_count: self.pending_records,
      artifact_key: &encoded.key,
      value: &encoded.value,
    };
    let receipt =
      sink.publish_synced(&prepared).map_err(|source| RetirementJournalOwnerErrorV1::Sink { source, incoming_record_retained })?;
    let stored_value_length = u32::try_from(encoded.value.len()).map_err(|_| RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    if receipt.artifact_key != encoded.key || receipt.stored_value_length != stored_value_length || receipt.hard_publication_sequence == 0 {
      self.failed = true;
      return Err(RetirementJournalOwnerErrorV1::ReceiptMismatch { incoming_record_retained });
    }

    self.durable_segments = next_durable_segments;
    self.durable_records = next_durable_records;
    self.durable_through_replacement_sequence = self.pending_last_sequence;
    self.last_durable_segment_ordinal = self.next_segment_ordinal;
    self.last_hard_publication_sequence = receipt.hard_publication_sequence;
    self.previous_segment_hash.clear();
    self.previous_segment_hash.extend_from_slice(&encoded.key);
    self.next_segment_ordinal = next_segment_ordinal;
    self.next_generation = next_generation;
    self.records.clear();
    self.pending_records = 0;
    self.pending_first_sequence = 0;
    self.pending_last_sequence = 0;
    self.pending_started_at_ms = None;
    Ok(true)
  }

  fn encode_pending_segment(&self) -> Result<EncodedImmutableGcArtifactV1, RetirementJournalOwnerErrorV1> {
    let hash_width = self.algorithm.hash_length();
    let body_length = 32usize
      .checked_add(hash_width)
      .and_then(|length| length.checked_add(self.records.len()))
      .ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    let mut body = Vec::with_capacity(body_length);
    body.extend_from_slice(&u32::from(self.previous_segment_hash.is_empty()).to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&self.pending_first_sequence.to_le_bytes());
    body.extend_from_slice(&self.pending_last_sequence.to_le_bytes());
    body.extend_from_slice(&self.pending_records.to_le_bytes());
    body
      .extend_from_slice(&u32::try_from(self.records.len()).map_err(|_| RetirementJournalOwnerErrorV1::ArithmeticOverflow)?.to_le_bytes());
    if self.previous_segment_hash.is_empty() {
      body.resize(body.len() + hash_width, 0);
    } else {
      body.extend_from_slice(&self.previous_segment_hash);
    }
    body.extend_from_slice(&self.records);

    let mut identity = Vec::with_capacity(24);
    identity.extend_from_slice(&self.database_id);
    identity.extend_from_slice(&self.next_segment_ordinal.to_le_bytes());
    let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
      kind: GcArtifactKindV1::RetirementJournalSegment,
      hash_algorithm: self.algorithm,
      generation: self.next_generation,
      identity: &identity,
      body: &body,
    })?;
    let decoded = decode_retirement_journal_segment_v1(&encoded.value, self.algorithm)?;
    if decoded.key != encoded.key
      || decoded.segment_ordinal != self.next_segment_ordinal
      || decoded.generation != self.next_generation
      || decoded.record_count != self.pending_records
      || decoded.first_replacement_sequence != self.pending_first_sequence
      || decoded.last_replacement_sequence != self.pending_last_sequence
    {
      return Err(
        FormatError::new(
          MalformedInputClass::CrossRecordClosureMismatch,
          "retirement_journal_writer_readback",
          "encoded retirement segment did not decode to its prepared identity",
        )
        .into(),
      );
    }
    Ok(encoded)
  }

  fn invalid_options(message: &'static str) -> RetirementJournalOwnerErrorV1 {
    RetirementJournalOwnerErrorV1::InvalidOptions(message)
  }
}

fn validate_options(algorithm: HashAlgorithm, options: RetirementJournalBufferOptionsV1) -> Result<(), RetirementJournalOwnerErrorV1> {
  let minimum = complete_segment_fixed_length(algorithm)
    .checked_add(retirement_record_length(algorithm))
    .ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
  if options.flush_after_records == 0 {
    return Err(RetirementJournalOwnerErrorV1::InvalidOptions("flush record count must be nonzero"));
  }
  if options.target_segment_bytes < minimum || options.target_segment_bytes > RETIREMENT_JOURNAL_MAX_SEGMENT_BYTES_V1 {
    return Err(RetirementJournalOwnerErrorV1::InvalidOptions("target bytes must hold one record and stay within the 16 MiB cap"));
  }
  if options.flush_after_ms == 0 {
    return Err(RetirementJournalOwnerErrorV1::InvalidOptions("flush interval must be nonzero"));
  }
  Ok(())
}

fn required_memory_bytes(options: RetirementJournalBufferOptionsV1) -> Result<u64, RetirementJournalOwnerErrorV1> {
  let bytes = options
    .target_segment_bytes
    .checked_mul(3)
    .and_then(|bytes| bytes.checked_add(64 * 1024))
    .ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
  u64::try_from(bytes).map_err(|_| RetirementJournalOwnerErrorV1::ArithmeticOverflow)
}

fn complete_segment_fixed_length(algorithm: HashAlgorithm) -> usize {
  92 + algorithm.hash_length()
}

fn retirement_record_length(algorithm: HashAlgorithm) -> usize {
  72 + 4 * algorithm.hash_length()
}

fn encode_record(record: RetirementJournalRecordWriteV1<'_>, algorithm: HashAlgorithm) -> Result<Vec<u8>, RetirementJournalOwnerErrorV1> {
  let old = decode_physical_incarnation(record.old_incarnation, algorithm)?;
  let replacement = decode_physical_incarnation(record.replacement_incarnation, algorithm)?;
  if record.replacement_publication_sequence == 0 || record.retired_at_ms == 0 || old == replacement {
    return Err(
      FormatError::new(
        MalformedInputClass::CrossRecordClosureMismatch,
        "retirement_record_fields",
        "retirement record sequence, time, or incarnation pair is invalid",
      )
      .into(),
    );
  }
  let record_length = retirement_record_length(algorithm);
  let mut encoded = Vec::with_capacity(record_length);
  encoded.extend_from_slice(&u32::try_from(record_length).map_err(|_| RetirementJournalOwnerErrorV1::ArithmeticOverflow)?.to_le_bytes());
  encoded.extend_from_slice(&(record.reason as u16).to_le_bytes());
  encoded.extend_from_slice(&0u16.to_le_bytes());
  encoded.extend_from_slice(&record.replacement_publication_sequence.to_le_bytes());
  encoded.extend_from_slice(&record.retired_at_ms.to_le_bytes());
  encoded.extend_from_slice(record.old_incarnation);
  encoded.extend_from_slice(record.replacement_incarnation);
  Ok(encoded)
}
