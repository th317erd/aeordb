use crate::core::HashProfile;
use crate::semantics;

const DEFINITION_HEADER_LENGTH: usize = 32;
const CONVERTER_FIXED_LENGTH: usize = 120;
const MAX_CONVERTER_LENGTH: usize = 65_536;
const MAX_FIELD_INDEX_LENGTH: usize = 256 * 1_024;
const MAX_STRATEGY_NAME_LENGTH: usize = 256;

const TYPE_NULL: u32 = 1 << 0;
const TYPE_BOOL: u32 = 1 << 1;
const TYPE_I64: u32 = 1 << 2;
const TYPE_U64: u32 = 1 << 3;
const TYPE_F64: u32 = 1 << 4;
const TYPE_UTF8: u32 = 1 << 5;
const TYPE_BYTES: u32 = 1 << 6;
const TYPE_ARRAY: u32 = 1 << 7;
const TYPE_MAP: u32 = 1 << 8;
const KNOWN_TYPES: u32 = TYPE_NULL | TYPE_BOOL | TYPE_I64 | TYPE_U64 | TYPE_F64 | TYPE_UTF8 | TYPE_BYTES | TYPE_ARRAY | TYPE_MAP;
const SCALAR_TYPES: u32 = TYPE_NULL | TYPE_BOOL | TYPE_I64 | TYPE_U64 | TYPE_F64 | TYPE_UTF8 | TYPE_BYTES;

const OPS_EXACT: u64 = (1 << 0) | (1 << 1);
const OPS_ORDERED: u64 = OPS_EXACT | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 10) | (1 << 11);
const OPS_TRIGRAM: u64 = (1 << 5) | (1 << 6) | (1 << 8) | (1 << 9);
const OPS_PHONETIC: u64 = (1 << 7) | (1 << 9);

#[derive(Clone, Copy)]
pub enum FieldIndexFormat {
  ConverterDefinitionV1,
  FieldIndexDefinitionV1,
}

impl FieldIndexFormat {
  pub fn id(self) -> &'static str {
    match self {
      Self::ConverterDefinitionV1 => "converter-definition-v1",
      Self::FieldIndexDefinitionV1 => "field-index-definition-v1",
    }
  }

  pub fn family(self) -> &'static str {
    match self {
      Self::ConverterDefinitionV1 => "ConverterDefinitionV1",
      Self::FieldIndexDefinitionV1 => "FieldIndexDefinitionV1",
    }
  }
}

