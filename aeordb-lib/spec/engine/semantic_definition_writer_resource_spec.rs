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

#[path = "retained_active_pointer_resource_spec.rs"]
mod retained_active_pointer_resource_spec;

#[path = "retained_definition_read_resource_spec.rs"]
mod retained_definition_read_resource_spec;

#[path = "retained_native_resource_spec.rs"]
mod retained_native_resource_spec;

#[path = "retained_semantic_catalog_resource_spec.rs"]
mod retained_semantic_catalog_resource_spec;

#[path = "semantic_catalog_compiler_resource_spec.rs"]
mod semantic_catalog_compiler_resource_spec;

#[path = "semantic_catalog_lookup_resource_spec.rs"]
mod semantic_catalog_lookup_resource_spec;

#[path = "canonical_value_borrowed_resource_spec.rs"]
mod canonical_value_borrowed_resource_spec;

#[path = "semantic_mutation_control_resource_spec.rs"]
mod semantic_mutation_control_resource_spec;

#[path = "semantic_mutation_writer_resource_spec.rs"]
mod semantic_mutation_writer_resource_spec;

#[path = "plugin_artifact_identity_resource_spec.rs"]
mod plugin_artifact_identity_resource_spec;

#[derive(Clone, Copy, Debug, Default)]
struct Allocations {
  total: usize,
  maximum: usize,
  matching_requests: usize,
  injected_failure: bool,
}

thread_local! {
  static ENABLED: Cell<bool> = const { Cell::new(false) };
  static FAIL_SIZE: Cell<usize> = const { Cell::new(0) };
  static FAIL_OCCURRENCE: Cell<usize> = const { Cell::new(1) };
  static ALLOCATIONS: Cell<Allocations> = const { Cell::new(Allocations { total: 0, maximum: 0, matching_requests: 0, injected_failure: false }) };
}

struct WriterAllocator;

#[global_allocator]
static ALLOCATOR: WriterAllocator = WriterAllocator;

