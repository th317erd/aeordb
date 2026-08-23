use std::collections::BTreeMap;
use std::ops::Range;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use crate::engine::HashAlgorithm;

const MAX_DECODED_LENGTH: usize = 1_048_576;
const MAX_ENCODED_LENGTH: usize = 1_398_102;
const MAX_COMPONENTS: usize = 32;
const FIXED_WITHOUT_HASHES: usize = 24;
const POSITION_ORDER_FINGERPRINT_DOMAIN_V1: &[u8] = b"aeordb.position-order.v1\0";

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
pub enum PositionSortDirectionV1 {
  Ascending,
  Descending,
}

impl PositionSortDirectionV1 {
  pub const fn name(self) -> &'static str {
    match self {
      Self::Ascending => "asc",
      Self::Descending => "desc",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSortDefinitionV1<'a> {
  pub field: &'a str,
  pub direction: PositionSortDirectionV1,
  pub comparator: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRouteOrderDefinitionV1<'a> {
  pub route: PositionRouteV1,
  pub sort: &'a [PositionSortDefinitionV1<'a>],
  pub directories_first: &'a str,
  pub multi_value_selector: &'a str,
  pub name_collation: &'a str,
  pub null_missing_policy: &'a str,
  pub score_semantics: &'a str,
  pub semantic_fingerprints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRouteOrderV1 {
  route: PositionRouteV1,
  hash_algorithm: HashAlgorithm,
  canonical_definition: Vec<u8>,
  fingerprint: Vec<u8>,
  component_count: usize,
}

impl CompiledRouteOrderV1 {
  pub const fn route(&self) -> PositionRouteV1 {
    self.route
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub fn canonical_definition(&self) -> &[u8] {
    &self.canonical_definition
  }

  pub fn fingerprint(&self) -> &[u8] {
    &self.fingerprint
  }

  pub const fn component_count(&self) -> usize {
    self.component_count
  }
}

pub fn compile_route_order_definition(
  hash_algorithm: HashAlgorithm,
  definition: &CanonicalRouteOrderDefinitionV1<'_>,
) -> FormatResult<CompiledRouteOrderV1> {
  let expected_definition_length = validate_route_order_definition(definition)?;

  let sort = definition
    .sort
    .iter()
    .map(|component| {
      CanonicalConfigValueV1::Map(BTreeMap::from([
        ("comparator".to_string(), CanonicalConfigValueV1::String(component.comparator.to_string())),
        ("direction".to_string(), CanonicalConfigValueV1::String(component.direction.name().to_string())),
        ("field".to_string(), CanonicalConfigValueV1::String(component.field.to_string())),
      ]))
    })
    .collect();
  let semantic_fingerprints = definition.semantic_fingerprints.iter().cloned().map(CanonicalConfigValueV1::String).collect::<Vec<_>>();
  let value = CanonicalConfigValueV1::Map(BTreeMap::from([
    (
      "default_ties".to_string(),
      CanonicalConfigValueV1::Array(
        ["canonical_path_asc", "FileKey_asc", "RecordRevisionHash_asc"]
          .into_iter()
          .map(|value| CanonicalConfigValueV1::String(value.to_string()))
          .collect(),
      ),
    ),
    ("directories_first".to_string(), CanonicalConfigValueV1::String(definition.directories_first.to_string())),
    ("multi_value_selector".to_string(), CanonicalConfigValueV1::String(definition.multi_value_selector.to_string())),
    ("name_collation".to_string(), CanonicalConfigValueV1::String(definition.name_collation.to_string())),
    ("null_missing_policy".to_string(), CanonicalConfigValueV1::String(definition.null_missing_policy.to_string())),
    ("route_kind".to_string(), CanonicalConfigValueV1::Signed(definition.route as i64)),
    ("score_semantics".to_string(), CanonicalConfigValueV1::String(definition.score_semantics.to_string())),
    ("semantic_fingerprints".to_string(), CanonicalConfigValueV1::Array(semantic_fingerprints)),
    ("sort".to_string(), CanonicalConfigValueV1::Array(sort)),
  ]));
  let canonical_definition = encode_canonical_value(&value, CanonicalValueBounds::CONFIG)?;
  if canonical_definition.len() != expected_definition_length {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "invalid_position_order",
      format!(
        "canonical position order preflight expected {expected_definition_length} bytes but encoder produced {}",
        canonical_definition.len()
      ),
    ));
  }
  let fingerprint = digest_parts(hash_algorithm, &[POSITION_ORDER_FINGERPRINT_DOMAIN_V1, &canonical_definition]);
  if fingerprint.iter().all(|byte| *byte == 0) {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "invalid_position_order",
      "position order fingerprint must be nonzero",
    ));
  }

  Ok(CompiledRouteOrderV1 {
    route: definition.route,
    hash_algorithm,
    canonical_definition,
    fingerprint,
    component_count: definition.sort.len(),
  })
}

