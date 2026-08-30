use std::cmp::Ordering;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc::{
  EncodedImmutableGcArtifactV1, GcArtifactKindV1, ImmutableGcArtifactWriteV1, PhysicalIncarnationV1, compare_physical_incarnations_v1,
  decode_gc_artifact_envelope, decode_physical_incarnation, encode_immutable_gc_artifact, immutable_gc_artifact_key, u16_at, u32_at,
  u64_at,
};
use super::contract_generated::root_retirement_reason_v1;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MAX_MANIFEST_LENGTH: usize = 1_024 * 1_024;
const MAX_PAGE_LENGTH: usize = 16 * 1_024 * 1_024;
const MAX_DIRECTORY_LENGTH: usize = 4 * 1_024 * 1_024;
pub const MAXIMUM_GC_DIRECTORY_ENTRIES_V1: u32 = 65_536;
const MAX_KEY_LENGTH: usize = 1_024 * 1_024;
const MAX_DELTAS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum GcDirectoryRoleV1 {
  Candidates = 1,
  RootExpiry = 2,
  PhysicalInventory = 3,
  FreeExtents = 4,
  Claims = 5,
  RootCandidates = 8,
}

impl GcDirectoryRoleV1 {
  pub fn directory_name(self) -> &'static str {
    match self {
      Self::Candidates => "candidates",
      Self::RootExpiry => "root-expiry",
      Self::PhysicalInventory => "physical-inventory",
      Self::FreeExtents => "void-free-extents",
      Self::Claims => "void-claims",
      Self::RootCandidates => "root-candidates",
    }
  }

  pub fn page_name(self) -> &'static str {
    match self {
      Self::Candidates => "candidate",
      Self::RootExpiry => "root-expiry",
      Self::PhysicalInventory => "physical-inventory",
      Self::FreeExtents => "void-free-extent",
      Self::Claims => "void-claim",
      Self::RootCandidates => "root-candidate",
    }
  }

  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Candidates),
      2 => Ok(Self::RootExpiry),
      3 => Ok(Self::PhysicalInventory),
      4 => Ok(Self::FreeExtents),
      5 => Ok(Self::Claims),
      8 => Ok(Self::RootCandidates),
      _ => Err(kind_error("gc_directory_role", format!("unknown GC directory role {value}"))),
    }
  }
}

#[derive(Debug, Clone)]
pub struct GcStatePageV1<'a> {
  pub role: GcDirectoryRoleV1,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub record_count: u32,
  pub logical_bytes: u64,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub records: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcPhysicalHintV1 {
  pub wal_offset: u64,
  pub total_length: u32,
  pub write_sequence: u64,
}

#[derive(Debug, Clone)]
pub struct GcStatePageWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub role: GcDirectoryRoleV1,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub records: &'a [&'a [u8]],
}

#[derive(Debug, Clone, Copy)]
pub struct GcStateDirectoryEntryWriteV1<'a> {
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub child_hash: &'a [u8],
  pub child_generation: u64,
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub physical_hint: GcPhysicalHintV1,
}

#[derive(Debug, Clone)]
pub struct GcStateDirectoryWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub role: GcDirectoryRoleV1,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub level: u16,
  pub entries: &'a [GcStateDirectoryEntryWriteV1<'a>],
}

impl GcPhysicalHintV1 {
  pub fn is_complete(&self) -> bool {
    self.total_length != 0
  }
}

#[derive(Debug, Clone)]
pub struct GcStateDirectoryEntryV1<'a> {
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub child_hash: &'a [u8],
  pub child_generation: u64,
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub physical_hint: GcPhysicalHintV1,
}

#[derive(Debug, Clone)]
pub struct GcStateDirectoryV1<'a> {
  pub role: GcDirectoryRoleV1,
  pub database_id: &'a [u8],
  pub catalog_id: &'a [u8],
  pub generation: u64,
  pub level: u16,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub entries: Vec<GcStateDirectoryEntryV1<'a>>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct GcStateManifestV1<'a> {
  pub kind: GcArtifactKindV1,
  pub database_id: &'a [u8],
  pub generation: u64,
  pub populated: bool,
  pub record_count: u64,
  pub secondary_count: u64,
  pub primary_root: &'a [u8],
  pub secondary_root: Option<&'a [u8]>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum GcStateArtifactV1<'a> {
  Page(GcStatePageV1<'a>),
  Directory(GcStateDirectoryV1<'a>),
  CandidateDelta { record_count: u32, key: Vec<u8> },
  Manifest(GcStateManifestV1<'a>),
  RootRetirementCommit { mark_generation: u64, key: Vec<u8> },
  RootObjectReclaimProof { incarnation_count: u64, receipt_count: u64, key: Vec<u8> },
  RetirementJournal { record_count: u32, key: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootCandidateRecordV1<'a> {
  pub namespace_root_hash: &'a [u8],
  pub reason: u16,
  pub pending_since_ms: i64,
  pub first_unreachable_generation: u64,
  pub last_confirmed_unreachable_generation: u64,
  pub grace_at_pending_ms: u64,
  pub authority_root_set_digest: &'a [u8],
  pub admission_commit_payload_hash: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootExpiryStateV1 {
  LogicallyRetired,
  PhysicallyReclaimed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootExpiryRecordV1<'a> {
  pub namespace_root_hash: &'a [u8],
  pub retired_at_ms: i64,
  pub last_pending_since_ms: i64,
  pub final_mark_generation: u64,
  pub reason: u16,
  pub state: RootExpiryStateV1,
  pub retirement_commit_hash: &'a [u8],
  pub root_object_reclaim_proof_hash: Option<&'a [u8]>,
  pub evidence_expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalInventoryStateV1(u8);

impl PhysicalInventoryStateV1 {
  pub fn code(self) -> u8 {
    self.0
  }

  pub fn is_active(self) -> bool {
    self.0 == 1
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalInventoryRecordV1<'a> {
  pub encoded: &'a [u8],
  pub incarnation: PhysicalIncarnationV1<'a>,
  pub state: PhysicalInventoryStateV1,
  pub reason: u8,
  pub replacement: Option<PhysicalIncarnationV1<'a>>,
  pub discovered_at_ms: u64,
  pub retirement_sequence: Option<u64>,
  pub receipt_hash: Option<&'a [u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RetirementReasonV1 {
  StableKeyReplace = 1,
  Relocation = 2,
  Repair = 3,
  Migration = 4,
  PointerOrControlReplace = 5,
}

impl RetirementReasonV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::StableKeyReplace),
      2 => Ok(Self::Relocation),
      3 => Ok(Self::Repair),
      4 => Ok(Self::Migration),
      5 => Ok(Self::PointerOrControlReplace),
      _ => Err(closure_error("retirement_record_fields", format!("unknown retirement reason {value}"))),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetirementJournalRecordV1<'a> {
  pub encoded: &'a [u8],
  pub reason: RetirementReasonV1,
  pub replacement_publication_sequence: u64,
  pub retired_at_ms: u64,
  pub old: PhysicalIncarnationV1<'a>,
  pub replacement: PhysicalIncarnationV1<'a>,
}

#[derive(Debug, Clone)]
pub struct RetirementJournalSegmentV1<'a> {
  pub database_id: &'a [u8],
  pub segment_ordinal: u64,
  pub generation: u64,
  pub chain_reset: bool,
  pub first_replacement_sequence: u64,
  pub last_replacement_sequence: u64,
  pub record_count: u32,
  pub previous_segment_hash: Option<&'a [u8]>,
  pub records: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug)]
pub struct RetirementJournalRecordsV1<'a> {
  records: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
}

impl<'a> Iterator for RetirementJournalRecordsV1<'a> {
  type Item = FormatResult<RetirementJournalRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.records.next().map(|record| decode_retirement_journal_record_v1(record, self.algorithm))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.records.size_hint()
  }
}

impl ExactSizeIterator for RetirementJournalRecordsV1<'_> {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementJournalModelSummaryV1 {
  pub database_id: [u8; 16],
  pub segment_count: u64,
  pub record_count: u64,
  pub first_replacement_sequence: u64,
  pub last_replacement_sequence: u64,
  pub last_segment_ordinal: u64,
  pub last_segment_generation: u64,
  pub last_segment_hash: Vec<u8>,
  pub last_old_incarnation: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum RetirementJournalModelErrorV1 {
  #[error("retirement journal traversal was canceled")]
  Canceled,
  #[error("retirement journal exceeds its admitted record limit")]
  RecordLimit,
  #[error("the first retirement journal segment is not a chain reset")]
  InitialReset,
  #[error("a later retirement journal segment unexpectedly resets the chain")]
  UnexpectedReset,
  #[error("retirement journal segments belong to different databases")]
  DatabaseMismatch,
  #[error("retirement journal segment ordinals are not contiguous")]
  SegmentOrdinal,
  #[error("retirement journal predecessor hash does not select the prior segment")]
  PreviousHash,
  #[error("retirement journal segment shape is invalid")]
  SegmentShape,
  #[error("retirement journal records are not canonically ordered across segments")]
  RecordOrder,
  #[error("retirement journal counters overflowed")]
  ArithmeticOverflow,
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error("retirement journal model has already failed")]
  Failed,
  #[error("retirement journal chain is empty")]
  Empty,
}

impl RetirementJournalModelErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "retirement_journal_cancelled",
      Self::RecordLimit => "retirement_journal_record_limit",
      Self::InitialReset => "retirement_journal_initial_reset",
      Self::UnexpectedReset => "retirement_journal_unexpected_reset",
      Self::DatabaseMismatch => "retirement_journal_database",
      Self::SegmentOrdinal => "retirement_journal_segment_ordinal",
      Self::PreviousHash => "retirement_journal_previous_hash",
      Self::SegmentShape => "retirement_journal_segment_shape",
      Self::RecordOrder => "retirement_journal_record_order",
      Self::ArithmeticOverflow => "retirement_journal_arithmetic",
      Self::Format(error) => error.code(),
      Self::Failed => "retirement_journal_model_failed",
      Self::Empty => "retirement_journal_empty",
    }
  }
}

/// Constant-memory validator for one immutable retirement-journal chain.
///
/// Publication and recovery remain later P4-2 units. This model has no
/// authority to retire an incarnation or advance an audited-through watermark.
#[derive(Debug)]
pub struct RetirementJournalReferenceModelV1<'a> {
  algorithm: HashAlgorithm,
  cancellation: &'a CancellationToken,
  maximum_records: u64,
  database_id: Option<[u8; 16]>,
  segment_count: u64,
  record_count: u64,
  first_replacement_sequence: u64,
  last_replacement_sequence: u64,
  last_segment_ordinal: u64,
  last_segment_generation: u64,
  last_segment_hash: Vec<u8>,
  previous_old_incarnation: Vec<u8>,
  failed: bool,
}

impl<'a> RetirementJournalReferenceModelV1<'a> {
  pub fn new(algorithm: HashAlgorithm, cancellation: &'a CancellationToken, maximum_records: u64) -> Self {
    Self {
      algorithm,
      cancellation,
      maximum_records,
      database_id: None,
      segment_count: 0,
      record_count: 0,
      first_replacement_sequence: 0,
      last_replacement_sequence: 0,
      last_segment_ordinal: 0,
      last_segment_generation: 0,
      last_segment_hash: Vec::with_capacity(algorithm.hash_length()),
      previous_old_incarnation: Vec::with_capacity(24 + 2 * algorithm.hash_length()),
      failed: false,
    }
  }

