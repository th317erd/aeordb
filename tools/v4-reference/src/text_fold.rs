use std::fs;
use std::path::Path;

const MAGIC: &[u8; 4] = b"ATF1";
const DOMAIN: &[u8] = b"aeordb.text-fold-table.v1\0";
const HEADER_LENGTH: usize = 20;
const MAPPING_LENGTH: usize = 20;
const RANGE_LENGTH: usize = 8;
const CHECKSUM_LENGTH: usize = 32;
const UNICODE_VERSION: (u16, u16, u16) = (17, 0, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableMetadata {
  pub mapping_count: u32,
  pub alphanumeric_range_count: u32,
  pub byte_length: usize,
  pub blake3: [u8; 32],
}

pub fn generate(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
  if std::char::UNICODE_VERSION != (UNICODE_VERSION.0 as u8, UNICODE_VERSION.1 as u8, UNICODE_VERSION.2 as u8) {
    return Err(
      format!(
        "AeorTextFoldV1 generation requires Unicode {}.{}.{}, rustc provides {:?}",
        UNICODE_VERSION.0,
        UNICODE_VERSION.1,
        UNICODE_VERSION.2,
        std::char::UNICODE_VERSION
      )
      .into(),
    );
  }
  let bytes = generate_from_standard_library()?;
  validate(&bytes)?;
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }
  fs::write(path, bytes)?;
  Ok(())
}

pub fn verify(path: &Path) -> Result<TableMetadata, Box<dyn std::error::Error>> {
  validate(&fs::read(path)?)
}

pub fn validate(bytes: &[u8]) -> Result<TableMetadata, Box<dyn std::error::Error>> {
  if bytes.len() < HEADER_LENGTH + CHECKSUM_LENGTH || &bytes[..4] != MAGIC {
    return Err("AeorTextFoldV1 table is truncated or has the wrong magic".into());
  }
  let major = u16_at(bytes, 4)?;
  let minor = u16_at(bytes, 6)?;
  let patch = u16_at(bytes, 8)?;
  if (major, minor, patch) != UNICODE_VERSION || bytes[10..12] != [0; 2] {
    return Err("AeorTextFoldV1 Unicode version or reserved bytes differ from the frozen contract".into());
  }
  let mapping_count = u32_at(bytes, 12)?;
  let range_count = u32_at(bytes, 16)?;
  let mappings_length = usize::try_from(mapping_count)?.checked_mul(MAPPING_LENGTH).ok_or("mapping length overflow")?;
  let ranges_length = usize::try_from(range_count)?.checked_mul(RANGE_LENGTH).ok_or("range length overflow")?;
  let checksum_offset = HEADER_LENGTH
    .checked_add(mappings_length)
    .and_then(|offset| offset.checked_add(ranges_length))
    .ok_or("AeorTextFoldV1 table length overflow")?;
  if checksum_offset.checked_add(CHECKSUM_LENGTH) != Some(bytes.len()) {
    return Err("AeorTextFoldV1 table length formula disagrees with the input".into());
  }

  let mut hasher = blake3::Hasher::new();
  hasher.update(DOMAIN);
  hasher.update(&bytes[..checksum_offset]);
  let checksum = hasher.finalize();
  if checksum.as_bytes() != &bytes[checksum_offset..] {
    return Err("AeorTextFoldV1 table checksum mismatch".into());
  }

  let mut prior_source: Option<u32> = None;
  for index in 0..mapping_count as usize {
    let offset = HEADER_LENGTH + index * MAPPING_LENGTH;
    let source = u32_at(bytes, offset)?;
    let output_count = usize::from(bytes[offset + 4]);
    if !(1..=3).contains(&output_count) || bytes[offset + 5..offset + 8] != [0; 3] {
      return Err("AeorTextFoldV1 mapping count or reserved bytes are invalid".into());
    }
    if char::from_u32(source).is_none() || prior_source.is_some_and(|prior| source <= prior) {
      return Err("AeorTextFoldV1 mapping sources are invalid or not strictly ordered".into());
    }
    prior_source = Some(source);
    let mut outputs = [0u32; 3];
    for (output_index, output) in outputs.iter_mut().enumerate() {
      *output = u32_at(bytes, offset + 8 + output_index * 4)?;
      if output_index < output_count {
        if char::from_u32(*output).is_none() {
          return Err("AeorTextFoldV1 mapping output is not a Unicode scalar".into());
        }
      } else if *output != 0 {
        return Err("AeorTextFoldV1 unused mapping output is nonzero".into());
      }
    }
    if output_count == 1 && outputs[0] == source {
      return Err("AeorTextFoldV1 stores a redundant identity mapping".into());
    }
  }

  let ranges_offset = HEADER_LENGTH + mappings_length;
  let mut prior_end: Option<u32> = None;
  for index in 0..range_count as usize {
    let offset = ranges_offset + index * RANGE_LENGTH;
    let start = u32_at(bytes, offset)?;
    let end = u32_at(bytes, offset + 4)?;
    if start > end || char::from_u32(start).is_none() || char::from_u32(end).is_none() {
      return Err("AeorTextFoldV1 alphanumeric range is invalid".into());
    }
    if prior_end.is_some_and(|prior| start <= prior.saturating_add(1)) {
      return Err("AeorTextFoldV1 alphanumeric ranges overlap or are not maximally coalesced".into());
    }
    prior_end = Some(end);
  }

  Ok(TableMetadata { mapping_count, alphanumeric_range_count: range_count, byte_length: bytes.len(), blake3: *checksum.as_bytes() })
}

