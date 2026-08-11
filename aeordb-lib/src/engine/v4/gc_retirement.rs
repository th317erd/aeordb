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
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use thiserror::Error as ThisError;
use tokio_util::sync::CancellationToken;

use super::entity::EntryTypeV4;
use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, PhysicalIncarnationV1, compare_physical_incarnations_v1,
  decode_physical_incarnation, encode_immutable_gc_artifact,
};
use super::gc_state::{
  PhysicalInventoryManifestV1, RetirementJournalModelErrorV1, RetirementJournalModelSummaryV1, RetirementJournalReferenceModelV1,
  RetirementJournalSegmentV1, RetirementReasonV1, decode_retirement_journal_segment_v1,
};
use super::hash::digest_parts;
use super::reader::{FormatError, MalformedInputClass};
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

pub const RETIREMENT_JOURNAL_TARGET_SEGMENT_BYTES_V1: usize = 1024 * 1024;
pub const RETIREMENT_JOURNAL_MAX_SEGMENT_BYTES_V1: usize = 16 * 1024 * 1024;
pub const RETIREMENT_JOURNAL_DEFAULT_FLUSH_RECORDS_V1: u32 = 4_096;
pub const RETIREMENT_JOURNAL_DEFAULT_FLUSH_AFTER_MS_V1: u64 = 30_000;

static NEXT_RETIREMENT_JOURNAL_OWNER_INSTANCE_ID_V1: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy)]
pub enum PhysicalInventoryRetirementClassificationV1<'a> {
  NonGcArtifact,
  NoncurrentGcArtifact,
  CurrentOtherGcArtifact(GcArtifactKindV1),
  CurrentRetirementSegment(&'a RetirementJournalSegmentV1<'a>),
}

#[derive(Debug, Clone, Copy)]
pub struct PhysicalInventoryRetirementObservationV1<'a> {
  pub incarnation: PhysicalIncarnationV1<'a>,
  pub classification: PhysicalInventoryRetirementClassificationV1<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalInventoryAuditBoundaryV1 {
  pub database_id: [u8; 16],
  pub scan_start_wal_offset: u64,
  pub audited_wal_offset: u64,
  pub audited_write_sequence: u64,
  pub maximum_physical_entities: u64,
  pub maximum_retirement_records: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementJournalCheckpointHandoffV1 {
  database_id: [u8; 16],
  scan_start_wal_offset: u64,
  audited_wal_offset: u64,
  audited_write_sequence: u64,
  retirement_journal_through_sequence: u64,
  physical_entity_count: u64,
  journal: Option<RetirementJournalModelSummaryV1>,
}

impl RetirementJournalCheckpointHandoffV1 {
  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn scan_start_wal_offset(&self) -> u64 {
    self.scan_start_wal_offset
  }

  pub const fn audited_wal_offset(&self) -> u64 {
    self.audited_wal_offset
  }

  pub const fn audited_write_sequence(&self) -> u64 {
    self.audited_write_sequence
  }

  pub const fn retirement_journal_through_sequence(&self) -> u64 {
    self.retirement_journal_through_sequence
  }

  pub const fn physical_entity_count(&self) -> u64 {
    self.physical_entity_count
  }

  pub const fn journal(&self) -> Option<&RetirementJournalModelSummaryV1> {
    self.journal.as_ref()
  }

  /// Bind the reconciled scan to the exact fields a future inventory writer
  /// may place in its immutable manifest. Directory/page closure remains the
  /// responsibility of `PhysicalInventoryReferenceModelV1`.
  pub fn validate_candidate_manifest(&self, candidate: &PhysicalInventoryManifestV1<'_>) -> Result<(), RetirementJournalCheckpointErrorV1> {
    if candidate.database_id != self.database_id
      || candidate.audited_wal_offset != self.audited_wal_offset
      || candidate.audited_write_sequence != self.audited_write_sequence
      || candidate.retirement_journal_through_sequence != self.retirement_journal_through_sequence
      || candidate.record_count() != self.physical_entity_count
    {
      return Err(RetirementJournalCheckpointErrorV1::CandidateManifest);
    }
    Ok(())
  }
}

#[derive(Debug, ThisError)]
pub enum RetirementJournalCheckpointErrorV1 {
  #[error("retirement-journal checkpoint boundary is invalid: {0}")]
  InvalidBoundary(&'static str),
  #[error("prior physical-inventory manifest does not match this database or hash profile")]
  PriorManifest,
  #[error("retirement-journal checkpoint regresses prior selected inventory authority")]
  PriorRegression,
  #[error("retirement-journal checkpoint reconciliation was canceled")]
  Canceled,
  #[error("retirement-journal checkpoint exceeds its admitted physical-entity limit")]
  EntityLimit,
  #[error("retirement-journal checkpoint physical extents are not contiguous and ordered")]
  PhysicalGap,
  #[error("retirement-journal checkpoint physical incarnation is malformed")]
  PhysicalIncarnation,
  #[error("retirement-journal checkpoint classification disagrees with the physical incarnation")]
  Classification,
  #[error("retirement-journal checkpoint did not close at its exact WAL/write-sequence boundary")]
  AuditBoundary,
  #[error("prior selected retirement-journal watermark was not rediscovered as current authority")]
  PriorWatermarkMissing,
  #[error("retirement-journal checkpoint counters or extents overflowed")]
  ArithmeticOverflow,
  #[error(transparent)]
  Journal(#[from] RetirementJournalModelErrorV1),
  #[error("candidate physical-inventory manifest disagrees with the reconciled journal handoff")]
  CandidateManifest,
  #[error("retirement-journal checkpoint reconciler has already failed")]
  Failed,
}

impl RetirementJournalCheckpointErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidBoundary(_) => "retirement_checkpoint_boundary",
      Self::PriorManifest => "retirement_checkpoint_prior_manifest",
      Self::PriorRegression => "retirement_checkpoint_prior_regression",
      Self::Canceled => "retirement_checkpoint_cancelled",
      Self::EntityLimit => "retirement_checkpoint_entity_limit",
      Self::PhysicalGap => "retirement_checkpoint_physical_gap",
      Self::PhysicalIncarnation => "retirement_checkpoint_physical_incarnation",
      Self::Classification => "retirement_checkpoint_classification",
      Self::AuditBoundary => "retirement_checkpoint_audit_boundary",
      Self::PriorWatermarkMissing => "retirement_checkpoint_prior_watermark_missing",
      Self::ArithmeticOverflow => "retirement_checkpoint_arithmetic",
      Self::Journal(error) => error.code(),
      Self::CandidateManifest => "retirement_checkpoint_candidate_manifest",
      Self::Failed => "retirement_checkpoint_failed",
    }
  }
}