  pub fn observe_segment(&mut self, segment: &RetirementJournalSegmentV1<'_>) -> Result<(), RetirementJournalModelErrorV1> {
    if self.failed {
      return Err(RetirementJournalModelErrorV1::Failed);
    }
    match self.observe_segment_inner(segment) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> Result<RetirementJournalModelSummaryV1, RetirementJournalModelErrorV1> {
    if self.failed {
      return Err(RetirementJournalModelErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(RetirementJournalModelErrorV1::Canceled);
    }
    if self.segment_count == 0 {
      return Err(RetirementJournalModelErrorV1::Empty);
    }
    let database_id = self.database_id.ok_or(RetirementJournalModelErrorV1::SegmentShape)?;
    Ok(RetirementJournalModelSummaryV1 {
      database_id,
      segment_count: self.segment_count,
      record_count: self.record_count,
      first_replacement_sequence: self.first_replacement_sequence,
      last_replacement_sequence: self.last_replacement_sequence,
      last_segment_ordinal: self.last_segment_ordinal,
      last_segment_generation: self.last_segment_generation,
      last_segment_hash: self.last_segment_hash,
      last_old_incarnation: self.previous_old_incarnation,
    })
  }

  fn observe_segment_inner(&mut self, segment: &RetirementJournalSegmentV1<'_>) -> Result<(), RetirementJournalModelErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(RetirementJournalModelErrorV1::Canceled);
    }
    let hash_width = self.algorithm.hash_length();
    if segment.database_id.len() != 16
      || segment.segment_ordinal == 0
      || segment.generation == 0
      || segment.first_replacement_sequence == 0
      || segment.first_replacement_sequence > segment.last_replacement_sequence
      || segment.record_count == 0
      || segment.key.len() != hash_width
      || segment.previous_segment_hash.is_some_and(|hash| hash.len() != hash_width)
      || segment.chain_reset != segment.previous_segment_hash.is_none()
    {
      return Err(RetirementJournalModelErrorV1::SegmentShape);
    }
    let next_record_count =
      self.record_count.checked_add(u64::from(segment.record_count)).ok_or(RetirementJournalModelErrorV1::ArithmeticOverflow)?;
    if next_record_count > self.maximum_records {
      return Err(RetirementJournalModelErrorV1::RecordLimit);
    }
    let database_id: [u8; 16] = segment.database_id.try_into().map_err(|_| RetirementJournalModelErrorV1::DatabaseMismatch)?;
    if self.segment_count == 0 {
      if !segment.chain_reset {
        return Err(RetirementJournalModelErrorV1::InitialReset);
      }
      self.database_id = Some(database_id);
      self.first_replacement_sequence = segment.first_replacement_sequence;
    } else {
      if segment.chain_reset {
        return Err(RetirementJournalModelErrorV1::UnexpectedReset);
      }
      if self.database_id != Some(database_id) {
        return Err(RetirementJournalModelErrorV1::DatabaseMismatch);
      }
      if self.last_segment_ordinal.checked_add(1) != Some(segment.segment_ordinal) {
        return Err(RetirementJournalModelErrorV1::SegmentOrdinal);
      }
      if segment.previous_segment_hash != Some(self.last_segment_hash.as_slice()) {
        return Err(RetirementJournalModelErrorV1::PreviousHash);
      }
    }

    let mut segment_first_sequence = None;
    let mut segment_last_sequence = 0;
    for record in retirement_journal_records_v1(segment, self.algorithm)? {
      if self.cancellation.is_cancelled() {
        return Err(RetirementJournalModelErrorV1::Canceled);
      }
      let record = record?;
      let physical_length = 24 + 2 * self.algorithm.hash_length();
      let old_bytes = &record.encoded[24..24 + physical_length];
      if self.last_replacement_sequence > record.replacement_publication_sequence
        || (self.last_replacement_sequence == record.replacement_publication_sequence
          && !self.previous_old_incarnation.is_empty()
          && compare_physical_bytes(self.algorithm, &self.previous_old_incarnation, old_bytes)? != Ordering::Less)
      {
        return Err(RetirementJournalModelErrorV1::RecordOrder);
      }
      segment_first_sequence.get_or_insert(record.replacement_publication_sequence);
      segment_last_sequence = record.replacement_publication_sequence;
      self.last_replacement_sequence = record.replacement_publication_sequence;
      self.previous_old_incarnation.clear();
      self.previous_old_incarnation.extend_from_slice(old_bytes);
    }
    if segment_first_sequence != Some(segment.first_replacement_sequence) || segment_last_sequence != segment.last_replacement_sequence {
      return Err(RetirementJournalModelErrorV1::SegmentShape);
    }

    self.segment_count = self.segment_count.checked_add(1).ok_or(RetirementJournalModelErrorV1::ArithmeticOverflow)?;
    self.record_count = next_record_count;
    self.last_segment_ordinal = segment.segment_ordinal;
    self.last_segment_generation = segment.generation;
    self.last_segment_hash.clear();
    self.last_segment_hash.extend_from_slice(&segment.key);
    Ok(())
  }
}

#[derive(Debug, Clone)]
pub struct PhysicalInventoryManifestV1<'a> {
  pub database_id: &'a [u8],
  pub generation: u64,
  pub completed_at_ms: u64,
  pub kv_layout_fingerprint: &'a [u8],
  pub audited_wal_offset: u64,
  pub audited_write_sequence: u64,
  pub retirement_journal_through_sequence: u64,
  pub directory_root: Option<&'a [u8]>,
  pub next_page_id: u64,
  pub active_count: u64,
  pub retired_count: u64,
  pub orphan_count: u64,
  pub quarantined_count: u64,
  pub reclaimed_count: u64,
  pub inventoried_bytes: u64,
  pub key: Vec<u8>,
  record_count: u64,
}

impl PhysicalInventoryManifestV1<'_> {
  pub fn record_count(&self) -> u64 {
    self.record_count
  }

  pub fn is_populated(&self) -> bool {
    self.directory_root.is_some()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalInventoryModelSummaryV1 {
  pub catalog_id: Option<[u8; 16]>,
  pub page_count: u64,
  pub record_count: u64,
  pub inventoried_bytes: u64,
  pub maximum_page_id: u64,
}

#[derive(Debug, Error)]
pub enum PhysicalInventoryModelErrorV1 {
  #[error("physical inventory traversal was canceled")]
  Canceled,
  #[error("physical inventory record limit is below the manifest count")]
  RecordLimit,
  #[error("physical inventory page belongs to another database")]
  DatabaseMismatch,
  #[error("physical inventory pages disagree on catalog identity")]
  CatalogMismatch,
  #[error("physical inventory records are not strictly ordered across pages")]
  RecordOrder,
  #[error("physical inventory extents overlap")]
  ExtentOverlap,
  #[error("physical inventory counters overflowed")]
  ArithmeticOverflow,
  #[error("physical inventory pages do not close against the selected manifest")]
  ManifestAggregate,
  #[error(transparent)]
  Format(#[from] FormatError),
  #[error("physical inventory model has already failed")]
  Failed,
}

impl PhysicalInventoryModelErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Canceled => "physical_inventory_canceled",
      Self::RecordLimit => "physical_inventory_record_limit",
      Self::DatabaseMismatch => "physical_inventory_database",
      Self::CatalogMismatch => "physical_inventory_catalog",
      Self::RecordOrder => "physical_inventory_record_order",
      Self::ExtentOverlap => "physical_inventory_extent_overlap",
      Self::ArithmeticOverflow => "physical_inventory_arithmetic",
      Self::ManifestAggregate => "physical_inventory_manifest_aggregate",
      Self::Format(error) => error.code(),
      Self::Failed => "physical_inventory_failed",
    }
  }
}

/// Constant-memory cross-page validator for one selected inventory manifest.
///
/// Storage traversal remains a P4-3 concern. This model consumes one decoded
/// page at a time and cannot publish inventory, quarantine, or reclaim state.
#[derive(Debug)]
pub struct PhysicalInventoryReferenceModelV1<'a> {
  manifest: &'a PhysicalInventoryManifestV1<'a>,
  algorithm: HashAlgorithm,
  cancellation: &'a CancellationToken,
  maximum_records: u64,
  catalog_id: Option<[u8; 16]>,
  page_count: u64,
  record_count: u64,
  inventoried_bytes: u64,
  maximum_page_id: u64,
  state_counts: [u64; 5],
  previous_row: Vec<u8>,
  previous_extent_end: Option<u64>,
  failed: bool,
}

impl<'a> PhysicalInventoryReferenceModelV1<'a> {
  pub fn new(
    manifest: &'a PhysicalInventoryManifestV1<'a>,
    algorithm: HashAlgorithm,
    cancellation: &'a CancellationToken,
    maximum_records: u64,
  ) -> Result<Self, PhysicalInventoryModelErrorV1> {
    if cancellation.is_cancelled() {
      return Err(PhysicalInventoryModelErrorV1::Canceled);
    }
    if manifest.record_count() > maximum_records {
      return Err(PhysicalInventoryModelErrorV1::RecordLimit);
    }
    let hash_width = algorithm.hash_length();
    if manifest.database_id.len() != 16
      || manifest.kv_layout_fingerprint.len() != hash_width
      || manifest.directory_root.is_some_and(|root| root.len() != hash_width)
      || manifest.key.len() != hash_width
    {
      return Err(PhysicalInventoryModelErrorV1::ManifestAggregate);
    }
    Ok(Self {
      manifest,
      algorithm,
      cancellation,
      maximum_records,
      catalog_id: None,
      page_count: 0,
      record_count: 0,
      inventoried_bytes: 0,
      maximum_page_id: 0,
      state_counts: [0; 5],
      previous_row: Vec::with_capacity(row_length(algorithm, GcDirectoryRoleV1::PhysicalInventory)),
      previous_extent_end: None,
      failed: false,
    })
  }

  pub fn observe_page(&mut self, page: &GcStatePageV1<'_>) -> Result<(), PhysicalInventoryModelErrorV1> {
    if self.failed {
      return Err(PhysicalInventoryModelErrorV1::Failed);
    }
    match self.observe_page_inner(page) {
      Ok(()) => Ok(()),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  pub fn finish(self) -> Result<PhysicalInventoryModelSummaryV1, PhysicalInventoryModelErrorV1> {
    if self.failed {
      return Err(PhysicalInventoryModelErrorV1::Failed);
    }
    if self.cancellation.is_cancelled() {
      return Err(PhysicalInventoryModelErrorV1::Canceled);
    }
    let expected_counts = [
      self.manifest.active_count,
      self.manifest.retired_count,
      self.manifest.orphan_count,
      self.manifest.quarantined_count,
      self.manifest.reclaimed_count,
    ];
    let populated = self.manifest.is_populated();
    if self.record_count != self.manifest.record_count()
      || self.inventoried_bytes != self.manifest.inventoried_bytes
      || self.state_counts != expected_counts
      || populated != (self.page_count != 0)
      || populated != self.catalog_id.is_some()
      || (populated && self.maximum_page_id >= self.manifest.next_page_id)
      || (!populated && self.maximum_page_id != 0)
    {
      return Err(PhysicalInventoryModelErrorV1::ManifestAggregate);
    }
    Ok(PhysicalInventoryModelSummaryV1 {
      catalog_id: self.catalog_id,
      page_count: self.page_count,
      record_count: self.record_count,
      inventoried_bytes: self.inventoried_bytes,
      maximum_page_id: self.maximum_page_id,
    })
  }

  fn observe_page_inner(&mut self, page: &GcStatePageV1<'_>) -> Result<(), PhysicalInventoryModelErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(PhysicalInventoryModelErrorV1::Canceled);
    }
    if page.database_id != self.manifest.database_id {
      return Err(PhysicalInventoryModelErrorV1::DatabaseMismatch);
    }
    let page_catalog_id: [u8; 16] = page.catalog_id.try_into().map_err(|_| PhysicalInventoryModelErrorV1::CatalogMismatch)?;
    if self.catalog_id.is_some_and(|catalog_id| catalog_id != page_catalog_id) {
      return Err(PhysicalInventoryModelErrorV1::CatalogMismatch);
    }
    self.catalog_id = Some(page_catalog_id);

    for record in physical_inventory_records_v1(page, self.algorithm)? {
      if self.cancellation.is_cancelled() {
        return Err(PhysicalInventoryModelErrorV1::Canceled);
      }
      if self.record_count >= self.maximum_records {
        return Err(PhysicalInventoryModelErrorV1::RecordLimit);
      }
      let record = record?;
      if !self.previous_row.is_empty()
        && compare_rows(self.algorithm, GcDirectoryRoleV1::PhysicalInventory, &self.previous_row, record.encoded)? != Ordering::Less
      {
        return Err(PhysicalInventoryModelErrorV1::RecordOrder);
      }
      if self.previous_extent_end.is_some_and(|end| end > record.incarnation.wal_offset) {
        return Err(PhysicalInventoryModelErrorV1::ExtentOverlap);
      }
      let extent_end = record
        .incarnation
        .wal_offset
        .checked_add(u64::from(record.incarnation.entity_length))
        .ok_or(PhysicalInventoryModelErrorV1::ArithmeticOverflow)?;
      let state_index = usize::from(record.state.code() - 1);
      self.state_counts[state_index] =
        self.state_counts[state_index].checked_add(1).ok_or(PhysicalInventoryModelErrorV1::ArithmeticOverflow)?;
      self.record_count = self.record_count.checked_add(1).ok_or(PhysicalInventoryModelErrorV1::ArithmeticOverflow)?;
      self.previous_row.clear();
      self.previous_row.extend_from_slice(record.encoded);
      self.previous_extent_end = Some(extent_end);
    }
    self.page_count = self.page_count.checked_add(1).ok_or(PhysicalInventoryModelErrorV1::ArithmeticOverflow)?;
    self.inventoried_bytes =
      self.inventoried_bytes.checked_add(page.logical_bytes).ok_or(PhysicalInventoryModelErrorV1::ArithmeticOverflow)?;
    self.maximum_page_id = self.maximum_page_id.max(page.page_id);
    Ok(())
  }
}

#[derive(Debug)]
pub struct PhysicalInventoryRecordsV1<'a> {
  rows: std::slice::ChunksExact<'a, u8>,
  algorithm: HashAlgorithm,
}

impl<'a> Iterator for PhysicalInventoryRecordsV1<'a> {
  type Item = FormatResult<PhysicalInventoryRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.rows.next().map(|row| decode_physical_inventory_record_v1(row, self.algorithm))
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.rows.size_hint()
  }
}

impl ExactSizeIterator for PhysicalInventoryRecordsV1<'_> {}

