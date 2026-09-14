//! Corrected source normalization, separate from typed selector byte encoding.
use std::cell::Cell;

use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::InvocationPolicyV1;
use aeordb::engine::v4::parser_plan::decode_parser_resolution_plan;
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use aeordb::engine::v4::source_selector::{JsonPathSegmentV1, decode_source_selector};
use aeordb::engine::v4::source_selector_compiler::{
  CompiledSourceSelectorV1, SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1,
};
use serde_json::{Value, json};

const WORKSPACE: usize = 64 << 20;

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap())
}

fn request<'a>(name: &'a str, source: SourceSelectorInputV1<'a>) -> SourceSelectorCompilationRequestV1<'a> {
  SourceSelectorCompilationRequestV1 { field_name: name, source, maximum_source_bytes: 1 << 20, maximum_workspace_bytes: WORKSPACE }
}

fn compile(name: &str, source: SourceSelectorInputV1<'_>) -> Result<CompiledSourceSelectorV1, SemanticCompilationErrorV1> {
  compile_source_selector_v1(request(name, source), &memory(), &|| false)
}

fn path(segments: &[(u8, u8, &[u8])]) -> Vec<u8> {
  let mut bytes = vec![0; 32];
  bytes[..2].copy_from_slice(&1u16.to_le_bytes());
  bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
  bytes[12..16].copy_from_slice(&(segments.len() as u32).to_le_bytes());
  bytes[16..18].copy_from_slice(&1u16.to_le_bytes());
  for (kind, flags, payload) in segments {
    bytes.extend_from_slice(&[*kind, *flags, 0, 0]);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
  }
  let length = bytes.len() as u32;
  bytes[4..8].copy_from_slice(&length.to_le_bytes());
  bytes
}

