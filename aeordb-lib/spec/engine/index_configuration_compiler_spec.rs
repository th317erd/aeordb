//! Whole corrected source configuration; frozen child codecs stay independent.
use std::cell::{Cell, RefCell};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::DependencyRecordV1;
use aeordb::engine::v4::index_configuration_compiler::{
  CompiledIndexConfigurationV1, IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
  default_index_configuration_v1,
};
use aeordb::engine::v4::parser_registry_compiler::{
  CompiledParserRegistryV1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1,
  compile_parser_registry_v1,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use sha2::Digest;
use serde_json::{Value, json};

const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];
const WORKSPACE: usize = 128 << 20;

#[derive(Default)]
struct Snapshot {
  calls: RefCell<Vec<(u16, String)>>,
  fail: bool,
}

fn dependency(role: u16) -> DependencyRecordV1<'static> {
  DependencyRecordV1 {
    kind: 1,
    role,
    flags: 4,
    abi: role + 2,
    executor_profile: 2,
    fingerprint_semantics: 1,
    artifact_kind: 1,
    artifact_length: 123,
    fingerprint: [0x42; 32],
    dependency_id: "/org/example/parser-and-mapper",
    version: "1.2.3",
  }
}

impl Snapshot {
  fn resolve(&self, role: u16, alias: &str) -> Result<Option<DependencyRecordV1<'static>>, SemanticCompilationErrorV1> {
    self.calls.borrow_mut().push((role, alias.to_string()));
    if self.fail {
      return Err(SemanticCompilationErrorV1::Operational { path: "/snapshot", message: "injected source read failure".into() });
    }
    Ok((alias != "missing").then(|| dependency(role)))
  }
}

impl ParserAliasSnapshotV1 for Snapshot {
  fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(1, alias)
  }
}

impl IndexConfigurationAliasSnapshotV1 for Snapshot {
  fn resolve_mapper_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(2, alias)
  }
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 32 << 20, 16 << 20).unwrap())
}

fn registry(memory: &MemoryCoordinator, algorithm: HashAlgorithm) -> CompiledParserRegistryV1 {
  compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: None,
      hash_algorithm: algorithm,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: WORKSPACE,
    },
    &Snapshot::default(),
    memory,
    &|| false,
  )
  .unwrap()
}

fn request<'a>(
  source: &'a [u8],
  owner_path: &'a str,
  registry: &'a CompiledParserRegistryV1,
  hash_algorithm: HashAlgorithm,
) -> IndexConfigurationCompilationRequestV1<'a> {
  IndexConfigurationCompilationRequestV1 {
    source,
    owner_path,
    registry,
    hash_algorithm,
    maximum_source_bytes: 256 << 10,
    maximum_workspace_bytes: WORKSPACE,
  }
}

fn compile(source: &[u8], owner_path: &str, algorithm: HashAlgorithm) -> Result<CompiledIndexConfigurationV1, SemanticCompilationErrorV1> {
  let memory = memory();
  let registry = registry(&memory, algorithm);
  compile_index_configuration_v1(request(source, owner_path, &registry, algorithm), &Snapshot::default(), &memory, &|| false)
}

#[test]
fn corrected_bootstrap_preserves_twelve_fields_eighteen_indexes_and_recursive_scope_for_every_hash() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let registry = registry(&memory, algorithm);
    let snapshot = Snapshot::default();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let compiled =
      compile_index_configuration_v1(request(default_index_configuration_v1(), "/", &registry, algorithm), &snapshot, &memory, &|| false)
        .unwrap();
    let scope = decode_scope_definition(&compiled.scope().value, algorithm).unwrap();
    assert_eq!(scope.owner_path, "/");
    assert_eq!(scope.glob, Some("**/*"));
    assert_eq!(compiled.fields().len(), 12);
    assert_eq!(compiled.fields().iter().map(|field| field.field_indexes().len()).sum::<usize>(), 18);
    assert_eq!(compiled.dependencies().len(), 4);
    assert!(compiled.fields().windows(2).all(|pair| pair[0].field_name() < pair[1].field_name()));
    assert!(snapshot.calls.borrow().is_empty());
    assert!(memory.snapshot().unwrap().reserved_bytes > baseline);
    drop(compiled);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
}

