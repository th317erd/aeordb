use super::dependency::{InvocationPolicyKind, InvocationPolicyV1, decode_invocation_policy};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const PLAN_HEADER_LENGTH: usize = 48;
const CANDIDATE_HEADER_LENGTH: usize = 32;
const POLICY_LENGTH: usize = 128;
const PLAN_MAX_LENGTH: usize = 128 * 1_024;
const MAX_REGISTRY_CANDIDATES: usize = 512;
const MAX_CANDIDATES: usize = MAX_REGISTRY_CANDIDATES + 2;
const MAX_CORRECTED_MIME_LENGTH: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserPlanKind {
  None,
  ExplicitPlugin,
  Automatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserCandidateKind {
  Explicit,
  Registry,
  RawJson,
  NativeSuite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserCandidateV1<'a> {
  pub kind: ParserCandidateKind,
  pub match_semantics: u16,
  pub dependency_ordinal: u32,
  pub match_bytes: &'a [u8],
  pub policy: InvocationPolicyV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParserResolutionPlanV1<'a> {
  pub kind: ParserPlanKind,
  pub resolution_semantics: u16,
  pub mime_semantics: u16,
  pub no_match_semantics: u16,
  pub mime_dependency_ordinal: u32,
  pub candidates: Vec<ParserCandidateV1<'a>>,
}

pub fn decode_parser_resolution_plan(value: &[u8]) -> FormatResult<ParserResolutionPlanV1<'_>> {
  if value.len() > PLAN_MAX_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "parser_plan_exceeds_cap",
      format!("{} bytes exceeds {PLAN_MAX_LENGTH}", value.len()),
    ));
  }
  if value.len() < PLAN_HEADER_LENGTH {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "parser_plan_truncated", "plan header is truncated"));
  }
  if &value[..4] != b"APRP" || u16_at(value, 4)? != 1 || u16_at(value, 6)? as usize != PLAN_HEADER_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "parser_plan_envelope", "expected APRP v1 with 48-byte header"));
  }
  if u32_at(value, 8)? as usize != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "parser_plan_total_length",
      format!("declared {}, got {}", u32_at(value, 8)?, value.len()),
    ));
  }
  if u32_at(value, 12)? != 0 || value[32..48].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "parser_plan_reserved", "flags or plan reserve are nonzero"));
  }

  let resolution_semantics = u16_at(value, 18)?;
  let mime_semantics = u16_at(value, 20)?;
  let no_match_semantics = u16_at(value, 22)?;
  let candidate_count = u32_at(value, 24)? as usize;
  let mime_dependency_ordinal = u32_at(value, 28)?;
  if candidate_count > MAX_CANDIDATES {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "parser_candidate_count",
      format!("{candidate_count} candidates exceeds {MAX_CANDIDATES}"),
    ));
  }

  let mut cursor = PLAN_HEADER_LENGTH;
  let mut candidates = Vec::with_capacity(candidate_count);
  for _ in 0..candidate_count {
    let (candidate, next) = decode_candidate(value, cursor)?;
    candidates.push(candidate);
    cursor = next;
  }
  if cursor != value.len() {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "parser_candidate_count_mismatch",
      format!("candidate records end at {cursor}, plan ends at {}", value.len()),
    ));
  }

  let kind = match u16_at(value, 16)? {
    1 => {
      if value.len() != PLAN_HEADER_LENGTH
        || resolution_semantics != 0
        || mime_semantics != 0
        || no_match_semantics != 0
        || mime_dependency_ordinal != 0
        || !candidates.is_empty()
      {
        return Err(error(MalformedInputClass::CrossRecordClosureMismatch, "parser_none_context", "none plan has nonempty context"));
      }
      ParserPlanKind::None
    }
    2 => {
      if !matches!(resolution_semantics, 1 | 2)
        || mime_semantics != 0
        || no_match_semantics != 0
        || mime_dependency_ordinal != 0
        || candidates.len() != 1
        || candidates[0].kind != ParserCandidateKind::Explicit
      {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "parser_explicit_context",
          "explicit plan does not contain exactly one applicable candidate",
        ));
      }
      ParserPlanKind::ExplicitPlugin
    }
    3 => {
      validate_automatic_plan(resolution_semantics, mime_semantics, no_match_semantics, mime_dependency_ordinal, &candidates)?;
      ParserPlanKind::Automatic
    }
    kind => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "parser_plan_kind", format!("unknown kind {kind}")));
    }
  };

  Ok(ParserResolutionPlanV1 { kind, resolution_semantics, mime_semantics, no_match_semantics, mime_dependency_ordinal, candidates })
}