/// Constant-memory handoff between a complete physical scan and the next
/// selected inventory manifest.
///
/// The caller must classify every physical entity in exact WAL order. Current
/// retirement segments rebuild their immutable chain from its reset; leaked or
/// superseded crash-prefix copies remain inventoried but cannot become journal
/// authority. Any gap, cancellation, corrupt chain, or missing prior watermark
/// latches failure and leaves the previously selected manifest untouched.
#[derive(Debug)]
pub struct RetirementJournalCheckpointReconcilerV1<'a> {
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  scan_start_wal_offset: u64,
  audited_wal_offset: u64,
  audited_write_sequence: u64,
  previous_retirement_journal_through_sequence: u64,
  maximum_physical_entities: u64,
  maximum_retirement_records: u64,
  cancellation: &'a CancellationToken,
  next_wal_offset: u64,
  physical_entity_count: u64,
  maximum_observed_write_sequence: u64,
  retirement_journal_through_sequence: u64,
  previous_watermark_seen: bool,
  journal: Option<RetirementJournalReferenceModelV1<'a>>,
  failed: bool,
}

impl<'a> RetirementJournalCheckpointReconcilerV1<'a> {
  pub fn new(
    algorithm: HashAlgorithm,
    boundary: PhysicalInventoryAuditBoundaryV1,
    prior_manifest: Option<&PhysicalInventoryManifestV1<'_>>,
    cancellation: &'a CancellationToken,
  ) -> Result<Self, RetirementJournalCheckpointErrorV1> {
    if boundary.database_id.iter().all(|byte| *byte == 0)
      || boundary.scan_start_wal_offset == 0
      || boundary.scan_start_wal_offset >= boundary.audited_wal_offset
      || boundary.audited_write_sequence == 0
      || boundary.maximum_physical_entities == 0
    {
      return Err(RetirementJournalCheckpointErrorV1::InvalidBoundary(
        "database, WAL interval, write sequence, and physical-entity limit must be nonzero and ordered",
      ));
    }
    let previous_retirement_journal_through_sequence = if let Some(prior) = prior_manifest {
      if prior.database_id != boundary.database_id
        || prior.kv_layout_fingerprint.len() != algorithm.hash_length()
        || prior.key.len() != algorithm.hash_length()
      {
        return Err(RetirementJournalCheckpointErrorV1::PriorManifest);
      }
      if prior.audited_wal_offset > boundary.audited_wal_offset
        || prior.audited_write_sequence > boundary.audited_write_sequence
        || prior.retirement_journal_through_sequence > boundary.audited_write_sequence
      {
        return Err(RetirementJournalCheckpointErrorV1::PriorRegression);
      }
      prior.retirement_journal_through_sequence
    } else {
      0
    };
    Ok(Self {
      algorithm,
      database_id: boundary.database_id,
      scan_start_wal_offset: boundary.scan_start_wal_offset,
      audited_wal_offset: boundary.audited_wal_offset,
      audited_write_sequence: boundary.audited_write_sequence,
      previous_retirement_journal_through_sequence,
      maximum_physical_entities: boundary.maximum_physical_entities,
      maximum_retirement_records: boundary.maximum_retirement_records,
      cancellation,
      next_wal_offset: boundary.scan_start_wal_offset,
      physical_entity_count: 0,
      maximum_observed_write_sequence: 0,
      retirement_journal_through_sequence: 0,
      previous_watermark_seen: previous_retirement_journal_through_sequence == 0,
      journal: None,
      failed: false,
    })
  }

