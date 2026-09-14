//! Measured preflight and fallible-output allocation for canonical writers.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use aeordb::engine::v4::dependency::{encode_dependency_record, encode_dependency_table};
use aeordb::engine::v4::field_definition::{
  encode_converter_definition, encode_field_index_definition, ConverterDefinitionWriteV1, FieldIndexDefinitionWriteV1,
};
use aeordb::engine::v4::native_semantics::NativeSemanticComponentV1;
use aeordb::engine::v4::namespace::{
  encode_semantic_catalog_internal, encode_semantic_catalog_leaf, encode_semantic_definition_object, SemanticCatalogChildV1,
  SemanticCatalogRecordV1,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::v4::value_store::{encode_value_store_definition, ValueStoreDefinitionWriteV1, ValueStoreSemanticFamily};
use aeordb::engine::HashAlgorithm;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

#[derive(Clone, Copy, Debug, Default)]
struct Allocations {
  total: usize,
  maximum: usize,
  injected_failure: bool,
}

thread_local! {
  static ENABLED: Cell<bool> = const { Cell::new(false) };
  static FAIL_SIZE: Cell<usize> = const { Cell::new(0) };
  static ALLOCATIONS: Cell<Allocations> = const { Cell::new(Allocations { total: 0, maximum: 0, injected_failure: false }) };
}

struct WriterAllocator;

#[global_allocator]
static ALLOCATOR: WriterAllocator = WriterAllocator;

fn should_fail(size: usize) -> bool {
  if !ENABLED.try_with(Cell::get).unwrap_or(false) {
    return false;
  }
  let fail = FAIL_SIZE.with(|target| target.get() != 0 && target.get() == size);
  if fail {
    FAIL_SIZE.with(|target| target.set(0));
  }
  ALLOCATIONS.with(|value| {
    let mut measured = value.get();
    measured.total = measured.total.saturating_add(size);
    measured.maximum = measured.maximum.max(size);
    measured.injected_failure |= fail;
    value.set(measured);
  });
  fail
}

unsafe impl GlobalAlloc for WriterAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if should_fail(layout.size()) {
      std::ptr::null_mut()
    } else {
      unsafe { System.alloc(layout) }
    }
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    if should_fail(layout.size()) {
      std::ptr::null_mut()
    } else {
      unsafe { System.alloc_zeroed(layout) }
    }
  }

  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    if should_fail(size) {
      std::ptr::null_mut()
    } else {
      unsafe { System.realloc(pointer, layout, size) }
    }
  }

  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    unsafe { System.dealloc(pointer, layout) };
  }
}

struct Measurement;

impl Drop for Measurement {
  fn drop(&mut self) {
    ENABLED.with(|enabled| enabled.set(false));
    FAIL_SIZE.with(|target| target.set(0));
  }
}

fn measure<T>(fail_size: usize, action: impl FnOnce() -> T) -> (T, Allocations) {
  ALLOCATIONS.with(|measured| measured.set(Allocations::default()));
  FAIL_SIZE.with(|target| target.set(fail_size));
  ENABLED.with(|enabled| enabled.set(true));
  let guard = Measurement;
  let result = action();
  let allocations = ALLOCATIONS.with(Cell::get);
  drop(guard);
  (result, allocations)
}

fn fixture(family: &str, filename: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{family}/{filename}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn metadata_request(bytes: &[u8]) -> ValueStoreDefinitionWriteV1<'_> {
  assert_eq!(&bytes[144..149], b"@hash");
  assert_eq!(bytes.len(), 269);
  ValueStoreDefinitionWriteV1 {
    scope_id: &bytes[32..64],
    field_name: "@hash",
    semantic_family: ValueStoreSemanticFamily::CorrectedV1,
    max_source_values_per_document: 1,
    max_canonical_source_bytes_per_document: 4096,
    max_document_input_bytes: 0,
    max_selector_work_items_per_document: 0,
    max_selector_examined_bytes_per_document: 0,
    selector: &bytes[149..189],
    parser_plan: &bytes[189..237],
    dependencies: &bytes[237..269],
  }
}