fn decode_candidate(value: &[u8], start: usize) -> FormatResult<(ParserCandidateV1<'_>, usize)> {
  let header_end = start.checked_add(CANDIDATE_HEADER_LENGTH).ok_or_else(|| length_error("candidate header overflow"))?;
  if header_end > value.len() {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "parser_candidate_truncated", "candidate header is truncated"));
  }
  let total_length = u32_at(value, start)? as usize;
  let policy_length = u32_at(value, start + 12)? as usize;
  let match_length = u32_at(value, start + 16)? as usize;
  let expected_length = CANDIDATE_HEADER_LENGTH
    .checked_add(match_length)
    .and_then(|length| length.checked_add(policy_length))
    .ok_or_else(|| length_error("candidate length overflow"))?;
  let end = start.checked_add(total_length).ok_or_else(|| length_error("candidate end overflow"))?;
  if total_length != expected_length || policy_length != POLICY_LENGTH || end > value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "parser_candidate_length",
      format!("declared {total_length}, expected {expected_length}, end {end}"),
    ));
  }
  if u32_at(value, start + 20)? != 0 || value[start + 24..header_end].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "parser_candidate_reserved",
      "candidate flags or reserve are nonzero",
    ));
  }
  let dependency_ordinal = u32_at(value, start + 8)?;
  if dependency_ordinal == 0 {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "parser_candidate_dependency",
      "candidate dependency ordinal is zero",
    ));
  }
  let match_end = header_end.checked_add(match_length).ok_or_else(|| length_error("candidate match end overflow"))?;
  let match_bytes = &value[header_end..match_end];
  let policy = decode_invocation_policy(&value[match_end..end])?;
  let match_semantics = u16_at(value, start + 6)?;
  let kind = match u16_at(value, start + 4)? {
    1 if match_semantics == 0
      && match_bytes.is_empty()
      && matches!(policy.kind, InvocationPolicyKind::PureWasm | InvocationPolicyKind::LegacyWasm) =>
    {
      ParserCandidateKind::Explicit
    }
    2 if matches!(match_semantics, 1 | 2)
      && !match_bytes.is_empty()
      && matches!(policy.kind, InvocationPolicyKind::PureWasm | InvocationPolicyKind::LegacyWasm) =>
    {
      std::str::from_utf8(match_bytes).map_err(|source| {
        error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "parser_candidate_match_utf8", format!("invalid UTF-8: {source}"))
      })?;
      if match_semantics == 1
        && (policy.kind != InvocationPolicyKind::PureWasm
          || match_bytes.len() > MAX_CORRECTED_MIME_LENGTH
          || !is_canonical_mime_essence(match_bytes)
          || match_bytes == b"application/json")
      {
        return Err(error(
          MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
          "parser_candidate_mime",
          "corrected registry match is not a canonical, non-JSON MIME essence",
        ));
      }
      if match_semantics == 2 && policy.kind != InvocationPolicyKind::LegacyWasm {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "parser_candidate_context",
          "migration registry match requires a legacy-WASM policy",
        ));
      }
      ParserCandidateKind::Registry
    }
    3 if match_semantics == 0 && match_bytes.is_empty() && policy.kind == InvocationPolicyKind::Native => ParserCandidateKind::RawJson,
    4 if match_semantics == 0 && match_bytes.is_empty() && policy.kind == InvocationPolicyKind::Native => ParserCandidateKind::NativeSuite,
    1..=4 => {
      return Err(error(
        MalformedInputClass::CrossRecordClosureMismatch,
        "parser_candidate_context",
        "candidate match semantics, bytes, and policy are incompatible",
      ));
    }
    kind => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "parser_candidate_kind", format!("unknown kind {kind}")));
    }
  };
  Ok((ParserCandidateV1 { kind, match_semantics, dependency_ordinal, match_bytes, policy }, end))
}

