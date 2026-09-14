//! Compiler-owned program/table ordinals, independently of the byte writers.
use std::cell::Cell;

use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::{DependencyRecordV1, InvocationPolicyKind, InvocationPolicyV1, decode_dependency_table};
use aeordb::engine::v4::native_semantics::NativeSemanticComponentV1;
use aeordb::engine::v4::parser_context_compiler::{
  CompiledParserContextV1, ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
};
use aeordb::engine::v4::parser_plan::decode_parser_resolution_plan;
use aeordb::engine::v4::parser_registry_compiler::{
  CompiledParserRegistryV1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1,
  compile_parser_registry_v1,
};
use aeordb::engine::HashAlgorithm;

const WORKSPACE: usize = 16 * 1024 * 1024;

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap())
}

fn policy(wasm: bool) -> InvocationPolicyV1 {
  InvocationPolicyV1 {
    kind: if wasm { InvocationPolicyKind::PureWasm } else { InvocationPolicyKind::Native },
    max_request_bytes: if wasm { 64 * 1024 * 1024 } else { 0 },
    max_response_bytes: 16 * 1024 * 1024,
    max_linear_memory_bytes: if wasm { 64 * 1024 * 1024 } else { 0 },
    max_fuel: if wasm { 10_000_000 } else { 0 },
    max_table_elements: if wasm { 100_000 } else { 0 },
    max_structure_nodes: 100_000,
    max_scalar_bytes: 65_536,
    max_structure_depth: 32,
    max_container_members: 65_535,
    max_wasm_instances: u32::from(wasm),
    max_wasm_memories: u32::from(wasm),
    max_wasm_tables: u32::from(wasm),
    max_value_stack_height: 4096,
    max_recursion_depth: 256,
  }
}

fn dependency(role: u16, fingerprint: u8) -> DependencyRecordV1<'static> {
  DependencyRecordV1 {
    kind: 1,
    role,
    flags: 4,
    abi: role + 2,
    executor_profile: 2,
    fingerprint_semantics: 1,
    artifact_kind: 1,
    artifact_length: 123,
    fingerprint: [fingerprint; 32],
    dependency_id: "/org/example/shared",
    version: "1.2.3",
  }
}

struct Snapshot;
impl ParserAliasSnapshotV1 for Snapshot {
  fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    Ok(Some(dependency(1, if alias == "second" { 2 } else { 1 })))
  }
}

fn registry(source: Option<&[u8]>) -> CompiledParserRegistryV1 {
  compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source,
      hash_algorithm: HashAlgorithm::Blake3_256,
      maximum_source_bytes: WORKSPACE,
      maximum_workspace_bytes: WORKSPACE,
    },
    &Snapshot,
    &memory(),
    &|| false,
  )
  .unwrap()
}

fn request<'a>(
  source: ParserContextSourceV1<'a>,
  selector_dependency: ParserSelectorDependencyV1<'a>,
) -> ParserContextCompilationRequestV1<'a> {
  ParserContextCompilationRequestV1 { source, selector_dependency, maximum_workspace_bytes: WORKSPACE }
}

fn compile(request: ParserContextCompilationRequestV1<'_>) -> Result<CompiledParserContextV1, SemanticCompilationErrorV1> {
  compile_parser_context_v1(request, &memory(), &|| false)
}

fn automatic<'a>(
  registry: &'a CompiledParserRegistryV1,
  wasm: &'a InvocationPolicyV1,
  native: &'a InvocationPolicyV1,
) -> ParserContextSourceV1<'a> {
  ParserContextSourceV1::Automatic {
    registry,
    registry_policy: (!registry.entries().is_empty()).then_some(wasm),
    raw_json_policy: native,
    native_suite_policy: native,
  }
}

