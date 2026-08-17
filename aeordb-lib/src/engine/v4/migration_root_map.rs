//! Typed codecs and bounded chain verification for the finite v3 root map.
//!
//! Page links use domain-separated identity hashes derived from the database,
//! migration, and ordinal. They deliberately do not hash complete page bytes:
//! bidirectional content hashes would create a cyclic hash dependency.

use super::hash::{IncrementalDigestV1, digest_parts};
use super::namespace::SemanticUnavailableReasonV1;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::system_control::{SystemControlKindV1, decode_system_control, encode_system_control};
use crate::engine::HashAlgorithm;

const PAGE_IDENTITY_DOMAIN: &[u8] = b"aeordb.legacy-root-map-page.identity.v1\0";
const COMPLETE_MAP_DOMAIN: &[u8] = b"aeordb.legacy-root-map.complete.v1\0";
const SOURCE_FORMAT: u16 = 3;
const DESTINATION_FORMAT: u16 = 4;
const PAGE_BODY_FIXED_WITHOUT_HASHES: usize = 96;
const CONTROL_BODY_FIXED_WITHOUT_HASHES: usize = 104;
pub(super) const PAGE_BODY_MAX_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyRootSemanticAvailabilityV1 {
  Complete,
  ContentOnly { reason: SemanticUnavailableReasonV1 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyRootMapRowV1 {
  pub legacy_root_hash: Vec<u8>,
  pub namespace_root_v1_hash: Vec<u8>,
  pub semantic_availability: LegacyRootSemanticAvailabilityV1,
  pub captured_source_write_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyRootMapPageBodyV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub logical_database_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub page_ordinal: u64,
  pub previous_page_hash: Vec<u8>,
  pub next_page_hash: Vec<u8>,
  pub rows: Vec<LegacyRootMapRowV1>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedLegacyRootMapPageV1 {
  pub sequence: u64,
  pub body: LegacyRootMapPageBodyV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyRootMapControlBodyV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub logical_database_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub map_generation: u64,
  pub page_count: u32,
  pub record_count: u32,
  pub first_page_hash: Vec<u8>,
  pub last_page_hash: Vec<u8>,
  pub complete_map_digest: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedLegacyRootMapControlV1 {
  pub sequence: u64,
  pub body: LegacyRootMapControlBodyV1,
}

pub fn legacy_root_map_page_identity_hash(
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  migration_id: [u8; 16],
  page_ordinal: u64,
) -> FormatResult<Vec<u8>> {
  validate_algorithm(algorithm)?;
  require_nonzero_id(database_id, "legacy_root_map_page_database_id")?;
  require_nonzero_id(migration_id, "legacy_root_map_page_migration_id")?;
  Ok(digest_parts(algorithm, &[PAGE_IDENTITY_DOMAIN, &database_id, &migration_id, &page_ordinal.to_le_bytes()]))
}

pub fn encode_legacy_root_map_page(body: &LegacyRootMapPageBodyV1, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_page_body(body, algorithm)?;
  let hash_width = algorithm.hash_length();
  let row_width = row_width(hash_width)?;
  let rows_length = body
    .rows
    .len()
    .checked_mul(row_width)
    .ok_or_else(|| format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_page_length", "rows overflow"))?;
  let body_length = PAGE_BODY_FIXED_WITHOUT_HASHES
    .checked_add(2 * hash_width)
    .and_then(|length| length.checked_add(rows_length))
    .ok_or_else(|| format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_page_length", "body overflow"))?;
  let mut encoded = Vec::new();
  encoded
    .try_reserve_exact(body_length)
    .map_err(|error| format_error(MalformedInputClass::AllocationAmplification, "legacy_root_map_page_allocation", error.to_string()))?;
  append_ids(
    &mut encoded,
    body.database_id,
    body.migration_id,
    body.logical_database_id,
    body.source_physical_instance_id,
    body.destination_physical_instance_id,
  );
  encoded.extend_from_slice(&body.page_ordinal.to_le_bytes());
  encoded.extend_from_slice(&body.previous_page_hash);
  encoded.extend_from_slice(&body.next_page_hash);
  encoded.extend_from_slice(
    &u32::try_from(body.rows.len())
      .map_err(|error| format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_page_count", error.to_string()))?
      .to_le_bytes(),
  );
  encoded.extend_from_slice(
    &u32::try_from(rows_length)
      .map_err(|error| {
        format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_page_length", error.to_string())
      })?
      .to_le_bytes(),
  );
  for row in &body.rows {
    encode_row(&mut encoded, row);
  }
  debug_assert_eq!(encoded.len(), body_length);
  encode_system_control(SystemControlKindV1::LegacyRootMapPage, 1, &encoded, algorithm)
}

pub fn decode_legacy_root_map_page(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<DecodedLegacyRootMapPageV1> {
  validate_algorithm(algorithm)?;
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::LegacyRootMapPage {
    return Err(format_error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "legacy_root_map_page_control_kind",
      "control is not a LegacyRootMapPage",
    ));
  }
  let hash_width = algorithm.hash_length();
  let body = control.body;
  let row_count = usize::try_from(u32_at(body, 88 + 2 * hash_width)?)
    .map_err(|error| format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_page_count", error.to_string()))?;
  let rows_offset = PAGE_BODY_FIXED_WITHOUT_HASHES + 2 * hash_width;
  let mut rows = Vec::new();
  rows
    .try_reserve_exact(row_count)
    .map_err(|error| format_error(MalformedInputClass::AllocationAmplification, "legacy_root_map_page_allocation", error.to_string()))?;
  let width = row_width(hash_width)?;
  for row in body[rows_offset..].chunks_exact(width) {
    rows.push(decode_row(row, hash_width)?);
  }
  let decoded = LegacyRootMapPageBodyV1 {
    database_id: array_16(body, 0)?,
    migration_id: array_16(body, 16)?,
    logical_database_id: array_16(body, 32)?,
    source_physical_instance_id: array_16(body, 48)?,
    destination_physical_instance_id: array_16(body, 64)?,
    page_ordinal: u64_at(body, 80)?,
    previous_page_hash: body[88..88 + hash_width].to_vec(),
    next_page_hash: body[88 + hash_width..88 + 2 * hash_width].to_vec(),
    rows,
  };
  validate_page_body(&decoded, algorithm)?;
  Ok(DecodedLegacyRootMapPageV1 { sequence: control.sequence, body: decoded })
}

pub fn encode_legacy_root_map_control(sequence: u64, body: &LegacyRootMapControlBodyV1, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_control_body(body, algorithm)?;
  let mut encoded = Vec::new();
  encoded
    .try_reserve_exact(CONTROL_BODY_FIXED_WITHOUT_HASHES + 3 * algorithm.hash_length())
    .map_err(|error| format_error(MalformedInputClass::AllocationAmplification, "legacy_root_map_control_allocation", error.to_string()))?;
  append_ids(
    &mut encoded,
    body.database_id,
    body.migration_id,
    body.logical_database_id,
    body.source_physical_instance_id,
    body.destination_physical_instance_id,
  );
  encoded.extend_from_slice(&SOURCE_FORMAT.to_le_bytes());
  encoded.extend_from_slice(&DESTINATION_FORMAT.to_le_bytes());
  encoded.extend_from_slice(&0u32.to_le_bytes());
  encoded.extend_from_slice(&body.map_generation.to_le_bytes());
  encoded.extend_from_slice(&body.page_count.to_le_bytes());
  encoded.extend_from_slice(&body.record_count.to_le_bytes());
  encoded.extend_from_slice(&body.first_page_hash);
  encoded.extend_from_slice(&body.last_page_hash);
  encoded.extend_from_slice(&body.complete_map_digest);
  encode_system_control(SystemControlKindV1::LegacyRootMapControl, sequence, &encoded, algorithm)
}

pub fn decode_legacy_root_map_control(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<DecodedLegacyRootMapControlV1> {
  validate_algorithm(algorithm)?;
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::LegacyRootMapControl {
    return Err(format_error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "legacy_root_map_control_kind",
      "control is not a LegacyRootMapControl",
    ));
  }
  let body = control.body;
  let hash_width = algorithm.hash_length();
  let decoded = LegacyRootMapControlBodyV1 {
    database_id: array_16(body, 0)?,
    migration_id: array_16(body, 16)?,
    logical_database_id: array_16(body, 32)?,
    source_physical_instance_id: array_16(body, 48)?,
    destination_physical_instance_id: array_16(body, 64)?,
    map_generation: u64_at(body, 88)?,
    page_count: u32_at(body, 96)?,
    record_count: u32_at(body, 100)?,
    first_page_hash: body[104..104 + hash_width].to_vec(),
    last_page_hash: body[104 + hash_width..104 + 2 * hash_width].to_vec(),
    complete_map_digest: body[104 + 2 * hash_width..104 + 3 * hash_width].to_vec(),
  };
  validate_control_body(&decoded, algorithm)?;
  Ok(DecodedLegacyRootMapControlV1 { sequence: control.sequence, body: decoded })
}

pub struct LegacyRootMapChainDigestBuilderV1 {
  algorithm: HashAlgorithm,
  expected: LegacyRootMapControlBodyV1,
  hasher: IncrementalDigestV1,
  next_ordinal: u64,
  record_count: u64,
  previous_page_hash: Option<Vec<u8>>,
  previous_row_hash: Option<Vec<u8>>,
}

impl LegacyRootMapChainDigestBuilderV1 {
  pub fn push_page(&mut self, bytes: &[u8]) -> FormatResult<()> {
    let page = decode_legacy_root_map_page(bytes, self.algorithm)?;
    validate_chain_page(self, &page.body)?;
    let page_hash =
      legacy_root_map_page_identity_hash(self.algorithm, page.body.database_id, page.body.migration_id, page.body.page_ordinal)?;
    self.hasher.update(&page_hash);
    self.hasher.update(
      &u32::try_from(bytes.len())
        .map_err(|error| {
          format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_chain_page_length", error.to_string())
        })?
        .to_le_bytes(),
    );
    self.hasher.update(bytes);
    self.previous_page_hash = Some(page_hash);
    if let Some(last) = page.body.rows.last() {
      self.previous_row_hash = Some(last.legacy_root_hash.clone());
    }
    self.record_count = self.record_count.checked_add(page.body.rows.len() as u64).ok_or_else(|| {
      format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_chain_count", "record count overflow")
    })?;
    self.next_ordinal = self.next_ordinal.checked_add(1).ok_or_else(|| {
      format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_chain_count", "page ordinal overflow")
    })?;
    Ok(())
  }

  pub fn finish(self) -> FormatResult<Vec<u8>> {
    validate_chain_finish(&self)?;
    if self.expected.page_count == 0 {
      return Ok(vec![0; self.algorithm.hash_length()]);
    }
    Ok(self.hasher.finalize())
  }
}

pub struct LegacyRootMapChainVerifierV1 {
  expected_digest: Vec<u8>,
  builder: LegacyRootMapChainDigestBuilderV1,
}

impl LegacyRootMapChainVerifierV1 {
  pub fn digest_builder(control: &LegacyRootMapControlBodyV1, algorithm: HashAlgorithm) -> FormatResult<LegacyRootMapChainDigestBuilderV1> {
    validate_control_body(control, algorithm)?;
    let mut hasher = IncrementalDigestV1::new(algorithm);
    hasher.update(COMPLETE_MAP_DOMAIN);
    append_digest_control_basis(&mut hasher, control);
    Ok(LegacyRootMapChainDigestBuilderV1 {
      algorithm,
      expected: control.clone(),
      hasher,
      next_ordinal: 0,
      record_count: 0,
      previous_page_hash: None,
      previous_row_hash: None,
    })
  }

  pub fn new(control: &DecodedLegacyRootMapControlV1, algorithm: HashAlgorithm) -> FormatResult<Self> {
    Ok(Self { expected_digest: control.body.complete_map_digest.clone(), builder: Self::digest_builder(&control.body, algorithm)? })
  }

  pub fn push_page(&mut self, bytes: &[u8]) -> FormatResult<()> {
    self.builder.push_page(bytes)
  }

  pub fn finish(self) -> FormatResult<u32> {
    let record_count = u32::try_from(self.builder.record_count).map_err(|error| {
      format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_chain_count", error.to_string())
    })?;
    let digest = self.builder.finish()?;
    if digest != self.expected_digest {
      return Err(format_error(
        MalformedInputClass::ChecksumOrIntegrityMismatch,
        "legacy_root_map_chain_digest",
        "selected control digest differs from the complete page chain",
      ));
    }
    Ok(record_count)
  }
}

fn validate_page_body(body: &LegacyRootMapPageBodyV1, algorithm: HashAlgorithm) -> FormatResult<()> {
  validate_algorithm(algorithm)?;
  validate_ids(
    body.database_id,
    body.migration_id,
    body.logical_database_id,
    body.source_physical_instance_id,
    body.destination_physical_instance_id,
    "legacy_root_map_page_identity",
  )?;
  let hash_width = algorithm.hash_length();
  if body.page_ordinal >= u64::from(u32::MAX) {
    return Err(format_error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "legacy_root_map_page_ordinal",
      "page ordinal cannot be represented by the selected control page count",
    ));
  }
  require_hash_width(&body.previous_page_hash, hash_width, true, "legacy_root_map_page_hash_width")?;
  require_hash_width(&body.next_page_hash, hash_width, true, "legacy_root_map_page_hash_width")?;
  let previous_zero = all_zero(&body.previous_page_hash);
  if (body.page_ordinal == 0) != previous_zero {
    return Err(format_error(
      MalformedInputClass::InvalidGraphEdgeOrCycle,
      "legacy_root_map_page_link",
      "only page zero may have a zero previous-page identity",
    ));
  }
  if body.rows.is_empty() {
    return Err(format_error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "legacy_root_map_page_count",
      "published root-map pages must contain at least one row",
    ));
  }
  let row_width = row_width(hash_width)?;
  let body_length = PAGE_BODY_FIXED_WITHOUT_HASHES
    .checked_add(2 * hash_width)
    .and_then(|length| length.checked_add(body.rows.len().checked_mul(row_width)?))
    .ok_or_else(|| format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_page_length", "body overflow"))?;
  if body_length > PAGE_BODY_MAX_BYTES {
    return Err(format_error(
      MalformedInputClass::AllocationAmplification,
      "legacy_root_map_page_length",
      format!("page body has {body_length} bytes, exceeding {PAGE_BODY_MAX_BYTES}"),
    ));
  }
  let mut previous: Option<&[u8]> = None;
  for row in &body.rows {
    validate_row(row, hash_width)?;
    if previous.is_some_and(|hash| hash >= row.legacy_root_hash.as_slice()) {
      return Err(format_error(
        MalformedInputClass::NoncanonicalOrderOrDuplicate,
        "legacy_root_map_page_order",
        "root-map rows are not in strict legacy-root order",
      ));
    }
    previous = Some(&row.legacy_root_hash);
  }
  Ok(())
}

fn validate_control_body(body: &LegacyRootMapControlBodyV1, algorithm: HashAlgorithm) -> FormatResult<()> {
  validate_algorithm(algorithm)?;
  validate_ids(
    body.database_id,
    body.migration_id,
    body.logical_database_id,
    body.source_physical_instance_id,
    body.destination_physical_instance_id,
    "legacy_root_map_control_identity",
  )?;
  if body.map_generation == 0 {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "legacy_root_map_control_generation",
      "map generation is zero",
    ));
  }
  let populated = body.page_count != 0;
  if populated != (body.record_count != 0) || body.page_count > body.record_count {
    return Err(format_error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "legacy_root_map_control_counts",
      "page and record counts do not describe nonempty pages",
    ));
  }
  let hash_width = algorithm.hash_length();
  require_hash_width(&body.first_page_hash, hash_width, !populated, "legacy_root_map_control_hash_width")?;
  require_hash_width(&body.last_page_hash, hash_width, !populated, "legacy_root_map_control_hash_width")?;
  require_hash_width(&body.complete_map_digest, hash_width, !populated, "legacy_root_map_control_hash_width")?;
  if !populated && (!all_zero(&body.first_page_hash) || !all_zero(&body.last_page_hash) || !all_zero(&body.complete_map_digest)) {
    return Err(format_error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "legacy_root_map_control_counts",
      "empty map carries nonzero page or digest identities",
    ));
  }
  Ok(())
}

fn validate_chain_page(builder: &LegacyRootMapChainDigestBuilderV1, page: &LegacyRootMapPageBodyV1) -> FormatResult<()> {
  if page.database_id != builder.expected.database_id
    || page.migration_id != builder.expected.migration_id
    || page.logical_database_id != builder.expected.logical_database_id
    || page.source_physical_instance_id != builder.expected.source_physical_instance_id
    || page.destination_physical_instance_id != builder.expected.destination_physical_instance_id
    || page.page_ordinal != builder.next_ordinal
  {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "legacy_root_map_chain_identity",
      "page identity or ordinal differs from the selected map",
    ));
  }
  let page_hash = legacy_root_map_page_identity_hash(builder.algorithm, page.database_id, page.migration_id, page.page_ordinal)?;
  if page.page_ordinal == 0 && page_hash != builder.expected.first_page_hash {
    return Err(format_error(
      MalformedInputClass::InvalidGraphEdgeOrCycle,
      "legacy_root_map_chain_link",
      "first page identity differs from the selected control",
    ));
  }
  if page.page_ordinal == 0 {
    if !all_zero(&page.previous_page_hash) {
      return Err(format_error(MalformedInputClass::InvalidGraphEdgeOrCycle, "legacy_root_map_chain_link", "first page has a predecessor"));
    }
  } else {
    let Some(expected_previous) = builder.previous_page_hash.as_deref() else {
      return Err(format_error(
        MalformedInputClass::InvalidGraphEdgeOrCycle,
        "legacy_root_map_chain_link",
        "noninitial page has no observed predecessor",
      ));
    };
    if page.previous_page_hash != expected_previous {
      return Err(format_error(
        MalformedInputClass::InvalidGraphEdgeOrCycle,
        "legacy_root_map_chain_link",
        "page predecessor differs from the prior page identity",
      ));
    }
  }
  let successor_ordinal = page.page_ordinal.checked_add(1).ok_or_else(|| {
    format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_chain_count", "page ordinal overflow")
  })?;
  let is_last = successor_ordinal == u64::from(builder.expected.page_count);
  if is_last {
    if page_hash != builder.expected.last_page_hash || !all_zero(&page.next_page_hash) {
      return Err(format_error(
        MalformedInputClass::InvalidGraphEdgeOrCycle,
        "legacy_root_map_chain_link",
        "last page identity or successor differs from the selected control",
      ));
    }
  } else {
    let next = legacy_root_map_page_identity_hash(builder.algorithm, page.database_id, page.migration_id, successor_ordinal)?;
    if page.next_page_hash != next {
      return Err(format_error(
        MalformedInputClass::InvalidGraphEdgeOrCycle,
        "legacy_root_map_chain_link",
        "page successor is not the next canonical page identity",
      ));
    }
  }
  if let (Some(previous), Some(first)) = (builder.previous_row_hash.as_deref(), page.rows.first()) {
    if previous >= first.legacy_root_hash.as_slice() {
      return Err(format_error(
        MalformedInputClass::NoncanonicalOrderOrDuplicate,
        "legacy_root_map_chain_order",
        "rows are not globally ordered across pages",
      ));
    }
  }
  Ok(())
}

fn validate_chain_finish(builder: &LegacyRootMapChainDigestBuilderV1) -> FormatResult<()> {
  if builder.next_ordinal != u64::from(builder.expected.page_count) || builder.record_count != u64::from(builder.expected.record_count) {
    return Err(format_error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "legacy_root_map_chain_incomplete",
      "observed pages or rows do not match the selected control",
    ));
  }
  Ok(())
}

