use crate::engine::HashAlgorithm;

use super::config_value::{CanonicalValueBounds, validate_canonical_value};
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::scope::validate_canonical_absolute_path;

pub const MAX_INDEX_RECORD_KEY_LENGTH: usize = 1_024 * 1_024;
pub const MAX_DOCUMENT_STATE_EVIDENCE_LENGTH: usize = 4 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentStateOwnerV1 {
  ValueStore,
  FieldIndex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDocumentRecordV1<'a> {
  pub tombstone: bool,
  pub document_ordinal: u64,
  pub file_key: &'a [u8],
  pub record_revision_hash: &'a [u8],
  pub path: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeReverseRecordV1<'a> {
  pub document_ordinal: u64,
  pub file_key: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalValueRecordV1<'a> {
  pub tombstone: bool,
  pub document_ordinal: u64,
  pub source_value_ordinal: u32,
  pub record_revision_hash: &'a [u8],
  pub canonical_value: Option<&'a [u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentStateRecordV1<'a> {
  pub tombstone: bool,
  pub stage: u8,
  pub reason: u16,
  pub document_ordinal: u64,
  pub record_revision_hash: &'a [u8],
  pub observed_value_count: u64,
  pub observed_canonical_bytes: u64,
  pub observed_work_units: u64,
  pub dependency_ordinal: u32,
  pub evidence: &'a [u8],
}

pub fn decode_scope_document_record(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ScopeDocumentRecordV1<'_>> {
  let (record, consumed) = decode_scope_document_record_prefix(value, hash_algorithm)?;
  require_exact_record_length(consumed, value.len())?;
  Ok(record)
}

pub fn decode_scope_reverse_record(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ScopeReverseRecordV1<'_>> {
  let (record, consumed) = decode_scope_reverse_record_prefix(value, hash_algorithm)?;
  require_exact_record_length(consumed, value.len())?;
  Ok(record)
}

pub fn decode_canonical_value_record(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<CanonicalValueRecordV1<'_>> {
  let (record, consumed) = decode_canonical_value_record_prefix(value, hash_algorithm)?;
  require_exact_record_length(consumed, value.len())?;
  Ok(record)
}

pub fn decode_document_state_record(
  value: &[u8],
  owner: DocumentStateOwnerV1,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<DocumentStateRecordV1<'_>> {
  let (record, consumed) = decode_document_state_record_prefix(value, owner, hash_algorithm)?;
  require_exact_record_length(consumed, value.len())?;
  Ok(record)
}

pub fn encode_scope_document_record(record: &ScopeDocumentRecordV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_document_ordinal(record.document_ordinal)?;
  validate_hash(record.file_key, hash_algorithm, "scope document FileKey")?;
  validate_hash(record.record_revision_hash, hash_algorithm, "scope document revision")?;
  validate_canonical_absolute_path(record.path)?;
  validate_record_variable_length(record.path.len(), "scope document path")?;
  let expected_file_key = digest_parts(hash_algorithm, &[b"file:", record.path.as_bytes()]);
  if expected_file_key != record.file_key {
    return Err(identity_error("scope document FileKey does not derive from its canonical path"));
  }

  let hash_width = hash_algorithm.hash_length();
  let length = 16usize
    .checked_add(2 * hash_width)
    .and_then(|value| value.checked_add(record.path.len()))
    .ok_or_else(|| length_error("scope document record length overflow"))?;
  let mut encoded = allocate_zeroed(length)?;
  encoded[0] = u8::from(record.tombstone);
  encoded[4..8].copy_from_slice(&checked_u32(record.path.len(), "scope document path length")?.to_le_bytes());
  encoded[8..16].copy_from_slice(&record.document_ordinal.to_le_bytes());
  encoded[16..16 + hash_width].copy_from_slice(record.file_key);
  encoded[16 + hash_width..16 + 2 * hash_width].copy_from_slice(record.record_revision_hash);
  encoded[16 + 2 * hash_width..].copy_from_slice(record.path.as_bytes());
  Ok(encoded)
}

pub fn encode_scope_reverse_record(record: &ScopeReverseRecordV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_document_ordinal(record.document_ordinal)?;
  validate_hash(record.file_key, hash_algorithm, "scope reverse FileKey")?;
  let hash_width = hash_algorithm.hash_length();
  let mut encoded = allocate_zeroed(12usize.checked_add(hash_width).ok_or_else(|| length_error("scope reverse record length overflow"))?)?;
  encoded[4..12].copy_from_slice(&record.document_ordinal.to_le_bytes());
  encoded[12..].copy_from_slice(record.file_key);
  Ok(encoded)
}

pub fn encode_canonical_value_record(record: &CanonicalValueRecordV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_document_ordinal(record.document_ordinal)?;
  validate_hash(record.record_revision_hash, hash_algorithm, "canonical value revision")?;
  let canonical_value = match (record.tombstone, record.canonical_value) {
    (true, None) => &[][..],
    (false, Some(value)) if !value.is_empty() => {
      validate_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE)?;
      value
    }
    _ => return Err(closure_error("canonical value tombstone and payload presence disagree")),
  };

  let hash_width = hash_algorithm.hash_length();
  let length = 24usize
    .checked_add(hash_width)
    .and_then(|value| value.checked_add(canonical_value.len()))
    .ok_or_else(|| length_error("canonical value record length overflow"))?;
  let mut encoded = allocate_zeroed(length)?;
  encoded[0] = u8::from(record.tombstone);
  encoded[4..8].copy_from_slice(&checked_u32(canonical_value.len(), "canonical value length")?.to_le_bytes());
  encoded[8..16].copy_from_slice(&record.document_ordinal.to_le_bytes());
  encoded[16..20].copy_from_slice(&record.source_value_ordinal.to_le_bytes());
  encoded[24..24 + hash_width].copy_from_slice(record.record_revision_hash);
  encoded[24 + hash_width..].copy_from_slice(canonical_value);
  Ok(encoded)
}

