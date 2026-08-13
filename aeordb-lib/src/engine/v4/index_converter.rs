use std::cmp::Ordering;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use chrono::DateTime;

use crate::engine::HashAlgorithm;

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value, encode_canonical_value};
use super::field_definition::{ConverterDefinitionV1, decode_converter_definition};
use super::index_converter_v0::{MigrationConverterErrorClassV0, compile_migration_value_v0};
use super::index_semantic_registry::{ConverterRegistryEntryV1, converter_registry_entry};
use super::text_fold::{TextFoldErrorClassV1, fold_characters, is_alphanumeric};

const EXACT_POSTING_DOMAIN: &[u8] = b"aeordb.typed-exact-posting.v1\0";
const EXACT_COORDINATE_DOMAIN: &[u8] = b"aeordb.index.exact-coordinate.v1\0";
const TOKEN_COORDINATE_DOMAIN: &[u8] = b"aeordb.index.token-coordinate.v1\0";
const WORD_TRIGRAM_CLASS: u8 = 0x01;
const SUBSTRING_TRIGRAM_CLASS: u8 = 0x02;
const SOUNDEX_CLASS: u8 = 0x03;
const DOUBLE_METAPHONE_PRIMARY_CLASS: u8 = 0x04;
const DOUBLE_METAPHONE_ALTERNATE_CLASS: u8 = 0x05;

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
      0x0009..=0x000c => {
        validate_corrected_token_key(self.definition.converter_id, left)?;
        validate_corrected_token_key(self.definition.converter_id, right)?;
        Ok(left.cmp(right))
      }
      0x8001..=0x800d => Ok(left.cmp(right)),
      _ => Err(error(
        IndexSemanticErrorClassV1::UnsupportedDefinition,
        "converter_runtime_missing",
        format!("converter 0x{:04x} has no runtime", self.definition.converter_id),
      )),
    }
  }

  fn compile_value(&self, value: &CanonicalConfigValueV1) -> IndexSemanticResultV1<CompiledSourceValueV1> {
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
    if !self.definition.corrected {
      let migrated = compile_migration_value_v0(&self.definition, value).map_err(|source| {
        let class = match source.class() {
          MigrationConverterErrorClassV0::InvalidSourceValue => IndexSemanticErrorClassV1::InvalidSourceValue,
          MigrationConverterErrorClassV0::ResourceLimit => IndexSemanticErrorClassV1::ResourceLimit,
          MigrationConverterErrorClassV0::UnsupportedDefinition => IndexSemanticErrorClassV1::UnsupportedDefinition,
        };
        error(class, source.code(), source.context())
      })?;
      let mut postings = Vec::new();
      postings.try_reserve_exact(migrated.len()).map_err(|source| {
        error(
          IndexSemanticErrorClassV1::ResourceLimit,
          "converter_posting_reserve",
          format!("cannot reserve bounded migration posting output: {source}"),
        )
      })?;
      for (expansion_ordinal, posting) in migrated.into_iter().enumerate() {
        let expansion_ordinal = u32::try_from(expansion_ordinal)
          .map_err(|source| limit_context("converter_expansion_ordinal", format!("posting ordinal exceeds u32: {source}")))?;
        postings.push(CompiledPostingKeyV1 { posting_key: posting.posting_key, coordinate: posting.coordinate, expansion_ordinal });
      }
      self.enforce_posting_limits(&postings)?;
      return Ok(CompiledSourceValueV1 { canonical_value, postings });
    }

    if matches!(self.definition.converter_id, 0x0009..=0x000c) {
      let text = require_string(value)?;
      let posting_keys = compile_corrected_tokens(self.definition.converter_id, text, self.definition.max_output_values as usize)?;
      let mut postings = Vec::new();
      postings.try_reserve_exact(posting_keys.len()).map_err(|source| {
        error(
          IndexSemanticErrorClassV1::ResourceLimit,
          "converter_posting_reserve",
          format!("cannot reserve bounded corrected token output: {source}"),
        )
      })?;
      for (expansion_ordinal, posting_key) in posting_keys.into_iter().enumerate() {
        let expansion_ordinal = u32::try_from(expansion_ordinal)
          .map_err(|source| limit_context("converter_expansion_ordinal", format!("posting ordinal exceeds u32: {source}")))?;
        let coordinate_digest = digest_blake3(TOKEN_COORDINATE_DOMAIN, &posting_key);
        let coordinate = u64::from_be_bytes(coordinate_digest[..8].try_into().map_err(|source| {
          error(
            IndexSemanticErrorClassV1::UnsupportedDefinition,
            "token_coordinate_digest",
            format!("BLAKE3 token digest lacks eight bytes: {source}"),
          )
        })?);
        postings.push(CompiledPostingKeyV1 { posting_key, coordinate, expansion_ordinal });
      }
      self.enforce_posting_limits(&postings)?;
      return Ok(CompiledSourceValueV1 { canonical_value, postings });
    }

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
      converter_id => {
        return Err(error(
          IndexSemanticErrorClassV1::UnsupportedDefinition,
          "converter_runtime_missing",
          format!("converter 0x{converter_id:04x} has no corrected runtime"),
        ));
      }
    };

    let postings = vec![CompiledPostingKeyV1 { posting_key, coordinate, expansion_ordinal: 0 }];
    self.enforce_posting_limits(&postings)?;
    Ok(CompiledSourceValueV1 { canonical_value, postings })
  }

  fn enforce_input_limit(&self, length: usize) -> IndexSemanticResultV1<()> {
    if length as u64 > self.definition.max_input_bytes {
      return Err(limit_error("converter_input_limit", length as u64, self.definition.max_input_bytes));
    }
    Ok(())
  }

  fn enforce_posting_limits(&self, postings: &[CompiledPostingKeyV1]) -> IndexSemanticResultV1<()> {
    if postings.len() > self.definition.max_output_values as usize {
      return Err(limit_error("converter_output_count_limit", postings.len() as u64, u64::from(self.definition.max_output_values)));
    }
    let mut total_output_bytes = 0u64;
    for posting in postings {
      let posting_length = posting.posting_key.len() as u64;
      if posting_length > u64::from(self.definition.max_output_value_bytes) {
        return Err(limit_error("converter_single_output_limit", posting_length, u64::from(self.definition.max_output_value_bytes)));
      }
      total_output_bytes = total_output_bytes
        .checked_add(posting_length)
        .ok_or_else(|| limit_context("converter_total_output_overflow", "posting byte count overflowed u64"))?;
      if total_output_bytes > self.definition.max_total_output_bytes {
        return Err(limit_error("converter_total_output_limit", total_output_bytes, self.definition.max_total_output_bytes));
      }
    }
    Ok(())
  }
}

