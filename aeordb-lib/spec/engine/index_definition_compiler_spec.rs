//! Corrected complete definitions, with independent frozen-byte expectations.
use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::dependency::{DependencyRecordV1, InvocationPolicyKind, InvocationPolicyV1};
use aeordb::engine::v4::field_definition::{decode_converter_definition, decode_field_index_definition};
use aeordb::engine::v4::index_definition_compiler::{
  CompiledIndexDefinitionsV1, ConverterDefinitionLimitsInputV1, CorrectedIndexInputV1, FieldDefinitionLimitsInputV1,
  IndexDefinitionCompilationRequestV1, SourceDefinitionLimitsInputV1, compile_index_definitions_v1, default_metadata_indexes_v1,
};
use aeordb::engine::v4::parser_context_compiler::{
  CompiledParserContextV1, ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
};
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use aeordb::engine::v4::source_selector_compiler::{
  CompiledSourceSelectorV1, SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1,
};
use aeordb::engine::v4::value_store::decode_value_store_definition;
use sha2::Digest;

const WORKSPACE: usize = 64 << 20;
const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];
const CONVERTERS: [&str; 12] = [
  "typed_exact_blake3_v1",
  "bytes_binary_order_v1",
  "utf8_binary_order_v1",
  "u64_order_v1",
  "i64_order_v1",
  "f64_finite_order_v1",
  "timestamp_ms_order_v1",
  "bool_order_v1",
  "unicode_trigram_v1",
  "soundex_ascii_v1",
  "double_metaphone_primary_ascii_v1",
  "double_metaphone_alt_ascii_v1",
];

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(192 << 20, 256 << 20, 32 << 20, 16 << 20).unwrap())
}

fn fixture(family: &str, prefix: &str, algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  let profile = if algorithm.hash_length() == 64 { "sha512" } else { "blake3-256" };
  std::fs::read(format!("{}/spec/fixtures/v4/{family}/{prefix}-{profile}-{name}-valid.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn identity(algorithm: HashAlgorithm, domain: &[u8], value: &[u8]) -> Vec<u8> {
  let mut input = domain.to_vec();
  input.extend_from_slice(value);
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(&input).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(&input).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(&input).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(&input).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(&input).to_vec(),
  }
}

fn policy() -> InvocationPolicyV1 {
  InvocationPolicyV1 {
    kind: InvocationPolicyKind::PureWasm,
    max_request_bytes: 64 << 20,
    max_response_bytes: 16 << 20,
    max_linear_memory_bytes: 64 << 20,
    max_fuel: 10_000_000,
    max_table_elements: 100_000,
    max_structure_nodes: 100_000,
    max_scalar_bytes: 65536,
    max_structure_depth: 32,
    max_container_members: 65535,
    max_wasm_instances: 1,
    max_wasm_memories: 1,
    max_wasm_tables: 1,
    max_value_stack_height: 4096,
    max_recursion_depth: 256,
  }
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
    fingerprint: [1; 32],
    dependency_id: "/org/example/shared",
    version: "1.2.3",
  }
}

fn children(memory: &MemoryCoordinator, name: &str, kind: u8) -> (CompiledSourceSelectorV1, CompiledParserContextV1) {
  let policy = policy();
  let context = compile_parser_context_v1(
    ParserContextCompilationRequestV1 {
      source: if kind == 0 {
        ParserContextSourceV1::Metadata
      } else {
        ParserContextSourceV1::Explicit { dependency: dependency(1), policy: &policy }
      },
      selector_dependency: match kind {
        0 => ParserSelectorDependencyV1::None,
        1 => ParserSelectorDependencyV1::JsonPath,
        _ => ParserSelectorDependencyV1::Mapper(dependency(2)),
      },
      maximum_workspace_bytes: WORKSPACE,
    },
    memory,
    &|| false,
  )
  .unwrap();
  let source = match kind {
    0 => SourceSelectorInputV1::Metadata,
    1 => SourceSelectorInputV1::JsonPath(None),
    _ => {
      SourceSelectorInputV1::Mapper { dependency_ordinal: context.selector_dependency_ordinal().unwrap(), arguments: None, policy: &policy }
    }
  };
  let source = compile_source_selector_v1(
    SourceSelectorCompilationRequestV1 { field_name: name, source, maximum_source_bytes: 1 << 20, maximum_workspace_bytes: WORKSPACE },
    memory,
    &|| false,
  )
  .unwrap();
  (source, context)
}