fn should_fail(size: usize) -> bool {
  if !ENABLED.try_with(Cell::get).unwrap_or(false) {
    return false;
  }
  let matches_size = FAIL_SIZE.with(|target| target.get() != 0 && target.get() == size);
  let fail = matches_size
    && FAIL_OCCURRENCE.with(|remaining| {
      let occurrence = remaining.get();
      remaining.set(occurrence.saturating_sub(1));
      occurrence == 1
    });
  if fail {
    FAIL_SIZE.with(|target| target.set(0));
  }
  ALLOCATIONS.with(|value| {
    let mut measured = value.get();
    measured.total = measured.total.saturating_add(size);
    measured.maximum = measured.maximum.max(size);
    measured.matching_requests += usize::from(matches_size);
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
  measure_nth(fail_size, 1, action)
}

fn measure_nth<T>(fail_size: usize, occurrence: usize, action: impl FnOnce() -> T) -> (T, Allocations) {
  assert!(occurrence > 0);
  ALLOCATIONS.with(|measured| measured.set(Allocations::default()));
  FAIL_SIZE.with(|target| target.set(fail_size));
  FAIL_OCCURRENCE.with(|remaining| remaining.set(occurrence));
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

#[test]
fn dependency_metadata_validation_borrows_long_canonical_versions_without_heap_work() {
  use aeordb::engine::v4::dependency::decode_dependency_record_bytes;
  use aeordb::engine::v4::native_semantics::NativeSemanticComponentV1;
  for version in ["1.0.0".to_string(), format!("1.0.0-{}+{}", "a".repeat(120), "b".repeat(120))] {
    let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
    record.version = &version;
    let bytes = encode_dependency_record(&record).unwrap();
    let (result, allocations) = measure(0, || decode_dependency_record_bytes(&bytes));
    let decoded = result.unwrap();
    assert_eq!(decoded.version, version);
    assert_eq!(allocations.total, 0, "borrowed metadata allocated: {allocations:?}");
  }
}

#[test]
fn plugin_metadata_readers_borrow_maximum_fields_without_heap_work() {
  use aeordb::engine::v4::plugin_identity::{decode_plugin_alias_v1, decode_plugin_manifest_payload_v1};
  for profile in ["blake3-256", "sha512"] {
    for (case, alias) in [("corrected", "parse/é".to_string()), ("legacy", "old".to_string()), ("maximum", "a".repeat(4096))] {
      let bytes = fixture("plugin-alias-record-v1", &format!("apal-{profile}-{case}"));
      let path = format!("/.aeordb-system/plugin-aliases/{}", blake3::hash(alias.as_bytes()).to_hex());
      let (result, allocations) = measure(0, || decode_plugin_alias_v1(&bytes, &path));
      assert_eq!(result.unwrap().alias, alias);
      assert_eq!(allocations.total, 0, "alias {case}: {allocations:?}");
    }
    for case in ["parser", "mapper", "both", "maximum"] {
      let bytes = fixture("plugin-manifest-v1", &format!("apwm-{profile}-{case}"));
      let (result, allocations) = measure(0, || decode_plugin_manifest_payload_v1(&bytes).map(|manifest| manifest.roles().count()));
      assert_eq!(result.unwrap(), if matches!(case, "both" | "maximum") { 2 } else { 1 });
      assert_eq!(allocations.total, 0, "manifest {case}: {allocations:?}");
    }
  }
}

#[test]
fn plugin_metadata_rejects_amplified_counts_before_input_sized_allocation() {
  use aeordb::engine::v4::plugin_identity::{decode_plugin_alias_v1, decode_plugin_manifest_payload_v1};
  let alias = fixture("plugin-alias-record-v1", "apal-blake3-256-corrected");
  for offset in [16, 20, 24, 28, 32] {
    let mut bytes = alias.clone();
    bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let (result, allocations) = measure(0, || decode_plugin_alias_v1(&bytes, ""));
    assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
    assert!(allocations.total <= 256, "{allocations:?}");
  }
  let manifest = fixture("plugin-manifest-v1", "apwm-blake3-256-both");
  for (offset, length) in [(16, 4), (20, 4), (24, 4), (28, 4), (32, 2)] {
    let mut bytes = manifest.clone();
    bytes[offset..offset + length].fill(0xff);
    let (result, allocations) = measure(0, || decode_plugin_manifest_payload_v1(&bytes));
    assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
    assert!(allocations.total <= 256, "{allocations:?}");
  }
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

#[test]
fn canonical_argument_encoding_returns_errors_when_frame_or_container_allocation_is_refused() {
  use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};

  // The failing-first encoder aborts on refusal. Prevent any system core-dump
  // service from collecting this disposable test process, even with a piped
  // core_pattern that ignores the shell's RLIMIT_CORE. Restore after success.
  #[cfg(target_os = "linux")]
  let previous_dumpability = unsafe {
    let previous = libc::prctl(libc::PR_GET_DUMPABLE, 0usize, 0usize, 0usize, 0usize);
    assert!(matches!(previous, 0 | 1));
    assert_eq!(libc::prctl(libc::PR_SET_DUMPABLE, 0usize, 0usize, 0usize, 0usize), 0);
    previous
  };

  // Build caller-owned inputs before enabling the allocator hook. The first
  // size selects a Null frame; the second selects initial container growth.
  for (value, size) in [(CanonicalConfigValueV1::Null, 5), (CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Null]), 8)] {
    let (result, allocations) = measure(size, || encode_canonical_value(&value, CanonicalValueBounds::CONFIG));
    assert!(allocations.injected_failure, "size={size}: {allocations:?}");
    assert_eq!(result.unwrap_err().class(), MalformedInputClass::AllocationAmplification);
  }

  #[cfg(target_os = "linux")]
  assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, previous_dumpability as usize, 0usize, 0usize, 0usize) }, 0);
}