pub fn encode_document_state_record(
  record: &DocumentStateRecordV1<'_>,
  owner: DocumentStateOwnerV1,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  validate_document_ordinal(record.document_ordinal)?;
  validate_hash(record.record_revision_hash, hash_algorithm, "document state revision")?;
  validate_state(owner, record.stage, record.reason, record.evidence)?;
  let hash_width = hash_algorithm.hash_length();
  let length = 48usize
    .checked_add(hash_width)
    .and_then(|value| value.checked_add(record.evidence.len()))
    .ok_or_else(|| length_error("document state record length overflow"))?;
  let mut encoded = allocate_zeroed(length)?;
  encoded[0] = u8::from(record.tombstone);
  encoded[1] = record.stage;
  encoded[2..4].copy_from_slice(&record.reason.to_le_bytes());
  encoded[4..8].copy_from_slice(&checked_u32(record.evidence.len(), "document state evidence length")?.to_le_bytes());
  encoded[8..16].copy_from_slice(&record.document_ordinal.to_le_bytes());
  encoded[16..16 + hash_width].copy_from_slice(record.record_revision_hash);
  encoded[16 + hash_width..24 + hash_width].copy_from_slice(&record.observed_value_count.to_le_bytes());
  encoded[24 + hash_width..32 + hash_width].copy_from_slice(&record.observed_canonical_bytes.to_le_bytes());
  encoded[32 + hash_width..40 + hash_width].copy_from_slice(&record.observed_work_units.to_le_bytes());
  encoded[40 + hash_width..44 + hash_width].copy_from_slice(&record.dependency_ordinal.to_le_bytes());
  encoded[48 + hash_width..].copy_from_slice(record.evidence);
  Ok(encoded)
}

pub(crate) fn decode_scope_document_record_prefix(
  value: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(ScopeDocumentRecordV1<'_>, usize)> {
  let hash_width = hash_algorithm.hash_length();
  let tombstone = decode_record_flags(value)?;
  let path_length = usize::try_from(read_u32(value, 4)?).map_err(|_| length_error("scope document path length conversion"))?;
  validate_record_variable_length(path_length, "scope document path")?;
  let end = 16usize
    .checked_add(2 * hash_width)
    .and_then(|length| length.checked_add(path_length))
    .ok_or_else(|| length_error("scope document record length overflow"))?;
  require_prefix_length(value, end, "scope document record")?;
  let document_ordinal = read_u64(value, 8)?;
  validate_document_ordinal(document_ordinal)?;
  let file_key = &value[16..16 + hash_width];
  let record_revision_hash = &value[16 + hash_width..16 + 2 * hash_width];
  validate_hash(file_key, hash_algorithm, "scope document FileKey")?;
  validate_hash(record_revision_hash, hash_algorithm, "scope document revision")?;
  let path = std::str::from_utf8(&value[16 + 2 * hash_width..end])
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "index_record_path_utf8", source.to_string()))?;
  validate_canonical_absolute_path(path)?;
  let expected_file_key = digest_parts(hash_algorithm, &[b"file:", path.as_bytes()]);
  if expected_file_key != file_key {
    return Err(identity_error("scope document FileKey does not derive from its canonical path"));
  }
  Ok((ScopeDocumentRecordV1 { tombstone, document_ordinal, file_key, record_revision_hash, path }, end))
}