fn index(converter_id: u16) -> CorrectedIndexInputV1 {
  CorrectedIndexInputV1 {
    converter_id,
    converter_limits: ConverterDefinitionLimitsInputV1::default(),
    field_limits: FieldDefinitionLimitsInputV1::default(),
  }
}

fn request<'a>(
  scope: &'a [u8],
  source: &'a CompiledSourceSelectorV1,
  context: &'a CompiledParserContextV1,
  indexes: &'a [CorrectedIndexInputV1],
  algorithm: HashAlgorithm,
) -> IndexDefinitionCompilationRequestV1<'a> {
  IndexDefinitionCompilationRequestV1 {
    scope_id: scope,
    source,
    parser_context: context,
    source_limits: SourceDefinitionLimitsInputV1::default(),
    indexes,
    hash_algorithm: algorithm,
    maximum_workspace_bytes: WORKSPACE,
  }
}

fn metadata_bytes(algorithm: HashAlgorithm, scope: &[u8]) -> Vec<u8> {
  let mut expected = fixture("value-store-definition-v1", "avst", algorithm, "metadata-hash-corrected");
  let fixed = 32 + algorithm.hash_length();
  expected[32..fixed].copy_from_slice(scope);
  expected[fixed + 36..fixed + 40].copy_from_slice(&1024u32.to_le_bytes());
  expected[fixed + 48..fixed + 56].copy_from_slice(&(8u64 << 20).to_le_bytes());
  expected
}

fn converter_bytes(algorithm: HashAlgorithm, converter_id: u16) -> Vec<u8> {
  let mut expected = fixture("converter-definition-v1", "acnv", algorithm, CONVERTERS[usize::from(converter_id - 1)]);
  assert_eq!(expected.len(), 120);
  expected[64..72].copy_from_slice(&(1u64 << 20).to_le_bytes());
  expected[72..76].copy_from_slice(&65536u32.to_le_bytes());
  expected[76..80].copy_from_slice(&(1u32 << 20).to_le_bytes());
  expected[80..88].copy_from_slice(&(4u64 << 20).to_le_bytes());
  expected
}

fn field_bytes(algorithm: HashAlgorithm, value_id: &[u8], converter_id: u16) -> Vec<u8> {
  let mut expected = fixture("field-index-definition-v1", "afix", algorithm, CONVERTERS[usize::from(converter_id - 1)]);
  let fixed = 32 + algorithm.hash_length();
  expected[32..fixed].copy_from_slice(value_id);
  expected[fixed + 44..fixed + 48].copy_from_slice(&65536u32.to_le_bytes());
  expected[fixed + 48..fixed + 52].copy_from_slice(&65536u32.to_le_bytes());
  expected[fixed + 56..fixed + 64].copy_from_slice(&(8u64 << 20).to_le_bytes());
  expected[fixed + 64..fixed + 72].copy_from_slice(&(8u64 << 20).to_le_bytes());
  let converter_start = expected.len() - 120;
  expected[converter_start..].copy_from_slice(&converter_bytes(algorithm, converter_id));
  expected
}

#[test]
fn all_corrected_converters_compile_complete_independent_bytes_for_every_database_hash() {
  for algorithm in ALGORITHMS {
    let memory = memory();
    let (source, context) = children(&memory, "@hash", 0);
    let scope = vec![0x33; algorithm.hash_length()];
    for converter_id in 1..=12 {
      let indexes = [index(converter_id)];
      let compiled = compile_index_definitions_v1(request(&scope, &source, &context, &indexes, algorithm), &memory, &|| false).unwrap();
      let expected = metadata_bytes(algorithm, &scope);
      let value_id = identity(algorithm, b"aeordb.index.value-store-definition.v1\0", &expected);
      assert_eq!(compiled.value_store().value, expected);
      assert_eq!(compiled.value_store().value_store_id, value_id);
      let expected = field_bytes(algorithm, &value_id, converter_id);
      assert_eq!(compiled.field_indexes().len(), 1);
      assert_eq!(compiled.field_indexes()[0].value, expected);
      assert_eq!(compiled.field_indexes()[0].index_id, identity(algorithm, b"aeordb.index.field-definition.v1\0", &expected));
    }
  }
}

