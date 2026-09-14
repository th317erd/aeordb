//! Typed selector execution against ordered, hand-authored semantic vectors.
use super::*;

fn fixture() -> serde_json::Value {
  serde_json::from_str(include_str!("../fixtures/v4/native-semantic-conformance-v1/regex-selector-v1/fixtures.json")).unwrap()
}

fn definition_bytes(profile: &str) -> Vec<u8> {
  std::fs::read(format!(
    "{}/spec/fixtures/v4/value-store-definition-v1/avst-{profile}-json-corrected-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap()
}

#[test]
fn json_regex_workspace_accounts_for_worst_case_escape_expansion() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let encoded = definition_bytes(profile);
    let mut definition = decode_value_store_definition(&encoded, algorithm).unwrap();
    let dependency = definition.dependencies.records.iter_mut().find(|record| record.kind == 2 && record.role == 4).unwrap();
    // The immutable format fixture predates the executable conformance identity.
    // Bind only this test-owned decoded copy; preserve the original golden bytes.
    *dependency = NativeSemanticComponentV1::RegexSelector.dependency_record();
    let runtime = ValueStoreRuntimeV1::from_definition(definition, algorithm.hash_length()).unwrap();
    assert!(runtime.selector_execution_supported());
    assert!(runtime.json_segments.iter().any(|segment| matches!(segment, CompiledJsonPathSegmentV1::Regex(_))));
    let frame_count = runtime.json_segments.len() as u64 * 2 + 1;
    let frame_bytes = frame_count * std::mem::size_of::<SelectorFrameV1<'_>>() as u64;
    assert_eq!(
      runtime.maximum_extract_workspace_bytes().unwrap(),
      VALUE_STORE_EXTRACT_WORKSPACE_FIXED_BYTES + frame_bytes + 6 * 4 * 1_024 * 1_024,
    );
  }
}

#[test]
fn unavailable_json_regex_retains_definition_without_executable_memory() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let encoded = definition_bytes(profile);
    let runtime = ValueStoreRuntimeV1::from_encoded(&encoded, algorithm).unwrap();
    assert!(!runtime.selector_execution_supported());
    assert!(runtime.json_segments.is_empty());
    assert_eq!(runtime.json_segments.capacity(), 0);
    assert_eq!(
      runtime.maximum_extract_workspace_bytes().unwrap(),
      VALUE_STORE_EXTRACT_WORKSPACE_FIXED_BYTES + std::mem::size_of::<SelectorFrameV1<'_>>() as u64,
    );
    let definition = runtime.definition();
    let retained_bytes = VALUE_STORE_RUNTIME_FIXED_BYTES
      + definition.value_store_id.capacity() as u64
      + (definition.selector.segments.capacity() * std::mem::size_of::<JsonPathSegmentV1<'_>>()) as u64
      + (definition.parser_plan.candidates.capacity() * std::mem::size_of::<ParserCandidateV1<'_>>()) as u64
      + (definition.dependencies.records.capacity() * std::mem::size_of::<DependencyRecordV1<'_>>()) as u64;
    assert_eq!(ValueStoreRuntimeV1::maximum_retained_bytes_for_definition(definition).unwrap(), retained_bytes);
    assert_eq!(runtime.ensure_selector_execution_supported().unwrap_err().class(), SourceOperationalErrorClassV1::DependencyUnavailable);
  }
}

fn canonical(value: &serde_json::Value) -> CanonicalConfigValueV1 {
  match value {
    serde_json::Value::Null => CanonicalConfigValueV1::Null,
    serde_json::Value::Bool(value) => CanonicalConfigValueV1::Boolean(*value),
    serde_json::Value::String(value) => CanonicalConfigValueV1::String(value.clone()),
    serde_json::Value::Array(values) => CanonicalConfigValueV1::Array(values.iter().map(canonical).collect()),
    serde_json::Value::Object(values) => {
      CanonicalConfigValueV1::Map(values.iter().map(|(key, value)| (key.clone(), canonical(value))).collect())
    }
    serde_json::Value::Number(value) => {
      if let Some(value) = value.as_i64() {
        return CanonicalConfigValueV1::Signed(value);
      }
      if let Some(value) = value.as_u64() {
        return CanonicalConfigValueV1::Unsigned(value);
      }
      CanonicalConfigValueV1::FloatBits(value.as_f64().unwrap().to_bits())
    }
  }
}

fn definition_for_case<'a>(bytes: &'a [u8], algorithm: HashAlgorithm, case: &'a serde_json::Value) -> ValueStoreDefinitionV1<'a> {
  let mut definition = decode_value_store_definition(bytes, algorithm).unwrap();
  let dependency = definition.dependencies.records.iter_mut().find(|dependency| dependency.kind == 2 && dependency.role == 4).unwrap();
  *dependency = NativeSemanticComponentV1::RegexSelector.dependency_record();
  definition.selector.segments = case["segments"]
    .as_array()
    .unwrap()
    .iter()
    .map(|segment| match segment["kind"].as_str().unwrap() {
      "object_key" => JsonPathSegmentV1::ObjectKey(segment["value"].as_str().unwrap()),
      "numeric_index" => JsonPathSegmentV1::NumericIndex(segment["value"].as_str().unwrap().parse().unwrap()),
      "fan_out" => JsonPathSegmentV1::FanOut,
      "regex" => JsonPathSegmentV1::Regex {
        pattern: segment["pattern"].as_str().unwrap(),
        case_insensitive: segment["case_insensitive"].as_bool().unwrap(),
      },
      other => panic!("unexercised segment kind: {other}"),
    })
    .collect();
  definition.selector.item_count = definition.selector.segments.len().try_into().unwrap();
  definition
}

fn extract(runtime: &ValueStoreRuntimeV1<'_>, input: &serde_json::Value) -> SourceExtractionV1 {
  let record = FileRecord::new("/conformance.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
  let input = canonical(input);
  runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&input) }, None, &|| false).unwrap()
}