#[derive(Clone)]
pub struct FieldIndexFixtureCase {
  pub id: &'static str,
  pub format: FieldIndexFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct ConverterRow {
  id: u16,
  name: &'static str,
  corrected: bool,
  source_mask: u32,
  tokenizing: bool,
}

#[derive(Clone, Copy, Debug)]
struct StrategyRow {
  id: u16,
  name: &'static str,
  corrected: bool,
  operations: u64,
  converter_id: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DecodedConverter {
  pub id: u16,
  pub name: &'static str,
  pub corrected: bool,
  pub comparison_semantics: u16,
  pub collation_semantics: u16,
  pub tokenizer_semantics: u16,
  pub expansion_semantics: u16,
}

#[derive(Debug, PartialEq, Eq)]
struct DecodedFieldIndex {
  strategy_name: String,
  converter_name: &'static str,
  operations: u64,
}

pub fn fixture_cases() -> Vec<FieldIndexFixtureCase> {
  let mut cases = Vec::with_capacity(100);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for converter in converter_rows() {
      let converter_bytes = build_converter(converter).expect("registered converter must encode");
      let converter_expected = leak(format!("converter:{}:semantics={}", converter.name, converter_semantics(converter)));
      cases.push(FieldIndexFixtureCase {
        id: fixture_id("acnv", profile, converter.name),
        format: FieldIndexFormat::ConverterDefinitionV1,
        profile,
        expected: converter_expected,
        relation: Some(if converter.corrected { "semantic-family:corrected" } else { "semantic-family:migration-v0" }),
        canonical_key: Some(hex::encode(converter_fingerprint(profile, &converter_bytes))),
        bytes: converter_bytes.clone(),
      });

      let strategy = strategy_for_converter(converter);
      let field_bytes = build_field_index(profile, strategy, &converter_bytes).expect("registered field index must encode");
      let field_expected =
        leak(format!("field-index:{}:converter={}:operations=0x{:x}", strategy.name, converter.name, strategy.operations));
      cases.push(FieldIndexFixtureCase {
        id: fixture_id("afix", profile, converter.name),
        format: FieldIndexFormat::FieldIndexDefinitionV1,
        profile,
        expected: field_expected,
        relation: Some(if converter.corrected { "strategy-family:corrected" } else { "strategy-family:migration-v0" }),
        canonical_key: Some(hex::encode(index_id(profile, &field_bytes))),
        bytes: field_bytes,
      });
    }
  }
  cases
}

pub fn observe(format: FieldIndexFormat, profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match format {
    FieldIndexFormat::ConverterDefinitionV1 => match decode_converter(bytes) {
      Ok(converter) => (
        format!("converter:{}:semantics={}", converter.name, if converter.corrected { 1 } else { converter.id }),
        Some(hex::encode(converter_fingerprint(profile, bytes))),
      ),
      Err(error) => (format!("error:{error}"), None),
    },
    FieldIndexFormat::FieldIndexDefinitionV1 => match decode_field_index(profile, bytes) {
      Ok(index) => (
        format!("field-index:{}:converter={}:operations=0x{:x}", index.strategy_name, index.converter_name, index.operations),
        Some(hex::encode(index_id(profile, bytes))),
      ),
      Err(error) => (format!("error:{error}"), None),
    },
  }
}

pub fn annotation_lines(format: FieldIndexFormat, profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  match format {
    FieldIndexFormat::ConverterDefinitionV1 => vec![
      "definition +0x000 len 32: ACNV canonical-definition envelope".to_string(),
      "definition +0x020 len 24: converter IDs, source mask, and semantic IDs".to_string(),
      "definition +0x038 len 32: parameter length, bounds, and flags".to_string(),
      "definition +0x058 len 32: behavior-bundle fingerprint".to_string(),
      format!("definition +0x078 len {}: canonical parameters", bytes.len().saturating_sub(CONVERTER_FIXED_LENGTH)),
    ],
    FieldIndexFormat::FieldIndexDefinitionV1 => {
      let h = profile.width();
      let name_length = read_u16(bytes, 72 + h).unwrap_or(0) as usize;
      let converter_length = read_u32(bytes, 68 + h).unwrap_or(0) as usize;
      vec![
        "definition +0x000 len 32: AFIX canonical-definition envelope".to_string(),
        format!("definition +0x020 len {h}: exact ValueStoreId"),
        format!("definition +0x{:03x} len 104: strategy semantics, limits, and fingerprint", 32 + h),
        format!("definition +0x{:03x} len {name_length}: exact strategy name", 136 + h),
        format!("definition +0x{:03x} len {converter_length}: complete ACNV definition", 136 + h + name_length),
      ]
    }
  }
}

fn converter_rows() -> Vec<ConverterRow> {
  (0x0001..=0x000c).chain(0x8001..=0x800d).map(converter_row).collect::<Option<Vec<_>>>().expect("complete registry")
}

fn converter_row(id: u16) -> Option<ConverterRow> {
  let descriptor = semantics::converter_descriptor(id)?;
  let (source_mask, tokenizing) = match id {
    0x0001 => (SCALAR_TYPES, false),
    0x0002 => (TYPE_BYTES, false),
    0x0003 => (TYPE_UTF8, false),
    0x0004 | 0x0005 => (TYPE_I64 | TYPE_U64, false),
    0x0006 => (TYPE_I64 | TYPE_U64 | TYPE_F64, false),
    0x0007 => (TYPE_I64 | TYPE_U64 | TYPE_UTF8, false),
    0x0008 => (TYPE_BOOL, false),
    0x0009..=0x000c => (TYPE_UTF8, true),
    0x8001 => (TYPE_BYTES, false),
    0x8002..=0x8009 => (TYPE_BYTES, false),
    0x800a..=0x800d => (TYPE_BYTES, true),
    _ => return None,
  };
  Some(ConverterRow { id, name: descriptor.name, corrected: descriptor.corrected, source_mask, tokenizing })
}

fn strategy_for_converter(converter: ConverterRow) -> StrategyRow {
  let id = match converter.id {
    0x0001 | 0x8001 => 1,
    0x0002..=0x0008 | 0x8002..=0x8009 => 2,
    0x0009 | 0x800a => 3,
    0x000a | 0x800b => 4,
    0x000b | 0x800c => 5,
    0x000c | 0x800d => 6,
    _ => unreachable!(),
  };
  let descriptor = semantics::strategy_descriptor(id, converter.corrected).expect("registered strategy");
  let operations = match id {
    1 => OPS_EXACT,
    2 => OPS_ORDERED,
    3 => OPS_TRIGRAM,
    4..=6 => OPS_PHONETIC,
    _ => unreachable!(),
  };
  StrategyRow { id, name: descriptor.name, corrected: converter.corrected, operations, converter_id: converter.id }
}

fn build_converter(row: ConverterRow) -> Result<Vec<u8>, &'static str> {
  let parameters = default_parameters(row);
  let total_length = CONVERTER_FIXED_LENGTH.checked_add(parameters.len()).ok_or("converter_length_overflow")?;
  let mut value = vec![0u8; total_length];
  value[0..4].copy_from_slice(b"ACNV");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, DEFINITION_HEADER_LENGTH as u16);
  put_u32(&mut value, 8, total_length as u32);
  put_u16(&mut value, 32, row.id);
  put_u16(&mut value, 34, converter_semantics(row));
  put_u32(&mut value, 36, row.source_mask);
  for offset in [40, 42, 44, 50, 52] {
    put_u16(&mut value, offset, row.id);
  }
  put_u16(&mut value, 46, if matches!(row.id, 0x0003 | 0x0009..=0x000c | 0x8008 | 0x800a..=0x800d) { row.id } else { 0 });
  put_u16(&mut value, 48, if row.tokenizing { row.id } else { 0 });
  put_u16(&mut value, 54, 1);
  put_u32(&mut value, 56, parameters.len() as u32);
  put_u64(&mut value, 64, 1_048_576);
  put_u32(&mut value, 72, if row.tokenizing { 65_536 } else { 1 });
  put_u32(&mut value, 76, 1_048_576);
  put_u64(&mut value, 80, if row.tokenizing { 4 * 1_048_576 } else { 1_048_576 });
  let descriptor = semantics::converter_descriptor(row.id).ok_or("converter_id")?;
  value[88..120].copy_from_slice(&semantics::fingerprint(descriptor));
  value[120..].copy_from_slice(&parameters);
  decode_converter(&value)?;
  Ok(value)
}

