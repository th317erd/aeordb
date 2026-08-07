use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use super::reader::{FormatError, FormatResult, MalformedInputClass};

const FRAME_LENGTH: usize = 5;
const MAX_CONTAINER_MEMBERS: usize = 65_535;
const MAX_CONTAINER_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalValueBounds {
  pub maximum_value_length: usize,
  pub maximum_scalar_length: usize,
  pub maximum_key_length: usize,
  allow_small_u64: bool,
}

impl CanonicalValueBounds {
  pub const CONFIG: Self =
    Self { maximum_value_length: 256 * 1_024, maximum_scalar_length: 64 * 1_024, maximum_key_length: 64 * 1_024, allow_small_u64: false };

  pub const SOURCE_VALUE: Self = Self {
    maximum_value_length: 1_048_576,
    maximum_scalar_length: 1_048_576 - FRAME_LENGTH,
    maximum_key_length: 64 * 1_024,
    allow_small_u64: true,
  };

  pub const AUDIT_VALUE: Self =
    Self { maximum_value_length: 1_048_576, maximum_scalar_length: 64 * 1_024, maximum_key_length: 64 * 1_024, allow_small_u64: false };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalValueSummary {
  pub tag_name: &'static str,
  pub detail_name: &'static str,
  pub detail: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalConfigValueV1 {
  Null,
  Boolean(bool),
  Signed(i64),
  Unsigned(u64),
  FloatBits(u64),
  String(String),
  Bytes(Vec<u8>),
  Array(Vec<CanonicalConfigValueV1>),
  Map(BTreeMap<String, CanonicalConfigValueV1>),
}

pub fn canonicalize_json(bytes: &[u8], bounds: CanonicalValueBounds) -> FormatResult<Vec<u8>> {
  if bytes.len() > bounds.maximum_value_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "canonical_json_oversize",
      format!("{} bytes exceeds {}", bytes.len(), bounds.maximum_value_length),
    ));
  }
  let value = serde_json::from_slice::<CanonicalConfigValueV1>(bytes).map_err(|source| {
    let class = if source.to_string().contains("duplicate canonical JSON key") {
      MalformedInputClass::NoncanonicalOrderOrDuplicate
    } else {
      MalformedInputClass::UnknownTypeKindOrEnum
    };
    error(class, "canonical_json_parse", source.to_string())
  })?;
  encode_canonical_value(&value, bounds)
}

pub fn encode_canonical_value(value: &CanonicalConfigValueV1, bounds: CanonicalValueBounds) -> FormatResult<Vec<u8>> {
  encode_value(value, 0, bounds)
}

pub fn decode_canonical_value(bytes: &[u8], bounds: CanonicalValueBounds) -> FormatResult<CanonicalConfigValueV1> {
  validate_canonical_value(bytes, bounds)?;
  let (value, end) = decode_value_at(bytes, 0, bytes.len())?;
  if end != bytes.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "config_value_decode_trailing",
      format!("{} trailing bytes", bytes.len() - end),
    ));
  }
  Ok(value)
}

pub fn validate_canonical_value(bytes: &[u8], bounds: CanonicalValueBounds) -> FormatResult<CanonicalValueSummary> {
  if bytes.len() > bounds.maximum_value_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "config_value_oversize",
      format!("{} bytes exceeds {}", bytes.len(), bounds.maximum_value_length),
    ));
  }
  let (summary, end) = validate_at(bytes, 0, bytes.len(), 0, bounds)?;
  if end != bytes.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "config_value_trailing",
      format!("{} trailing bytes", bytes.len() - end),
    ));
  }
  Ok(summary)
}