  pub fn observe(&mut self, observation: PhysicalInventoryRetirementObservationV1<'_>) -> Result<(), RetirementJournalCheckpointErrorV1> {
    if self.failed {
      return Err(RetirementJournalCheckpointErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return self.fail(RetirementJournalCheckpointErrorV1::Canceled);
    }
    if self.physical_entity_count >= self.maximum_physical_entities {
      return self.fail(RetirementJournalCheckpointErrorV1::EntityLimit);
    }
    if !valid_inventory_incarnation_shape(self.algorithm, &observation.incarnation) {
      return self.fail(RetirementJournalCheckpointErrorV1::PhysicalIncarnation);
    }
    if observation.incarnation.wal_offset != self.next_wal_offset {
      return self.fail(RetirementJournalCheckpointErrorV1::PhysicalGap);
    }
    let Some(extent_end) = observation.incarnation.wal_offset.checked_add(u64::from(observation.incarnation.entity_length)) else {
      return self.fail(RetirementJournalCheckpointErrorV1::ArithmeticOverflow);
    };
    if extent_end > self.audited_wal_offset || observation.incarnation.write_sequence > self.audited_write_sequence {
      return self.fail(RetirementJournalCheckpointErrorV1::AuditBoundary);
    }
    if !classification_matches_incarnation(&observation) {
      return self.fail(RetirementJournalCheckpointErrorV1::Classification);
    }

    if let PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(segment) = observation.classification {
      if segment.database_id != self.database_id || segment.key != observation.incarnation.logical_key {
        return self.fail(RetirementJournalCheckpointErrorV1::Classification);
      }
      let journal = self
        .journal
        .get_or_insert_with(|| RetirementJournalReferenceModelV1::new(self.algorithm, self.cancellation, self.maximum_retirement_records));
      if let Err(error) = journal.observe_segment(segment) {
        return self.fail(error.into());
      }
      if observation.incarnation.write_sequence <= self.retirement_journal_through_sequence {
        return self.fail(RetirementJournalCheckpointErrorV1::AuditBoundary);
      }
      self.retirement_journal_through_sequence = observation.incarnation.write_sequence;
      if observation.incarnation.write_sequence == self.previous_retirement_journal_through_sequence {
        self.previous_watermark_seen = true;
      }
    }

    self.next_wal_offset = extent_end;
    self.physical_entity_count = self.physical_entity_count.checked_add(1).ok_or(RetirementJournalCheckpointErrorV1::ArithmeticOverflow)?;
    self.maximum_observed_write_sequence = self.maximum_observed_write_sequence.max(observation.incarnation.write_sequence);
    Ok(())
  }

  pub fn finish(mut self) -> Result<RetirementJournalCheckpointHandoffV1, RetirementJournalCheckpointErrorV1> {
    if self.failed {
      return Err(RetirementJournalCheckpointErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return self.fail(RetirementJournalCheckpointErrorV1::Canceled);
    }
    if self.next_wal_offset != self.audited_wal_offset || self.maximum_observed_write_sequence != self.audited_write_sequence {
      return self.fail(RetirementJournalCheckpointErrorV1::AuditBoundary);
    }
    if !self.previous_watermark_seen || self.retirement_journal_through_sequence < self.previous_retirement_journal_through_sequence {
      return self.fail(RetirementJournalCheckpointErrorV1::PriorWatermarkMissing);
    }
    let journal = match self.journal.take() {
      Some(journal) => {
        let summary = journal.finish()?;
        if summary.database_id != self.database_id {
          return Err(RetirementJournalCheckpointErrorV1::PriorManifest);
        }
        Some(summary)
      }
      None => None,
    };
    Ok(RetirementJournalCheckpointHandoffV1 {
      database_id: self.database_id,
      scan_start_wal_offset: self.scan_start_wal_offset,
      audited_wal_offset: self.audited_wal_offset,
      audited_write_sequence: self.audited_write_sequence,
      retirement_journal_through_sequence: self.retirement_journal_through_sequence,
      physical_entity_count: self.physical_entity_count,
      journal,
    })
  }

  fn fail<T>(&mut self, error: RetirementJournalCheckpointErrorV1) -> Result<T, RetirementJournalCheckpointErrorV1> {
    self.failed = true;
    Err(error)
  }
}

fn valid_inventory_incarnation_shape(algorithm: HashAlgorithm, incarnation: &PhysicalIncarnationV1<'_>) -> bool {
  incarnation.logical_key.len() == algorithm.hash_length()
    && incarnation.integrity_or_legacy_digest.len() == algorithm.hash_length()
    && incarnation.logical_key.iter().any(|byte| *byte != 0)
    && incarnation.integrity_or_legacy_digest.iter().any(|byte| *byte != 0)
    && incarnation.wal_offset != 0
    && incarnation.entity_length != 0
    && (1..=EntryTypeV4::GcArtifact.to_u8()).contains(&incarnation.entry_type)
    && (incarnation.entity_version == 0) == (incarnation.write_sequence == 0)
}

fn classification_matches_incarnation(observation: &PhysicalInventoryRetirementObservationV1<'_>) -> bool {
  let is_gc_artifact = observation.incarnation.entry_type == EntryTypeV4::GcArtifact.to_u8();
  match observation.classification {
    PhysicalInventoryRetirementClassificationV1::NonGcArtifact => !is_gc_artifact,
    PhysicalInventoryRetirementClassificationV1::NoncurrentGcArtifact => is_gc_artifact,
    PhysicalInventoryRetirementClassificationV1::CurrentOtherGcArtifact(kind) => {
      is_gc_artifact && kind != GcArtifactKindV1::RetirementJournalSegment
    }
    PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(_) => {
      is_gc_artifact && observation.incarnation.entity_version == 1 && observation.incarnation.write_sequence != 0
    }
  }
}

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
  #[error("buffered retirement rollback belongs to another owner")]
  BufferedRollbackOwner,
  #[error("buffered retirement rollback no longer matches the owner's exact soft state")]
  BufferedRollbackState,
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
      Self::BufferedRollbackOwner => "retirement_journal_buffered_rollback_owner",
      Self::BufferedRollbackState => "retirement_journal_buffered_rollback_state",
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

#[derive(Debug, Clone, Copy)]
pub struct RetirementJournalReplacementV1<'a> {
  pub reason: RetirementReasonV1,
  pub old_incarnation: &'a [u8],
  pub replacement_incarnation: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct RetirementJournalReplacementBatchV1<'a> {
  pub replacement_publication_sequence: u64,
  pub retired_at_ms: u64,
  pub replacements: &'a [RetirementJournalReplacementV1<'a>],
}