#[test]
fn default_recipe_materializes_exactly_eight_metadata_fields_and_thirteen_indexes() {
  let expected: BTreeMap<&str, Vec<u16>> = BTreeMap::from([
    ("@path", vec![3, 9]),
    ("@filename", vec![3, 9, 10, 11, 12]),
    ("@extension", vec![3]),
    ("@hash", vec![1]),
    ("@created_at", vec![7]),
    ("@updated_at", vec![7]),
    ("@size", vec![4]),
    ("@content_type", vec![1]),
  ]);
  let recipe = default_metadata_indexes_v1();
  assert_eq!(recipe.len(), 8);
  assert!(recipe.windows(2).all(|pair| pair[0].field_name < pair[1].field_name));
  assert_eq!(recipe.iter().map(|field| (field.field_name, field.converter_ids.to_vec())).collect::<BTreeMap<_, _>>(), expected);
  for algorithm in ALGORITHMS {
    let memory = memory();
    let mut count = 0;
    let scope = vec![1; algorithm.hash_length()];
    for field in recipe {
      let (source, context) = children(&memory, field.field_name, 0);
      let indexes: Vec<_> = field.converter_ids.iter().copied().map(index).collect();
      let compiled = compile_index_definitions_v1(request(&scope, &source, &context, &indexes, algorithm), &memory, &|| false).unwrap();
      let value = decode_value_store_definition(&compiled.value_store().value, algorithm).unwrap();
      assert_eq!(value.field_name, field.field_name);
      assert_eq!(
        (value.max_document_input_bytes, value.max_selector_work_items_per_document, value.max_selector_examined_bytes_per_document),
        (0, 0, 0)
      );
      count += compiled.field_indexes().len();
    }
    assert_eq!(count, 13);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn index_order_and_exact_duplicates_do_not_change_definition_outputs() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  let scope = [1; 32];
  let indexes = [index(12), index(1), index(3), index(1)];
  let first = compile_index_definitions_v1(request(&scope, &source, &context, &indexes, ALGORITHMS[0]), &memory, &|| false).unwrap();
  let indexes = [index(3), index(12), index(1)];
  let second = compile_index_definitions_v1(request(&scope, &source, &context, &indexes, ALGORITHMS[0]), &memory, &|| false).unwrap();
  assert_eq!(first.value_store(), second.value_store());
  assert_eq!(first.field_indexes(), second.field_indexes());
  assert_eq!(first.field_indexes().len(), 3);
  assert!(first.field_indexes().windows(2).all(|pair| pair[0].index_id < pair[1].index_id));
}

#[test]
fn distinct_limits_for_one_converter_remain_distinct_complete_definitions() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  let mut changed = index(1);
  changed.converter_limits.max_input_bytes = Some(4096);
  let indexes = [index(1), changed];
  let compiled = compile_index_definitions_v1(request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]), &memory, &|| false).unwrap();
  assert_eq!(compiled.field_indexes().len(), 2);
  assert_ne!(compiled.field_indexes()[0].index_id, compiled.field_indexes()[1].index_id);
  assert!(compiled.field_indexes().iter().all(|field| decode_field_index_definition(&field.value, ALGORITHMS[0])
    .unwrap()
    .converter
    .converter_id
    == 1));
}

#[test]
fn empty_legacy_and_unknown_converter_requests_are_rejected() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  for indexes in [vec![], vec![index(0)], vec![index(13)], vec![index(0x8001)], vec![index(0xffff)]] {
    let result = compile_index_definitions_v1(request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]), &memory, &|| false);
    assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })));
  }
}

#[test]
fn scope_identity_must_be_nonzero_and_have_the_selected_hash_width() {
  let memory = memory();
  let (source, context) = children(&memory, "@hash", 0);
  for algorithm in ALGORITHMS {
    for scope in [vec![], vec![1; algorithm.hash_length() - 1], vec![1; algorithm.hash_length() + 1], vec![0; algorithm.hash_length()]] {
      let result = compile_index_definitions_v1(request(&scope, &source, &context, &[index(1)], algorithm), &memory, &|| false);
      assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })));
    }
  }
}

#[test]
fn metadata_json_and_mapper_limits_materialize_only_applicable_defaults() {
  let memory = memory();
  for (kind, name, document, work, examined) in
    [(0, "@hash", 0, 0, 0), (1, "value", 64 << 20, 1_000_000, 64 << 20), (2, "value", 64 << 20, 0, 0)]
  {
    let (source, context) = children(&memory, name, kind);
    let compiled =
      compile_index_definitions_v1(request(&[1; 32], &source, &context, &[index(1)], ALGORITHMS[0]), &memory, &|| false).unwrap();
    let value = decode_value_store_definition(&compiled.value_store().value, ALGORITHMS[0]).unwrap();
    assert_eq!(value.max_source_values_per_document, 1024);
    assert_eq!(value.max_canonical_source_bytes_per_document, 8 << 20);
    assert_eq!(value.max_document_input_bytes, document);
    assert_eq!(value.max_selector_work_items_per_document, work);
    assert_eq!(value.max_selector_examined_bytes_per_document, examined);
    assert_eq!(value.parser_plan, aeordb::engine::v4::parser_plan::decode_parser_resolution_plan(context.parser_plan()).unwrap());
  }
}