fn policy_bytes(policy: &InvocationPolicyV1) -> Vec<u8> {
  let mut bytes = vec![0; 128];
  bytes[..4].copy_from_slice(b"AIVP");
  let wasm = policy.kind == InvocationPolicyKind::PureWasm;
  for (offset, value) in [(4, 1u16), (6, 128), (16, if wasm { 2 } else { 1 }), (18, u16::from(wasm)), (20, 1), (22, 1)] {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
  }
  bytes[8..12].copy_from_slice(&128u32.to_le_bytes());
  for (index, value) in [
    policy.max_request_bytes,
    policy.max_response_bytes,
    policy.max_linear_memory_bytes,
    policy.max_fuel,
    policy.max_table_elements,
    policy.max_structure_nodes,
    policy.max_scalar_bytes,
  ]
  .into_iter()
  .enumerate()
  {
    bytes[24 + 8 * index..32 + 8 * index].copy_from_slice(&value.to_le_bytes());
  }
  for (index, value) in [
    policy.max_structure_depth,
    policy.max_container_members,
    policy.max_wasm_instances,
    policy.max_wasm_memories,
    policy.max_wasm_tables,
    policy.max_value_stack_height,
    policy.max_recursion_depth,
  ]
  .into_iter()
  .enumerate()
  {
    bytes[80 + 4 * index..84 + 4 * index].copy_from_slice(&value.to_le_bytes());
  }
  bytes
}

