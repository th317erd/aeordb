use std::collections::BTreeMap;
use std::fmt;

use serde::de::{DeserializeSeed, Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;

use crate::core::HashProfile;

const FRAME_LENGTH: usize = 5;
const MAX_VALUE_LENGTH: usize = 256 * 1_024;
const MAX_SCALAR_LENGTH: usize = 64 * 1_024;
const MAX_CONTAINER_MEMBERS: usize = 65_535;
const MAX_CONTAINER_DEPTH: usize = 32;

#[derive(Clone, Copy)]
pub enum ConfigFormat {
  CanonicalConfigValueV1,
}

impl ConfigFormat {
  pub fn id(self) -> &'static str {
    "canonical-config-value-v1"
  }

  pub fn family(self) -> &'static str {
    "CanonicalConfigValueV1"
  }
}

#[derive(Clone)]
pub struct ConfigFixtureCase {
  pub id: &'static str,
  pub format: ConfigFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
enum CanonicalValue {
  Null,
  False,
  True,
  I64(i64),
  U64(u64),
  F64(f64),
  Utf8(String),
  Bytes(Vec<u8>),
  Array(Vec<CanonicalValue>),
  Map(BTreeMap<String, CanonicalValue>),
}

#[derive(Clone, Copy)]
struct CanonicalSeed {
  depth: usize,
}

struct CanonicalVisitor {
  depth: usize,
}

impl<'de> DeserializeSeed<'de> for CanonicalSeed {
  type Value = CanonicalValue;

  fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(CanonicalVisitor { depth: self.depth })
  }
}

impl<'de> Visitor<'de> for CanonicalVisitor {
  type Value = CanonicalValue;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a bounded canonical JSON configuration value")
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(CanonicalValue::Null)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(CanonicalValue::Null)
  }

  fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
    Ok(if value { CanonicalValue::True } else { CanonicalValue::False })
  }

  fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
    Ok(CanonicalValue::I64(value))
  }

  fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
    if value <= i64::MAX as u64 {
      Ok(CanonicalValue::I64(value as i64))
    } else {
      Ok(CanonicalValue::U64(value))
    }
  }

  fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
  where
    E: DeError,
  {
    if !value.is_finite() {
      return Err(E::custom("canonical config float must be finite"));
    }
    Ok(CanonicalValue::F64(if value == 0.0 { 0.0 } else { value }))
  }

  fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
  where
    E: DeError,
  {
    self.visit_string(value.to_string())
  }

  fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
  where
    E: DeError,
  {
    if value.len() > MAX_SCALAR_LENGTH {
      return Err(E::custom("canonical config string exceeds 64 KiB"));
    }
    Ok(CanonicalValue::Utf8(value))
  }

  fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    if self.depth >= MAX_CONTAINER_DEPTH {
      return Err(A::Error::custom("canonical config nesting exceeds 32 containers"));
    }
    let mut values = Vec::new();
    while let Some(value) = sequence.next_element_seed(CanonicalSeed { depth: self.depth + 1 })? {
      if values.len() >= MAX_CONTAINER_MEMBERS {
        return Err(A::Error::custom("canonical config array exceeds 65535 members"));
      }
      values.push(value);
    }
    Ok(CanonicalValue::Array(values))
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    if self.depth >= MAX_CONTAINER_DEPTH {
      return Err(A::Error::custom("canonical config nesting exceeds 32 containers"));
    }
    let mut values = BTreeMap::new();
    while let Some(key) = map.next_key::<String>()? {
      if key.len() > MAX_SCALAR_LENGTH {
        return Err(A::Error::custom("canonical config map key exceeds 64 KiB"));
      }
      if values.len() >= MAX_CONTAINER_MEMBERS {
        return Err(A::Error::custom("canonical config map exceeds 65535 members"));
      }
      if values.contains_key(&key) {
        return Err(A::Error::custom(format!("duplicate canonical config map key: {key}")));
      }
      let value = map.next_value_seed(CanonicalSeed { depth: self.depth + 1 })?;
      values.insert(key, value);
    }
    Ok(CanonicalValue::Map(values))
  }
}