fn encode_value(value: &CanonicalConfigValueV1, depth: usize, bounds: CanonicalValueBounds) -> FormatResult<Vec<u8>> {
  match value {
    CanonicalConfigValueV1::Null => encode_frame(0x01, &[], bounds),
    CanonicalConfigValueV1::Boolean(false) => encode_frame(0x02, &[], bounds),
    CanonicalConfigValueV1::Boolean(true) => encode_frame(0x03, &[], bounds),
    CanonicalConfigValueV1::Signed(value) => encode_frame(0x04, &value.to_le_bytes(), bounds),
    CanonicalConfigValueV1::Unsigned(value) => {
      if !bounds.allow_small_u64 && *value <= i64::MAX as u64 {
        return Err(error(
          MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
          "config_u64_noncanonical",
          "u64 values at or below i64::MAX must use the i64 tag",
        ));
      }
      encode_frame(0x05, &value.to_le_bytes(), bounds)
    }
    CanonicalConfigValueV1::FloatBits(bits) => {
      let value = f64::from_bits(*bits);
      if !value.is_finite() || *bits == (-0.0f64).to_bits() {
        return Err(error(
          MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
          "config_f64_noncanonical",
          "f64 must be finite and encode zero positively",
        ));
      }
      encode_frame(0x06, &bits.to_le_bytes(), bounds)
    }
    CanonicalConfigValueV1::String(value) => {
      if value.len() > bounds.maximum_scalar_length {
        return Err(error(
          MalformedInputClass::AllocationAmplification,
          "config_scalar_oversize",
          format!("string is {} bytes", value.len()),
        ));
      }
      encode_frame(0x07, value.as_bytes(), bounds)
    }
    CanonicalConfigValueV1::Bytes(value) => {
      if value.len() > bounds.maximum_scalar_length {
        return Err(error(
          MalformedInputClass::AllocationAmplification,
          "config_scalar_oversize",
          format!("byte string is {} bytes", value.len()),
        ));
      }
      encode_frame(0x08, value, bounds)
    }
    CanonicalConfigValueV1::Array(values) => {
      validate_encode_container(depth, values.len(), "array")?;
      let mut payload = Vec::new();
      append_encoded(&mut payload, &(values.len() as u32).to_le_bytes(), bounds)?;
      for value in values {
        append_encoded(&mut payload, &encode_value(value, depth + 1, bounds)?, bounds)?;
      }
      encode_frame(0x09, &payload, bounds)
    }
    CanonicalConfigValueV1::Map(values) => {
      validate_encode_container(depth, values.len(), "map")?;
      let mut payload = Vec::new();
      append_encoded(&mut payload, &(values.len() as u32).to_le_bytes(), bounds)?;
      for (key, value) in values {
        if key.len() > bounds.maximum_key_length {
          return Err(error(
            MalformedInputClass::AllocationAmplification,
            "config_map_key_oversize",
            format!("key is {} bytes", key.len()),
          ));
        }
        let key_length = u32::try_from(key.len()).map_err(|_| length_error("canonical map key length exceeds u32"))?;
        append_encoded(&mut payload, &key_length.to_le_bytes(), bounds)?;
        append_encoded(&mut payload, key.as_bytes(), bounds)?;
        append_encoded(&mut payload, &encode_value(value, depth + 1, bounds)?, bounds)?;
      }
      encode_frame(0x0a, &payload, bounds)
    }
  }
}

fn validate_encode_container(depth: usize, members: usize, kind: &'static str) -> FormatResult<()> {
  if depth >= MAX_CONTAINER_DEPTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "config_container_depth",
      format!("{kind} exceeds {MAX_CONTAINER_DEPTH} containers"),
    ));
  }
  validate_member_count(members, kind)
}

fn append_encoded(target: &mut Vec<u8>, bytes: &[u8], bounds: CanonicalValueBounds) -> FormatResult<()> {
  let next_length = target.len().checked_add(bytes.len()).ok_or_else(|| length_error("canonical config length overflow"))?;
  if FRAME_LENGTH.checked_add(next_length).is_none_or(|length| length > bounds.maximum_value_length) {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "config_value_oversize",
      format!("canonical value exceeds {} bytes", bounds.maximum_value_length),
    ));
  }
  target.extend_from_slice(bytes);
  Ok(())
}