fn assert_values(outcome: SourceExtractionV1, case: &serde_json::Value) {
  let expected: Vec<_> = case["expected_values"].as_array().unwrap().iter().map(canonical).collect();
  match outcome {
    SourceExtractionV1::Missing => assert!(expected.is_empty(), "{}", case["id"]),
    SourceExtractionV1::Values(values) => {
      assert!(!expected.is_empty(), "missing must not become an empty present values list");
      let decoded: Vec<_> = values.iter().map(|bytes| decode_canonical_value(bytes, CanonicalValueBounds::SOURCE_VALUE).unwrap()).collect();
      assert_eq!(decoded, expected, "{}", case["id"]);
    }
    other => panic!("{}: unexpected outcome {other:?}", case["id"]),
  }
}

#[test]
fn conformance_selector_preserves_exact_order_types_and_regex_semantics() {
  let vectors = fixture();
  assert_eq!(vectors["cases"].as_array().unwrap().len(), 24);
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = definition_bytes(profile);
    for case in vectors["cases"].as_array().unwrap() {
      let runtime = ValueStoreRuntimeV1::from_definition(definition_for_case(&bytes, algorithm, case), algorithm.hash_length()).unwrap();
      assert_values(extract(&runtime, &case["input"]), case);
    }
  }
}