struct DecodedConfig {
  tag_name: &'static str,
  detail_name: &'static str,
  detail: usize,
}

pub fn fixture_cases() -> Vec<ConfigFixtureCase> {
  let mut cases = Vec::with_capacity(6);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let all_tags = all_tags_value();
    cases.push(config_fixture(
      profile,
      match profile {
        HashProfile::Blake3_256 => "config-blake3-256-all-tags-valid",
        HashProfile::Sha512 => "config-sha512-all-tags-valid",
      },
      &all_tags,
      "config:map:members=7",
      Some("covers:all-permanent-tags"),
    ));

    let numeric_boundaries =
      parse_json("[-9223372036854775808,9223372036854775807,18446744073709551615,0.0,5e-324,1.7976931348623157e308]")
        .expect("numeric boundary fixture JSON must canonicalize");
    cases.push(config_fixture(
      profile,
      match profile {
        HashProfile::Blake3_256 => "config-blake3-256-numeric-boundaries-valid",
        HashProfile::Sha512 => "config-sha512-numeric-boundaries-valid",
      },
      &numeric_boundaries,
      "config:array:members=6",
      Some("covers:numeric-boundaries"),
    ));

    let maximum_string = CanonicalValue::Utf8("x".repeat(MAX_SCALAR_LENGTH));
    cases.push(config_fixture(
      profile,
      match profile {
        HashProfile::Blake3_256 => "config-blake3-256-maximum-string-valid",
        HashProfile::Sha512 => "config-sha512-maximum-string-valid",
      },
      &maximum_string,
      "config:utf8:bytes=65536",
      Some("boundary:65536-byte-scalar"),
    ));
  }
  cases
}

