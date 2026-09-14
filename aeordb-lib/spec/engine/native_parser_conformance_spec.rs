//! Exercises the production MIME and raw-JSON owners against reviewed vectors.
use super::*;
use crate::engine::v4::config_value::decode_canonical_value;

fn fixture(component: &str) -> serde_json::Value {
  let path = format!("{}/spec/fixtures/v4/native-semantic-conformance-v1/{component}/fixtures.json", env!("CARGO_MANIFEST_DIR"));
  let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
  assert_eq!(value["schema_version"], 1);
  assert_eq!(value["component"], format!("/org/aeordev/aeordb/native/{component}"));
  value
}

fn raw_candidate() -> ParserCandidateV1<'static> {
  let encoded = include_bytes!("../fixtures/v4/value-store-definition-v1/avst-blake3-256-json-corrected-valid.bin");
  let definition =
    crate::engine::v4::value_store::decode_value_store_definition(encoded, crate::engine::HashAlgorithm::Blake3_256).unwrap();
  definition.parser_plan.candidates.into_iter().find(|candidate| candidate.kind == ParserCandidateKind::RawJson).unwrap()
}

#[test]
fn conformance_native_parser_uses_the_reviewed_specification_and_fixture_identity() {
  let encoded = include_bytes!("../fixtures/v4/value-store-definition-v1/avst-blake3-256-json-corrected-valid.bin");
  let definition =
    crate::engine::v4::value_store::decode_value_store_definition(encoded, crate::engine::HashAlgorithm::Blake3_256).unwrap();
  let mut failures = Vec::new();
  for (component, role) in [("mime-router-v1", 3), ("raw-json-v1", 1), ("native-suite-v1", 1), ("regex-selector-v1", 4)] {
    let root = format!("{}/spec/fixtures/v4/native-semantic-conformance-v1/{component}", env!("CARGO_MANIFEST_DIR"));
    let specification = std::fs::read(format!("{root}/SPEC.md")).unwrap();
    let fixtures = std::fs::read(format!("{root}/fixtures.json")).unwrap();
    // Independent framing oracle: domain, LE-u64 spec length + bytes,
    // then LE-u64 manifest length + bytes. Production uses frozen literals.
    let mut manifest = b"aeordb.native-semantic-conformance.v1\0".to_vec();
    manifest.extend_from_slice(&(specification.len() as u64).to_le_bytes());
    manifest.extend_from_slice(&specification);
    manifest.extend_from_slice(&(fixtures.len() as u64).to_le_bytes());
    manifest.extend_from_slice(&fixtures);
    let fingerprint = *blake3::hash(&manifest).as_bytes();
    eprintln!("{component} {}", hex::encode(fingerprint));
    let manifest_id: serde_json::Value = serde_json::from_slice(&fixtures).unwrap();
    let registered =
      NativeSemanticComponentV1::ALL.into_iter().find(|entry| entry.dependency_id() == manifest_id["component"].as_str().unwrap()).unwrap();
    assert_eq!(registered.fingerprint(), fingerprint, "{component}: frozen specification/fixture digest");
    assert_eq!(registered.role(), role);
    if role == 4 {
      continue;
    }
    let id = match component {
      "mime-router-v1" => MIME_ROUTER_ID,
      "raw-json-v1" => RAW_JSON_ID,
      "native-suite-v1" => NATIVE_SUITE_ID,
      _ => unreachable!(),
    };
    let mut record = definition.dependencies.records.iter().find(|record| record.dependency_id == id).unwrap().clone();
    record.fingerprint = fingerprint;
    let table = DependencyTableV1 { records: vec![record] };
    if let Err(error) = require_native_dependency(&table, 1, id, role) {
      failures.push(format!("{error:?}"));
    }
  }
  assert!(failures.is_empty(), "reviewed native components unavailable: {failures:?}");
}