pub(super) fn validate_row(row: &LegacyRootMapRowV1, hash_width: usize) -> FormatResult<()> {
  require_hash_width(&row.legacy_root_hash, hash_width, false, "legacy_root_map_page_row_hash")?;
  require_hash_width(&row.namespace_root_v1_hash, hash_width, false, "legacy_root_map_page_row_hash")?;
  if row.captured_source_write_sequence == 0 {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "legacy_root_map_page_row_sequence",
      "captured source write sequence is zero",
    ));
  }
  Ok(())
}

pub(super) fn encode_row(encoded: &mut Vec<u8>, row: &LegacyRootMapRowV1) {
  encoded.extend_from_slice(&row.legacy_root_hash);
  encoded.extend_from_slice(&row.namespace_root_v1_hash);
  match row.semantic_availability {
    LegacyRootSemanticAvailabilityV1::Complete => {
      encoded.extend_from_slice(&1u16.to_le_bytes());
      encoded.extend_from_slice(&0u16.to_le_bytes());
    }
    LegacyRootSemanticAvailabilityV1::ContentOnly { reason } => {
      encoded.extend_from_slice(&2u16.to_le_bytes());
      encoded.extend_from_slice(&(reason as u16).to_le_bytes());
    }
  }
  encoded.extend_from_slice(&row.captured_source_write_sequence.to_le_bytes());
}