#[test]
fn source_compiler_retained_names_segments_and_selector_buffers_fail_without_leaks() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
  use aeordb::engine::v4::source_selector::JsonPathSegmentV1;
  use aeordb::engine::v4::source_selector_compiler::{SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1};
  let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
  let name = "n".repeat(313);
  let path = [serde_json::Value::String("k".repeat(53))];
  for metadata in [true, false] {
    let request = SourceSelectorCompilationRequestV1 {
      field_name: if metadata { "@path" } else { &name },
      source: if metadata { SourceSelectorInputV1::Metadata } else { SourceSelectorInputV1::JsonPath(Some(&path)) },
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    };
    let compiled = compile_source_selector_v1(request.clone(), &memory, &|| false).unwrap();
    let mut sizes = vec![compiled.field_name().len(), compiled.selector().len()];
    if !metadata {
      sizes.push(std::mem::size_of::<JsonPathSegmentV1<'_>>());
    }
    drop(compiled);
    for size in sizes {
      let (result, allocations) = measure(size, || compile_source_selector_v1(request.clone(), &memory, &|| false));
      assert!(allocations.injected_failure, "metadata={metadata} size={size}: {allocations:?}");
      assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
    let (result, allocations) = measure(0, || {
      compile_source_selector_v1(SourceSelectorCompilationRequestV1 { maximum_workspace_bytes: 1, ..request }, &memory, &|| false)
    });
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    assert_preflight(allocations);
  }
}

#[test]
fn source_compiler_rejects_impossible_regex_wire_length_before_building_its_syntax_tree() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
  use aeordb::engine::v4::source_selector_compiler::{SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1};
  let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
  let path = [serde_json::Value::String(format!("/{}/", "x".repeat(300_000)))];
  let (result, allocations) = measure(0, || {
    compile_source_selector_v1(
      SourceSelectorCompilationRequestV1 {
        field_name: "value",
        source: SourceSelectorInputV1::JsonPath(Some(&path)),
        maximum_source_bytes: 1 << 20,
        maximum_workspace_bytes: 64 << 20,
      },
      &memory,
      &|| false,
    )
  });
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_preflight(allocations);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn canonical_encoder_refusal_is_operational_at_metadata_source_consumers() {
  use aeordb::engine::file_record::FileRecord;
  use aeordb::engine::v4::index_source::{SourceDocumentV1, SourceOperationalErrorClassV1, ValueStoreRuntimeV1};
  let definition = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid");
  let runtime = ValueStoreRuntimeV1::from_encoded(&definition, ALGORITHM).unwrap();
  let mut record = FileRecord::new("/data".into(), None, 0, Vec::new());
  record.content_hash = vec![1; 32];
  let (result, allocations) =
    measure(37, || runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| false));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(
    result.expect_err("transient allocation refusal must not become a durable unindexable value").class(),
    SourceOperationalErrorClassV1::HostFailure
  );
}

#[test]
fn canonical_encoder_refusal_is_operational_at_converter_and_definition_consumers() {
  use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
  use aeordb::engine::v4::index_converter::ConverterRuntimeV1;
  use aeordb::engine::v4::index_definition_runtime::IndexDefinitionRuntimeV1;
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  let definition = fixture("converter-definition-v1", "acnv-blake3-256-typed_exact_blake3_v1-valid");
  let converter = ConverterRuntimeV1::from_encoded(&definition, ALGORITHM).unwrap();
  let (result, allocations) = measure(5, || converter.compile_source_value(&CanonicalConfigValueV1::Null));
  assert!(allocations.injected_failure, "{allocations:?}");
  // Name the required runtime classification before adding its enum variant.
  assert_eq!(format!("{:?}", result.unwrap_err().class()), "HostFailure");

  let value = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid");
  let value_id = decode_value_store_definition(&value, ALGORITHM).unwrap().value_store_id;
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid");
  field[32..64].copy_from_slice(&value_id);
  let runtime = IndexDefinitionRuntimeV1::from_encoded(&value, &field, ALGORITHM).unwrap();
  let mut canonical = vec![8, 32, 0, 0, 0];
  canonical.extend_from_slice(&[1; 32]);
  let values = [canonical];
  let (result, allocations) = measure(37, || runtime.compile_source_values(&values));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(format!("{:?}", result.unwrap_err().class()), "HostFailure");
}

#[path = "../helpers/native_semantic_dependencies.rs"]
mod native_semantic_dependencies;

fn json_root_definition(legacy: bool) -> Vec<u8> {
  json_selector_definition(legacy, &[])
}