fn validate_automatic_plan(
  resolution_semantics: u16,
  mime_semantics: u16,
  no_match_semantics: u16,
  mime_dependency_ordinal: u32,
  candidates: &[ParserCandidateV1<'_>],
) -> FormatResult<()> {
  if !matches!(resolution_semantics, 1 | 2)
    || !matches!(mime_semantics, 1 | 2)
    || !matches!(no_match_semantics, 1 | 2)
    || mime_dependency_ordinal == 0
    || candidates.len() < 2
  {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "parser_automatic_context",
      "automatic plan has inapplicable semantics, dependency, or candidate count",
    ));
  }
  let registry_count = candidates.len() - 2;
  if registry_count > MAX_REGISTRY_CANDIDATES
    || candidates[registry_count].kind != ParserCandidateKind::RawJson
    || candidates[registry_count + 1].kind != ParserCandidateKind::NativeSuite
    || candidates[..registry_count].iter().any(|candidate| candidate.kind != ParserCandidateKind::Registry)
  {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "parser_candidate_order",
      "automatic candidates are not registry tier followed by raw JSON and native suite",
    ));
  }
  if candidates[..registry_count].windows(2).any(|pair| pair[0].match_bytes >= pair[1].match_bytes) {
    return Err(error(
      MalformedInputClass::NoncanonicalOrderOrDuplicate,
      "parser_registry_order",
      "registry matches are not strictly byte ordered",
    ));
  }
  if resolution_semantics == 1
    && (mime_semantics != 1
      || no_match_semantics != 1
      || candidates[..registry_count]
        .iter()
        .any(|candidate| candidate.match_semantics != 1 || !is_canonical_mime_essence(candidate.match_bytes)))
  {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "parser_corrected_semantics",
      "corrected plan mixes semantic families",
    ));
  }
  if resolution_semantics == 2
    && (mime_semantics != 2
      || no_match_semantics != 2
      || candidates[..registry_count].iter().any(|candidate| candidate.match_semantics != 2 || candidate.match_bytes.is_empty()))
  {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "parser_legacy_semantics",
      "migration plan mixes semantic families",
    ));
  }
  Ok(())
}

fn is_canonical_mime_essence(value: &[u8]) -> bool {
  if value.len() > MAX_CORRECTED_MIME_LENGTH || value.iter().any(u8::is_ascii_uppercase) {
    return false;
  }
  let Some(slash) = value.iter().position(|byte| *byte == b'/') else {
    return false;
  };
  if slash == 0 || slash > 127 || slash + 1 == value.len() || value.len() - slash - 1 > 127 || value[slash + 1..].contains(&b'/') {
    return false;
  }
  value.iter().enumerate().all(|(index, byte)| {
    index == slash || byte.is_ascii_alphanumeric() || matches!(*byte, b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-')
  })
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let raw = bytes
    .get(offset..offset + 2)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "parser_u16_truncated", format!("u16 at {offset}")))?;
  Ok(u16::from_le_bytes(raw.try_into().expect("checked parser u16 length")))
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let raw = bytes
    .get(offset..offset + 4)
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "parser_u32_truncated", format!("u32 at {offset}")))?;
  Ok(u32::from_le_bytes(raw.try_into().expect("checked parser u32 length")))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "parser_length_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
