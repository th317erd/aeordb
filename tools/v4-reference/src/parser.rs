use crate::core::HashProfile;
use crate::policy::{self, PolicyKind};

const PLAN_HEADER_LENGTH: usize = 48;
const CANDIDATE_HEADER_LENGTH: usize = 32;
const POLICY_LENGTH: usize = 128;
const PLAN_MAX_LENGTH: usize = 128 * 1_024;
const MAX_REGISTRY_CANDIDATES: usize = 512;
const MAX_CORRECTED_MIME_LENGTH: usize = 255;

#[derive(Clone, Copy)]
pub enum ParserFormat {
  ParserResolutionPlanV1,
}

impl ParserFormat {
  pub fn id(self) -> &'static str {
    "parser-resolution-plan-v1"
  }

  pub fn family(self) -> &'static str {
    "ParserResolutionPlanV1"
  }
}

#[derive(Clone)]
pub struct ParserFixtureCase {
  pub id: &'static str,
  pub format: ParserFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone)]
struct Candidate {
  kind: u16,
  match_semantics: u16,
  dependency_ordinal: u32,
  match_bytes: Vec<u8>,
  policy: Vec<u8>,
}

#[derive(Clone)]
struct ParserPlan {
  kind: u16,
  resolution_semantics: u16,
  mime_semantics: u16,
  no_match_semantics: u16,
  mime_dependency_ordinal: u32,
  candidates: Vec<Candidate>,
}

#[derive(Clone)]
struct DecodedCandidate {
  kind: u16,
  match_semantics: u16,
  dependency_ordinal: u32,
  match_bytes: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DecodedPlan {
  pub kind: u16,
  pub resolution_semantics: u16,
  pub mime_semantics: u16,
  pub no_match_semantics: u16,
  pub mime_dependency_ordinal: u32,
  pub candidate_dependencies: Vec<(u16, u32)>,
}

pub fn fixture_cases() -> Vec<ParserFixtureCase> {
  let none = ParserPlan {
    kind: 1,
    resolution_semantics: 0,
    mime_semantics: 0,
    no_match_semantics: 0,
    mime_dependency_ordinal: 0,
    candidates: Vec::new(),
  };
  let explicit = ParserPlan {
    kind: 2,
    resolution_semantics: 1,
    mime_semantics: 0,
    no_match_semantics: 0,
    mime_dependency_ordinal: 0,
    candidates: vec![wasm_candidate(1, 1, Vec::new())],
  };
  let automatic = ParserPlan {
    kind: 3,
    resolution_semantics: 1,
    mime_semantics: 1,
    no_match_semantics: 1,
    mime_dependency_ordinal: 4,
    candidates: vec![
      wasm_candidate(2, 1, b"application/pdf".to_vec()),
      wasm_candidate(2, 1, b"text/plain".to_vec()),
      native_candidate(3, 3),
      native_candidate(4, 4),
    ],
  };
  let legacy_automatic = ParserPlan {
    kind: 3,
    resolution_semantics: 2,
    mime_semantics: 2,
    no_match_semantics: 2,
    mime_dependency_ordinal: 4,
    candidates: vec![legacy_wasm_candidate(2, b"Text/Plain; charset=UTF-8".to_vec()), native_candidate(3, 3), native_candidate(4, 4)],
  };

  let mut cases = Vec::with_capacity(8);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for (suffix, plan, expected, relation) in [
      ("none", &none, "parser-plan:none:candidates=0", Some("selector:metadata-or-always-missing")),
      ("explicit-plugin", &explicit, "parser-plan:explicit-plugin:candidates=1", Some("dependency:wasm-parser")),
      ("automatic", &automatic, "parser-plan:automatic:candidates=4", Some("registry:canonical-snapshot")),
      ("automatic-legacy", &legacy_automatic, "parser-plan:automatic:candidates=3", Some("migration:effective-pipeline-v0")),
    ] {
      cases.push(ParserFixtureCase {
        id: fixture_id(profile, suffix),
        format: ParserFormat::ParserResolutionPlanV1,
        profile,
        expected,
        relation,
        canonical_key: None,
        bytes: build_plan(plan).expect("fixture parser plan must encode"),
      });
    }
  }
  cases
}