impl GcStateArtifactV1<'_> {
  pub fn key(&self) -> &[u8] {
    match self {
      Self::Page(value) => &value.key,
      Self::Directory(value) => &value.key,
      Self::CandidateDelta { key, .. }
      | Self::RootRetirementCommit { key, .. }
      | Self::RootObjectReclaimProof { key, .. }
      | Self::RetirementJournal { key, .. } => key,
      Self::Manifest(value) => &value.key,
    }
  }

  pub fn summary(&self) -> String {
    match self {
      Self::Page(page) => format!("gc:page:{}:page={}:records={}", page.role.page_name(), page.page_id, page.record_count),
      Self::Directory(directory) => {
        format!("gc:directory:{}:level={}:records={}", directory.role.directory_name(), directory.level, directory.live_count)
      }
      Self::CandidateDelta { record_count, .. } => format!("gc:delta:candidate:records={record_count}"),
      Self::Manifest(manifest) => manifest_summary(manifest),
      Self::RootRetirementCommit { mark_generation, .. } => format!("gc:commit:root-retirement:mark={mark_generation}"),
      Self::RootObjectReclaimProof { incarnation_count, receipt_count, .. } => {
        format!("gc:proof:root-object-reclaim:incarnations={incarnation_count}:receipts={receipt_count}")
      }
      Self::RetirementJournal { record_count, .. } => format!("gc:journal:retirement:records={record_count}"),
    }
  }
}

pub fn encode_gc_state_page_v1(request: &GcStatePageWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  validate_gc_state_writer_identity(request.database_id, request.catalog_id, request.generation, "gc_page_identity")?;
  let artifact_kind = gc_state_page_kind(request.role)?;
  if request.page_id == 0 {
    return Err(identity_error("gc_page_identity", "GC page ID must be nonzero"));
  }
  if request.records.is_empty() {
    return Err(closure_error("gc_page_records", "GC page must contain at least one record"));
  }

  let expected_row_length = row_length(request.hash_algorithm, request.role);
  let mut records_length = 0usize;
  let mut previous: Option<&[u8]> = None;
  for row in request.records {
    if row.len() != expected_row_length {
      validate_row(request.hash_algorithm, request.role, row, false)?;
      return Err(closure_error("gc_page_records_length", "GC page row has the wrong fixed length"));
    }
    validate_row(request.hash_algorithm, request.role, row, false)?;
    if let Some(prior) = previous {
      if compare_rows(request.hash_algorithm, request.role, prior, row)? != Ordering::Less {
        return Err(order_error("gc_page_record_order", "GC page rows are not strictly ordered"));
      }
    }
    previous = Some(row);
    records_length = records_length.checked_add(row.len()).ok_or_else(|| length_error("GC page records length overflow"))?;
  }

  let lower_fence = gc_state_row_fence(request.hash_algorithm, request.role, request.records[0])?;
  let upper_fence = gc_state_row_fence(request.hash_algorithm, request.role, request.records[request.records.len() - 1])?;
  let body_length = 64usize
    .checked_add(lower_fence.len())
    .and_then(|length| length.checked_add(upper_fence.len()))
    .and_then(|length| length.checked_add(records_length))
    .ok_or_else(|| length_error("GC page body length overflow"))?;
  if body_length > MAX_PAGE_LENGTH {
    return Err(amplification_error("gc_page_length", body_length, MAX_PAGE_LENGTH));
  }
  let record_count = gc_writer_u32(request.records.len(), "GC page record count exceeds u32")?;
  let lower_length = gc_writer_u32(lower_fence.len(), "GC page lower-fence length exceeds u32")?;
  let upper_length = gc_writer_u32(upper_fence.len(), "GC page upper-fence length exceeds u32")?;
  let logical_bytes = gc_writer_u64(records_length, "GC page logical byte count exceeds u64")?;

  let mut body = vec![0u8; body_length];
  put_u16_v1(&mut body, 4, 1);
  put_u16_v1(&mut body, 6, request.role as u16);
  put_u32_v1(&mut body, 8, lower_length);
  put_u32_v1(&mut body, 12, upper_length);
  put_u32_v1(&mut body, 16, record_count);
  put_u32_v1(&mut body, 20, record_count);
  put_u64_v1(&mut body, 24, logical_bytes);
  put_u64_v1(&mut body, 32, logical_bytes);
  let mut cursor = 64;
  body[cursor..cursor + lower_fence.len()].copy_from_slice(&lower_fence);
  cursor += lower_fence.len();
  body[cursor..cursor + upper_fence.len()].copy_from_slice(&upper_fence);
  cursor += upper_fence.len();
  for row in request.records {
    body[cursor..cursor + row.len()].copy_from_slice(row);
    cursor += row.len();
  }

  let mut identity = Vec::with_capacity(42);
  identity.extend_from_slice(request.database_id);
  identity.extend_from_slice(request.catalog_id);
  identity.extend_from_slice(&(request.role as u16).to_le_bytes());
  identity.extend_from_slice(&request.page_id.to_le_bytes());
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: artifact_kind,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let GcStateArtifactV1::Page(_) = decode_gc_state_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("gc_page_encoded_kind", "encoded GC page did not decode as a page"));
  };
  Ok(encoded)
}

pub fn encode_gc_state_directory_v1(request: &GcStateDirectoryWriteV1<'_>) -> FormatResult<EncodedImmutableGcArtifactV1> {
  validate_gc_state_writer_identity(request.database_id, request.catalog_id, request.generation, "gc_directory_identity")?;
  if request.level > 15 {
    return Err(closure_error("gc_directory_level", "GC directory level exceeds the frozen maximum"));
  }
  if request.entries.is_empty() {
    return Err(closure_error("gc_directory_entries", "GC directory must contain at least one child"));
  }
  if request.entries.len() > MAXIMUM_GC_DIRECTORY_ENTRIES_V1 as usize {
    return Err(amplification_error("gc_directory_entries", request.entries.len(), MAXIMUM_GC_DIRECTORY_ENTRIES_V1 as usize));
  }

  let hash_width = request.hash_algorithm.hash_length();
  let descriptor_fixed_length = if request.level == 0 { 72usize } else { 88usize }
    .checked_add(hash_width)
    .ok_or_else(|| length_error("GC directory descriptor width overflow"))?;
  let mut entries_length = 0usize;
  let mut live_count = 0u64;
  let mut tombstone_count = 0u64;
  let mut page_count = 0u64;
  let mut logical_bytes = 0u64;
  let page_backed = gc_directory_role_is_page_backed(request.role);
  let mut minimum_page_id = if page_backed { u64::MAX } else { 0 };
  let mut maximum_page_id = 0u64;
  let mut previous_upper: Option<&[u8]> = None;
  for entry in request.entries {
    compare_fences(request.hash_algorithm, request.role, entry.lower_fence, entry.upper_fence)?;
    if let Some(prior) = previous_upper {
      if compare_fence_values(request.hash_algorithm, request.role, prior, entry.lower_fence)? != Ordering::Less {
        return Err(order_error("gc_directory_child_order", "GC directory child ranges overlap or are not strictly ordered"));
      }
    }
    previous_upper = Some(entry.upper_fence);
    let valid_page_shape = if page_backed {
      entry.page_count > 0
        && entry.minimum_page_id > 0
        && entry.minimum_page_id <= entry.maximum_page_id
        && (request.level != 0 || (entry.page_count == 1 && entry.minimum_page_id == entry.maximum_page_id))
    } else {
      entry.page_count == 0 && entry.minimum_page_id == 0 && entry.maximum_page_id == 0
    };
    if entry.child_hash.len() != hash_width
      || all_zero(entry.child_hash)
      || entry.child_generation == 0
      || entry.child_generation > request.generation
      || entry.live_count == 0
      || entry.tombstone_count != 0
      || entry.logical_bytes == 0
      || !valid_page_shape
    {
      let code = if request.level == 0 { "gc_directory_leaf" } else { "gc_directory_internal" };
      return Err(closure_error(code, "GC directory child identity, generation, counts, or bytes are invalid"));
    }
    entries_length = entries_length
      .checked_add(descriptor_fixed_length)
      .and_then(|length| length.checked_add(entry.lower_fence.len()))
      .and_then(|length| length.checked_add(entry.upper_fence.len()))
      .ok_or_else(|| length_error("GC directory entries length overflow"))?;
    live_count = live_count.checked_add(entry.live_count).ok_or_else(|| length_error("GC directory live count overflow"))?;
    tombstone_count =
      tombstone_count.checked_add(entry.tombstone_count).ok_or_else(|| length_error("GC directory tombstone count overflow"))?;
    page_count = page_count.checked_add(entry.page_count).ok_or_else(|| length_error("GC directory page count overflow"))?;
    logical_bytes = logical_bytes.checked_add(entry.logical_bytes).ok_or_else(|| length_error("GC directory logical bytes overflow"))?;
    minimum_page_id = minimum_page_id.min(entry.minimum_page_id);
    maximum_page_id = maximum_page_id.max(entry.maximum_page_id);
  }

  let lower_fence = request.entries[0].lower_fence;
  let upper_fence = request.entries[request.entries.len() - 1].upper_fence;
  let body_length = 80usize
    .checked_add(lower_fence.len())
    .and_then(|length| length.checked_add(upper_fence.len()))
    .and_then(|length| length.checked_add(entries_length))
    .ok_or_else(|| length_error("GC directory body length overflow"))?;
  if body_length > MAX_DIRECTORY_LENGTH {
    return Err(amplification_error("gc_directory_length", body_length, MAX_DIRECTORY_LENGTH));
  }
  let entry_count = gc_writer_u32(request.entries.len(), "GC directory entry count exceeds u32")?;
  let lower_length = gc_writer_u32(lower_fence.len(), "GC directory lower-fence length exceeds u32")?;
  let upper_length = gc_writer_u32(upper_fence.len(), "GC directory upper-fence length exceeds u32")?;
  let encoded_entries_length = gc_writer_u32(entries_length, "GC directory entries length exceeds u32")?;

  let mut body = vec![0u8; body_length];
  put_u16_v1(&mut body, 0, request.level);
  put_u16_v1(&mut body, 2, request.role as u16);
  put_u32_v1(&mut body, 4, entry_count);
  put_u32_v1(&mut body, 16, lower_length);
  put_u32_v1(&mut body, 20, upper_length);
  put_u64_v1(&mut body, 24, live_count);
  put_u64_v1(&mut body, 32, tombstone_count);
  put_u64_v1(&mut body, 40, page_count);
  put_u64_v1(&mut body, 48, logical_bytes);
  put_u64_v1(&mut body, 56, minimum_page_id);
  put_u64_v1(&mut body, 64, maximum_page_id);
  put_u32_v1(&mut body, 72, encoded_entries_length);
  let mut cursor = 80;
  body[cursor..cursor + lower_fence.len()].copy_from_slice(lower_fence);
  cursor += lower_fence.len();
  body[cursor..cursor + upper_fence.len()].copy_from_slice(upper_fence);
  cursor += upper_fence.len();
  for entry in request.entries {
    put_u32_v1(&mut body, cursor, gc_writer_u32(entry.lower_fence.len(), "GC directory child lower-fence length exceeds u32")?);
    put_u32_v1(&mut body, cursor + 4, gc_writer_u32(entry.upper_fence.len(), "GC directory child upper-fence length exceeds u32")?);
    let fields = if request.level == 0 {
      put_u64_v1(&mut body, cursor + 8, entry.minimum_page_id);
      body[cursor + 16..cursor + 16 + hash_width].copy_from_slice(entry.child_hash);
      cursor + 16 + hash_width
    } else {
      body[cursor + 8..cursor + 8 + hash_width].copy_from_slice(entry.child_hash);
      cursor + 8 + hash_width
    };
    put_u64_v1(&mut body, fields, entry.child_generation);
    put_u64_v1(&mut body, fields + 8, entry.live_count);
    put_u64_v1(&mut body, fields + 16, entry.tombstone_count);
    let physical_hint_offset = if request.level == 0 {
      put_u64_v1(&mut body, fields + 24, entry.logical_bytes);
      fields + 32
    } else {
      put_u64_v1(&mut body, fields + 24, entry.page_count);
      put_u64_v1(&mut body, fields + 32, entry.logical_bytes);
      put_u64_v1(&mut body, fields + 40, entry.minimum_page_id);
      put_u64_v1(&mut body, fields + 48, entry.maximum_page_id);
      fields + 56
    };
    put_u64_v1(&mut body, physical_hint_offset, entry.physical_hint.wal_offset);
    put_u32_v1(&mut body, physical_hint_offset + 8, entry.physical_hint.total_length);
    put_u64_v1(&mut body, physical_hint_offset + 16, entry.physical_hint.write_sequence);
    cursor += descriptor_fixed_length;
    body[cursor..cursor + entry.lower_fence.len()].copy_from_slice(entry.lower_fence);
    cursor += entry.lower_fence.len();
    body[cursor..cursor + entry.upper_fence.len()].copy_from_slice(entry.upper_fence);
    cursor += entry.upper_fence.len();
  }

  let mut identity = Vec::with_capacity(34);
  identity.extend_from_slice(request.database_id);
  identity.extend_from_slice(request.catalog_id);
  identity.extend_from_slice(&(request.role as u16).to_le_bytes());
  let encoded = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::GcArtifactDirectoryNode,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  let GcStateArtifactV1::Directory(_) = decode_gc_state_artifact(&encoded.value, request.hash_algorithm)? else {
    return Err(closure_error("gc_directory_encoded_kind", "encoded GC directory did not decode as a directory"));
  };
  Ok(encoded)
}

