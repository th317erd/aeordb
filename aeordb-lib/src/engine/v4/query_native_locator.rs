//! Inactive native composition for authorized query-hit locators and ranges.
//!
//! File identities enter through retained selected-root query results. The
//! captured native query workspace restores the exact FileRecord row, and the
//! shared selected-body reader remains the sole chunk/content authority.

use std::mem::size_of;
use std::slice;

use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner, MemoryReservation};

use super::hash::digest_parts;
use super::locator_range::{
  ExactByteRangeV1, ExactSourceRangeContinuationV1, ExactSourceRangeLimitsV1, ExactSourceRangeSelectorV1, LineColumnRangeV1,
  LocatedSourceMatchV1, LocatorMatchSemanticsV1, LocatorRangeErrorClassV1, LocatorRangeErrorV1, LocatorScanLimitsV1, LocatorScanV1,
  UnicodeScalarRangeV1, locate_source_matches_v1, read_exact_source_range_v1,
};
use super::query_executor::{QueryExecutionMatchV1, QueryExecutionSourceErrorClassV1, QueryExecutionSourceErrorV1, RootAwareQueryExecutionV1};
use super::query_native_source::{NativeAuthoritativeFieldPartitionSourceV1, map_workspace_error, path_is_within};
use super::query_native_workspace::NativeQueryOrderingLookupV1;
use super::query_scope_execution::{QueryExactScopeExecutionV1, QueryExactScopeIdentityIterV1};
use super::read_view_native::{
  NativeSelectedFileBodyLimitsV1, NativeSelectedNamespaceFileRowV1, NativeSelectedNamespaceReadErrorClassV1,
  NativeSelectedNamespaceReadErrorV1, NativeSelectedNamespaceReaderV1,
};

const MAXIMUM_QUERY_HITS_V1: usize = 1_024;
const MAXIMUM_QUERY_BODY_BYTES_V1: u64 = 256 * 1_024 * 1_024;
const MAXIMUM_QUERY_BODY_CHUNKS_V1: usize = 65_536;
const MAXIMUM_QUERY_RETAINED_BYTES_V1: u64 = 256 * 1_024 * 1_024;
const MAXIMUM_QUERY_LOCATOR_MATCHES_V1: usize = 1_024;
const MAXIMUM_QUERY_LOCATOR_SCAN_BYTES_V1: u64 = 256 * 1_024 * 1_024;
const MAXIMUM_QUERY_LOCATOR_LITERAL_BYTES_V1: usize = 1_024 * 1_024;
const MAXIMUM_QUERY_RANGE_OUTPUT_BYTES_PER_HIT_V1: u64 = 16 * 1_024 * 1_024;
const MAXIMUM_QUERY_RANGE_OUTPUT_BYTES_TOTAL_V1: u64 = 256 * 1_024 * 1_024;
const QUERY_HIT_RESULT_FIXED_BYTES_V1: u64 = 4 * 1_024;
const QUERY_HIT_TRANSIENT_FIXED_BYTES_V1: u64 = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeQueryHitReadErrorClassV1 {
  InvalidRequest,
  InvalidUtf8,
  AssertionFailed,
  ResourceLimit,
  HistoricalViewUnavailable,
  CorruptSource,
  Cancelled,
  Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeQueryHitReadErrorV1 {
  class: NativeQueryHitReadErrorClassV1,
  code: &'static str,
  context: String,
}

impl NativeQueryHitReadErrorV1 {
  pub const fn class(&self) -> NativeQueryHitReadErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl std::fmt::Display for NativeQueryHitReadErrorV1 {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl std::error::Error for NativeQueryHitReadErrorV1 {}

/// A borrowed file identity that can only be produced from an exact retained
/// query result. Synthetic aggregate identities have no constructor here.
#[derive(Clone, Copy)]
pub struct NativeAuthorizedQueryFileHitV1<'execution> {
  source: &'execution NativeAuthoritativeFieldPartitionSourceV1,
  selected_namespace_root: &'execution [u8],
  file_key: &'execution [u8],
  record_revision: &'execution [u8],
}

impl std::fmt::Debug for NativeAuthorizedQueryFileHitV1<'_> {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("NativeAuthorizedQueryFileHitV1")
      .field("selected_namespace_root", &hex::encode(self.selected_namespace_root))
      .field("file_key", &hex::encode(self.file_key))
      .field("record_revision", &hex::encode(self.record_revision))
      .finish_non_exhaustive()
  }
}

impl PartialEq for NativeAuthorizedQueryFileHitV1<'_> {
  fn eq(&self, other: &Self) -> bool {
    std::ptr::eq(self.source, other.source)
      && self.selected_namespace_root == other.selected_namespace_root
      && self.file_key == other.file_key
      && self.record_revision == other.record_revision
  }
}

impl Eq for NativeAuthorizedQueryFileHitV1<'_> {}

impl NativeAuthorizedQueryFileHitV1<'_> {
  pub const fn selected_namespace_root(&self) -> &[u8] {
    self.selected_namespace_root
  }

  pub const fn file_key(&self) -> &[u8] {
    self.file_key
  }

  pub const fn record_revision(&self) -> &[u8] {
    self.record_revision
  }
}

pub enum NativeAuthorizedQueryFileHitIterV1<'execution> {
  Query {
    source: &'execution NativeAuthoritativeFieldPartitionSourceV1,
    selected_namespace_root: &'execution [u8],
    rows: slice::Iter<'execution, QueryExecutionMatchV1>,
  },
  ExactScope {
    source: &'execution NativeAuthoritativeFieldPartitionSourceV1,
    selected_namespace_root: &'execution [u8],
    rows: QueryExactScopeIdentityIterV1<'execution>,
  },
}

