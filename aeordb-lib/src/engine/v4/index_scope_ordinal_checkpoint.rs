//! Bounded pending scope-ordinal claims stored in an index checkpoint resume key.
//!
//! The enclosing immutable checkpoint provides content identity and CRC
//! protection. This nested payload freezes framing, ordering, and resource
//! bounds so recovery can reject malformed validation state before allocating
//! or returning an ordinal.

use crate::engine::HashAlgorithm;

use super::reader::{FormatError, FormatResult, MalformedInputClass};

const MAGIC: &[u8; 4] = b"SORC";
const VERSION: u16 = 1;
const HEADER_LENGTH: usize = 32;
const FIXED_CLAIM_LENGTH: usize = 32;

/// Keeps the largest hash profile's resume payload below one MiB.
pub const MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1: u32 = 8_192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeOrdinalPendingClaimWriteV1<'a> {
  pub operation_id: [u8; 16],
  pub request_fingerprint: &'a [u8],
  pub document_ordinal: u64,
  pub source_publication_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeOrdinalPendingClaimV1<'a> {
  pub operation_id: [u8; 16],
  pub request_fingerprint: &'a [u8],
  pub document_ordinal: u64,
  pub source_publication_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeOrdinalClaimResumeV1<'a> {
  pub applied_through_sequence: u64,
  pub claims: Vec<ScopeOrdinalPendingClaimV1<'a>>,
}