fn validate_gc_state_writer_identity(database_id: &[u8], catalog_id: &[u8], generation: u64, code: &'static str) -> FormatResult<()> {
  if database_id.len() != 16 || catalog_id.len() != 16 || all_zero(database_id) || all_zero(catalog_id) || generation == 0 {
    return Err(identity_error(code, "GC state database/catalog identity or generation is invalid"));
  }
  Ok(())
}

fn gc_state_page_kind(role: GcDirectoryRoleV1) -> FormatResult<GcArtifactKindV1> {
  match role {
    GcDirectoryRoleV1::Candidates => Ok(GcArtifactKindV1::CandidatePage),
    GcDirectoryRoleV1::RootExpiry => Ok(GcArtifactKindV1::RootExpiryPage),
    GcDirectoryRoleV1::PhysicalInventory => Ok(GcArtifactKindV1::PhysicalInventoryPage),
    GcDirectoryRoleV1::RootCandidates => Ok(GcArtifactKindV1::RootCandidatePage),
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => {
      Err(kind_error("gc_page_role", "specialized Void roles cannot use the generic GC state page writer"))
    }
  }
}

fn gc_directory_role_is_page_backed(role: GcDirectoryRoleV1) -> bool {
  role != GcDirectoryRoleV1::Claims
}

fn gc_state_row_fence(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, row: &[u8]) -> FormatResult<Vec<u8>> {
  let hash_width = algorithm.hash_length();
  Ok(match role {
    GcDirectoryRoleV1::Candidates => row[..24 + 2 * hash_width].to_vec(),
    GcDirectoryRoleV1::RootExpiry | GcDirectoryRoleV1::RootCandidates => row[..hash_width].to_vec(),
    GcDirectoryRoleV1::PhysicalInventory => {
      let physical_length = 24 + 2 * hash_width;
      let mut fence = Vec::with_capacity(8 + physical_length);
      fence.extend_from_slice(&row[2 * hash_width..2 * hash_width + 8]);
      fence.extend_from_slice(&row[..physical_length]);
      fence
    }
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => {
      return Err(kind_error("gc_page_role", "specialized Void roles cannot use generic GC state rows"));
    }
  })
}