fn compile_corrected_tokens(converter_id: u16, text: &str, maximum_values: usize) -> IndexSemanticResultV1<Vec<Vec<u8>>> {
  let folded = if converter_id == 0x0009 { fold_characters(text).map_err(text_fold_error)? } else { Vec::new() };
  let mut postings = Vec::new();
  let estimated_postings = folded.len().saturating_mul(3).saturating_add(1);
  postings.try_reserve(maximum_values.min(estimated_postings)).map_err(token_reserve_error)?;
  let mut seen = HashSet::new();
  seen.try_reserve(maximum_values).map_err(token_reserve_error)?;

  match converter_id {
    0x0009 => {
      let mut start = 0usize;
      while start < folded.len() {
        while start < folded.len() && !is_alphanumeric(folded[start]).map_err(text_fold_error)? {
          start += 1;
        }
        let mut end = start;
        while end < folded.len() && is_alphanumeric(folded[end]).map_err(text_fold_error)? {
          end += 1;
        }
        if start < end {
          let word = &folded[start..end];
          for window_start in 0..word.len().saturating_add(1) {
            let characters = [
              padded_word_character(word, window_start),
              padded_word_character(word, window_start + 1),
              padded_word_character(word, window_start + 2),
            ];
            push_corrected_token(&mut postings, &mut seen, token_key(WORD_TRIGRAM_CLASS, &characters)?, maximum_values)?;
          }
        }
        start = end.saturating_add(1);
      }
      for window in folded.windows(3) {
        push_corrected_token(&mut postings, &mut seen, token_key(SUBSTRING_TRIGRAM_CLASS, window)?, maximum_values)?;
      }
    }
    0x000a..=0x000c => {
      let class = match converter_id {
        0x000a => SOUNDEX_CLASS,
        0x000b => DOUBLE_METAPHONE_PRIMARY_CLASS,
        _ => DOUBLE_METAPHONE_ALTERNATE_CLASS,
      };
      let mut ascii_word = String::new();
      ascii_word.try_reserve(text.len()).map_err(token_reserve_error)?;
      let mut inside_word = false;
      for character in text.chars() {
        if is_alphanumeric(character).map_err(text_fold_error)? {
          inside_word = true;
          if character.is_ascii_alphabetic() {
            ascii_word.push(character);
          }
        } else if inside_word {
          push_corrected_phonetic(&mut postings, &mut seen, converter_id, class, &ascii_word, maximum_values)?;
          ascii_word.clear();
          inside_word = false;
        }
      }
      if inside_word {
        push_corrected_phonetic(&mut postings, &mut seen, converter_id, class, &ascii_word, maximum_values)?;
      }
    }
    _ => {
      return Err(error(
        IndexSemanticErrorClassV1::UnsupportedDefinition,
        "token_converter_unknown",
        format!("converter 0x{converter_id:04x} is not a corrected token converter"),
      ));
    }
  }
  Ok(postings)
}

fn padded_word_character(word: &[char], position: usize) -> char {
  if position < 2 {
    return ' ';
  }
  match word.get(position - 2) {
    Some(character) => *character,
    None => ' ',
  }
}

fn token_key(class: u8, characters: &[char]) -> IndexSemanticResultV1<Vec<u8>> {
  let encoded_length = characters.iter().map(|character| character.len_utf8()).sum::<usize>();
  let mut posting_key = Vec::new();
  posting_key.try_reserve_exact(encoded_length.saturating_add(1)).map_err(token_reserve_error)?;
  posting_key.push(class);
  let mut buffer = [0u8; 4];
  for character in characters {
    posting_key.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
  }
  Ok(posting_key)
}