fn json_selector_definition(legacy: bool, segments: &[aeordb::engine::v4::source_selector::JsonPathSegmentV1<'_>]) -> Vec<u8> {
  use aeordb::engine::v4::source_selector::{SourceSelectorWriteV1, encode_source_selector};
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  let mut bytes =
    fixture("value-store-definition-v1", if legacy { "avst-blake3-256-json-legacy-valid" } else { "avst-blake3-256-json-corrected-valid" });
  native_semantic_dependencies::pin_native_semantics(&mut bytes, ALGORITHM);
  let definition = decode_value_store_definition(&bytes, ALGORITHM).unwrap();
  let length = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
  let parser_start = 144 + length(64) + length(68);
  let dependencies_start = parser_start + length(72);
  let selector = encode_source_selector(SourceSelectorWriteV1::JsonPath { segments }).unwrap();
  encode_value_store_definition(
    ValueStoreDefinitionWriteV1 {
      scope_id: definition.scope_id,
      field_name: definition.field_name,
      semantic_family: definition.semantic_family,
      max_source_values_per_document: definition.max_source_values_per_document,
      max_canonical_source_bytes_per_document: definition.max_canonical_source_bytes_per_document,
      max_document_input_bytes: definition.max_document_input_bytes,
      max_selector_work_items_per_document: definition.max_selector_work_items_per_document,
      max_selector_examined_bytes_per_document: definition.max_selector_examined_bytes_per_document,
      selector: &selector,
      parser_plan: &bytes[parser_start..dependencies_start],
      dependencies: &bytes[dependencies_start..],
    },
    ALGORITHM,
  )
  .unwrap()
  .value
}

#[test]
fn source_encoding_failures_preserve_operational_class_for_both_semantic_families() {
  use aeordb::engine::file_record::FileRecord;
  use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
  use aeordb::engine::v4::index_source::{SourceDocumentV1, SourceOperationalErrorClassV1, ValueStoreRuntimeV1};
  let record = FileRecord::new("/data".into(), None, 0, Vec::new());
  for legacy in [false, true] {
    let definition = json_root_definition(legacy);
    let runtime = ValueStoreRuntimeV1::from_encoded(&definition, ALGORITHM).unwrap();
    for (value, size) in [(CanonicalConfigValueV1::Null, 5), (CanonicalConfigValueV1::Array(vec![]), 9)] {
      let input = SourceDocumentV1 { file_record: &record, parsed_value: Some(&value) };
      assert!(runtime.extract(input, None, &|| false).is_ok());
      let (result, allocations) = measure(size, || runtime.extract(input, None, &|| false));
      assert!(allocations.injected_failure, "legacy={legacy} size={size}: {allocations:?}");
      assert_eq!(result.unwrap_err().class(), SourceOperationalErrorClassV1::HostFailure);
    }
  }
  let definition = fixture("value-store-definition-v1", "avst-blake3-256-metadata-created-at-legacy-valid");
  let runtime = ValueStoreRuntimeV1::from_encoded(&definition, ALGORITHM).unwrap();
  let (result, allocations) =
    measure(13, || runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: None }, None, &|| false));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().class(), SourceOperationalErrorClassV1::HostFailure);
}

#[test]
fn canonical_allocation_origin_is_distinct_from_deterministic_bounds() {
  use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, canonical_value_to_json, encode_canonical_value};
  for (value, size) in [(CanonicalConfigValueV1::Null, 5), (CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Null]), 8)] {
    let (result, allocations) = measure(size, || encode_canonical_value(&value, CanonicalValueBounds::CONFIG));
    assert!(allocations.injected_failure);
    assert!(result.unwrap_err().is_allocation_failure());
  }
  let null = [1, 0, 0, 0, 0];
  let (result, allocations) = measure(8, || canonical_value_to_json(&null, CanonicalValueBounds::CONFIG, 1024));
  assert!(allocations.injected_failure);
  assert!(result.unwrap_err().is_allocation_failure());
  let limit = canonical_value_to_json(&null, CanonicalValueBounds::CONFIG, 1).unwrap_err();
  assert_eq!(limit.class(), MalformedInputClass::AllocationAmplification);
  assert!(!limit.is_allocation_failure());
  let value = CanonicalConfigValueV1::String("x".repeat(65537));
  let limit = encode_canonical_value(&value, CanonicalValueBounds::CONFIG).unwrap_err();
  assert_eq!(limit.class(), MalformedInputClass::AllocationAmplification);
  assert!(!limit.is_allocation_failure());
}

#[test]
fn token_workspace_refusals_are_operational_in_corrected_and_migration_converters() {
  use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
  use aeordb::engine::v4::index_converter::{ConverterRuntimeV1, IndexSemanticErrorClassV1};
  for (name, value) in [
    ("acnv-blake3-256-unicode_trigram_v1-valid", CanonicalConfigValueV1::String("a".repeat(313))),
    ("acnv-blake3-256-trigram_v0-valid", CanonicalConfigValueV1::Bytes(vec![b'a'; 313])),
  ] {
    let definition = fixture("converter-definition-v1", name);
    let converter = ConverterRuntimeV1::from_encoded(&definition, ALGORITHM).unwrap();
    converter.compile_source_value(&value).unwrap();
    let (result, allocations) = measure(313 * std::mem::size_of::<char>(), || converter.compile_source_value(&value));
    assert!(allocations.injected_failure, "{name}: {allocations:?}");
    assert_eq!(result.unwrap_err().class(), IndexSemanticErrorClassV1::HostFailure);
  }
}