fn validate_route_order_definition(definition: &CanonicalRouteOrderDefinitionV1<'_>) -> FormatResult<usize> {
  if definition.sort.is_empty() {
    return Err(order_error("position order must declare at least one sort component"));
  }
  if definition.sort.len() > MAX_COMPONENTS {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_order",
      format!("position order declares {} components, cap is {MAX_COMPONENTS}", definition.sort.len()),
    ));
  }
  for component in definition.sort {
    if component.field.is_empty() || component.comparator.is_empty() {
      return Err(order_error("position sort fields and comparators must be nonempty"));
    }
    validate_position_sort_comparator(component.comparator)?;
  }
  for policy in [
    definition.directories_first,
    definition.multi_value_selector,
    definition.name_collation,
    definition.null_missing_policy,
    definition.score_semantics,
  ] {
    if policy.is_empty() {
      return Err(order_error("position order policies must be nonempty"));
    }
  }
  if definition.semantic_fingerprints.is_empty() || definition.semantic_fingerprints.len() > MAX_COMPONENTS {
    return Err(order_error(format!("position order must declare between 1 and {MAX_COMPONENTS} semantic fingerprints")));
  }
  if definition
    .semantic_fingerprints
    .iter()
    .any(|fingerprint| fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
  {
    return Err(order_error("position semantic fingerprints must be 64 lowercase hexadecimal characters"));
  }
  preflight_route_order_definition_length(definition)
}

fn preflight_route_order_definition_length(definition: &CanonicalRouteOrderDefinitionV1<'_>) -> FormatResult<usize> {
  let bounds = CanonicalValueBounds::CONFIG;
  let default_ties = canonical_array_length(
    ["canonical_path_asc", "FileKey_asc", "RecordRevisionHash_asc"].into_iter().map(|value| canonical_string_length(value, bounds)),
  )?;

  let mut sort_payload = 4usize;
  for component in definition.sort {
    let component_length = canonical_map_length([
      ("comparator", canonical_string_length(component.comparator, bounds)?),
      ("direction", canonical_string_length(component.direction.name(), bounds)?),
      ("field", canonical_string_length(component.field, bounds)?),
    ])?;
    sort_payload = checked_position_length_add(sort_payload, component_length, "route order sort length overflow")?;
  }
  let sort = canonical_frame_length(sort_payload, "route order sort frame overflow")?;
  let semantic_fingerprints =
    canonical_array_length(definition.semantic_fingerprints.iter().map(|value| canonical_string_length(value, bounds)))?;
  let total = canonical_map_length([
    ("default_ties", default_ties),
    ("directories_first", canonical_string_length(definition.directories_first, bounds)?),
    ("multi_value_selector", canonical_string_length(definition.multi_value_selector, bounds)?),
    ("name_collation", canonical_string_length(definition.name_collation, bounds)?),
    ("null_missing_policy", canonical_string_length(definition.null_missing_policy, bounds)?),
    ("route_kind", 13),
    ("score_semantics", canonical_string_length(definition.score_semantics, bounds)?),
    ("semantic_fingerprints", semantic_fingerprints),
    ("sort", sort),
  ])?;
  if total > bounds.maximum_value_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_order",
      format!("canonical position order is {total} bytes, cap is {}", bounds.maximum_value_length),
    ));
  }
  Ok(total)
}