#[test]
fn explicit_defaults_equal_omission_in_all_three_source_contexts() {
  let memory = memory();
  for (kind, name) in [(0, "@hash"), (1, "value"), (2, "value")] {
    let (source, context) = children(&memory, name, kind);
    let indexes = [index(1)];
    let initial = request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]);
    let omitted = compile_index_definitions_v1(initial.clone(), &memory, &|| false).unwrap();
    let mut explicit_index = index(1);
    explicit_index.converter_limits = ConverterDefinitionLimitsInputV1 {
      max_input_bytes: Some(1 << 20),
      max_output_values: Some(65536),
      max_output_value_bytes: Some(1 << 20),
      max_total_output_bytes: Some(4 << 20),
    };
    explicit_index.field_limits = FieldDefinitionLimitsInputV1 {
      max_terms_per_document: Some(65536),
      max_postings_per_document: Some(65536),
      max_canonical_posting_bytes_per_document: Some(8 << 20),
      max_query_recheck_value_bytes: Some(8 << 20),
    };
    let explicit_indexes = [explicit_index];
    let mut explicit = IndexDefinitionCompilationRequestV1 { indexes: &explicit_indexes, ..initial };
    explicit.source_limits = SourceDefinitionLimitsInputV1 {
      max_source_values_per_document: Some(1024),
      max_canonical_source_bytes_per_document: Some(8 << 20),
      max_document_input_bytes: Some(if kind == 0 { 0 } else { 64 << 20 }),
      max_selector_work_items_per_document: Some(if kind == 1 { 1_000_000 } else { 0 }),
      max_selector_examined_bytes_per_document: Some(if kind == 1 { 64 << 20 } else { 0 }),
    };
    let explicit = compile_index_definitions_v1(explicit, &memory, &|| false).unwrap();
    assert_eq!(omitted.value_store(), explicit.value_store());
    assert_eq!(omitted.field_indexes(), explicit.field_indexes());
  }
}

#[test]
fn corrected_converter_limits_reject_zero_and_values_above_each_public_maximum() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  for field in 0..4 {
    let maximum = [1 << 20, 65536, 1 << 20, 4 << 20][field];
    for value in [0, maximum + 1] {
      let mut input = index(1);
      match field {
        0 => input.converter_limits.max_input_bytes = Some(value),
        1 => input.converter_limits.max_output_values = Some(value as u32),
        2 => input.converter_limits.max_output_value_bytes = Some(value as u32),
        _ => input.converter_limits.max_total_output_bytes = Some(value),
      }
      let result = compile_index_definitions_v1(request(&[1; 32], &source, &context, &[input], ALGORITHMS[0]), &memory, &|| false);
      assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })), "field={field} value={value}");
    }
  }
  // The retained codec is not tightened to match the new-authoring policy.
  let mut retained = converter_bytes(ALGORITHMS[0], 1);
  retained[64..72].copy_from_slice(&(2u64 << 20).to_le_bytes());
  assert!(decode_converter_definition(&retained, ALGORITHMS[0]).is_ok());
}

#[test]
fn corrected_field_limits_reject_zero_and_values_above_each_public_maximum() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  for field in 0..4 {
    let maximum = [65536, 65536, 8 << 20, 8 << 20][field];
    for value in [0, maximum + 1] {
      let mut input = index(1);
      match field {
        0 => input.field_limits.max_terms_per_document = Some(value as u32),
        1 => input.field_limits.max_postings_per_document = Some(value as u32),
        2 => input.field_limits.max_canonical_posting_bytes_per_document = Some(value),
        _ => input.field_limits.max_query_recheck_value_bytes = Some(value),
      }
      let result = compile_index_definitions_v1(request(&[1; 32], &source, &context, &[input], ALGORITHMS[0]), &memory, &|| false);
      assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })), "field={field} value={value}");
    }
  }
}

