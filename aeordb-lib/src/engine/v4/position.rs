use std::ops::Range;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MAX_DECODED_LENGTH: usize = 1_048_576;
const MAX_ENCODED_LENGTH: usize = 1_398_102;
const MAX_COMPONENTS: usize = 32;
const FIXED_WITHOUT_HASHES: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum PositionRouteV1 {
  DirectoryListing = 1,
  Query = 2,
  GlobalSearch = 3,
  AggregateGroups = 4,
}

impl PositionRouteV1 {
  pub fn name(self) -> &'static str {
    match self {
      Self::DirectoryListing => "directory-listing",
      Self::Query => "query",
      Self::GlobalSearch => "global-search",
      Self::AggregateGroups => "aggregate-groups",
    }
  }

  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::DirectoryListing),
      2 => Ok(Self::Query),
      3 => Ok(Self::GlobalSearch),
      4 => Ok(Self::AggregateGroups),
      _ => Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "invalid_position_route", format!("unknown route kind {value}"))),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionComponentStateV1 {
  Present,
  TypedNull,
  Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionComparatorV1 {
  BytesBinary,
  Utf8Binary,
  U64,
  I64,
  FiniteF64,
  TimestampMs,
  Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionComponentV1<'a> {
  pub comparator: Option<PositionComparatorV1>,
  pub state: PositionComponentStateV1,
  pub payload: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct LogicalPositionV1 {
  decoded: Vec<u8>,
  hash_width: usize,
  tuple_range: Range<usize>,
  pub route: PositionRouteV1,
  pub component_count: u8,
}

impl LogicalPositionV1 {
  pub fn decoded_len(&self) -> usize {
    self.decoded.len()
  }

  pub fn order_fingerprint(&self) -> &[u8] {
    &self.decoded[16..16 + self.hash_width]
  }

  pub fn namespace_root(&self) -> &[u8] {
    &self.decoded[16 + self.hash_width..16 + 2 * self.hash_width]
  }

  pub fn file_key_tie(&self) -> &[u8] {
    &self.decoded[20 + 2 * self.hash_width..20 + 3 * self.hash_width]
  }

  pub fn record_revision_tie(&self) -> &[u8] {
    &self.decoded[20 + 3 * self.hash_width..20 + 4 * self.hash_width]
  }

  pub fn sort_tuple(&self) -> &[u8] {
    &self.decoded[self.tuple_range.clone()]
  }

  pub fn components(&self) -> PositionComponentIter<'_> {
    PositionComponentIter { bytes: self.sort_tuple(), offset: 0, remaining: usize::from(self.component_count) }
  }
}

#[derive(Debug, Clone)]
pub struct PositionComponentIter<'a> {
  bytes: &'a [u8],
  offset: usize,
  remaining: usize,
}

impl<'a> Iterator for PositionComponentIter<'a> {
  type Item = FormatResult<PositionComponentV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      return None;
    }
    let result = decode_component(self.bytes, self.offset).map(|(component, next)| {
      self.offset = next;
      self.remaining -= 1;
      component
    });
    if result.is_err() {
      self.remaining = 0;
    }
    Some(result)
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    (self.remaining, Some(self.remaining))
  }
}

impl ExactSizeIterator for PositionComponentIter<'_> {}

#[derive(Debug, Clone, Copy)]
pub struct PositionContextV1<'a> {
  pub route: PositionRouteV1,
  pub namespace_root: &'a [u8],
  pub order_fingerprint: &'a [u8],
  pub file_key_tie: &'a [u8],
  pub record_revision_tie: &'a [u8],
  pub sort_tuple: &'a [u8],
}