#[must_use = "replacement activation journal state may contain a deferred durability failure"]
#[derive(Debug)]
pub enum RetirementJournalActivationJournalStateV1 {
  Buffered,
  HardPublished { hard_publication_sequence: u64 },
  BufferedAfterSinkFailure { source: RetirementJournalSinkErrorV1 },
}

impl RetirementJournalActivationJournalStateV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::Buffered => "buffered",
      Self::HardPublished { .. } => "hard_published",
      Self::BufferedAfterSinkFailure { .. } => "buffered_after_sink_failure",
    }
  }

  pub const fn deferred_sink_error(&self) -> Option<&RetirementJournalSinkErrorV1> {
    match self {
      Self::BufferedAfterSinkFailure { source } => Some(source),
      Self::Buffered | Self::HardPublished { .. } => None,
    }
  }
}

#[must_use = "replacement activation requires this admitted retirement permit"]
#[derive(Debug)]
pub struct RetirementJournalActivationPermitV1 {
  hash_algorithm: HashAlgorithm,
  replacement_publication_sequence: u64,
  retired_at_ms: u64,
  replacement_count: u32,
  reason_counts: [u32; 5],
  batch_digest: Vec<u8>,
}

impl RetirementJournalActivationPermitV1 {
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn replacement_publication_sequence(&self) -> u64 {
    self.replacement_publication_sequence
  }

  pub const fn retired_at_ms(&self) -> u64 {
    self.retired_at_ms
  }

  pub const fn replacement_count(&self) -> u32 {
    self.replacement_count
  }

  pub const fn reason_count(&self, reason: RetirementReasonV1) -> u32 {
    self.reason_counts[retirement_reason_index(reason)]
  }

  pub fn batch_digest(&self) -> &[u8] {
    &self.batch_digest
  }
}

#[must_use = "prepared retirement admission must be activated or retained for retry"]
#[derive(Debug)]
pub struct PreparedRetirementJournalReplacementV1 {
  permit: RetirementJournalActivationPermitV1,
  journal_state: RetirementJournalActivationJournalStateV1,
  buffered_rollback: Option<Box<RetirementJournalBufferedRollbackV1>>,
}

impl PreparedRetirementJournalReplacementV1 {
  pub fn permit(&self) -> &RetirementJournalActivationPermitV1 {
    &self.permit
  }

  pub fn journal_state(&self) -> &RetirementJournalActivationJournalStateV1 {
    &self.journal_state
  }

  pub fn activate<T, E, F>(self, activate: F) -> Result<RetirementJournalReplacementOutcomeV1<T>, RetirementJournalReplacementErrorV1<E>>
  where
    F: FnOnce(&RetirementJournalActivationPermitV1) -> Result<T, E>,
  {
    match activate(&self.permit) {
      Ok(output) => Ok(RetirementJournalReplacementOutcomeV1 { output, journal_state: self.journal_state }),
      Err(source) => Err(RetirementJournalReplacementErrorV1::Activation { source, prepared: self }),
    }
  }

  /// Remove the exact soft record admitted by `prepare_buffered_single` after
  /// a replacement is proven not to have activated.
  ///
  /// Any intervening owner mutation latches the owner instead of guessing
  /// which retirement evidence is safe to remove.
  pub fn discard_buffered(mut self, owner: &mut RetirementJournalOwnerV1<'_>) -> Result<(), RetirementJournalBufferedDiscardErrorV1> {
    let Some(rollback) = self.buffered_rollback.as_ref() else {
      return Err(RetirementJournalBufferedDiscardErrorV1::new(RetirementJournalOwnerErrorV1::BufferedRollbackState, self));
    };
    if rollback.owner_instance_id != owner.owner_instance_id
      || rollback.algorithm != owner.algorithm
      || rollback.database_id != owner.database_id
    {
      return Err(RetirementJournalBufferedDiscardErrorV1::new(RetirementJournalOwnerErrorV1::BufferedRollbackOwner, self));
    }
    if owner.soft_state() != rollback.after {
      owner.failed = true;
      return Err(RetirementJournalBufferedDiscardErrorV1::new(RetirementJournalOwnerErrorV1::BufferedRollbackState, self));
    }
    let Some(rollback) = self.buffered_rollback.take() else {
      return Err(RetirementJournalBufferedDiscardErrorV1::new(RetirementJournalOwnerErrorV1::BufferedRollbackState, self));
    };
    let rollback = *rollback;
    owner.restore_soft_state(rollback.before);
    Ok(())
  }
}

#[derive(Debug)]
pub struct RetirementJournalBufferedDiscardErrorV1 {
  source: RetirementJournalOwnerErrorV1,
  prepared: Box<PreparedRetirementJournalReplacementV1>,
}

impl RetirementJournalBufferedDiscardErrorV1 {
  fn new(source: RetirementJournalOwnerErrorV1, prepared: PreparedRetirementJournalReplacementV1) -> Self {
    Self { source, prepared: Box::new(prepared) }
  }

  pub fn code(&self) -> &'static str {
    self.source.code()
  }

  pub fn into_parts(self) -> (RetirementJournalOwnerErrorV1, Box<PreparedRetirementJournalReplacementV1>) {
    (self.source, self.prepared)
  }
}

impl Display for RetirementJournalBufferedDiscardErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "buffered retirement rollback refused: {}", self.source)
  }
}

impl Error for RetirementJournalBufferedDiscardErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    Some(&self.source)
  }
}

#[derive(Debug, PartialEq, Eq)]
struct RetirementJournalSoftStateV1 {
  records_length: usize,
  pending_records: u32,
  pending_first_sequence: u64,
  pending_last_sequence: u64,
  pending_started_at_ms: Option<u64>,
  last_observed_at_ms: Option<u64>,
  last_record_sequence: u64,
  last_old_incarnation: Vec<u8>,
}

#[derive(Debug)]
struct RetirementJournalBufferedRollbackV1 {
  owner_instance_id: u64,
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  before: RetirementJournalSoftStateV1,
  after: RetirementJournalSoftStateV1,
}

#[must_use = "replacement outcome may contain a deferred retirement-journal durability failure"]
#[derive(Debug)]
pub struct RetirementJournalReplacementOutcomeV1<T> {
  pub output: T,
  pub journal_state: RetirementJournalActivationJournalStateV1,
}

#[derive(Debug, ThisError)]
pub enum RetirementJournalReplacementAdmissionErrorV1 {
  #[error("v4 retirement replacement batch failed preflight: {0}")]
  Preflight(&'static str),
  #[error("retirement journal rejected a replacement batch after admitting {admitted_records} records: {source}")]
  Journal {
    #[source]
    source: RetirementJournalOwnerErrorV1,
    admitted_records: u32,
  },
}

impl RetirementJournalReplacementAdmissionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Preflight(_) => "retirement_replacement_preflight",
      Self::Journal { source, .. } => source.code(),
    }
  }

  pub const fn admitted_records(&self) -> u32 {
    match self {
      Self::Preflight(_) => 0,
      Self::Journal { admitted_records, .. } => *admitted_records,
    }
  }
}

#[derive(Debug)]
pub enum RetirementJournalReplacementErrorV1<E> {
  Admission(RetirementJournalReplacementAdmissionErrorV1),
  Activation { source: E, prepared: PreparedRetirementJournalReplacementV1 },
}

impl<E> RetirementJournalReplacementErrorV1<E> {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Admission(source) => source.code(),
      Self::Activation { .. } => "retirement_replacement_activation",
    }
  }

  pub const fn admitted_records(&self) -> u32 {
    match self {
      Self::Admission(source) => source.admitted_records(),
      Self::Activation { prepared, .. } => prepared.permit.replacement_count,
    }
  }

  pub fn into_activation_failure(self) -> Option<(E, PreparedRetirementJournalReplacementV1)> {
    match self {
      Self::Activation { source, prepared } => Some((source, prepared)),
      Self::Admission(_) => None,
    }
  }
}

impl<E: Display> Display for RetirementJournalReplacementErrorV1<E> {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Admission(source) => Display::fmt(source, formatter),
      Self::Activation { source, .. } => write!(formatter, "v4 replacement activation failed after retirement admission: {source}"),
    }
  }
}

impl<E: Error + 'static> Error for RetirementJournalReplacementErrorV1<E> {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Admission(source) => Some(source),
      Self::Activation { source, .. } => Some(source),
    }
  }
}

impl<E> From<RetirementJournalReplacementAdmissionErrorV1> for RetirementJournalReplacementErrorV1<E> {
  fn from(source: RetirementJournalReplacementAdmissionErrorV1) -> Self {
    Self::Admission(source)
  }
}

/// The sole future v4 stable-key replacement boundary.
///
/// The caller must reserve the hard authority publication sequence and append
/// each replacement entity before entering this coordinator. Every exact old
/// and replacement incarnation is then admitted to the bounded retirement
/// owner before the activation callback can run. The current v3 mutation
/// acknowledgement cannot construct this input because it lacks the v4
/// integrity digest and entity write sequence.
pub struct RetirementJournalReplacementCoordinatorV1<'coordinator, 'owner> {
  owner: &'coordinator mut RetirementJournalOwnerV1<'owner>,
  sink: &'coordinator mut dyn RetirementJournalDurableSinkV1,
}

impl<'coordinator, 'owner> RetirementJournalReplacementCoordinatorV1<'coordinator, 'owner> {
  pub fn new(
    owner: &'coordinator mut RetirementJournalOwnerV1<'owner>,
    sink: &'coordinator mut dyn RetirementJournalDurableSinkV1,
  ) -> Self {
    Self { owner, sink }
  }

