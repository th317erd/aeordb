use crate::engine::HashAlgorithm;

use super::hash::digest_parts;
use super::index_semantic_registry::{KNOWN_SOURCE_TYPES, converter_registry_entry, strategy_registry_entry};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const DEFINITION_HEADER_LENGTH: usize = 32;
const CONVERTER_FIXED_LENGTH: usize = 120;
const MAX_CONVERTER_LENGTH: usize = 65_536;
const MAX_FIELD_INDEX_LENGTH: usize = 256 * 1_024;
const MAX_STRATEGY_NAME_LENGTH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConverterDefinitionV1<'a> {
  pub converter_fingerprint: Vec<u8>,
  pub converter_id: u16,
  pub name: &'static str,
  pub corrected: bool,
  pub source_type_mask: u32,
  pub comparison_semantics: u16,
  pub collation_semantics: u16,
  pub tokenizer_semantics: u16,
  pub expansion_semantics: u16,
  pub max_input_bytes: u64,
  pub max_output_values: u32,
  pub max_output_value_bytes: u32,
  pub max_total_output_bytes: u64,
  pub parameters: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldIndexDefinitionV1<'a> {
  pub index_id: Vec<u8>,
  pub value_store_id: &'a [u8],
  pub strategy_id: u16,
  pub strategy_name: &'a str,
  pub corrected: bool,
  pub operations: u64,
  pub max_terms_per_document: u32,
  pub max_postings_per_document: u32,
  pub max_canonical_posting_bytes_per_document: u64,
  pub max_query_recheck_value_bytes: u64,
  pub converter: ConverterDefinitionV1<'a>,
}

pub fn decode_converter_definition(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ConverterDefinitionV1<'_>> {
  if value.len() > MAX_CONVERTER_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "converter_exceeds_cap",
      format!("{} bytes exceeds {MAX_CONVERTER_LENGTH}", value.len()),
    ));
  }
  if value.len() < CONVERTER_FIXED_LENGTH {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "converter_truncated",
      format!("{} bytes is shorter than {CONVERTER_FIXED_LENGTH}", value.len()),
    ));
  }
  validate_definition_envelope(value, b"ACNV", "converter")?;

  let parameter_length = usize::try_from(u32_at(value, 56)?).map_err(|_| length_error("converter parameter length does not fit usize"))?;
  let expected_length =
    CONVERTER_FIXED_LENGTH.checked_add(parameter_length).ok_or_else(|| length_error("converter parameter length overflow"))?;
  if expected_length != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "converter_parameter_length",
      format!("parameters end at {expected_length}, definition ends at {}", value.len()),
    ));
  }
  if u32_at(value, 60)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "converter_flags", "converter flags are nonzero"));
  }

  let converter_id = u16_at(value, 32)?;
  let spec = converter_registry_entry(converter_id)
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "converter_id", format!("unknown converter 0x{converter_id:04x}")))?;
  let source_type_mask = u32_at(value, 36)?;
  if source_type_mask & !KNOWN_SOURCE_TYPES != 0 {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "converter_source_type",
      format!("unknown source type bits 0x{:x}", source_type_mask & !KNOWN_SOURCE_TYPES),
    ));
  }

  let expected_semantics = if spec.corrected { 1 } else { converter_id };
  let expected_collation = if matches!(converter_id, 0x0003 | 0x0009..=0x000c | 0x8008 | 0x800a..=0x800d) { converter_id } else { 0 };
  let expected_tokenizer = if spec.tokenizing { converter_id } else { 0 };
  if source_type_mask == 0
    || source_type_mask != spec.source_type_mask
    || u16_at(value, 34)? != expected_semantics
    || [40, 42, 44, 50, 52].iter().any(|offset| u16_at(value, *offset).ok() != Some(converter_id))
    || u16_at(value, 46)? != expected_collation
    || u16_at(value, 48)? != expected_tokenizer
    || u16_at(value, 54)? != 1
  {
    return Err(closure_error("converter semantic IDs or source type mask disagree with its registry row"));
  }

  let max_input_bytes = u64_at(value, 64)?;
  let max_output_values = u32_at(value, 72)?;
  let max_output_value_bytes = u32_at(value, 76)?;
  let max_total_output_bytes = u64_at(value, 80)?;
  if max_input_bytes == 0
    || max_output_values == 0
    || max_output_values > 65_536
    || max_output_value_bytes == 0
    || max_total_output_bytes == 0
  {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "converter_limits",
      "converter limits are zero or exceed the frozen cap",
    ));
  }

  if value[88..120] != spec.behavior_fingerprint {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "converter_bundle_fingerprint",
      "converter behavior fingerprint differs from the frozen bundle",
    ));
  }
  let parameters = &value[CONVERTER_FIXED_LENGTH..];
  validate_parameters(converter_id, parameters)?;

  Ok(ConverterDefinitionV1 {
    converter_fingerprint: digest_parts(hash_algorithm, &[b"aeordb.index.converter-definition.v1\0", value]),
    converter_id,
    name: spec.name,
    corrected: spec.corrected,
    source_type_mask,
    comparison_semantics: converter_id,
    collation_semantics: expected_collation,
    tokenizer_semantics: expected_tokenizer,
    expansion_semantics: converter_id,
    max_input_bytes,
    max_output_values,
    max_output_value_bytes,
    max_total_output_bytes,
    parameters,
  })
}