fn put_u16_v1(target: &mut [u8], offset: usize, value: u16) {
  target[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32_v1(target: &mut [u8], offset: usize, value: u32) {
  target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64_v1(target: &mut [u8], offset: usize, value: u64) {
  target[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn gc_writer_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  match u32::try_from(value) {
    Ok(value) => Ok(value),
    Err(error) => Err(length_error(format!("{context}: {error}"))),
  }
}

fn gc_writer_u64(value: usize, context: &'static str) -> FormatResult<u64> {
  match u64::try_from(value) {
    Ok(value) => Ok(value),
    Err(error) => Err(length_error(format!("{context}: {error}"))),
  }
}

pub fn decode_gc_state_artifact(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<GcStateArtifactV1<'_>> {
  let hinted_kind = u16_at(bytes, 6).ok().and_then(GcArtifactKindV1::from_u16);
  let hard_cap = match hinted_kind {
    Some(GcArtifactKindV1::GcArtifactDirectoryNode) => MAX_DIRECTORY_LENGTH,
    Some(
      GcArtifactKindV1::CandidatePage
      | GcArtifactKindV1::RootExpiryPage
      | GcArtifactKindV1::PhysicalInventoryPage
      | GcArtifactKindV1::RootCandidatePage
      | GcArtifactKindV1::RetirementJournalSegment,
    ) => MAX_PAGE_LENGTH,
    Some(
      GcArtifactKindV1::RootExpiryCatalogManifest
      | GcArtifactKindV1::PhysicalInventoryManifest
      | GcArtifactKindV1::QuarantineManifest
      | GcArtifactKindV1::RootLifecycleManifest
      | GcArtifactKindV1::RootRetirementCommit
      | GcArtifactKindV1::RootObjectReclaimProof,
    ) => MAX_MANIFEST_LENGTH,
    _ => super::gc::MAX_GC_ARTIFACT_LENGTH,
  };
  if bytes.len() > hard_cap {
    return Err(amplification_error("gc_state_artifact_length", bytes.len(), hard_cap));
  }
  let envelope = decode_gc_artifact_envelope(bytes)?;
  let key = immutable_gc_artifact_key(algorithm, envelope.kind, bytes);
  match envelope.kind {
    GcArtifactKindV1::CandidatePage
    | GcArtifactKindV1::RootExpiryPage
    | GcArtifactKindV1::PhysicalInventoryPage
    | GcArtifactKindV1::RootCandidatePage => decode_page(bytes, algorithm, key).map(GcStateArtifactV1::Page),
    GcArtifactKindV1::GcArtifactDirectoryNode => decode_directory(bytes, algorithm, key).map(GcStateArtifactV1::Directory),
    GcArtifactKindV1::CandidateDelta => decode_candidate_delta(bytes, algorithm, key),
    GcArtifactKindV1::RootExpiryCatalogManifest
    | GcArtifactKindV1::PhysicalInventoryManifest
    | GcArtifactKindV1::QuarantineManifest
    | GcArtifactKindV1::RootLifecycleManifest => decode_manifest(bytes, algorithm, key).map(GcStateArtifactV1::Manifest),
    GcArtifactKindV1::RootRetirementCommit => decode_root_retirement_commit(bytes, algorithm, key),
    GcArtifactKindV1::RootObjectReclaimProof => decode_root_object_reclaim_proof(bytes, algorithm, key),
    GcArtifactKindV1::RetirementJournalSegment => decode_retirement_journal(bytes, algorithm, key),
    _ => Err(kind_error("gc_state_kind", format!("{} is not a lifecycle/inventory artifact", envelope.kind.name()))),
  }
}

pub fn decode_physical_inventory_manifest_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<PhysicalInventoryManifestV1<'_>> {
  if bytes.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("gc_manifest_length", bytes.len(), MAX_MANIFEST_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.kind != GcArtifactKindV1::PhysicalInventoryManifest {
    return Err(kind_error("physical_inventory_manifest_kind", "artifact is not a physical-inventory manifest"));
  }
  if artifact.identity.len() != 24 || all_zero(&artifact.identity[..16]) || u64_at(artifact.identity, 16)? != artifact.generation {
    return Err(identity_error(
      "physical_inventory_manifest_identity",
      "physical-inventory manifest database/generation identity is invalid",
    ));
  }
  let body = decode_inventory_manifest_body(artifact.body, algorithm, artifact.generation)?;
  Ok(PhysicalInventoryManifestV1 {
    database_id: &artifact.identity[..16],
    generation: artifact.generation,
    completed_at_ms: body.completed_at_ms,
    kv_layout_fingerprint: body.kv_layout_fingerprint,
    audited_wal_offset: body.audited_wal_offset,
    audited_write_sequence: body.audited_write_sequence,
    retirement_journal_through_sequence: body.retirement_journal_through_sequence,
    directory_root: body.populated().then_some(body.directory_root),
    next_page_id: body.next_page_id,
    active_count: body.active_count,
    retired_count: body.retired_count,
    orphan_count: body.orphan_count,
    quarantined_count: body.quarantined_count,
    reclaimed_count: body.reclaimed_count,
    inventoried_bytes: body.inventoried_bytes,
    key: immutable_gc_artifact_key(algorithm, artifact.kind, bytes),
    record_count: body.record_count,
  })
}

pub fn validate_physical_inventory_manifest_directory(
  manifest: &PhysicalInventoryManifestV1<'_>,
  directory: &GcStateDirectoryV1<'_>,
) -> FormatResult<()> {
  if !manifest.is_populated()
    || directory.role != GcDirectoryRoleV1::PhysicalInventory
    || directory.database_id != manifest.database_id
    || manifest.directory_root != Some(directory.key.as_slice())
    || directory.live_count != manifest.record_count()
    || directory.tombstone_count != 0
    || directory.logical_bytes != manifest.inventoried_bytes
    || directory.maximum_page_id >= manifest.next_page_id
  {
    return Err(closure_error(
      "physical_inventory_manifest_directory",
      "physical-inventory root directory does not close against its selected manifest",
    ));
  }
  Ok(())
}

pub fn validate_gc_directory_child(parent: &GcStateDirectoryV1<'_>, child: &GcStateDirectoryV1<'_>) -> FormatResult<()> {
  let descriptor = parent.entries.iter().find(|entry| entry.child_hash == child.key);
  if parent.level == 0
    || child.level.checked_add(1) != Some(parent.level)
    || parent.database_id != child.database_id
    || parent.catalog_id != child.catalog_id
    || parent.role != child.role
    || descriptor.is_none_or(|entry| {
      entry.child_generation != child.generation
        || entry.live_count != child.live_count
        || entry.tombstone_count != child.tombstone_count
        || entry.page_count != child.page_count
        || entry.logical_bytes != child.logical_bytes
        || entry.minimum_page_id != child.minimum_page_id
        || entry.maximum_page_id != child.maximum_page_id
        || entry.lower_fence != child.lower_fence
        || entry.upper_fence != child.upper_fence
    })
  {
    return Err(closure_error("gc_directory_child_closure", "GC directory descriptor does not match its immutable child directory"));
  }
  Ok(())
}

pub fn validate_gc_directory_page(directory: &GcStateDirectoryV1<'_>, page: &GcStatePageV1<'_>) -> FormatResult<()> {
  let descriptor = directory.entries.iter().find(|entry| entry.child_hash == page.key);
  if directory.level != 0
    || directory.database_id != page.database_id
    || directory.catalog_id != page.catalog_id
    || directory.role != page.role
    || descriptor.is_none_or(|entry| {
      entry.minimum_page_id != page.page_id
        || entry.maximum_page_id != page.page_id
        || entry.child_generation != page.generation
        || entry.live_count != u64::from(page.record_count)
        || entry.tombstone_count != 0
        || entry.page_count != 1
        || entry.logical_bytes != page.logical_bytes
        || entry.lower_fence != page.lower_fence
        || entry.upper_fence != page.upper_fence
    })
  {
    return Err(closure_error("gc_directory_page_closure", "GC directory descriptor does not match its immutable page"));
  }
  Ok(())
}

fn decode_page(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStatePageV1<'_>> {
  if bytes.len() > MAX_PAGE_LENGTH {
    return Err(amplification_error("gc_page_length", bytes.len(), MAX_PAGE_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let role = match artifact.kind {
    GcArtifactKindV1::CandidatePage => GcDirectoryRoleV1::Candidates,
    GcArtifactKindV1::RootExpiryPage => GcDirectoryRoleV1::RootExpiry,
    GcArtifactKindV1::PhysicalInventoryPage => GcDirectoryRoleV1::PhysicalInventory,
    GcArtifactKindV1::RootCandidatePage => GcDirectoryRoleV1::RootCandidates,
    _ => return Err(kind_error("gc_page_kind", "artifact is not a GC state page")),
  };
  if artifact.identity.len() != 42 {
    return Err(closure_error("gc_page_identity", "GC page identity must be 42 bytes"));
  }
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  if all_zero(database_id) || all_zero(catalog_id) || u16_at(artifact.identity, 32)? != role as u16 {
    return Err(identity_error("gc_page_identity", "GC page database/catalog/role identity is invalid"));
  }
  let page_id = u64_at(artifact.identity, 34)?;
  let body = artifact.body;
  if page_id == 0 || body.len() < 64 {
    return Err(identity_error("gc_page_identity_or_length", "GC page ID is zero or body is truncated"));
  }
  if u32_at(body, 0)? != 0 || body[40..64].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("gc_page_header", "GC page reserve fields must be zero"));
  }
  let lower_length = usize::try_from(u32_at(body, 8)?).map_err(|_| length_error("GC page lower-fence length"))?;
  let upper_length = usize::try_from(u32_at(body, 12)?).map_err(|_| length_error("GC page upper-fence length"))?;
  let record_count = u32_at(body, 16)?;
  let records_length = usize::try_from(u64_at(body, 24)?).map_err(|_| length_error("GC page records length"))?;
  if u16_at(body, 4)? != 1
    || u16_at(body, 6)? != role as u16
    || lower_length == 0
    || lower_length > MAX_KEY_LENGTH
    || upper_length == 0
    || upper_length > MAX_KEY_LENGTH
    || record_count == 0
    || u32_at(body, 20)? != record_count
    || u64_at(body, 32)? != records_length as u64
  {
    return Err(closure_error("gc_page_header", "GC page codec, role, fences, counts, or logical length is invalid"));
  }
  let records_start = 64usize
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("GC page fence end overflow"))?;
  if records_start.checked_add(records_length) != Some(body.len()) {
    return Err(trailing_error("gc_page_header", "GC page lengths do not consume body exactly"));
  }
  let lower_fence = &body[64..64 + lower_length];
  let upper_fence = &body[64 + lower_length..records_start];
  compare_fences(algorithm, role, lower_fence, upper_fence)?;
  let records = &body[records_start..];
  let row_length = row_length(algorithm, role);
  let expected_records_length = usize::try_from(record_count)
    .ok()
    .and_then(|count| count.checked_mul(row_length))
    .ok_or_else(|| length_error("GC page record-count multiplication overflow"))?;
  if expected_records_length != records.len() {
    return Err(closure_error("gc_page_records_length", "GC page record count does not match fixed records length"));
  }
  let mut previous: Option<&[u8]> = None;
  for row in records.chunks_exact(row_length) {
    validate_row(algorithm, role, row, false)?;
    if let Some(prior) = previous {
      if compare_rows(algorithm, role, prior, row)? != Ordering::Less {
        return Err(order_error("gc_page_record_order", "GC page rows are not strictly ordered"));
      }
    }
    previous = Some(row);
  }
  let first = records.get(..row_length).ok_or_else(|| trailing_error("gc_page_lower", "GC page has no first row"))?;
  let last = records.get(records.len() - row_length..).ok_or_else(|| trailing_error("gc_page_upper", "GC page has no last row"))?;
  if !row_key_equals_fence(algorithm, role, first, lower_fence)? || !row_key_equals_fence(algorithm, role, last, upper_fence)? {
    return Err(closure_error("gc_page_fences", "GC page fences do not match first/last record keys"));
  }
  Ok(GcStatePageV1 {
    role,
    database_id,
    catalog_id,
    generation: artifact.generation,
    page_id,
    record_count,
    logical_bytes: records_length as u64,
    lower_fence,
    upper_fence,
    records,
    key,
  })
}

fn decode_directory(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateDirectoryV1<'_>> {
  if bytes.len() > MAX_DIRECTORY_LENGTH {
    return Err(amplification_error("gc_directory_length", bytes.len(), MAX_DIRECTORY_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.kind != GcArtifactKindV1::GcArtifactDirectoryNode || artifact.identity.len() != 34 {
    return Err(closure_error("gc_directory_identity", "GC directory kind/identity length is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let catalog_id = &artifact.identity[16..32];
  let role = GcDirectoryRoleV1::from_u16(u16_at(artifact.identity, 32)?)?;
  if all_zero(database_id) || all_zero(catalog_id) || artifact.body.len() < 80 {
    return Err(identity_error("gc_directory_identity", "GC directory database/catalog identity is zero or body is truncated"));
  }
  let body = artifact.body;
  let level = u16_at(body, 0)?;
  let entry_count = u32_at(body, 4)?;
  if u32_at(body, 8)? != 0 || u32_at(body, 12)? != 0 || u32_at(body, 76)? != 0 {
    return Err(reserved_error("gc_directory_header", "GC directory reserve fields must be zero"));
  }
  let lower_length = usize::try_from(u32_at(body, 16)?).map_err(|_| length_error("GC directory lower length"))?;
  let upper_length = usize::try_from(u32_at(body, 20)?).map_err(|_| length_error("GC directory upper length"))?;
  let entries_length = usize::try_from(u32_at(body, 72)?).map_err(|_| length_error("GC directory entries length"))?;
  if level > 15
    || u16_at(body, 2)? != role as u16
    || entry_count == 0
    || entry_count > MAXIMUM_GC_DIRECTORY_ENTRIES_V1
    || lower_length == 0
    || lower_length > MAX_KEY_LENGTH
    || upper_length == 0
    || upper_length > MAX_KEY_LENGTH
  {
    return Err(closure_error("gc_directory_header", "GC directory role, count, or fence length is invalid"));
  }
  let descriptor_start = 80usize
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("GC directory descriptor start overflow"))?;
  if descriptor_start.checked_add(entries_length) != Some(body.len()) {
    return Err(trailing_error("gc_directory_header", "GC directory lengths do not consume body exactly"));
  }
  let lower_fence = &body[80..80 + lower_length];
  let upper_fence = &body[80 + lower_length..descriptor_start];
  compare_fences(algorithm, role, lower_fence, upper_fence)?;
  let entries_end = descriptor_start.checked_add(entries_length).ok_or_else(|| length_error("GC directory entries end overflow"))?;
  let mut cursor = descriptor_start;
  let mut entries: Vec<GcStateDirectoryEntryV1<'_>> = Vec::new();
  entries.try_reserve_exact(entry_count as usize).map_err(|error| {
    FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "gc_directory_entries",
      format!("could not reserve {entry_count} bounded descriptors: {error}"),
    )
  })?;
  for _ in 0..entry_count {
    let entry = if level == 0 {
      decode_gc_leaf_descriptor(algorithm, role, artifact.generation, body, &mut cursor, entries_end)?
    } else {
      decode_gc_internal_descriptor(algorithm, role, artifact.generation, body, &mut cursor, entries_end)?
    };
    if let Some(previous) = entries.last() {
      if compare_fence_values(algorithm, role, previous.upper_fence, entry.lower_fence)? != Ordering::Less {
        return Err(order_error("gc_directory_child_order", "GC directory child ranges overlap or are not strictly ordered"));
      }
    }
    entries.push(entry);
  }
  if cursor != entries_end
    || entries.first().map(|entry| entry.lower_fence) != Some(lower_fence)
    || entries.last().map(|entry| entry.upper_fence) != Some(upper_fence)
  {
    return Err(closure_error("gc_directory_descriptor_length", "GC directory descriptors do not close against outer fences"));
  }
  let live_count = checked_directory_sum(entries.iter().map(|entry| entry.live_count), "GC directory live count overflow")?;
  let tombstone_count = checked_directory_sum(entries.iter().map(|entry| entry.tombstone_count), "GC directory tombstone count overflow")?;
  let page_count = checked_directory_sum(entries.iter().map(|entry| entry.page_count), "GC directory page count overflow")?;
  let logical_bytes = checked_directory_sum(entries.iter().map(|entry| entry.logical_bytes), "GC directory logical bytes overflow")?;
  let minimum_page_id =
    entries.iter().map(|entry| entry.minimum_page_id).min().ok_or_else(|| closure_error("gc_directory_empty", "GC directory is empty"))?;
  let maximum_page_id =
    entries.iter().map(|entry| entry.maximum_page_id).max().ok_or_else(|| closure_error("gc_directory_empty", "GC directory is empty"))?;
  let page_shape_is_valid = if gc_directory_role_is_page_backed(role) {
    page_count > 0 && minimum_page_id > 0 && minimum_page_id <= maximum_page_id
  } else {
    page_count == 0 && minimum_page_id == 0 && maximum_page_id == 0
  };
  if u64_at(body, 24)? != live_count
    || u64_at(body, 32)? != tombstone_count
    || u64_at(body, 40)? != page_count
    || u64_at(body, 48)? != logical_bytes
    || u64_at(body, 56)? != minimum_page_id
    || u64_at(body, 64)? != maximum_page_id
    || tombstone_count != 0
    || !page_shape_is_valid
  {
    return Err(closure_error("gc_directory_aggregate", "GC directory aggregate fields disagree with child descriptors"));
  }
  Ok(GcStateDirectoryV1 {
    role,
    database_id,
    catalog_id,
    generation: artifact.generation,
    level,
    lower_fence,
    upper_fence,
    live_count,
    tombstone_count,
    page_count,
    logical_bytes,
    minimum_page_id,
    maximum_page_id,
    entries,
    key,
  })
}

fn decode_gc_leaf_descriptor<'a>(
  algorithm: HashAlgorithm,
  role: GcDirectoryRoleV1,
  parent_generation: u64,
  body: &'a [u8],
  cursor: &mut usize,
  end: usize,
) -> FormatResult<GcStateDirectoryEntryV1<'a>> {
  let hash_width = algorithm.hash_length();
  let fixed_length = 72usize.checked_add(hash_width).ok_or_else(|| length_error("GC leaf descriptor width overflow"))?;
  let start = *cursor;
  if start.checked_add(fixed_length).is_none_or(|next| next > end) {
    return Err(trailing_error("gc_directory_leaf_length", "GC leaf descriptor is truncated"));
  }
  let lower_length = usize::try_from(u32_at(body, start)?).map_err(|_| length_error("GC leaf lower fence length"))?;
  let upper_length = usize::try_from(u32_at(body, start + 4)?).map_err(|_| length_error("GC leaf upper fence length"))?;
  if lower_length == 0 || lower_length > MAX_KEY_LENGTH || upper_length == 0 || upper_length > MAX_KEY_LENGTH {
    return Err(amplification_error("gc_directory_leaf_fence", lower_length.max(upper_length), MAX_KEY_LENGTH));
  }
  let page_id = u64_at(body, start + 8)?;
  let child_hash = &body[start + 16..start + 16 + hash_width];
  let fields = start + 16 + hash_width;
  let child_generation = u64_at(body, fields)?;
  let live_count = u64_at(body, fields + 8)?;
  let tombstone_count = u64_at(body, fields + 16)?;
  let logical_bytes = u64_at(body, fields + 24)?;
  let physical_hint = decode_gc_physical_hint(body, fields + 32)?;
  let fences_start = start + fixed_length;
  let next = fences_start
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("GC leaf descriptor fence end overflow"))?;
  if next > end {
    return Err(trailing_error("gc_directory_leaf_length", "GC leaf descriptor fences are truncated"));
  }
  let lower_fence = &body[fences_start..fences_start + lower_length];
  let upper_fence = &body[fences_start + lower_length..next];
  compare_fences(algorithm, role, lower_fence, upper_fence)?;
  let page_shape_is_valid = if gc_directory_role_is_page_backed(role) { page_id > 0 } else { page_id == 0 };
  if !page_shape_is_valid
    || all_zero(child_hash)
    || child_generation == 0
    || child_generation > parent_generation
    || live_count == 0
    || tombstone_count != 0
    || logical_bytes == 0
  {
    return Err(closure_error("gc_directory_leaf", "GC leaf descriptor identity, generation, counts, or bytes are invalid"));
  }
  *cursor = next;
  Ok(GcStateDirectoryEntryV1 {
    lower_fence,
    upper_fence,
    child_hash,
    child_generation,
    live_count,
    tombstone_count,
    page_count: u64::from(page_shape_is_valid && page_id > 0),
    logical_bytes,
    minimum_page_id: page_id,
    maximum_page_id: page_id,
    physical_hint,
  })
}

fn decode_gc_internal_descriptor<'a>(
  algorithm: HashAlgorithm,
  role: GcDirectoryRoleV1,
  parent_generation: u64,
  body: &'a [u8],
  cursor: &mut usize,
  end: usize,
) -> FormatResult<GcStateDirectoryEntryV1<'a>> {
  let hash_width = algorithm.hash_length();
  let fixed_length = 88usize.checked_add(hash_width).ok_or_else(|| length_error("GC internal descriptor width overflow"))?;
  let start = *cursor;
  if start.checked_add(fixed_length).is_none_or(|next| next > end) {
    return Err(trailing_error("gc_directory_internal_length", "GC internal descriptor is truncated"));
  }
  let lower_length = usize::try_from(u32_at(body, start)?).map_err(|_| length_error("GC internal lower fence length"))?;
  let upper_length = usize::try_from(u32_at(body, start + 4)?).map_err(|_| length_error("GC internal upper fence length"))?;
  if lower_length == 0 || lower_length > MAX_KEY_LENGTH || upper_length == 0 || upper_length > MAX_KEY_LENGTH {
    return Err(amplification_error("gc_directory_internal_fence", lower_length.max(upper_length), MAX_KEY_LENGTH));
  }
  let child_hash = &body[start + 8..start + 8 + hash_width];
  let fields = start + 8 + hash_width;
  let child_generation = u64_at(body, fields)?;
  let live_count = u64_at(body, fields + 8)?;
  let tombstone_count = u64_at(body, fields + 16)?;
  let page_count = u64_at(body, fields + 24)?;
  let logical_bytes = u64_at(body, fields + 32)?;
  let minimum_page_id = u64_at(body, fields + 40)?;
  let maximum_page_id = u64_at(body, fields + 48)?;
  let physical_hint = decode_gc_physical_hint(body, fields + 56)?;
  let fences_start = start + fixed_length;
  let next = fences_start
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("GC internal descriptor fence end overflow"))?;
  if next > end {
    return Err(trailing_error("gc_directory_internal_length", "GC internal descriptor fences are truncated"));
  }
  let lower_fence = &body[fences_start..fences_start + lower_length];
  let upper_fence = &body[fences_start + lower_length..next];
  compare_fences(algorithm, role, lower_fence, upper_fence)?;
  let page_shape_is_valid = if gc_directory_role_is_page_backed(role) {
    page_count > 0 && minimum_page_id > 0 && minimum_page_id <= maximum_page_id
  } else {
    page_count == 0 && minimum_page_id == 0 && maximum_page_id == 0
  };
  if all_zero(child_hash)
    || child_generation == 0
    || child_generation > parent_generation
    || live_count == 0
    || tombstone_count != 0
    || logical_bytes == 0
    || !page_shape_is_valid
  {
    return Err(closure_error("gc_directory_internal", "GC internal descriptor identity, generation, ranks, or bytes are invalid"));
  }
  *cursor = next;
  Ok(GcStateDirectoryEntryV1 {
    lower_fence,
    upper_fence,
    child_hash,
    child_generation,
    live_count,
    tombstone_count,
    page_count,
    logical_bytes,
    minimum_page_id,
    maximum_page_id,
    physical_hint,
  })
}

fn decode_gc_physical_hint(body: &[u8], offset: usize) -> FormatResult<GcPhysicalHintV1> {
  if u32_at(body, offset + 12)? != 0 {
    return Err(reserved_error("gc_directory_physical_hint", "GC directory physical-hint reserve is nonzero"));
  }
  Ok(GcPhysicalHintV1 {
    wal_offset: u64_at(body, offset)?,
    total_length: u32_at(body, offset + 8)?,
    write_sequence: u64_at(body, offset + 16)?,
  })
}

fn checked_directory_sum(mut values: impl Iterator<Item = u64>, context: &'static str) -> FormatResult<u64> {
  values.try_fold(0u64, |total, value| total.checked_add(value).ok_or_else(|| length_error(context)))
}

fn decode_candidate_delta(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::CandidateDelta
    || artifact.identity.len() != 28
    || all_zero(&artifact.identity[..16])
    || u64_at(artifact.identity, 16)? != artifact.generation
    || u32_at(artifact.identity, 24)? == 0
    || artifact.body.len() < 16 + h
  {
    return Err(identity_error("candidate_delta_identity", "candidate delta identity/body is invalid"));
  }
  let body = artifact.body;
  if u32_at(body, 0)? != 0 || u16_at(body, 6)? != 0 {
    return Err(reserved_error("candidate_delta_header", "candidate delta reserve fields must be zero"));
  }
  let count = u32_at(body, 8)?;
  let records_length = usize::try_from(u32_at(body, 12)?).map_err(|_| length_error("candidate delta records length"))?;
  if u16_at(body, 4)? != 1 || count == 0 || 16usize.checked_add(h).and_then(|value| value.checked_add(records_length)) != Some(body.len()) {
    return Err(closure_error("candidate_delta_header", "candidate delta codec/count/length is invalid"));
  }
  let row_length = 52 + 2 * h;
  let framed_row_length = 4usize.checked_add(row_length).ok_or_else(|| length_error("candidate delta row width overflow"))?;
  if usize::try_from(count).ok().and_then(|value| value.checked_mul(framed_row_length)) != Some(records_length) {
    return Err(closure_error("candidate_delta_count", "candidate delta count does not match records length"));
  }
  let records = &body[16 + h..];
  let mut previous: Option<&[u8]> = None;
  for framed in records.chunks_exact(framed_row_length) {
    let operation = framed[0];
    if !matches!(operation, 1 | 2) {
      return Err(kind_error("candidate_delta_operation", format!("unknown candidate delta operation {operation}")));
    }
    if framed[1..4].iter().any(|byte| *byte != 0) {
      return Err(reserved_error("candidate_delta_operation", "candidate delta operation reserve bytes must be zero"));
    }
    let row = &framed[4..];
    validate_candidate_row(algorithm, row, operation == 2)?;
    if let Some(prior) = previous {
      if compare_physical_rows(algorithm, prior, row)? != Ordering::Less {
        return Err(order_error("candidate_delta_order", "candidate delta records are not strictly ordered"));
      }
    }
    previous = Some(row);
  }
  Ok(GcStateArtifactV1::CandidateDelta { record_count: count, key })
}

fn decode_manifest(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateManifestV1<'_>> {
  if bytes.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("gc_manifest_length", bytes.len(), MAX_MANIFEST_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  if artifact.identity.len() != 24 || all_zero(&artifact.identity[..16]) || u64_at(artifact.identity, 16)? != artifact.generation {
    return Err(identity_error("gc_manifest_identity", "GC manifest database/generation identity is invalid"));
  }
  let database_id = &artifact.identity[..16];
  let generation = artifact.generation;
  let (populated, record_count, secondary_count, primary_root, secondary_root) = match artifact.kind {
    GcArtifactKindV1::RootExpiryCatalogManifest => decode_root_expiry_manifest_body(artifact.body, algorithm)?,
    GcArtifactKindV1::RootLifecycleManifest => decode_root_lifecycle_manifest_body(artifact.body, algorithm, generation)?,
    GcArtifactKindV1::PhysicalInventoryManifest => {
      let inventory = decode_inventory_manifest_body(artifact.body, algorithm, generation)?;
      (inventory.populated(), inventory.record_count, 0, inventory.directory_root, None)
    }
    GcArtifactKindV1::QuarantineManifest => decode_quarantine_manifest_body(artifact.body, algorithm, generation)?,
    _ => return Err(kind_error("gc_manifest_kind", "artifact is not a GC lifecycle manifest")),
  };
  Ok(GcStateManifestV1 {
    kind: artifact.kind,
    database_id,
    generation,
    populated,
    record_count,
    secondary_count,
    primary_root,
    secondary_root,
    key,
  })
}

type ManifestBody<'a> = (bool, u64, u64, &'a [u8], Option<&'a [u8]>);

#[derive(Debug, Clone, Copy)]
struct PhysicalInventoryManifestBodyV1<'a> {
  completed_at_ms: u64,
  kv_layout_fingerprint: &'a [u8],
  audited_wal_offset: u64,
  audited_write_sequence: u64,
  retirement_journal_through_sequence: u64,
  directory_root: &'a [u8],
  next_page_id: u64,
  active_count: u64,
  retired_count: u64,
  orphan_count: u64,
  quarantined_count: u64,
  reclaimed_count: u64,
  inventoried_bytes: u64,
  record_count: u64,
}

impl PhysicalInventoryManifestBodyV1<'_> {
  fn populated(self) -> bool {
    !all_zero(self.directory_root)
  }
}

fn decode_root_expiry_manifest_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<ManifestBody<'_>> {
  let h = algorithm.hash_length();
  if body.len() != 124 + h || u32_at(body, 0)? != 0 || !valid_capabilities(&body[4..36], &[12, 17]) {
    return Err(closure_error("root_expiry_manifest_header", "root-expiry manifest shape/capabilities are invalid"));
  }
  if u64_at(body, 36)? == 0 || u64_at(body, 44)? == 0 {
    return Err(identity_error("root_expiry_manifest_header", "root-expiry policy values are zero"));
  }
  let root = &body[52..52 + h];
  let next = u64_at(body, 52 + h)?;
  let count = u64_at(body, 60 + h)?;
  let logical_bytes = u64_at(body, 68 + h)?;
  let mandatory_count = u64_at(body, 76 + h)?;
  let mandatory_bytes = u64_at(body, 84 + h)?;
  let optional_count = u64_at(body, 92 + h)?;
  let optional_bytes = u64_at(body, 100 + h)?;
  let oldest = i64_at(body, 108 + h)?;
  let newest = i64_at(body, 116 + h)?;
  let populated = !all_zero(root);
  if next == 0
    || mandatory_count.checked_add(optional_count) != Some(count)
    || mandatory_bytes.checked_add(optional_bytes) != Some(logical_bytes)
    || populated != (count != 0)
    || populated != (logical_bytes != 0)
    || (if populated { oldest <= 0 || newest <= 0 } else { oldest != 0 || newest != 0 })
    || oldest > newest
  {
    return Err(closure_error("root_expiry_manifest_state", "root-expiry manifest counts/times/root disagree"));
  }
  Ok((populated, count, mandatory_count, root, None))
}

