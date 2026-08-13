use std::error::Error;
use std::fmt;
use std::sync::OnceLock;

const TABLE: &[u8] = include_bytes!("../../../spec/semantics/v1/aeor-text-fold-unicode-17.bin");
const MAGIC: &[u8; 4] = b"ATF1";
const DOMAIN: &[u8] = b"aeordb.text-fold-table.v1\0";
const HEADER_LENGTH: usize = 20;
const MAPPING_LENGTH: usize = 20;
const RANGE_LENGTH: usize = 8;
const CHECKSUM_LENGTH: usize = 32;
const MAXIMUM_TABLE_LENGTH: usize = 128 * 1_024;
const EXPECTED_CHECKSUM: [u8; 32] = [
  0x9f, 0x1b, 0xdd, 0x82, 0xa6, 0x14, 0x2d, 0xdc, 0x38, 0x24, 0xe1, 0x25, 0xc2, 0x8a, 0xb9, 0x41, 0xde, 0x2a, 0xc9, 0xb9, 0x8f, 0xd7, 0xea,
  0xff, 0xaa, 0x5b, 0x85, 0xa3, 0xf6, 0xf8, 0x84, 0xd2,
];

pub const AEOR_TEXT_FOLD_UNICODE_VERSION_V1: (u16, u16, u16) = (17, 0, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFoldErrorClassV1 {
  MalformedTable,
  ResourceLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFoldErrorV1 {
  class: TextFoldErrorClassV1,
  code: &'static str,
  context: String,
}

impl TextFoldErrorV1 {
  pub fn class(&self) -> TextFoldErrorClassV1 {
    self.class
  }

  pub fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for TextFoldErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for TextFoldErrorV1 {}

#[derive(Clone, Copy, Debug)]
struct TableLayout {
  mapping_count: usize,
  ranges_offset: usize,
  range_count: usize,
}

static TABLE_LAYOUT: OnceLock<Result<TableLayout, TextFoldErrorV1>> = OnceLock::new();

pub fn fold_characters(text: &str) -> Result<Vec<char>, TextFoldErrorV1> {
  let layout = table_layout()?;
  let mut folded = Vec::new();
  folded.try_reserve(text.len()).map_err(reserve_error)?;
  for character in text.chars() {
    append_lowercase(layout, character, &mut folded)?;
  }
  Ok(folded)
}

pub fn is_alphanumeric(character: char) -> Result<bool, TextFoldErrorV1> {
  let layout = table_layout()?;
  let codepoint = u32::from(character);
  let mut low = 0usize;
  let mut high = layout.range_count;
  while low < high {
    let middle = low + (high - low) / 2;
    let offset = layout.ranges_offset + middle * RANGE_LENGTH;
    let start = u32_at(TABLE, offset)?;
    let end = u32_at(TABLE, offset + 4)?;
    if codepoint < start {
      high = middle;
    } else if codepoint > end {
      low = middle + 1;
    } else {
      return Ok(true);
    }
  }
  Ok(false)
}

fn append_lowercase(layout: TableLayout, character: char, output: &mut Vec<char>) -> Result<(), TextFoldErrorV1> {
  let codepoint = u32::from(character);
  let mut low = 0usize;
  let mut high = layout.mapping_count;
  while low < high {
    let middle = low + (high - low) / 2;
    let offset = HEADER_LENGTH + middle * MAPPING_LENGTH;
    let source = u32_at(TABLE, offset)?;
    if codepoint < source {
      high = middle;
    } else if codepoint > source {
      low = middle + 1;
    } else {
      let output_count = usize::from(TABLE[offset + 4]);
      reserve_additional(output, output_count)?;
      for index in 0..output_count {
        let mapped = u32_at(TABLE, offset + 8 + index * 4)?;
        let mapped =
          char::from_u32(mapped).ok_or_else(|| malformed("text_fold_mapping_scalar", "mapping output is not a Unicode scalar"))?;
        output.push(mapped);
      }
      return Ok(());
    }
  }
  reserve_additional(output, 1)?;
  output.push(character);
  Ok(())
}

fn reserve_additional(output: &mut Vec<char>, additional: usize) -> Result<(), TextFoldErrorV1> {
  if output.capacity().saturating_sub(output.len()) < additional {
    output.try_reserve(additional).map_err(reserve_error)?;
  }
  Ok(())
}

fn table_layout() -> Result<TableLayout, TextFoldErrorV1> {
  match TABLE_LAYOUT.get_or_init(|| validate_table(TABLE)) {
    Ok(layout) => Ok(*layout),
    Err(source) => Err(source.clone()),
  }
}

fn validate_table(table: &[u8]) -> Result<TableLayout, TextFoldErrorV1> {
  if table.len() < HEADER_LENGTH + CHECKSUM_LENGTH || table.len() > MAXIMUM_TABLE_LENGTH || &table[..4] != MAGIC {
    return Err(malformed("text_fold_table_envelope", "table length or magic differs from AeorTextFoldV1"));
  }
  let version = (u16_at(table, 4)?, u16_at(table, 6)?, u16_at(table, 8)?);
  if version != AEOR_TEXT_FOLD_UNICODE_VERSION_V1 || table[10..12] != [0; 2] {
    return Err(malformed("text_fold_table_version", "Unicode version or reserved bytes differ from AeorTextFoldV1"));
  }
  let mapping_count = usize::try_from(u32_at(table, 12)?).map_err(|source| malformed("text_fold_mapping_count", source.to_string()))?;
  let range_count = usize::try_from(u32_at(table, 16)?).map_err(|source| malformed("text_fold_range_count", source.to_string()))?;
  let mappings_length =
    mapping_count.checked_mul(MAPPING_LENGTH).ok_or_else(|| malformed("text_fold_mapping_length", "mapping byte length overflow"))?;
  let ranges_length =
    range_count.checked_mul(RANGE_LENGTH).ok_or_else(|| malformed("text_fold_range_length", "range byte length overflow"))?;
  let ranges_offset =
    HEADER_LENGTH.checked_add(mappings_length).ok_or_else(|| malformed("text_fold_mapping_end", "mapping end overflow"))?;
  let checksum_offset = ranges_offset.checked_add(ranges_length).ok_or_else(|| malformed("text_fold_range_end", "range end overflow"))?;
  if checksum_offset.checked_add(CHECKSUM_LENGTH) != Some(table.len()) {
    return Err(malformed("text_fold_table_length", "table length formula disagrees with embedded bytes"));
  }

  let mut hasher = blake3::Hasher::new();
  hasher.update(DOMAIN);
  hasher.update(&table[..checksum_offset]);
  let checksum = hasher.finalize();
  if checksum.as_bytes() != &table[checksum_offset..] || checksum.as_bytes() != &EXPECTED_CHECKSUM {
    return Err(malformed("text_fold_table_checksum", "embedded table checksum differs from the frozen semantic contract"));
  }

  let mut prior_source = None;
  for index in 0..mapping_count {
    let offset = HEADER_LENGTH + index * MAPPING_LENGTH;
    let source = u32_at(table, offset)?;
    let output_count = usize::from(table[offset + 4]);
    if !(1..=3).contains(&output_count) || table[offset + 5..offset + 8] != [0; 3] {
      return Err(malformed("text_fold_mapping_header", "mapping output count or reserved bytes are invalid"));
    }
    if char::from_u32(source).is_none() || prior_source.is_some_and(|prior| source <= prior) {
      return Err(malformed("text_fold_mapping_order", "mapping sources are invalid or not strictly ordered"));
    }
    prior_source = Some(source);
    for output_index in 0..3 {
      let mapped = u32_at(table, offset + 8 + output_index * 4)?;
      if output_index < output_count {
        if char::from_u32(mapped).is_none() {
          return Err(malformed("text_fold_mapping_scalar", "mapping output is not a Unicode scalar"));
        }
      } else if mapped != 0 {
        return Err(malformed("text_fold_mapping_padding", "unused mapping output is nonzero"));
      }
    }
  }

  let mut prior_end: Option<u32> = None;
  for index in 0..range_count {
    let offset = ranges_offset + index * RANGE_LENGTH;
    let start = u32_at(table, offset)?;
    let end = u32_at(table, offset + 4)?;
    if start > end || char::from_u32(start).is_none() || char::from_u32(end).is_none() {
      return Err(malformed("text_fold_range", "alphanumeric range is invalid"));
    }
    if prior_end.is_some_and(|prior| start <= prior.saturating_add(1)) {
      return Err(malformed("text_fold_range_order", "alphanumeric ranges overlap or are not maximally coalesced"));
    }
    prior_end = Some(end);
  }

  Ok(TableLayout { mapping_count, ranges_offset, range_count })
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, TextFoldErrorV1> {
  let value = bytes.get(offset..offset + 2).ok_or_else(|| malformed("text_fold_table_truncated", format!("need two bytes at {offset}")))?;
  let fixed: [u8; 2] =
    value.try_into().map_err(|source| malformed("text_fold_table_width", format!("cannot decode two bytes at {offset}: {source}")))?;
  Ok(u16::from_le_bytes(fixed))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, TextFoldErrorV1> {
  let value =
    bytes.get(offset..offset + 4).ok_or_else(|| malformed("text_fold_table_truncated", format!("need four bytes at {offset}")))?;
  let fixed: [u8; 4] =
    value.try_into().map_err(|source| malformed("text_fold_table_width", format!("cannot decode four bytes at {offset}: {source}")))?;
  Ok(u32::from_le_bytes(fixed))
}

fn reserve_error(source: std::collections::TryReserveError) -> TextFoldErrorV1 {
  TextFoldErrorV1 { class: TextFoldErrorClassV1::ResourceLimit, code: "text_fold_reserve", context: source.to_string() }
}

fn malformed(code: &'static str, context: impl Into<String>) -> TextFoldErrorV1 {
  TextFoldErrorV1 { class: TextFoldErrorClassV1::MalformedTable, code, context: context.into() }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn frozen_table_validates_and_covers_lowercase_expansion_and_properties() {
    let layout = validate_table(TABLE).unwrap();
    assert_eq!(layout.mapping_count, 1_488);
    assert_eq!(layout.range_count, 844);
    assert_eq!(AEOR_TEXT_FOLD_UNICODE_VERSION_V1, (17, 0, 0));
    assert_eq!(fold_characters("İKÉ").unwrap(), ['i', '\u{307}', 'k', 'é']);
    assert!(is_alphanumeric('K').unwrap());
    assert!(is_alphanumeric('界').unwrap());
    assert!(!is_alphanumeric('\u{307}').unwrap());
    assert!(!is_alphanumeric('.').unwrap());
  }

  #[test]
  fn frozen_table_rejects_corrupt_envelope_version_and_checksum() {
    let mut wrong_magic = TABLE.to_vec();
    wrong_magic[0] ^= 0xff;
    assert_eq!(validate_table(&wrong_magic).unwrap_err().code(), "text_fold_table_envelope");

    let mut wrong_version = TABLE.to_vec();
    wrong_version[4..6].copy_from_slice(&16u16.to_le_bytes());
    assert_eq!(validate_table(&wrong_version).unwrap_err().code(), "text_fold_table_version");

    let mut wrong_checksum = TABLE.to_vec();
    let final_index = wrong_checksum.len() - 1;
    wrong_checksum[final_index] ^= 0xff;
    assert_eq!(validate_table(&wrong_checksum).unwrap_err().code(), "text_fold_table_checksum");
  }
}