pub(crate) fn decode_converter(value: &[u8]) -> Result<DecodedConverter, &'static str> {
  if !(CONVERTER_FIXED_LENGTH..=MAX_CONVERTER_LENGTH).contains(&value.len()) {
    return Err("converter_length");
  }
  validate_definition_envelope(value, b"ACNV")?;
  let parameter_length = read_u32(value, 56)? as usize;
  if CONVERTER_FIXED_LENGTH.checked_add(parameter_length).ok_or("converter_length_overflow")? != value.len() {
    return Err("converter_parameter_length");
  }
  if read_u32(value, 60)? != 0 {
    return Err("converter_flags");
  }
  let id = read_u16(value, 32)?;
  let row = converter_row(id).ok_or("converter_id")?;
  let source_mask = read_u32(value, 36)?;
  if source_mask == 0
    || source_mask & !KNOWN_TYPES != 0
    || read_u16(value, 34)? != converter_semantics(row)
    || source_mask != row.source_mask
    || [40, 42, 44, 50, 52].iter().any(|offset| read_u16(value, *offset).ok() != Some(id))
    || read_u16(value, 46)? != if matches!(id, 0x0003 | 0x0009..=0x000c | 0x8008 | 0x800a..=0x800d) { id } else { 0 }
    || read_u16(value, 48)? != if row.tokenizing { id } else { 0 }
    || read_u16(value, 54)? != 1
  {
    return Err("converter_semantics");
  }
  if read_u64(value, 64)? == 0
    || read_u32(value, 72)? == 0
    || read_u32(value, 76)? == 0
    || read_u64(value, 80)? == 0
    || read_u32(value, 72)? > 65_536
  {
    return Err("converter_limit");
  }
  if parameter_length != expected_parameter_length(row) {
    return Err("converter_parameters");
  }
  let descriptor = semantics::converter_descriptor(id).ok_or("converter_id")?;
  if value[88..120] != semantics::fingerprint(descriptor) {
    return Err("converter_fingerprint");
  }
  validate_parameters(row, &value[120..])?;
  Ok(DecodedConverter {
    id,
    name: row.name,
    corrected: row.corrected,
    comparison_semantics: id,
    collation_semantics: read_u16(value, 46)?,
    tokenizer_semantics: read_u16(value, 48)?,
    expansion_semantics: read_u16(value, 50)?,
  })
}