pub fn observe(_profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode(bytes) {
    Ok(decoded) => (format!("config:{}:{}={}", decoded.tag_name, decoded.detail_name, decoded.detail), None),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(bytes: &[u8]) -> Vec<String> {
  let payload_length = read_u32(bytes, 1).unwrap_or(0) as usize;
  let tag = bytes.first().copied().unwrap_or(0);
  vec![
    "value +0x000 len 1: permanent value_tag".to_string(),
    "value +0x001 len 4: payload_length".to_string(),
    format!("value tag: 0x{tag:02x}"),
    format!("value +0x005 len {payload_length}: canonical payload"),
  ]
}

fn config_fixture(
  profile: HashProfile,
  id: &'static str,
  value: &CanonicalValue,
  expected: &'static str,
  relation: Option<&'static str>,
) -> ConfigFixtureCase {
  ConfigFixtureCase {
    id,
    format: ConfigFormat::CanonicalConfigValueV1,
    profile,
    expected,
    relation,
    canonical_key: None,
    bytes: encode(value).expect("fixture value must encode canonically"),
  }
}

fn all_tags_value() -> CanonicalValue {
  CanonicalValue::Map(BTreeMap::from([
    ("array".to_string(), CanonicalValue::Array(vec![CanonicalValue::Null, CanonicalValue::False, CanonicalValue::True])),
    ("bytes".to_string(), CanonicalValue::Bytes(vec![0x00, 0xff])),
    ("f64".to_string(), CanonicalValue::F64(1.5)),
    ("i64".to_string(), CanonicalValue::I64(-42)),
    (
      "map".to_string(),
      CanonicalValue::Map(BTreeMap::from([
        ("a".to_string(), CanonicalValue::Utf8("alpha".to_string())),
        ("z".to_string(), CanonicalValue::Null),
      ])),
    ),
    ("u64".to_string(), CanonicalValue::U64(i64::MAX as u64 + 1)),
    ("utf8".to_string(), CanonicalValue::Utf8("AeorDB".to_string())),
  ]))
}

fn parse_json(input: &str) -> Result<CanonicalValue, String> {
  validate_json_numbers(input)?;
  let mut deserializer = serde_json::Deserializer::from_str(input);
  let value = CanonicalSeed { depth: 0 }.deserialize(&mut deserializer).map_err(|error| error.to_string())?;
  deserializer.end().map_err(|error| error.to_string())?;
  encode(&value).map_err(str::to_string)?;
  Ok(value)
}

fn validate_json_numbers(input: &str) -> Result<(), String> {
  let bytes = input.as_bytes();
  let mut index = 0;
  let mut in_string = false;
  let mut escaped = false;
  while index < bytes.len() {
    let byte = bytes[index];
    if in_string {
      if escaped {
        escaped = false;
      } else if byte == b'\\' {
        escaped = true;
      } else if byte == b'"' {
        in_string = false;
      }
      index += 1;
      continue;
    }
    if byte == b'"' {
      in_string = true;
      index += 1;
      continue;
    }
    if byte != b'-' && !byte.is_ascii_digit() {
      index += 1;
      continue;
    }

    let start = index;
    index += 1;
    while index < bytes.len() && matches!(bytes[index], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
      index += 1;
    }
    let token = &input[start..index];
    if token.contains(['.', 'e', 'E']) {
      let value = token.parse::<f64>().map_err(|_| format!("invalid JSON number: {token}"))?;
      if !value.is_finite() {
        return Err(format!("JSON number is not a finite f64: {token}"));
      }
    } else if token.starts_with('-') {
      token.parse::<i64>().map_err(|_| format!("JSON integer is outside i64: {token}"))?;
    } else {
      token.parse::<u64>().map_err(|_| format!("JSON integer is outside u64: {token}"))?;
    }
  }
  Ok(())
}

fn encode(value: &CanonicalValue) -> Result<Vec<u8>, &'static str> {
  encode_value(value, 0)
}

fn encode_value(value: &CanonicalValue, depth: usize) -> Result<Vec<u8>, &'static str> {
  match value {
    CanonicalValue::Null => frame(0x01, &[]),
    CanonicalValue::False => frame(0x02, &[]),
    CanonicalValue::True => frame(0x03, &[]),
    CanonicalValue::I64(value) => frame(0x04, &value.to_le_bytes()),
    CanonicalValue::U64(value) if *value > i64::MAX as u64 => frame(0x05, &value.to_le_bytes()),
    CanonicalValue::U64(_) => Err("canonical u64 must exceed i64::MAX"),
    CanonicalValue::F64(value) if value.is_finite() && value.to_bits() != (-0.0f64).to_bits() => {
      frame(0x06, &value.to_bits().to_le_bytes())
    }
    CanonicalValue::F64(_) => Err("canonical f64 must be finite positive-zero form"),
    CanonicalValue::Utf8(value) => {
      if value.len() > MAX_SCALAR_LENGTH {
        return Err("canonical utf8 exceeds 64 KiB");
      }
      frame(0x07, value.as_bytes())
    }
    CanonicalValue::Bytes(value) => {
      if value.len() > MAX_SCALAR_LENGTH {
        return Err("canonical bytes exceed 64 KiB");
      }
      frame(0x08, value)
    }
    CanonicalValue::Array(values) => {
      check_container(depth, values.len())?;
      let mut payload = Vec::with_capacity(4);
      payload.extend_from_slice(&(values.len() as u32).to_le_bytes());
      for value in values {
        append_bounded(&mut payload, &encode_value(value, depth + 1)?)?;
      }
      frame(0x09, &payload)
    }
    CanonicalValue::Map(values) => {
      check_container(depth, values.len())?;
      let mut payload = Vec::with_capacity(4);
      payload.extend_from_slice(&(values.len() as u32).to_le_bytes());
      for (key, value) in values {
        if key.len() > MAX_SCALAR_LENGTH {
          return Err("canonical map key exceeds 64 KiB");
        }
        append_bounded(&mut payload, &(key.len() as u32).to_le_bytes())?;
        append_bounded(&mut payload, key.as_bytes())?;
        append_bounded(&mut payload, &encode_value(value, depth + 1)?)?;
      }
      frame(0x0a, &payload)
    }
  }
}

fn check_container(depth: usize, members: usize) -> Result<(), &'static str> {
  if depth >= MAX_CONTAINER_DEPTH {
    return Err("canonical config nesting exceeds 32 containers");
  }
  if members > MAX_CONTAINER_MEMBERS {
    return Err("canonical config container exceeds 65535 members");
  }
  Ok(())
}