pub(super) fn decode_row(row: &[u8], hash_width: usize) -> FormatResult<LegacyRootMapRowV1> {
  let availability = u16_at(row, 2 * hash_width)?;
  let reason = u16_at(row, 2 * hash_width + 2)?;
  let semantic_availability = match (availability, reason) {
    (1, 0) => LegacyRootSemanticAvailabilityV1::Complete,
    (2, 1) => LegacyRootSemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    (2, 2) => LegacyRootSemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyDependencyCannotBeProven },
    (2, 3) => {
      LegacyRootSemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacySemanticControlCorruptOrIncomplete }
    }
    _ => {
      return Err(format_error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "legacy_root_map_page_row_kind",
        "semantic availability and reason are not canonical",
      ));
    }
  };
  Ok(LegacyRootMapRowV1 {
    legacy_root_hash: row[..hash_width].to_vec(),
    namespace_root_v1_hash: row[hash_width..2 * hash_width].to_vec(),
    semantic_availability,
    captured_source_write_sequence: u64_at(row, 2 * hash_width + 4)?,
  })
}

fn append_digest_control_basis(hasher: &mut IncrementalDigestV1, control: &LegacyRootMapControlBodyV1) {
  hasher.update(&control.database_id);
  hasher.update(&control.migration_id);
  hasher.update(&control.logical_database_id);
  hasher.update(&control.source_physical_instance_id);
  hasher.update(&control.destination_physical_instance_id);
  hasher.update(&control.map_generation.to_le_bytes());
  hasher.update(&control.page_count.to_le_bytes());
  hasher.update(&control.record_count.to_le_bytes());
  hasher.update(&control.first_page_hash);
  hasher.update(&control.last_page_hash);
}

