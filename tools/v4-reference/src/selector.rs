use crate::config;
use crate::core::HashProfile;
use crate::policy::{self, PolicyKind};

const SELECTOR_HEADER_LENGTH: usize = 32;
const SEGMENT_HEADER_LENGTH: usize = 8;
const MAPPER_HEADER_LENGTH: usize = 16;
const POLICY_LENGTH: usize = 128;
const SELECTOR_MAX_LENGTH: usize = 4 * 1_024;
const MAX_SEGMENTS: usize = 1_024;
const REGEX_COMPILED_SIZE_LIMIT: usize = 1_024 * 1_024;
const REGEX_DFA_SIZE_LIMIT: usize = 1_024 * 1_024;

#[derive(Clone, Copy)]
pub enum SelectorFormat {
  SourceSelectorV1,
}

impl SelectorFormat {
  pub fn id(self) -> &'static str {
    "source-selector-v1"
  }

  pub fn family(self) -> &'static str {
    "SourceSelectorV1"
  }
}

#[derive(Clone)]
pub struct SelectorFixtureCase {
  pub id: &'static str,
  pub format: SelectorFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone)]
enum Segment {
  ObjectKey(String),
  NumericIndex(u64),
  FanOut,
  Regex { pattern: String, case_insensitive: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DecodedSelector {
  pub kind: u16,
  pub item_count: u32,
  pub metadata_id: Option<u16>,
  pub dependency_ordinal: Option<u32>,
  pub mapper_contract: u16,
}

pub fn fixture_cases() -> Vec<SelectorFixtureCase> {
  let metadata = build_metadata(8).expect("metadata selector must encode");
  let root = build_json_path(&[]).expect("root selector must encode");
  let path = build_json_path(&[
    Segment::ObjectKey("messages".to_string()),
    Segment::NumericIndex(u64::MAX),
    Segment::FanOut,
    Segment::Regex { pattern: "^user$".to_string(), case_insensitive: true },
  ])
  .expect("path selector must encode");
  let corrected_mapper = build_mapper(2, 1, &canonical_null()).expect("corrected mapper selector must encode");
  let legacy_mapper = build_mapper(1, 1, &canonical_null()).expect("legacy mapper selector must encode");
  let always_missing = build_always_missing();
  let maximum = build_mapper(2, 1, &canonical_utf8(3_915)).expect("maximum selector must encode");
  assert_eq!(maximum.len(), SELECTOR_MAX_LENGTH);

  let mut cases = Vec::with_capacity(14);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for (suffix, bytes, expected, relation) in [
      ("metadata-hash", metadata.clone(), "selector:metadata:items=0", Some("metadata-id:hash")),
      ("json-root", root.clone(), "selector:json-path:items=0", Some("json-path:root")),
      ("json-mixed", path.clone(), "selector:json-path:items=4", Some("json-path:all-segment-kinds")),
      ("mapper-corrected", corrected_mapper.clone(), "selector:plugin-mapper:items=0", Some("mapper-contract:typed-plural-v1")),
      ("mapper-legacy", legacy_mapper.clone(), "selector:plugin-mapper:items=0", Some("mapper-contract:legacy-single-v0")),
      ("always-missing", always_missing.clone(), "selector:always-missing-v0:items=0", Some("migration:canonical-always-missing")),
      ("maximum-length", maximum.clone(), "selector:plugin-mapper:items=0", Some("boundary:4096-bytes")),
    ] {
      cases.push(SelectorFixtureCase {
        id: fixture_id(profile, suffix),
        format: SelectorFormat::SourceSelectorV1,
        profile,
        expected,
        relation,
        canonical_key: None,
        bytes,
      });
    }
  }
  cases
}

pub fn observe(_profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_selector(bytes) {
    Ok(selector) => {
      let kind = match selector.kind {
        1 => "metadata",
        2 => "json-path",
        3 => "plugin-mapper",
        4 => "always-missing-v0",
        _ => unreachable!("decoder rejects unknown selector kinds"),
      };
      (format!("selector:{kind}:items={}", selector.item_count), None)
    }
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(bytes: &[u8]) -> Vec<String> {
  vec![
    "selector +0x000 len 32: schema, kind, length, semantics, and reserve".to_string(),
    format!("selector kind: {}; item_count: {}", read_u16(bytes, 2).unwrap_or(0), read_u32(bytes, 12).unwrap_or(0)),
    format!("selector +0x020 len {}: kind-specific payload", bytes.len().saturating_sub(SELECTOR_HEADER_LENGTH)),
  ]
}

fn selector_header(kind: u16, total_length: usize, item_count: usize, regex_semantics: u16, mapper_contract: u16) -> Vec<u8> {
  let mut value = vec![0u8; SELECTOR_HEADER_LENGTH];
  put_u16(&mut value, 0, 1);
  put_u16(&mut value, 2, kind);
  put_u32(&mut value, 4, total_length as u32);
  put_u32(&mut value, 12, item_count as u32);
  put_u16(&mut value, 16, regex_semantics);
  put_u16(&mut value, 18, mapper_contract);
  value
}

fn build_metadata(metadata_id: u16) -> Result<Vec<u8>, &'static str> {
  let mut value = selector_header(1, 40, 0, 0, 0);
  value.resize(40, 0);
  put_u16(&mut value, 32, metadata_id);
  decode_selector(&value)?;
  Ok(value)
}

fn build_json_path(segments: &[Segment]) -> Result<Vec<u8>, &'static str> {
  if segments.len() > MAX_SEGMENTS {
    return Err("selector_item_count");
  }
  let mut body = Vec::new();
  for segment in segments {
    let encoded = build_segment(segment)?;
    if SELECTOR_HEADER_LENGTH + body.len() + encoded.len() > SELECTOR_MAX_LENGTH {
      return Err("selector_length");
    }
    body.extend_from_slice(&encoded);
  }
  let mut value = selector_header(2, SELECTOR_HEADER_LENGTH + body.len(), segments.len(), 1, 0);
  value.extend_from_slice(&body);
  decode_selector(&value)?;
  Ok(value)
}

fn build_mapper(mapper_contract: u16, dependency_ordinal: u32, arguments: &[u8]) -> Result<Vec<u8>, &'static str> {
  let policy_kind = match mapper_contract {
    1 => PolicyKind::LegacyWasm,
    2 => PolicyKind::PureWasm,
    _ => return Err("selector_mapper_contract"),
  };
  config::validate(arguments).map_err(|_| "selector_mapper_arguments")?;
  let policy = policy::build_policy(policy_kind);
  let total_length = SELECTOR_HEADER_LENGTH
    .checked_add(MAPPER_HEADER_LENGTH)
    .and_then(|length| length.checked_add(arguments.len()))
    .and_then(|length| length.checked_add(policy.len()))
    .ok_or("selector_length")?;
  if total_length > SELECTOR_MAX_LENGTH {
    return Err("selector_length");
  }
  let mut value = selector_header(3, total_length, 0, 0, mapper_contract);
  value.resize(SELECTOR_HEADER_LENGTH + MAPPER_HEADER_LENGTH, 0);
  put_u32(&mut value, 32, dependency_ordinal);
  put_u32(&mut value, 36, arguments.len() as u32);
  put_u32(&mut value, 40, policy.len() as u32);
  value.extend_from_slice(arguments);
  value.extend_from_slice(&policy);
  decode_selector(&value)?;
  Ok(value)
}