fn append_bounded(target: &mut Vec<u8>, bytes: &[u8]) -> Result<(), &'static str> {
  let next_length = target.len().checked_add(bytes.len()).ok_or("canonical config length overflow")?;
  if FRAME_LENGTH.checked_add(next_length).ok_or("canonical config length overflow")? > MAX_VALUE_LENGTH {
    return Err("canonical config exceeds 256 KiB");
  }
  target.extend_from_slice(bytes);
  Ok(())
}

fn frame(tag: u8, payload: &[u8]) -> Result<Vec<u8>, &'static str> {
  let total_length = FRAME_LENGTH.checked_add(payload.len()).ok_or("canonical config length overflow")?;
  if total_length > MAX_VALUE_LENGTH {
    return Err("canonical config exceeds 256 KiB");
  }
  let mut bytes = Vec::with_capacity(total_length);
  bytes.push(tag);
  bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  bytes.extend_from_slice(payload);
  Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<DecodedConfig, &'static str> {
  if bytes.len() > MAX_VALUE_LENGTH {
    return Err("config_value_oversize");
  }
  let (decoded, end) = validate_at(bytes, 0, bytes.len(), 0)?;
  if end != bytes.len() {
    return Err("config_value_trailing");
  }
  Ok(decoded)
}

fn validate_at(bytes: &[u8], start: usize, limit: usize, depth: usize) -> Result<(DecodedConfig, usize), &'static str> {
  let header_end = start.checked_add(FRAME_LENGTH).ok_or("config_value_overflow")?;
  if header_end > limit {
    return Err("config_value_truncated");
  }
  let payload_length = read_u32(bytes, start + 1)? as usize;
  let payload_end = header_end.checked_add(payload_length).ok_or("config_value_overflow")?;
  if payload_end > limit || payload_end > bytes.len() || payload_end - start > MAX_VALUE_LENGTH {
    return Err("config_value_length");
  }
  let payload = &bytes[header_end..payload_end];
  let (tag_name, detail_name, detail) = match bytes[start] {
    0x01 if payload.is_empty() => ("null", "bytes", 0),
    0x02 if payload.is_empty() => ("false", "bytes", 0),
    0x03 if payload.is_empty() => ("true", "bytes", 0),
    0x04 if payload.len() == 8 => ("i64", "bytes", 8),
    0x05 if payload.len() == 8 && read_u64(payload, 0)? > i64::MAX as u64 => ("u64", "bytes", 8),
    0x06 if payload.len() == 8 => {
      let value = f64::from_bits(read_u64(payload, 0)?);
      if !value.is_finite() || value.to_bits() == (-0.0f64).to_bits() {
        return Err("config_f64_noncanonical");
      }
      ("f64", "bytes", 8)
    }
    0x07 if payload.len() <= MAX_SCALAR_LENGTH => {
      std::str::from_utf8(payload).map_err(|_| "config_utf8")?;
      ("utf8", "bytes", payload.len())
    }
    0x08 if payload.len() <= MAX_SCALAR_LENGTH => ("bytes", "bytes", payload.len()),
    0x09 => {
      let count = validate_array(bytes, header_end, payload_end, depth)?;
      ("array", "members", count)
    }
    0x0a => {
      let count = validate_map(bytes, header_end, payload_end, depth)?;
      ("map", "members", count)
    }
    0x01..=0x08 => return Err("config_scalar_payload"),
    _ => return Err("config_value_tag"),
  };
  Ok((DecodedConfig { tag_name, detail_name, detail }, payload_end))
}

fn validate_array(bytes: &[u8], start: usize, end: usize, depth: usize) -> Result<usize, &'static str> {
  if depth >= MAX_CONTAINER_DEPTH || start.checked_add(4).ok_or("config_value_overflow")? > end {
    return Err("config_array_header");
  }
  let count = read_u32(bytes, start)? as usize;
  if count > MAX_CONTAINER_MEMBERS {
    return Err("config_container_members");
  }
  let mut cursor = start + 4;
  for _ in 0..count {
    let (_, next) = validate_at(bytes, cursor, end, depth + 1)?;
    cursor = next;
  }
  if cursor != end {
    return Err("config_array_count");
  }
  Ok(count)
}