#[test]
fn definition_output_reservation_refusal_is_operational() {
  use aeordb::engine::v4::index_definition_runtime::{CompiledDocumentValueV1, IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  let value = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid");
  let value_id = decode_value_store_definition(&value, ALGORITHM).unwrap().value_store_id;
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid");
  field[32..64].copy_from_slice(&value_id);
  let runtime = IndexDefinitionRuntimeV1::from_encoded(&value, &field, ALGORITHM).unwrap();
  let values = [vec![1, 0, 0, 0, 0]];
  let (result, allocations) = measure(std::mem::size_of::<CompiledDocumentValueV1>(), || runtime.compile_source_values(&values));
  assert!(allocations.injected_failure);
  let error = result.unwrap_err();
  assert_eq!(error.class(), IndexDefinitionErrorClassV1::HostFailure);
  assert_eq!(error.code(), "index_source_value_reserve");
}

#[test]
fn collector_keeps_allocation_failure_retryable_and_succeeds_after_pressure_clears() {
  use aeordb::engine::file_record::FileRecord;
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::field_definition::decode_field_index_definition;
  use aeordb::engine::v4::index_producer_collector::*;
  use aeordb::engine::v4::index_producer_coordinator::IndexProducerOwnerDispositionV1;
  use aeordb::engine::v4::scope::decode_scope_definition;
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  struct NoParser;
  impl IndexParserExecutorV1 for NoParser {
    fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
      panic!("metadata extraction must not invoke a parser");
    }
  }
  let scope = fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid");
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let mut value = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid");
  value[32..64].copy_from_slice(&scope_id);
  let value_id = decode_value_store_definition(&value, ALGORITHM).unwrap().value_store_id;
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid");
  field[32..64].copy_from_slice(&value_id);
  let field_id = decode_field_index_definition(&field, ALGORITHM).unwrap().index_id;
  let bundle = || IndexCollectorScopeDefinitionV1 {
    expected_scope_id: &scope_id,
    encoded_definition: &scope,
    value_stores: vec![IndexCollectorValueStoreDefinitionV1 {
      expected_value_store_id: &value_id,
      encoded_definition: &value,
      field_indexes: vec![IndexCollectorFieldDefinitionV1 { expected_index_id: &field_id, encoded_definition: &field }],
    }],
  };
  let memory = MemoryCoordinator::new(MemoryPolicy::new((144 << 20) - 1, 192 << 20, 1, 48 << 20).unwrap());
  let options = IndexProducerCollectorOptionsV1::new(16, 16, 16, 2 << 20, 256, 2 << 20, 50).unwrap();
  let collector = IndexProducerCollectorV1::new(ALGORITHM, memory.clone(), options).unwrap();
  let mut record = FileRecord::new("/data".into(), None, 0, Vec::new());
  record.content_hash = vec![1; 32];
  let transition = IndexCollectorDocumentTransitionV1 {
    document_ordinal: 7,
    before: None,
    after: Some(IndexCollectorDocumentV1 { namespace_root: &[2; 32], record_revision_hash: &[3; 32], file_record: &record }),
  };
  let input = bundle();
  let (result, allocations) = measure(37, || collector.collect(input, transition, &NoParser, None, &|| false));
  assert!(allocations.injected_failure, "{allocations:?}");
  let report = result.unwrap();
  for owner in [&value_id, &field_id] {
    let outcome = report.report().outcomes.iter().find(|outcome| &outcome.owner_id == owner).unwrap();
    assert!(matches!(outcome.disposition, IndexProducerOwnerDispositionV1::Retryable { .. }));
    assert!(outcome.mutations.is_empty());
    assert!(outcome.membership.is_none());
  }
  drop(report);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  let report = collector.collect(bundle(), transition, &NoParser, None, &|| false).unwrap();
  assert!(report.report().outcomes.iter().all(|outcome| matches!(outcome.disposition, IndexProducerOwnerDispositionV1::Ready)));
  drop(report);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn complete_index_definition_output_allocations_are_fallible_and_release_only_their_admission() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::field_definition::EncodedFieldIndexDefinitionV1;
  use aeordb::engine::v4::index_definition_compiler::{
    ConverterDefinitionLimitsInputV1, CorrectedIndexInputV1, FieldDefinitionLimitsInputV1, IndexDefinitionCompilationRequestV1,
    SourceDefinitionLimitsInputV1, compile_index_definitions_v1,
  };
  use aeordb::engine::v4::parser_context_compiler::{
    ParserContextCompilationRequestV1, ParserContextSourceV1, ParserSelectorDependencyV1, compile_parser_context_v1,
  };
  use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
  use aeordb::engine::v4::source_selector_compiler::{SourceSelectorCompilationRequestV1, SourceSelectorInputV1, compile_source_selector_v1};

  let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
  let source = compile_source_selector_v1(
    SourceSelectorCompilationRequestV1 {
      field_name: "@hash",
      source: SourceSelectorInputV1::Metadata,
      maximum_source_bytes: 1 << 20,
      maximum_workspace_bytes: 64 << 20,
    },
    &memory,
    &|| false,
  )
  .unwrap();
  let context = compile_parser_context_v1(
    ParserContextCompilationRequestV1 {
      source: ParserContextSourceV1::Metadata,
      selector_dependency: ParserSelectorDependencyV1::None,
      maximum_workspace_bytes: 64 << 20,
    },
    &memory,
    &|| false,
  )
  .unwrap();
  let indexes = [1, 3].map(|converter_id| CorrectedIndexInputV1 {
    converter_id,
    converter_limits: ConverterDefinitionLimitsInputV1::default(),
    field_limits: FieldDefinitionLimitsInputV1::default(),
  });
  let request = IndexDefinitionCompilationRequestV1 {
    scope_id: &[1; 32],
    source: &source,
    parser_context: &context,
    source_limits: SourceDefinitionLimitsInputV1::default(),
    indexes: &indexes,
    hash_algorithm: ALGORITHM,
    maximum_workspace_bytes: 64 << 20,
  };
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let compiled = compile_index_definitions_v1(request.clone(), &memory, &|| false).unwrap();
  let sizes = [
    2 * std::mem::size_of::<EncodedFieldIndexDefinitionV1>(),
    120,
    compiled.value_store().value.len(),
    compiled.field_indexes()[0].value.len(),
  ];
  drop(compiled);
  for size in sizes {
    let (result, allocations) = measure(size, || compile_index_definitions_v1(request.clone(), &memory, &|| false));
    assert!(allocations.injected_failure, "size={size}: {allocations:?}");
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
  let (result, allocations) = measure(0, || {
    compile_index_definitions_v1(IndexDefinitionCompilationRequestV1 { maximum_workspace_bytes: 1, ..request }, &memory, &|| false)
  });
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_preflight(allocations);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
}

#[test]
fn whole_configuration_admits_before_parsing_and_handles_real_scope_output_refusal() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::dependency::DependencyRecordV1;
  use aeordb::engine::v4::index_configuration_compiler::{
    IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1, compile_index_configuration_v1,
  };
  use aeordb::engine::v4::parser_registry_compiler::{
    ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1, compile_parser_registry_v1,
  };
  struct Snapshot;
  impl ParserAliasSnapshotV1 for Snapshot {
    fn resolve_parser_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      panic!("empty configuration must not resolve aliases")
    }
  }
  impl IndexConfigurationAliasSnapshotV1 for Snapshot {
    fn resolve_mapper_alias(&self, _: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
      panic!("empty configuration must not resolve aliases")
    }
  }
  let memory = MemoryCoordinator::new(MemoryPolicy::new(96 << 20, 128 << 20, 32 << 20, 8 << 20).unwrap());
  let registry = compile_parser_registry_v1(
    ParserRegistryCompilationRequestV1 {
      source: None,
      hash_algorithm: ALGORITHM,
      maximum_source_bytes: 1024,
      maximum_workspace_bytes: 32 << 20,
    },
    &Snapshot,
    &memory,
    &|| false,
  )
  .unwrap();
  let baseline = memory.snapshot().unwrap().reserved_bytes;
  let request = IndexConfigurationCompilationRequestV1 {
    source: br#"{"$v":1,"indexes":[]}"#,
    owner_path: "/",
    registry: &registry,
    hash_algorithm: ALGORITHM,
    maximum_source_bytes: 1024,
    maximum_workspace_bytes: 64 << 20,
  };
  let compiled = compile_index_configuration_v1(request, &Snapshot, &memory, &|| false).unwrap();
  assert_eq!(compiled.scope().value.len(), 65);
  drop(compiled);
  let (result, allocations) = measure(65, || compile_index_configuration_v1(request, &Snapshot, &memory, &|| false));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  let oversized_owner = "/a".repeat((5 << 20) / 2);
  for input in [
    IndexConfigurationCompilationRequestV1 { maximum_source_bytes: 1, ..request },
    IndexConfigurationCompilationRequestV1 { maximum_workspace_bytes: 1, ..request },
    IndexConfigurationCompilationRequestV1 { owner_path: &oversized_owner, ..request },
  ] {
    let (result, allocations) = measure(0, || compile_index_configuration_v1(input, &Snapshot, &memory, &|| false));
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
    assert_preflight(allocations);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
  }
}