fn converter_request() -> ConverterDefinitionWriteV1<'static> {
  ConverterDefinitionWriteV1 {
    converter_id: 1,
    max_input_bytes: 4096,
    max_output_values: 1,
    max_output_value_bytes: 4096,
    max_total_output_bytes: 4096,
    parameters: &[],
  }
}

fn field_request(converter: &[u8]) -> FieldIndexDefinitionWriteV1<'_> {
  FieldIndexDefinitionWriteV1 {
    value_store_id: &[1; 32],
    converter_definition: converter,
    max_terms_per_document: 1,
    max_postings_per_document: 1,
    max_canonical_posting_bytes_per_document: 4096,
    max_query_recheck_value_bytes: 4096,
  }
}

fn assert_preflight(allocations: Allocations) {
  // Only bounded diagnostic strings may allocate; no child copy/output buffer.
  assert!(allocations.maximum < 1024, "{allocations:?}");
  assert!(allocations.total < 4096, "{allocations:?}");
}

#[test]
fn dependency_writer_rejects_oversized_components_and_counts_before_copying() {
  let large_id = "/x".repeat(128 * 1024);
  let large_version = "a".repeat(128 * 1024);
  for component in 0..2 {
    let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
    if component == 0 {
      record.dependency_id = &large_id;
    } else {
      record.version = &large_version;
    }
    let (result, allocations) = measure(0, || encode_dependency_record(&record));
    assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
    assert_preflight(allocations);
  }
  let records = vec![NativeSemanticComponentV1::RawJson.dependency_record(); 1025];
  let (result, allocations) = measure(0, || encode_dependency_table(&records));
  assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
  assert_preflight(allocations);
}

#[test]
fn value_store_writer_caps_all_children_before_decoding_or_copying() {
  let bytes = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid");
  // Warm immutable registry initialization outside the measured operation.
  encode_value_store_definition(metadata_request(&bytes), ALGORITHM).unwrap();
  let large = vec![0; 512 * 1024];
  for child in 0..3 {
    let mut request = metadata_request(&bytes);
    match child {
      0 => request.selector = &large,
      1 => request.parser_plan = &large,
      2 => request.dependencies = &large,
      _ => unreachable!(),
    }
    let (result, allocations) = measure(0, || encode_value_store_definition(request, ALGORITHM));
    assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
    assert_preflight(allocations);
  }
}

#[test]
fn converter_and_field_writers_preflight_oversized_payloads() {
  encode_converter_definition(converter_request(), ALGORITHM).unwrap();
  let excessive = vec![0; 256 * 1024];
  let mut converter = converter_request();
  converter.parameters = &excessive;
  let (result, allocations) = measure(0, || encode_converter_definition(converter, ALGORITHM));
  assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
  assert_preflight(allocations);
  let (result, allocations) = measure(0, || encode_field_index_definition(field_request(&excessive), ALGORITHM));
  assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
  assert_preflight(allocations);
}