fn nonempty_code(value: String) -> Option<String> {
  if value.is_empty() {
    None
  } else {
    Some(value)
  }
}

fn push_corrected_phonetic(
  postings: &mut Vec<Vec<u8>>,
  seen: &mut HashSet<Vec<u8>>,
  converter_id: u16,
  class: u8,
  ascii_word: &str,
  maximum_values: usize,
) -> IndexSemanticResultV1<()> {
  if ascii_word.is_empty() {
    return Ok(());
  }
  let code = match converter_id {
    0x000a => nonempty_code(crate::engine::phonetic::soundex(ascii_word)),
    0x000b => nonempty_code(crate::engine::phonetic::dmetaphone_primary(ascii_word)),
    _ => crate::engine::phonetic::dmetaphone_alt(ascii_word),
  };
  let Some(code) = code else {
    return Ok(());
  };
  let mut posting_key = Vec::new();
  posting_key.try_reserve_exact(code.len().saturating_add(1)).map_err(token_reserve_error)?;
  posting_key.push(class);
  posting_key.extend_from_slice(code.as_bytes());
  push_corrected_token(postings, seen, posting_key, maximum_values)
}

fn push_corrected_token(
  postings: &mut Vec<Vec<u8>>,
  seen: &mut HashSet<Vec<u8>>,
  posting_key: Vec<u8>,
  maximum_values: usize,
) -> IndexSemanticResultV1<()> {
  if seen.contains(&posting_key) {
    return Ok(());
  }
  if postings.len() >= maximum_values {
    return Err(limit_error("converter_output_count_limit", postings.len() as u64 + 1, maximum_values as u64));
  }
  if postings.len() == postings.capacity() {
    postings.try_reserve(1).map_err(token_reserve_error)?;
  }
  let mut seen_key = Vec::new();
  seen_key.try_reserve_exact(posting_key.len()).map_err(token_reserve_error)?;
  seen_key.extend_from_slice(&posting_key);
  seen.insert(seen_key);
  postings.push(posting_key);
  Ok(())
}

fn validate_corrected_token_key(converter_id: u16, value: &[u8]) -> IndexSemanticResultV1<()> {
  let Some((&class, payload)) = value.split_first() else {
    return Err(malformed_key("corrected token posting key is empty"));
  };
  match converter_id {
    0x0009 => {
      if !matches!(class, WORD_TRIGRAM_CLASS | SUBSTRING_TRIGRAM_CLASS) {
        return Err(malformed_key("trigram posting key has an unknown class"));
      }
      let text = std::str::from_utf8(payload).map_err(|source| malformed_key(format!("trigram posting key is invalid UTF-8: {source}")))?;
      if text.chars().count() != 3 {
        return Err(malformed_key("trigram posting key must contain exactly three Unicode scalars"));
      }
    }
    0x000a => {
      if class != SOUNDEX_CLASS || payload.len() != 4 || !payload[0].is_ascii_uppercase() || !payload[1..].iter().all(u8::is_ascii_digit) {
        return Err(malformed_key("Soundex posting key must be class 03, one uppercase ASCII letter, and three digits"));
      }
    }
    0x000b | 0x000c => {
      let expected_class = if converter_id == 0x000b { DOUBLE_METAPHONE_PRIMARY_CLASS } else { DOUBLE_METAPHONE_ALTERNATE_CLASS };
      if class != expected_class
        || payload.is_empty()
        || payload.len() > 4
        || !payload.iter().all(|byte| byte.is_ascii_uppercase() || *byte == b'0')
      {
        return Err(malformed_key("Double Metaphone posting key has the wrong class or noncanonical code"));
      }
    }
    _ => return Err(malformed_key("converter does not own a corrected token posting key")),
  }
  Ok(())
}

fn token_reserve_error(source: std::collections::TryReserveError) -> IndexSemanticErrorV1 {
  error(
    IndexSemanticErrorClassV1::ResourceLimit,
    "converter_token_reserve",
    format!("cannot reserve bounded corrected token workspace: {source}"),
  )
}

fn text_fold_error(source: super::text_fold::TextFoldErrorV1) -> IndexSemanticErrorV1 {
  let class = match source.class() {
    TextFoldErrorClassV1::MalformedTable => IndexSemanticErrorClassV1::UnsupportedDefinition,
    TextFoldErrorClassV1::ResourceLimit => IndexSemanticErrorClassV1::ResourceLimit,
  };
  error(class, source.code(), source.context())
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

fn limit_context(code: &'static str, context: impl Into<String>) -> IndexSemanticErrorV1 {
  error(IndexSemanticErrorClassV1::ResourceLimit, code, context)
}

fn error(class: IndexSemanticErrorClassV1, code: &'static str, context: impl Into<String>) -> IndexSemanticErrorV1 {
  IndexSemanticErrorV1 { class, code, context: context.into() }
}