fn decode_root_lifecycle_manifest_body(body: &[u8], algorithm: HashAlgorithm, generation: u64) -> FormatResult<ManifestBody<'_>> {
  let h = algorithm.hash_length();
  if body.len() != 108 + 3 * h
    || u32_at(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 17])
    || u64_at(body, 36)? != generation
    || i64_at(body, 44)? <= 0
    || u64_at(body, 52)? == 0
    || all_zero(&body[60..60 + h])
  {
    return Err(closure_error("root_lifecycle_manifest_header", "root-lifecycle manifest header/semantics are invalid"));
  }
  let candidate_root = &body[60 + h..60 + 2 * h];
  let expiry_root = &body[60 + 2 * h..60 + 3 * h];
  let next_page = u64_at(body, 60 + 3 * h)?;
  let candidate_count = u64_at(body, 68 + 3 * h)?;
  let pending_count = u64_at(body, 76 + 3 * h)?;
  let retired_count = u64_at(body, 84 + 3 * h)?;
  let candidate_bytes = u64_at(body, 92 + 3 * h)?;
  let expiry_bytes = u64_at(body, 100 + 3 * h)?;
  if next_page == 0
    || candidate_count != pending_count
    || all_zero(candidate_root) == (candidate_count != 0)
    || all_zero(expiry_root) == (retired_count != 0)
    || (candidate_count == 0) != (candidate_bytes == 0)
    || (retired_count == 0) != (expiry_bytes == 0)
  {
    return Err(closure_error("root_lifecycle_manifest_state", "root-lifecycle roots/counts/bytes disagree"));
  }
  Ok((candidate_count != 0 || retired_count != 0, candidate_count, retired_count, candidate_root, Some(expiry_root)))
}

fn decode_inventory_manifest_body(
  body: &[u8],
  algorithm: HashAlgorithm,
  generation: u64,
) -> FormatResult<PhysicalInventoryManifestBodyV1<'_>> {
  let h = algorithm.hash_length();
  if body.len() != 132 + 2 * h
    || u32_at(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 13])
    || u64_at(body, 36)? != generation
    || u64_at(body, 44)? == 0
    || all_zero(&body[52..52 + h])
    || u64_at(body, 52 + h)? == 0
    || u64_at(body, 60 + h)? == 0
    || u64_at(body, 68 + h)? > u64_at(body, 60 + h)?
    || u64_at(body, 76 + 2 * h)? == 0
  {
    return Err(closure_error("inventory_manifest_header", "inventory manifest header/capture state is invalid"));
  }
  let directory_root = &body[76 + h..76 + 2 * h];
  let active_count = u64_at(body, 84 + 2 * h)?;
  let retired_count = u64_at(body, 92 + 2 * h)?;
  let orphan_count = u64_at(body, 100 + 2 * h)?;
  let quarantined_count = u64_at(body, 108 + 2 * h)?;
  let reclaimed_count = u64_at(body, 116 + 2 * h)?;
  let record_count = [active_count, retired_count, orphan_count, quarantined_count, reclaimed_count]
    .into_iter()
    .try_fold(0u64, |total, count| total.checked_add(count).ok_or_else(|| length_error("inventory count overflow")))?;
  let inventoried_bytes = u64_at(body, 124 + 2 * h)?;
  let populated = !all_zero(directory_root);
  if populated != (record_count != 0) || populated != (inventoried_bytes != 0) {
    return Err(closure_error("inventory_manifest_state", "inventory root/count/bytes disagree"));
  }
  Ok(PhysicalInventoryManifestBodyV1 {
    completed_at_ms: u64_at(body, 44)?,
    kv_layout_fingerprint: &body[52..52 + h],
    audited_wal_offset: u64_at(body, 52 + h)?,
    audited_write_sequence: u64_at(body, 60 + h)?,
    retirement_journal_through_sequence: u64_at(body, 68 + h)?,
    directory_root,
    next_page_id: u64_at(body, 76 + 2 * h)?,
    active_count,
    retired_count,
    orphan_count,
    quarantined_count,
    reclaimed_count,
    inventoried_bytes,
    record_count,
  })
}

fn decode_quarantine_manifest_body(body: &[u8], algorithm: HashAlgorithm, generation: u64) -> FormatResult<ManifestBody<'_>> {
  let h = algorithm.hash_length();
  if body.len() < 100 + 6 * h || body.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("quarantine_manifest_length", body.len(), MAX_MANIFEST_LENGTH));
  }
  if u32_at(body, 0)? != 0
    || !valid_capabilities(&body[4..36], &[12, 13, 15, 17])
    || u64_at(body, 36)? != generation
    || u64_at(body, 44)? == 0
    || (0..4).any(|index| all_zero(&body[52 + index * h..52 + (index + 1) * h]))
  {
    return Err(closure_error("quarantine_manifest_header", "quarantine manifest capture authority is invalid"));
  }
  let root = &body[52 + 4 * h..52 + 5 * h];
  let captured_lifecycle = &body[52 + 5 * h..52 + 6 * h];
  if all_zero(captured_lifecycle) {
    return Err(identity_error("quarantine_manifest_header", "captured root-lifecycle manifest is zero"));
  }
  let delta_count = usize::try_from(u32_at(body, 52 + 6 * h)?).map_err(|_| length_error("quarantine delta count"))?;
  let count = u64_at(body, 60 + 6 * h)?;
  let bytes_count = u64_at(body, 68 + 6 * h)?;
  let eligible_count = u64_at(body, 76 + 6 * h)?;
  let eligible_bytes = u64_at(body, 84 + 6 * h)?;
  let next_page = u64_at(body, 92 + 6 * h)?;
  let deltas_start = 100 + 6 * h;
  if delta_count > MAX_DELTAS {
    return Err(amplification_error("quarantine_delta_count", delta_count, MAX_DELTAS));
  }
  if deltas_start.checked_add(delta_count.checked_mul(h).ok_or_else(|| length_error("quarantine delta bytes overflow"))?)
    != Some(body.len())
    || body[56 + 6 * h..60 + 6 * h].iter().any(|byte| *byte != 0)
  {
    return Err(reserved_error("quarantine_manifest_formula", "quarantine delta framing/reserve is invalid"));
  }
  if body[deltas_start..].chunks_exact(h).any(all_zero)
    || next_page == 0
    || eligible_count > count
    || eligible_bytes > bytes_count
    || (count != 0) != (bytes_count != 0)
    || (eligible_count == 0) != (eligible_bytes == 0)
    || (count != 0 && all_zero(root) && delta_count == 0)
  {
    return Err(closure_error("quarantine_manifest_state", "quarantine roots/deltas/counts/bytes disagree"));
  }
  Ok((count != 0, count, eligible_count, root, Some(captured_lifecycle)))
}

