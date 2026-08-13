use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

use chrono::DateTime;

use crate::engine::HashAlgorithm;

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value, encode_canonical_value};
use super::field_definition::{ConverterDefinitionV1, decode_converter_definition};
use super::index_semantic_registry::{ConverterRegistryEntryV1, converter_registry_entry};

const EXACT_POSTING_DOMAIN: &[u8] = b"aeordb.typed-exact-posting.v1\0";
const EXACT_COORDINATE_DOMAIN: &[u8] = b"aeordb.index.exact-coordinate.v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexSemanticErrorClassV1 {
  UnsupportedDefinition,
  InvalidSourceValue,
  ResourceLimit,
  MalformedPostingKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSemanticErrorV1 {
  class: IndexSemanticErrorClassV1,
  code: &'static str,
  context: String,
}

impl IndexSemanticErrorV1 {
  pub fn class(&self) -> IndexSemanticErrorClassV1 {
    self.class
  }

  pub fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for IndexSemanticErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for IndexSemanticErrorV1 {}

pub type IndexSemanticResultV1<T> = Result<T, IndexSemanticErrorV1>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledPostingKeyV1 {
  pub posting_key: Vec<u8>,
  pub coordinate: u64,
  pub expansion_ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledSourceValueV1 {
  pub canonical_value: Vec<u8>,
  pub postings: Vec<CompiledPostingKeyV1>,
}

#[derive(Debug, Clone)]
pub struct ConverterRuntimeV1<'a> {
  definition: ConverterDefinitionV1<'a>,
  registry: &'static ConverterRegistryEntryV1,
}

impl<'a> ConverterRuntimeV1<'a> {
  pub fn from_encoded(value: &'a [u8], hash_algorithm: HashAlgorithm) -> IndexSemanticResultV1<Self> {
    let definition = decode_converter_definition(value, hash_algorithm).map_err(|source| {
      error(
        IndexSemanticErrorClassV1::UnsupportedDefinition,
        "converter_definition_invalid",
        format!("{}: {}", source.code(), source.context()),
      )
    })?;
    Self::from_definition(definition)
  }

  pub(crate) fn from_definition(definition: ConverterDefinitionV1<'a>) -> IndexSemanticResultV1<Self> {
    let registry = converter_registry_entry(definition.converter_id).ok_or_else(|| {
      error(
        IndexSemanticErrorClassV1::UnsupportedDefinition,
        "converter_registry_missing",
        format!("converter 0x{:04x} is not registered", definition.converter_id),
      )
    })?;
    if definition.name != registry.name
      || definition.corrected != registry.corrected
      || definition.source_type_mask != registry.source_type_mask
    {
      return Err(error(
        IndexSemanticErrorClassV1::UnsupportedDefinition,
        "converter_registry_mismatch",
        "decoded converter does not match its permanent runtime registry row",
      ));
    }
    Ok(Self { definition, registry })
  }

  pub fn definition(&self) -> &ConverterDefinitionV1<'a> {
    &self.definition
  }