fn canonical_string_length(value: &str, bounds: CanonicalValueBounds) -> FormatResult<usize> {
  if value.len() > bounds.maximum_scalar_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_order",
      format!("position order string is {} bytes, scalar cap is {}", value.len(), bounds.maximum_scalar_length),
    ));
  }
  canonical_frame_length(value.len(), "route order string frame overflow")
}

fn canonical_array_length(lengths: impl IntoIterator<Item = FormatResult<usize>>) -> FormatResult<usize> {
  let mut payload = 4usize;
  for length in lengths {
    payload = checked_position_length_add(payload, length?, "route order array length overflow")?;
  }
  canonical_frame_length(payload, "route order array frame overflow")
}

fn canonical_map_length<const COUNT: usize>(entries: [(&str, usize); COUNT]) -> FormatResult<usize> {
  let mut payload = 4usize;
  for (key, value_length) in entries {
    payload = checked_position_length_add(payload, 4, "route order map key frame overflow")?;
    payload = checked_position_length_add(payload, key.len(), "route order map key length overflow")?;
    payload = checked_position_length_add(payload, value_length, "route order map value length overflow")?;
  }
  canonical_frame_length(payload, "route order map frame overflow")
}

fn canonical_frame_length(payload: usize, context: &'static str) -> FormatResult<usize> {
  checked_position_length_add(5, payload, context)
}

fn checked_position_length_add(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_add(right).ok_or_else(|| length_error(context))
}