  pub fn prepare(
    &mut self,
    batch: RetirementJournalReplacementBatchV1<'_>,
    monotonic_now_ms: u64,
  ) -> Result<PreparedRetirementJournalReplacementV1, RetirementJournalReplacementAdmissionErrorV1> {
    let (reason_counts, batch_digest) = self.preflight_batch(&batch, monotonic_now_ms)?;
    let replacement_count = u32::try_from(batch.replacements.len())
      .map_err(|_| RetirementJournalReplacementAdmissionErrorV1::Preflight("replacement count exceeds u32"))?;
    let mut admitted_records = 0u32;
    let mut deferred_sink_error = None;

    for (index, replacement) in batch.replacements.iter().enumerate() {
      let record = RetirementJournalRecordWriteV1 {
        reason: replacement.reason,
        replacement_publication_sequence: batch.replacement_publication_sequence,
        retired_at_ms: batch.retired_at_ms,
        old_incarnation: replacement.old_incarnation,
        replacement_incarnation: replacement.replacement_incarnation,
      };
      match self.owner.append(record, monotonic_now_ms, self.sink) {
        Ok(()) => {
          admitted_records = admitted_records
            .checked_add(1)
            .ok_or(RetirementJournalReplacementAdmissionErrorV1::Preflight("admitted replacement count overflowed"))?;
        }
        Err(RetirementJournalOwnerErrorV1::Sink { source, incoming_record_retained: true }) if index + 1 == batch.replacements.len() => {
          admitted_records = admitted_records
            .checked_add(1)
            .ok_or(RetirementJournalReplacementAdmissionErrorV1::Preflight("admitted replacement count overflowed"))?;
          deferred_sink_error = Some(source);
        }
        Err(source) => {
          if source.incoming_record_retained() {
            admitted_records = admitted_records
              .checked_add(1)
              .ok_or(RetirementJournalReplacementAdmissionErrorV1::Preflight("admitted replacement count overflowed"))?;
          }
          return Err(RetirementJournalReplacementAdmissionErrorV1::Journal { source, admitted_records });
        }
      }
    }

    if admitted_records != replacement_count {
      return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("not every replacement entered retirement ownership"));
    }
    let status = self.owner.status();
    let journal_state = match deferred_sink_error {
      Some(source) => RetirementJournalActivationJournalStateV1::BufferedAfterSinkFailure { source },
      None if status.pending_records == 0 => {
        RetirementJournalActivationJournalStateV1::HardPublished { hard_publication_sequence: status.last_hard_publication_sequence }
      }
      None => RetirementJournalActivationJournalStateV1::Buffered,
    };
    Ok(PreparedRetirementJournalReplacementV1 {
      permit: RetirementJournalActivationPermitV1 {
        hash_algorithm: self.owner.algorithm,
        replacement_publication_sequence: batch.replacement_publication_sequence,
        retired_at_ms: batch.retired_at_ms,
        replacement_count,
        reason_counts,
        batch_digest,
      },
      journal_state,
      buffered_rollback: None,
    })
  }

  /// Admit one already-appended replacement without recursively invoking the
  /// durable sink while its publication authority is held.
  ///
  /// The caller must activate through the returned permit and flush the owner
  /// immediately after releasing that authority. A crash before the flush can
  /// only lose soft retirement evidence; physical reconciliation then leaks or
  /// protects the affected extent instead of authorizing reclaim.
  pub fn prepare_buffered_single(
    &mut self,
    batch: RetirementJournalReplacementBatchV1<'_>,
    monotonic_now_ms: u64,
  ) -> Result<PreparedRetirementJournalReplacementV1, RetirementJournalReplacementAdmissionErrorV1> {
    if batch.replacements.len() != 1 {
      return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("buffered authority admission requires exactly one replacement"));
    }
    let (reason_counts, batch_digest) = self.preflight_batch(&batch, monotonic_now_ms)?;
    let replacement = &batch.replacements[0];
    let before = self.owner.soft_state();
    self
      .owner
      .append_buffered(
        RetirementJournalRecordWriteV1 {
          reason: replacement.reason,
          replacement_publication_sequence: batch.replacement_publication_sequence,
          retired_at_ms: batch.retired_at_ms,
          old_incarnation: replacement.old_incarnation,
          replacement_incarnation: replacement.replacement_incarnation,
        },
        monotonic_now_ms,
      )
      .map_err(|source| RetirementJournalReplacementAdmissionErrorV1::Journal {
        admitted_records: u32::from(source.incoming_record_retained()),
        source,
      })?;
    let after = self.owner.soft_state();
    Ok(PreparedRetirementJournalReplacementV1 {
      permit: RetirementJournalActivationPermitV1 {
        hash_algorithm: self.owner.algorithm,
        replacement_publication_sequence: batch.replacement_publication_sequence,
        retired_at_ms: batch.retired_at_ms,
        replacement_count: 1,
        reason_counts,
        batch_digest,
      },
      journal_state: RetirementJournalActivationJournalStateV1::Buffered,
      buffered_rollback: Some(Box::new(RetirementJournalBufferedRollbackV1 {
        owner_instance_id: self.owner.owner_instance_id,
        algorithm: self.owner.algorithm,
        database_id: self.owner.database_id,
        before,
        after,
      })),
    })
  }

  pub fn execute<T, E, F>(
    self,
    batch: RetirementJournalReplacementBatchV1<'_>,
    monotonic_now_ms: u64,
    activate: F,
  ) -> Result<RetirementJournalReplacementOutcomeV1<T>, RetirementJournalReplacementErrorV1<E>>
  where
    F: FnOnce(&RetirementJournalActivationPermitV1) -> Result<T, E>,
  {
    let prepared = {
      let mut coordinator = self;
      coordinator.prepare(batch, monotonic_now_ms)?
    };
    prepared.activate(activate)
  }

  fn preflight_batch(
    &self,
    batch: &RetirementJournalReplacementBatchV1<'_>,
    monotonic_now_ms: u64,
  ) -> Result<([u32; 5], Vec<u8>), RetirementJournalReplacementAdmissionErrorV1> {
    if batch.replacements.is_empty() {
      return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("replacement batch is empty"));
    }
    if batch.replacement_publication_sequence == 0 || batch.retired_at_ms == 0 {
      return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("publication sequence and retirement time must be nonzero"));
    }
    self.owner.ensure_operable().map_err(|source| RetirementJournalReplacementAdmissionErrorV1::Journal { source, admitted_records: 0 })?;
    if self.owner.last_observed_at_ms.is_some_and(|previous| monotonic_now_ms < previous) {
      return Err(RetirementJournalReplacementAdmissionErrorV1::Journal {
        source: RetirementJournalOwnerErrorV1::ClockRegression,
        admitted_records: 0,
      });
    }

    let algorithm = self.owner.algorithm;
    let batch_digest = retirement_journal_replacement_batch_digest_v1(batch, algorithm)?;
    let mut previous_old = if self.owner.last_old_incarnation.is_empty() {
      None
    } else {
      Some(decode_physical_incarnation(&self.owner.last_old_incarnation, algorithm).map_err(|source| {
        RetirementJournalReplacementAdmissionErrorV1::Journal { source: RetirementJournalOwnerErrorV1::from(source), admitted_records: 0 }
      })?)
    };
    let mut previous_sequence = self.owner.last_record_sequence;
    let mut previous_logical_key: Option<&[u8]> = None;
    let mut reason_counts = [0u32; 5];

    for replacement in batch.replacements {
      let record = RetirementJournalRecordWriteV1 {
        reason: replacement.reason,
        replacement_publication_sequence: batch.replacement_publication_sequence,
        retired_at_ms: batch.retired_at_ms,
        old_incarnation: replacement.old_incarnation,
        replacement_incarnation: replacement.replacement_incarnation,
      };
      encode_record(record, algorithm)
        .map_err(|source| RetirementJournalReplacementAdmissionErrorV1::Journal { source, admitted_records: 0 })?;
      let old = decode_physical_incarnation(replacement.old_incarnation, algorithm).map_err(|source| {
        RetirementJournalReplacementAdmissionErrorV1::Journal { source: RetirementJournalOwnerErrorV1::from(source), admitted_records: 0 }
      })?;
      let next = decode_physical_incarnation(replacement.replacement_incarnation, algorithm).map_err(|source| {
        RetirementJournalReplacementAdmissionErrorV1::Journal { source: RetirementJournalOwnerErrorV1::from(source), admitted_records: 0 }
      })?;
      if old.logical_key != next.logical_key {
        return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("old and replacement logical keys differ"));
      }
      if old.entry_type != next.entry_type {
        return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("old and replacement entry types differ"));
      }
      if next.write_sequence <= old.write_sequence {
        return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("replacement write sequence does not advance"));
      }
      if physical_extents_overlap(&old, &next) {
        return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("old and replacement WAL extents overlap"));
      }
      if previous_logical_key.is_some_and(|previous| previous >= old.logical_key) {
        return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("replacement stable keys are not strictly ordered and unique"));
      }
      if previous_sequence > batch.replacement_publication_sequence
        || (previous_sequence == batch.replacement_publication_sequence
          && previous_old.as_ref().is_some_and(|previous| compare_physical_incarnations_v1(previous, &old) != Ordering::Less))
      {
        return Err(RetirementJournalReplacementAdmissionErrorV1::Preflight("replacement records violate journal order"));
      }
      previous_sequence = batch.replacement_publication_sequence;
      previous_old = Some(old);
      previous_logical_key = Some(old.logical_key);
      let count = &mut reason_counts[retirement_reason_index(replacement.reason)];
      *count =
        count.checked_add(1).ok_or(RetirementJournalReplacementAdmissionErrorV1::Preflight("replacement reason count overflowed"))?;
    }
    Ok((reason_counts, batch_digest))
  }
}