#[test]
fn every_definition_writer_returns_an_error_when_its_output_reservation_fails() {
  let record = NativeSemanticComponentV1::RawJson.dependency_record();
  let record_length = 96 + record.dependency_id.len() + record.version.len();
  let (result, allocations) = measure(record_length, || encode_dependency_record(&record));
  assert!(allocations.injected_failure);
  assert_eq!(result.unwrap_err().code(), "dependency_writer_allocation");
  let (result, allocations) = measure(32 + record_length, || encode_dependency_table(std::slice::from_ref(&record)));
  assert!(allocations.injected_failure);
  assert_eq!(result.unwrap_err().code(), "dependency_writer_allocation");
  let metadata = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid");
  encode_value_store_definition(metadata_request(&metadata), ALGORITHM).unwrap();
  let (result, allocations) = measure(metadata.len(), || encode_value_store_definition(metadata_request(&metadata), ALGORITHM));
  assert!(allocations.injected_failure);
  assert_eq!(result.unwrap_err().code(), "value_store_writer_allocation");
  let converter = encode_converter_definition(converter_request(), ALGORITHM).unwrap().value;
  let (result, allocations) = measure(converter.len(), || encode_converter_definition(converter_request(), ALGORITHM));
  assert!(allocations.injected_failure);
  assert_eq!(result.unwrap_err().code(), "definition_writer_allocation");
  let field_length = encode_field_index_definition(field_request(&converter), ALGORITHM).unwrap().value.len();
  let (result, allocations) = measure(field_length, || encode_field_index_definition(field_request(&converter), ALGORITHM));
  assert!(allocations.injected_failure);
  assert_eq!(result.unwrap_err().code(), "definition_writer_allocation");
}

#[test]
fn catalog_writers_preflight_large_inputs_without_copying_or_output_allocation() {
  let owner = vec![0; 65_537];
  let record = SemanticCatalogRecordV1 { record_kind: 1, semantic_id: &[1; 32], definition_object_id: &[2; 32], owner_key: &owner };
  let records = vec![record; 17];
  let (result, allocations) = measure(0, || encode_semantic_catalog_leaf(&records, ALGORITHM));
  assert_eq!(result.unwrap_err().code(), "catalog_leaf_exceeds_cap");
  assert_preflight(allocations);
  let excessive_count = vec![record; 4097];
  let (result, allocations) = measure(0, || encode_semantic_catalog_leaf(&excessive_count, ALGORITHM));
  assert_eq!(result.unwrap_err().code(), "catalog_leaf_count");
  assert_preflight(allocations);
  let prefix = vec![0; 65_536];
  let children = [
    SemanticCatalogChildV1 { edge: 0, record_count: 1, object_id: &[1; 32] },
    SemanticCatalogChildV1 { edge: 1, record_count: 1, object_id: &[2; 32] },
  ];
  let (result, allocations) = measure(0, || encode_semantic_catalog_internal(0, &prefix, &children, ALGORITHM));
  assert_eq!(result.unwrap_err().code(), "catalog_internal_metadata");
  assert_preflight(allocations);
}

#[test]
fn catalog_writers_reject_malformed_bounded_records_before_allocating_output() {
  let owner = vec![0xff; 60_000];
  let record = SemanticCatalogRecordV1 { record_kind: 1, semantic_id: &[1; 32], definition_object_id: &[2; 32], owner_key: &owner };
  let (result, allocations) = measure(0, || encode_semantic_catalog_leaf(&[record], ALGORITHM));
  assert!(result.is_err());
  assert_preflight(allocations);
  let children = [
    SemanticCatalogChildV1 { edge: 0, record_count: 1, object_id: &[1; 32] },
    SemanticCatalogChildV1 { edge: 1, record_count: 1, object_id: &owner },
  ];
  let (result, allocations) = measure(0, || encode_semantic_catalog_internal(0, &[], &children, ALGORITHM));
  assert!(result.is_err());
  assert_preflight(allocations);
}

#[test]
fn both_catalog_writers_return_typed_allocation_failure_for_every_hash_width() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let width = algorithm.hash_length();
    let identity = vec![1; width];
    let record = SemanticCatalogRecordV1 { record_kind: 3, semantic_id: &identity, definition_object_id: &identity, owner_key: &identity };
    let length = 60 + 4 * width;
    let (result, allocations) = measure(length, || encode_semantic_catalog_leaf(&[record], algorithm));
    assert!(allocations.injected_failure);
    assert_eq!(result.unwrap_err().code(), "catalog_writer_allocation");
    let children = [
      SemanticCatalogChildV1 { edge: 0, record_count: 1, object_id: &identity },
      SemanticCatalogChildV1 { edge: 1, record_count: 1, object_id: &identity },
    ];
    let length = 56 + 2 * (12 + width);
    let (result, allocations) = measure(length, || encode_semantic_catalog_internal(0, &[], &children, algorithm));
    assert!(allocations.injected_failure);
    assert_eq!(result.unwrap_err().code(), "catalog_writer_allocation");
  }
}