#[test]
fn corrected_source_limits_reject_zero_and_values_above_each_public_maximum() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  let indexes = [index(1)];
  for field in 0..5 {
    let maximum = [1024, 8 << 20, 1 << 30, 1_000_000, 64 << 20][field];
    for value in [0, maximum + 1] {
      let mut request = request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]);
      match field {
        0 => request.source_limits.max_source_values_per_document = Some(value as u32),
        1 => request.source_limits.max_canonical_source_bytes_per_document = Some(value),
        2 => request.source_limits.max_document_input_bytes = Some(value),
        3 => request.source_limits.max_selector_work_items_per_document = Some(value),
        _ => request.source_limits.max_selector_examined_bytes_per_document = Some(value),
      }
      assert!(
        matches!(compile_index_definitions_v1(request, &memory, &|| false), Err(SemanticCompilationErrorV1::InvalidSource { .. })),
        "field={field} value={value}"
      );
    }
  }
}

#[test]
fn explicit_document_limit_can_reach_one_gib_without_rewriting_child_invocation_policy() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  let indexes = [index(1)];
  let mut request = request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]);
  request.source_limits.max_document_input_bytes = Some(1 << 30);
  let compiled = compile_index_definitions_v1(request, &memory, &|| false).unwrap();
  let value = decode_value_store_definition(&compiled.value_store().value, ALGORITHMS[0]).unwrap();
  assert_eq!(value.max_document_input_bytes, 1 << 30);
  assert_eq!(value.parser_plan.candidates[0].policy.max_request_bytes, 64 << 20);
  assert_eq!(value.parser_plan.candidates[0].policy.max_response_bytes, 16 << 20);
}

#[test]
fn inapplicable_source_limits_are_rejected_instead_of_silently_discarded() {
  let memory = memory();
  let indexes = [index(1)];
  for (kind, name, fields) in [(0, "@hash", vec![0, 1, 2]), (2, "value", vec![1, 2])] {
    let (source, context) = children(&memory, name, kind);
    for field in fields {
      let mut request = request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]);
      match field {
        0 => request.source_limits.max_document_input_bytes = Some(1),
        1 => request.source_limits.max_selector_work_items_per_document = Some(1),
        _ => request.source_limits.max_selector_examined_bytes_per_document = Some(1),
      }
      assert!(matches!(compile_index_definitions_v1(request, &memory, &|| false), Err(SemanticCompilationErrorV1::InvalidSource { .. })));
    }
  }
}

#[test]
fn independently_compiled_children_cannot_form_an_invalid_parent_closure() {
  let memory = memory();
  let (metadata, metadata_context) = children(&memory, "@hash", 0);
  let (json, json_context) = children(&memory, "value", 1);
  let (mapper, _mapper_context) = children(&memory, "value", 2);
  for (source, context) in [(&metadata, &json_context), (&json, &metadata_context), (&mapper, &json_context)] {
    let result = compile_index_definitions_v1(request(&[1; 32], source, context, &[index(1)], ALGORITHMS[0]), &memory, &|| false);
    assert!(matches!(result, Err(SemanticCompilationErrorV1::InvalidSource { .. })));
  }
}

#[test]
fn compilation_admission_and_cancellation_release_only_their_own_reservation() {
  let memory = memory();
  let (source, context) = children(&memory, "@hash", 0);
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let indexes = [index(1), index(3), index(9)];
  let initial = request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]);
  let limited = IndexDefinitionCompilationRequestV1 { maximum_workspace_bytes: 1, ..initial.clone() };
  assert!(matches!(compile_index_definitions_v1(limited, &memory, &|| false), Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  for cancellation_check in [1, 2, 3, 4] {
    let checks = std::cell::Cell::new(0);
    let result = compile_index_definitions_v1(initial.clone(), &memory, &|| {
      checks.set(checks.get() + 1);
      checks.get() >= cancellation_check
    });
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Cancelled)));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  let compiled = compile_index_definitions_v1(initial, &memory, &|| false).unwrap();
  assert!(memory.snapshot().unwrap().reserved_bytes > baseline);
  drop(compiled);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
}

