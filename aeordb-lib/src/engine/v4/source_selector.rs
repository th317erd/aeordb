use super::config_value::{CanonicalValueBounds, validate_canonical_value};
use super::dependency::{InvocationPolicyKind, InvocationPolicyV1, decode_invocation_policy};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const SELECTOR_HEADER_LENGTH: usize = 32;
const SEGMENT_HEADER_LENGTH: usize = 8;
const MAPPER_HEADER_LENGTH: usize = 16;
const POLICY_LENGTH: usize = 128;
const SELECTOR_MAX_LENGTH: usize = 4 * 1_024;
const MAX_SEGMENTS: usize = 1_024;
const REGEX_COMPILED_SIZE_LIMIT: usize = 1_024 * 1_024;
const REGEX_DFA_SIZE_LIMIT: usize = 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceSelectorKind {
  Metadata,
  JsonPath,
  PluginMapper,
  AlwaysMissingV0,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPathSegmentV1<'a> {
  ObjectKey(&'a str),
  NumericIndex(u64),
  FanOut,
  Regex { pattern: &'a str, case_insensitive: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSelectorV1<'a> {
  pub kind: SourceSelectorKind,
  pub item_count: u32,
  pub regex_semantics: u16,
  pub mapper_contract: u16,
  pub metadata_id: Option<u16>,
  pub segments: Vec<JsonPathSegmentV1<'a>>,
  pub dependency_ordinal: Option<u32>,
  pub arguments: Option<&'a [u8]>,
  pub policy: Option<InvocationPolicyV1>,
}

pub fn decode_source_selector(value: &[u8]) -> FormatResult<SourceSelectorV1<'_>> {
  if value.len() > SELECTOR_MAX_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "selector_exceeds_cap",
      format!("{} bytes exceeds {SELECTOR_MAX_LENGTH}", value.len()),
    ));
  }
  if value.len() < SELECTOR_HEADER_LENGTH {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "selector_truncated", "selector header is truncated"));
  }
  if u16_at(value, 0)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "selector_schema", "selector schema must be one"));
  }
  if u32_at(value, 4)? as usize != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "selector_total_length",
      format!("declared {}, got {}", u32_at(value, 4)?, value.len()),
    ));
  }
  if u32_at(value, 8)? != 0 || value[20..32].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "selector_reserved", "flags or selector reserve are nonzero"));
  }
  let item_count = u32_at(value, 12)?;
  if item_count as usize > MAX_SEGMENTS {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "selector_item_count",
      format!("{item_count} items exceeds {MAX_SEGMENTS}"),
    ));
  }
  let regex_semantics = u16_at(value, 16)?;
  let mapper_contract = u16_at(value, 18)?;

  let mut selector = SourceSelectorV1 {
    kind: SourceSelectorKind::AlwaysMissingV0,
    item_count,
    regex_semantics,
    mapper_contract,
    metadata_id: None,
    segments: Vec::new(),
    dependency_ordinal: None,
    arguments: None,
    policy: None,
  };
  selector.kind = match u16_at(value, 2)? {
    1 => {
      decode_metadata(value, item_count, regex_semantics, mapper_contract, &mut selector)?;
      SourceSelectorKind::Metadata
    }
    2 => {
      selector.segments = decode_json_path(value, item_count, regex_semantics, mapper_contract)?;
      SourceSelectorKind::JsonPath
    }
    3 => {
      decode_mapper(value, item_count, regex_semantics, mapper_contract, &mut selector)?;
      SourceSelectorKind::PluginMapper
    }
    4 => {
      if value.len() != SELECTOR_HEADER_LENGTH || item_count != 0 || regex_semantics != 0 || mapper_contract != 0 {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "selector_always_missing_context",
          "always-missing selector is not the canonical empty form",
        ));
      }
      SourceSelectorKind::AlwaysMissingV0
    }
    kind => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "selector_kind", format!("unknown kind {kind}")));
    }
  };
  Ok(selector)
}

fn decode_metadata<'a>(
  value: &'a [u8],
  item_count: u32,
  regex_semantics: u16,
  mapper_contract: u16,
  selector: &mut SourceSelectorV1<'a>,
) -> FormatResult<()> {
  if value.len() != 40 || item_count != 0 || regex_semantics != 0 || mapper_contract != 0 {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "selector_metadata_context",
      "metadata selector has noncanonical context or length",
    ));
  }
  if value[34..40].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "selector_metadata_reserved", "metadata reserve is nonzero"));
  }
  let metadata_id = u16_at(value, 32)?;
  if !(1..=8).contains(&metadata_id) {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "selector_metadata_id", format!("unknown metadata ID {metadata_id}")));
  }
  selector.metadata_id = Some(metadata_id);
  Ok(())
}

fn decode_json_path(value: &[u8], item_count: u32, regex_semantics: u16, mapper_contract: u16) -> FormatResult<Vec<JsonPathSegmentV1<'_>>> {
  if regex_semantics != 1 || mapper_contract != 0 {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "selector_json_context",
      "JSON-path selector requires AeorRegexV1 and no mapper contract",
    ));
  }
  let mut cursor = SELECTOR_HEADER_LENGTH;
  let mut segments = Vec::with_capacity(item_count as usize);
  for _ in 0..item_count {
    let (segment, next) = decode_segment(value, cursor)?;
    segments.push(segment);
    cursor = next;
  }
  if cursor != value.len() {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "selector_item_count_mismatch",
      format!("segments end at {cursor}, selector ends at {}", value.len()),
    ));
  }
  Ok(segments)
}