fn expected_parameter_length(row: ConverterRow) -> usize {
  match row.id {
    0x8002 => 2,
    0x8003 => 4,
    0x8004 => 8,
    0x8005..=0x8007 | 0x8009 => 16,
    0x8008 => 4,
    _ => 0,
  }
}

fn default_parameters(row: ConverterRow) -> Vec<u8> {
  match row.id {
    0x8002 => vec![u8::MIN, u8::MAX],
    0x8003 => [u16::MIN.to_le_bytes(), u16::MAX.to_le_bytes()].concat(),
    0x8004 => [u32::MIN.to_le_bytes(), u32::MAX.to_le_bytes()].concat(),
    0x8005 => [u64::MIN.to_le_bytes(), u64::MAX.to_le_bytes()].concat(),
    0x8006 => [i64::MIN.to_le_bytes(), i64::MAX.to_le_bytes()].concat(),
    0x8007 => [0.0f64.to_bits().to_le_bytes(), 1.0f64.to_bits().to_le_bytes()].concat(),
    0x8008 => 1_024u32.to_le_bytes().to_vec(),
    0x8009 => [0i64.to_le_bytes(), 4_102_444_800_000i64.to_le_bytes()].concat(),
    _ => Vec::new(),
  }
}

fn validate_parameters(row: ConverterRow, parameters: &[u8]) -> Result<(), &'static str> {
  if parameters.len() != expected_parameter_length(row) {
    return Err("converter_parameters");
  }
  if row.id == 0x8008 && read_u32(parameters, 0)? == 0 {
    return Err("converter_parameters");
  }
  Ok(())
}