pub fn decode_field_index_definition(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<FieldIndexDefinitionV1<'_>> {
  if value.len() > MAX_FIELD_INDEX_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "field_index_exceeds_cap",
      format!("{} bytes exceeds {MAX_FIELD_INDEX_LENGTH}", value.len()),
    ));
  }
  let hash_width = hash_algorithm.hash_length();
  let minimum_length = (136usize)
    .checked_add(hash_width)
    .and_then(|length| length.checked_add(1 + CONVERTER_FIXED_LENGTH))
    .ok_or_else(|| length_error("field-index minimum length overflow"))?;
  if value.len() < minimum_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "field_index_truncated",
      format!("{} bytes is shorter than {minimum_length}", value.len()),
    ));
  }
  validate_definition_envelope(value, b"AFIX", "field index")?;

  let value_store_end = DEFINITION_HEADER_LENGTH.checked_add(hash_width).ok_or_else(|| length_error("ValueStoreId end overflow"))?;
  let value_store_id = &value[DEFINITION_HEADER_LENGTH..value_store_end];
  if value_store_id.iter().all(|byte| *byte == 0) {
    return Err(error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "field_index_value_store_id", "ValueStoreId is all zero"));
  }

  let fixed = value_store_end;
  let converter_length =
    usize::try_from(u32_at(value, fixed + 36)?).map_err(|_| length_error("field-index converter length does not fit usize"))?;
  let strategy_name_length = usize::from(u16_at(value, fixed + 40)?);
  if converter_length > MAX_CONVERTER_LENGTH || strategy_name_length > MAX_STRATEGY_NAME_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "field_index_child_exceeds_cap",
      "converter or strategy name exceeds its frozen cap",
    ));
  }
  if converter_length < CONVERTER_FIXED_LENGTH || strategy_name_length == 0 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "field_index_child_length",
      "converter or strategy name is shorter than its minimum",
    ));
  }

  let strategy_name_start = 136usize.checked_add(hash_width).ok_or_else(|| length_error("strategy name start overflow"))?;
  let strategy_name_end =
    strategy_name_start.checked_add(strategy_name_length).ok_or_else(|| length_error("strategy name end overflow"))?;
  let converter_end = strategy_name_end.checked_add(converter_length).ok_or_else(|| length_error("nested converter end overflow"))?;
  if converter_end != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "field_index_length_formula",
      format!("children end at {converter_end}, definition ends at {}", value.len()),
    ));
  }
  if value[fixed + 42..fixed + 44].iter().any(|byte| *byte != 0) || value[fixed + 52..fixed + 56].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "field_index_reserved", "field-index reserve is nonzero"));
  }

  let converter = decode_converter_definition(&value[strategy_name_end..converter_end], hash_algorithm)
    .map_err(|source| closure_error(format!("nested converter rejected: {} ({})", source.code(), source.context())))?;
  let converter_registry = converter_registry_entry(converter.converter_id).ok_or_else(|| {
    error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "field_index_strategy",
      format!("no registry row for converter 0x{:04x}", converter.converter_id),
    )
  })?;
  let strategy = strategy_registry_entry(converter_registry.strategy_id, converter.corrected).ok_or_else(|| {
    error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "field_index_strategy",
      format!("no strategy for converter 0x{:04x}", converter.converter_id),
    )
  })?;
  let strategy_name = std::str::from_utf8(&value[strategy_name_start..strategy_name_end]).map_err(|source| {
    error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "field_index_strategy_utf8", format!("invalid UTF-8: {source}"))
  })?;

  let strategy_id = u16_at(value, fixed)?;
  let expected_semantics = if strategy.corrected { 1 } else { 0x8000 | strategy_id };
  let expected_tokenizer = if converter.tokenizer_semantics != 0 { converter.converter_id } else { 0 };
  if strategy_id != strategy.id
    || strategy_name != strategy.name
    || u16_at(value, fixed + 2)? != expected_semantics
    || u16_at(value, fixed + 4)? != 1
    || u16_at(value, fixed + 6)? != 1
    || u16_at(value, fixed + 8)? != converter.comparison_semantics
    || u64_at(value, fixed + 10)? != strategy.operations
    || u16_at(value, fixed + 18)? != expected_tokenizer
    || u16_at(value, fixed + 20)? != converter.collation_semantics
    || u16_at(value, fixed + 22)? != converter.tokenizer_semantics
    || u16_at(value, fixed + 24)? != converter.expansion_semantics
    || u16_at(value, fixed + 26)? != 1
    || u16_at(value, fixed + 28)? != 1
    || u16_at(value, fixed + 30)? != if strategy.corrected { strategy_id } else { 0x8000 | strategy_id }
    || u16_at(value, fixed + 32)? != if strategy_id == 3 { expected_semantics } else { 0 }
    || u16_at(value, fixed + 34)? != 1
  {
    return Err(closure_error("field-index strategy, operations, or converter semantics disagree"));
  }

  let max_terms_per_document = u32_at(value, fixed + 44)?;
  let max_postings_per_document = u32_at(value, fixed + 48)?;
  let max_canonical_posting_bytes_per_document = u64_at(value, fixed + 56)?;
  let max_query_recheck_value_bytes = u64_at(value, fixed + 64)?;
  if max_terms_per_document == 0
    || max_terms_per_document > 65_536
    || max_postings_per_document == 0
    || max_postings_per_document > 65_536
    || max_canonical_posting_bytes_per_document == 0
    || max_canonical_posting_bytes_per_document > 8 * 1_048_576
    || max_query_recheck_value_bytes == 0
    || max_query_recheck_value_bytes > 8 * 1_048_576
  {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "field_index_limits",
      "field-index resource limits are zero or exceed frozen caps",
    ));
  }
  if value[fixed + 72..fixed + 104] != strategy.behavior_fingerprint {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "field_index_strategy_fingerprint",
      "strategy behavior fingerprint differs from the frozen bundle",
    ));
  }

  Ok(FieldIndexDefinitionV1 {
    index_id: digest_parts(hash_algorithm, &[b"aeordb.index.field-definition.v1\0", value]),
    value_store_id,
    strategy_id,
    strategy_name,
    corrected: strategy.corrected,
    operations: strategy.operations,
    max_terms_per_document,
    max_postings_per_document,
    max_canonical_posting_bytes_per_document,
    max_query_recheck_value_bytes,
    converter,
  })
}