#[test]
fn empty_configuration_retains_a_real_scope_without_synthesizing_fields_or_parser_dependencies() {
  for algorithm in ALGORITHMS {
    let compiled = compile(br#"{"$v":1,"indexes":[]}"#, "  /data//./child/../items\0 ", algorithm).unwrap();
    let scope = decode_scope_definition(&compiled.scope().value, algorithm).unwrap();
    assert_eq!(scope.owner_path, "/data/items");
    assert_eq!(scope.glob, None);
    assert!(compiled.fields().is_empty());
    assert!(compiled.dependencies().is_empty());
    assert_eq!(compiled.projection().semantic_id.len(), algorithm.hash_length());
  }
}

#[test]
fn metadata_alias_converter_sets_row_order_and_nonsemantic_properties_have_one_projection() {
  let first = br#"{"$v":1,"glob":"//**///x","logging":false,"compression":"none","indexes":[{"name":"@filename","type":["unicode_trigram_v1","utf8_binary_order_v1","utf8_binary_order_v1"]},{"name":"@hash","type":"typed_exact_blake3_v1"}]}"#;
  let second = br#"{"indexes":[{"type":"typed_exact_blake3_v1","name":"@hash"},{"type":"utf8_binary_order_v1","name":"@file_name"},{"name":"@filename","type":"unicode_trigram_v1"}],"compression":"zstd","logging":true,"glob":"**/x","$v":1}"#;
  for algorithm in ALGORITHMS {
    let first = compile(first, "/data", algorithm).unwrap();
    let second = compile(second, "/data", algorithm).unwrap();
    assert_eq!(first.scope(), second.scope());
    assert_eq!(first.projection(), second.projection());
    assert_eq!(first.fields().len(), 2);
    assert_eq!(second.fields().len(), 2);
    assert!(first.dependencies().is_empty());
  }
}

#[test]
fn conflicting_same_field_sources_fail_instead_of_publishing_two_value_stores() {
  for source in [
    br#"{"$v":1,"indexes":[{"name":"value","type":"u64_order_v1","source":["left"]},{"name":"value","type":"typed_exact_blake3_v1","source":["right"]}]}"#.as_slice(),
    br#"{"$v":1,"indexes":[{"name":"@hash","type":"typed_exact_blake3_v1"},{"name":"@hash","type":"bytes_binary_order_v1","source_limits":{"max_source_values_per_document":1}}]}"#,
  ] {
    assert!(matches!(compile(source, "/", ALGORITHMS[0]), Err(SemanticCompilationErrorV1::InvalidSource { .. })));
  }
}

#[test]
fn strict_corrected_ingress_rejects_legacy_versions_unknown_members_duplicates_and_invalid_shapes() {
  for source in [
    "",
    "null",
    "[]",
    "{}",
    r#"{"indexes":[]}"#,
    r#"{"$v":0,"indexes":[]}"#,
    r#"{"$v":1.0,"indexes":[]}"#,
    r#"{"$v":1,"$v":1,"indexes":[]}"#,
    r#"{"$v":1,"indexes":[],"indexes":[]}"#,
    r#"{"$v":1,"indexes":[],"unknown":true}"#,
    r#"{"$v":1,"indexes":null}"#,
    r#"{"$v":1,"indexes":[],"logging":1}"#,
    r#"{"$v":1,"indexes":[],"glob":null}"#,
    r#"{"$v":1,"indexes":[{"name":"@hash","type":"hash"}]}"#,
    r#"{"$v":1,"indexes":[{"name":"@hash","type":[]}]}"#,
    r#"{"$v":1,"indexes":[{"field_name":"@hash","type":"typed_exact_blake3_v1"}]}"#,
    r#"{"$v":1,"indexes":[{"name":"@hash","type":"typed_exact_blake3_v1","min":0}]}"#,
    r#"{"$v":1,"indexes":[{"name":"@hash","type":"typed_exact_blake3_v1","source":[]}]}"#,
    r#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m","args":{"x":1,"x":2}}}]}"#,
    r#"{"$v":1,"indexes":[]} {}"#,
  ] {
    assert!(matches!(compile(source.as_bytes(), "/", ALGORITHMS[0]), Err(SemanticCompilationErrorV1::InvalidSource { .. })), "{source}");
  }
}

#[test]
fn explicit_parser_and_mapper_bind_distinct_roles_of_the_same_captured_module() {
  let source = br#"{"$v":1,"parser":"p","indexes":[{"name":"value","type":"typed_exact_blake3_v1","source":{"plugin":"m"}}]}"#;
  for algorithm in ALGORITHMS {
    let compiled = compile(source, "/data", algorithm).unwrap();
    assert_eq!(compiled.fields().len(), 1);
    assert_eq!(compiled.dependencies().len(), 2);
    assert_ne!(compiled.dependencies()[0].semantic_id, compiled.dependencies()[1].semantic_id);
    let value = decode_value_store_definition(&compiled.fields()[0].value_store().value, algorithm).unwrap();
    assert_eq!(value.dependencies.records.iter().map(|record| record.role).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(value.dependencies.records[0].fingerprint, value.dependencies.records[1].fingerprint);
  }
}

#[test]
fn dependency_absence_and_operational_failure_remain_distinct() {
  let memory = memory();
  let registry = registry(&memory, ALGORITHMS[0]);
  for (source, role) in [
    (br#"{"$v":1,"parser":"missing","indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]}"#.as_slice(), 1),
    (br#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"missing"}}]}"#.as_slice(), 2),
  ] {
    for fail in [false, true] {
      let snapshot = Snapshot { fail, ..Snapshot::default() };
      let result = compile_index_configuration_v1(request(source, "/", &registry, ALGORITHMS[0]), &snapshot, &memory, &|| false);
      if fail {
        assert!(matches!(result, Err(SemanticCompilationErrorV1::Operational { .. })));
      } else {
        assert!(matches!(result, Err(SemanticCompilationErrorV1::DependencyUnavailable { .. })));
      }
      assert!(snapshot.calls.borrow().iter().any(|call| call.0 == role));
    }
  }
}

fn identity(algorithm: HashAlgorithm, domain: &[u8], value: &[u8]) -> Vec<u8> {
  let mut bytes = domain.to_vec();
  bytes.extend_from_slice(value);
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(&bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(&bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(&bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(&bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(&bytes).to_vec(),
  }
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
  let mut value = vec![tag];
  value.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  value.extend_from_slice(payload);
  value
}

fn projection_map(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
  assert!(members.windows(2).all(|pair| pair[0].0.as_bytes() < pair[1].0.as_bytes()));
  let mut payload = (members.len() as u32).to_le_bytes().to_vec();
  for (key, value) in members {
    payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
    payload.extend_from_slice(key.as_bytes());
    payload.extend_from_slice(value);
  }
  frame(10, &payload)
}

fn fixture(family: &str, prefix: &str, algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  let profile = if algorithm.hash_length() == 64 { "sha512" } else { "blake3-256" };
  std::fs::read(format!("{}/spec/fixtures/v4/{family}/{prefix}-{profile}-{name}-valid.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn complete_metadata_projection_matches_independent_child_bytes_framing_and_hashes() {
  for algorithm in ALGORITHMS {
    let compiled = compile(br#"{"$v":1,"indexes":[{"name":"@hash","type":"typed_exact_blake3_v1"}]}"#, "/", algorithm).unwrap();
    let mut scope = vec![0; 65];
    scope[..4].copy_from_slice(b"ASCP");
    scope[4..6].copy_from_slice(&1u16.to_le_bytes());
    scope[6..8].copy_from_slice(&32u16.to_le_bytes());
    scope[8..12].copy_from_slice(&65u32.to_le_bytes());
    scope[32..36].copy_from_slice(&1u32.to_le_bytes());
    for offset in [40, 42, 44, 46, 48, 50, 52, 54] {
      scope[offset..offset + 2].copy_from_slice(&1u16.to_le_bytes());
    }
    scope[64] = b'/';
    let scope_id = identity(algorithm, b"aeordb.index.scope-definition.v1\0", &scope);
    assert_eq!(compiled.scope().value, scope);
    assert_eq!(compiled.scope().scope_id, scope_id);
    let fixed = 32 + algorithm.hash_length();
    let mut value = fixture("value-store-definition-v1", "avst", algorithm, "metadata-hash-corrected");
    value[32..fixed].copy_from_slice(&scope_id);
    value[fixed + 36..fixed + 40].copy_from_slice(&1024u32.to_le_bytes());
    value[fixed + 48..fixed + 56].copy_from_slice(&(8u64 << 20).to_le_bytes());
    let value_id = identity(algorithm, b"aeordb.index.value-store-definition.v1\0", &value);
    assert_eq!(compiled.fields()[0].value_store().value, value);
    assert_eq!(compiled.fields()[0].value_store().value_store_id, value_id);
    let mut converter = fixture("converter-definition-v1", "acnv", algorithm, "typed_exact_blake3_v1");
    converter[64..72].copy_from_slice(&(1u64 << 20).to_le_bytes());
    converter[72..76].copy_from_slice(&65536u32.to_le_bytes());
    converter[76..80].copy_from_slice(&(1u32 << 20).to_le_bytes());
    converter[80..88].copy_from_slice(&(4u64 << 20).to_le_bytes());
    let mut field = fixture("field-index-definition-v1", "afix", algorithm, "typed_exact_blake3_v1");
    field[32..fixed].copy_from_slice(&value_id);
    field[fixed + 44..fixed + 48].copy_from_slice(&65536u32.to_le_bytes());
    field[fixed + 48..fixed + 52].copy_from_slice(&65536u32.to_le_bytes());
    field[fixed + 56..fixed + 64].copy_from_slice(&(8u64 << 20).to_le_bytes());
    field[fixed + 64..fixed + 72].copy_from_slice(&(8u64 << 20).to_le_bytes());
    let converter_start = field.len() - converter.len();
    field[converter_start..].copy_from_slice(&converter);
    let index_id = identity(algorithm, b"aeordb.index.field-definition.v1\0", &field);
    assert_eq!(compiled.fields()[0].field_indexes()[0].value, field);
    assert_eq!(compiled.fields()[0].field_indexes()[0].index_id, index_id);
    let mut indexes = 1u32.to_le_bytes().to_vec();
    indexes.extend_from_slice(&frame(8, &index_id));
    let field_projection = projection_map(&[("indexes", frame(9, &indexes)), ("value_store_id", frame(8, &value_id))]);
    let expected = projection_map(&[("fields", projection_map(&[("@hash", field_projection)])), ("scope_id", frame(8, &scope_id))]);
    let object = &compiled.projection().object.value;
    assert_eq!(&object[48 + algorithm.hash_length()..object.len() - 4], expected);
    assert_eq!(
      compiled.projection().semantic_id,
      identity(algorithm, b"aeordb.semantic.effective-index-config-projection.v1\0", &expected)
    );
  }
}

#[test]
fn each_source_converter_and_field_limit_is_semantic_with_explicit_default_equivalence() {
  for algorithm in ALGORITHMS {
    let base = json!({"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]});
    let baseline = compile(&serde_json::to_vec(&base).unwrap(), "/", algorithm).unwrap();
    for (group, member, default, changed) in [
      ("source_limits", "max_source_values_per_document", 1024u64, 512u64),
      ("source_limits", "max_canonical_source_bytes_per_document", 8 << 20, 4 << 20),
      ("source_limits", "max_document_input_bytes", 64 << 20, 32 << 20),
      ("source_limits", "max_selector_work_items_per_document", 1_000_000, 500_000),
      ("source_limits", "max_selector_examined_bytes_per_document", 64 << 20, 32 << 20),
      ("converter_limits", "max_input_bytes", 1 << 20, 1 << 19),
      ("converter_limits", "max_output_values", 65536, 32768),
      ("converter_limits", "max_output_value_bytes", 1 << 20, 1 << 19),
      ("converter_limits", "max_total_output_bytes", 4 << 20, 2 << 20),
      ("field_limits", "max_terms_per_document", 65536, 32768),
      ("field_limits", "max_postings_per_document", 65536, 32768),
      ("field_limits", "max_canonical_posting_bytes_per_document", 8 << 20, 4 << 20),
      ("field_limits", "max_query_recheck_value_bytes", 8 << 20, 4 << 20),
    ] {
      let mut source = base.clone();
      source["indexes"][0][group] = json!({member: default});
      assert_eq!(compile(&serde_json::to_vec(&source).unwrap(), "/", algorithm).unwrap().projection(), baseline.projection());
      source["indexes"][0][group][member] = json!(changed);
      let compiled = compile(&serde_json::to_vec(&source).unwrap(), "/", algorithm).unwrap();
      assert_ne!(compiled.projection(), baseline.projection(), "{group}.{member}");
      assert_eq!(compiled.fields()[0].value_store() == baseline.fields()[0].value_store(), group != "source_limits");
      for invalid in [Value::Null, json!(-1), json!(1.0), json!("1"), json!(u64::MAX)] {
        source["indexes"][0][group][member] = invalid;
        assert!(
          matches!(compile(&serde_json::to_vec(&source).unwrap(), "/", algorithm), Err(SemanticCompilationErrorV1::InvalidSource { .. })),
          "{group}.{member}"
        );
      }
    }
  }
}

#[test]
fn all_invocation_policy_overrides_bind_only_their_concrete_call_site() {
  let properties = [
    ("max_request_bytes", 64u64 << 20, 32u64 << 20, true),
    ("max_response_bytes", 16 << 20, 8 << 20, false),
    ("max_linear_memory_bytes", 64 << 20, 32 << 20, true),
    ("max_fuel", 10_000_000, 9_000_000, true),
    ("max_table_elements", 65536, 32768, true),
    ("max_structure_nodes", 65536, 32768, false),
    ("max_scalar_bytes", 1 << 20, 1 << 19, false),
    ("max_structure_depth", 32, 16, false),
    ("max_container_members", 65535, 32767, false),
    ("max_wasm_instances", 1, 2, true),
    ("max_wasm_memories", 1, 2, true),
    ("max_wasm_tables", 1, 2, true),
    ("max_value_stack_height", 4096, 2048, false),
    ("max_recursion_depth", 256, 128, false),
  ];
  for tier in ["wasm", "raw_json", "native_suite", "mapper"] {
    let mut base = json!({"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]});
    if tier == "wasm" {
      base["parser"] = json!("p");
    } else if tier == "mapper" {
      base["indexes"][0]["source"] = json!({"plugin":"m"});
    }
    let baseline = compile(&serde_json::to_vec(&base).unwrap(), "/", ALGORITHMS[0]).unwrap();
    for (member, default, changed, wasm_only) in properties {
      let wasm = tier == "wasm" || tier == "mapper";
      let mut source = base.clone();
      let policy = if tier == "mapper" { &mut source["indexes"][0]["source"]["policy"] } else { &mut source["parser_policies"][tier] };
      *policy = json!({member: if wasm || !wasm_only { default } else { 0 }});
      assert_eq!(compile(&serde_json::to_vec(&source).unwrap(), "/", ALGORITHMS[0]).unwrap().projection(), baseline.projection());
      let policy = if tier == "mapper" { &mut source["indexes"][0]["source"]["policy"] } else { &mut source["parser_policies"][tier] };
      policy[member] = json!(changed);
      let result = compile(&serde_json::to_vec(&source).unwrap(), "/", ALGORITHMS[0]);
      if !wasm && wasm_only {
        assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })), "{tier}.{member}");
      } else {
        assert_ne!(result.unwrap().projection(), baseline.projection(), "{tier}.{member}");
      }
    }
  }
}

#[test]
fn parser_memory_alias_normalizes_without_overriding_a_conflicting_explicit_policy() {
  let first = br#"{"$v":1,"parser":"p","parser_memory_limit":" 32 MB ","indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]}"#;
  let second = br#"{"$v":1,"parser":"p","parser_policies":{"wasm":{"max_linear_memory_bytes":33554432}},"indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]}"#;
  assert_eq!(compile(first, "/", ALGORITHMS[0]).unwrap().projection(), compile(second, "/", ALGORITHMS[0]).unwrap().projection());
  for value in [json!(null), json!(32), json!("0"), json!("32 MiB"), json!("1"), json!("65mb")] {
    let mut source: Value = serde_json::from_slice(first).unwrap();
    source["parser_memory_limit"] = value;
    assert!(matches!(
      compile(&serde_json::to_vec(&source).unwrap(), "/", ALGORITHMS[0]),
      Err(SemanticCompilationErrorV1::InvalidSource { .. })
    ));
  }
  let mut source: Value = serde_json::from_slice(second).unwrap();
  source["parser_memory_limit"] = json!("64mb");
  assert!(matches!(
    compile(&serde_json::to_vec(&source).unwrap(), "/", ALGORITHMS[0]),
    Err(SemanticCompilationErrorV1::InvalidSource { .. })
  ));
  source["parser_memory_limit"] = json!("32mb");
  assert_eq!(
    compile(&serde_json::to_vec(&source).unwrap(), "/", ALGORITHMS[0]).unwrap().projection(),
    compile(second, "/", ALGORITHMS[0]).unwrap().projection()
  );
}

#[test]
fn ordered_sources_and_arguments_preserve_meaning_while_mapper_object_order_and_aliases_do_not() {
  for (first, equivalent, changed) in [
    (json!(["a", 0, "b"]), json!(["a", 0, "b"]), json!(["b", 0, "a"])),
    (
      json!({"plugin":"m","args":{"b":2,"a":[1,2]}}),
      json!({"args":{"a":[1,2],"b":2},"plugin":"another-alias"}),
      json!({"plugin":"m","args":{"a":[2,1],"b":2}}),
    ),
    (json!({"plugin":"m"}), json!({"plugin":"m","args":null}), json!({"plugin":"m","args":{}})),
  ] {
    let make =
      |source| serde_json::to_vec(&json!({"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":source}]})).unwrap();
    let first = compile(&make(first), "/", ALGORITHMS[0]).unwrap();
    assert_eq!(first.projection(), compile(&make(equivalent), "/", ALGORITHMS[0]).unwrap().projection());
    assert_ne!(first.projection(), compile(&make(changed), "/", ALGORITHMS[0]).unwrap().projection());
  }
}

#[test]
fn repeated_fields_merge_distinct_complete_indexes_and_each_alias_is_resolved_once() {
  let source = br#"{"$v":1,"parser":"p","indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m"}},{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m"},"field_limits":{"max_terms_per_document":1}},{"name":"y","type":"typed_exact_blake3_v1","source":{"plugin":"m"}}]}"#;
  let memory = memory();
  let registry = registry(&memory, ALGORITHMS[0]);
  let snapshot = Snapshot::default();
  let compiled = compile_index_configuration_v1(request(source, "/", &registry, ALGORITHMS[0]), &snapshot, &memory, &|| false).unwrap();
  assert_eq!(compiled.fields().len(), 2);
  assert_eq!(compiled.fields()[0].field_indexes().len(), 2);
  assert_eq!(compiled.dependencies().len(), 2);
  assert_eq!(*snapshot.calls.borrow(), vec![(1, "p".into()), (2, "m".into())]);
}

#[test]
fn unused_parser_does_not_create_dependencies_but_invalid_unused_policies_still_fail() {
  let memory = memory();
  let registry = registry(&memory, ALGORITHMS[0]);
  for rows in [json!([]), json!([{"name":"@hash","type":"typed_exact_blake3_v1"}])] {
    let source = serde_json::to_vec(&json!({"$v":1,"parser":"missing","indexes":rows})).unwrap();
    let snapshot = Snapshot { fail: true, ..Snapshot::default() };
    let compiled = compile_index_configuration_v1(request(&source, "/", &registry, ALGORITHMS[0]), &snapshot, &memory, &|| false).unwrap();
    assert!(snapshot.calls.borrow().is_empty());
    assert!(compiled.dependencies().is_empty());
  }
  let invalid = br#"{"$v":1,"indexes":[],"parser_policies":{"wasm":{"max_fuel":10000001}}}"#;
  assert!(matches!(compile(invalid, "/", ALGORITHMS[0]), Err(SemanticCompilationErrorV1::InvalidSource { .. })));
}

#[test]
fn configuration_admission_is_bounded_cancellable_revocable_and_releases_transient_children() {
  let memory = memory();
  let registry = registry(&memory, ALGORITHMS[0]);
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let source = default_index_configuration_v1();
  let snapshot = Snapshot::default();
  for source_limit in [true, false] {
    let mut input = request(source, "/", &registry, ALGORITHMS[0]);
    if source_limit {
      input.maximum_source_bytes = 1;
    } else {
      input.maximum_workspace_bytes = 1;
    }
    assert!(matches!(
      compile_index_configuration_v1(input, &snapshot, &memory, &|| false),
      Err(SemanticCompilationErrorV1::Resource { .. })
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  let calls = Cell::new(0usize);
  let compiled = compile_index_configuration_v1(request(source, "/", &registry, ALGORITHMS[0]), &snapshot, &memory, &|| {
    calls.set(calls.get() + 1);
    false
  })
  .unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes - baseline < 64 << 20, "temporary child workspaces were retained");
  drop(compiled);
  let total_calls = calls.get();
  for cancel_at in [1, 2, 5, total_calls / 2, total_calls] {
    calls.set(0);
    assert!(matches!(
      compile_index_configuration_v1(request(source, "/", &registry, ALGORITHMS[0]), &snapshot, &memory, &|| {
        calls.set(calls.get() + 1);
        calls.get() == cancel_at
      }),
      Err(SemanticCompilationErrorV1::Cancelled)
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  calls.set(0);
  let result = compile_index_configuration_v1(request(source, "/", &registry, ALGORITHMS[0]), &snapshot, &memory, &|| {
    calls.set(calls.get() + 1);
    if calls.get() == 2 {
      memory.reconfigure_policy(MemoryPolicy::new(1 << 20, 2 << 20, 1 << 20, 1 << 19).unwrap()).unwrap();
    }
    false
  });
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
}

#[test]
fn corrected_nested_default_sources_extract_real_native_values_where_old_literal_paths_miss() {
  use aeordb::engine::FileRecord;
  use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
  use aeordb::engine::v4::index_source::{SourceDocumentV1, SourceExtractionV1, ValueStoreRuntimeV1};
  let mut wav = b"RIFF".to_vec();
  wav.extend_from_slice(&44u32.to_le_bytes());
  wav.extend_from_slice(b"WAVEfmt ");
  wav.extend_from_slice(&16u32.to_le_bytes());
  wav.extend_from_slice(&1u16.to_le_bytes());
  wav.extend_from_slice(&1u16.to_le_bytes());
  wav.extend_from_slice(&8u32.to_le_bytes());
  wav.extend_from_slice(&8u32.to_le_bytes());
  wav.extend_from_slice(&1u16.to_le_bytes());
  wav.extend_from_slice(&8u16.to_le_bytes());
  wav.extend_from_slice(b"data");
  wav.extend_from_slice(&8u32.to_le_bytes());
  wav.extend_from_slice(&[0; 8]);
  let parsed =
    aeordb::engine::native_parsers::parse_native(&wav, "audio/wav", "example.wav", "/example.wav", wav.len() as u64).unwrap().unwrap();
  assert!(parsed["metadata"]["duration_seconds"].as_f64().unwrap() > 0.0);
  assert!(parsed["metadata"]["format"].as_str().is_some());
  let parsed: CanonicalConfigValueV1 = serde_json::from_slice(&serde_json::to_vec(&parsed).unwrap()).unwrap();
  let record = FileRecord::new("/example.wav".into(), Some("audio/wav".into()), wav.len() as u64, Vec::new());
  let document = SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed) };
  let compiled = compile(default_index_configuration_v1(), "/", ALGORITHMS[0]).unwrap();
  for (field_name, converter) in [("metadata.format", "utf8_binary_order_v1"), ("metadata.duration", "f64_finite_order_v1")] {
    let field = compiled.fields().iter().find(|field| field.field_name() == field_name).unwrap();
    let runtime = ValueStoreRuntimeV1::from_encoded(&field.value_store().value, ALGORITHMS[0]).unwrap();
    assert!(matches!(runtime.extract(document,None,&||false).unwrap(),SourceExtractionV1::Values(values) if values.len()==1));
    let old_literal = serde_json::to_vec(&json!({"$v":1,"indexes":[{"name":field_name,"type":converter}]})).unwrap();
    let old_literal = compile(&old_literal, "/", ALGORITHMS[0]).unwrap();
    let runtime = ValueStoreRuntimeV1::from_encoded(&old_literal.fields()[0].value_store().value, ALGORITHMS[0]).unwrap();
    assert_eq!(runtime.extract(document, None, &|| false).unwrap(), SourceExtractionV1::Missing);
  }
}

#[test]
fn strict_nested_objects_aliases_scope_and_source_forms_reject_malformed_input() {
  for fragment in [
    r#""parser":null"#,
    r#""parser":"""#,
    r#""parser":"bad\u0000alias""#,
    r#""compression":false"#,
    r#""parser_policies":null"#,
    r#""parser_policies":{"unknown":{}}"#,
    r#""parser_policies":{"wasm":null}"#,
    r#""parser_policies":{"raw_json":{"kind":1}}"#,
    r#""parser_policies":{"wasm":{"max_fuel":1,"max_fuel":2}}"#,
    r#""glob":"""#,
    r#""glob":"///""#,
    r#""glob":"a/../b""#,
    r#""glob":"a/./b""#,
    r#""glob":"a\u0000b""#,
  ] {
    let source = format!("{{\"$v\":1,\"indexes\":[],{fragment}}}");
    assert!(matches!(compile(source.as_bytes(), "/", ALGORITHMS[0]), Err(SemanticCompilationErrorV1::InvalidSource { .. })), "{source}");
  }
  for properties in [
    json!({"name":"","type":"typed_exact_blake3_v1"}),
    json!({"name":"@unknown","type":"typed_exact_blake3_v1"}),
    json!({"name":"x","type":[null]}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source":null}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source":[-1]}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source":[1.0]}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source":{"args":1}}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"","args":1}}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m","unknown":1}}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m","policy":null}}),
    json!({"name":"x","type":"typed_exact_blake3_v1","source_limits":null}),
    json!({"name":"x","type":"typed_exact_blake3_v1","converter_limits":{"wrong":1}}),
    json!({"name":"x","type":"typed_exact_blake3_v1","field_limits":{"max_terms_per_document":0}}),
  ] {
    let source = serde_json::to_vec(&json!({"$v":1,"indexes":[properties]})).unwrap();
    assert!(
      matches!(compile(&source, "/", ALGORITHMS[0]), Err(SemanticCompilationErrorV1::InvalidSource { .. })),
      "{}",
      String::from_utf8_lossy(&source)
    );
  }
}

struct VariantSnapshot {
  fingerprint: u8,
  profile: u16,
}

impl ParserAliasSnapshotV1 for VariantSnapshot {
  fn resolve_parser_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    let mut record = dependency(1);
    record.fingerprint = [self.fingerprint; 32];
    record.executor_profile = self.profile;
    Ok(Some(record))
  }
}

impl IndexConfigurationAliasSnapshotV1 for VariantSnapshot {
  fn resolve_mapper_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    let mut record = dependency(2);
    record.fingerprint = [self.fingerprint; 32];
    record.executor_profile = self.profile;
    Ok(Some(record))
  }
}

#[test]
fn captured_registry_pins_change_the_projection_without_any_later_alias_resolution() {
  let source = br#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]}"#;
  let registry_source = br#"{"$v":1,"parsers":{"application/x-example":"p"}}"#;
  for algorithm in ALGORITHMS {
    let memory = memory();
    let mut results = Vec::new();
    for fingerprint in [0x42, 0x43] {
      let registry = compile_parser_registry_v1(
        ParserRegistryCompilationRequestV1 {
          source: Some(registry_source),
          hash_algorithm: algorithm,
          maximum_source_bytes: 1 << 20,
          maximum_workspace_bytes: WORKSPACE,
        },
        &VariantSnapshot { fingerprint, profile: 2 },
        &memory,
        &|| false,
      )
      .unwrap();
      let snapshot = Snapshot { fail: true, ..Snapshot::default() };
      let compiled = compile_index_configuration_v1(request(source, "/", &registry, algorithm), &snapshot, &memory, &|| false).unwrap();
      assert!(snapshot.calls.borrow().is_empty());
      assert_eq!(compiled.dependencies().len(), 5);
      results.push(compiled.projection().semantic_id.clone());
    }
    assert_ne!(results[0], results[1]);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn unknown_executor_profiles_remain_unavailable_and_changed_artifact_pins_change_semantics() {
  let memory = memory();
  let registry = registry(&memory, ALGORITHMS[0]);
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  for source in [
    br#"{"$v":1,"parser":"p","indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]}"#.as_slice(),
    br#"{"$v":1,"indexes":[{"name":"x","type":"typed_exact_blake3_v1","source":{"plugin":"m"}}]}"#,
  ] {
    let mut results = Vec::new();
    for fingerprint in [0x42, 0x43] {
      let compiled = compile_index_configuration_v1(
        request(source, "/", &registry, ALGORITHMS[0]),
        &VariantSnapshot { fingerprint, profile: 2 },
        &memory,
        &|| false,
      )
      .unwrap();
      results.push(compiled.projection().semantic_id.clone());
    }
    assert_ne!(results[0], results[1]);
    let result = compile_index_configuration_v1(
      request(source, "/", &registry, ALGORITHMS[0]),
      &VariantSnapshot { fingerprint: 0x42, profile: 99 },
      &memory,
      &|| false,
    );
    assert!(matches!(result, Err(SemanticCompilationErrorV1::DependencyUnavailable { .. })));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
}

#[test]
fn fieldless_nearer_configuration_masks_defaults_using_its_actual_compiled_scope() {
  use aeordb::engine::v4::scope::{EffectiveScopeCandidateV1, EffectiveScopeResolverV1};
  for algorithm in ALGORITHMS {
    let root = compile(default_index_configuration_v1(), "/", algorithm).unwrap();
    let child = compile(br#"{"$v":1,"indexes":[]}"#, "/data", algorithm).unwrap();
    let candidates = [&root, &child].map(|configuration| EffectiveScopeCandidateV1 {
      scope_id: &configuration.scope().scope_id,
      encoded_definition: &configuration.scope().value,
    });
    let resolver = EffectiveScopeResolverV1::from_encoded(algorithm, &candidates).unwrap();
    assert_eq!(resolver.resolve("/data/file").unwrap(), Some(1));
    assert_eq!(resolver.resolve("/data/nested/file").unwrap(), Some(0));
    assert!(child.fields().is_empty());
    let recursive = compile(br#"{"$v":1,"indexes":[],"glob":"**/*"}"#, "/data", algorithm).unwrap();
    assert_ne!(child.projection(), recursive.projection());
    let candidates = [&root, &recursive].map(|configuration| EffectiveScopeCandidateV1 {
      scope_id: &configuration.scope().scope_id,
      encoded_definition: &configuration.scope().value,
    });
    let resolver = EffectiveScopeResolverV1::from_encoded(algorithm, &candidates).unwrap();
    assert_eq!(resolver.resolve("/data/nested/file").unwrap(), Some(1));
    assert_eq!(resolver.resolve("/elsewhere/file").unwrap(), Some(0));
  }
}

#[test]
fn whole_workspace_limit_covers_simultaneous_child_compiler_reservations() {
  let memory = memory();
  let registry = registry(&memory, ALGORITHMS[0]);
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let source = br#"{"$v":1,"indexes":[{"name":"@hash","type":"typed_exact_blake3_v1"}]}"#;
  let snapshot = Snapshot::default();
  for budget in [16usize << 20, 24 << 20] {
    let mut input = request(source, "/", &registry, ALGORITHMS[0]);
    input.maximum_workspace_bytes = budget;
    let peak = Cell::new(0);
    let result = compile_index_configuration_v1(input, &snapshot, &memory, &|| {
      peak.set(peak.get().max(memory.snapshot().unwrap().reserved_bytes - baseline));
      false
    });
    if budget == 16 << 20 {
      assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    } else {
      assert!(result.is_ok());
    }
    assert!(peak.get() <= budget as u64, "peak={} budget={budget}", peak.get());
    drop(result);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
}