fn build_field_index(profile: HashProfile, strategy: StrategyRow, converter: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  let name = strategy.name.as_bytes();
  let total = (136 + h).checked_add(name.len()).and_then(|length| length.checked_add(converter.len())).ok_or("field_index_length")?;
  if total > MAX_FIELD_INDEX_LENGTH {
    return Err("field_index_length");
  }
  let mut value = vec![0u8; total];
  value[0..4].copy_from_slice(b"AFIX");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, DEFINITION_HEADER_LENGTH as u16);
  put_u32(&mut value, 8, total as u32);
  value[32..32 + h].copy_from_slice(&profile.digest(format!("value-store-for:{}", strategy.converter_id).as_bytes()));
  let fixed = 32 + h;
  put_u16(&mut value, fixed, strategy.id);
  put_u16(&mut value, fixed + 2, strategy_semantics(strategy));
  put_u16(&mut value, fixed + 4, 1);
  put_u16(&mut value, fixed + 6, 1);
  let decoded = decode_converter(converter)?;
  put_u16(&mut value, fixed + 8, decoded.comparison_semantics);
  put_u64(&mut value, fixed + 10, strategy.operations);
  put_u16(&mut value, fixed + 18, if decoded.tokenizer_semantics != 0 { decoded.id } else { 0 });
  put_u16(&mut value, fixed + 20, decoded.collation_semantics);
  put_u16(&mut value, fixed + 22, decoded.tokenizer_semantics);
  put_u16(&mut value, fixed + 24, decoded.expansion_semantics);
  put_u16(&mut value, fixed + 26, 1);
  put_u16(&mut value, fixed + 28, 1);
  put_u16(&mut value, fixed + 30, if strategy.corrected { strategy.id } else { 0x8000 | strategy.id });
  put_u16(&mut value, fixed + 32, if strategy.id == 3 { strategy_semantics(strategy) } else { 0 });
  put_u16(&mut value, fixed + 34, 1);
  put_u32(&mut value, fixed + 36, converter.len() as u32);
  put_u16(&mut value, fixed + 40, name.len() as u16);
  put_u32(&mut value, fixed + 44, 65_536);
  put_u32(&mut value, fixed + 48, 65_536);
  put_u64(&mut value, fixed + 56, 8 * 1_048_576);
  put_u64(&mut value, fixed + 64, 8 * 1_048_576);
  let descriptor = semantics::strategy_descriptor(strategy.id, strategy.corrected).ok_or("strategy_id")?;
  value[fixed + 72..fixed + 104].copy_from_slice(&semantics::fingerprint(descriptor));
  value[136 + h..136 + h + name.len()].copy_from_slice(name);
  value[136 + h + name.len()..].copy_from_slice(converter);
  decode_field_index(profile, &value)?;
  Ok(value)
}

fn decode_field_index(profile: HashProfile, value: &[u8]) -> Result<DecodedFieldIndex, &'static str> {
  let h = profile.width();
  if value.len() < 136 + h + 1 + CONVERTER_FIXED_LENGTH || value.len() > MAX_FIELD_INDEX_LENGTH {
    return Err("field_index_length");
  }
  validate_definition_envelope(value, b"AFIX")?;
  if value[32..32 + h].iter().all(|byte| *byte == 0) {
    return Err("field_index_value_store_id");
  }
  let fixed = 32 + h;
  let converter_length = read_u32(value, fixed + 36)? as usize;
  let name_length = read_u16(value, fixed + 40)? as usize;
  if !(CONVERTER_FIXED_LENGTH..=MAX_CONVERTER_LENGTH).contains(&converter_length) || !(1..=MAX_STRATEGY_NAME_LENGTH).contains(&name_length)
  {
    return Err("field_index_child_length");
  }
  let name_start = 136 + h;
  let name_end = name_start.checked_add(name_length).ok_or("field_index_length_overflow")?;
  let converter_end = name_end.checked_add(converter_length).ok_or("field_index_length_overflow")?;
  if converter_end != value.len() {
    return Err("field_index_length_formula");
  }
  if value[fixed + 42..fixed + 44].iter().any(|byte| *byte != 0) || value[fixed + 52..fixed + 56].iter().any(|byte| *byte != 0) {
    return Err("field_index_reserved");
  }
  let converter = decode_converter(&value[name_end..converter_end]).map_err(|_| "field_index_converter")?;
  let strategy_id = read_u16(value, fixed)?;
  let expected = strategy_for_converter(converter_row(converter.id).ok_or("field_index_converter_id")?);
  let name = std::str::from_utf8(&value[name_start..name_end]).map_err(|_| "field_index_strategy_utf8")?;
  if strategy_id != expected.id
    || name != expected.name
    || read_u16(value, fixed + 2)? != strategy_semantics(expected)
    || read_u16(value, fixed + 4)? != 1
    || read_u16(value, fixed + 6)? != 1
    || read_u16(value, fixed + 8)? != converter.comparison_semantics
    || read_u64(value, fixed + 10)? != expected.operations
    || read_u16(value, fixed + 18)? != if converter.tokenizer_semantics != 0 { converter.id } else { 0 }
    || read_u16(value, fixed + 20)? != converter.collation_semantics
    || read_u16(value, fixed + 22)? != converter.tokenizer_semantics
    || read_u16(value, fixed + 24)? != converter.expansion_semantics
    || read_u16(value, fixed + 26)? != 1
    || read_u16(value, fixed + 28)? != 1
    || read_u16(value, fixed + 30)? != if expected.corrected { expected.id } else { 0x8000 | expected.id }
    || read_u16(value, fixed + 32)? != if expected.id == 3 { strategy_semantics(expected) } else { 0 }
    || read_u16(value, fixed + 34)? != 1
  {
    return Err("field_index_strategy_semantics");
  }
  if [read_u32(value, fixed + 44)?, read_u32(value, fixed + 48)?].contains(&0)
    || [read_u32(value, fixed + 44)?, read_u32(value, fixed + 48)?].iter().any(|limit| *limit > 65_536)
    || read_u64(value, fixed + 56)? == 0
    || read_u64(value, fixed + 64)? == 0
    || read_u64(value, fixed + 56)? > 8 * 1_048_576
    || read_u64(value, fixed + 64)? > 8 * 1_048_576
  {
    return Err("field_index_limit");
  }
  let descriptor = semantics::strategy_descriptor(expected.id, expected.corrected).ok_or("strategy_id")?;
  if value[fixed + 72..fixed + 104] != semantics::fingerprint(descriptor) {
    return Err("field_index_strategy_fingerprint");
  }
  Ok(DecodedFieldIndex { strategy_name: name.to_string(), converter_name: converter.name, operations: expected.operations })
}