#[test]
fn frozen_compiler_profile_lookup_never_allocates_or_reads_runtime_files() {
  use aeordb::engine::v4::semantic_compiler_profile::semantic_compiler_fingerprint_v1;
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    for fail_size in [1, 32, 64] {
      let (fingerprint, allocations) = measure(fail_size, || semantic_compiler_fingerprint_v1(algorithm));
      assert_eq!(fingerprint.len(), algorithm.hash_length());
      assert_eq!(allocations.total, 0);
      assert_eq!(allocations.maximum, 0);
      assert!(!allocations.injected_failure);
    }
  }
}

fn assert_runtime_constructor_refusal(legacy: bool, value: &[u8], fail_size: usize) {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::index_definition_runtime::{IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
  use aeordb::engine::v4::index_source::{SourceOperationalErrorClassV1, ValueStoreRuntimeV1};
  use aeordb::engine::v4::source_evaluator::{
    AuthoritativeSourceEvaluationErrorV1, AuthoritativeSourceEvaluatorV1, AuthoritativeSourceMemoryPolicyV1,
  };
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  let definition = decode_value_store_definition(value, ALGORITHM).unwrap();
  let mut field = fixture(
    "field-index-definition-v1",
    if legacy { "afix-blake3-256-hash_v0-valid" } else { "afix-blake3-256-typed_exact_blake3_v1-valid" },
  );
  field[32..64].copy_from_slice(&definition.value_store_id);
  ValueStoreRuntimeV1::from_encoded(value, ALGORITHM).unwrap();
  let (result, allocations) = measure(fail_size, || ValueStoreRuntimeV1::from_encoded(value, ALGORITHM));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().class(), SourceOperationalErrorClassV1::HostFailure);

  IndexDefinitionRuntimeV1::from_encoded(value, &field, ALGORITHM).unwrap();
  let (result, allocations) = measure(fail_size, || IndexDefinitionRuntimeV1::from_encoded(value, &field, ALGORITHM));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().class(), IndexDefinitionErrorClassV1::HostFailure);
  IndexDefinitionRuntimeV1::from_encoded(value, &field, ALGORITHM).unwrap();

  // The retained legacy fixture intentionally has unlimited semantic sentinels;
  // it is not a supported source-execution workspace. Both retained runtimes
  // above still have the same operational construction obligation.
  if legacy {
    return;
  }
  let memory = MemoryCoordinator::new(MemoryPolicy::new(144 << 20, 192 << 20, 1, 16 << 20).unwrap());
  for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
    let build = || {
      AuthoritativeSourceEvaluatorV1::from_encoded(
        value,
        ALGORITHM,
        definition.scope_id,
        &definition.value_store_id,
        memory.clone(),
        policy,
      )
    };
    drop(build().unwrap());
    let (result, allocations) = measure(fail_size, build);
    assert!(allocations.injected_failure, "{allocations:?}");
    match result {
      Err(AuthoritativeSourceEvaluationErrorV1::Source(source)) => assert_eq!(source.class(), SourceOperationalErrorClassV1::HostFailure),
      Err(AuthoritativeSourceEvaluationErrorV1::ResourcePressure(_)) => {}
      Err(error) => panic!("allocation failure changed into {error}"),
      Ok(_) => panic!("constructor unexpectedly succeeded after allocation refusal"),
    }
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    drop(build().unwrap());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn selector_decoder_allocation_refusal_remains_operational_for_runtime_construction() {
  use aeordb::engine::v4::source_selector::{JsonPathSegmentV1, SourceSelectorWriteV1, decode_source_selector, encode_source_selector};
  let segments = [JsonPathSegmentV1::ObjectKey("x")];
  let selector = encode_source_selector(SourceSelectorWriteV1::JsonPath { segments: &segments }).unwrap();
  let fail_size = std::mem::size_of::<JsonPathSegmentV1<'_>>();
  let (result, allocations) = measure(fail_size, || decode_source_selector(&selector));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(result.unwrap_err().is_allocation_failure());
  for legacy in [false, true] {
    assert_runtime_constructor_refusal(legacy, &json_selector_definition(legacy, &segments), fail_size);
  }
}