fn decode_mapper<'a>(
  value: &'a [u8],
  item_count: u32,
  regex_semantics: u16,
  mapper_contract: u16,
  selector: &mut SourceSelectorV1<'a>,
) -> FormatResult<()> {
  if item_count != 0 || regex_semantics != 0 || !matches!(mapper_contract, 1 | 2) {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "selector_mapper_context",
      "mapper selector has inapplicable item, regex, or contract fields",
    ));
  }
  let fixed_end = SELECTOR_HEADER_LENGTH + MAPPER_HEADER_LENGTH;
  if value.len() < fixed_end {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "selector_mapper_truncated", "mapper header is truncated"));
  }
  if u32_at(value, 44)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "selector_mapper_reserved", "mapper reserve is nonzero"));
  }
  let dependency_ordinal = u32_at(value, 32)?;
  let arguments_length = u32_at(value, 36)? as usize;
  let policy_length = u32_at(value, 40)? as usize;
  let arguments_end = fixed_end.checked_add(arguments_length).ok_or_else(|| length_error("mapper arguments end overflow"))?;
  let expected_end = arguments_end.checked_add(policy_length).ok_or_else(|| length_error("mapper policy end overflow"))?;
  if expected_end != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "selector_mapper_length",
      format!("nested fields end at {expected_end}, selector ends at {}", value.len()),
    ));
  }
  if dependency_ordinal == 0 || policy_length != POLICY_LENGTH {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "selector_mapper_contract",
      "mapper dependency is zero or policy length is not 128",
    ));
  }
  let arguments = &value[fixed_end..arguments_end];
  validate_canonical_value(arguments, CanonicalValueBounds::CONFIG)?;
  let policy = decode_invocation_policy(&value[arguments_end..])?;
  if !matches!((mapper_contract, policy.kind), (1, InvocationPolicyKind::LegacyWasm) | (2, InvocationPolicyKind::PureWasm)) {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "selector_mapper_policy_context",
      "mapper contract and invocation host profile disagree",
    ));
  }
  selector.dependency_ordinal = Some(dependency_ordinal);
  selector.arguments = Some(arguments);
  selector.policy = Some(policy);
  Ok(())
}

fn decode_segment(value: &[u8], start: usize) -> FormatResult<(JsonPathSegmentV1<'_>, usize)> {
  let header_end = start.checked_add(SEGMENT_HEADER_LENGTH).ok_or_else(|| length_error("segment header overflow"))?;
  if header_end > value.len() {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "selector_segment_truncated", "segment header is truncated"));
  }
  let payload_length = u32_at(value, start + 4)? as usize;
  let end = header_end.checked_add(payload_length).ok_or_else(|| length_error("segment end overflow"))?;
  if end > value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "selector_segment_length",
      format!("segment end {end} exceeds selector length {}", value.len()),
    ));
  }
  if u16_at(value, start + 2)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "selector_segment_reserved", "segment reserve is nonzero"));
  }
  let tag = value[start];
  let flags = value[start + 1];
  let payload = &value[header_end..end];
  let segment = match tag {
    1 if flags == 0 && !payload.is_empty() => JsonPathSegmentV1::ObjectKey(utf8(payload, "selector_object_key_utf8")?),
    2 if flags == 0 && payload.len() == 8 => {
      JsonPathSegmentV1::NumericIndex(u64::from_le_bytes(payload.try_into().expect("fixed numeric index")))
    }
    3 if flags == 0 && payload.is_empty() => JsonPathSegmentV1::FanOut,
    4 if flags & !0x01 == 0 => {
      let pattern = utf8(payload, "selector_regex_utf8")?;
      compile_regex(pattern, flags & 0x01 != 0)?;
      JsonPathSegmentV1::Regex { pattern, case_insensitive: flags & 0x01 != 0 }
    }
    1..=4 => {
      return Err(error(
        MalformedInputClass::CrossRecordClosureMismatch,
        "selector_segment_context",
        "segment tag, flags, and payload are incompatible",
      ));
    }
    tag => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "selector_segment_tag", format!("unknown tag {tag}")));
    }
  };
  Ok((segment, end))
}

fn compile_regex(pattern: &str, case_insensitive: bool) -> FormatResult<()> {
  regex::RegexBuilder::new(pattern)
    .case_insensitive(case_insensitive)
    .size_limit(REGEX_COMPILED_SIZE_LIMIT)
    .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
    .build()
    .map(|_| ())
    .map_err(|source| {
      error(
        MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
        "selector_segment_regex",
        format!("invalid or over-limit AeorRegexV1 pattern: {source}"),
      )
    })
}

fn utf8<'a>(bytes: &'a [u8], code: &'static str) -> FormatResult<&'a str> {
  std::str::from_utf8(bytes)
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, code, format!("invalid UTF-8: {source}")))
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes
    .get(offset..offset + 2)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "selector_u16_truncated", format!("u16 at {offset}")))?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked selector u16 length")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes
    .get(offset..offset + 4)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "selector_u32_truncated", format!("u32 at {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked selector u32 length")))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "selector_length_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