fn decode_root_retirement_commit(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  if bytes.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("root_retirement_length", bytes.len(), MAX_MANIFEST_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::RootRetirementCommit
    || artifact.identity.len() != 32 + h
    || all_zero(&artifact.identity[..16])
    || all_zero(&artifact.identity[16..16 + h])
    || all_zero(&artifact.identity[16 + h..])
    || artifact.body.len() != 72 + 4 * h
  {
    return Err(identity_error("root_retirement_shape", "root-retirement identity/body shape is invalid"));
  }
  let body = artifact.body;
  if body[..32 + h] != *artifact.identity {
    return Err(closure_error("root_retirement_identity", "root-retirement body does not repeat identity"));
  }
  if u16_at(body, 66 + h)? != 0 || u32_at(body, 68 + h)? != 0 {
    return Err(reserved_error("root_retirement_fields", "root-retirement reserve fields must be zero"));
  }
  let committed = i64_at(body, 32 + h)?;
  let pending = i64_at(body, 40 + h)?;
  let grace = u64_at(body, 48 + h)?;
  let grace = i64::try_from(grace).map_err(|_| length_error("root-retirement grace exceeds i64"))?;
  let eligible_at = pending.checked_add(grace).ok_or_else(|| length_error("root-retirement eligibility overflows i64"))?;
  let mark_generation = u64_at(body, 56 + h)?;
  if committed <= 0
    || pending <= 0
    || committed < eligible_at
    || mark_generation == 0
    || mark_generation != artifact.generation
    || u16_at(body, 64 + h)? == 0
    || (1..=3).any(|index| all_zero(&body[72 + index * h..72 + (index + 1) * h]))
  {
    return Err(closure_error("root_retirement_fields", "root-retirement timing, generation, reason, or evidence is invalid"));
  }
  Ok(GcStateArtifactV1::RootRetirementCommit { mark_generation, key })
}

fn decode_root_object_reclaim_proof(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  if bytes.len() > MAX_MANIFEST_LENGTH {
    return Err(amplification_error("root_reclaim_proof_length", bytes.len(), MAX_MANIFEST_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::RootObjectReclaimProof
    || artifact.identity.len() != 32 + h
    || all_zero(&artifact.identity[..16])
    || all_zero(&artifact.identity[16..16 + h])
    || all_zero(&artifact.identity[16 + h..])
    || artifact.body.len() != 40 + 6 * h
  {
    return Err(identity_error("root_reclaim_proof_shape", "root reclaim proof identity/body shape is invalid"));
  }
  let body = artifact.body;
  if body[..16] != artifact.identity[..16] || body[16..16 + h] != artifact.identity[16..16 + h] {
    return Err(closure_error("root_reclaim_proof_identity", "root reclaim proof body does not repeat database/root identity"));
  }
  let incarnation_count = u64_at(body, 24 + 4 * h)?;
  let receipt_count = u64_at(body, 32 + 5 * h)?;
  if all_zero(&body[16 + h..16 + 2 * h])
    || i64_at(body, 16 + 2 * h)? <= 0
    || all_zero(&body[24 + 2 * h..24 + 3 * h])
    || all_zero(&body[24 + 3 * h..24 + 4 * h])
    || incarnation_count == 0
    || all_zero(&body[32 + 4 * h..32 + 5 * h])
    || receipt_count == 0
    || all_zero(&body[40 + 5 * h..])
  {
    return Err(closure_error("root_reclaim_proof_fields", "root reclaim proof evidence/counts are invalid"));
  }
  Ok(GcStateArtifactV1::RootObjectReclaimProof { incarnation_count, receipt_count, key })
}

pub fn decode_retirement_journal_segment_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<RetirementJournalSegmentV1<'_>> {
  let key = immutable_gc_artifact_key(algorithm, GcArtifactKindV1::RetirementJournalSegment, bytes);
  decode_retirement_journal_segment_with_key(bytes, algorithm, key)
}

pub fn retirement_journal_records_v1<'a>(
  segment: &RetirementJournalSegmentV1<'a>,
  algorithm: HashAlgorithm,
) -> FormatResult<RetirementJournalRecordsV1<'a>> {
  let record_length = 72 + 4 * algorithm.hash_length();
  let expected_length = usize::try_from(segment.record_count)
    .ok()
    .and_then(|count| count.checked_mul(record_length))
    .ok_or_else(|| amplification_error("retirement_journal_count", segment.record_count as usize, MAX_PAGE_LENGTH / record_length))?;
  if segment.records.len() != expected_length {
    return Err(closure_error("retirement_journal_count", "retirement record count does not match its validated byte range"));
  }
  Ok(RetirementJournalRecordsV1 { records: segment.records.chunks_exact(record_length), algorithm })
}

fn decode_retirement_journal_record_v1(record: &[u8], algorithm: HashAlgorithm) -> FormatResult<RetirementJournalRecordV1<'_>> {
  let hash_width = algorithm.hash_length();
  let record_length = 72 + 4 * hash_width;
  if record.len() != record_length || usize::try_from(u32_at(record, 0)?).ok() != Some(record_length) || u16_at(record, 6)? != 0 {
    return Err(reserved_error("retirement_record_length", "retirement record length/reserve is invalid"));
  }
  let reason = RetirementReasonV1::from_u16(u16_at(record, 4)?)?;
  let replacement_publication_sequence = u64_at(record, 8)?;
  let retired_at_ms = u64_at(record, 16)?;
  let physical_length = 24 + 2 * hash_width;
  let old = decode_physical_incarnation(&record[24..24 + physical_length], algorithm)?;
  let replacement = decode_physical_incarnation(&record[24 + physical_length..], algorithm)?;
  if replacement_publication_sequence == 0 || retired_at_ms == 0 || old == replacement {
    return Err(closure_error("retirement_record_fields", "retirement record reason/time/incarnations are invalid"));
  }
  Ok(RetirementJournalRecordV1 { encoded: record, reason, replacement_publication_sequence, retired_at_ms, old, replacement })
}

fn decode_retirement_journal_segment_with_key(
  bytes: &[u8],
  algorithm: HashAlgorithm,
  key: Vec<u8>,
) -> FormatResult<RetirementJournalSegmentV1<'_>> {
  if bytes.len() > MAX_PAGE_LENGTH {
    return Err(amplification_error("retirement_journal_length", bytes.len(), MAX_PAGE_LENGTH));
  }
  let artifact = decode_gc_artifact_envelope(bytes)?;
  let h = algorithm.hash_length();
  if artifact.kind != GcArtifactKindV1::RetirementJournalSegment
    || artifact.identity.len() != 24
    || all_zero(&artifact.identity[..16])
    || u64_at(artifact.identity, 16)? == 0
    || artifact.body.len() < 32 + h
  {
    return Err(identity_error("retirement_journal_identity", "retirement journal identity/body is invalid"));
  }
  let body = artifact.body;
  let flags = u32_at(body, 0)?;
  if flags & !1 != 0 || u16_at(body, 6)? != 0 {
    return Err(reserved_error("retirement_journal_header", "retirement journal flags/reserve are invalid"));
  }
  let first = u64_at(body, 8)?;
  let last = u64_at(body, 16)?;
  let count = u32_at(body, 24)?;
  let records_length = usize::try_from(u32_at(body, 28)?).map_err(|_| length_error("retirement records length"))?;
  if u16_at(body, 4)? != 1
    || first == 0
    || first > last
    || count == 0
    || 32usize.checked_add(h).and_then(|value| value.checked_add(records_length)) != Some(body.len())
    || ((flags & 1 != 0) != all_zero(&body[32..32 + h]))
  {
    return Err(closure_error("retirement_journal_header", "retirement journal codec/range/reset/length is invalid"));
  }
  let record_length = 72 + 4 * h;
  if usize::try_from(count).ok().and_then(|value| value.checked_mul(record_length)) != Some(records_length) {
    return Err(closure_error("retirement_journal_count", "retirement record count does not match length"));
  }
  let previous_segment_hash = &body[32..32 + h];
  let records = &body[32 + h..];
  let mut first_observed = None;
  let mut previous: Option<(u64, &[u8])> = None;
  for record in records.chunks_exact(record_length) {
    let decoded = decode_retirement_journal_record_v1(record, algorithm)?;
    let sequence = decoded.replacement_publication_sequence;
    let physical_length = 24 + 2 * h;
    let old_bytes = &record[24..24 + physical_length];
    if let Some((prior_sequence, prior_old)) = previous {
      if prior_sequence > sequence
        || (prior_sequence == sequence && compare_physical_bytes(algorithm, prior_old, old_bytes)? != Ordering::Less)
      {
        return Err(order_error("retirement_record_order", "retirement records are not canonically ordered"));
      }
    }
    first_observed.get_or_insert(sequence);
    previous = Some((sequence, old_bytes));
  }
  if first_observed != Some(first) || previous.map(|(sequence, _)| sequence) != Some(last) {
    return Err(closure_error("retirement_journal_order", "retirement journal first/last sequence disagrees"));
  }
  Ok(RetirementJournalSegmentV1 {
    database_id: &artifact.identity[..16],
    segment_ordinal: u64_at(artifact.identity, 16)?,
    generation: artifact.generation,
    chain_reset: flags & 1 != 0,
    first_replacement_sequence: first,
    last_replacement_sequence: last,
    record_count: count,
    previous_segment_hash: (flags & 1 == 0).then_some(previous_segment_hash),
    records,
    key,
  })
}

fn decode_retirement_journal(bytes: &[u8], algorithm: HashAlgorithm, key: Vec<u8>) -> FormatResult<GcStateArtifactV1<'_>> {
  let segment = decode_retirement_journal_segment_with_key(bytes, algorithm, key)?;
  Ok(GcStateArtifactV1::RetirementJournal { record_count: segment.record_count, key: segment.key })
}

fn validate_row(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, row: &[u8], clear: bool) -> FormatResult<()> {
  match role {
    GcDirectoryRoleV1::Candidates => validate_candidate_row(algorithm, row, clear),
    GcDirectoryRoleV1::RootExpiry => validate_root_expiry_row(algorithm, row),
    GcDirectoryRoleV1::PhysicalInventory => validate_inventory_row(algorithm, row),
    GcDirectoryRoleV1::RootCandidates => validate_root_candidate_row(algorithm, row),
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => {
      Err(kind_error("gc_page_role", "specialized Void roles cannot use generic GC state rows"))
    }
  }
}

fn validate_candidate_row(algorithm: HashAlgorithm, row: &[u8], clear: bool) -> FormatResult<()> {
  let physical_length = 24 + 2 * algorithm.hash_length();
  if row.len() != 52 + 2 * algorithm.hash_length() {
    return Err(trailing_error("candidate_row_length", "candidate row has wrong fixed length"));
  }
  decode_physical_incarnation(&row[..physical_length], algorithm)?;
  let class = u16_at(row, physical_length)?;
  if !(1..=7).contains(&class) {
    return Err(kind_error("candidate_row_class", format!("unknown candidate class {class}")));
  }
  if u16_at(row, physical_length + 2)? != 0 {
    return Err(reserved_error("candidate_row_flags", "candidate row reserve must be zero"));
  }
  let pending = u64_at(row, physical_length + 4)?;
  let first_generation = u64_at(row, physical_length + 12)?;
  let grace = u64_at(row, physical_length + 20)?;
  if (clear && (pending != 0 || first_generation != 0 || grace != 0)) || (!clear && (pending == 0 || first_generation == 0)) {
    return Err(closure_error("candidate_row_state", "candidate set/clear state is invalid"));
  }
  Ok(())
}

fn validate_root_expiry_row(algorithm: HashAlgorithm, row: &[u8]) -> FormatResult<()> {
  decode_root_expiry_record_v1(row, algorithm).map(|_| ())
}

pub fn decode_root_expiry_record_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootExpiryRecordV1<'_>> {
  let hash_width = algorithm.hash_length();
  if row.len() != 40 + 3 * hash_width {
    return Err(trailing_error("root_expiry_row", "root-expiry row has wrong fixed length"));
  }
  if row[hash_width + 28..hash_width + 32].iter().any(|byte| *byte != 0) {
    return Err(reserved_error("root_expiry_row", "root-expiry row reserve must be zero"));
  }
  let retired_at_ms = i64_at(row, hash_width)?;
  let last_pending_since_ms = i64_at(row, hash_width + 8)?;
  let final_mark_generation = u64_at(row, hash_width + 16)?;
  let reason = u16_at(row, hash_width + 24)?;
  let state = row[hash_width + 26];
  let root_object_reclaim_proof = &row[hash_width + 32 + hash_width..hash_width + 32 + 2 * hash_width];
  let evidence_expires_at_ms = i64_at(row, hash_width + 32 + 2 * hash_width)?;
  if all_zero(&row[..hash_width])
    || retired_at_ms <= 0
    || last_pending_since_ms <= 0
    || last_pending_since_ms > retired_at_ms
    || final_mark_generation == 0
    || !matches!(state, 1 | 2)
    || all_zero(&row[hash_width + 32..hash_width + 32 + hash_width])
  {
    return Err(identity_error("root_expiry_row", "root-expiry identity/times/state are invalid"));
  }
  if !matches!(reason, root_retirement_reason_v1::ORDINARY_GC_UNREACHABLE | root_retirement_reason_v1::EXPLICIT_OPERATOR_RETIREMENT) {
    return Err(kind_error("root_expiry_reason", "root-expiry reason is outside RootRetirementReasonV1"));
  }
  let (state, root_object_reclaim_proof_hash, evidence_expires_at_ms) = match state {
    1 if row[hash_width + 27] == 0 && all_zero(root_object_reclaim_proof) && evidence_expires_at_ms == 0 => {
      (RootExpiryStateV1::LogicallyRetired, None, None)
    }
    2 if row[hash_width + 27] == 1 && !all_zero(root_object_reclaim_proof) && evidence_expires_at_ms >= retired_at_ms => {
      (RootExpiryStateV1::PhysicallyReclaimed, Some(root_object_reclaim_proof), Some(evidence_expires_at_ms))
    }
    _ => return Err(closure_error("root_expiry_row_state", "root-expiry pending/reclaimed evidence is inconsistent")),
  };
  Ok(RootExpiryRecordV1 {
    namespace_root_hash: &row[..hash_width],
    retired_at_ms,
    last_pending_since_ms,
    final_mark_generation,
    reason,
    state,
    retirement_commit_hash: &row[hash_width + 32..hash_width + 32 + hash_width],
    root_object_reclaim_proof_hash,
    evidence_expires_at_ms,
  })
}

