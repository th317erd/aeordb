use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

use super::config_value::CanonicalConfigValueV1;
use super::field_definition::ConverterDefinitionV1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationConverterErrorClassV0 {
  InvalidSourceValue,
  ResourceLimit,
  UnsupportedDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationConverterErrorV0 {
  class: MigrationConverterErrorClassV0,
  code: &'static str,
  context: String,
}

impl MigrationConverterErrorV0 {
  pub fn class(&self) -> MigrationConverterErrorClassV0 {
    self.class
  }

  pub fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for MigrationConverterErrorV0 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for MigrationConverterErrorV0 {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPostingV0 {
  pub posting_key: Vec<u8>,
  pub coordinate: u64,
}

pub fn compile_migration_value_v0(
  definition: &ConverterDefinitionV1<'_>,
  value: &CanonicalConfigValueV1,
) -> Result<Vec<MigrationPostingV0>, MigrationConverterErrorV0> {
  let CanonicalConfigValueV1::Bytes(source) = value else {
    return Err(error(
      MigrationConverterErrorClassV0::InvalidSourceValue,
      "legacy_converter_requires_bytes",
      "migration-only v0 converters accept canonical bytes values only",
    ));
  };

  let posting_keys = expand_posting_keys(definition, source)?;
  if posting_keys.len() > definition.max_output_values as usize {
    return Err(limit_error("converter_output_count_limit", posting_keys.len() as u64, u64::from(definition.max_output_values)));
  }

  let mut postings = Vec::new();
  postings.try_reserve_exact(posting_keys.len()).map_err(|source| {
    error(
      MigrationConverterErrorClassV0::ResourceLimit,
      "legacy_posting_reserve",
      format!("cannot reserve bounded migration posting output: {source}"),
    )
  })?;
  let mut total_output_bytes = 0u64;
  for posting_key in posting_keys {
    let posting_length = posting_key.len() as u64;
    if posting_length > u64::from(definition.max_output_value_bytes) {
      return Err(limit_error("converter_single_output_limit", posting_length, u64::from(definition.max_output_value_bytes)));
    }
    total_output_bytes = total_output_bytes.checked_add(posting_length).ok_or_else(|| {
      error(MigrationConverterErrorClassV0::ResourceLimit, "converter_total_output_overflow", "migration posting byte count overflowed u64")
    })?;
    if total_output_bytes > definition.max_total_output_bytes {
      return Err(limit_error("converter_total_output_limit", total_output_bytes, definition.max_total_output_bytes));
    }
    let scalar = legacy_scalar(definition, &posting_key)?;
    let coordinate = migration_scalar_coordinate_v0(scalar)?;
    postings.push(MigrationPostingV0 { posting_key, coordinate });
  }
  Ok(postings)
}

fn expand_posting_keys(definition: &ConverterDefinitionV1<'_>, source: &[u8]) -> Result<Vec<Vec<u8>>, MigrationConverterErrorV0> {
  match definition.converter_id {
    0x800a => legacy_trigrams(source, definition.max_output_values as usize),
    0x800b..=0x800d => legacy_phonetic(definition.converter_id, source, definition.max_output_values as usize),
    0x8001..=0x8009 => {
      let mut posting = Vec::new();
      posting.try_reserve_exact(source.len()).map_err(reserve_error)?;
      posting.extend_from_slice(source);
      Ok(vec![posting])
    }
    converter_id => Err(error(
      MigrationConverterErrorClassV0::UnsupportedDefinition,
      "legacy_converter_unknown",
      format!("converter 0x{converter_id:04x} is not a migration-only v0 converter"),
    )),
  }
}

fn legacy_trigrams(source: &[u8], maximum_values: usize) -> Result<Vec<Vec<u8>>, MigrationConverterErrorV0> {
  let text = legacy_utf8_or_empty(source);
  let folded = legacy_folded_characters(text)?;
  let mut seen = HashSet::new();
  seen.try_reserve(maximum_values).map_err(reserve_error)?;
  let mut postings = Vec::new();
  postings.try_reserve(maximum_values.min(folded.len())).map_err(reserve_error)?;

  let mut start = 0usize;
  while start < folded.len() {
    while start < folded.len() && !folded[start].is_alphanumeric() {
      start += 1;
    }
    let mut end = start;
    while end < folded.len() && folded[end].is_alphanumeric() {
      end += 1;
    }
    if start < end {
      let word = &folded[start..end];
      for window_start in 0..word.len().saturating_add(1) {
        let characters = [
          legacy_padded_word_character(word, window_start),
          legacy_padded_word_character(word, window_start + 1),
          legacy_padded_word_character(word, window_start + 2),
        ];
        push_unique_bounded(&mut postings, &mut seen, legacy_token_key(&characters)?, maximum_values)?;
      }
    }
    start = end.saturating_add(1);
  }
  Ok(postings)
}

fn legacy_folded_characters(text: &str) -> Result<Vec<char>, MigrationConverterErrorV0> {
  let mut folded = Vec::new();
  folded.try_reserve(text.len()).map_err(reserve_error)?;
  for character in text.chars() {
    for lowercase in character.to_lowercase() {
      if folded.len() == folded.capacity() {
        folded.try_reserve(1).map_err(reserve_error)?;
      }
      folded.push(lowercase);
    }
  }
  Ok(folded)
}

fn legacy_padded_word_character(word: &[char], position: usize) -> char {
  if position < 2 {
    return ' ';
  }
  match word.get(position - 2) {
    Some(character) => *character,
    None => ' ',
  }
}

fn legacy_token_key(characters: &[char]) -> Result<Vec<u8>, MigrationConverterErrorV0> {
  let encoded_length = characters.iter().map(|character| character.len_utf8()).sum::<usize>();
  let mut posting = Vec::new();
  posting.try_reserve_exact(encoded_length).map_err(reserve_error)?;
  let mut buffer = [0u8; 4];
  for character in characters {
    posting.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
  }
  Ok(posting)
}

fn legacy_phonetic(converter_id: u16, source: &[u8], maximum_values: usize) -> Result<Vec<Vec<u8>>, MigrationConverterErrorV0> {
  let text = legacy_utf8_or_empty(source);
  let mut unique = HashSet::new();
  unique.try_reserve(maximum_values).map_err(reserve_error)?;
  for word in text.split_whitespace().filter(|word| word.chars().any(char::is_alphabetic)) {
    let code = match converter_id {
      0x800b => nonempty(crate::engine::phonetic::soundex(word)),
      0x800c => nonempty(crate::engine::phonetic::dmetaphone_primary(word)),
      0x800d => match crate::engine::phonetic::dmetaphone_alt(word) {
        Some(code) => Some(code),
        None => nonempty(crate::engine::phonetic::dmetaphone_primary(word)),
      },
      _ => None,
    };
    if let Some(code) = code {
      if !unique.contains(code.as_bytes()) && unique.len() >= maximum_values {
        return Err(limit_error("converter_output_count_limit", unique.len() as u64 + 1, maximum_values as u64));
      }
      unique.insert(code.into_bytes());
    }
  }
  let mut postings = unique.into_iter().collect::<Vec<_>>();
  postings.sort_unstable();
  Ok(postings)
}

fn nonempty(value: String) -> Option<String> {
  if value.is_empty() {
    None
  } else {
    Some(value)
  }
}

fn legacy_utf8_or_empty(value: &[u8]) -> &str {
  match std::str::from_utf8(value) {
    Ok(text) => text,
    Err(error) => {
      ignore_legacy_parser_miss(error);
      ""
    }
  }
}

fn legacy_parse_candidate<T, E>(candidate: Result<T, E>) -> Option<T> {
  match candidate {
    Ok(value) => Some(value),
    Err(error) => {
      ignore_legacy_parser_miss(error);
      None
    }
  }
}

fn push_unique_bounded(
  postings: &mut Vec<Vec<u8>>,
  seen: &mut HashSet<Vec<u8>>,
  posting: Vec<u8>,
  maximum_values: usize,
) -> Result<(), MigrationConverterErrorV0> {
  if seen.contains(&posting) {
    return Ok(());
  }
  if postings.len() >= maximum_values {
    return Err(limit_error("converter_output_count_limit", postings.len() as u64 + 1, maximum_values as u64));
  }
  if postings.len() == postings.capacity() {
    postings.try_reserve(1).map_err(reserve_error)?;
  }
  let mut seen_key = Vec::new();
  seen_key.try_reserve_exact(posting.len()).map_err(reserve_error)?;
  seen_key.extend_from_slice(&posting);
  seen.insert(seen_key);
  postings.push(posting);
  Ok(())
}

fn legacy_scalar(definition: &ConverterDefinitionV1<'_>, value: &[u8]) -> Result<f64, MigrationConverterErrorV0> {
  match definition.converter_id {
    0x8001 => Ok(hash_scalar(value)),
    0x8002 => {
      Ok(unsigned_scalar(value, 1, parameter_unsigned(definition.parameters, 0, 1)?, parameter_unsigned(definition.parameters, 1, 1)?))
    }
    0x8003 => {
      Ok(unsigned_scalar(value, 2, parameter_unsigned(definition.parameters, 0, 2)?, parameter_unsigned(definition.parameters, 2, 2)?))
    }
    0x8004 => {
      Ok(unsigned_scalar(value, 4, parameter_unsigned(definition.parameters, 0, 4)?, parameter_unsigned(definition.parameters, 4, 4)?))
    }
    0x8005 => {
      Ok(unsigned_scalar(value, 8, parameter_unsigned(definition.parameters, 0, 8)?, parameter_unsigned(definition.parameters, 8, 8)?))
    }
    0x8006 => signed_scalar(definition.parameters, value),
    0x8007 => float_scalar(definition.parameters, value),
    0x8008 => string_scalar(definition.parameters, value),
    0x8009 => timestamp_scalar(definition.parameters, value),
    0x800a..=0x800d => Ok(hash_scalar_little_endian(value)),
    converter_id => Err(error(
      MigrationConverterErrorClassV0::UnsupportedDefinition,
      "legacy_converter_unknown",
      format!("converter 0x{converter_id:04x} has no captured scalar"),
    )),
  }
}

fn hash_scalar(value: &[u8]) -> f64 {
  let Some(bytes) = value.get(..8) else {
    return 0.0;
  };
  let mut fixed = [0u8; 8];
  fixed.copy_from_slice(bytes);
  u64::from_be_bytes(fixed) as f64 / u64::MAX as f64
}

fn hash_scalar_little_endian(value: &[u8]) -> f64 {
  let digest = blake3::hash(value);
  let mut fixed = [0u8; 8];
  fixed.copy_from_slice(&digest.as_bytes()[..8]);
  u64::from_le_bytes(fixed) as f64 / u64::MAX as f64
}

fn unsigned_scalar(value: &[u8], width: usize, minimum: u64, maximum: u64) -> f64 {
  let Some(bytes) = value.get(..width) else {
    return 0.0;
  };
  if minimum == maximum {
    return 0.5;
  }
  let numeric = bytes.iter().fold(0u64, |accumulator, byte| (accumulator << 8) | u64::from(*byte));
  let width_mask = if width == 8 { u64::MAX } else { (1u64 << (width * 8)) - 1 };
  let range = maximum.wrapping_sub(minimum) & width_mask;
  numeric.saturating_sub(minimum) as f64 / range as f64
}

fn signed_scalar(parameters: &[u8], value: &[u8]) -> Result<f64, MigrationConverterErrorV0> {
  let Some(bytes) = value.get(..8) else {
    return Ok(0.0);
  };
  let minimum = parameter_i64(parameters, 0)?;
  let maximum = parameter_i64(parameters, 8)?;
  if minimum == maximum {
    return Ok(0.5);
  }
  let mut fixed = [0u8; 8];
  fixed.copy_from_slice(bytes);
  let numeric = i64::from_be_bytes(fixed);
  Ok(((numeric as i128 - minimum as i128) as f64 / (maximum as i128 - minimum as i128) as f64).clamp(0.0, 1.0))
}

fn float_scalar(parameters: &[u8], value: &[u8]) -> Result<f64, MigrationConverterErrorV0> {
  let Some(bytes) = value.get(..8) else {
    return Ok(0.0);
  };
  let mut fixed = [0u8; 8];
  fixed.copy_from_slice(bytes);
  let numeric = f64::from_be_bytes(fixed);
  if numeric.is_nan() {
    return Ok(0.0);
  }
  let minimum = parameter_f64(parameters, 0)?;
  let maximum = parameter_f64(parameters, 8)?;
  if minimum == maximum {
    return Ok(0.5);
  }
  Ok(((numeric - minimum) / (maximum - minimum)).clamp(0.0, 1.0))
}

fn string_scalar(parameters: &[u8], value: &[u8]) -> Result<f64, MigrationConverterErrorV0> {
  if value.is_empty() {
    return Ok(0.0);
  }
  let maximum_length = parameter_unsigned(parameters, 0, 4)? as usize;
  let first_byte_scalar = f64::from(value[0]) / 255.0;
  let length_scalar = (value.len() as f64 / maximum_length as f64).min(1.0);
  Ok((first_byte_scalar * 0.7 + length_scalar * 0.3).clamp(0.0, 1.0))
}

fn timestamp_scalar(parameters: &[u8], value: &[u8]) -> Result<f64, MigrationConverterErrorV0> {
  let minimum = parameter_i64(parameters, 0)?;
  let maximum = parameter_i64(parameters, 8)?;
  if minimum == maximum {
    return Ok(0.5);
  }
  let milliseconds = legacy_timestamp_milliseconds(value);
  Ok(((milliseconds as i128 - minimum as i128) as f64 / (maximum as i128 - minimum as i128) as f64).clamp(0.0, 1.0))
}

fn legacy_timestamp_milliseconds(value: &[u8]) -> i64 {
  if value.len() == 8 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(value);
    return i64::from_be_bytes(bytes);
  }
  let text = match std::str::from_utf8(value) {
    Ok(text) => text,
    Err(error) => {
      ignore_legacy_parser_miss(error);
      return 0;
    }
  };
  let text = text.trim();
  if text.is_empty() {
    return 0;
  }
  if let Some(timestamp) = legacy_parse_candidate(DateTime::parse_from_rfc3339(text)) {
    return timestamp.with_timezone(&Utc).timestamp_millis();
  }
  if let Some(timestamp) = legacy_parse_candidate(NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")) {
    return timestamp.and_utc().timestamp_millis();
  }
  if let Some(timestamp) = legacy_parse_candidate(NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")) {
    return timestamp.and_utc().timestamp_millis();
  }
  if let Some(date) = legacy_parse_candidate(NaiveDate::parse_from_str(text, "%Y-%m-%d")) {
    if let Some(timestamp) = date.and_hms_opt(0, 0, 0) {
      return timestamp.and_utc().timestamp_millis();
    }
  }
  if let Some(milliseconds) = legacy_parse_candidate(text.parse::<i64>()) {
    return milliseconds;
  }
  0
}

fn ignore_legacy_parser_miss<E>(_error: E) {}

fn migration_scalar_coordinate_v0(scalar: f64) -> Result<u64, MigrationConverterErrorV0> {
  if scalar <= 0.0 {
    return Ok(0);
  }
  if scalar >= 1.0 {
    return Ok(u64::MAX);
  }
  if !scalar.is_finite() {
    return Err(error(
      MigrationConverterErrorClassV0::InvalidSourceValue,
      "legacy_scalar_nonfinite",
      "legacy converter produced a nonfinite interior scalar",
    ));
  }

  let bits = scalar.to_bits();
  let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
  if exponent_bits == 0 {
    return Ok(0);
  }
  let significand = (1u128 << 52) | u128::from(bits & ((1u64 << 52) - 1));
  let shift = exponent_bits - 1023 + 12;
  let coordinate = if shift >= 0 { significand << shift } else { significand >> -shift };
  u64::try_from(coordinate).map_err(|source| {
    error(
      MigrationConverterErrorClassV0::UnsupportedDefinition,
      "legacy_scalar_coordinate_overflow",
      format!("ratified scalar conversion exceeded u64: {source}"),
    )
  })
}

fn parameter_unsigned(parameters: &[u8], offset: usize, width: usize) -> Result<u64, MigrationConverterErrorV0> {
  let bytes = parameters.get(offset..offset + width).ok_or_else(parameter_error)?;
  Ok(bytes.iter().rev().fold(0u64, |accumulator, byte| (accumulator << 8) | u64::from(*byte)))
}

fn parameter_i64(parameters: &[u8], offset: usize) -> Result<i64, MigrationConverterErrorV0> {
  let bytes = parameters.get(offset..offset + 8).ok_or_else(parameter_error)?;
  let mut fixed = [0u8; 8];
  fixed.copy_from_slice(bytes);
  Ok(i64::from_le_bytes(fixed))
}

fn parameter_f64(parameters: &[u8], offset: usize) -> Result<f64, MigrationConverterErrorV0> {
  let bytes = parameters.get(offset..offset + 8).ok_or_else(parameter_error)?;
  let mut fixed = [0u8; 8];
  fixed.copy_from_slice(bytes);
  Ok(f64::from_le_bytes(fixed))
}

fn parameter_error() -> MigrationConverterErrorV0 {
  error(MigrationConverterErrorClassV0::UnsupportedDefinition, "legacy_converter_parameter", "migration converter parameters are truncated")
}

fn reserve_error(source: std::collections::TryReserveError) -> MigrationConverterErrorV0 {
  error(
    MigrationConverterErrorClassV0::ResourceLimit,
    "legacy_token_reserve",
    format!("cannot reserve bounded migration token workspace: {source}"),
  )
}

fn limit_error(code: &'static str, observed: u64, maximum: u64) -> MigrationConverterErrorV0 {
  error(MigrationConverterErrorClassV0::ResourceLimit, code, format!("observed {observed} exceeds {maximum}"))
}

fn error(class: MigrationConverterErrorClassV0, code: &'static str, context: impl Into<String>) -> MigrationConverterErrorV0 {
  MigrationConverterErrorV0 { class, code, context: context.into() }
}