pub fn decode_logical_position(token: &[u8], expected_algorithm: HashAlgorithm) -> FormatResult<LogicalPositionV1> {
  if token.len() > MAX_ENCODED_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_cursor",
      format!("encoded token is {} bytes, cap is {MAX_ENCODED_LENGTH}", token.len()),
    ));
  }
  if token.is_empty() || token.contains(&b'=') {
    return Err(error(
      MalformedInputClass::NoncanonicalOrderOrDuplicate,
      "invalid_position_cursor",
      "position token must be nonempty unpadded base64url",
    ));
  }

  let decoded = URL_SAFE_NO_PAD.decode(token).map_err(|source| {
    error(MalformedInputClass::UnknownMagicOrVersion, "invalid_position_cursor", format!("invalid base64url: {source}"))
  })?;
  if decoded.len() > MAX_DECODED_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_cursor",
      format!("decoded token is {} bytes, cap is {MAX_DECODED_LENGTH}", decoded.len()),
    ));
  }
  if URL_SAFE_NO_PAD.encode(&decoded).as_bytes() != token {
    return Err(error(
      MalformedInputClass::NoncanonicalOrderOrDuplicate,
      "invalid_position_cursor",
      "position token has a noncanonical base64url spelling",
    ));
  }

  let hash_width = expected_algorithm.hash_length();
  let minimum = FIXED_WITHOUT_HASHES.checked_add(4 * hash_width).ok_or_else(|| length_error("minimum position length overflow"))?;
  if decoded.len() < minimum {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "invalid_position_cursor",
      format!("decoded token is {} bytes, minimum is {minimum}", decoded.len()),
    ));
  }
  if &decoded[..4] != b"APOS" || u16_at(&decoded, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "invalid_position_cursor", "expected APOS schema version 1"));
  }

  let route = PositionRouteV1::from_u16(u16_at(&decoded, 6)?)?;
  if usize::try_from(u32_at(&decoded, 8)?).map_err(|_| length_error("position total length conversion"))? != decoded.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "invalid_position_cursor",
      "declared decoded length does not match token length",
    ));
  }
  let actual_algorithm = HashAlgorithm::from_u16(u16_at(&decoded, 12)?)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "invalid_position_cursor", "unknown position hash algorithm"))?;
  if actual_algorithm != expected_algorithm {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "invalid_position_cursor",
      "position hash algorithm does not match the opened database",
    ));
  }

  let component_count = decoded[14];
  if usize::from(component_count) > MAX_COMPONENTS {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_cursor",
      format!("position declares {component_count} components, cap is {MAX_COMPONENTS}"),
    ));
  }
  if decoded[15] != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "invalid_position_cursor", "position flags must be zero"));
  }

  let crc_offset = decoded.len() - 4;
  if u32_at(&decoded, crc_offset)? != crc32fast::hash(&decoded[..crc_offset]) {
    return Err(error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "invalid_position_cursor",
      "position CRC does not match decoded bytes",
    ));
  }

  let tuple_length =
    usize::try_from(u32_at(&decoded, 16 + 2 * hash_width)?).map_err(|_| length_error("position tuple length conversion"))?;
  let tuple_start = 20usize.checked_add(4 * hash_width).ok_or_else(|| length_error("position tuple start overflow"))?;
  let tuple_end = tuple_start.checked_add(tuple_length).ok_or_else(|| length_error("position tuple end overflow"))?;
  if tuple_end.checked_add(4) != Some(decoded.len()) {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "invalid_position_cursor",
      "position tuple length does not consume the decoded token exactly",
    ));
  }

  for identity in [
    &decoded[16..16 + hash_width],
    &decoded[16 + hash_width..16 + 2 * hash_width],
    &decoded[20 + 2 * hash_width..20 + 3 * hash_width],
    &decoded[20 + 3 * hash_width..20 + 4 * hash_width],
  ] {
    if identity.iter().all(|byte| *byte == 0) {
      return Err(error(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "invalid_position_cursor",
        "position identities must be nonzero",
      ));
    }
  }

  validate_components(&decoded[tuple_start..tuple_end], component_count)?;
  Ok(LogicalPositionV1 { decoded, hash_width, tuple_range: tuple_start..tuple_end, route, component_count })
}

pub fn validate_position_context(position: &LogicalPositionV1, context: PositionContextV1<'_>) -> FormatResult<()> {
  if position.route != context.route {
    return Err(context_error("invalid_position_cursor", "position route does not match request route"));
  }
  if position.namespace_root() != context.namespace_root {
    return Err(context_error("position_root_mismatch", "position root does not match selected root"));
  }
  if position.order_fingerprint() != context.order_fingerprint {
    return Err(context_error("position_order_mismatch", "position order does not match request order"));
  }
  if position.file_key_tie() != context.file_key_tie
    || position.record_revision_tie() != context.record_revision_tie
    || position.sort_tuple() != context.sort_tuple
  {
    return Err(context_error("invalid_position_cursor", "position ties do not resolve in the selected result universe"));
  }
  Ok(())
}