fn generate_from_standard_library() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
  let mut mappings = Vec::<(u32, Vec<u32>)>::new();
  let mut alphanumeric_ranges = Vec::<(u32, u32)>::new();
  let mut active_range = None;

  for codepoint in 0..=0x10ffff {
    let Some(character) = char::from_u32(codepoint) else {
      close_range(&mut active_range, &mut alphanumeric_ranges);
      continue;
    };
    let lowercase = character.to_lowercase().map(u32::from).collect::<Vec<_>>();
    if lowercase != [codepoint] {
      if lowercase.is_empty() || lowercase.len() > 3 {
        return Err(format!("Unicode lowercase mapping for U+{codepoint:04X} has unsupported length {}", lowercase.len()).into());
      }
      mappings.push((codepoint, lowercase));
    }

    if character.is_alphanumeric() {
      match active_range {
        Some((start, prior)) if codepoint == prior + 1 => active_range = Some((start, codepoint)),
        Some(_) => {
          close_range(&mut active_range, &mut alphanumeric_ranges);
          active_range = Some((codepoint, codepoint));
        }
        None => active_range = Some((codepoint, codepoint)),
      }
    } else {
      close_range(&mut active_range, &mut alphanumeric_ranges);
    }
  }
  close_range(&mut active_range, &mut alphanumeric_ranges);

  let mut bytes = Vec::new();
  bytes.extend_from_slice(MAGIC);
  bytes.extend_from_slice(&UNICODE_VERSION.0.to_le_bytes());
  bytes.extend_from_slice(&UNICODE_VERSION.1.to_le_bytes());
  bytes.extend_from_slice(&UNICODE_VERSION.2.to_le_bytes());
  bytes.extend_from_slice(&[0; 2]);
  bytes.extend_from_slice(&u32::try_from(mappings.len())?.to_le_bytes());
  bytes.extend_from_slice(&u32::try_from(alphanumeric_ranges.len())?.to_le_bytes());
  for (source, outputs) in mappings {
    bytes.extend_from_slice(&source.to_le_bytes());
    bytes.push(u8::try_from(outputs.len())?);
    bytes.extend_from_slice(&[0; 3]);
    for index in 0..3 {
      let output = match outputs.get(index) {
        Some(output) => *output,
        None => 0,
      };
      bytes.extend_from_slice(&output.to_le_bytes());
    }
  }
  for (start, end) in alphanumeric_ranges {
    bytes.extend_from_slice(&start.to_le_bytes());
    bytes.extend_from_slice(&end.to_le_bytes());
  }
  let mut hasher = blake3::Hasher::new();
  hasher.update(DOMAIN);
  hasher.update(&bytes);
  bytes.extend_from_slice(hasher.finalize().as_bytes());
  Ok(bytes)
}

fn close_range(active: &mut Option<(u32, u32)>, ranges: &mut Vec<(u32, u32)>) {
  if let Some(range) = active.take() {
    ranges.push(range);
  }
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, Box<dyn std::error::Error>> {
  let value = bytes.get(offset..offset + 2).ok_or("AeorTextFoldV1 u16 is truncated")?;
  Ok(u16::from_le_bytes(value.try_into()?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Box<dyn std::error::Error>> {
  let value = bytes.get(offset..offset + 4).ok_or("AeorTextFoldV1 u32 is truncated")?;
  Ok(u32::from_le_bytes(value.try_into()?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generated_unicode_17_table_is_complete_and_self_validating() {
    if std::char::UNICODE_VERSION != (17, 0, 0) {
      return;
    }
    let bytes = generate_from_standard_library().unwrap();
    let metadata = validate(&bytes).unwrap();
    assert!(metadata.mapping_count > 1_000);
    assert!(metadata.alphanumeric_range_count > 700);
    assert!(metadata.byte_length < 128 * 1_024);
  }
}