impl<'execution> Iterator for NativeAuthorizedQueryFileHitIterV1<'execution> {
  type Item = NativeAuthorizedQueryFileHitV1<'execution>;

  fn next(&mut self) -> Option<Self::Item> {
    match self {
      Self::Query { source, selected_namespace_root, rows } => rows.next().map(|row| NativeAuthorizedQueryFileHitV1 {
        source,
        selected_namespace_root,
        file_key: row.file_key(),
        record_revision: row.record_revision(),
      }),
      Self::ExactScope { source, selected_namespace_root, rows } => rows.next().map(|row| NativeAuthorizedQueryFileHitV1 {
        source,
        selected_namespace_root,
        file_key: row.file_key(),
        record_revision: row.record_revision(),
      }),
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let length = match self {
      Self::Query { rows, .. } => rows.len(),
      Self::ExactScope { rows, .. } => rows.len(),
    };
    (length, Some(length))
  }
}

impl ExactSizeIterator for NativeAuthorizedQueryFileHitIterV1<'_> {}

pub fn authorized_query_execution_file_hits_v1<'execution>(
  source: &'execution NativeAuthoritativeFieldPartitionSourceV1,
  execution: &'execution RootAwareQueryExecutionV1,
) -> NativeAuthorizedQueryFileHitIterV1<'execution> {
  NativeAuthorizedQueryFileHitIterV1::Query {
    source,
    selected_namespace_root: execution.selected_namespace_root(),
    rows: execution.matches().iter(),
  }
}

pub fn authorized_exact_scope_file_hits_v1<'execution>(
  source: &'execution NativeAuthoritativeFieldPartitionSourceV1,
  execution: &'execution QueryExactScopeExecutionV1,
) -> NativeAuthorizedQueryFileHitIterV1<'execution> {
  NativeAuthorizedQueryFileHitIterV1::ExactScope {
    source,
    selected_namespace_root: execution.selected_namespace_root(),
    rows: execution.identities(),
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeQueryContentAssertionsV1<'request> {
  if_content_hash: Option<&'request [u8]>,
  if_updated_at: Option<i64>,
}

impl<'request> NativeQueryContentAssertionsV1<'request> {
  pub const fn new(if_content_hash: Option<&'request [u8]>, if_updated_at: Option<i64>) -> Self {
    Self { if_content_hash, if_updated_at }
  }