fn append_ids(
  body: &mut Vec<u8>,
  database_id: [u8; 16],
  migration_id: [u8; 16],
  logical_database_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
) {
  body.extend_from_slice(&database_id);
  body.extend_from_slice(&migration_id);
  body.extend_from_slice(&logical_database_id);
  body.extend_from_slice(&source_physical_instance_id);
  body.extend_from_slice(&destination_physical_instance_id);
}

fn validate_ids(
  database_id: [u8; 16],
  migration_id: [u8; 16],
  logical_database_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  code: &'static str,
) -> FormatResult<()> {
  if [database_id, migration_id, logical_database_id, source_physical_instance_id, destination_physical_instance_id]
    .into_iter()
    .any(|value| all_zero(&value))
    || source_physical_instance_id == destination_physical_instance_id
  {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      code,
      "root-map IDs are zero or source and destination physical IDs are equal",
    ));
  }
  Ok(())
}

fn validate_algorithm(algorithm: HashAlgorithm) -> FormatResult<()> {
  if !matches!(algorithm, HashAlgorithm::Blake3_256 | HashAlgorithm::Sha512) {
    return Err(format_error(
      MalformedInputClass::UnknownMagicOrVersion,
      "legacy_root_map_hash_algorithm",
      "v4 root maps accept only BLAKE3-256 or SHA-512",
    ));
  }
  Ok(())
}