fn converter_semantics(row: ConverterRow) -> u16 {
  if row.corrected {
    1
  } else {
    row.id
  }
}

fn strategy_semantics(row: StrategyRow) -> u16 {
  if row.corrected {
    1
  } else {
    0x8000 | row.id
  }
}

fn validate_definition_envelope(value: &[u8], magic: &[u8; 4]) -> Result<(), &'static str> {
  if value.len() < DEFINITION_HEADER_LENGTH
    || &value[0..4] != magic
    || read_u16(value, 4)? != 1
    || read_u16(value, 6)? as usize != DEFINITION_HEADER_LENGTH
    || read_u32(value, 8)? as usize != value.len()
  {
    return Err("definition_envelope");
  }
  if read_u32(value, 12)? != 0 || value[16..32].iter().any(|byte| *byte != 0) {
    return Err("definition_reserved");
  }
  Ok(())
}

fn converter_fingerprint(profile: HashProfile, bytes: &[u8]) -> Vec<u8> {
  let mut input = Vec::with_capacity(38 + bytes.len());
  input.extend_from_slice(b"aeordb.index.converter-definition.v1\0");
  input.extend_from_slice(bytes);
  profile.digest(&input)
}

fn index_id(profile: HashProfile, bytes: &[u8]) -> Vec<u8> {
  let mut input = Vec::with_capacity(40 + bytes.len());
  input.extend_from_slice(b"aeordb.index.field-definition.v1\0");
  input.extend_from_slice(bytes);
  profile.digest(&input)
}