fn validate_components(tuple: &[u8], expected_count: u8) -> FormatResult<()> {
  let mut offset = 0usize;
  let mut count = 0usize;
  while offset < tuple.len() {
    let (_, next) = decode_component(tuple, offset)?;
    count += 1;
    if count > MAX_COMPONENTS {
      return Err(error(
        MalformedInputClass::AllocationAmplification,
        "invalid_position_cursor",
        format!("position tuple exceeds {MAX_COMPONENTS} components"),
      ));
    }
    offset = next;
  }
  if offset != tuple.len() || count != usize::from(expected_count) {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "invalid_position_cursor",
      format!("position header declares {expected_count} components but tuple contains {count}"),
    ));
  }
  Ok(())
}

fn decode_component(tuple: &[u8], offset: usize) -> FormatResult<(PositionComponentV1<'_>, usize)> {
  let header_end = offset.checked_add(8).ok_or_else(|| length_error("position component header overflow"))?;
  let header = tuple.get(offset..header_end).ok_or_else(|| {
    error(MalformedInputClass::TruncationOrTrailingBytes, "invalid_position_cursor", "position component header is truncated")
  })?;
  if header[3] != 0 {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "invalid_position_cursor",
      "position component reserve byte must be zero",
    ));
  }
  let payload_length = usize::try_from(u32_at(header, 4)?).map_err(|_| length_error("position component length conversion"))?;
  let end = header_end.checked_add(payload_length).ok_or_else(|| length_error("position component end overflow"))?;
  let payload = tuple.get(header_end..end).ok_or_else(|| {
    error(MalformedInputClass::TruncationOrTrailingBytes, "invalid_position_cursor", "position component payload is truncated")
  })?;
  let tag = u16_at(header, 0)?;
  let (comparator, state) = match header[2] {
    0 => (Some(validate_present_component(tag, payload)?), PositionComponentStateV1::Present),
    1 if tag == 0 && payload.is_empty() => (None, PositionComponentStateV1::TypedNull),
    2 if tag == 0 && payload.is_empty() => (None, PositionComponentStateV1::Missing),
    state => {
      return Err(error(
        MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
        "invalid_position_cursor",
        format!("invalid position component state {state} for tag {tag}"),
      ));
    }
  };
  Ok((PositionComponentV1 { comparator, state, payload }, end))
}

fn validate_present_component(tag: u16, payload: &[u8]) -> FormatResult<PositionComparatorV1> {
  match tag {
    2 => Ok(PositionComparatorV1::BytesBinary),
    3 => {
      std::str::from_utf8(payload).map_err(|source| {
        error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "invalid_position_cursor", format!("invalid UTF-8: {source}"))
      })?;
      Ok(PositionComparatorV1::Utf8Binary)
    }
    4 if payload.len() == 8 => Ok(PositionComparatorV1::U64),
    5 if payload.len() == 8 => Ok(PositionComparatorV1::I64),
    6 if payload.len() == 8 => {
      let value = f64::from_le_bytes(payload.try_into().expect("checked f64 component width"));
      if !value.is_finite() || (value == 0.0 && value.to_bits() != 0) {
        return Err(error(
          MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
          "invalid_position_cursor",
          "position f64 must be finite and encode zero positively",
        ));
      }
      Ok(PositionComparatorV1::FiniteF64)
    }
    7 if payload.len() == 8 => Ok(PositionComparatorV1::TimestampMs),
    8 if payload == [0] || payload == [1] => Ok(PositionComparatorV1::Boolean),
    4..=8 => Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "invalid_position_cursor",
      format!("noncanonical payload length {} for comparator tag {tag}", payload.len()),
    )),
    _ => {
      Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "invalid_position_cursor", format!("unknown position comparator tag {tag}")))
    }
  }
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes.get(offset..offset + 2).ok_or_else(|| {
    error(MalformedInputClass::TruncationOrTrailingBytes, "invalid_position_cursor", format!("u16 at offset {offset} is truncated"))
  })?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked position u16 width")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes.get(offset..offset + 4).ok_or_else(|| {
    error(MalformedInputClass::TruncationOrTrailingBytes, "invalid_position_cursor", format!("u32 at offset {offset} is truncated"))
  })?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked position u32 width")))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "invalid_position_cursor", context)
}

fn context_error(code: &'static str, context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