fn validate_root_candidate_row(algorithm: HashAlgorithm, row: &[u8]) -> FormatResult<()> {
  decode_root_candidate_record_v1(row, algorithm).map(|_| ())
}

pub fn decode_root_candidate_record_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<RootCandidateRecordV1<'_>> {
  let hash_width = algorithm.hash_length();
  if row.len() != 36 + 3 * hash_width {
    return Err(trailing_error("root_candidate_row", "root-candidate row has wrong fixed length"));
  }
  if row[hash_width + 1] != 0 {
    return Err(reserved_error("root_candidate_row", "root-candidate reserve must be zero"));
  }
  let reason = u16_at(row, hash_width + 2)?;
  let pending_since_ms = i64_at(row, hash_width + 4)?;
  let first_unreachable_generation = u64_at(row, hash_width + 12)?;
  let last_confirmed_unreachable_generation = u64_at(row, hash_width + 20)?;
  let grace_at_pending_ms = u64_at(row, hash_width + 28)?;
  if all_zero(&row[..hash_width])
    || row[hash_width] != 1
    || pending_since_ms <= 0
    || first_unreachable_generation == 0
    || last_confirmed_unreachable_generation < first_unreachable_generation
    || all_zero(&row[hash_width + 36..hash_width + 36 + hash_width])
    || all_zero(&row[hash_width + 36 + hash_width..])
  {
    return Err(identity_error("root_candidate_row", "root-candidate state/identity/evidence is invalid"));
  }
  if !matches!(reason, root_retirement_reason_v1::ORDINARY_GC_UNREACHABLE | root_retirement_reason_v1::EXPLICIT_OPERATOR_RETIREMENT) {
    return Err(kind_error("root_candidate_reason", "root-candidate reason is outside RootRetirementReasonV1"));
  }
  Ok(RootCandidateRecordV1 {
    namespace_root_hash: &row[..hash_width],
    reason,
    pending_since_ms,
    first_unreachable_generation,
    last_confirmed_unreachable_generation,
    grace_at_pending_ms,
    authority_root_set_digest: &row[hash_width + 36..hash_width + 36 + hash_width],
    admission_commit_payload_hash: &row[hash_width + 36 + hash_width..],
  })
}

fn validate_inventory_row(algorithm: HashAlgorithm, row: &[u8]) -> FormatResult<()> {
  decode_physical_inventory_record_v1(row, algorithm).map(|_| ())
}

pub fn decode_physical_inventory_record_v1(row: &[u8], algorithm: HashAlgorithm) -> FormatResult<PhysicalInventoryRecordV1<'_>> {
  let h = algorithm.hash_length();
  let physical_length = 24 + 2 * h;
  if row.len() != 68 + 5 * h {
    return Err(trailing_error("inventory_row_length", "inventory row has wrong fixed length"));
  }
  let incarnation = decode_physical_incarnation(&row[..physical_length], algorithm)?;
  let state = row[physical_length];
  let reason = row[physical_length + 1];
  let flags = u16_at(row, physical_length + 2)?;
  if !(1..=5).contains(&state) || flags & !3 != 0 || (state == 1) != (reason == 0) {
    return Err(kind_error("inventory_row_state", "inventory state/reason/flags are invalid"));
  }
  let replacement = &row[physical_length + 4..physical_length + 4 + physical_length];
  let replacement = if flags & 1 != 0 {
    Some(decode_physical_incarnation(replacement, algorithm)?)
  } else if replacement.iter().any(|byte| *byte != 0) {
    return Err(closure_error("inventory_row_replacement", "replacement is present without replacement flag"));
  } else {
    None
  };
  let tail = physical_length + 4 + physical_length;
  let discovered_at_ms = u64_at(row, tail)?;
  let retirement_sequence = u64_at(row, tail + 8)?;
  if discovered_at_ms == 0 || (state == 1 && (retirement_sequence != 0 || flags != 0)) {
    return Err(closure_error("inventory_row_time_or_sequence", "inventory observed time/sequence is invalid"));
  }
  let receipt = &row[tail + 16..tail + 16 + h];
  if (flags & 2 != 0) == all_zero(receipt) || (state == 5) != (flags & 2 != 0) {
    return Err(closure_error("inventory_row_receipt", "inventory receipt/state flags disagree"));
  }
  Ok(PhysicalInventoryRecordV1 {
    encoded: row,
    incarnation,
    state: PhysicalInventoryStateV1(state),
    reason,
    replacement,
    discovered_at_ms,
    retirement_sequence: (retirement_sequence != 0).then_some(retirement_sequence),
    receipt_hash: (flags & 2 != 0).then_some(receipt),
  })
}

pub fn physical_inventory_records_v1<'a>(
  page: &GcStatePageV1<'a>,
  algorithm: HashAlgorithm,
) -> FormatResult<PhysicalInventoryRecordsV1<'a>> {
  if page.role != GcDirectoryRoleV1::PhysicalInventory {
    return Err(kind_error("physical_inventory_page_role", "GC state page is not a physical-inventory page"));
  }
  let row_length = row_length(algorithm, GcDirectoryRoleV1::PhysicalInventory);
  let expected_length = usize::try_from(page.record_count)
    .ok()
    .and_then(|count| count.checked_mul(row_length))
    .ok_or_else(|| amplification_error("physical_inventory_page_count", page.record_count as usize, MAX_PAGE_LENGTH / row_length))?;
  if expected_length != page.records.len() {
    return Err(closure_error("physical_inventory_page_count", "physical-inventory record count does not match its validated byte range"));
  }
  Ok(PhysicalInventoryRecordsV1 { rows: page.records.chunks_exact(row_length), algorithm })
}

fn compare_rows(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  match role {
    GcDirectoryRoleV1::Candidates => compare_physical_rows(algorithm, left, right),
    GcDirectoryRoleV1::RootExpiry | GcDirectoryRoleV1::RootCandidates => {
      Ok(left[..algorithm.hash_length()].cmp(&right[..algorithm.hash_length()]))
    }
    GcDirectoryRoleV1::PhysicalInventory => {
      let h = algorithm.hash_length();
      let physical_length = 24 + 2 * h;
      let left_physical = decode_physical_incarnation(&left[..physical_length], algorithm)?;
      let right_physical = decode_physical_incarnation(&right[..physical_length], algorithm)?;
      Ok(left_physical.wal_offset.cmp(&right_physical.wal_offset).then_with(|| compare_physical(&left_physical, &right_physical)))
    }
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => {
      Err(kind_error("gc_page_role", "specialized Void roles cannot use generic GC state rows"))
    }
  }
}

fn compare_physical_rows(algorithm: HashAlgorithm, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  let physical_length = 24 + 2 * algorithm.hash_length();
  compare_physical_bytes(algorithm, &left[..physical_length], &right[..physical_length])
}

fn compare_physical_bytes(algorithm: HashAlgorithm, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  let left = decode_physical_incarnation(left, algorithm)?;
  let right = decode_physical_incarnation(right, algorithm)?;
  Ok(compare_physical(&left, &right))
}

fn compare_physical(left: &PhysicalIncarnationV1<'_>, right: &PhysicalIncarnationV1<'_>) -> Ordering {
  compare_physical_incarnations_v1(left, right)
}

fn compare_fences(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, left: &[u8], right: &[u8]) -> FormatResult<()> {
  if compare_fence_values(algorithm, role, left, right)? == Ordering::Greater {
    return Err(order_error("gc_fence_order", "GC lower fence sorts after upper fence"));
  }
  Ok(())
}

fn compare_fence_values(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  Ok(match role {
    GcDirectoryRoleV1::Candidates => compare_physical_bytes(algorithm, left, right)?,
    GcDirectoryRoleV1::RootExpiry | GcDirectoryRoleV1::RootCandidates => {
      if left.len() != algorithm.hash_length() || right.len() != algorithm.hash_length() {
        return Err(closure_error("root_key_length", "root fence has wrong hash width"));
      }
      left.cmp(right)
    }
    GcDirectoryRoleV1::PhysicalInventory => {
      let physical_length = 24 + 2 * algorithm.hash_length();
      if left.len() != 8 + physical_length || right.len() != 8 + physical_length {
        return Err(closure_error("inventory_key_length", "inventory fence has wrong fixed width"));
      }
      let offset_order = u64_at(left, 0)?.cmp(&u64_at(right, 0)?);
      if offset_order == Ordering::Equal {
        compare_physical_bytes(algorithm, &left[8..], &right[8..])?
      } else {
        offset_order
      }
    }
    GcDirectoryRoleV1::FreeExtents => {
      if left.len() != 8 || right.len() != 8 {
        return Err(closure_error("void_extent_key_length", "Void extent fence must be one WAL offset"));
      }
      u64_at(left, 0)?.cmp(&u64_at(right, 0)?)
    }
    GcDirectoryRoleV1::Claims => {
      if left.len() != 16 || right.len() != 16 {
        return Err(closure_error("void_claim_key_length", "Void claim fence must be one claim ID"));
      }
      left.cmp(right)
    }
  })
}

fn row_key_equals_fence(algorithm: HashAlgorithm, role: GcDirectoryRoleV1, row: &[u8], fence: &[u8]) -> FormatResult<bool> {
  let h = algorithm.hash_length();
  match role {
    GcDirectoryRoleV1::Candidates => Ok(&row[..24 + 2 * h] == fence),
    GcDirectoryRoleV1::RootExpiry | GcDirectoryRoleV1::RootCandidates => Ok(&row[..h] == fence),
    GcDirectoryRoleV1::PhysicalInventory => {
      let physical_length = 24 + 2 * h;
      Ok(fence.len() == 8 + physical_length && fence[..8] == row[2 * h..2 * h + 8] && fence[8..] == row[..physical_length])
    }
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => {
      Err(kind_error("gc_page_role", "specialized Void roles cannot use generic GC state rows"))
    }
  }
}

fn row_length(algorithm: HashAlgorithm, role: GcDirectoryRoleV1) -> usize {
  let h = algorithm.hash_length();
  match role {
    GcDirectoryRoleV1::Candidates => 52 + 2 * h,
    GcDirectoryRoleV1::RootExpiry => 40 + 3 * h,
    GcDirectoryRoleV1::PhysicalInventory => 68 + 5 * h,
    GcDirectoryRoleV1::RootCandidates => 36 + 3 * h,
    GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => 0,
  }
}

fn manifest_summary(manifest: &GcStateManifestV1<'_>) -> String {
  let state = if manifest.populated { "populated" } else { "empty" };
  match manifest.kind {
    GcArtifactKindV1::QuarantineManifest => format!("gc:manifest:quarantine:{state}:candidates={}", manifest.record_count),
    GcArtifactKindV1::RootExpiryCatalogManifest => format!("gc:manifest:root-expiry:{state}:records={}", manifest.record_count),
    GcArtifactKindV1::PhysicalInventoryManifest => {
      format!("gc:manifest:physical-inventory:{state}:records={}", manifest.record_count)
    }
    GcArtifactKindV1::RootLifecycleManifest => {
      format!("gc:manifest:root-lifecycle:{state}:candidates={}:retired={}", manifest.record_count, manifest.secondary_count)
    }
    _ => unreachable!("GcStateManifestV1 only represents lifecycle manifests"),
  }
}

fn valid_capabilities(actual: &[u8], bits: &[usize]) -> bool {
  if actual.len() != 32 {
    return false;
  }
  let mut expected = [0u8; 32];
  for bit in bits {
    expected[bit / 8] |= 1 << (bit % 8);
  }
  actual == expected
}

fn i64_at(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let raw =
    bytes.get(offset..offset + 8).ok_or_else(|| trailing_error("gc_artifact_truncated", format!("i64 at offset {offset} is truncated")))?;
  Ok(i64::from_le_bytes(raw.try_into().expect("checked GC i64 width")))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn amplification_error(code: &'static str, actual: usize, cap: usize) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, code, format!("{actual} exceeds cap {cap}"))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "gc_state_overflow", context)
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

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