fn program(kind: u16, mime_ordinal: u32, candidates: &[(u16, u32, &[u8], &InvocationPolicyV1)]) -> Vec<u8> {
  let mut bytes = vec![0; 48];
  bytes[..4].copy_from_slice(b"APRP");
  for (offset, value) in
    [(4, 1u16), (6, 48), (16, kind), (18, u16::from(kind != 1)), (20, u16::from(kind == 3)), (22, u16::from(kind == 3))]
  {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
  }
  bytes[24..28].copy_from_slice(&(candidates.len() as u32).to_le_bytes());
  bytes[28..32].copy_from_slice(&mime_ordinal.to_le_bytes());
  for (kind, ordinal, essence, policy) in candidates {
    let mut candidate = vec![0; 32];
    candidate[..4].copy_from_slice(&((160 + essence.len()) as u32).to_le_bytes());
    candidate[4..6].copy_from_slice(&kind.to_le_bytes());
    candidate[6..8].copy_from_slice(&u16::from(*kind == 2).to_le_bytes());
    candidate[8..12].copy_from_slice(&ordinal.to_le_bytes());
    candidate[12..16].copy_from_slice(&128u32.to_le_bytes());
    candidate[16..20].copy_from_slice(&(essence.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&candidate);
    bytes.extend_from_slice(essence);
    bytes.extend_from_slice(&policy_bytes(policy));
  }
  let length = bytes.len() as u32;
  bytes[8..12].copy_from_slice(&length.to_le_bytes());
  bytes
}

#[test]
fn metadata_has_the_independent_none_program_empty_table_and_no_selector_ordinal() {
  let compiled = compile(request(ParserContextSourceV1::Metadata, ParserSelectorDependencyV1::None)).unwrap();
  assert_eq!(compiled.parser_plan(), program(1, 0, &[]));
  let expected =
    std::fs::read(format!("{}/spec/fixtures/v4/dependency-table-v1/adpt-sha512-empty-valid.bin", env!("CARGO_MANIFEST_DIR"))).unwrap();
  assert_eq!(compiled.dependencies(), expected);
  assert_eq!(compiled.selector_dependency_ordinal(), None);
}

#[test]
fn automatic_empty_registry_uses_only_the_four_exact_native_components_in_canonical_order() {
  let registry = registry(None);
  let native = policy(false);
  let wasm = policy(true);
  let compiled = compile(request(automatic(&registry, &wasm, &native), ParserSelectorDependencyV1::JsonPath)).unwrap();
  assert_eq!(compiled.parser_plan(), program(3, 3, &[(3, 2, b"", &native), (4, 1, b"", &native)]));
  assert_eq!(compiled.selector_dependency_ordinal(), Some(4));
  let table = decode_dependency_table(compiled.dependencies()).unwrap();
  assert_eq!(
    table.records,
    vec![
      NativeSemanticComponentV1::NativeSuite.dependency_record(),
      NativeSemanticComponentV1::RawJson.dependency_record(),
      NativeSemanticComponentV1::MimeRouter.dependency_record(),
      NativeSemanticComponentV1::RegexSelector.dependency_record()
    ]
  );
}

#[test]
fn registry_alias_equivalence_deduplicates_exact_records_before_assigning_all_program_ordinals() {
  let native = policy(false);
  let wasm = policy(true);
  let first = registry(Some(br#"{"$v":1,"parsers":{"text/plain":"first","application/pdf":"renamed","image/png":"second"}}"#));
  let second = registry(Some(br#"{"parsers":{"IMAGE/PNG":"second","APPLICATION/PDF":"first","TEXT/PLAIN":"renamed"},"$v":1}"#));
  let compile_registry = |registry| compile(request(automatic(registry, &wasm, &native), ParserSelectorDependencyV1::JsonPath)).unwrap();
  let left = compile_registry(&first);
  let right = compile_registry(&second);
  assert_eq!(left.parser_plan(), right.parser_plan());
  assert_eq!(left.dependencies(), right.dependencies());
  assert_eq!(
    left.parser_plan(),
    program(
      3,
      5,
      &[
        (2, 1, b"application/pdf", &wasm),
        (2, 2, b"image/png", &wasm),
        (2, 1, b"text/plain", &wasm),
        (3, 4, b"", &native),
        (4, 3, b"", &native)
      ]
    )
  );
  let table = decode_dependency_table(left.dependencies()).unwrap();
  assert_eq!(table.records.len(), 6);
  assert_eq!(&table.records[..2], &[dependency(1, 1), dependency(1, 2)]);
  assert_eq!(left.selector_dependency_ordinal(), Some(6));
}

#[test]
fn explicit_parser_and_mapper_share_one_table_without_collapsing_distinct_roles() {
  let invocation = policy(true);
  let compiled = compile(request(
    ParserContextSourceV1::Explicit { dependency: dependency(1, 1), policy: &invocation },
    ParserSelectorDependencyV1::Mapper(dependency(2, 1)),
  ))
  .unwrap();
  assert_eq!(compiled.parser_plan(), program(2, 0, &[(1, 1, b"", &invocation)]));
  assert_eq!(decode_dependency_table(compiled.dependencies()).unwrap().records, vec![dependency(1, 1), dependency(2, 1)]);
  assert_eq!(compiled.selector_dependency_ordinal(), Some(2));
}

#[test]
fn changing_call_site_policy_changes_program_but_not_executable_deduplication() {
  let original = policy(true);
  let mut smaller = original.clone();
  smaller.max_fuel -= 1;
  let compile_policy = |policy| {
    compile(request(ParserContextSourceV1::Explicit { dependency: dependency(1, 1), policy }, ParserSelectorDependencyV1::JsonPath))
      .unwrap()
  };
  let left = compile_policy(&original);
  let right = compile_policy(&smaller);
  assert_eq!(left.dependencies(), right.dependencies());
  assert_ne!(left.parser_plan(), right.parser_plan());
}

#[test]
fn metadata_and_ordinary_context_mismatches_are_rejected_instead_of_retaining_unused_dependencies() {
  let invocation = policy(true);
  for selector in [ParserSelectorDependencyV1::JsonPath, ParserSelectorDependencyV1::Mapper(dependency(2, 1))] {
    assert!(compile(request(ParserContextSourceV1::Metadata, selector)).is_err());
  }
  let source = ParserContextSourceV1::Explicit { dependency: dependency(1, 1), policy: &invocation };
  assert!(compile(request(source, ParserSelectorDependencyV1::None)).is_err());
  let registry = registry(None);
  let native = policy(false);
  assert!(compile(request(automatic(&registry, &invocation, &native), ParserSelectorDependencyV1::None)).is_err());
}

#[test]
fn corrected_compilation_rejects_structurally_retainable_legacy_or_unknown_executors_and_wrong_roles() {
  let invocation = policy(true);
  for mapper in [false, true] {
    for field in 0..9 {
      let mut invalid = dependency(if mapper { 2 } else { 1 }, 1);
      match field {
        0 => invalid.kind = 2,
        1 => invalid.role = if mapper { 1 } else { 2 },
        2 => invalid.abi = 99,
        3 => invalid.executor_profile = 99,
        4 => invalid.flags = 5,
        5 => invalid.fingerprint = [0; 32],
        6 => invalid.artifact_length = 0,
        7 => invalid.dependency_id = "/bad/../id",
        8 => invalid.version = "invalid",
        _ => unreachable!(),
      }
      let parser = if mapper { dependency(1, 1) } else { invalid.clone() };
      let selector = if mapper { ParserSelectorDependencyV1::Mapper(invalid) } else { ParserSelectorDependencyV1::JsonPath };
      assert!(
        compile(request(ParserContextSourceV1::Explicit { dependency: parser, policy: &invocation }, selector)).is_err(),
        "mapper={mapper} field={field}"
      );
    }
  }
}

#[test]
fn public_corrected_wasm_policy_maxima_are_enforced_without_weakening_structural_readers() {
  let at_limit = policy(true);
  assert!(compile(request(
    ParserContextSourceV1::Explicit { dependency: dependency(1, 1), policy: &at_limit },
    ParserSelectorDependencyV1::JsonPath
  ))
  .is_ok());
  for field in 0..9 {
    let mut invalid = at_limit.clone();
    match field {
      0 => invalid.max_request_bytes += 1,
      1 => invalid.max_response_bytes += 1,
      2 => invalid.max_linear_memory_bytes += 65536,
      3 => invalid.max_fuel += 1,
      4 => invalid.kind = InvocationPolicyKind::LegacyWasm,
      5 => invalid.max_fuel = 0,
      6 => invalid.max_structure_nodes = 0,
      7 => invalid.max_table_elements = u64::MAX,
      8 => invalid.max_linear_memory_bytes -= 1,
      _ => unreachable!(),
    }
    assert!(
      compile(request(
        ParserContextSourceV1::Explicit { dependency: dependency(1, 1), policy: &invalid },
        ParserSelectorDependencyV1::JsonPath
      ))
      .is_err(),
      "policy field {field}"
    );
  }
}

#[test]
fn operational_admission_and_cancellation_never_return_a_partially_compiled_context() {
  let source = request(ParserContextSourceV1::Metadata, ParserSelectorDependencyV1::None);
  let memory = memory();
  assert!(matches!(compile_parser_context_v1(source.clone(), &memory, &|| true), Err(SemanticCompilationErrorV1::Cancelled)));
  assert!(matches!(
    compile_parser_context_v1(source.clone(), &MemoryCoordinator::without_policy(), &|| false),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  assert!(matches!(
    compile(ParserContextCompilationRequestV1 { maximum_workspace_bytes: 1, ..source.clone() }),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  let checks = Cell::new(0);
  let cancelled = || {
    checks.set(checks.get() + 1);
    checks.get() > 1
  };
  assert!(matches!(compile_parser_context_v1(source.clone(), &memory, &cancelled), Err(SemanticCompilationErrorV1::Cancelled)));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let compiled = compile_parser_context_v1(source, &memory, &|| false).unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes > 0);
  drop(compiled);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn corrected_automatic_native_policies_and_registry_policy_presence_are_validated() {
  let empty = registry(None);
  let populated = registry(Some(br#"{"$v":1,"parsers":{"text/plain":"first"}}"#));
  let wasm = policy(true);
  let native = policy(false);
  for (registry, registry_policy, raw_json_policy, native_suite_policy) in [
    (&populated, None, &native, &native),
    (&populated, Some(&native), &native, &native),
    (&empty, None, &wasm, &native),
    (&empty, None, &native, &wasm),
  ] {
    assert!(compile(request(
      ParserContextSourceV1::Automatic { registry, registry_policy, raw_json_policy, native_suite_policy },
      ParserSelectorDependencyV1::JsonPath
    ))
    .is_err());
  }
  let mut wrong_native = native.clone();
  wrong_native.max_request_bytes = 1;
  assert!(compile(request(automatic(&empty, &wasm, &wrong_native), ParserSelectorDependencyV1::JsonPath)).is_err());
  let compiled = compile(request(automatic(&populated, &wasm, &native), ParserSelectorDependencyV1::Mapper(dependency(2, 1)))).unwrap();
  assert_eq!(compiled.selector_dependency_ordinal(), Some(2));
  assert_eq!(decode_parser_resolution_plan(compiled.parser_plan()).unwrap().mime_dependency_ordinal, 5);
}

#[test]
fn compiled_automatic_program_enforces_the_exact_128_kib_boundary() {
  // 48 + two native candidates *160 + 512*(160+95) +144 =131072.
  let mut keys: Vec<_> = (0..512).map(|index| format!("application/x-{index:03}{}", "x".repeat(78))).collect();
  assert_eq!(keys[0].len(), 95);
  keys.last_mut().unwrap().push_str(&"x".repeat(144));
  // Neither restricted MIME name may exceed127; distribute long keys across
  // type and subtype while keeping the same total length and unique suffix.
  *keys.last_mut().unwrap() = format!("{}/{}", "x".repeat(119), "y".repeat(119));
  assert_eq!(keys.last().unwrap().len(), 239);
  let source = |keys: &[String]| {
    let entries: serde_json::Map<String, serde_json::Value> =
      keys.iter().map(|key| (key.clone(), serde_json::Value::String("first".into()))).collect();
    serde_json::to_vec(&serde_json::json!({"$v":1,"parsers":entries})).unwrap()
  };
  let wasm = policy(true);
  let native = policy(false);
  let exact = registry(Some(&source(&keys)));
  let context = compile(request(automatic(&exact, &wasm, &native), ParserSelectorDependencyV1::JsonPath)).unwrap();
  assert_eq!(context.parser_plan().len(), 131072);
  assert_eq!(decode_parser_resolution_plan(context.parser_plan()).unwrap().candidates.len(), 514);
  keys.last_mut().unwrap().push('y');
  let oversized = registry(Some(&source(&keys)));
  let result = compile(request(automatic(&oversized, &wasm, &native), ParserSelectorDependencyV1::JsonPath));
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
}

fn value_store(context: &CompiledParserContextV1, algorithm: HashAlgorithm, metadata: bool) -> Vec<u8> {
  use aeordb::engine::v4::scope::{ScopeDefinitionWriteV1, ScopeMatchingMode, encode_scope_definition};
  use aeordb::engine::v4::source_selector::{JsonPathSegmentV1, SourceSelectorWriteV1, encode_source_selector};
  use aeordb::engine::v4::value_store::{ValueStoreDefinitionWriteV1, ValueStoreSemanticFamily, encode_value_store_definition};
  let scope =
    encode_scope_definition(ScopeDefinitionWriteV1 { mode: ScopeMatchingMode::DirectChildren, owner_path: "/", glob: None }, algorithm)
      .unwrap();
  let selector = if metadata {
    encode_source_selector(SourceSelectorWriteV1::Metadata { metadata_id: 8 }).unwrap()
  } else {
    encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &[JsonPathSegmentV1::ObjectKey("title")] }).unwrap()
  };
  encode_value_store_definition(
    ValueStoreDefinitionWriteV1 {
      scope_id: &scope.scope_id,
      field_name: if metadata { "@hash" } else { "title" },
      semantic_family: ValueStoreSemanticFamily::CorrectedV1,
      max_source_values_per_document: 1024,
      max_canonical_source_bytes_per_document: 8 * 1024 * 1024,
      max_document_input_bytes: if metadata { 0 } else { 64 * 1024 * 1024 },
      max_selector_work_items_per_document: if metadata { 0 } else { 1_000_000 },
      max_selector_examined_bytes_per_document: if metadata { 0 } else { 64 * 1024 * 1024 },
      selector: &selector,
      parser_plan: context.parser_plan(),
      dependencies: context.dependencies(),
    },
    algorithm,
  )
  .unwrap()
  .value
}

#[test]
fn compiled_contexts_close_real_value_store_definitions_for_every_database_hash() {
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  let empty = registry(None);
  let wasm = policy(true);
  let native = policy(false);
  let automatic = compile(request(automatic(&empty, &wasm, &native), ParserSelectorDependencyV1::JsonPath)).unwrap();
  let metadata = compile(request(ParserContextSourceV1::Metadata, ParserSelectorDependencyV1::None)).unwrap();
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for (context, is_metadata) in [(&automatic, false), (&metadata, true)] {
      let bytes = value_store(context, algorithm, is_metadata);
      let definition = decode_value_store_definition(&bytes, algorithm).unwrap();
      assert_eq!(definition.value_store_id.len(), algorithm.hash_length());
      assert_eq!(definition.dependencies.records.len(), if is_metadata { 0 } else { 4 });
    }
  }
}

#[test]
fn compiled_automatic_context_executes_the_existing_native_parser_on_a_real_captured_revision() {
  use aeordb::engine::{RequestContext, StorageEngine};
  use aeordb::engine::directory_ops::DirectoryOps;
  use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
  use aeordb::engine::v4::index_native_parser::NativeIndexParserExecutorV1;
  use aeordb::engine::v4::index_native_source::{NativeIndexFileRevisionSourceV1, NativeIndexSourceLimitsV1};
  use aeordb::engine::v4::index_producer_collector::{IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1};
  use aeordb::engine::v4::index_producer_source::IndexFileRevisionSourceV1;
  use aeordb::engine::v4::value_store::decode_value_store_definition;

  // Native execution proof through the existing source adapter. This does not
  // claim that the ordinary StorageEngine below is already a v4 service.
  let directory = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(directory.path().join("parser-context-source.aeordb").to_str().unwrap()).unwrap();
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.ensure_root_directory(&context).unwrap();
  operations.store_file_buffered(&context, "/document.json", br#"{"title":"compiled"}"#, Some("Application/JSON; charset=utf-8")).unwrap();
  let root = engine.head_hash().unwrap();
  let empty = registry(None);
  let wasm = policy(true);
  let native = policy(false);
  let compiled = compile(request(automatic(&empty, &wasm, &native), ParserSelectorDependencyV1::JsonPath)).unwrap();
  let bytes = value_store(&compiled, engine.hash_algo(), false);
  let definition = decode_value_store_definition(&bytes, engine.hash_algo()).unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, NativeIndexSourceLimitsV1::new(16 << 20, 16 << 20, 64).unwrap());
  let loaded = source.load_file_revision(&root, "/document.json").unwrap().unwrap();
  let revision = loaded.revision();
  let result = NativeIndexParserExecutorV1::new(&engine)
    .parse(IndexParserExecutionRequestV1::new(
      &root,
      &revision.revision_hash,
      &revision.file_record,
      &definition.parser_plan,
      &definition.dependencies,
      64 << 20,
      &|| false,
    ))
    .unwrap();
  let IndexParserOutcomeV1::Parsed(CanonicalConfigValueV1::Map(result)) = result else {
    panic!("expected parsed document")
  };
  assert_eq!(result.get("title"), Some(&CanonicalConfigValueV1::String("compiled".into())));
}
#[test]
fn dependency_table_cap_is_checked_after_exact_dedup_and_including_mapper_dependencies() {
  struct LongSnapshot {
    identifiers: Vec<String>,
  }
  impl ParserAliasSnapshotV1 for LongSnapshot {
    fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      let mut record = dependency(1, 1);
      record.dependency_id = &self.identifiers[alias.parse::<usize>().unwrap()];
      Ok(Some(record))
    }
  }
  let snapshot = LongSnapshot { identifiers: (0..64).map(|index| format!("/d/{index:03}/{}", "a".repeat(3961))).collect() };
  assert_eq!(snapshot.identifiers[0].len(), 3968);
  let entries: serde_json::Map<String, serde_json::Value> = (0..64)
    .map(|index| {
      let suffix = if index == 63 { "x".repeat(55) } else { String::new() };
      (format!("application/x-{index:03}{suffix}"), serde_json::Value::String(index.to_string()))
    })
    .collect();
  let bytes = serde_json::to_vec(&serde_json::json!({"$v":1,"parsers":entries})).unwrap();
  let registry = compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: Some(&bytes),
      hash_algorithm: HashAlgorithm::Blake3_256,
      maximum_source_bytes: WORKSPACE,
      maximum_workspace_bytes: WORKSPACE,
    },
    &snapshot,
    &memory(),
    &|| false,
  )
  .unwrap();
  let wasm = policy(true);
  let native = policy(false);
  assert!(compile(request(automatic(&registry, &wasm, &native), ParserSelectorDependencyV1::JsonPath)).is_ok());
  let mapper_id = format!("/{}", "m".repeat(4095));
  let mut mapper = dependency(2, 1);
  mapper.dependency_id = &mapper_id;
  let memory = memory();
  let result =
    compile_parser_context_v1(request(automatic(&registry, &wasm, &native), ParserSelectorDependencyV1::Mapper(mapper)), &memory, &|| {
      false
    });
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn context_compilation_rechecks_shared_policy_after_initial_admission() {
  let memory = memory();
  let checks = Cell::new(0);
  let check = || {
    checks.set(checks.get() + 1);
    if checks.get() == 2 {
      memory.reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
    }
    false
  };
  let result = compile_parser_context_v1(request(ParserContextSourceV1::Metadata, ParserSelectorDependencyV1::None), &memory, &check);
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn empty_automatic_registry_cannot_retain_an_unused_wasm_invocation_policy() {
  let registry = registry(None);
  let wasm = policy(true);
  let native = policy(false);
  assert!(matches!(
    compile(request(
      ParserContextSourceV1::Automatic {
        registry: &registry,
        registry_policy: Some(&wasm),
        raw_json_policy: &native,
        native_suite_policy: &native,
      },
      ParserSelectorDependencyV1::JsonPath
    )),
    Err(SemanticCompilationErrorV1::InvalidSource { .. })
  ));
}

#[test]
fn registry_records_with_the_same_artifact_but_distinct_full_identity_are_not_deduplicated() {
  struct DistinctSnapshot;
  impl ParserAliasSnapshotV1 for DistinctSnapshot {
    fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      let mut record = dependency(1, 1);
      match alias {
        "id" => record.dependency_id = "/org/example/renamed",
        "version" => record.version = "2.0.0",
        "length" => record.artifact_length += 1,
        "original" => {}
        _ => panic!("unexpected alias"),
      }
      Ok(Some(record))
    }
  }
  let source = br#"{"$v":1,"parsers":{"text/a":"original","text/b":"id","text/c":"version","text/d":"length"}}"#;
  let registry = compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: Some(source),
      hash_algorithm: HashAlgorithm::Sha512,
      maximum_source_bytes: WORKSPACE,
      maximum_workspace_bytes: WORKSPACE,
    },
    &DistinctSnapshot,
    &memory(),
    &|| false,
  )
  .unwrap();
  let wasm = policy(true);
  let native = policy(false);
  let compiled = compile(request(automatic(&registry, &wasm, &native), ParserSelectorDependencyV1::JsonPath)).unwrap();
  let table = decode_dependency_table(compiled.dependencies()).unwrap();
  assert_eq!(table.records.len(), 8);
  assert_eq!(table.records[0].dependency_id, "/org/example/renamed");
  assert_eq!(table.records[1], dependency(1, 1));
  assert_eq!(table.records[2].artifact_length, 124);
  assert_eq!(table.records[3].version, "2.0.0");
  let plan = decode_parser_resolution_plan(compiled.parser_plan()).unwrap();
  assert_eq!(plan.candidates.iter().map(|candidate| candidate.dependency_ordinal).collect::<Vec<_>>(), [2, 1, 4, 3, 6, 5]);
  assert_eq!(plan.mime_dependency_ordinal, 7);
  assert_eq!(compiled.selector_dependency_ordinal(), Some(8));
}