fn require_nonzero_id(value: [u8; 16], code: &'static str) -> FormatResult<()> {
  if all_zero(&value) {
    return Err(format_error(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, "identity is zero"));
  }
  Ok(())
}

fn require_hash_width(bytes: &[u8], width: usize, allow_zero: bool, code: &'static str) -> FormatResult<()> {
  if bytes.len() != width || (!allow_zero && all_zero(bytes)) {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      code,
      format!("hash has width {}, expected {width}, or is unexpectedly zero", bytes.len()),
    ));
  }
  Ok(())
}

pub(super) fn row_width(hash_width: usize) -> FormatResult<usize> {
  hash_width
    .checked_mul(2)
    .and_then(|width| width.checked_add(12))
    .ok_or_else(|| format_error(MalformedInputClass::LengthCountOrArithmeticOverflow, "legacy_root_map_row_width", "row width overflow"))
}

fn array_16(bytes: &[u8], offset: usize) -> FormatResult<[u8; 16]> {
  fixed_array_at(bytes, offset)
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  Ok(u16::from_le_bytes(fixed_array_at(bytes, offset)?))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  Ok(u32::from_le_bytes(fixed_array_at(bytes, offset)?))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  Ok(u64::from_le_bytes(fixed_array_at(bytes, offset)?))
}

fn fixed_array_at<const N: usize>(bytes: &[u8], offset: usize) -> FormatResult<[u8; N]> {
  let end = offset.checked_add(N).ok_or_else(bounds_error)?;
  let raw = bytes.get(offset..end).ok_or_else(bounds_error)?;
  match raw.try_into() {
    Ok(value) => Ok(value),
    Err(error) => Err(format_error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "legacy_root_map_bounds",
      format!("root-map field has the wrong fixed width: {error}"),
    )),
  }
}

fn bounds_error() -> FormatError {
  format_error(MalformedInputClass::TruncationOrTrailingBytes, "legacy_root_map_bounds", "root-map field is truncated")
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn format_error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