#[test]
fn conformance_selector_exact_quota_and_one_below_are_whole_document_results() {
  let vectors = fixture();
  assert_eq!(vectors["limit_cases"].as_array().unwrap().len(), 6);
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = definition_bytes(profile);
    for case in vectors["limit_cases"].as_array().unwrap() {
      let exact = case["exact"].as_u64().unwrap();
      for limit in [exact, exact - 1] {
        let mut definition = definition_for_case(&bytes, algorithm, case);
        match case["field"].as_str().unwrap() {
          "max_selector_work_items_per_document" => definition.max_selector_work_items_per_document = limit,
          "max_selector_examined_bytes_per_document" => definition.max_selector_examined_bytes_per_document = limit,
          "max_source_values_per_document" => definition.max_source_values_per_document = limit.try_into().unwrap(),
          "max_canonical_source_bytes_per_document" => definition.max_canonical_source_bytes_per_document = limit,
          other => panic!("unexercised limit: {other}"),
        }
        let runtime = ValueStoreRuntimeV1::from_definition(definition, algorithm.hash_length()).unwrap();
        let outcome = extract(&runtime, &case["input"]);
        if limit == exact {
          assert_values(outcome, case);
        } else {
          let SourceExtractionV1::DeterministicUnindexable { code, .. } = outcome else {
            panic!("{}: limit did not reject the whole document: {outcome:?}", case["id"]);
          };
          assert_eq!(code, case["below_error"].as_str().unwrap(), "{}", case["id"]);
        }
      }
    }
  }
}

#[test]
fn conformance_selector_invalid_regex_never_falls_back_to_literal_keys() {
  let vectors = fixture();
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = definition_bytes(profile);
    for pattern in vectors["invalid_patterns"].as_array().unwrap() {
      let case = serde_json::json!({"segments": [{"kind": "regex", "pattern": pattern, "case_insensitive": false}]});
      assert!(
        ValueStoreRuntimeV1::from_definition(definition_for_case(&bytes, algorithm, &case), algorithm.hash_length()).is_err(),
        "{pattern}"
      );
    }
  }
}

#[test]
fn conformance_unavailable_selector_never_compiles_the_current_regex_implementation() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = definition_bytes(profile);
    // Typed test construction reaches the runtime independently of decoding;
    // this intentionally uncompilable regex detects an accidental compilation.
    let case = serde_json::json!({"segments": [{"kind": "regex", "pattern": "(", "case_insensitive": false}]});
    for unknown in 0..3 {
      let mut definition = definition_for_case(&bytes, algorithm, &case);
      let dependency = definition.dependencies.records.iter_mut().find(|record| record.role == 4).unwrap();
      match unknown {
        0 => dependency.fingerprint[0] ^= 1,
        1 => dependency.abi = u16::MAX,
        2 => dependency.executor_profile = u16::MAX,
        _ => unreachable!(),
      }
      let runtime = ValueStoreRuntimeV1::from_definition(definition, algorithm.hash_length()).unwrap();
      assert!(runtime.json_segments.is_empty());
      assert_eq!(runtime.ensure_selector_execution_supported().unwrap_err().class(), SourceOperationalErrorClassV1::DependencyUnavailable);
    }
  }
}

#[test]
fn conformance_selector_generated_nested_maps_match_a_materialized_reference() {
  let mut seed = 0x35f4_22c8u64;
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = definition_bytes(profile);
    for _ in 0..128 {
      let mut input = Vec::new();
      let mut expected = Vec::new();
      for row in 0..8 {
        let mut object = serde_json::Map::new();
        for column in (0..8).rev() {
          seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
          let key = format!("{}{column}", if seed & 1 == 0 { 'A' } else { 'b' });
          object.insert(key, serde_json::json!([row, column, seed % 10]));
        }
        // Materialized breadth-stage oracle, unlike the production heap-stack
        // traversal. ASCII key prefix matches ^a with the explicit i flag.
        let mut ordered: Vec<_> = object.iter().collect();
        ordered.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        for (key, values) in ordered {
          if key.as_bytes()[0].eq_ignore_ascii_case(&b'a') {
            expected.extend(values.as_array().unwrap().iter().cloned());
          }
        }
        input.push(serde_json::Value::Object(object));
      }
      let case = serde_json::json!({
        "id": "generated-nested-order",
        "segments": [{"kind": "fan_out"}, {"kind": "regex", "pattern": "^a", "case_insensitive": true}, {"kind": "fan_out"}],
        "expected_values": expected,
      });
      let runtime = ValueStoreRuntimeV1::from_definition(definition_for_case(&bytes, algorithm, &case), algorithm.hash_length()).unwrap();
      assert_values(extract(&runtime, &serde_json::Value::Array(input)), &case);
    }
  }
}