fn build_always_missing() -> Vec<u8> {
  selector_header(4, SELECTOR_HEADER_LENGTH, 0, 0, 0)
}

fn build_segment(segment: &Segment) -> Result<Vec<u8>, &'static str> {
  let (tag, flags, payload) = match segment {
    Segment::ObjectKey(key) => {
      if key.is_empty() {
        return Err("selector_object_key");
      }
      (1, 0, key.as_bytes().to_vec())
    }
    Segment::NumericIndex(index) => (2, 0, index.to_le_bytes().to_vec()),
    Segment::FanOut => (3, 0, Vec::new()),
    Segment::Regex { pattern, case_insensitive } => (4, u8::from(*case_insensitive), pattern.as_bytes().to_vec()),
  };
  let total_length = SEGMENT_HEADER_LENGTH.checked_add(payload.len()).ok_or("selector_segment_length")?;
  let mut value = vec![0u8; total_length];
  value[0] = tag;
  value[1] = flags;
  put_u32(&mut value, 4, payload.len() as u32);
  value[8..].copy_from_slice(&payload);
  Ok(value)
}

pub(crate) fn decode_selector(value: &[u8]) -> Result<DecodedSelector, &'static str> {
  if !(SELECTOR_HEADER_LENGTH..=SELECTOR_MAX_LENGTH).contains(&value.len()) {
    return Err("selector_length");
  }
  if read_u16(value, 0)? != 1 || read_u32(value, 4)? as usize != value.len() {
    return Err("selector_envelope");
  }
  if read_u32(value, 8)? != 0 || value[20..32].iter().any(|byte| *byte != 0) {
    return Err("selector_reserved");
  }
  let kind = read_u16(value, 2)?;
  let item_count = read_u32(value, 12)?;
  let regex_semantics = read_u16(value, 16)?;
  let mapper_contract = read_u16(value, 18)?;
  if item_count as usize > MAX_SEGMENTS {
    return Err("selector_item_count");
  }

  let mut decoded = DecodedSelector { kind, item_count, metadata_id: None, dependency_ordinal: None, mapper_contract };
  match kind {
    1 => {
      if value.len() != 40 || item_count != 0 || regex_semantics != 0 || mapper_contract != 0 || value[34..40].iter().any(|byte| *byte != 0)
      {
        return Err("selector_metadata_context");
      }
      let metadata_id = read_u16(value, 32)?;
      if !(1..=8).contains(&metadata_id) {
        return Err("selector_metadata_id");
      }
      decoded.metadata_id = Some(metadata_id);
    }
    2 => {
      if regex_semantics != 1 || mapper_contract != 0 {
        return Err("selector_json_context");
      }
      let mut cursor = SELECTOR_HEADER_LENGTH;
      for _ in 0..item_count {
        cursor = decode_segment(value, cursor)?;
      }
      if cursor != value.len() {
        return Err("selector_item_count_mismatch");
      }
    }
    3 => {
      if item_count != 0 || regex_semantics != 0 || !matches!(mapper_contract, 1 | 2) {
        return Err("selector_mapper_context");
      }
      let fixed_end = SELECTOR_HEADER_LENGTH + MAPPER_HEADER_LENGTH;
      if value.len() < fixed_end || read_u32(value, 44)? != 0 {
        return Err("selector_mapper_length");
      }
      let dependency_ordinal = read_u32(value, 32)?;
      let arguments_length = read_u32(value, 36)? as usize;
      let policy_length = read_u32(value, 40)? as usize;
      let arguments_end = fixed_end.checked_add(arguments_length).ok_or("selector_mapper_length")?;
      let expected_end = arguments_end.checked_add(policy_length).ok_or("selector_mapper_length")?;
      if dependency_ordinal == 0 || policy_length != POLICY_LENGTH || expected_end != value.len() {
        return Err("selector_mapper_length");
      }
      config::validate(&value[fixed_end..arguments_end]).map_err(|_| "selector_mapper_arguments")?;
      let policy_kind = policy::decode_policy(&value[arguments_end..]).map_err(|_| "selector_mapper_policy")?;
      if !matches!((mapper_contract, policy_kind), (1, PolicyKind::LegacyWasm) | (2, PolicyKind::PureWasm)) {
        return Err("selector_mapper_policy_context");
      }
      decoded.dependency_ordinal = Some(dependency_ordinal);
    }
    4 => {
      if value.len() != SELECTOR_HEADER_LENGTH || item_count != 0 || regex_semantics != 0 || mapper_contract != 0 {
        return Err("selector_always_missing_context");
      }
    }
    _ => return Err("selector_kind"),
  }
  Ok(decoded)
}