/// Produce the exact bounded rolling identity carried by an activation permit.
///
/// The chaining form avoids materializing a replacement batch while binding
/// the shared publication metadata and every canonical encoded retirement
/// record in order.
pub fn retirement_journal_replacement_batch_digest_v1(
  batch: &RetirementJournalReplacementBatchV1<'_>,
  algorithm: HashAlgorithm,
) -> Result<Vec<u8>, RetirementJournalReplacementAdmissionErrorV1> {
  let replacement_count = u32::try_from(batch.replacements.len())
    .map_err(|_| RetirementJournalReplacementAdmissionErrorV1::Preflight("replacement count exceeds u32"))?;
  let sequence = batch.replacement_publication_sequence.to_le_bytes();
  let retired_at = batch.retired_at_ms.to_le_bytes();
  let count = replacement_count.to_le_bytes();
  let mut digest = digest_parts(algorithm, &[b"aeordb.retirement-replacement-batch.v1\0", &sequence, &retired_at, &count]);
  for replacement in batch.replacements {
    let encoded = encode_record(
      RetirementJournalRecordWriteV1 {
        reason: replacement.reason,
        replacement_publication_sequence: batch.replacement_publication_sequence,
        retired_at_ms: batch.retired_at_ms,
        old_incarnation: replacement.old_incarnation,
        replacement_incarnation: replacement.replacement_incarnation,
      },
      algorithm,
    )
    .map_err(|source| RetirementJournalReplacementAdmissionErrorV1::Journal { source, admitted_records: 0 })?;
    digest = digest_parts(algorithm, &[b"aeordb.retirement-replacement-batch-step.v1\0", &digest, &encoded]);
  }
  Ok(digest)
}

const fn retirement_reason_index(reason: RetirementReasonV1) -> usize {
  match reason {
    RetirementReasonV1::StableKeyReplace => 0,
    RetirementReasonV1::Relocation => 1,
    RetirementReasonV1::Repair => 2,
    RetirementReasonV1::Migration => 3,
    RetirementReasonV1::PointerOrControlReplace => 4,
  }
}

fn physical_extents_overlap(left: &PhysicalIncarnationV1<'_>, right: &PhysicalIncarnationV1<'_>) -> bool {
  let left_end = left.wal_offset + u64::from(left.entity_length);
  let right_end = right.wal_offset + u64::from(right.entity_length);
  left.wal_offset < right_end && right.wal_offset < left_end
}