#[test]
fn compiled_definitions_own_bytes_after_input_compilers_are_dropped() {
  let memory = memory();
  let compiled: CompiledIndexDefinitionsV1 = {
    let (source, context) = children(&memory, "@hash", 0);
    compile_index_definitions_v1(request(&[1; 32], &source, &context, &[index(1)], ALGORITHMS[0]), &memory, &|| false).unwrap()
  };
  assert_eq!(decode_value_store_definition(&compiled.value_store().value, ALGORITHMS[0]).unwrap().field_name, "@hash");
  assert_eq!(
    decode_field_index_definition(&compiled.field_indexes()[0].value, ALGORITHMS[0]).unwrap().value_store_id,
    compiled.value_store().value_store_id
  );
  assert!(memory.snapshot().unwrap().reserved_bytes > 0);
  drop(compiled);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn compiled_metadata_definitions_execute_the_actual_source_and_converter_runtime() {
  use aeordb::engine::file_record::FileRecord;
  use aeordb::engine::v4::index_definition_runtime::IndexDefinitionRuntimeV1;
  use aeordb::engine::v4::index_source::{SourceDocumentV1, SourceExtractionV1};
  for algorithm in ALGORITHMS {
    let memory = memory();
    let (source, context) = children(&memory, "@hash", 0);
    let scope = vec![1; algorithm.hash_length()];
    let compiled = compile_index_definitions_v1(request(&scope, &source, &context, &[index(1)], algorithm), &memory, &|| false).unwrap();
    let runtime =
      IndexDefinitionRuntimeV1::from_encoded(&compiled.value_store().value, &compiled.field_indexes()[0].value, algorithm).unwrap();
    let mut record = FileRecord::new("/data".into(), None, 0, vec![]);
    record.content_hash = vec![0x44; algorithm.hash_length()];
    let SourceExtractionV1::Values(values) =
      runtime.value_store().extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| false).unwrap()
    else {
      panic!("metadata source must be present");
    };
    let document = runtime.compile_source_values(&values).unwrap();
    assert_eq!(document.values.len(), 1);
    assert_eq!(document.posting_count, 1);
    assert_eq!(document.values[0].canonical_value, values[0]);
    assert_eq!(document.values[0].postings[0].posting_key.len(), 33);
  }
}

#[test]
fn every_materialized_limit_change_changes_exactly_its_transitive_identity() {
  let memory = memory();
  let (source, context) = children(&memory, "value", 1);
  let indexes = [index(1)];
  let initial = request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]);
  let baseline = compile_index_definitions_v1(initial.clone(), &memory, &|| false).unwrap();
  for field in 0..13 {
    let mut changed_index = index(1);
    match field {
      5 => changed_index.converter_limits.max_input_bytes = Some(1),
      6 => changed_index.converter_limits.max_output_values = Some(1),
      7 => changed_index.converter_limits.max_output_value_bytes = Some(1),
      8 => changed_index.converter_limits.max_total_output_bytes = Some(1),
      9 => changed_index.field_limits.max_terms_per_document = Some(1),
      10 => changed_index.field_limits.max_postings_per_document = Some(1),
      11 => changed_index.field_limits.max_canonical_posting_bytes_per_document = Some(1),
      12 => changed_index.field_limits.max_query_recheck_value_bytes = Some(1),
      _ => {}
    }
    let changed_indexes = [changed_index];
    let mut changed = IndexDefinitionCompilationRequestV1 { indexes: &changed_indexes, ..initial.clone() };
    match field {
      0 => changed.source_limits.max_source_values_per_document = Some(1),
      1 => changed.source_limits.max_canonical_source_bytes_per_document = Some(1),
      2 => changed.source_limits.max_document_input_bytes = Some(1),
      3 => changed.source_limits.max_selector_work_items_per_document = Some(1),
      4 => changed.source_limits.max_selector_examined_bytes_per_document = Some(1),
      _ => {}
    }
    let changed = compile_index_definitions_v1(changed, &memory, &|| false).unwrap();
    assert_ne!(changed.field_indexes()[0].index_id, baseline.field_indexes()[0].index_id, "field={field}");
    assert_eq!(changed.value_store().value_store_id != baseline.value_store().value_store_id, field < 5, "field={field}");
  }
}

#[test]
fn index_compilation_rechecks_revoked_shared_memory_admission() {
  let memory = memory();
  let (source, context) = children(&memory, "@hash", 0);
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let calls = std::cell::Cell::new(0);
  let indexes = [index(1), index(3), index(9)];
  let result = compile_index_definitions_v1(request(&[1; 32], &source, &context, &indexes, ALGORITHMS[0]), &memory, &|| {
    calls.set(calls.get() + 1);
    if calls.get() == 2 {
      memory.reconfigure_policy(MemoryPolicy::new(32 << 10, 64 << 10, 1, 16 << 10).unwrap()).unwrap();
    }
    false
  });
  assert!(calls.get() >= 2);
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
}