#[test]
fn conformance_native_availability_checks_every_dependency_identity_field() {
  for component in NativeSemanticComponentV1::ALL {
    let original = component.dependency_record();
    assert!(component.matches_dependency(&original));
    for field in 0..12 {
      let mut changed = original.clone();
      match field {
        0 => changed.kind = 1,
        1 => changed.role = if original.role == 1 { 3 } else { 1 },
        2 => changed.flags = 1,
        3 => changed.abi = u16::MAX,
        4 => changed.executor_profile = u16::MAX,
        5 => changed.fingerprint_semantics = 1,
        6 => changed.artifact_kind = 1,
        7 => changed.artifact_length = 1,
        8 => changed.fingerprint[0] ^= 1,
        9 => changed.dependency_id = "/org/aeordev/aeordb/native/unavailable",
        10 => changed.version = "1.0.1",
        11 => changed.fingerprint = *blake3::hash(format!("{}:semantic-conformance-v1", original.dependency_id).as_bytes()).as_bytes(),
        _ => unreachable!(),
      }
      assert!(!component.matches_dependency(&changed), "{component:?} field {field}");
    }
    for other in NativeSemanticComponentV1::ALL {
      assert_eq!(component.matches_dependency(&other.dependency_record()), component == other);
    }
  }
}

fn outcome_name(outcome: &RawJsonAttemptV1) -> &'static str {
  match outcome {
    RawJsonAttemptV1::Parsed(_) => "parsed",
    RawJsonAttemptV1::NotClaimed => "not_claimed",
    RawJsonAttemptV1::Deterministic(IndexParserOutcomeV1::DeterministicUnindexable(failure)) => {
      let decoded = decode_canonical_value(failure.evidence(), CanonicalValueBounds::CONFIG).unwrap();
      match decoded {
        CanonicalConfigValueV1::String(code) if code == "raw_json_malformed" => "malformed",
        CanonicalConfigValueV1::String(code) if code == "raw_json_policy_limit" => "policy",
        other => panic!("unexpected deterministic reason: {other:?}"),
      }
    }
    RawJsonAttemptV1::Deterministic(other) => panic!("not a deterministic failure: {other:?}"),
  }
}

#[test]
fn conformance_mime_normalization_and_length_boundaries() {
  let vectors = fixture("mime-router-v1");
  assert_eq!(vectors["media_types"].as_array().unwrap().len(), 22);
  for case in vectors["media_types"].as_array().unwrap() {
    assert_eq!(corrected_mime_essence(case["stored"].as_str()).as_deref(), case["essence"].as_str(), "{case}");
  }
  for case in vectors["boundary_media_types"].as_array().unwrap() {
    let input = format!(
      "{}/{}",
      "A".repeat(case["type_length"].as_u64().unwrap() as usize),
      "B".repeat(case["subtype_length"].as_u64().unwrap() as usize)
    );
    let output = corrected_mime_essence(Some(&input));
    assert_eq!(output.is_some(), case["valid"].as_bool().unwrap(), "{case}");
    if let Some(output) = output {
      assert_eq!(output, input.to_ascii_lowercase());
      assert!(output.len() <= 255);
    }
  }
}

#[test]
fn conformance_mime_extensions_and_json_claims() {
  let vectors = fixture("mime-router-v1");
  for case in vectors["extensions"].as_array().unwrap() {
    assert_eq!(corrected_extension(case["filename"].as_str().unwrap()).as_deref(), case["extension"].as_str(), "{case}");
  }
  for case in vectors["json_media_types"].as_array().unwrap() {
    // JSON claim takes a valid essence, never an unchecked stored string.
    let essence = corrected_mime_essence(case["essence"].as_str());
    assert_eq!(is_json_media_type(essence.as_deref(), case["essence"].as_str(), true), case["claimed"].as_bool().unwrap(), "{case}");
  }
}

#[test]
fn conformance_mime_parameter_grammar_and_all_ascii_character_classes() {
  let vectors = fixture("mime-router-v1");
  for case in vectors["parameter_cases"].as_array().unwrap() {
    let essence = corrected_mime_essence(case["stored"].as_str());
    let expected = case["valid"].as_bool().unwrap().then_some("text/plain");
    assert_eq!(essence.as_deref(), expected, "{case}");
  }
  for byte in 0..=127u8 {
    let character = char::from(byte);
    let token = byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte);
    let quoted = matches!(byte, 9 | 32..=33 | 35..=91 | 93..=126);
    let escaped = matches!(byte, 9 | 32..=126);
    for (stored, valid) in [
      (format!("text/plain; {character}=value"), token),
      (format!("text/plain; p={character}"), token),
      (format!("text/plain; p=\"{character}\""), quoted),
      (format!("text/plain; p=\"\\{character}\""), escaped),
    ] {
      assert_eq!(corrected_mime_essence(Some(&stored)).as_deref(), valid.then_some("text/plain"), "ASCII {byte}: {stored:?}");
    }
  }
}