fn fixture_id(prefix: &str, profile: HashProfile, name: &str) -> &'static str {
  leak(format!("{prefix}-{}-{name}-valid", profile.label()))
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  let slice = bytes.get(offset..offset + 2).ok_or("truncated")?;
  Ok(u16::from_le_bytes(slice.try_into().unwrap()))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let slice = bytes.get(offset..offset + 4).ok_or("truncated")?;
  Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  let slice = bytes.get(offset..offset + 8).ok_or("truncated")?;
  Ok(u64::from_le_bytes(slice.try_into().unwrap()))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_converter_and_strategy_round_trips() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      for converter in converter_rows() {
        let bytes = build_converter(converter).unwrap();
        let decoded = decode_converter(&bytes).unwrap();
        assert_eq!(decoded.id, converter.id);
        let strategy = strategy_for_converter(converter);
        let field = build_field_index(profile, strategy, &bytes).unwrap();
        assert_eq!(decode_field_index(profile, &field).unwrap().converter_name, converter.name);
      }
    }
  }

  #[test]
  fn malformed_converter_fields_fail_closed() {
    let baseline = build_converter(converter_row(1).unwrap()).unwrap();
    for offset in [0, 4, 6, 8, 12, 16, 32, 34, 36, 40, 42, 44, 46, 48, 50, 52, 54, 56, 60, 88] {
      let mut changed = baseline.clone();
      changed[offset] ^= 1;
      assert!(decode_converter(&changed).is_err(), "offset {offset} accepted");
    }
    assert!(decode_converter(&baseline[..baseline.len() - 1]).is_err());
    let mut trailing = baseline.clone();
    trailing.push(0);
    assert!(decode_converter(&trailing).is_err());
  }

  #[test]
  fn migration_converter_parameters_are_exact_and_preserve_legacy_ranges() {
    for id in 0x8001..=0x800d {
      let row = converter_row(id).unwrap();
      let bytes = build_converter(row).unwrap();
      assert_eq!(bytes.len(), CONVERTER_FIXED_LENGTH + expected_parameter_length(row));
      assert_eq!(decode_converter(&bytes).unwrap().id, id);
    }

    let row = converter_row(0x8005).unwrap();
    let mut reversed = build_converter(row).unwrap();
    reversed[120..128].copy_from_slice(&100u64.to_le_bytes());
    reversed[128..136].copy_from_slice(&10u64.to_le_bytes());
    assert_eq!(decode_converter(&reversed).unwrap().id, row.id);

    let mut zero_string_bound = build_converter(converter_row(0x8008).unwrap()).unwrap();
    zero_string_bound[120..124].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(decode_converter(&zero_string_bound), Err("converter_parameters"));

    let mut corrected_with_parameter = build_converter(converter_row(1).unwrap()).unwrap();
    corrected_with_parameter.push(0);
    put_u32(&mut corrected_with_parameter, 8, 121);
    put_u32(&mut corrected_with_parameter, 56, 1);
    assert_eq!(decode_converter(&corrected_with_parameter), Err("converter_parameters"));
  }

  #[test]
  fn field_index_rejects_wrong_converter_strategy_or_fingerprint() {
    let profile = HashProfile::Blake3_256;
    let converter = build_converter(converter_row(1).unwrap()).unwrap();
    let baseline = build_field_index(profile, strategy_for_converter(converter_row(1).unwrap()), &converter).unwrap();
    for offset in [0, 4, 6, 8, 12, 16, 64, 66, 68, 70, 72, 74, 76, 80, 84, 104, 136] {
      let mut changed = baseline.clone();
      changed[offset] ^= 1;
      assert!(decode_field_index(profile, &changed).is_err(), "offset {offset} accepted");
    }

    let fixed = 32 + profile.width();
    let mut zero_limit = baseline.clone();
    put_u32(&mut zero_limit, fixed + 44, 0);
    assert_eq!(decode_field_index(profile, &zero_limit), Err("field_index_limit"));
    let mut excess_limit = baseline.clone();
    put_u32(&mut excess_limit, fixed + 48, 65_537);
    assert_eq!(decode_field_index(profile, &excess_limit), Err("field_index_limit"));
    let mut unknown_operation = baseline.clone();
    put_u64(&mut unknown_operation, fixed + 10, OPS_EXACT | (1 << 63));
    assert_eq!(decode_field_index(profile, &unknown_operation), Err("field_index_strategy_semantics"));
  }

  #[test]
  fn fixture_mutations_reject_or_change_identity() {
    for case in fixture_cases() {
      for offset in 0..case.bytes.len() {
        let mut changed = case.bytes.clone();
        changed[offset] ^= 1;
        let (observed, key) = observe(case.format, case.profile, &changed);
        assert!(observed.starts_with("error:") || key != case.canonical_key, "{} byte {offset} was semantically invisible", case.id);
      }
    }
  }

  #[test]
  fn index_id_uses_the_approved_field_definition_domain() {
    let profile = HashProfile::Blake3_256;
    let converter = build_converter(converter_row(1).unwrap()).unwrap();
    let bytes = build_field_index(profile, strategy_for_converter(converter_row(1).unwrap()), &converter).unwrap();
    let mut approved = b"aeordb.index.field-definition.v1\0".to_vec();
    approved.extend_from_slice(&bytes);
    assert_eq!(index_id(profile, &bytes), profile.digest(&approved));
  }
}
