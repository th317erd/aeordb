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