fn encode_frame(tag: u8, payload: &[u8], bounds: CanonicalValueBounds) -> FormatResult<Vec<u8>> {
  let total_length = FRAME_LENGTH.checked_add(payload.len()).ok_or_else(|| length_error("canonical config length overflow"))?;
  if total_length > bounds.maximum_value_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "config_value_oversize",
      format!("{total_length} bytes exceeds {}", bounds.maximum_value_length),
    ));
  }
  let payload_length = u32::try_from(payload.len()).map_err(|_| length_error("canonical config payload exceeds u32"))?;
  let mut bytes = Vec::with_capacity(total_length);
  bytes.push(tag);
  bytes.extend_from_slice(&payload_length.to_le_bytes());
  bytes.extend_from_slice(payload);
  Ok(bytes)
}

fn decode_value_at(bytes: &[u8], start: usize, limit: usize) -> FormatResult<(CanonicalConfigValueV1, usize)> {
  let header_end = start.checked_add(FRAME_LENGTH).ok_or_else(|| length_error("canonical decode frame overflow"))?;
  let payload_length = u32_at(bytes, start + 1)? as usize;
  let payload_end = header_end.checked_add(payload_length).ok_or_else(|| length_error("canonical decode payload overflow"))?;
  if payload_end > limit {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "config_value_decode_length", "payload exceeds enclosing value"));
  }
  let payload = &bytes[header_end..payload_end];
  let value = match bytes[start] {
    0x01 => CanonicalConfigValueV1::Null,
    0x02 => CanonicalConfigValueV1::Boolean(false),
    0x03 => CanonicalConfigValueV1::Boolean(true),
    0x04 => CanonicalConfigValueV1::Signed(i64::from_le_bytes(payload.try_into().expect("validated i64 payload"))),
    0x05 => CanonicalConfigValueV1::Unsigned(u64::from_le_bytes(payload.try_into().expect("validated u64 payload"))),
    0x06 => CanonicalConfigValueV1::FloatBits(u64::from_le_bytes(payload.try_into().expect("validated f64 payload"))),
    0x07 => CanonicalConfigValueV1::String(std::str::from_utf8(payload).expect("validated UTF-8 payload").to_string()),
    0x08 => CanonicalConfigValueV1::Bytes(payload.to_vec()),
    0x09 => {
      let count = u32_at(bytes, header_end)? as usize;
      let mut cursor = header_end + 4;
      let mut values = Vec::with_capacity(count);
      for _ in 0..count {
        let (value, next) = decode_value_at(bytes, cursor, payload_end)?;
        values.push(value);
        cursor = next;
      }
      CanonicalConfigValueV1::Array(values)
    }
    0x0a => {
      let count = u32_at(bytes, header_end)? as usize;
      let mut cursor = header_end + 4;
      let mut values = BTreeMap::new();
      for _ in 0..count {
        let key_length = u32_at(bytes, cursor)? as usize;
        let key_start = cursor + 4;
        let key_end = key_start + key_length;
        let key = std::str::from_utf8(&bytes[key_start..key_end]).expect("validated canonical map key").to_string();
        let (value, next) = decode_value_at(bytes, key_end, payload_end)?;
        values.insert(key, value);
        cursor = next;
      }
      CanonicalConfigValueV1::Map(values)
    }
    _ => unreachable!("canonical value was validated before decode"),
  };
  Ok((value, payload_end))
}

impl<'de> Deserialize<'de> for CanonicalConfigValueV1 {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    deserializer.deserialize_any(CanonicalJSONVisitor)
  }
}

struct CanonicalJSONVisitor;

impl<'de> Visitor<'de> for CanonicalJSONVisitor {
  type Value = CanonicalConfigValueV1;

  fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("a canonical JSON value")
  }

  fn visit_unit<E>(self) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Null)
  }

  fn visit_none<E>(self) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Null)
  }

  fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Boolean(value))
  }

  fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::Signed(value))
  }

  fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
    if value <= i64::MAX as u64 {
      return Ok(CanonicalConfigValueV1::Signed(value as i64));
    }
    Ok(CanonicalConfigValueV1::Unsigned(value))
  }

  fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
  where
    E: de::Error,
  {
    if !value.is_finite() || value.to_bits() == (-0.0f64).to_bits() {
      return Err(E::custom("canonical JSON number must be finite and encode zero positively"));
    }
    Ok(CanonicalConfigValueV1::FloatBits(value.to_bits()))
  }

  fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::String(value.to_string()))
  }

  fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
    Ok(CanonicalConfigValueV1::String(value))
  }

  fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
  where
    A: SeqAccess<'de>,
  {
    let mut values = Vec::new();
    while let Some(value) = sequence.next_element()? {
      if values.len() == MAX_CONTAINER_MEMBERS {
        return Err(de::Error::custom("canonical JSON array exceeds member cap"));
      }
      values.push(value);
    }
    Ok(CanonicalConfigValueV1::Array(values))
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut values = BTreeMap::new();
    while let Some((key, value)) = map.next_entry::<String, CanonicalConfigValueV1>()? {
      if values.contains_key(&key) {
        return Err(de::Error::custom(format!("duplicate canonical JSON key {key}")));
      }
      if values.len() == MAX_CONTAINER_MEMBERS {
        return Err(de::Error::custom("canonical JSON map exceeds member cap"));
      }
      values.insert(key, value);
    }
    Ok(CanonicalConfigValueV1::Map(values))
  }
}

fn validate_at(
  bytes: &[u8],
  start: usize,
  limit: usize,
  depth: usize,
  bounds: CanonicalValueBounds,
) -> FormatResult<(CanonicalValueSummary, usize)> {
  let header_end = start.checked_add(FRAME_LENGTH).ok_or_else(|| length_error("config frame offset overflow"))?;
  if header_end > limit || header_end > bytes.len() {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "config_value_truncated", "canonical frame is truncated"));
  }
  let payload_length = u32_at(bytes, start + 1)? as usize;
  let payload_end = header_end.checked_add(payload_length).ok_or_else(|| length_error("config payload end overflow"))?;
  if payload_end > limit || payload_end > bytes.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "config_value_length",
      format!("payload ends at {payload_end}, limit {limit}"),
    ));
  }
  if payload_end - start > bounds.maximum_value_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "config_nested_value_oversize",
      format!("nested value is {} bytes", payload_end - start),
    ));
  }

  let payload = &bytes[header_end..payload_end];
  let summary = match bytes[start] {
    0x01 if payload.is_empty() => summary("null", "bytes", 0),
    0x02 if payload.is_empty() => summary("false", "bytes", 0),
    0x03 if payload.is_empty() => summary("true", "bytes", 0),
    0x04 if payload.len() == 8 => summary("i64", "bytes", 8),
    0x05 if payload.len() == 8 && (bounds.allow_small_u64 || u64_at(payload, 0)? > i64::MAX as u64) => summary("u64", "bytes", 8),
    0x06 if payload.len() == 8 => {
      let value = f64::from_bits(u64_at(payload, 0)?);
      if !value.is_finite() || value.to_bits() == (-0.0f64).to_bits() {
        return Err(error(
          MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
          "config_f64_noncanonical",
          "f64 must be finite and encode zero positively",
        ));
      }
      summary("f64", "bytes", 8)
    }
    0x07 if payload.len() <= bounds.maximum_scalar_length => {
      std::str::from_utf8(payload)
        .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "config_utf8", format!("invalid UTF-8: {source}")))?;
      summary("utf8", "bytes", payload.len())
    }
    0x08 if payload.len() <= bounds.maximum_scalar_length => summary("bytes", "bytes", payload.len()),
    0x09 => summary("array", "members", validate_array(bytes, header_end, payload_end, depth, bounds)?),
    0x0a => summary("map", "members", validate_map(bytes, header_end, payload_end, depth, bounds)?),
    0x07 | 0x08 => {
      return Err(error(
        MalformedInputClass::AllocationAmplification,
        "config_scalar_oversize",
        format!("scalar is {} bytes", payload.len()),
      ));
    }
    0x01..=0x06 => {
      return Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "config_scalar_payload",
        format!("tag {:#04x} has noncanonical payload length {}", bytes[start], payload.len()),
      ));
    }
    tag => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "config_value_tag", format!("unknown tag {tag:#04x}")));
    }
  };
  Ok((summary, payload_end))
}