pub fn observe(_profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_plan(bytes) {
    Ok(plan) => {
      let kind = match plan.kind {
        1 => "none",
        2 => "explicit-plugin",
        3 => "automatic",
        _ => unreachable!("decoder rejects unknown plan kinds"),
      };
      (format!("parser-plan:{kind}:candidates={}", plan.candidate_dependencies.len()), None)
    }
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(bytes: &[u8]) -> Vec<String> {
  vec![
    "plan +0x000 len 48: APRP header and semantic IDs".to_string(),
    format!("plan kind: {}; candidate_count: {}", read_u16(bytes, 16).unwrap_or(0), read_u32(bytes, 24).unwrap_or(0)),
    format!("plan +0x030 len {}: exact candidate records", bytes.len().saturating_sub(PLAN_HEADER_LENGTH)),
  ]
}

fn wasm_candidate(kind: u16, dependency_ordinal: u32, match_bytes: Vec<u8>) -> Candidate {
  Candidate {
    kind,
    match_semantics: if kind == 2 { 1 } else { 0 },
    dependency_ordinal,
    match_bytes,
    policy: policy::build_policy(PolicyKind::PureWasm),
  }
}

fn native_candidate(kind: u16, dependency_ordinal: u32) -> Candidate {
  Candidate { kind, match_semantics: 0, dependency_ordinal, match_bytes: Vec::new(), policy: policy::build_policy(PolicyKind::Native) }
}

fn legacy_wasm_candidate(dependency_ordinal: u32, match_bytes: Vec<u8>) -> Candidate {
  Candidate { kind: 2, match_semantics: 2, dependency_ordinal, match_bytes, policy: policy::build_policy(PolicyKind::LegacyWasm) }
}

fn build_plan(plan: &ParserPlan) -> Result<Vec<u8>, &'static str> {
  let mut body = Vec::new();
  for candidate in &plan.candidates {
    let encoded = build_candidate(candidate)?;
    if PLAN_HEADER_LENGTH + body.len() + encoded.len() > PLAN_MAX_LENGTH {
      return Err("parser_plan_length");
    }
    body.extend_from_slice(&encoded);
  }
  let total_length = PLAN_HEADER_LENGTH + body.len();
  let mut value = vec![0u8; PLAN_HEADER_LENGTH];
  value[0..4].copy_from_slice(b"APRP");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, PLAN_HEADER_LENGTH as u16);
  put_u32(&mut value, 8, total_length as u32);
  put_u16(&mut value, 16, plan.kind);
  put_u16(&mut value, 18, plan.resolution_semantics);
  put_u16(&mut value, 20, plan.mime_semantics);
  put_u16(&mut value, 22, plan.no_match_semantics);
  put_u32(&mut value, 24, plan.candidates.len() as u32);
  put_u32(&mut value, 28, plan.mime_dependency_ordinal);
  value.extend_from_slice(&body);
  decode_plan(&value)?;
  Ok(value)
}

fn build_candidate(candidate: &Candidate) -> Result<Vec<u8>, &'static str> {
  let total_length = CANDIDATE_HEADER_LENGTH
    .checked_add(candidate.match_bytes.len())
    .and_then(|length| length.checked_add(candidate.policy.len()))
    .ok_or("parser_candidate_length")?;
  if total_length > PLAN_MAX_LENGTH {
    return Err("parser_candidate_length");
  }
  let mut value = vec![0u8; total_length];
  put_u32(&mut value, 0, total_length as u32);
  put_u16(&mut value, 4, candidate.kind);
  put_u16(&mut value, 6, candidate.match_semantics);
  put_u32(&mut value, 8, candidate.dependency_ordinal);
  put_u32(&mut value, 12, candidate.policy.len() as u32);
  put_u32(&mut value, 16, candidate.match_bytes.len() as u32);
  value[32..32 + candidate.match_bytes.len()].copy_from_slice(&candidate.match_bytes);
  value[32 + candidate.match_bytes.len()..].copy_from_slice(&candidate.policy);
  Ok(value)
}