#[test]
fn definition_object_wrapper_caps_inputs_and_returns_output_allocation_failures() {
  let excessive = vec![0; 1_048_576];
  for class in 1..=7 {
    let (result, allocations) = measure(0, || encode_semantic_definition_object(class, &excessive, ALGORITHM));
    assert_eq!(result.unwrap_err().code(), "semantic_definition_exceeds_cap");
    assert_preflight(allocations);
  }
  let projection = [0x0a, 4, 0, 0, 0, 0, 0, 0, 0];
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let length = 52 + algorithm.hash_length() + projection.len();
    let (result, allocations) = measure(length, || encode_semantic_definition_object(1, &projection, algorithm));
    assert!(allocations.injected_failure);
    assert_eq!(result.unwrap_err().code(), "semantic_definition_writer_allocation");
  }
}

#[test]
fn catalog_cow_returns_fallible_metadata_and_leaf_output_allocation_errors_without_leaking_admission() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::namespace::EncodedSemanticObjectV1;
  use aeordb::engine::v4::semantic_catalog::{SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorV1};
  use aeordb::engine::v4::semantic_catalog_mutation::{
    SemanticCatalogMutationRequestV1, SemanticCatalogMutationV1, SemanticCatalogSnapshotV1, plan_semantic_catalog_mutation_v1,
  };

  struct EmptySource;

  impl SemanticCatalogObjectSourceV1 for EmptySource {
    fn load_semantic_object(&self, _kind: u16, _identity: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
      panic!("empty catalog must not read a source node");
    }
  }

  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let identity = vec![1; algorithm.hash_length()];
    let definition = vec![2; algorithm.hash_length()];
    let record = SemanticCatalogRecordV1 {
      record_kind: 2,
      owner_key: b"\x02\x00/controls/example.json",
      semantic_id: &identity,
      definition_object_id: &definition,
    };
    let encoded_length = encode_semantic_catalog_leaf(&[record], algorithm).unwrap().value.len();
    let metadata_length = (algorithm.hash_length() + 3) * std::mem::size_of::<EncodedSemanticObjectV1>();
    for (length, expected_code) in [(metadata_length, "catalog_mutation_allocation"), (encoded_length, "catalog_writer_allocation")] {
      let memory =
        MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap());
      let (result, allocations) = measure(length, || {
        plan_semantic_catalog_mutation_v1(
          SemanticCatalogMutationRequestV1 {
            hash_algorithm: algorithm,
            snapshot: SemanticCatalogSnapshotV1 { root_object_id: None, record_count: 0, node_count: 0 },
            mutation: SemanticCatalogMutationV1::Upsert(record),
            maximum_workspace_bytes: 32 * 1024 * 1024,
          },
          &EmptySource,
          &memory,
          &|| false,
        )
      });
      assert!(allocations.injected_failure, "{allocations:?}");
      assert_eq!(result.err().expect("allocation refusal must propagate").code(), expected_code);
      assert!(allocations.total < 65536, "{allocations:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}

#[test]
fn registry_compiler_preflights_source_and_propagates_projection_allocation_failure() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::dependency::DependencyRecordV1;
  use aeordb::engine::v4::parser_registry_compiler::{
    ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
  };

  struct Snapshot;
  impl ParserAliasSnapshotV1 for Snapshot {
    fn resolve_parser_alias(&self, _alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      panic!("empty or unadmitted sources may not resolve dependencies");
    }
  }
  let excessive = vec![b' '; 1_048_576];
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap());
    let request = ParserRegistryCompilationRequestV1 {
      source: Some(&excessive),
      hash_algorithm: algorithm,
      maximum_source_bytes: 4096,
      maximum_workspace_bytes: 16 * 1024 * 1024,
    };
    let (result, allocations) = measure(0, || compile_parser_registry_v1(request, &Snapshot, &memory, &|| false));
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    assert_preflight(allocations);
    let (result, allocations) = measure(61 + algorithm.hash_length(), || {
      compile_parser_registry_v1(ParserRegistryCompilationRequestV1 { source: None, ..request }, &Snapshot, &memory, &|| false)
    });
    assert!(allocations.injected_failure, "{allocations:?}");
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn registry_source_allocation_failure_is_operational_not_invalid_configuration() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::dependency::DependencyRecordV1;
  use aeordb::engine::v4::parser_registry_compiler::{
    ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
  };

  struct NoLookup;
  impl ParserAliasSnapshotV1 for NoLookup {
    fn resolve_parser_alias(&self, _alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      panic!("source allocation refusal must occur before dependency lookup");
    }
  }
  let memory = MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap());
  let request = ParserRegistryCompilationRequestV1 {
    source: Some(br#"{"$v":1,"parsers":{"text/plain":"x"}}"#),
    hash_algorithm: HashAlgorithm::Blake3_256,
    maximum_source_bytes: 4096,
    maximum_workspace_bytes: 16 * 1024 * 1024,
  };
  let (result, allocations) =
    measure(4 * std::mem::size_of::<(String, String)>(), || compile_parser_registry_v1(request, &NoLookup, &memory, &|| false));
  assert!(allocations.injected_failure, "{allocations:?}");
  let error = result.err().expect("allocation refusal must propagate");
  assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }), "{error}");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn parser_context_allocations_are_fallible_and_release_shared_admission() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::dependency::{DependencyRecordV1, decode_dependency_record_bytes};
  use aeordb::engine::v4::parser_context_compiler::{
    ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
  };
  use aeordb::engine::v4::parser_plan::{ParserCandidateV1, decode_parser_resolution_plan};
  use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;

  let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
  let definition = fixture("semantic-object-v1", "asem-blake3-256-wasm-parser-definition-valid");
  let dependency = decode_dependency_record_bytes(&definition[80..definition.len() - 4]).unwrap();
  let program = fixture("parser-resolution-plan-v1", "aprp-blake3-256-explicit-plugin-valid");
  let mut policy = decode_parser_resolution_plan(&program).unwrap().candidates[0].policy.clone();
  policy.max_fuel = 10_000_000;
  for source in [ParserContextSourceV1::Metadata, ParserContextSourceV1::Explicit { dependency: dependency.clone(), policy: &policy }] {
    let metadata = matches!(source, ParserContextSourceV1::Metadata);
    let request = ParserContextCompilationRequestV1 {
      source,
      selector_dependency: if metadata { ParserSelectorDependencyV1::None } else { ParserSelectorDependencyV1::JsonPath },
      maximum_workspace_bytes: 16 << 20,
    };
    // Warm native identities before injecting individual output/metadata failures.
    let compiled = compile_parser_context_v1(request.clone(), &memory, &|| false).unwrap();
    let mut sizes = vec![compiled.dependencies().len(), compiled.parser_plan().len()];
    if !metadata {
      sizes.extend([2 * std::mem::size_of::<DependencyRecordV1<'_>>(), std::mem::size_of::<ParserCandidateV1<'_>>()]);
    }
    drop(compiled);
    for size in sizes {
      let (result, allocations) = measure(size, || compile_parser_context_v1(request.clone(), &memory, &|| false));
      assert!(allocations.injected_failure, "metadata={metadata} size={size}: {allocations:?}");
      assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
    let (result, allocations) = measure(0, || {
      compile_parser_context_v1(ParserContextCompilationRequestV1 { maximum_workspace_bytes: 1, ..request }, &memory, &|| false)
    });
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    assert_preflight(allocations);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
