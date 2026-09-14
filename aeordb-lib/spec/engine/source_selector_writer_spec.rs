//! Canonical selectors from independent frozen bytes; no configuration coercion.
use aeordb::engine::v4::dependency::{InvocationPolicyKind, InvocationPolicyV1};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::source_selector::{JsonPathSegmentV1, SourceSelectorWriteV1, decode_source_selector, encode_source_selector};

fn policy(kind: InvocationPolicyKind) -> InvocationPolicyV1 {
  InvocationPolicyV1 {
    kind,
    max_request_bytes: 8 * 1_024 * 1_024,
    max_response_bytes: 4 * 1_024 * 1_024,
    max_linear_memory_bytes: 64 * 1_024 * 1_024,
    max_fuel: 50_000_000,
    max_table_elements: 100_000,
    max_structure_nodes: 100_000,
    max_scalar_bytes: 65_536,
    max_structure_depth: 32,
    max_container_members: 65_535,
    max_wasm_instances: 1,
    max_wasm_memories: 1,
    max_wasm_tables: 1,
    max_value_stack_height: 4_096,
    max_recursion_depth: 256,
  }
}

fn string_argument(payload_length: usize) -> Vec<u8> {
  let mut bytes = vec![7];
  bytes.extend_from_slice(&(payload_length as u32).to_le_bytes());
  bytes.resize(5 + payload_length, b'x');
  bytes
}

#[test]
fn selector_writer_matches_all_fourteen_frozen_fixtures() {
  let mixed = [
    JsonPathSegmentV1::ObjectKey("messages"),
    JsonPathSegmentV1::NumericIndex(u64::MAX),
    JsonPathSegmentV1::FanOut,
    JsonPathSegmentV1::Regex { pattern: "^user$", case_insensitive: true },
  ];
  let pure = policy(InvocationPolicyKind::PureWasm);
  let legacy = policy(InvocationPolicyKind::LegacyWasm);
  let maximum_arguments = string_argument(4_096 - 48 - 128 - 5);
  let null = [1, 0, 0, 0, 0];
  for profile in ["blake3-256", "sha512"] {
    for (name, request) in [
      ("metadata-hash", SourceSelectorWriteV1::Metadata { metadata_id: 8 }),
      ("json-root", SourceSelectorWriteV1::JsonPath { segments: &[] }),
      ("json-mixed", SourceSelectorWriteV1::JsonPath { segments: &mixed }),
      ("always-missing", SourceSelectorWriteV1::AlwaysMissingV0),
      (
        "mapper-corrected",
        SourceSelectorWriteV1::PluginMapper { dependency_ordinal: 1, mapper_contract: 2, arguments: &null, policy: &pure },
      ),
      (
        "mapper-legacy",
        SourceSelectorWriteV1::PluginMapper { dependency_ordinal: 1, mapper_contract: 1, arguments: &null, policy: &legacy },
      ),
      (
        "maximum-length",
        SourceSelectorWriteV1::PluginMapper { dependency_ordinal: 1, mapper_contract: 2, arguments: &maximum_arguments, policy: &pure },
      ),
    ] {
      let expected =
        std::fs::read(format!("{}/spec/fixtures/v4/source-selector-v1/asel-{profile}-{name}-valid.bin", env!("CARGO_MANIFEST_DIR")))
          .unwrap();
      assert_eq!(encode_source_selector(request).unwrap(), expected, "{profile}/{name}");
    }
  }
}

#[test]
fn selector_writer_accepts_exact_registered_metadata_ids_only() {
  for metadata_id in 1u16..=8 {
    let encoded = encode_source_selector(SourceSelectorWriteV1::Metadata { metadata_id }).unwrap();
    assert_eq!(encoded.len(), 40);
    assert_eq!(&encoded[32..34], &metadata_id.to_le_bytes());
    assert!(encoded[34..].iter().all(|byte| *byte == 0));
    assert_eq!(decode_source_selector(&encoded).unwrap().metadata_id, Some(metadata_id));
  }
  for metadata_id in [0, 9, u16::MAX] {
    assert_eq!(
      encode_source_selector(SourceSelectorWriteV1::Metadata { metadata_id }).unwrap_err().class(),
      MalformedInputClass::UnknownTypeKindOrEnum
    );
  }
}

#[test]
fn selector_writer_rejects_empty_keys_and_invalid_regex_without_literal_fallback() {
  assert_eq!(
    encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &[JsonPathSegmentV1::ObjectKey("")] }).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  for pattern in ["[", "(?z)", "(?=x)"] {
    assert_eq!(
      encode_source_selector(SourceSelectorWriteV1::JsonPath {
        segments: &[JsonPathSegmentV1::Regex { pattern, case_insensitive: false }],
      })
      .unwrap_err()
      .class(),
      MalformedInputClass::InvalidUtf8PathGlobOrNativePath
    );
  }
  let empty_regex = encode_source_selector(SourceSelectorWriteV1::JsonPath {
    segments: &[JsonPathSegmentV1::Regex { pattern: "", case_insensitive: false }],
  })
  .unwrap();
  assert_eq!(empty_regex.len(), 40);
}