fn policy() -> InvocationPolicyV1 {
  let bytes = std::fs::read(format!(
    "{}/spec/fixtures/v4/parser-resolution-plan-v1/aprp-blake3-256-explicit-plugin-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap();
  let mut policy = decode_parser_resolution_plan(&bytes).unwrap().candidates[0].policy.clone();
  policy.max_fuel = 10_000_000;
  policy
}

#[test]
fn all_metadata_names_and_the_single_alias_compile_to_independent_canonical_bytes() {
  for (index, name) in
    ["@path", "@filename", "@extension", "@content_type", "@size", "@created_at", "@updated_at", "@hash"].into_iter().enumerate()
  {
    let compiled = compile(name, SourceSelectorInputV1::Metadata).unwrap();
    let mut expected = vec![0; 40];
    expected[..4].copy_from_slice(&[1, 0, 1, 0]);
    expected[4..8].copy_from_slice(&40u32.to_le_bytes());
    expected[32..34].copy_from_slice(&((index + 1) as u16).to_le_bytes());
    assert_eq!(compiled.selector(), expected);
    assert_eq!(compiled.field_name(), name);
  }
  let alias = compile("@file_name", SourceSelectorInputV1::Metadata).unwrap();
  let canonical = compile("@filename", SourceSelectorInputV1::Metadata).unwrap();
  assert_eq!(alias.field_name(), "@filename");
  assert_eq!(alias.selector(), canonical.selector());
}

#[test]
fn omitted_and_explicit_same_field_sources_use_one_normalizer_including_regex_looking_names() {
  for (name, kind, flags, payload) in [
    ("title", 1, 0, "title"),
    ("a.b", 1, 0, "a.b"),
    ("é", 1, 0, "é"),
    ("/^Name$/igg", 4, 1, "^Name$"),
    ("/[broken/", 1, 0, "/[broken/"),
    ("//", 4, 0, ""),
  ] {
    let explicit = [Value::String(name.into())];
    let omitted = compile(name, SourceSelectorInputV1::JsonPath(None)).unwrap();
    let supplied = compile(name, SourceSelectorInputV1::JsonPath(Some(&explicit))).unwrap();
    assert_eq!(omitted.field_name(), name);
    assert_eq!(omitted.selector(), supplied.selector());
    assert_eq!(omitted.selector(), path(&[(kind, flags, payload.as_bytes())]));
  }
}

#[test]
fn json_segments_preserve_full_unsigned_precision_order_and_empty_path_root_selection() {
  let source = [json!("items"), json!(u64::MAX), json!(""), json!("/^key$/i")];
  let compiled = compile("value", SourceSelectorInputV1::JsonPath(Some(&source))).unwrap();
  assert_eq!(compiled.selector(), path(&[(1, 0, b"items"), (2, 0, &u64::MAX.to_le_bytes()), (3, 0, b""), (4, 1, b"^key$")]));
  assert_eq!(compile("value", SourceSelectorInputV1::JsonPath(Some(&[]))).unwrap().selector(), path(&[]));
  assert_ne!(compile("value", SourceSelectorInputV1::JsonPath(None)).unwrap().selector(), path(&[]));
}

#[test]
fn regex_delimiters_ignored_flags_and_syntax_fallback_are_normalized_without_losing_literal_bytes() {
  for (text, kind, flags, payload) in [
    ("/a/b/imx", 4, 1, "a/b"),
    ("/x/g", 4, 0, "x"),
    ("/x/ii", 4, 1, "x"),
    ("/", 1, 0, "/"),
    ("/(?=a)/", 1, 0, "/(?=a)/"),
    ("/[/i", 1, 0, "/[/i"),
    ("a/b", 1, 0, "a/b"),
    ("\\u0000", 1, 0, "\\u0000"),
    ("\0", 1, 0, "\0"),
  ] {
    let source = [json!(text)];
    let compiled = compile("value", SourceSelectorInputV1::JsonPath(Some(&source))).unwrap();
    assert_eq!(compiled.selector(), path(&[(kind, flags, payload.as_bytes())]));
  }
}

#[test]
fn corrected_sources_reject_all_legacy_always_missing_segment_types() {
  for invalid in [json!(-1), json!(1.5), json!(1.0), json!(true), Value::Null, json!([]), json!({})] {
    let source = [json!("prefix"), invalid, json!("suffix")];
    assert!(matches!(
      compile("value", SourceSelectorInputV1::JsonPath(Some(&source))),
      Err(SemanticCompilationErrorV1::InvalidSource { .. })
    ));
  }
}

#[test]
fn field_names_are_exact_utf8_with_one_metadata_ingress_and_strict_kind_matching() {
  for name in ["", "bad\0field", "@unknown", "@File_Name"] {
    assert!(compile(name, SourceSelectorInputV1::JsonPath(None)).is_err());
    assert!(compile(name, SourceSelectorInputV1::Metadata).is_err());
  }
  assert!(compile("ordinary", SourceSelectorInputV1::Metadata).is_err());
  assert!(compile("@path", SourceSelectorInputV1::JsonPath(None)).is_err());
  let name = "é".repeat(2048);
  assert_eq!(compile(&name, SourceSelectorInputV1::JsonPath(None)).unwrap().field_name(), name);
  assert!(compile(&(name + "a"), SourceSelectorInputV1::JsonPath(None)).is_err());
  assert_ne!(
    compile("é", SourceSelectorInputV1::JsonPath(None)).unwrap().selector(),
    compile("e\u{301}", SourceSelectorInputV1::JsonPath(None)).unwrap().selector()
  );
}

#[test]
fn path_count_and_complete_byte_limits_are_independently_enforced() {
  let exact = vec![json!(""); 1024];
  assert!(compile("value", SourceSelectorInputV1::JsonPath(Some(&exact))).is_ok());
  assert!(compile("value", SourceSelectorInputV1::JsonPath(Some(&vec![json!(""); 1025]))).is_err());
  let exact = [json!("x".repeat(65_536 - 40))];
  assert_eq!(compile("value", SourceSelectorInputV1::JsonPath(Some(&exact))).unwrap().selector().len(), 65_536);
  let excess = [json!("x".repeat(65_536 - 39))];
  assert!(compile("value", SourceSelectorInputV1::JsonPath(Some(&excess))).is_err());
}

#[test]
fn compiled_regex_budget_failure_is_not_reinterpreted_as_a_literal_key() {
  // Valid regex syntax whose counted expansion cannot fit the frozen1MiB NFA.
  let source = [json!("/(?:a{1000}){1000}/")];
  assert!(matches!(compile("value", SourceSelectorInputV1::JsonPath(Some(&source))), Err(SemanticCompilationErrorV1::Resource { .. })));
}

#[test]
fn mapper_omitted_and_explicit_null_arguments_have_identical_selector_bytes() {
  let policy = policy();
  let input = |arguments| SourceSelectorInputV1::Mapper { dependency_ordinal: 7, arguments, policy: &policy };
  let omitted = compile("value", input(None)).unwrap();
  let explicit = compile("value", input(Some(b" null \n"))).unwrap();
  assert_eq!(omitted.selector(), explicit.selector());
  let bytes = omitted.selector();
  assert_eq!(&bytes[2..4], &3u16.to_le_bytes());
  assert_eq!(&bytes[18..20], &2u16.to_le_bytes());
  assert_eq!(&bytes[32..36], &7u32.to_le_bytes());
  assert_eq!(&bytes[36..40], &5u32.to_le_bytes());
  assert_eq!(&bytes[48..53], &[1, 0, 0, 0, 0]);
}

#[test]
fn mapper_argument_map_order_is_irrelevant_but_types_values_and_policy_remain_semantic() {
  let policy = policy();
  let compile_arguments = |arguments| {
    compile("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: Some(arguments), policy: &policy }).unwrap()
  };
  assert_eq!(compile_arguments(br#"{"b":2,"a":1}"#).selector(), compile_arguments(br#"{ "a": 1, "b": 2 }"#).selector());
  assert_ne!(compile_arguments(b"null").selector(), compile_arguments(br#""""#).selector());
  assert_ne!(compile_arguments(b"[1,2]").selector(), compile_arguments(b"[2,1]").selector());
}

#[test]
fn mapper_rejects_invalid_json_duplicate_members_legacy_policy_zero_ordinal_and_metadata_name() {
  let policy = policy();
  for arguments in [b"".as_slice(), b"[", b"{} {}", br#"{"x":1,"x":2}"#, &[255]] {
    assert!(compile("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: Some(arguments), policy: &policy }).is_err());
  }
  for (name, ordinal) in [("@path", 1), ("value", 0)] {
    assert!(compile(name, SourceSelectorInputV1::Mapper { dependency_ordinal: ordinal, arguments: None, policy: &policy }).is_err());
  }
  let mut invalid = policy.clone();
  invalid.max_fuel += 1;
  assert!(compile("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: None, policy: &invalid }).is_err());
  invalid.kind = aeordb::engine::v4::dependency::InvocationPolicyKind::LegacyWasm;
  assert!(compile("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: None, policy: &invalid }).is_err());
}

#[test]
fn source_compilation_respects_admission_cancellation_and_retained_memory_ownership() {
  let memory = memory();
  let input = request("value", SourceSelectorInputV1::JsonPath(None));
  assert!(matches!(compile_source_selector_v1(input.clone(), &memory, &|| true), Err(SemanticCompilationErrorV1::Cancelled)));
  assert!(matches!(
    compile_source_selector_v1(input.clone(), &MemoryCoordinator::without_policy(), &|| false),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  assert!(matches!(
    compile_source_selector_v1(SourceSelectorCompilationRequestV1 { maximum_workspace_bytes: 1, ..input.clone() }, &memory, &|| false),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  assert!(matches!(
    compile_source_selector_v1(SourceSelectorCompilationRequestV1 { maximum_source_bytes: 1, ..input.clone() }, &memory, &|| false),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  let checks = Cell::new(0);
  let cancel = || {
    checks.set(checks.get() + 1);
    checks.get() > 1
  };
  assert!(matches!(compile_source_selector_v1(input.clone(), &memory, &cancel), Err(SemanticCompilationErrorV1::Cancelled)));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let compiled = compile_source_selector_v1(input, &memory, &|| false).unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes > 0);
  assert_eq!(decode_source_selector(compiled.selector()).unwrap().segments, [JsonPathSegmentV1::ObjectKey("value")]);
  drop(compiled);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn mapper_complete_selector_cap_is_enforced_after_argument_canonicalization() {
  let policy = policy();
  // 48-byte prefix + 128-byte invocation + 5-byte canonical string frame.
  let exact = serde_json::to_vec(&"x".repeat(65_536 - 48 - 128 - 5)).unwrap();
  let excess = serde_json::to_vec(&"x".repeat(65_536 - 48 - 128 - 4)).unwrap();
  let input = |arguments| SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: Some(arguments), policy: &policy };
  assert_eq!(compile("value", input(&exact)).unwrap().selector().len(), 65_536);
  assert!(compile("value", input(&excess)).is_err());
}

#[test]
fn mapper_call_site_policy_and_dependency_ordinal_are_part_of_the_selector_bytes() {
  let original = policy();
  let mut changed = original.clone();
  changed.max_fuel -= 1;
  let first = compile("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: None, policy: &original }).unwrap();
  let second = compile("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: None, policy: &changed }).unwrap();
  let third = compile("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 2, arguments: None, policy: &original }).unwrap();
  assert_ne!(first.selector(), second.selector());
  assert_ne!(first.selector(), third.selector());
}

#[test]
fn source_admission_limits_raw_ignored_flags_before_regex_compilation() {
  let source = [json!(format!("/x/{}", "g".repeat(4096)))];
  let input = request("value", SourceSelectorInputV1::JsonPath(Some(&source)));
  let small = SourceSelectorCompilationRequestV1 { maximum_source_bytes: 4096, ..input.clone() };
  assert!(matches!(compile_source_selector_v1(small, &memory(), &|| false), Err(SemanticCompilationErrorV1::Resource { .. })));
  let admitted = compile_source_selector_v1(input, &memory(), &|| false).unwrap();
  assert_eq!(admitted.selector(), path(&[(4, 0, b"x")]));
}

#[test]
fn source_compilation_rechecks_memory_policy_before_returning_a_result() {
  let memory = memory();
  let calls = Cell::new(0);
  let check = || {
    calls.set(calls.get() + 1);
    if calls.get() == 2 {
      memory.reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
    }
    false
  };
  let input = request("@path", SourceSelectorInputV1::Metadata);
  assert!(matches!(compile_source_selector_v1(input, &memory, &check), Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn mapper_dense_arguments_are_charged_before_parsing_and_release_their_reservation() {
  let policy = policy();
  let arguments = format!("[{}]", vec!["null"; 4096].join(","));
  let input =
    request("value", SourceSelectorInputV1::Mapper { dependency_ordinal: 1, arguments: Some(arguments.as_bytes()), policy: &policy });
  let memory = memory();
  assert!(matches!(
    compile_source_selector_v1(SourceSelectorCompilationRequestV1 { maximum_workspace_bytes: 8 << 20, ..input.clone() }, &memory, &|| {
      false
    }),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let compiled = compile_source_selector_v1(input, &memory, &|| false).unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes > arguments.len() as u64 * 768);
  drop(compiled);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn compiled_selectors_close_and_execute_real_value_stores_for_every_database_hash() {
  use aeordb::engine::HashAlgorithm;
  use aeordb::engine::file_record::FileRecord;
  use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value};
  use aeordb::engine::v4::dependency::decode_dependency_record_bytes;
  use aeordb::engine::v4::index_source::{SourceDocumentV1, SourceExtractionV1, ValueStoreRuntimeV1};
  use aeordb::engine::v4::parser_context_compiler::{
    ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
  };
  use aeordb::engine::v4::scope::{ScopeDefinitionWriteV1, ScopeMatchingMode, encode_scope_definition};
  use aeordb::engine::v4::value_store::{ValueStoreDefinitionWriteV1, ValueStoreSemanticFamily, encode_value_store_definition};
  let bytes = std::fs::read(format!(
    "{}/spec/fixtures/v4/semantic-object-v1/asem-blake3-256-wasm-parser-definition-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap();
  let dependency = decode_dependency_record_bytes(&bytes[80..bytes.len() - 4]).unwrap();
  let policy = policy();
  let memory = memory();
  let paths = [json!("items"), json!(""), json!("/^name$/igg")];
  let document: CanonicalConfigValueV1 = serde_json::from_str(r#"{"items":[{"Name":null,"name":"first"},{"name":"second"}]}"#).unwrap();
  let record = FileRecord::new("/example.json".into(), Some("application/json".into()), 100, Vec::new());
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let scope =
      encode_scope_definition(ScopeDefinitionWriteV1 { mode: ScopeMatchingMode::DirectChildren, owner_path: "/", glob: None }, algorithm)
        .unwrap();
    for metadata in [true, false] {
      let (name, selector_source, parser_source, selector_dependency) = if metadata {
        ("@file_name", SourceSelectorInputV1::Metadata, ParserContextSourceV1::Metadata, ParserSelectorDependencyV1::None)
      } else {
        (
          "value",
          SourceSelectorInputV1::JsonPath(Some(&paths)),
          ParserContextSourceV1::Explicit { dependency: dependency.clone(), policy: &policy },
          ParserSelectorDependencyV1::JsonPath,
        )
      };
      let selector = compile_source_selector_v1(request(name, selector_source), &memory, &|| false).unwrap();
      let context = compile_parser_context_v1(
        ParserContextCompilationRequestV1 { source: parser_source, selector_dependency, maximum_workspace_bytes: 4 << 20 },
        &memory,
        &|| false,
      )
      .unwrap();
      let definition = encode_value_store_definition(
        ValueStoreDefinitionWriteV1 {
          scope_id: &scope.scope_id,
          field_name: selector.field_name(),
          semantic_family: ValueStoreSemanticFamily::CorrectedV1,
          max_source_values_per_document: 1024,
          max_canonical_source_bytes_per_document: 8 << 20,
          max_document_input_bytes: if metadata { 0 } else { 64 << 20 },
          max_selector_work_items_per_document: if metadata { 0 } else { 1_000_000 },
          max_selector_examined_bytes_per_document: if metadata { 0 } else { 64 << 20 },
          selector: selector.selector(),
          parser_plan: context.parser_plan(),
          dependencies: context.dependencies(),
        },
        algorithm,
      )
      .unwrap();
      assert_eq!(definition.value_store_id.len(), algorithm.hash_length());
      let runtime = ValueStoreRuntimeV1::from_encoded(&definition.value, algorithm).unwrap();
      let input = SourceDocumentV1 { file_record: &record, parsed_value: Some(&document) };
      assert!(runtime.extract(input, None, &|| true).is_err());
      let SourceExtractionV1::Values(values) = runtime.extract(input, None, &|| false).unwrap() else {
        panic!("expected values")
      };
      let decoded: Vec<_> = values.iter().map(|value| decode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE).unwrap()).collect();
      let expected = if metadata {
        vec![CanonicalConfigValueV1::String("example.json".into())]
      } else {
        vec![CanonicalConfigValueV1::Null, CanonicalConfigValueV1::String("first".into()), CanonicalConfigValueV1::String("second".into())]
      };
      assert_eq!(decoded, expected);
      if !metadata {
        assert!(runtime.extract(SourceDocumentV1 { parsed_value: None, ..input }, None, &|| false).is_err());
      }
    }
  }
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn regex_syntax_workspace_is_admitted_separately_from_compiled_program_bytes() {
  let source = [json!(format!("/{}/g", "a?".repeat(1000)))];
  let input = request("value", SourceSelectorInputV1::JsonPath(Some(&source)));
  let memory = memory();
  assert!(matches!(
    compile_source_selector_v1(SourceSelectorCompilationRequestV1 { maximum_workspace_bytes: 9 << 20, ..input.clone() }, &memory, &|| {
      false
    }),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let compiled = compile_source_selector_v1(input, &memory, &|| false).unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes >= (8 << 20) + 2000 * 768);
  drop(compiled);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}