pub(crate) fn decode_plan(value: &[u8]) -> Result<DecodedPlan, &'static str> {
  if !(PLAN_HEADER_LENGTH..=PLAN_MAX_LENGTH).contains(&value.len()) {
    return Err("parser_plan_length");
  }
  if &value[0..4] != b"APRP"
    || read_u16(value, 4)? != 1
    || read_u16(value, 6)? as usize != PLAN_HEADER_LENGTH
    || read_u32(value, 8)? as usize != value.len()
  {
    return Err("parser_plan_envelope");
  }
  if read_u32(value, 12)? != 0 || value[32..48].iter().any(|byte| *byte != 0) {
    return Err("parser_plan_reserved");
  }
  let kind = read_u16(value, 16)?;
  let resolution_semantics = read_u16(value, 18)?;
  let mime_semantics = read_u16(value, 20)?;
  let no_match_semantics = read_u16(value, 22)?;
  let candidate_count = read_u32(value, 24)? as usize;
  let mime_dependency_ordinal = read_u32(value, 28)?;
  if candidate_count > MAX_REGISTRY_CANDIDATES + 2 {
    return Err("parser_candidate_count");
  }

  let mut cursor = PLAN_HEADER_LENGTH;
  let mut candidates = Vec::with_capacity(candidate_count.min(MAX_REGISTRY_CANDIDATES + 2));
  for _ in 0..candidate_count {
    let (candidate, next) = decode_candidate(value, cursor)?;
    candidates.push(candidate);
    cursor = next;
  }
  if cursor != value.len() {
    return Err("parser_candidate_count_mismatch");
  }

  match kind {
    1 => {
      if value.len() != PLAN_HEADER_LENGTH
        || resolution_semantics != 0
        || mime_semantics != 0
        || no_match_semantics != 0
        || mime_dependency_ordinal != 0
        || !candidates.is_empty()
      {
        return Err("parser_none_context");
      }
    }
    2 => {
      if !matches!(resolution_semantics, 1 | 2)
        || mime_semantics != 0
        || no_match_semantics != 0
        || mime_dependency_ordinal != 0
        || candidates.len() != 1
        || candidates[0].kind != 1
      {
        return Err("parser_explicit_context");
      }
    }
    3 => validate_automatic_plan(resolution_semantics, mime_semantics, no_match_semantics, mime_dependency_ordinal, &candidates)?,
    _ => return Err("parser_plan_kind"),
  }

  Ok(DecodedPlan {
    kind,
    resolution_semantics,
    mime_semantics,
    no_match_semantics,
    mime_dependency_ordinal,
    candidate_dependencies: candidates.iter().map(|candidate| (candidate.kind, candidate.dependency_ordinal)).collect(),
  })
}

fn decode_candidate(value: &[u8], start: usize) -> Result<(DecodedCandidate, usize), &'static str> {
  let header_end = start.checked_add(CANDIDATE_HEADER_LENGTH).ok_or("parser_candidate_length")?;
  if header_end > value.len() {
    return Err("parser_candidate_truncated");
  }
  let total_length = read_u32(value, start)? as usize;
  let match_length = read_u32(value, start + 16)? as usize;
  let policy_length = read_u32(value, start + 12)? as usize;
  let expected_length = CANDIDATE_HEADER_LENGTH
    .checked_add(match_length)
    .and_then(|length| length.checked_add(policy_length))
    .ok_or("parser_candidate_length")?;
  let end = start.checked_add(total_length).ok_or("parser_candidate_length")?;
  if total_length != expected_length || policy_length != POLICY_LENGTH || end > value.len() {
    return Err("parser_candidate_length");
  }
  if read_u32(value, start + 20)? != 0 || value[start + 24..header_end].iter().any(|byte| *byte != 0) {
    return Err("parser_candidate_reserved");
  }
  let kind = read_u16(value, start + 4)?;
  let match_semantics = read_u16(value, start + 6)?;
  let dependency_ordinal = read_u32(value, start + 8)?;
  if dependency_ordinal == 0 {
    return Err("parser_candidate_dependency");
  }
  let match_end = header_end + match_length;
  let match_bytes = value[header_end..match_end].to_vec();
  let backend = policy::decode_policy(&value[match_end..end]).map_err(|_| "parser_candidate_policy")?;
  match kind {
    1 if match_semantics == 0 && match_bytes.is_empty() && matches!(backend, PolicyKind::PureWasm | PolicyKind::LegacyWasm) => {}
    2 if matches!(match_semantics, 1 | 2)
      && !match_bytes.is_empty()
      && matches!(backend, PolicyKind::PureWasm | PolicyKind::LegacyWasm) =>
    {
      std::str::from_utf8(&match_bytes).map_err(|_| "parser_candidate_match_utf8")?;
      if match_semantics == 1
        && (backend != PolicyKind::PureWasm
          || match_bytes.len() > MAX_CORRECTED_MIME_LENGTH
          || !is_canonical_mime_essence(&match_bytes)
          || match_bytes == b"application/json")
      {
        return Err("parser_candidate_mime");
      }
      if match_semantics == 2 && backend != PolicyKind::LegacyWasm {
        return Err("parser_candidate_context");
      }
    }
    3 | 4 if match_semantics == 0 && match_bytes.is_empty() && backend == PolicyKind::Native => {}
    1..=4 => return Err("parser_candidate_context"),
    _ => return Err("parser_candidate_kind"),
  }
  Ok((DecodedCandidate { kind, match_semantics, dependency_ordinal, match_bytes }, end))
}