fn validate_map(bytes: &[u8], start: usize, end: usize, depth: usize) -> Result<usize, &'static str> {
  if depth >= MAX_CONTAINER_DEPTH || start.checked_add(4).ok_or("config_value_overflow")? > end {
    return Err("config_map_header");
  }
  let count = read_u32(bytes, start)? as usize;
  if count > MAX_CONTAINER_MEMBERS {
    return Err("config_container_members");
  }
  let mut cursor = start + 4;
  let mut previous_key: Option<&[u8]> = None;
  for _ in 0..count {
    let key_header_end = cursor.checked_add(4).ok_or("config_value_overflow")?;
    if key_header_end > end {
      return Err("config_map_key_truncated");
    }
    let key_length = read_u32(bytes, cursor)? as usize;
    if key_length > MAX_SCALAR_LENGTH {
      return Err("config_map_key_oversize");
    }
    let key_end = key_header_end.checked_add(key_length).ok_or("config_value_overflow")?;
    if key_end > end {
      return Err("config_map_key_truncated");
    }
    let key = &bytes[key_header_end..key_end];
    std::str::from_utf8(key).map_err(|_| "config_map_key_utf8")?;
    if previous_key.is_some_and(|previous| previous >= key) {
      return Err("config_map_key_order");
    }
    previous_key = Some(key);
    let (_, next) = validate_at(bytes, key_end, end, depth + 1)?;
    cursor = next;
  }
  if cursor != end {
    return Err("config_map_count");
  }
  Ok(count)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let raw = bytes.get(offset..offset + 4).ok_or("truncated")?;
  Ok(u32::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  let raw = bytes.get(offset..offset + 8).ok_or("truncated")?;
  Ok(u64::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn config_fixtures_match_results() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn config_mutations_are_rejected_or_change_the_enclosing_identity_input() {
    for case in fixture_cases() {
      let original_digest = case.profile.digest(&case.bytes);
      let mutation_offsets: Vec<usize> = if case.bytes.len() <= 4_096 {
        (0..case.bytes.len()).collect()
      } else {
        let mut offsets = vec![0, 1, 4, 5, case.bytes.len() / 2, case.bytes.len() - 1];
        offsets.extend((0..case.bytes.len()).step_by(4_096));
        offsets.sort_unstable();
        offsets.dedup();
        offsets
      };
      for index in mutation_offsets {
        let mut mutated = case.bytes.clone();
        mutated[index] ^= 0x01;
        let observed = observe(case.profile, &mutated).0;
        assert!(
          observed.starts_with("error:") || case.profile.digest(&mutated) != original_digest,
          "fixture {} byte {index} was not protected",
          case.id
        );
      }
    }
  }

  #[test]
  fn json_parser_preserves_number_types_and_canonicalizes_maps() {
    assert_eq!(parse_json("1").unwrap(), CanonicalValue::I64(1));
    assert_eq!(parse_json("1.0").unwrap(), CanonicalValue::F64(1.0));
    assert_eq!(parse_json("9223372036854775808").unwrap(), CanonicalValue::U64(i64::MAX as u64 + 1));
    assert_eq!(parse_json("-0.0").unwrap(), CanonicalValue::F64(0.0));

    let left = parse_json(r#"{"z":null,"a":1}"#).unwrap();
    let right = parse_json(r#"{"a":1,"z":null}"#).unwrap();
    assert_eq!(encode(&left).unwrap(), encode(&right).unwrap());
  }

  #[test]
  fn json_parser_rejects_duplicates_overflow_depth_and_trailing_input() {
    assert!(parse_json(r#"{"a":1,"a":2}"#).unwrap_err().contains("duplicate"));
    assert!(parse_json("18446744073709551616").is_err());
    assert!(parse_json("1 true").is_err());

    let depth_32 = format!("{}null{}", "[".repeat(32), "]".repeat(32));
    let depth_33 = format!("{}null{}", "[".repeat(33), "]".repeat(33));
    assert!(parse_json(&depth_32).is_ok());
    assert!(parse_json(&depth_33).unwrap_err().contains("nesting"));
  }

  #[test]
  fn encoder_enforces_scalar_member_depth_and_total_bounds() {
    assert_eq!(encode(&CanonicalValue::Utf8("x".repeat(MAX_SCALAR_LENGTH))).unwrap().len(), MAX_SCALAR_LENGTH + FRAME_LENGTH);
    assert!(encode(&CanonicalValue::Utf8("x".repeat(MAX_SCALAR_LENGTH + 1))).is_err());
    assert!(encode(&CanonicalValue::Bytes(vec![0; MAX_SCALAR_LENGTH + 1])).is_err());
    assert!(encode(&CanonicalValue::Array(vec![CanonicalValue::Null; MAX_CONTAINER_MEMBERS + 1])).is_err());
    assert!(encode(&CanonicalValue::U64(1)).is_err());
    assert!(encode(&CanonicalValue::F64(f64::NAN)).is_err());
    assert!(encode(&CanonicalValue::F64(-0.0)).is_err());

    let mut nested = CanonicalValue::Null;
    for _ in 0..MAX_CONTAINER_DEPTH {
      nested = CanonicalValue::Array(vec![nested]);
    }
    assert!(encode(&nested).is_ok());
    nested = CanonicalValue::Array(vec![nested]);
    assert!(encode(&nested).is_err());
  }

  #[test]
  fn decoder_rejects_malformed_scalars_lengths_tags_and_trailing_bytes() {
    for malformed in [vec![], vec![0x01], vec![0x00, 0, 0, 0, 0], vec![0x01, 1, 0, 0, 0, 0]] {
      assert!(decode(&malformed).is_err());
    }

    let mut trailing = encode(&CanonicalValue::Null).unwrap();
    trailing.push(0);
    assert_eq!(decode(&trailing).err(), Some("config_value_trailing"));

    let small_u64 = frame(0x05, &1u64.to_le_bytes()).unwrap();
    assert_eq!(decode(&small_u64).err(), Some("config_scalar_payload"));
    let negative_zero = frame(0x06, &(-0.0f64).to_bits().to_le_bytes()).unwrap();
    assert_eq!(decode(&negative_zero).err(), Some("config_f64_noncanonical"));
    let infinity = frame(0x06, &f64::INFINITY.to_bits().to_le_bytes()).unwrap();
    assert_eq!(decode(&infinity).err(), Some("config_f64_noncanonical"));
    let invalid_utf8 = frame(0x07, &[0xff]).unwrap();
    assert_eq!(decode(&invalid_utf8).err(), Some("config_utf8"));
  }

  #[test]
  fn decoder_rejects_container_count_and_map_order_failures() {
    let mut array_payload = 2u32.to_le_bytes().to_vec();
    array_payload.extend_from_slice(&encode(&CanonicalValue::Null).unwrap());
    let wrong_array_count = frame(0x09, &array_payload).unwrap();
    assert!(decode(&wrong_array_count).is_err());

    let mut map_payload = 2u32.to_le_bytes().to_vec();
    for key in ["z", "a"] {
      map_payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
      map_payload.extend_from_slice(key.as_bytes());
      map_payload.extend_from_slice(&encode(&CanonicalValue::Null).unwrap());
    }
    let unordered_map = frame(0x0a, &map_payload).unwrap();
    assert_eq!(decode(&unordered_map).err(), Some("config_map_key_order"));

    let mut duplicate_payload = 2u32.to_le_bytes().to_vec();
    for _ in 0..2 {
      duplicate_payload.extend_from_slice(&1u32.to_le_bytes());
      duplicate_payload.push(b'a');
      duplicate_payload.extend_from_slice(&encode(&CanonicalValue::Null).unwrap());
    }
    let duplicate_map = frame(0x0a, &duplicate_payload).unwrap();
    assert_eq!(decode(&duplicate_map).err(), Some("config_map_key_order"));
  }
}