fn validate_position_sort_comparator(comparator: &str) -> FormatResult<()> {
  match comparator {
    "bytes_binary_order_v1"
    | "utf8_binary_order_v1"
    | "u64_order_v1"
    | "i64_order_v1"
    | "f64_finite_order_v1"
    | "timestamp_ms_order_v1"
    | "bool_order_v1"
    | "null"
    | "missing" => Ok(()),
    _ => Err(order_error(format!("unknown position sort comparator {comparator:?}"))),
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

impl PositionComparatorV1 {
  const fn tag(self) -> u16 {
    match self {
      Self::BytesBinary => 2,
      Self::Utf8Binary => 3,
      Self::U64 => 4,
      Self::I64 => 5,
      Self::FiniteF64 => 6,
      Self::TimestampMs => 7,
      Self::Boolean => 8,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionComponentWriteV1<'a> {
  pub comparator: Option<PositionComparatorV1>,
  pub state: PositionComponentStateV1,
  pub payload: &'a [u8],
}

impl<'a> PositionComponentWriteV1<'a> {
  pub const fn bytes(payload: &'a [u8]) -> Self {
    Self { comparator: Some(PositionComparatorV1::BytesBinary), state: PositionComponentStateV1::Present, payload }
  }

  pub const fn utf8(payload: &'a [u8]) -> Self {
    Self { comparator: Some(PositionComparatorV1::Utf8Binary), state: PositionComponentStateV1::Present, payload }
  }

  pub const fn boolean_payload(payload: &'a [u8]) -> Self {
    Self { comparator: Some(PositionComparatorV1::Boolean), state: PositionComponentStateV1::Present, payload }
  }

  pub const fn typed_null() -> Self {
    Self { comparator: None, state: PositionComponentStateV1::TypedNull, payload: &[] }
  }

  pub const fn missing() -> Self {
    Self { comparator: None, state: PositionComponentStateV1::Missing, payload: &[] }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct LogicalPositionWriteV1<'a> {
  pub order: &'a CompiledRouteOrderV1,
  pub namespace_root: &'a [u8],
  pub file_key_tie: &'a [u8],
  pub record_revision_tie: &'a [u8],
  pub components: &'a [PositionComponentWriteV1<'a>],
}

pub fn encode_logical_position(request: &LogicalPositionWriteV1<'_>) -> FormatResult<Vec<u8>> {
  let hash_width = request.order.hash_algorithm.hash_length();
  for (name, identity) in [
    ("namespace root", request.namespace_root),
    ("FileKey tie", request.file_key_tie),
    ("record revision tie", request.record_revision_tie),
  ] {
    if identity.len() != hash_width || identity.iter().all(|byte| *byte == 0) {
      return Err(error(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "invalid_position_cursor",
        format!("position {name} must be a nonzero {hash_width}-byte identity"),
      ));
    }
  }
  if request.order.fingerprint.len() != hash_width || request.order.fingerprint.iter().all(|byte| *byte == 0) {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "invalid_position_cursor",
      "compiled position order has an invalid fingerprint",
    ));
  }
  if request.components.len() != request.order.component_count {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "invalid_position_cursor",
      format!("position order declares {} components but the tuple contains {}", request.order.component_count, request.components.len()),
    ));
  }

  let mut tuple_length = 0usize;
  for component in request.components {
    tuple_length = tuple_length
      .checked_add(8)
      .and_then(|length| length.checked_add(component.payload.len()))
      .ok_or_else(|| length_error("position tuple length overflow"))?;
    let total = FIXED_WITHOUT_HASHES
      .checked_add(4 * hash_width)
      .and_then(|length| length.checked_add(tuple_length))
      .ok_or_else(|| length_error("position total length overflow"))?;
    if total > MAX_DECODED_LENGTH {
      return Err(error(
        MalformedInputClass::AllocationAmplification,
        "invalid_position_cursor",
        format!("decoded position exceeds {MAX_DECODED_LENGTH} bytes"),
      ));
    }
    validate_write_component(component)?;
  }

  let total_length = FIXED_WITHOUT_HASHES + 4 * hash_width + tuple_length;
  let mut decoded = Vec::new();
  decoded.try_reserve_exact(total_length).map_err(|source| {
    error(
      MalformedInputClass::AllocationAmplification,
      "invalid_position_cursor",
      format!("cannot reserve {total_length} position bytes: {source}"),
    )
  })?;
  decoded.extend_from_slice(b"APOS");
  decoded.extend_from_slice(&1u16.to_le_bytes());
  decoded.extend_from_slice(&(request.order.route as u16).to_le_bytes());
  decoded.extend_from_slice(&(total_length as u32).to_le_bytes());
  decoded.extend_from_slice(&request.order.hash_algorithm.to_u16().to_le_bytes());
  decoded.push(request.components.len() as u8);
  decoded.push(0);
  decoded.extend_from_slice(&request.order.fingerprint);
  decoded.extend_from_slice(request.namespace_root);
  decoded.extend_from_slice(&(tuple_length as u32).to_le_bytes());
  decoded.extend_from_slice(request.file_key_tie);
  decoded.extend_from_slice(request.record_revision_tie);
  for component in request.components {
    let tag = component.comparator.map_or(0, PositionComparatorV1::tag);
    let state = match component.state {
      PositionComponentStateV1::Present => 0,
      PositionComponentStateV1::TypedNull => 1,
      PositionComponentStateV1::Missing => 2,
    };
    decoded.extend_from_slice(&tag.to_le_bytes());
    decoded.push(state);
    decoded.push(0);
    decoded.extend_from_slice(&(component.payload.len() as u32).to_le_bytes());
    decoded.extend_from_slice(component.payload);
  }
  let checksum = crc32fast::hash(&decoded);
  decoded.extend_from_slice(&checksum.to_le_bytes());
  debug_assert_eq!(decoded.len(), total_length);
  Ok(URL_SAFE_NO_PAD.encode(decoded).into_bytes())
}

fn validate_write_component(component: &PositionComponentWriteV1<'_>) -> FormatResult<()> {
  match component.state {
    PositionComponentStateV1::Present => {
      let comparator = component.comparator.ok_or_else(|| {
        error(
          MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
          "invalid_position_cursor",
          "present position components require a comparator",
        )
      })?;
      validate_present_component(comparator.tag(), component.payload)?;
    }
    PositionComponentStateV1::TypedNull => {
      if component.comparator.is_some() || !component.payload.is_empty() {
        return Err(error(
          MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
          "invalid_position_cursor",
          "null and missing position components require zero tag and payload",
        ));
      }
    }
    PositionComponentStateV1::Missing => {
      if component.comparator.is_some() || !component.payload.is_empty() {
        return Err(error(
          MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
          "invalid_position_cursor",
          "null and missing position components require zero tag and payload",
        ));
      }
    }
  }
  Ok(())
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

fn order_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "invalid_position_order", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