fn decode_segment(value: &[u8], start: usize) -> Result<usize, &'static str> {
  let header_end = start.checked_add(SEGMENT_HEADER_LENGTH).ok_or("selector_segment_length")?;
  if header_end > value.len() {
    return Err("selector_segment_truncated");
  }
  let payload_length = read_u32(value, start + 4)? as usize;
  let end = header_end.checked_add(payload_length).ok_or("selector_segment_length")?;
  if end > value.len() || read_u16(value, start + 2)? != 0 {
    return Err("selector_segment_length");
  }
  let tag = value[start];
  let flags = value[start + 1];
  let payload = &value[header_end..end];
  match tag {
    1 if flags == 0 && !payload.is_empty() => {
      std::str::from_utf8(payload).map_err(|_| "selector_segment_utf8")?;
    }
    2 if flags == 0 && payload.len() == 8 => {}
    3 if flags == 0 && payload.is_empty() => {}
    4 if flags & !0x01 == 0 => {
      let pattern = std::str::from_utf8(payload).map_err(|_| "selector_segment_utf8")?;
      compile_regex(pattern, flags & 0x01 != 0)?;
    }
    1..=4 => return Err("selector_segment_context"),
    _ => return Err("selector_segment_tag"),
  }
  Ok(end)
}

fn compile_regex(pattern: &str, case_insensitive: bool) -> Result<regex::Regex, &'static str> {
  regex::RegexBuilder::new(pattern)
    .case_insensitive(case_insensitive)
    .size_limit(REGEX_COMPILED_SIZE_LIMIT)
    .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
    .build()
    .map_err(|_| "selector_segment_regex")
}