pub(crate) fn decode_scope_reverse_record_prefix(
  value: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(ScopeReverseRecordV1<'_>, usize)> {
  let hash_width = hash_algorithm.hash_length();
  let tombstone = decode_record_flags(value)?;
  if tombstone {
    return Err(closure_error("scope reverse records cannot be tombstones"));
  }
  let end = 12usize.checked_add(hash_width).ok_or_else(|| length_error("scope reverse record length overflow"))?;
  require_prefix_length(value, end, "scope reverse record")?;
  let document_ordinal = read_u64(value, 4)?;
  validate_document_ordinal(document_ordinal)?;
  let file_key = &value[12..end];
  validate_hash(file_key, hash_algorithm, "scope reverse FileKey")?;
  Ok((ScopeReverseRecordV1 { document_ordinal, file_key }, end))
}

pub(crate) fn decode_canonical_value_record_prefix(
  value: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(CanonicalValueRecordV1<'_>, usize)> {
  let hash_width = hash_algorithm.hash_length();
  let tombstone = decode_record_flags(value)?;
  let value_length = usize::try_from(read_u32(value, 4)?).map_err(|_| length_error("canonical value length conversion"))?;
  let end = 24usize
    .checked_add(hash_width)
    .and_then(|length| length.checked_add(value_length))
    .ok_or_else(|| length_error("canonical value record length overflow"))?;
  require_prefix_length(value, end, "canonical value record")?;
  if value[20..24].iter().any(|byte| *byte != 0) {
    return Err(reserve_error("canonical value record reserve is nonzero"));
  }
  let document_ordinal = read_u64(value, 8)?;
  validate_document_ordinal(document_ordinal)?;
  let source_value_ordinal = read_u32(value, 16)?;
  let record_revision_hash = &value[24..24 + hash_width];
  validate_hash(record_revision_hash, hash_algorithm, "canonical value revision")?;
  if tombstone != (value_length == 0) {
    return Err(closure_error("canonical value tombstone and payload presence disagree"));
  }
  let canonical_value = if tombstone {
    None
  } else {
    let canonical_value = &value[24 + hash_width..end];
    validate_canonical_value(canonical_value, CanonicalValueBounds::SOURCE_VALUE)?;
    Some(canonical_value)
  };
  Ok((CanonicalValueRecordV1 { tombstone, document_ordinal, source_value_ordinal, record_revision_hash, canonical_value }, end))
}

pub(crate) fn decode_document_state_record_prefix(
  value: &[u8],
  owner: DocumentStateOwnerV1,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(DocumentStateRecordV1<'_>, usize)> {
  let hash_width = hash_algorithm.hash_length();
  let tombstone = decode_state_flags(value)?;
  let stage = *value.get(1).ok_or_else(|| truncated_error("document state stage is truncated"))?;
  let reason = read_u16(value, 2)?;
  let evidence_length = usize::try_from(read_u32(value, 4)?).map_err(|_| length_error("document state evidence length conversion"))?;
  let end = 48usize
    .checked_add(hash_width)
    .and_then(|length| length.checked_add(evidence_length))
    .ok_or_else(|| length_error("document state record length overflow"))?;
  require_prefix_length(value, end, "document state record")?;
  let document_ordinal = read_u64(value, 8)?;
  validate_document_ordinal(document_ordinal)?;
  let record_revision_hash = &value[16..16 + hash_width];
  validate_hash(record_revision_hash, hash_algorithm, "document state revision")?;
  if read_u32(value, 44 + hash_width)? != 0 {
    return Err(reserve_error("document state record reserve is nonzero"));
  }
  let evidence = &value[48 + hash_width..end];
  validate_state(owner, stage, reason, evidence)?;
  Ok((
    DocumentStateRecordV1 {
      tombstone,
      stage,
      reason,
      document_ordinal,
      record_revision_hash,
      observed_value_count: read_u64(value, 16 + hash_width)?,
      observed_canonical_bytes: read_u64(value, 24 + hash_width)?,
      observed_work_units: read_u64(value, 32 + hash_width)?,
      dependency_ordinal: read_u32(value, 40 + hash_width)?,
      evidence,
    },
    end,
  ))
}

fn validate_state(owner: DocumentStateOwnerV1, stage: u8, reason: u16, evidence: &[u8]) -> FormatResult<()> {
  if !valid_state_reason(owner, stage, reason) {
    return Err(closure_error("document state stage and reason are not a legal pair for its owner"));
  }
  if evidence.is_empty() {
    return Err(closure_error("document state evidence is empty"));
  }
  if evidence.len() > MAX_DOCUMENT_STATE_EVIDENCE_LENGTH {
    return Err(amplification_error(format!(
      "document state evidence length {} exceeds {MAX_DOCUMENT_STATE_EVIDENCE_LENGTH}",
      evidence.len()
    )));
  }
  validate_canonical_value(evidence, CanonicalValueBounds::CONFIG).map(|_| ())
}

fn valid_state_reason(owner: DocumentStateOwnerV1, stage: u8, reason: u16) -> bool {
  match owner {
    DocumentStateOwnerV1::ValueStore => {
      matches!((stage, reason), (1, 0x0001..=0x0003) | (2, 0x0005..=0x0008) | (3, 0x0002 | 0x0004 | 0x0007 | 0x0008) | (4, 0x0007..=0x000b))
    }
    DocumentStateOwnerV1::FieldIndex => {
      matches!((stage, reason), (5, 0x0009..=0x000c | 0x000e | 0x000f) | (6, 0x0002 | 0x000d..=0x000f))
    }
  }
}

fn decode_record_flags(value: &[u8]) -> FormatResult<bool> {
  let flags = *value.first().ok_or_else(|| truncated_error("ordered record flags are truncated"))?;
  let reserve = value.get(1..4).ok_or_else(|| truncated_error("ordered record reserve is truncated"))?;
  if flags & !1 != 0 || reserve.iter().any(|byte| *byte != 0) {
    return Err(reserve_error("ordered record flags or reserve are noncanonical"));
  }
  Ok(flags & 1 != 0)
}

fn decode_state_flags(value: &[u8]) -> FormatResult<bool> {
  let flags = *value.first().ok_or_else(|| truncated_error("document state flags are truncated"))?;
  if flags & !1 != 0 {
    return Err(reserve_error("document state flags contain unknown bits"));
  }
  Ok(flags & 1 != 0)
}

fn validate_document_ordinal(document_ordinal: u64) -> FormatResult<()> {
  if document_ordinal == 0 {
    return Err(identity_error("document ordinal zero is reserved"));
  }
  Ok(())
}

fn validate_hash(value: &[u8], hash_algorithm: HashAlgorithm, context: &'static str) -> FormatResult<()> {
  if value.len() != hash_algorithm.hash_length() || value.iter().all(|byte| *byte == 0) {
    return Err(identity_error(format!("{context} has the wrong width or is all zero")));
  }
  Ok(())
}

fn validate_record_variable_length(length: usize, context: &'static str) -> FormatResult<()> {
  if length == 0 || length > MAX_INDEX_RECORD_KEY_LENGTH {
    return Err(amplification_error(format!("{context} length {length} is outside 1..={MAX_INDEX_RECORD_KEY_LENGTH}")));
  }
  Ok(())
}

fn require_prefix_length(value: &[u8], required: usize, context: &'static str) -> FormatResult<()> {
  if required > value.len() {
    return Err(truncated_error(format!("{context} needs {required} bytes, got {}", value.len())));
  }
  Ok(())
}

fn require_exact_record_length(consumed: usize, actual: usize) -> FormatResult<()> {
  if consumed != actual {
    return Err(truncated_error(format!("record consumes {consumed} bytes, input contains {actual}")));
  }
  Ok(())
}

fn allocate_zeroed(length: usize) -> FormatResult<Vec<u8>> {
  let mut value = Vec::new();
  value.try_reserve_exact(length).map_err(|source| amplification_error(format!("record allocation of {length} bytes failed: {source}")))?;
  value.resize(length, 0);
  Ok(value)
}

fn checked_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  u32::try_from(value).map_err(|source| length_error(format!("{context} does not fit u32: {source}")))
}

fn read_u16(value: &[u8], offset: usize) -> FormatResult<u16> {
  let end = offset.checked_add(2).ok_or_else(|| length_error("u16 offset overflow"))?;
  let bytes = value.get(offset..end).ok_or_else(|| truncated_error("u16 field is truncated"))?;
  let bytes = bytes.try_into().map_err(|source| truncated_error(format!("u16 field width conversion failed: {source}")))?;
  Ok(u16::from_le_bytes(bytes))
}

fn read_u32(value: &[u8], offset: usize) -> FormatResult<u32> {
  let end = offset.checked_add(4).ok_or_else(|| length_error("u32 offset overflow"))?;
  let bytes = value.get(offset..end).ok_or_else(|| truncated_error("u32 field is truncated"))?;
  let bytes = bytes.try_into().map_err(|source| truncated_error(format!("u32 field width conversion failed: {source}")))?;
  Ok(u32::from_le_bytes(bytes))
}

fn read_u64(value: &[u8], offset: usize) -> FormatResult<u64> {
  let end = offset.checked_add(8).ok_or_else(|| length_error("u64 offset overflow"))?;
  let bytes = value.get(offset..end).ok_or_else(|| truncated_error("u64 field is truncated"))?;
  let bytes = bytes.try_into().map_err(|source| truncated_error(format!("u64 field width conversion failed: {source}")))?;
  Ok(u64::from_le_bytes(bytes))
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}

fn truncated_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "index_record_length", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "index_record_arithmetic", context)
}

fn amplification_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, "index_record_bound", context)
}

fn reserve_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, "index_record_reserved", context)
}

fn identity_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "index_record_identity", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "index_record_closure", context)
}