fn validate_automatic_plan(
  resolution_semantics: u16,
  mime_semantics: u16,
  no_match_semantics: u16,
  mime_dependency_ordinal: u32,
  candidates: &[DecodedCandidate],
) -> Result<(), &'static str> {
  if !matches!(resolution_semantics, 1 | 2)
    || !matches!(mime_semantics, 1 | 2)
    || !matches!(no_match_semantics, 1 | 2)
    || mime_dependency_ordinal == 0
    || candidates.len() < 2
  {
    return Err("parser_automatic_context");
  }
  let registry_count = candidates.len() - 2;
  if registry_count > MAX_REGISTRY_CANDIDATES
    || candidates[registry_count].kind != 3
    || candidates[registry_count + 1].kind != 4
    || candidates[..registry_count].iter().any(|candidate| candidate.kind != 2)
  {
    return Err("parser_candidate_order");
  }
  for pair in candidates[..registry_count].windows(2) {
    if pair[0].match_bytes >= pair[1].match_bytes {
      return Err("parser_registry_order");
    }
  }
  if resolution_semantics == 1
    && (mime_semantics != 1
      || no_match_semantics != 1
      || candidates[..registry_count]
        .iter()
        .any(|candidate| candidate.match_semantics != 1 || !is_canonical_mime_essence(&candidate.match_bytes)))
  {
    return Err("parser_corrected_semantics");
  }
  if resolution_semantics == 2
    && (mime_semantics != 2
      || no_match_semantics != 2
      || candidates[..registry_count].iter().any(|candidate| candidate.match_semantics != 2 || candidate.match_bytes.is_empty()))
  {
    return Err("parser_legacy_semantics");
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

fn fixture_id(profile: HashProfile, suffix: &str) -> &'static str {
  match (profile, suffix) {
    (HashProfile::Blake3_256, "none") => "aprp-blake3-256-none-valid",
    (HashProfile::Blake3_256, "explicit-plugin") => "aprp-blake3-256-explicit-plugin-valid",
    (HashProfile::Blake3_256, "automatic") => "aprp-blake3-256-automatic-valid",
    (HashProfile::Blake3_256, "automatic-legacy") => "aprp-blake3-256-automatic-legacy-valid",
    (HashProfile::Sha512, "none") => "aprp-sha512-none-valid",
    (HashProfile::Sha512, "explicit-plugin") => "aprp-sha512-explicit-plugin-valid",
    (HashProfile::Sha512, "automatic") => "aprp-sha512-automatic-valid",
    (HashProfile::Sha512, "automatic-legacy") => "aprp-sha512-automatic-legacy-valid",
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
  fn parser_fixtures_match_expected_plans() {
    for case in fixture_cases() {
      assert_eq!(observe(case.profile, &case.bytes).0, case.expected, "fixture {}", case.id);
    }
  }

  #[test]
  fn decoder_rejects_envelope_reserve_counts_and_trailing_records() {
    let none = fixture_cases().remove(0).bytes;
    for length in [0, 47] {
      assert_eq!(decode_plan(&none[..length]).err(), Some("parser_plan_length"));
    }
    let mut reserved = none.clone();
    reserved[32] = 1;
    assert_eq!(decode_plan(&reserved).err(), Some("parser_plan_reserved"));
    let mut count = none.clone();
    put_u32(&mut count, 24, 1);
    assert_eq!(decode_plan(&count).err(), Some("parser_candidate_truncated"));
    let mut trailing = none;
    trailing.push(0);
    put_u32(&mut trailing, 8, 49);
    assert_eq!(decode_plan(&trailing).err(), Some("parser_candidate_count_mismatch"));
  }

  #[test]
  fn plan_kinds_and_candidate_policy_contexts_fail_closed() {
    let mut explicit = fixture_cases().remove(1).bytes;
    put_u16(&mut explicit, 16, 1);
    assert_eq!(decode_plan(&explicit).err(), Some("parser_none_context"));
    put_u16(&mut explicit, 16, 2);
    put_u16(&mut explicit, 48 + 4, 3);
    assert_eq!(decode_plan(&explicit).err(), Some("parser_candidate_context"));

    let mut automatic = fixture_cases().remove(2).bytes;
    put_u16(&mut automatic, 48 + 4, 4);
    assert_eq!(decode_plan(&automatic).err(), Some("parser_candidate_context"));
    automatic = fixture_cases().remove(2).bytes;
    let second = 48 + read_u32(&automatic, 48).unwrap() as usize;
    automatic[48 + 32..48 + 32 + "application/pdf".len()].copy_from_slice(b"text/plainxxxxx");
    assert!(matches!(decode_plan(&automatic).err(), Some("parser_candidate_mime") | Some("parser_registry_order")));
    assert!(second < automatic.len());
  }

  #[test]
  fn registry_candidates_are_sorted_unique_and_correctly_normalized() {
    let plan = ParserPlan {
      kind: 3,
      resolution_semantics: 1,
      mime_semantics: 1,
      no_match_semantics: 1,
      mime_dependency_ordinal: 4,
      candidates: vec![
        wasm_candidate(2, 1, b"text/plain".to_vec()),
        wasm_candidate(2, 2, b"application/pdf".to_vec()),
        native_candidate(3, 3),
        native_candidate(4, 4),
      ],
    };
    assert_eq!(build_plan(&plan).err(), Some("parser_registry_order"));

    let mut uppercase = plan;
    uppercase.candidates.swap(0, 1);
    uppercase.candidates[0].match_bytes = b"Application/Pdf".to_vec();
    assert_eq!(build_plan(&uppercase).err(), Some("parser_candidate_mime"));

    uppercase.candidates[0].match_bytes = b"application/json".to_vec();
    assert_eq!(build_plan(&uppercase).err(), Some("parser_candidate_mime"));
  }

  #[test]
  fn lengths_ordinals_and_automatic_tiers_are_bounded() {
    let mut explicit = fixture_cases().remove(1).bytes;
    put_u32(&mut explicit, 48 + 8, 0);
    assert_eq!(decode_plan(&explicit).err(), Some("parser_candidate_dependency"));
    let mut wrong_length = fixture_cases().remove(1).bytes;
    put_u32(&mut wrong_length, 48, 31 + POLICY_LENGTH as u32);
    assert_eq!(decode_plan(&wrong_length).err(), Some("parser_candidate_length"));

    let mut too_many = vec![0u8; PLAN_HEADER_LENGTH];
    too_many[0..4].copy_from_slice(b"APRP");
    put_u16(&mut too_many, 4, 1);
    put_u16(&mut too_many, 6, PLAN_HEADER_LENGTH as u16);
    put_u32(&mut too_many, 8, PLAN_HEADER_LENGTH as u32);
    put_u32(&mut too_many, 24, (MAX_REGISTRY_CANDIDATES + 3) as u32);
    assert_eq!(decode_plan(&too_many).err(), Some("parser_candidate_count"));

    let mut maximum_registry = Vec::with_capacity(MAX_REGISTRY_CANDIDATES + 2);
    for index in 0..MAX_REGISTRY_CANDIDATES {
      maximum_registry.push(wasm_candidate(2, index as u32 + 1, format!("application/x-{index:03}").into_bytes()));
    }
    maximum_registry.push(native_candidate(3, 513));
    maximum_registry.push(native_candidate(4, 514));
    let maximum = ParserPlan {
      kind: 3,
      resolution_semantics: 1,
      mime_semantics: 1,
      no_match_semantics: 1,
      mime_dependency_ordinal: 515,
      candidates: maximum_registry,
    };
    assert_eq!(decode_plan(&build_plan(&maximum).unwrap()).unwrap().candidate_dependencies.len(), MAX_REGISTRY_CANDIDATES + 2);

    let mut over_count = maximum;
    over_count.candidates.insert(MAX_REGISTRY_CANDIDATES, wasm_candidate(2, 516, b"application/x-zzz".to_vec()));
    assert_eq!(build_plan(&over_count).err(), Some("parser_candidate_count"));
  }

  #[test]
  fn semantic_families_and_nested_policies_cannot_be_mixed() {
    let mut corrected = fixture_cases().remove(2).bytes;
    put_u16(&mut corrected, 20, 2);
    assert_eq!(decode_plan(&corrected).err(), Some("parser_corrected_semantics"));

    let mut legacy = fixture_cases().remove(3).bytes;
    put_u16(&mut legacy, 22, 1);
    assert_eq!(decode_plan(&legacy).err(), Some("parser_legacy_semantics"));

    let mut bad_policy = fixture_cases().remove(1).bytes;
    let policy_offset = PLAN_HEADER_LENGTH + CANDIDATE_HEADER_LENGTH;
    bad_policy[policy_offset] ^= 1;
    assert_eq!(decode_plan(&bad_policy).err(), Some("parser_candidate_policy"));

    let mut bad_match_semantics = fixture_cases().remove(2).bytes;
    put_u16(&mut bad_match_semantics, PLAN_HEADER_LENGTH + 6, 2);
    assert_eq!(decode_plan(&bad_match_semantics).err(), Some("parser_candidate_context"));
  }

  #[test]
  fn every_parser_fixture_byte_is_structural_or_identity_protected() {
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