#[test]
fn conformance_raw_json_exact_independent_canonical_bytes() {
  let vectors = fixture("raw-json-v1");
  assert_eq!(vectors["parsed"].as_array().unwrap().len(), 19);
  for case in vectors["parsed"].as_array().unwrap() {
    for claimed in [false, true] {
      let outcome = parse_raw_json(case["json"].as_str().unwrap().as_bytes(), &raw_candidate(), true, claimed).unwrap();
      let RawJsonAttemptV1::Parsed(value) = outcome else {
        panic!("valid vector was not parsed: {case}");
      };
      let bytes = encode_canonical_value(&value, CanonicalValueBounds::CONFIG).unwrap();
      assert_eq!(hex::encode(bytes), case["canonical_hex"].as_str().unwrap(), "{case}");
    }
  }
}

#[test]
fn conformance_raw_json_generated_integer_bytes_match_the_independent_codec_model() {
  let mut seed = 0x91c2_4e76_a513_082fu64;
  for _ in 0..512 {
    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    for signed in [false, true] {
      let (input, tag, number) = if signed {
        ((seed as i64).to_string(), 4u8, (seed as i64).to_le_bytes())
      } else {
        (seed.to_string(), if seed <= i64::MAX as u64 { 4 } else { 5 }, seed.to_le_bytes())
      };
      // Fixed canonical scalar framing from Round 7, not the encoder under test.
      let mut expected = vec![tag, 8, 0, 0, 0];
      expected.extend_from_slice(&number);
      let RawJsonAttemptV1::Parsed(value) = parse_raw_json(input.as_bytes(), &raw_candidate(), true, true).unwrap() else {
        panic!("integer rejected: {input}");
      };
      assert_eq!(encode_canonical_value(&value, CanonicalValueBounds::CONFIG).unwrap(), expected, "{input}");
    }
  }
}

#[test]
fn conformance_raw_json_malformed_and_not_claimed_are_distinct() {
  let vectors = fixture("raw-json-v1");
  assert_eq!(vectors["rejected"].as_array().unwrap().len(), 17);
  for case in vectors["rejected"].as_array().unwrap() {
    let bytes = match case["json"].as_str() {
      Some(value) => value.as_bytes().to_vec(),
      None => hex::decode(case["hex"].as_str().unwrap()).unwrap(),
    };
    let outcome = parse_raw_json(&bytes, &raw_candidate(), true, case["json_mime"].as_bool().unwrap()).unwrap();
    assert_eq!(outcome_name(&outcome), case["outcome"].as_str().unwrap(), "{case}");
  }
}

#[test]
fn conformance_raw_json_exact_and_exceeded_structural_limits() {
  let vectors = fixture("raw-json-v1");
  for case in vectors["limits"].as_array().unwrap() {
    let mut candidate = raw_candidate();
    let limit = case["limit"].as_u64().unwrap();
    match case["field"].as_str().unwrap() {
      "max_structure_nodes" => candidate.policy.max_structure_nodes = limit,
      "max_structure_depth" => candidate.policy.max_structure_depth = limit.try_into().unwrap(),
      "max_scalar_bytes" => candidate.policy.max_scalar_bytes = limit,
      "max_container_members" => candidate.policy.max_container_members = limit.try_into().unwrap(),
      other => panic!("unexercised policy field: {other}"),
    }
    let outcome = parse_raw_json(case["json"].as_str().unwrap().as_bytes(), &candidate, true, false).unwrap();
    assert_eq!(outcome_name(&outcome), case["outcome"].as_str().unwrap(), "{case}");
  }
}