pub struct RetirementJournalOwnerV1<'a> {
  owner_instance_id: u64,
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
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

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
    let mut owner_instance_id = NEXT_RETIREMENT_JOURNAL_OWNER_INSTANCE_ID_V1.load(AtomicOrdering::Relaxed);
    loop {
      let next_owner_instance_id = owner_instance_id.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
      match NEXT_RETIREMENT_JOURNAL_OWNER_INSTANCE_ID_V1.compare_exchange_weak(
        owner_instance_id,
        next_owner_instance_id,
        AtomicOrdering::Relaxed,
        AtomicOrdering::Relaxed,
      ) {
        Ok(reserved_owner_instance_id) => {
          owner_instance_id = reserved_owner_instance_id;
          break;
        }
        Err(observed_owner_instance_id) => owner_instance_id = observed_owner_instance_id,
      }
    }
    let record_capacity = options.target_segment_bytes.saturating_sub(complete_segment_fixed_length(algorithm));
    Ok(Self {
      owner_instance_id,
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

  fn append_buffered(
    &mut self,
    record: RetirementJournalRecordWriteV1<'_>,
    monotonic_now_ms: u64,
  ) -> Result<(), RetirementJournalOwnerErrorV1> {
    self.preflight(monotonic_now_ms)?;
    let encoded = encode_record(record, self.algorithm)?;
    self.validate_order(&encoded)?;
    let prospective_length =
      self.current_segment_length().checked_add(encoded.len()).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    if prospective_length > self.options.target_segment_bytes {
      return Err(RetirementJournalOwnerErrorV1::InvalidOptions("buffered replacement does not fit the admitted retirement segment"));
    }
    let pending_records = self.pending_records.checked_add(1).ok_or(RetirementJournalOwnerErrorV1::ArithmeticOverflow)?;
    if self.pending_records == 0 {
      self.pending_first_sequence = record.replacement_publication_sequence;
      self.pending_started_at_ms = Some(monotonic_now_ms);
    }
    self.records.extend_from_slice(&encoded);
    self.pending_records = pending_records;
    self.pending_last_sequence = record.replacement_publication_sequence;
    self.last_record_sequence = record.replacement_publication_sequence;
    self.last_old_incarnation.clear();
    self.last_old_incarnation.extend_from_slice(record.old_incarnation);
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

  fn soft_state(&self) -> RetirementJournalSoftStateV1 {
    RetirementJournalSoftStateV1 {
      records_length: self.records.len(),
      pending_records: self.pending_records,
      pending_first_sequence: self.pending_first_sequence,
      pending_last_sequence: self.pending_last_sequence,
      pending_started_at_ms: self.pending_started_at_ms,
      last_observed_at_ms: self.last_observed_at_ms,
      last_record_sequence: self.last_record_sequence,
      last_old_incarnation: self.last_old_incarnation.clone(),
    }
  }

  fn restore_soft_state(&mut self, state: RetirementJournalSoftStateV1) {
    self.records.truncate(state.records_length);
    self.pending_records = state.pending_records;
    self.pending_first_sequence = state.pending_first_sequence;
    self.pending_last_sequence = state.pending_last_sequence;
    self.pending_started_at_ms = state.pending_started_at_ms;
    self.last_observed_at_ms = state.last_observed_at_ms;
    self.last_record_sequence = state.last_record_sequence;
    self.last_old_incarnation = state.last_old_incarnation;
  }

  pub(crate) fn preflight_record_batch<'record>(
    &self,
    records: impl IntoIterator<Item = RetirementJournalRecordWriteV1<'record>>,
    monotonic_now_ms: u64,
  ) -> Result<(), RetirementJournalOwnerErrorV1> {
    self.preflight_operation(monotonic_now_ms)?;
    let mut previous_sequence = self.last_record_sequence;
    let mut previous_old = self.last_old_incarnation.clone();
    for record in records {
      let encoded = encode_record(record, self.algorithm)?;
      let sequence = u64::from_le_bytes(encoded[8..16].try_into().map_err(|_| RetirementJournalOwnerErrorV1::ArithmeticOverflow)?);
      if sequence < previous_sequence {
        return Err(RetirementJournalOwnerErrorV1::RecordOrder);
      }
      if sequence == previous_sequence && previous_sequence != 0 {
        let physical_length = 24 + 2 * self.algorithm.hash_length();
        let old = decode_physical_incarnation(&encoded[24..24 + physical_length], self.algorithm)?;
        let previous = decode_physical_incarnation(&previous_old, self.algorithm)?;
        if compare_physical_incarnations_v1(&previous, &old) != Ordering::Less {
          return Err(RetirementJournalOwnerErrorV1::RecordOrder);
        }
      }
      previous_sequence = sequence;
      previous_old.clear();
      previous_old.extend_from_slice(record.old_incarnation);
    }
    Ok(())
  }

  fn preflight(&mut self, monotonic_now_ms: u64) -> Result<(), RetirementJournalOwnerErrorV1> {
    self.preflight_operation(monotonic_now_ms)?;
    self.last_observed_at_ms = Some(monotonic_now_ms);
    Ok(())
  }

  pub(crate) fn preflight_operation(&self, monotonic_now_ms: u64) -> Result<(), RetirementJournalOwnerErrorV1> {
    self.ensure_operable()?;
    if self.last_observed_at_ms.is_some_and(|previous| monotonic_now_ms < previous) {
      return Err(RetirementJournalOwnerErrorV1::ClockRegression);
    }
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