fn validate_array(bytes: &[u8], start: usize, end: usize, depth: usize, bounds: CanonicalValueBounds) -> FormatResult<usize> {
  validate_container_header(bytes, start, end, depth, "array")?;
  let count = u32_at(bytes, start)? as usize;
  validate_member_count(count, "array")?;
  let mut cursor = start + 4;
  for _ in 0..count {
    let (_, next) = validate_at(bytes, cursor, end, depth + 1, bounds)?;
    cursor = next;
  }
  if cursor != end {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "config_array_count",
      format!("array members ended at {cursor}, payload ends at {end}"),
    ));
  }
  Ok(count)
}

fn validate_map(bytes: &[u8], start: usize, end: usize, depth: usize, bounds: CanonicalValueBounds) -> FormatResult<usize> {
  validate_container_header(bytes, start, end, depth, "map")?;
  let count = u32_at(bytes, start)? as usize;
  validate_member_count(count, "map")?;
  let mut cursor = start + 4;
  let mut previous_key: Option<&[u8]> = None;
  for _ in 0..count {
    let key_header_end = cursor.checked_add(4).ok_or_else(|| length_error("config map key header overflow"))?;
    if key_header_end > end {
      return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "config_map_key_truncated", "map key length is truncated"));
    }
    let key_length = u32_at(bytes, cursor)? as usize;
    if key_length > bounds.maximum_key_length {
      return Err(error(MalformedInputClass::AllocationAmplification, "config_map_key_oversize", format!("key is {key_length} bytes")));
    }
    let key_end = key_header_end.checked_add(key_length).ok_or_else(|| length_error("config map key end overflow"))?;
    if key_end > end {
      return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "config_map_key_truncated", "map key bytes are truncated"));
    }
    let key = &bytes[key_header_end..key_end];
    std::str::from_utf8(key).map_err(|source| {
      error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "config_map_key_utf8", format!("invalid UTF-8: {source}"))
    })?;
    if previous_key.is_some_and(|previous| previous >= key) {
      return Err(error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "config_map_key_order", "map keys are not strictly increasing"));
    }
    previous_key = Some(key);
    let (_, next) = validate_at(bytes, key_end, end, depth + 1, bounds)?;
    cursor = next;
  }
  if cursor != end {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "config_map_count",
      format!("map members ended at {cursor}, payload ends at {end}"),
    ));
  }
  Ok(count)
}

fn validate_container_header(bytes: &[u8], start: usize, end: usize, depth: usize, kind: &'static str) -> FormatResult<()> {
  if depth >= MAX_CONTAINER_DEPTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "config_container_depth",
      format!("{kind} exceeds {MAX_CONTAINER_DEPTH} containers"),
    ));
  }
  if start.checked_add(4).is_none_or(|header_end| header_end > end || header_end > bytes.len()) {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "config_container_header", format!("{kind} count is truncated")));
  }
  Ok(())
}

fn validate_member_count(count: usize, kind: &'static str) -> FormatResult<()> {
  if count > MAX_CONTAINER_MEMBERS {
    return Err(error(MalformedInputClass::AllocationAmplification, "config_container_members", format!("{kind} has {count} members")));
  }
  Ok(())
}

fn summary(tag_name: &'static str, detail_name: &'static str, detail: usize) -> CanonicalValueSummary {
  CanonicalValueSummary { tag_name, detail_name, detail }
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes
    .get(offset..offset + 4)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "config_u32_truncated", format!("u32 at offset {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked config u32 length")))
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let raw = bytes
    .get(offset..offset + 8)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "config_u64_truncated", format!("u64 at offset {offset}")))?;
  Ok(u64::from_le_bytes(raw.try_into().expect("checked config u64 length")))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "config_value_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