fn canonical_null() -> Vec<u8> {
  vec![1, 0, 0, 0, 0]
}

fn canonical_utf8(payload_length: usize) -> Vec<u8> {
  let mut value = vec![0u8; 5 + payload_length];
  value[0] = 7;
  put_u32(&mut value, 1, payload_length as u32);
  value[5..].fill(b'x');
  value
}

fn fixture_id(profile: HashProfile, suffix: &str) -> &'static str {
  match (profile, suffix) {
    (HashProfile::Blake3_256, "metadata-hash") => "asel-blake3-256-metadata-hash-valid",
    (HashProfile::Blake3_256, "json-root") => "asel-blake3-256-json-root-valid",
    (HashProfile::Blake3_256, "json-mixed") => "asel-blake3-256-json-mixed-valid",
    (HashProfile::Blake3_256, "mapper-corrected") => "asel-blake3-256-mapper-corrected-valid",
    (HashProfile::Blake3_256, "mapper-legacy") => "asel-blake3-256-mapper-legacy-valid",
    (HashProfile::Blake3_256, "always-missing") => "asel-blake3-256-always-missing-valid",
    (HashProfile::Blake3_256, "maximum-length") => "asel-blake3-256-maximum-length-valid",
    (HashProfile::Sha512, "metadata-hash") => "asel-sha512-metadata-hash-valid",
    (HashProfile::Sha512, "json-root") => "asel-sha512-json-root-valid",
    (HashProfile::Sha512, "json-mixed") => "asel-sha512-json-mixed-valid",
    (HashProfile::Sha512, "mapper-corrected") => "asel-sha512-mapper-corrected-valid",
    (HashProfile::Sha512, "mapper-legacy") => "asel-sha512-mapper-legacy-valid",
    (HashProfile::Sha512, "always-missing") => "asel-sha512-always-missing-valid",
    (HashProfile::Sha512, "maximum-length") => "asel-sha512-maximum-length-valid",
    _ => unreachable!("fixture suffixes are fixed"),
  }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  Ok(u16::from_le_bytes(bytes.get(offset..offset + 2).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  Ok(u32::from_le_bytes(bytes.get(offset..offset + 4).ok_or("truncated")?.try_into().map_err(|_| "truncated")?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn selector_fixtures_match_expected_kinds() {
    for case in fixture_cases() {
      assert_eq!(observe(case.profile, &case.bytes).0, case.expected, "fixture {}", case.id);
    }
  }

  #[test]
  fn selector_envelope_kinds_and_reserves_fail_closed() {
    let metadata = fixture_cases().remove(0).bytes;
    for length in [0, 31] {
      assert_eq!(decode_selector(&metadata[..length]).err(), Some("selector_length"));
    }
    let mut reserve = metadata.clone();
    reserve[20] = 1;
    assert_eq!(decode_selector(&reserve).err(), Some("selector_reserved"));
    let mut unknown = metadata.clone();
    put_u16(&mut unknown, 2, 0);
    assert_eq!(decode_selector(&unknown).err(), Some("selector_kind"));
    let mut metadata_reserve = metadata;
    metadata_reserve[34] = 1;
    assert_eq!(decode_selector(&metadata_reserve).err(), Some("selector_metadata_context"));
  }

  #[test]
  fn json_segments_reject_malformed_tags_flags_lengths_utf8_and_counts() {
    let mixed = fixture_cases().remove(2).bytes;
    let mut tag = mixed.clone();
    tag[32] = 0;
    assert_eq!(decode_selector(&tag).err(), Some("selector_segment_tag"));
    let mut flag = mixed.clone();
    flag[33] = 1;
    assert_eq!(decode_selector(&flag).err(), Some("selector_segment_context"));
    let mut reserve = mixed.clone();
    reserve[34] = 1;
    assert_eq!(decode_selector(&reserve).err(), Some("selector_segment_length"));
    let mut invalid_utf8 = mixed.clone();
    invalid_utf8[40] = 0xff;
    assert_eq!(decode_selector(&invalid_utf8).err(), Some("selector_segment_utf8"));
    let mut count = mixed.clone();
    put_u32(&mut count, 12, 3);
    assert_eq!(decode_selector(&count).err(), Some("selector_item_count_mismatch"));
    put_u32(&mut count, 12, (MAX_SEGMENTS + 1) as u32);
    assert_eq!(decode_selector(&count).err(), Some("selector_item_count"));

    let invalid_regex = build_json_path(&[Segment::Regex { pattern: "[".to_string(), case_insensitive: false }]);
    assert_eq!(invalid_regex.err(), Some("selector_segment_regex"));
  }

  #[test]
  fn mapper_requires_canonical_arguments_exact_policy_and_nonzero_dependency() {
    let corrected = fixture_cases().remove(3).bytes;
    let mut ordinal = corrected.clone();
    put_u32(&mut ordinal, 32, 0);
    assert_eq!(decode_selector(&ordinal).err(), Some("selector_mapper_length"));
    let mut arguments = corrected.clone();
    arguments[48] = 0;
    assert_eq!(decode_selector(&arguments).err(), Some("selector_mapper_arguments"));
    let mut policy = corrected.clone();
    let policy_offset = 48 + canonical_null().len();
    policy[policy_offset] ^= 1;
    assert_eq!(decode_selector(&policy).err(), Some("selector_mapper_policy"));
    let mut contract = corrected;
    put_u16(&mut contract, 18, 1);
    assert_eq!(decode_selector(&contract).err(), Some("selector_mapper_policy_context"));
  }

  #[test]
  fn exact_selector_length_boundary_is_accepted_then_rejected() {
    let maximum = fixture_cases().remove(6).bytes;
    assert_eq!(maximum.len(), SELECTOR_MAX_LENGTH);
    assert!(decode_selector(&maximum).is_ok());
    let mut oversized = maximum;
    oversized.push(0);
    assert_eq!(decode_selector(&oversized).err(), Some("selector_length"));
  }

  #[test]
  fn aeor_regex_v1_conformance_is_search_based_unicode_aware_and_rejects_unsupported_syntax() {
    assert!(compile_regex("user", false).unwrap().is_match("end-user-role"));
    assert!(compile_regex("^café$", true).unwrap().is_match("CAFÉ"));
    assert!(compile_regex(r"^[\p{Greek}]+$", false).unwrap().is_match("αβγ"));
    assert_eq!(compile_regex(r"value(?=suffix)", false).err(), Some("selector_segment_regex"));
    assert_eq!(compile_regex("[", false).err(), Some("selector_segment_regex"));
  }

  #[test]
  fn every_selector_fixture_byte_is_structural_or_identity_protected() {
    for case in fixture_cases() {
      let original_digest = case.profile.digest(&case.bytes);
      for index in 0..case.bytes.len() {
        let mut mutated = case.bytes.clone();
        mutated[index] ^= 1;
        let observed = observe(case.profile, &mutated).0;
        assert!(
          observed.starts_with("error:") || case.profile.digest(&mutated) != original_digest,
          "fixture {} byte {index} was not protected",
          case.id
        );
      }
    }
  }
}