  pub const fn none() -> Self {
    Self { if_content_hash: None, if_updated_at: None }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeQueryHitSourceLimitsV1 {
  maximum_hits: usize,
  maximum_body_bytes_per_hit: u64,
  maximum_total_body_bytes: u64,
  maximum_chunks_per_hit: usize,
  maximum_retained_bytes: u64,
}

impl NativeQueryHitSourceLimitsV1 {
  pub fn new(
    maximum_hits: usize,
    maximum_body_bytes_per_hit: u64,
    maximum_total_body_bytes: u64,
    maximum_chunks_per_hit: usize,
    maximum_retained_bytes: u64,
  ) -> Result<Self, NativeQueryHitReadErrorV1> {
    if maximum_hits == 0
      || maximum_hits > MAXIMUM_QUERY_HITS_V1
      || maximum_body_bytes_per_hit == 0
      || maximum_body_bytes_per_hit > MAXIMUM_QUERY_BODY_BYTES_V1
      || maximum_total_body_bytes == 0
      || maximum_total_body_bytes > MAXIMUM_QUERY_BODY_BYTES_V1
      || maximum_body_bytes_per_hit > maximum_total_body_bytes
      || maximum_chunks_per_hit == 0
      || maximum_chunks_per_hit > MAXIMUM_QUERY_BODY_CHUNKS_V1
      || maximum_retained_bytes == 0
      || maximum_retained_bytes > MAXIMUM_QUERY_RETAINED_BYTES_V1
    {
      return Err(invalid(
        "native_query_hit_source_limits",
        "query-hit count, body, chunk, total-body, and retained limits must be nonzero, ordered, and within protocol maxima",
      ));
    }
    Ok(Self { maximum_hits, maximum_body_bytes_per_hit, maximum_total_body_bytes, maximum_chunks_per_hit, maximum_retained_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeQueryHitLocatorLimitsV1 {
  source: NativeQueryHitSourceLimitsV1,
  maximum_matches_per_hit: usize,
  maximum_total_matches: usize,
  maximum_scanned_bytes_per_hit: u64,
  maximum_total_scanned_bytes: u64,
  maximum_literal_bytes_per_hit: usize,
}

impl NativeQueryHitLocatorLimitsV1 {
  pub fn new(
    source: NativeQueryHitSourceLimitsV1,
    maximum_matches_per_hit: usize,
    maximum_total_matches: usize,
    maximum_scanned_bytes_per_hit: u64,
    maximum_total_scanned_bytes: u64,
    maximum_literal_bytes_per_hit: usize,
  ) -> Result<Self, NativeQueryHitReadErrorV1> {
    if maximum_matches_per_hit == 0
      || maximum_matches_per_hit > MAXIMUM_QUERY_LOCATOR_MATCHES_V1
      || maximum_total_matches == 0
      || maximum_total_matches > MAXIMUM_QUERY_LOCATOR_MATCHES_V1
      || maximum_matches_per_hit > maximum_total_matches
      || maximum_scanned_bytes_per_hit == 0
      || maximum_scanned_bytes_per_hit > MAXIMUM_QUERY_LOCATOR_SCAN_BYTES_V1
      || maximum_total_scanned_bytes == 0
      || maximum_total_scanned_bytes > MAXIMUM_QUERY_LOCATOR_SCAN_BYTES_V1
      || maximum_scanned_bytes_per_hit > maximum_total_scanned_bytes
      || maximum_literal_bytes_per_hit == 0
      || maximum_literal_bytes_per_hit > MAXIMUM_QUERY_LOCATOR_LITERAL_BYTES_V1
    {
      return Err(invalid(
        "native_query_hit_locator_limits",
        "query-hit locator match, scan, total, and literal limits must be nonzero, ordered, and within protocol maxima",
      ));
    }
    Ok(Self {
      source,
      maximum_matches_per_hit,
      maximum_total_matches,
      maximum_scanned_bytes_per_hit,
      maximum_total_scanned_bytes,
      maximum_literal_bytes_per_hit,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeQueryHitRangeLimitsV1 {
  source: NativeQueryHitSourceLimitsV1,
  maximum_output_bytes_per_hit: u64,
  maximum_total_output_bytes: u64,
}

impl NativeQueryHitRangeLimitsV1 {
  pub fn new(
    source: NativeQueryHitSourceLimitsV1,
    maximum_output_bytes_per_hit: u64,
    maximum_total_output_bytes: u64,
  ) -> Result<Self, NativeQueryHitReadErrorV1> {
    if maximum_output_bytes_per_hit == 0
      || maximum_output_bytes_per_hit > MAXIMUM_QUERY_RANGE_OUTPUT_BYTES_PER_HIT_V1
      || maximum_total_output_bytes == 0
      || maximum_total_output_bytes > MAXIMUM_QUERY_RANGE_OUTPUT_BYTES_TOTAL_V1
      || maximum_output_bytes_per_hit > maximum_total_output_bytes
    {
      return Err(invalid(
        "native_query_hit_range_limits",
        "query-hit range per-hit and total output limits must be nonzero, ordered, and within protocol maxima",
      ));
    }
    Ok(Self { source, maximum_output_bytes_per_hit, maximum_total_output_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeQueryHitLocatorRequestV1<'execution, 'request> {
  hit: NativeAuthorizedQueryFileHitV1<'execution>,
  literal: &'request [u8],
  matching_semantics: LocatorMatchSemanticsV1,
  start_byte: u64,
  assertions: NativeQueryContentAssertionsV1<'request>,
}

impl<'execution, 'request> NativeQueryHitLocatorRequestV1<'execution, 'request> {
  pub const fn new(
    hit: NativeAuthorizedQueryFileHitV1<'execution>,
    literal: &'request [u8],
    matching_semantics: LocatorMatchSemanticsV1,
    start_byte: u64,
    assertions: NativeQueryContentAssertionsV1<'request>,
  ) -> Self {
    Self { hit, literal, matching_semantics, start_byte, assertions }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeQueryHitRangeRequestV1<'execution, 'request> {
  hit: NativeAuthorizedQueryFileHitV1<'execution>,
  selector: ExactSourceRangeSelectorV1,
  assertions: NativeQueryContentAssertionsV1<'request>,
}

impl<'execution, 'request> NativeQueryHitRangeRequestV1<'execution, 'request> {
  pub const fn new(
    hit: NativeAuthorizedQueryFileHitV1<'execution>,
    selector: ExactSourceRangeSelectorV1,
    assertions: NativeQueryContentAssertionsV1<'request>,
  ) -> Self {
    Self { hit, selector, assertions }
  }
}

pub struct NativeQueryHitLocatorV1<'execution> {
  hit: NativeAuthorizedQueryFileHitV1<'execution>,
  path: String,
  content_hash: Vec<u8>,
  updated_at: i64,
  scan: LocatorScanV1,
}

impl NativeQueryHitLocatorV1<'_> {
  pub const fn hit(&self) -> NativeAuthorizedQueryFileHitV1<'_> {
    self.hit
  }

  pub fn path(&self) -> &str {
    &self.path
  }

  pub fn content_hash(&self) -> &[u8] {
    &self.content_hash
  }

  pub const fn updated_at(&self) -> i64 {
    self.updated_at
  }

  pub const fn scan(&self) -> &LocatorScanV1 {
    &self.scan
  }
}

pub struct NativeQueryHitLocatorsV1<'execution> {
  selected_namespace_root: &'execution [u8],
  hits: Vec<NativeQueryHitLocatorV1<'execution>>,
  total_body_bytes: u64,
  total_scanned_bytes: u64,
  total_match_count: usize,
  _memory: MemoryReservation,
}

impl NativeQueryHitLocatorsV1<'_> {
  pub const fn selected_namespace_root(&self) -> &[u8] {
    self.selected_namespace_root
  }

  pub fn hits(&self) -> &[NativeQueryHitLocatorV1<'_>] {
    &self.hits
  }

  pub const fn total_body_bytes(&self) -> u64 {
    self.total_body_bytes
  }

  pub const fn total_scanned_bytes(&self) -> u64 {
    self.total_scanned_bytes
  }

  pub const fn total_match_count(&self) -> usize {
    self.total_match_count
  }
}

pub struct NativeQueryHitRangeV1<'execution> {
  hit: NativeAuthorizedQueryFileHitV1<'execution>,
  path: String,
  content_hash: Vec<u8>,
  updated_at: i64,
  bytes: Vec<u8>,
  source_byte_range: ExactByteRangeV1,
  unicode_scalar_range: Option<UnicodeScalarRangeV1>,
  line_column_range: Option<LineColumnRangeV1>,
  continuation: Option<ExactSourceRangeContinuationV1>,
}

impl NativeQueryHitRangeV1<'_> {
  pub const fn hit(&self) -> NativeAuthorizedQueryFileHitV1<'_> {
    self.hit
  }

  pub fn path(&self) -> &str {
    &self.path
  }

  pub fn content_hash(&self) -> &[u8] {
    &self.content_hash
  }

  pub const fn updated_at(&self) -> i64 {
    self.updated_at
  }

  pub fn bytes(&self) -> &[u8] {
    &self.bytes
  }

  pub const fn source_byte_range(&self) -> ExactByteRangeV1 {
    self.source_byte_range
  }

  pub const fn unicode_scalar_range(&self) -> Option<UnicodeScalarRangeV1> {
    self.unicode_scalar_range
  }

  pub const fn line_column_range(&self) -> Option<LineColumnRangeV1> {
    self.line_column_range
  }

  pub const fn truncated(&self) -> bool {
    self.continuation.is_some()
  }

  pub const fn continuation(&self) -> Option<ExactSourceRangeContinuationV1> {
    self.continuation
  }
}

pub struct NativeQueryHitRangesV1<'execution> {
  selected_namespace_root: &'execution [u8],
  ranges: Vec<NativeQueryHitRangeV1<'execution>>,
  total_body_bytes: u64,
  total_output_bytes: u64,
  _memory: MemoryReservation,
}

impl NativeQueryHitRangesV1<'_> {
  pub const fn selected_namespace_root(&self) -> &[u8] {
    self.selected_namespace_root
  }

  pub fn ranges(&self) -> &[NativeQueryHitRangeV1<'_>] {
    &self.ranges
  }

  pub const fn total_body_bytes(&self) -> u64 {
    self.total_body_bytes
  }

  pub const fn total_output_bytes(&self) -> u64 {
    self.total_output_bytes
  }
}

impl NativeAuthoritativeFieldPartitionSourceV1 {
  pub fn locate_authorized_query_hits_v1<'execution, 'request>(
    &self,
    requests: &[NativeQueryHitLocatorRequestV1<'execution, 'request>],
    cancellation: &CancellationToken,
    limits: NativeQueryHitLocatorLimitsV1,
  ) -> Result<NativeQueryHitLocatorsV1<'execution>, NativeQueryHitReadErrorV1> {
    validate_request_count(requests.len(), limits.source)?;
    require_not_cancelled(self, cancellation)?;
    for request in requests {
      validate_hit(self, request.hit)?;
      if request.literal.is_empty() || request.literal.len() > limits.maximum_literal_bytes_per_hit {
        return Err(invalid(
          "native_query_hit_locator_literal",
          "query-hit locator literal must be nonempty and remain within its per-hit byte limit",
        ));
      }
    }

    let mut memory = reserve_result_memory(self, limits.source.maximum_retained_bytes, "native_query_hit_locator_memory")?;
    let mut hits = Vec::new();
    hits
      .try_reserve_exact(requests.len())
      .map_err(|error| resource("native_query_hit_locator_allocation", format!("cannot reserve query-hit locator results: {error}")))?;
    let mut retained_bytes = locator_result_base_bytes(hits.capacity())?;
    require_retained_bound(retained_bytes, limits.source.maximum_retained_bytes, "native_query_hit_locator_retained_bytes")?;
    let reader = self.open_query_hit_namespace_reader_v1().map_err(map_source_error)?;
    let mut lookup = self.open_query_hit_ordering_lookup_v1().map_err(map_source_error)?;
    let body_limits = NativeSelectedFileBodyLimitsV1::new(limits.source.maximum_body_bytes_per_hit, limits.source.maximum_chunks_per_hit)
      .map_err(map_native_read_error)?;
    let mut total_body_bytes = 0u64;
    let mut total_scanned_bytes = 0u64;
    let mut total_match_count = 0usize;

    for request in requests {
      require_not_cancelled(self, cancellation)?;
      let row = restore_query_hit_row(self, &reader, &mut lookup, request.hit, cancellation)?;
      validate_assertions(self, request.assertions)?;
      total_body_bytes = admit_body(&row, total_body_bytes, limits.source)?;
      let body = reader.read_file_body(&row, body_limits).map_err(map_native_read_error)?;
      require_not_cancelled(self, cancellation)?;
      let content_hash = digest_parts(self.query_hit_hash_algorithm_v1(), &[body.as_bytes()]);
      check_assertions(&row, &content_hash, request.assertions)?;
      let remaining_matches = limits
        .maximum_total_matches
        .checked_sub(total_match_count)
        .ok_or_else(|| resource("native_query_hit_locator_total_matches", "query-hit locator total match accounting underflowed"))?;
      let remaining_scan_bytes = limits
        .maximum_total_scanned_bytes
        .checked_sub(total_scanned_bytes)
        .ok_or_else(|| resource("native_query_hit_locator_total_scan", "query-hit locator total scan accounting underflowed"))?;
      if remaining_matches == 0 || remaining_scan_bytes == 0 {
        return Err(resource(
          "native_query_hit_locator_total_limit",
          "query-hit locator request-total match or scan budget was exhausted before all requested hits",
        ));
      }
      let maximum_matches = limits.maximum_matches_per_hit.min(remaining_matches);
      let maximum_scanned_bytes = limits.maximum_scanned_bytes_per_hit.min(remaining_scan_bytes);
      let prospective = locator_hit_upper_bytes(row.path().len(), content_hash.len(), maximum_matches)?;
      require_retained_bound(
        retained_bytes.checked_add(prospective).ok_or_else(retained_overflow)?,
        limits.source.maximum_retained_bytes,
        "native_query_hit_locator_retained_bytes",
      )?;
      let transient_bytes = checked_usize_bytes(request.literal.len(), size_of::<usize>(), "locator prefix workspace")?
        .checked_add(QUERY_HIT_TRANSIENT_FIXED_BYTES_V1)
        .ok_or_else(retained_overflow)?;
      let _transient = self
        .query_hit_memory_coordinator_v1()
        .reserve(MemoryOwner::Query, transient_bytes, AdmissionClass::Workload)
        .map_err(|error| resource("native_query_hit_locator_workspace_memory", error.to_string()))?;
      let scan = locate_source_matches_v1(
        body.as_bytes(),
        request.literal,
        request.matching_semantics,
        request.start_byte,
        LocatorScanLimitsV1::new(maximum_matches, maximum_scanned_bytes, limits.maximum_literal_bytes_per_hit)
          .map_err(map_locator_error)?,
      )
      .map_err(map_locator_error)?;
      require_not_cancelled(self, cancellation)?;
      total_scanned_bytes = total_scanned_bytes
        .checked_add(scan.scanned_byte_range().len())
        .ok_or_else(|| resource("native_query_hit_locator_total_scan", "query-hit locator total scanned bytes overflowed"))?;
      total_match_count = total_match_count
        .checked_add(scan.matches().len())
        .ok_or_else(|| resource("native_query_hit_locator_total_matches", "query-hit locator total matches overflowed"))?;
      let path = try_clone_string(row.path(), "query-hit locator path")?;
      let actual = locator_hit_actual_bytes(path.capacity(), content_hash.capacity(), scan.allocated_match_capacity_v1())?;
      retained_bytes = retained_bytes.checked_add(actual).ok_or_else(retained_overflow)?;
      require_retained_bound(retained_bytes, limits.source.maximum_retained_bytes, "native_query_hit_locator_retained_bytes")?;
      hits.push(NativeQueryHitLocatorV1 { hit: request.hit, path, content_hash, updated_at: row.file_record().updated_at, scan });
    }

    shrink_result_memory(&mut memory, retained_bytes, "native_query_hit_locator_memory_accounting")?;
    require_not_cancelled(self, cancellation)?;
    Ok(NativeQueryHitLocatorsV1 {
      selected_namespace_root: requests[0].hit.selected_namespace_root,
      hits,
      total_body_bytes,
      total_scanned_bytes,
      total_match_count,
      _memory: memory,
    })
  }

  pub fn read_authorized_query_hit_ranges_v1<'execution, 'request>(
    &self,
    requests: &[NativeQueryHitRangeRequestV1<'execution, 'request>],
    cancellation: &CancellationToken,
    limits: NativeQueryHitRangeLimitsV1,
  ) -> Result<NativeQueryHitRangesV1<'execution>, NativeQueryHitReadErrorV1> {
    validate_request_count(requests.len(), limits.source)?;
    require_not_cancelled(self, cancellation)?;
    for request in requests {
      validate_hit(self, request.hit)?;
    }

    let mut memory = reserve_result_memory(self, limits.source.maximum_retained_bytes, "native_query_hit_range_memory")?;
    let mut ranges = Vec::new();
    ranges
      .try_reserve_exact(requests.len())
      .map_err(|error| resource("native_query_hit_range_allocation", format!("cannot reserve query-hit range results: {error}")))?;
    let mut retained_bytes = range_result_base_bytes(ranges.capacity())?;
    require_retained_bound(retained_bytes, limits.source.maximum_retained_bytes, "native_query_hit_range_retained_bytes")?;
    let reader = self.open_query_hit_namespace_reader_v1().map_err(map_source_error)?;
    let mut lookup = self.open_query_hit_ordering_lookup_v1().map_err(map_source_error)?;
    let body_limits = NativeSelectedFileBodyLimitsV1::new(limits.source.maximum_body_bytes_per_hit, limits.source.maximum_chunks_per_hit)
      .map_err(map_native_read_error)?;
    let mut total_body_bytes = 0u64;
    let mut total_output_bytes = 0u64;

    for request in requests {
      require_not_cancelled(self, cancellation)?;
      let row = restore_query_hit_row(self, &reader, &mut lookup, request.hit, cancellation)?;
      validate_assertions(self, request.assertions)?;
      total_body_bytes = admit_body(&row, total_body_bytes, limits.source)?;
      let body = reader.read_file_body(&row, body_limits).map_err(map_native_read_error)?;
      require_not_cancelled(self, cancellation)?;
      let content_hash = digest_parts(self.query_hit_hash_algorithm_v1(), &[body.as_bytes()]);
      check_assertions(&row, &content_hash, request.assertions)?;
      let remaining_output_bytes = limits
        .maximum_total_output_bytes
        .checked_sub(total_output_bytes)
        .ok_or_else(|| resource("native_query_hit_range_total_output", "query-hit range total output accounting underflowed"))?;
      if remaining_output_bytes == 0 {
        return Err(resource(
          "native_query_hit_range_total_output",
          "query-hit range request-total output budget was exhausted before all requested hits",
        ));
      }
      let maximum_output_bytes = limits.maximum_output_bytes_per_hit.min(remaining_output_bytes);
      let selected = read_exact_source_range_v1(
        body.as_bytes(),
        request.selector,
        ExactSourceRangeLimitsV1::new(maximum_output_bytes).map_err(map_locator_error)?,
      )
      .map_err(map_locator_error)?;
      require_not_cancelled(self, cancellation)?;
      let output_length =
        u64::try_from(selected.bytes().len()).map_err(|error| resource("native_query_hit_range_platform_output", error.to_string()))?;
      total_output_bytes = total_output_bytes
        .checked_add(output_length)
        .ok_or_else(|| resource("native_query_hit_range_total_output", "query-hit range total output bytes overflowed"))?;
      let prospective = range_hit_upper_bytes(row.path().len(), content_hash.len(), selected.bytes().len())?;
      require_retained_bound(
        retained_bytes.checked_add(prospective).ok_or_else(retained_overflow)?,
        limits.source.maximum_retained_bytes,
        "native_query_hit_range_retained_bytes",
      )?;
      let path = try_clone_string(row.path(), "query-hit range path")?;
      let bytes = try_clone_bytes(selected.bytes(), "query-hit range bytes")?;
      let source_byte_range = selected.source_byte_range();
      let unicode_scalar_range = selected.unicode_scalar_range();
      let line_column_range = selected.line_column_range();
      let continuation = selected.continuation();
      let actual = range_hit_actual_bytes(path.capacity(), content_hash.capacity(), bytes.capacity())?;
      retained_bytes = retained_bytes.checked_add(actual).ok_or_else(retained_overflow)?;
      require_retained_bound(retained_bytes, limits.source.maximum_retained_bytes, "native_query_hit_range_retained_bytes")?;
      ranges.push(NativeQueryHitRangeV1 {
        hit: request.hit,
        path,
        content_hash,
        updated_at: row.file_record().updated_at,
        bytes,
        source_byte_range,
        unicode_scalar_range,
        line_column_range,
        continuation,
      });
    }

    shrink_result_memory(&mut memory, retained_bytes, "native_query_hit_range_memory_accounting")?;
    require_not_cancelled(self, cancellation)?;
    Ok(NativeQueryHitRangesV1 {
      selected_namespace_root: requests[0].hit.selected_namespace_root,
      ranges,
      total_body_bytes,
      total_output_bytes,
      _memory: memory,
    })
  }
}

fn validate_request_count(request_count: usize, limits: NativeQueryHitSourceLimitsV1) -> Result<(), NativeQueryHitReadErrorV1> {
  if request_count == 0 || request_count > limits.maximum_hits {
    return Err(invalid(
      "native_query_hit_request_count",
      "query-hit request count must be nonzero and remain within its admitted hit bound",
    ));
  }
  Ok(())
}

fn validate_hit(
  source: &NativeAuthoritativeFieldPartitionSourceV1,
  hit: NativeAuthorizedQueryFileHitV1<'_>,
) -> Result<(), NativeQueryHitReadErrorV1> {
  if !std::ptr::eq(hit.source, source) {
    return Err(corrupt("native_query_hit_source_authority", "authorized query hit was produced by a different native query source"));
  }
  let hash_width = source.query_hit_hash_algorithm_v1().hash_length();
  if hit.selected_namespace_root != source.query_hit_selected_namespace_root_v1() {
    return Err(corrupt("native_query_hit_root_authority", "authorized query hit does not bind this captured selected-root source"));
  }
  if hit.file_key.len() != hash_width
    || hit.record_revision.len() != hash_width
    || hit.file_key.iter().all(|byte| *byte == 0)
    || hit.record_revision.iter().all(|byte| *byte == 0)
  {
    return Err(corrupt("native_query_hit_identity", "authorized query hit FileKey or RecordRevision has the wrong width or is all zero"));
  }
  Ok(())
}

fn validate_assertions(
  source: &NativeAuthoritativeFieldPartitionSourceV1,
  assertions: NativeQueryContentAssertionsV1<'_>,
) -> Result<(), NativeQueryHitReadErrorV1> {
  if let Some(content_hash) = assertions.if_content_hash {
    if content_hash.len() != source.query_hit_hash_algorithm_v1().hash_length() || content_hash.iter().all(|byte| *byte == 0) {
      return Err(invalid("native_query_hit_content_hash_assertion", "optional content-hash assertion has the wrong width or is all zero"));
    }
  }
  Ok(())
}

fn restore_query_hit_row(
  source: &NativeAuthoritativeFieldPartitionSourceV1,
  reader: &NativeSelectedNamespaceReaderV1<'_>,
  lookup: &mut NativeQueryOrderingLookupV1,
  hit: NativeAuthorizedQueryFileHitV1<'_>,
  cancellation: &CancellationToken,
) -> Result<NativeSelectedNamespaceFileRowV1, NativeQueryHitReadErrorV1> {
  let ordered = lookup.find_row(hit.file_key, cancellation).map_err(map_workspace_error).map_err(map_source_error)?;
  let Some(ordered) = ordered else {
    return Err(corrupt("native_query_hit_workspace_absent", "authorized query hit FileKey is absent from the captured query workspace"));
  };
  if ordered.record_revision() != hit.record_revision {
    return Err(corrupt(
      "native_query_hit_revision_authority",
      "authorized query hit RecordRevision does not match the captured query workspace",
    ));
  }
  let row = reader
    .restore_ordered_file_row(ordered.file_key(), ordered.record_revision(), ordered.entity_version(), ordered.encoded_file_record())
    .map_err(map_native_read_error)?;
  if row.file_key() != hit.file_key
    || row.record_revision() != hit.record_revision
    || !path_is_within(source.query_hit_query_path_v1(), row.path())
  {
    return Err(corrupt(
      "native_query_hit_row_authority",
      "restored query-hit row does not match the exact authorized identity and query path",
    ));
  }
  Ok(row)
}

fn admit_body(
  row: &NativeSelectedNamespaceFileRowV1,
  total_body_bytes: u64,
  limits: NativeQueryHitSourceLimitsV1,
) -> Result<u64, NativeQueryHitReadErrorV1> {
  if row.file_record().total_size > limits.maximum_body_bytes_per_hit {
    return Err(resource("native_query_hit_body_bytes_per_hit", "authorized query-hit body exceeds its per-hit byte bound"));
  }
  if row.file_record().chunk_hashes.len() > limits.maximum_chunks_per_hit {
    return Err(resource("native_query_hit_body_chunks_per_hit", "authorized query-hit body exceeds its per-hit chunk bound"));
  }
  let next = total_body_bytes
    .checked_add(row.file_record().total_size)
    .ok_or_else(|| resource("native_query_hit_total_body_bytes", "query-hit total body bytes overflowed"))?;
  if next > limits.maximum_total_body_bytes {
    return Err(resource("native_query_hit_total_body_bytes", "authorized query-hit bodies exceed the request-total byte bound"));
  }
  Ok(next)
}

fn check_assertions(
  row: &NativeSelectedNamespaceFileRowV1,
  content_hash: &[u8],
  assertions: NativeQueryContentAssertionsV1<'_>,
) -> Result<(), NativeQueryHitReadErrorV1> {
  if let Some(expected) = assertions.if_content_hash {
    if expected != content_hash {
      return Err(assertion_failed(
        "native_query_hit_content_hash_mismatch",
        "authorized query-hit content does not match the optional content-hash assertion",
      ));
    }
  }
  if let Some(expected) = assertions.if_updated_at {
    if expected != row.file_record().updated_at {
      return Err(assertion_failed(
        "native_query_hit_updated_at_mismatch",
        "authorized query-hit metadata does not match the optional updated-at assertion",
      ));
    }
  }
  Ok(())
}

fn require_not_cancelled(
  source: &NativeAuthoritativeFieldPartitionSourceV1,
  cancellation: &CancellationToken,
) -> Result<(), NativeQueryHitReadErrorV1> {
  if cancellation.is_cancelled() || source.query_hit_view_cancellation_v1().is_cancelled() {
    Err(cancelled())
  } else {
    Ok(())
  }
}

fn reserve_result_memory(
  source: &NativeAuthoritativeFieldPartitionSourceV1,
  bytes: u64,
  code: &'static str,
) -> Result<MemoryReservation, NativeQueryHitReadErrorV1> {
  source
    .query_hit_memory_coordinator_v1()
    .reserve(MemoryOwner::Query, bytes, AdmissionClass::Workload)
    .map_err(|error| resource(code, error.to_string()))
}

fn shrink_result_memory(memory: &mut MemoryReservation, retained_bytes: u64, code: &'static str) -> Result<(), NativeQueryHitReadErrorV1> {
  let release = memory
    .bytes()
    .checked_sub(retained_bytes)
    .ok_or_else(|| resource(code, "query-hit result retained more bytes than its admitted reservation"))?;
  memory.shrink(release).map_err(|error| internal(code, error.to_string()))
}

fn locator_result_base_bytes(capacity: usize) -> Result<u64, NativeQueryHitReadErrorV1> {
  checked_usize_bytes(capacity, size_of::<NativeQueryHitLocatorV1<'static>>(), "locator result vector")?
    .checked_add(QUERY_HIT_RESULT_FIXED_BYTES_V1)
    .ok_or_else(retained_overflow)
}

fn range_result_base_bytes(capacity: usize) -> Result<u64, NativeQueryHitReadErrorV1> {
  checked_usize_bytes(capacity, size_of::<NativeQueryHitRangeV1<'static>>(), "range result vector")?
    .checked_add(QUERY_HIT_RESULT_FIXED_BYTES_V1)
    .ok_or_else(retained_overflow)
}

fn locator_hit_upper_bytes(path_bytes: usize, hash_bytes: usize, maximum_matches: usize) -> Result<u64, NativeQueryHitReadErrorV1> {
  locator_hit_actual_bytes(path_bytes, hash_bytes, maximum_matches)
}

fn locator_hit_actual_bytes(path_capacity: usize, hash_capacity: usize, match_capacity: usize) -> Result<u64, NativeQueryHitReadErrorV1> {
  let match_bytes = checked_usize_bytes(match_capacity, size_of::<LocatedSourceMatchV1>(), "locator matches")?;
  let path_bytes = usize_to_u64(path_capacity, "locator path")?;
  let hash_bytes = usize_to_u64(hash_capacity, "locator content hash")?;
  match_bytes.checked_add(path_bytes).and_then(|bytes| bytes.checked_add(hash_bytes)).ok_or_else(retained_overflow)
}

fn range_hit_upper_bytes(path_bytes: usize, hash_bytes: usize, output_bytes: usize) -> Result<u64, NativeQueryHitReadErrorV1> {
  range_hit_actual_bytes(path_bytes, hash_bytes, output_bytes)
}

fn range_hit_actual_bytes(path_capacity: usize, hash_capacity: usize, output_capacity: usize) -> Result<u64, NativeQueryHitReadErrorV1> {
  let path_bytes = usize_to_u64(path_capacity, "range path")?;
  let hash_bytes = usize_to_u64(hash_capacity, "range content hash")?;
  let output_bytes = usize_to_u64(output_capacity, "range output")?;
  path_bytes.checked_add(hash_bytes).and_then(|bytes| bytes.checked_add(output_bytes)).ok_or_else(retained_overflow)
}

fn checked_usize_bytes(count: usize, width: usize, role: &str) -> Result<u64, NativeQueryHitReadErrorV1> {
  let bytes =
    count.checked_mul(width).ok_or_else(|| resource("native_query_hit_retained_bytes", format!("{role} byte count overflowed")))?;
  usize_to_u64(bytes, role)
}

fn usize_to_u64(value: usize, role: &str) -> Result<u64, NativeQueryHitReadErrorV1> {
  u64::try_from(value).map_err(|error| resource("native_query_hit_retained_bytes", format!("{role} does not fit accounting: {error}")))
}

fn require_retained_bound(bytes: u64, maximum: u64, code: &'static str) -> Result<(), NativeQueryHitReadErrorV1> {
  if bytes > maximum {
    Err(resource(code, "query-hit retained result exceeds its admitted byte bound"))
  } else {
    Ok(())
  }
}

fn try_clone_string(value: &str, role: &str) -> Result<String, NativeQueryHitReadErrorV1> {
  let mut retained = String::new();
  retained
    .try_reserve_exact(value.len())
    .map_err(|error| resource("native_query_hit_allocation", format!("cannot retain {role}: {error}")))?;
  retained.push_str(value);
  Ok(retained)
}

fn try_clone_bytes(value: &[u8], role: &str) -> Result<Vec<u8>, NativeQueryHitReadErrorV1> {
  let mut retained = Vec::new();
  retained
    .try_reserve_exact(value.len())
    .map_err(|error| resource("native_query_hit_allocation", format!("cannot retain {role}: {error}")))?;
  retained.extend_from_slice(value);
  Ok(retained)
}

fn map_native_read_error(error: NativeSelectedNamespaceReadErrorV1) -> NativeQueryHitReadErrorV1 {
  let class = match error.class() {
    NativeSelectedNamespaceReadErrorClassV1::InvalidRequest | NativeSelectedNamespaceReadErrorClassV1::Corrupt => {
      NativeQueryHitReadErrorClassV1::CorruptSource
    }
    NativeSelectedNamespaceReadErrorClassV1::ResourceLimit => NativeQueryHitReadErrorClassV1::ResourceLimit,
    NativeSelectedNamespaceReadErrorClassV1::Unavailable => NativeQueryHitReadErrorClassV1::HistoricalViewUnavailable,
    NativeSelectedNamespaceReadErrorClassV1::Cancelled => NativeQueryHitReadErrorClassV1::Cancelled,
  };
  NativeQueryHitReadErrorV1 { class, code: error.code(), context: error.context().to_string() }
}

fn map_source_error(error: QueryExecutionSourceErrorV1) -> NativeQueryHitReadErrorV1 {
  let class = match error.class() {
    QueryExecutionSourceErrorClassV1::Unavailable => NativeQueryHitReadErrorClassV1::HistoricalViewUnavailable,
    QueryExecutionSourceErrorClassV1::ResourceLimit => NativeQueryHitReadErrorClassV1::ResourceLimit,
    QueryExecutionSourceErrorClassV1::Corrupt => NativeQueryHitReadErrorClassV1::CorruptSource,
    QueryExecutionSourceErrorClassV1::Cancelled => NativeQueryHitReadErrorClassV1::Cancelled,
    QueryExecutionSourceErrorClassV1::Internal => NativeQueryHitReadErrorClassV1::Internal,
  };
  NativeQueryHitReadErrorV1 { class, code: error.code(), context: error.context().to_string() }
}

fn map_locator_error(error: LocatorRangeErrorV1) -> NativeQueryHitReadErrorV1 {
  let class = match error.class() {
    LocatorRangeErrorClassV1::InvalidRequest => NativeQueryHitReadErrorClassV1::InvalidRequest,
    LocatorRangeErrorClassV1::InvalidUtf8 => NativeQueryHitReadErrorClassV1::InvalidUtf8,
    LocatorRangeErrorClassV1::ResourceLimit => NativeQueryHitReadErrorClassV1::ResourceLimit,
  };
  NativeQueryHitReadErrorV1 { class, code: error.code(), context: error.context().to_string() }
}

fn retained_overflow() -> NativeQueryHitReadErrorV1 {
  resource("native_query_hit_retained_bytes", "query-hit retained-byte accounting overflowed")
}

fn invalid(code: &'static str, context: impl Into<String>) -> NativeQueryHitReadErrorV1 {
  NativeQueryHitReadErrorV1 { class: NativeQueryHitReadErrorClassV1::InvalidRequest, code, context: context.into() }
}

fn assertion_failed(code: &'static str, context: impl Into<String>) -> NativeQueryHitReadErrorV1 {
  NativeQueryHitReadErrorV1 { class: NativeQueryHitReadErrorClassV1::AssertionFailed, code, context: context.into() }
}

fn resource(code: &'static str, context: impl Into<String>) -> NativeQueryHitReadErrorV1 {
  NativeQueryHitReadErrorV1 { class: NativeQueryHitReadErrorClassV1::ResourceLimit, code, context: context.into() }
}

fn corrupt(code: &'static str, context: impl Into<String>) -> NativeQueryHitReadErrorV1 {
  NativeQueryHitReadErrorV1 { class: NativeQueryHitReadErrorClassV1::CorruptSource, code, context: context.into() }
}

fn cancelled() -> NativeQueryHitReadErrorV1 {
  NativeQueryHitReadErrorV1 {
    class: NativeQueryHitReadErrorClassV1::Cancelled,
    code: "native_query_hit_cancelled",
    context: "authorized query-hit locator/range work was cancelled".to_string(),
  }
}

fn internal(code: &'static str, context: impl Into<String>) -> NativeQueryHitReadErrorV1 {
  NativeQueryHitReadErrorV1 { class: NativeQueryHitReadErrorClassV1::Internal, code, context: context.into() }
}