  pub fn registry(&self) -> &'static ConverterRegistryEntryV1 {
    self.registry
  }

  pub fn compile_source_value(&self, value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<CompiledSourceValueV1> {
    self.compile_value(value)
  }

  pub fn compile_query_literal(&self, value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<CompiledSourceValueV1> {
    self.compile_value(value)
  }

  pub fn exact_values_equal(&self, left: &[u8], right: &[u8]) -> IndexSemanticResultV1<bool> {
    decode_canonical_value(left, CanonicalValueBounds::SOURCE_VALUE)
      .map_err(|source| malformed_value("left canonical source value", source.to_string()))?;
    decode_canonical_value(right, CanonicalValueBounds::SOURCE_VALUE)
      .map_err(|source| malformed_value("right canonical source value", source.to_string()))?;
    Ok(left == right)
  }

  pub fn compare_posting_keys(&self, left: &[u8], right: &[u8]) -> IndexSemanticResultV1<Ordering> {
    match self.definition.converter_id {
      0x0001 => {
        validate_typed_exact_key(left)?;
        validate_typed_exact_key(right)?;
        Ok(left.cmp(right))
      }
      0x0002 => Ok(left.cmp(right)),
      0x0003 => {
        std::str::from_utf8(left).map_err(|source| malformed_key(format!("left UTF-8 posting key is invalid: {source}")))?;
        std::str::from_utf8(right).map_err(|source| malformed_key(format!("right UTF-8 posting key is invalid: {source}")))?;
        Ok(left.cmp(right))
      }
      0x0004 => compare_fixed(left, right, u64::from_le_bytes),
      0x0005 | 0x0007 => compare_fixed(left, right, i64::from_le_bytes),
      0x0006 => {
        let left_bits = read_u64_key(left)?;
        let right_bits = read_u64_key(right)?;
        if left_bits == (-0.0f64).to_bits() || right_bits == (-0.0f64).to_bits() {
          return Err(malformed_key("f64 posting key encodes noncanonical negative zero"));
        }
        let left = f64::from_bits(left_bits);
        let right = f64::from_bits(right_bits);
        if !left.is_finite() || !right.is_finite() {
          return Err(malformed_key("nonfinite f64 posting key"));
        }
        left.partial_cmp(&right).ok_or_else(|| malformed_key("unordered f64 posting key"))
      }
      0x0008 => match (left, right) {
        ([left], [right]) if *left <= 1 && *right <= 1 => Ok(left.cmp(right)),
        _ => Err(malformed_key("bool posting key must be one canonical byte")),
      },
      0x0009..=0x000c => Ok(left.cmp(right)),
      _ => Err(error(
        IndexSemanticErrorClassV1::UnsupportedDefinition,
        "legacy_converter_runtime_pending",
        format!("migration converter 0x{:04x} requires its isolated v0 adapter", self.definition.converter_id),
      )),
    }
  }

  fn compile_value(&self, value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<CompiledSourceValueV1> {
    if !self.definition.corrected {
      return Err(error(
        IndexSemanticErrorClassV1::UnsupportedDefinition,
        "legacy_converter_runtime_pending",
        format!("migration converter 0x{:04x} requires its isolated v0 adapter", self.definition.converter_id),
      ));
    }

    let normalized_value;
    let value = if self.definition.converter_id == 0x0006
      && matches!(value, CanonicalConfigValueV1::FloatBits(bits) if *bits == (-0.0f64).to_bits())
    {
      normalized_value = CanonicalConfigValueV1::FloatBits(0.0f64.to_bits());
      &normalized_value
    } else {
      value
    };
    let canonical_value = encode_source(value)?;
    self.enforce_input_limit(canonical_value.len())?;
    let (posting_key, coordinate) = match self.definition.converter_id {
      0x0001 => compile_exact(&canonical_value)?,
      0x0002 => {
        let bytes = require_bytes(value)?.to_vec();
        let coordinate = ordered_bytes_coordinate(&bytes);
        (bytes, coordinate)
      }
      0x0003 => {
        let bytes = require_string(value)?.as_bytes().to_vec();
        let coordinate = ordered_bytes_coordinate(&bytes);
        (bytes, coordinate)
      }
      0x0004 => {
        let number = canonical_u64(value)?;
        (number.to_le_bytes().to_vec(), number)
      }
      0x0005 => {
        let number = canonical_i64(value)?;
        (number.to_le_bytes().to_vec(), sign_flipped_coordinate(number))
      }
      0x0006 => {
        let number = canonical_f64(value)?;
        let bits = number.to_bits();
        (bits.to_le_bytes().to_vec(), sortable_f64_coordinate(bits))
      }
      0x0007 => {
        let milliseconds = canonical_timestamp_milliseconds(value)?;
        (milliseconds.to_le_bytes().to_vec(), sign_flipped_coordinate(milliseconds))
      }
      0x0008 => {
        let value = require_bool(value)?;
        (vec![u8::from(value)], if value { u64::MAX } else { 0 })
      }
      0x0009..=0x000c => {
        return Err(error(
          IndexSemanticErrorClassV1::UnsupportedDefinition,
          "token_converter_runtime_pending",
          format!("token converter 0x{:04x} requires its frozen expansion runtime", self.definition.converter_id),
        ));
      }
      converter_id => {
        return Err(error(
          IndexSemanticErrorClassV1::UnsupportedDefinition,
          "converter_runtime_missing",
          format!("converter 0x{converter_id:04x} has no corrected runtime"),
        ));
      }
    };

    self.enforce_output_limits(&posting_key)?;
    Ok(CompiledSourceValueV1 { canonical_value, postings: vec![CompiledPostingKeyV1 { posting_key, coordinate, expansion_ordinal: 0 }] })
  }

  fn enforce_input_limit(&self, length: usize) -> IndexSemanticResultV1<()> {
    if length as u64 > self.definition.max_input_bytes {
      return Err(limit_error("converter_input_limit", length as u64, self.definition.max_input_bytes));
    }
    Ok(())
  }

  fn enforce_output_limits(&self, posting_key: &[u8]) -> IndexSemanticResultV1<()> {
    if posting_key.len() as u64 > u64::from(self.definition.max_output_value_bytes) {
      return Err(limit_error("converter_single_output_limit", posting_key.len() as u64, self.definition.max_output_value_bytes as u64));
    }
    if posting_key.len() as u64 > self.definition.max_total_output_bytes {
      return Err(limit_error("converter_total_output_limit", posting_key.len() as u64, self.definition.max_total_output_bytes));
    }
    Ok(())
  }
}

fn compile_exact(canonical: &[u8]) -> IndexSemanticResultV1<(Vec<u8>, u64)> {
  if matches!(canonical.first(), Some(0x09 | 0x0a)) {
    return Err(invalid_value("typed exact accepts canonical scalar values only"));
  }
  let digest = digest_blake3(EXACT_POSTING_DOMAIN, canonical);
  let mut posting_key = Vec::with_capacity(33);
  posting_key.push(canonical[0]);
  posting_key.extend_from_slice(&digest);
  let coordinate_digest = digest_blake3(EXACT_COORDINATE_DOMAIN, &posting_key);
  let mut coordinate_bytes = [0u8; 8];
  coordinate_bytes.copy_from_slice(&coordinate_digest[..8]);
  let coordinate = u64::from_be_bytes(coordinate_bytes);
  Ok((posting_key, coordinate))
}

fn validate_typed_exact_key(value: &[u8]) -> IndexSemanticResultV1<()> {
  if value.len() != 33 || !matches!(value.first(), Some(0x01..=0x08)) {
    return Err(malformed_key("typed exact posting key must be one scalar type tag followed by a 32-byte BLAKE3 digest"));
  }
  Ok(())
}

fn encode_source(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<Vec<u8>> {
  encode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE)
    .map_err(|source| malformed_value("canonical source value", source.to_string()))
}

fn require_bytes(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<&[u8]> {
  match value {
    CanonicalConfigValueV1::Bytes(value) => Ok(value),
    _ => Err(invalid_value("converter requires a bytes source value")),
  }
}

fn require_string(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<&str> {
  match value {
    CanonicalConfigValueV1::String(value) => Ok(value),
    _ => Err(invalid_value("converter requires a UTF-8 source value")),
  }
}

fn require_bool(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<bool> {
  match value {
    CanonicalConfigValueV1::Boolean(value) => Ok(*value),
    _ => Err(invalid_value("converter requires a bool source value")),
  }
}

fn canonical_u64(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<u64> {
  match value {
    CanonicalConfigValueV1::Unsigned(value) => Ok(*value),
    CanonicalConfigValueV1::Signed(value) if *value >= 0 => Ok(*value as u64),
    _ => Err(invalid_value("u64 converter requires u64 or nonnegative i64")),
  }
}

fn canonical_i64(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<i64> {
  match value {
    CanonicalConfigValueV1::Signed(value) => Ok(*value),
    CanonicalConfigValueV1::Unsigned(value) => match i64::try_from(*value) {
      Ok(value) => Ok(value),
      Err(error) => Err(invalid_value(format!("u64 source value exceeds i64::MAX: {error}"))),
    },
    _ => Err(invalid_value("i64 converter requires i64 or in-range u64")),
  }
}

fn canonical_f64(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<f64> {
  let number = match value {
    CanonicalConfigValueV1::FloatBits(bits) => f64::from_bits(*bits),
    CanonicalConfigValueV1::Signed(value) => {
      let number = *value as f64;
      if number as i128 != *value as i128 {
        return Err(invalid_value("i64 source value does not round-trip exactly through f64"));
      }
      number
    }
    CanonicalConfigValueV1::Unsigned(value) => {
      let number = *value as f64;
      if number as u128 != *value as u128 {
        return Err(invalid_value("u64 source value does not round-trip exactly through f64"));
      }
      number
    }
    _ => return Err(invalid_value("f64 converter requires finite f64 or exactly representable integer")),
  };
  if !number.is_finite() {
    return Err(invalid_value("f64 converter rejects NaN and infinity"));
  }
  Ok(if number == 0.0 { 0.0 } else { number })
}

fn canonical_timestamp_milliseconds(value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<i64> {
  match value {
    CanonicalConfigValueV1::Signed(value) => Ok(*value),
    CanonicalConfigValueV1::Unsigned(value) => match i64::try_from(*value) {
      Ok(value) => Ok(value),
      Err(source) => Err(invalid_value(format!("timestamp milliseconds exceed i64::MAX: {source}"))),
    },
    CanonicalConfigValueV1::String(value) => match DateTime::parse_from_rfc3339(value) {
      Ok(timestamp) => Ok(timestamp.timestamp_millis()),
      Err(source) => Err(invalid_value(format!("timestamp requires strict RFC 3339 with an explicit offset: {source}"))),
    },
    _ => Err(invalid_value("timestamp requires integer milliseconds or strict RFC 3339 text")),
  }
}

fn ordered_bytes_coordinate(bytes: &[u8]) -> u64 {
  let mut prefix = [0u8; 8];
  let length = bytes.len().min(prefix.len());
  prefix[..length].copy_from_slice(&bytes[..length]);
  u64::from_be_bytes(prefix)
}

fn sign_flipped_coordinate(value: i64) -> u64 {
  (value as u64) ^ (1 << 63)
}

fn sortable_f64_coordinate(bits: u64) -> u64 {
  if bits & (1 << 63) != 0 {
    !bits
  } else {
    bits ^ (1 << 63)
  }
}

fn digest_blake3(domain: &[u8], value: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(domain);
  hasher.update(value);
  *hasher.finalize().as_bytes()
}

fn read_u64_key(value: &[u8]) -> IndexSemanticResultV1<u64> {
  let bytes: [u8; 8] = match value.try_into() {
    Ok(bytes) => bytes,
    Err(source) => return Err(malformed_key(format!("numeric posting key must be exactly eight bytes: {source}"))),
  };
  Ok(u64::from_le_bytes(bytes))
}

fn compare_fixed<T: Ord>(left: &[u8], right: &[u8], decode: impl Fn([u8; 8]) -> T) -> IndexSemanticResultV1<Ordering> {
  let left: [u8; 8] = match left.try_into() {
    Ok(bytes) => bytes,
    Err(source) => return Err(malformed_key(format!("left numeric posting key must be exactly eight bytes: {source}"))),
  };
  let right: [u8; 8] = match right.try_into() {
    Ok(bytes) => bytes,
    Err(source) => return Err(malformed_key(format!("right numeric posting key must be exactly eight bytes: {source}"))),
  };
  Ok(decode(left).cmp(&decode(right)))
}

fn invalid_value(context: impl Into<String>) -> IndexSemanticErrorV1 {
  error(IndexSemanticErrorClassV1::InvalidSourceValue, "converter_invalid_source_value", context)
}

fn malformed_value(label: &str, context: impl fmt::Display) -> IndexSemanticErrorV1 {
  error(IndexSemanticErrorClassV1::InvalidSourceValue, "converter_malformed_canonical_value", format!("{label}: {context}"))
}

fn malformed_key(context: impl Into<String>) -> IndexSemanticErrorV1 {
  error(IndexSemanticErrorClassV1::MalformedPostingKey, "converter_malformed_posting_key", context)
}

fn limit_error(code: &'static str, observed: u64, maximum: u64) -> IndexSemanticErrorV1 {
  error(IndexSemanticErrorClassV1::ResourceLimit, code, format!("observed {observed} exceeds {maximum}"))
}

fn error(class: IndexSemanticErrorClassV1, code: &'static str, context: impl Into<String>) -> IndexSemanticErrorV1 {
  IndexSemanticErrorV1 { class, code, context: context.into() }
}