#[test]
fn selector_key_allocation_refusal_remains_operational_for_runtime_construction() {
  use aeordb::engine::v4::source_selector::JsonPathSegmentV1;
  let key = "k".repeat(4093);
  let segments = [JsonPathSegmentV1::ObjectKey(&key)];
  for legacy in [false, true] {
    assert_runtime_constructor_refusal(legacy, &json_selector_definition(legacy, &segments), key.len());
  }
}

#[test]
fn collector_constructor_allocation_refusal_discards_partial_report_and_can_retry() {
  use aeordb::engine::file_record::FileRecord;
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::field_definition::decode_field_index_definition;
  use aeordb::engine::v4::index_producer_collector::*;
  use aeordb::engine::v4::index_producer_coordinator::IndexProducerOwnerDispositionV1;
  use aeordb::engine::v4::scope::decode_scope_definition;
  use aeordb::engine::v4::source_selector::JsonPathSegmentV1;
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  struct Parser;
  impl IndexParserExecutorV1 for Parser {
    fn parse(&self, _: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
      Ok(IndexParserOutcomeV1::NotApplicable)
    }
  }
  let scope = fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid");
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let key = "k".repeat(4093);
  let mut value = json_selector_definition(false, &[JsonPathSegmentV1::ObjectKey(&key)]);
  value[32..64].copy_from_slice(&scope_id);
  let value_id = decode_value_store_definition(&value, ALGORITHM).unwrap().value_store_id;
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid");
  field[32..64].copy_from_slice(&value_id);
  let field_id = decode_field_index_definition(&field, ALGORITHM).unwrap().index_id;
  let bundle = || IndexCollectorScopeDefinitionV1 {
    expected_scope_id: &scope_id,
    encoded_definition: &scope,
    value_stores: vec![IndexCollectorValueStoreDefinitionV1 {
      expected_value_store_id: &value_id,
      encoded_definition: &value,
      field_indexes: vec![IndexCollectorFieldDefinitionV1 { expected_index_id: &field_id, encoded_definition: &field }],
    }],
  };
  let memory = MemoryCoordinator::new(MemoryPolicy::new(144 << 20, 192 << 20, 1, 16 << 20).unwrap());
  let collector = IndexProducerCollectorV1::new(
    ALGORITHM,
    memory.clone(),
    IndexProducerCollectorOptionsV1::new(16, 16, 16, 2 << 20, 256, 2 << 20, 50).unwrap(),
  )
  .unwrap();
  let record = FileRecord::new("/data.json".into(), None, 0, Vec::new());
  let transition = IndexCollectorDocumentTransitionV1 {
    document_ordinal: 7,
    before: None,
    after: Some(IndexCollectorDocumentV1 { namespace_root: &[2; 32], record_revision_hash: &[3; 32], file_record: &record }),
  };
  drop(collector.collect(bundle(), transition, &Parser, None, &|| false).unwrap());
  for (fail_size, occurrence) in [(key.len(), 1), (key.len(), 2), (std::mem::size_of::<JsonPathSegmentV1<'_>>(), 1)] {
    let input = bundle();
    let (result, allocations) = measure_nth(fail_size, occurrence, || collector.collect(input, transition, &Parser, None, &|| false));
    assert!(allocations.injected_failure, "{allocations:?}");
    assert!(matches!(result, Err(IndexProducerCollectorErrorV1::ResourcePressure(_))));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    let report = collector.collect(bundle(), transition, &Parser, None, &|| false).unwrap();
    assert!(report.report().outcomes.iter().all(|outcome| matches!(outcome.disposition, IndexProducerOwnerDispositionV1::Ready)));
    drop(report);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn malformed_runtime_definitions_remain_invalid_not_resource_failures() {
  use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
  use aeordb::engine::v4::index_definition_runtime::{IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
  use aeordb::engine::v4::source_evaluator::{
    AuthoritativeSourceEvaluationErrorV1, AuthoritativeSourceEvaluatorV1, AuthoritativeSourceMemoryPolicyV1,
  };
  use aeordb::engine::v4::value_store::decode_value_store_definition;
  let value = json_root_definition(false);
  let definition = decode_value_store_definition(&value, ALGORITHM).unwrap();
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid");
  field[32..64].copy_from_slice(&definition.value_store_id);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(144 << 20, 192 << 20, 1, 16 << 20).unwrap());
  let mut wrong_magic = value.clone();
  wrong_magic[0] = 0;
  for malformed in [&wrong_magic[..], &value[..value.len() - 1]] {
    assert!(!decode_value_store_definition(malformed, ALGORITHM).unwrap_err().is_allocation_failure());
    assert_eq!(
      IndexDefinitionRuntimeV1::from_encoded(malformed, &field, ALGORITHM).unwrap_err().class(),
      IndexDefinitionErrorClassV1::UnsupportedDefinition
    );
    for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
      let result = AuthoritativeSourceEvaluatorV1::from_encoded(
        malformed,
        ALGORITHM,
        definition.scope_id,
        &definition.value_store_id,
        memory.clone(),
        policy,
      );
      assert!(matches!(result, Err(AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration { .. })));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
  field[0] = 0;
  assert_eq!(
    IndexDefinitionRuntimeV1::from_encoded(&value, &field, ALGORITHM).unwrap_err().class(),
    IndexDefinitionErrorClassV1::UnsupportedDefinition
  );
}
