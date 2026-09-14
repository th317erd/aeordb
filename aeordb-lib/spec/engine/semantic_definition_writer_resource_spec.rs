//! Measured preflight and fallible-output allocation for canonical writers.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use aeordb::engine::v4::dependency::{encode_dependency_record, encode_dependency_table};
use aeordb::engine::v4::field_definition::{
  encode_converter_definition, encode_field_index_definition, ConverterDefinitionWriteV1, FieldIndexDefinitionWriteV1,
};
use aeordb::engine::v4::native_semantics::NativeSemanticComponentV1;
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