#[test]
fn selector_writer_preflights_combined_lengths_and_counts_before_regex_work() {
  let maximum_key = "é".repeat((65_536 - 32 - 8) / 2);
  let encoded =
    encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &[JsonPathSegmentV1::ObjectKey(&maximum_key)] }).unwrap();
  assert_eq!(encoded.len(), 65_536);
  let oversized_key = format!("{maximum_key}x");
  for segment in
    [JsonPathSegmentV1::ObjectKey(&oversized_key), JsonPathSegmentV1::Regex { pattern: &"[".repeat(65_537), case_insensitive: false }]
  {
    assert_eq!(
      encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &[segment] }).unwrap_err().class(),
      MalformedInputClass::AllocationAmplification
    );
  }
  assert_eq!(
    encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &vec![JsonPathSegmentV1::FanOut; 1_025] }).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  for count in [508, 509, 1_024] {
    assert_eq!(
      encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &vec![JsonPathSegmentV1::FanOut; count] }).unwrap().len(),
      32 + 8 * count
    );
  }
}

#[test]
fn selector_writer_validates_mapper_dependency_contract_policy_and_canonical_arguments() {
  let pure = policy(InvocationPolicyKind::PureWasm);
  let legacy = policy(InvocationPolicyKind::LegacyWasm);
  let null = [1, 0, 0, 0, 0];
  for (dependency_ordinal, mapper_contract, invocation) in [(0, 2, &pure), (1, 0, &pure), (1, 3, &pure), (1, 1, &pure), (1, 2, &legacy)] {
    assert_eq!(
      encode_source_selector(SourceSelectorWriteV1::PluginMapper {
        dependency_ordinal,
        mapper_contract,
        arguments: &null,
        policy: invocation
      })
      .unwrap_err()
      .class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );
  }
  let mut invalid_policy = pure.clone();
  invalid_policy.max_fuel = 0;
  assert!(encode_source_selector(SourceSelectorWriteV1::PluginMapper {
    dependency_ordinal: 1,
    mapper_contract: 2,
    arguments: &null,
    policy: &invalid_policy
  })
  .is_err());
  for arguments in [&[][..], &[0, 0, 0, 0, 0], &[1, 0, 0, 0, 0, 0], &[7, 1, 0, 0, 0, 0xff]] {
    assert!(encode_source_selector(SourceSelectorWriteV1::PluginMapper {
      dependency_ordinal: 1,
      mapper_contract: 2,
      arguments,
      policy: &pure
    })
    .is_err());
  }
}

#[test]
fn selector_writer_mapper_cap_is_combined_not_an_independent_argument_allowance() {
  let pure = policy(InvocationPolicyKind::PureWasm);
  for (payload_length, should_fit) in [(3_915, true), (3_916, true), (65_355, true), (65_356, false), (65_536, false)] {
    let arguments = string_argument(payload_length);
    let result = encode_source_selector(SourceSelectorWriteV1::PluginMapper {
      dependency_ordinal: u32::MAX,
      mapper_contract: 2,
      arguments: &arguments,
      policy: &pure,
    });
    if should_fit {
      let encoded = result.unwrap();
      assert_eq!(encoded.len(), payload_length + 181);
      assert_eq!(&encoded[32..36], &u32::MAX.to_le_bytes());
    } else {
      assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
    }
  }
}

#[test]
fn selector_writer_preserves_order_types_and_exact_key_regex_bytes() {
  let segments = [
    JsonPathSegmentV1::ObjectKey("0"),
    JsonPathSegmentV1::NumericIndex(0),
    JsonPathSegmentV1::NumericIndex(u64::MAX),
    JsonPathSegmentV1::ObjectKey("a\0é"),
    JsonPathSegmentV1::Regex { pattern: "a", case_insensitive: false },
    JsonPathSegmentV1::Regex { pattern: "a", case_insensitive: true },
  ];
  let encoded = encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &segments }).unwrap();
  assert_eq!(decode_source_selector(&encoded).unwrap().segments, segments);
  let mut reversed = segments.clone();
  reversed.reverse();
  assert_ne!(encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &reversed }).unwrap(), encoded);
  let mut unique = std::collections::BTreeSet::new();
  for segment in &segments {
    assert!(unique.insert(encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: std::slice::from_ref(segment) }).unwrap()));
  }
}