pub fn encode_scope_ordinal_claim_resume_v1(
  hash_algorithm: HashAlgorithm,
  applied_through_sequence: u64,
  claims: &[ScopeOrdinalPendingClaimWriteV1<'_>],
) -> FormatResult<Vec<u8>> {
  let hash_width = hash_algorithm.hash_length();
  validate_claim_count(claims.len())?;
  let row_length = FIXED_CLAIM_LENGTH.checked_add(hash_width).ok_or_else(|| length_error("scope ordinal claim row length overflow"))?;
  let claims_length = claims.len().checked_mul(row_length).ok_or_else(|| length_error("scope ordinal claim bytes overflow"))?;
  let total_length = HEADER_LENGTH.checked_add(claims_length).ok_or_else(|| length_error("scope ordinal resume length overflow"))?;
  let total_length_u32 =
    u32::try_from(total_length).map_err(|error| length_error(format!("scope ordinal resume length exceeds u32: {error}")))?;
  let claim_count_u32 =
    u32::try_from(claims.len()).map_err(|error| length_error(format!("scope ordinal claim count exceeds u32: {error}")))?;
  let row_length_u16 =
    u16::try_from(row_length).map_err(|error| length_error(format!("scope ordinal claim row length exceeds u16: {error}")))?;
  let header_length_u16 =
    u16::try_from(HEADER_LENGTH).map_err(|error| length_error(format!("scope ordinal header length exceeds u16: {error}")))?;

  let mut encoded = Vec::new();
  encoded.try_reserve_exact(total_length).map_err(|error| allocation_error(format!("scope ordinal resume allocation failed: {error}")))?;
  encoded.resize(total_length, 0);
  encoded[..4].copy_from_slice(MAGIC);
  encoded[4..6].copy_from_slice(&VERSION.to_le_bytes());
  encoded[6..8].copy_from_slice(&header_length_u16.to_le_bytes());
  encoded[8..12].copy_from_slice(&total_length_u32.to_le_bytes());
  encoded[12..16].copy_from_slice(&claim_count_u32.to_le_bytes());
  encoded[16..24].copy_from_slice(&applied_through_sequence.to_le_bytes());
  encoded[24..26].copy_from_slice(&row_length_u16.to_le_bytes());

  let mut previous_operation_id = None;
  for (index, claim) in claims.iter().enumerate() {
    validate_claim(
      claim.operation_id,
      claim.request_fingerprint,
      claim.document_ordinal,
      claim.source_publication_sequence,
      applied_through_sequence,
      hash_width,
    )?;
    if previous_operation_id.is_some_and(|previous| previous >= claim.operation_id) {
      return Err(order_error("scope ordinal pending claims are not strictly ordered by operation ID"));
    }
    previous_operation_id = Some(claim.operation_id);
    let offset = HEADER_LENGTH + index * row_length;
    encoded[offset..offset + 16].copy_from_slice(&claim.operation_id);
    encoded[offset + 16..offset + 16 + hash_width].copy_from_slice(claim.request_fingerprint);
    encoded[offset + 16 + hash_width..offset + 24 + hash_width].copy_from_slice(&claim.document_ordinal.to_le_bytes());
    encoded[offset + 24 + hash_width..offset + 32 + hash_width].copy_from_slice(&claim.source_publication_sequence.to_le_bytes());
  }
  Ok(encoded)
}

pub fn decode_scope_ordinal_claim_resume_v1(encoded: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ScopeOrdinalClaimResumeV1<'_>> {
  if encoded.len() < HEADER_LENGTH {
    return Err(truncation_error("scope ordinal resume header is truncated"));
  }
  if &encoded[..4] != MAGIC || u16_at(encoded, 4)? != VERSION {
    return Err(FormatError::new(
      MalformedInputClass::UnknownMagicOrVersion,
      "scope_ordinal_resume_magic_version",
      "scope ordinal resume magic or version is unsupported",
    ));
  }
  if usize::from(u16_at(encoded, 6)?) != HEADER_LENGTH {
    return Err(length_error("scope ordinal resume header length is not 32"));
  }
  let total_length = usize::try_from(u32_at(encoded, 8)?)
    .map_err(|error| length_error(format!("scope ordinal resume length conversion failed: {error}")))?;
  if total_length != encoded.len() {
    return Err(truncation_error("scope ordinal resume declared length does not consume the input"));
  }
  let claim_count =
    usize::try_from(u32_at(encoded, 12)?).map_err(|error| length_error(format!("scope ordinal claim count conversion failed: {error}")))?;
  validate_claim_count(claim_count)?;
  let applied_through_sequence = u64_at(encoded, 16)?;
  let hash_width = hash_algorithm.hash_length();
  let expected_row_length =
    FIXED_CLAIM_LENGTH.checked_add(hash_width).ok_or_else(|| length_error("scope ordinal claim row length overflow"))?;
  if usize::from(u16_at(encoded, 24)?) != expected_row_length {
    return Err(length_error("scope ordinal claim row length disagrees with the database hash profile"));
  }
  if encoded[26..32].iter().any(|byte| *byte != 0) {
    return Err(FormatError::new(
      MalformedInputClass::NonzeroReservedOrPadding,
      "scope_ordinal_resume_reserve",
      "scope ordinal resume flags or reserve are nonzero",
    ));
  }
  let expected_length = claim_count
    .checked_mul(expected_row_length)
    .and_then(|length| length.checked_add(HEADER_LENGTH))
    .ok_or_else(|| length_error("scope ordinal resume length formula overflow"))?;
  if expected_length != encoded.len() {
    return Err(truncation_error("scope ordinal claim count does not consume the declared input"));
  }

  let mut claims = Vec::new();
  claims.try_reserve_exact(claim_count).map_err(|error| allocation_error(format!("scope ordinal claim allocation failed: {error}")))?;
  let mut previous_operation_id = None;
  for index in 0..claim_count {
    let offset = HEADER_LENGTH + index * expected_row_length;
    let mut operation_id = [0; 16];
    operation_id.copy_from_slice(&encoded[offset..offset + 16]);
    let fingerprint_start = offset + 16;
    let fingerprint_end = fingerprint_start + hash_width;
    let request_fingerprint = &encoded[fingerprint_start..fingerprint_end];
    let document_ordinal = u64_at(encoded, fingerprint_end)?;
    let source_publication_sequence = u64_at(encoded, fingerprint_end + 8)?;
    validate_claim(operation_id, request_fingerprint, document_ordinal, source_publication_sequence, applied_through_sequence, hash_width)?;
    if previous_operation_id.is_some_and(|previous| previous >= operation_id) {
      return Err(order_error("scope ordinal pending claims are not strictly ordered by operation ID"));
    }
    previous_operation_id = Some(operation_id);
    claims.push(ScopeOrdinalPendingClaimV1 { operation_id, request_fingerprint, document_ordinal, source_publication_sequence });
  }
  Ok(ScopeOrdinalClaimResumeV1 { applied_through_sequence, claims })
}

fn validate_claim_count(count: usize) -> FormatResult<()> {
  let maximum = usize::try_from(MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1)
    .map_err(|error| length_error(format!("scope ordinal claim limit conversion failed: {error}")))?;
  if count > maximum {
    return Err(allocation_error(format!("scope ordinal pending claim count {count} exceeds {}", MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1)));
  }
  Ok(())
}

fn validate_claim(
  operation_id: [u8; 16],
  request_fingerprint: &[u8],
  document_ordinal: u64,
  source_publication_sequence: u64,
  applied_through_sequence: u64,
  hash_width: usize,
) -> FormatResult<()> {
  if operation_id.iter().all(|byte| *byte == 0)
    || request_fingerprint.len() != hash_width
    || request_fingerprint.iter().all(|byte| *byte == 0)
    || document_ordinal == 0
    || source_publication_sequence == 0
  {
    return Err(FormatError::new(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "scope_ordinal_resume_claim_identity",
      "pending claim operation, fingerprint, ordinal, or source sequence is malformed",
    ));
  }
  if source_publication_sequence <= applied_through_sequence {
    return Err(FormatError::new(
      MalformedInputClass::CrossRecordClosureMismatch,
      "scope_ordinal_resume_claim_watermark",
      "pending claim source sequence is already covered by the applied watermark",
    ));
  }
  Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let end = offset.checked_add(2).ok_or_else(|| length_error("u16 offset overflow"))?;
  let value = bytes.get(offset..end).ok_or_else(|| truncation_error("u16 field is truncated"))?;
  let mut encoded = [0; 2];
  encoded.copy_from_slice(value);
  Ok(u16::from_le_bytes(encoded))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let end = offset.checked_add(4).ok_or_else(|| length_error("u32 offset overflow"))?;
  let value = bytes.get(offset..end).ok_or_else(|| truncation_error("u32 field is truncated"))?;
  let mut encoded = [0; 4];
  encoded.copy_from_slice(value);
  Ok(u32::from_le_bytes(encoded))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let end = offset.checked_add(8).ok_or_else(|| length_error("u64 offset overflow"))?;
  let value = bytes.get(offset..end).ok_or_else(|| truncation_error("u64 field is truncated"))?;
  let mut encoded = [0; 8];
  encoded.copy_from_slice(value);
  Ok(u64::from_le_bytes(encoded))
}

fn truncation_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::TruncationOrTrailingBytes, "scope_ordinal_resume_length", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "scope_ordinal_resume_length", context)
}

fn allocation_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::AllocationAmplification, "scope_ordinal_resume_allocation", context)
}

fn order_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NoncanonicalOrderOrDuplicate, "scope_ordinal_resume_order", context)
}