fn validate_parameters(converter_id: u16, parameters: &[u8]) -> FormatResult<()> {
  let expected_length = match converter_id {
    0x8002 => 2,
    0x8003 => 4,
    0x8004 => 8,
    0x8005..=0x8007 | 0x8009 => 16,
    0x8008 => 4,
    _ => 0,
  };
  if parameters.len() != expected_length {
    return Err(closure_error(format!(
      "converter 0x{converter_id:04x} requires {expected_length} parameter bytes, got {}",
      parameters.len()
    )));
  }
  if converter_id == 0x8008 && u32_at(parameters, 0)? == 0 {
    return Err(closure_error("legacy string converter requires a nonzero maximum length"));
  }
  Ok(())
}

fn validate_definition_envelope(value: &[u8], magic: &[u8; 4], label: &'static str) -> FormatResult<()> {
  if &value[..4] != magic || u16_at(value, 4)? != 1 || usize::from(u16_at(value, 6)?) != DEFINITION_HEADER_LENGTH {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "definition_envelope",
      format!("expected {} v1 with a 32-byte header", String::from_utf8_lossy(magic)),
    ));
  }
  if usize::try_from(u32_at(value, 8)?).map_err(|_| length_error("definition total length does not fit usize"))? != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "definition_total_length",
      format!("{label} declared length differs from input"),
    ));
  }
  if u32_at(value, 12)? != 0 || value[16..32].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "definition_reserved", format!("{label} reserve is nonzero")));
  }
  Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let value = bytes.get(offset..offset + 2).ok_or_else(|| truncated_error(offset, 2))?;
  Ok(u16::from_le_bytes(value.try_into().expect("exact slice length")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let value = bytes.get(offset..offset + 4).ok_or_else(|| truncated_error(offset, 4))?;
  Ok(u32::from_le_bytes(value.try_into().expect("exact slice length")))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let value = bytes.get(offset..offset + 8).ok_or_else(|| truncated_error(offset, 8))?;
  Ok(u64::from_le_bytes(value.try_into().expect("exact slice length")))
}

fn truncated_error(offset: usize, width: usize) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "definition_truncated", format!("need {width} bytes at offset {offset}"))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "definition_length_overflow", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "definition_semantic_closure", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
