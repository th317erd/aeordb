use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::config_value::{CanonicalValueBounds, validate_canonical_value};
use aeordb::engine::v4::database_header::decode_header_region;
use aeordb::engine::v4::dependency::{decode_dependency_table, decode_invocation_policy};
use aeordb::engine::v4::entity::decode_whole_entity;
use aeordb::engine::v4::field_definition::{decode_converter_definition, decode_field_index_definition};
use aeordb::engine::v4::gc::decode_gc_active_control;
use aeordb::engine::v4::gc_audit::decode_audit_artifact;
use aeordb::engine::v4::gc_mark::{decode_gc_mark_artifact, decode_mark_workspace_manifest, decode_mark_workspace_object};
use aeordb::engine::v4::gc_state::decode_gc_state_artifact;
use aeordb::engine::v4::gc_void::decode_sweep_void_artifact;
use aeordb::engine::v4::index_artifact::decode_index_control_or_manifest;
use aeordb::engine::v4::index_nvt::decode_nvt_tile;
use aeordb::engine::v4::index_page::decode_ordered_index_artifact;
use aeordb::engine::v4::index_task::decode_index_task_artifact;
use aeordb::engine::v4::migration_capture::decode_migration_capture_manifest;
use aeordb::engine::v4::namespace::{decode_namespace_root, decode_semantic_object};
use aeordb::engine::v4::parser_plan::decode_parser_resolution_plan;
use aeordb::engine::v4::position::decode_logical_position;
use aeordb::engine::v4::reader::FormatError;
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::source_selector::decode_source_selector;
use aeordb::engine::v4::system_control::{decode_system_control, select_cutover_journal};
use aeordb::engine::v4::system_family::decode_system_family_registry;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct FixtureManifest {
  fixtures: Vec<FixtureRow>,
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureRow {
  id: String,
  format_id: String,
  hash_algorithm: String,
  binary: String,
  expected: String,
}

struct MeasuringAllocator;

thread_local! {
  static MEASURE_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
  static ALLOCATED_BYTES: Cell<usize> = const { Cell::new(0) };
}

#[global_allocator]
static GLOBAL_ALLOCATOR: MeasuringAllocator = MeasuringAllocator;

unsafe impl GlobalAlloc for MeasuringAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let pointer = unsafe { System.alloc(layout) };
    if !pointer.is_null() {
      record_allocation(layout.size());
    }
    pointer
  }

  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    let pointer = unsafe { System.alloc_zeroed(layout) };
    if !pointer.is_null() {
      record_allocation(layout.size());
    }
    pointer
  }

  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    unsafe { System.dealloc(pointer, layout) };
  }

  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
    if !new_pointer.is_null() {
      record_allocation(new_size);
    }
    new_pointer
  }
}

fn record_allocation(bytes: usize) {
  MEASURE_ALLOCATIONS.with(|enabled| {
    if enabled.get() {
      ALLOCATED_BYTES.with(|total| total.set(total.get().saturating_add(bytes)));
    }
  });
}

struct AllocationMeasurement;

impl AllocationMeasurement {
  fn begin() -> Self {
    ALLOCATED_BYTES.with(|total| total.set(0));
    MEASURE_ALLOCATIONS.with(|enabled| enabled.set(true));
    Self
  }

  fn allocated_bytes(&self) -> usize {
    ALLOCATED_BYTES.with(Cell::get)
  }
}

impl Drop for AllocationMeasurement {
  fn drop(&mut self) {
    MEASURE_ALLOCATIONS.with(|enabled| enabled.set(false));
  }
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn manifest() -> FixtureManifest {
  serde_json::from_slice(&fs::read(fixture_root().join("format-fixture-manifest.json")).unwrap()).unwrap()
}

#[test]
fn every_manifest_row_has_one_production_decoder() {
  let root = fixture_root();
  let rows = manifest().fixtures;
  assert_eq!(rows.len(), 440);
  for row in rows {
    let bytes = fs::read(root.join(&row.binary)).unwrap();
    let decoded = decode_fixture_row(&row, &bytes).unwrap_or_else(|error| panic!("fixture {}: {error}", row.id));
    if let Some(expected_code) = row.expected.strip_prefix("error:") {
      let error = match decoded {
        Ok(()) => panic!("fixture {} should reject as {expected_code}", row.id),
        Err(error) => error,
      };
      assert_eq!(error.code(), expected_code, "fixture {}", row.id);
    } else {
      decoded.unwrap_or_else(|error| panic!("fixture {} unexpectedly rejected: {error}", row.id));
    }
  }
}

#[test]
fn deterministic_mutation_corpus_is_bounded_and_watchdog_protected() {
  let (request_tx, request_rx) = mpsc::channel::<MutationRequest>();
  let (result_tx, result_rx) = mpsc::channel::<MutationResult>();
  let worker = thread::spawn(move || mutation_worker(request_rx, result_tx));
  let root = fixture_root();
  let rows = manifest().fixtures;
  let mut case_count = 0usize;

  for row in rows {
    let original = fs::read(root.join(&row.binary)).unwrap();
    let mut truncations = BTreeSet::from([0, 1.min(original.len()), original.len() / 2, original.len().saturating_sub(1)]);
    truncations.retain(|length| *length < original.len());
    for length in truncations {
      run_mutation_case(
        &request_tx,
        &result_rx,
        MutationRequest::new(row.clone(), format!("truncate-{length}"), original[..length].to_vec(), true),
      );
      case_count += 1;
    }

    let mut trailing = original.clone();
    trailing.push(0);
    run_mutation_case(&request_tx, &result_rx, MutationRequest::new(row.clone(), "trailing-byte", trailing, true));
    case_count += 1;

    for offset in sampled_offsets(original.len(), 32) {
      let mut changed = original.clone();
      changed[offset] ^= 1 << (offset % 8);
      run_mutation_case(&request_tx, &result_rx, MutationRequest::new(row.clone(), format!("bit-flip-{offset}"), changed, false));
      case_count += 1;
    }

    for offset in sampled_window_offsets(original.len(), 4, 8) {
      let mut changed = original.clone();
      changed[offset..offset + 4].fill(0xff);
      run_mutation_case(&request_tx, &result_rx, MutationRequest::new(row.clone(), format!("u32-max-{offset}"), changed, false));
      case_count += 1;
    }

    for offset in sampled_window_offsets(original.len(), 8, 4) {
      let mut changed = original.clone();
      changed[offset..offset + 8].fill(0xff);
      run_mutation_case(&request_tx, &result_rx, MutationRequest::new(row.clone(), format!("u64-max-{offset}"), changed, false));
      case_count += 1;
    }
  }

  drop(request_tx);
  worker.join().expect("mutation worker panicked");
  assert!(case_count >= 20_000, "mutation corpus unexpectedly shrank to {case_count} cases");
  eprintln!("v4 deterministic mutation corpus: {case_count} bounded cases");
}

#[derive(Debug)]
struct MutationRequest {
  row: FixtureRow,
  mutation: String,
  bytes: Vec<u8>,
  must_reject: bool,
}

impl MutationRequest {
  fn new(row: FixtureRow, mutation: impl Into<String>, bytes: Vec<u8>, must_reject: bool) -> Self {
    Self { row, mutation: mutation.into(), bytes, must_reject }
  }
}

#[derive(Debug)]
struct MutationResult {
  case: String,
  accepted: bool,
  allocation_bytes: usize,
  elapsed: Duration,
  panic: Option<String>,
  route_error: Option<String>,
}

fn mutation_worker(requests: mpsc::Receiver<MutationRequest>, results: mpsc::Sender<MutationResult>) {
  for request in requests {
    let case = format!("{}:{}", request.row.id, request.mutation);
    let started = Instant::now();
    let measurement = AllocationMeasurement::begin();
    let decoded = catch_unwind(AssertUnwindSafe(|| decode_fixture_row(&request.row, &request.bytes)));
    let allocation_bytes = measurement.allocated_bytes();
    drop(measurement);
    let elapsed = started.elapsed();
    let (accepted, panic, route_error) = match decoded {
      Ok(Ok(result)) => (result.is_ok(), None, None),
      Ok(Err(error)) => (false, None, Some(error)),
      Err(payload) => {
        let message = payload
          .downcast_ref::<&str>()
          .map(|value| (*value).to_string())
          .or_else(|| payload.downcast_ref::<String>().cloned())
          .unwrap_or_else(|| "non-string panic payload".to_string());
        (false, Some(message), None)
      }
    };
    let result = MutationResult { case, accepted, allocation_bytes, elapsed, panic, route_error };
    if results.send(result).is_err() {
      return;
    }
  }
}

fn run_mutation_case(sender: &mpsc::Sender<MutationRequest>, receiver: &mpsc::Receiver<MutationResult>, request: MutationRequest) {
  const CASE_TIMEOUT: Duration = Duration::from_secs(1);
  const FIXED_ALLOCATION_ALLOWANCE: usize = 512 * 1_024;
  let must_reject = request.must_reject;
  let input_length = request.bytes.len();
  let allocation_ceiling = input_length.saturating_mul(2).saturating_add(FIXED_ALLOCATION_ALLOWANCE);
  let expected_case = format!("{}:{}", request.row.id, request.mutation);
  sender.send(request).unwrap();
  let result = receiver.recv_timeout(CASE_TIMEOUT).unwrap_or_else(|_| panic!("mutation case {expected_case} exceeded {CASE_TIMEOUT:?}"));
  assert_eq!(result.case, expected_case);
  assert!(result.panic.is_none(), "mutation case {} panicked: {:?}", result.case, result.panic);
  assert!(result.route_error.is_none(), "mutation case {} lost its decoder route: {:?}", result.case, result.route_error);
  assert!(result.elapsed <= CASE_TIMEOUT, "mutation case {} ran for {:?}", result.case, result.elapsed);
  assert!(
    result.allocation_bytes <= allocation_ceiling,
    "mutation case {} allocated {} bytes, ceiling {} for {} input bytes",
    result.case,
    result.allocation_bytes,
    allocation_ceiling,
    input_length
  );
  if must_reject {
    assert!(!result.accepted, "malformed mutation case {} was accepted", result.case);
  }
}

fn sampled_offsets(length: usize, maximum_samples: usize) -> Vec<usize> {
  if length == 0 {
    return Vec::new();
  }
  if length <= maximum_samples {
    return (0..length).collect();
  }
  (0..maximum_samples).map(|index| index * (length - 1) / (maximum_samples - 1)).collect::<BTreeSet<_>>().into_iter().collect()
}

fn sampled_window_offsets(length: usize, width: usize, maximum_samples: usize) -> Vec<usize> {
  if length < width {
    return Vec::new();
  }
  sampled_offsets(length - width + 1, maximum_samples)
}

fn decode_fixture_row(row: &FixtureRow, bytes: &[u8]) -> Result<Result<(), FormatError>, String> {
  let algorithm = hash_algorithm(&row.hash_algorithm)?;
  let decoded = match row.format_id.as_str() {
    "database-header-v4" => decode_header_region(bytes).map(|_| ()),
    "whole-entity-v1" => decode_whole_entity(bytes, algorithm, u64::MAX).map(|_| ()),
    "directory-index-v1" => decode_namespace_root(bytes, algorithm).map(|_| ()),
    "semantic-object-v1" => decode_semantic_object(bytes, algorithm).map(|_| ()),
    "canonical-config-value-v1" => validate_canonical_value(bytes, CanonicalValueBounds::CONFIG).map(|_| ()),
    "invocation-policy-v1" => decode_invocation_policy(bytes).map(|_| ()),
    "dependency-table-v1" => decode_dependency_table(bytes).map(|_| ()),
    "scope-definition-v1" => decode_scope_definition(bytes, algorithm).map(|_| ()),
    "parser-resolution-plan-v1" => decode_parser_resolution_plan(bytes).map(|_| ()),
    "source-selector-v1" => decode_source_selector(bytes).map(|_| ()),
    "value-store-definition-v1" => decode_value_store_definition(bytes, algorithm).map(|_| ()),
    "converter-definition-v1" => decode_converter_definition(bytes, algorithm).map(|_| ()),
    "field-index-definition-v1" => decode_field_index_definition(bytes, algorithm).map(|_| ()),
    "index-artifact-v1" if row.expected.starts_with("index:pointer:") || row.expected.starts_with("index:manifest:") => {
      decode_index_control_or_manifest(bytes, algorithm).map(|_| ())
    }
    "index-artifact-v1" if row.expected.starts_with("index:page:") || row.expected.starts_with("index:directory:") => {
      decode_ordered_index_artifact(bytes, algorithm).map(|_| ())
    }
    "index-artifact-v1" if row.expected.starts_with("index:nvt-tile:") => decode_nvt_tile(bytes, algorithm).map(|_| ()),
    "index-artifact-v1" if row.expected.starts_with("index:journal:") || row.expected.starts_with("index:checkpoint:") => {
      decode_index_task_artifact(bytes, algorithm).map(|_| ())
    }
    "logical-position-v1" => decode_logical_position(bytes, algorithm).map(|_| ()),
    "migration-capture-v1" => decode_migration_capture_manifest(bytes, algorithm).map(|_| ()),
    "gc-artifact-v1" if row.expected.starts_with("gc:control:") => decode_gc_active_control(bytes, algorithm).map(|_| ()),
    "gc-artifact-v1" if is_gc_state_fixture(row) => decode_gc_state_artifact(bytes, algorithm).map(|_| ()),
    "gc-artifact-v1" if is_gc_mark_fixture(row) => decode_gc_mark_artifact(bytes, algorithm).map(|_| ()),
    "gc-artifact-v1" if is_sweep_void_fixture(row) => decode_sweep_void_artifact(bytes, algorithm).map(|_| ()),
    "gc-artifact-v1" if is_gc_audit_fixture(row) => decode_audit_artifact(bytes, algorithm).map(|_| ()),
    "gc-mark-workspace-manifest-v1" => decode_mark_workspace_manifest(bytes, algorithm).map(|_| ()),
    "gc-mark-workspace-object-v1" => decode_mark_workspace_object(bytes, algorithm).map(|_| ()),
    "system-control-v1" => decode_system_control(bytes, algorithm).map(|_| ()),
    "cutover-journal-v1" => select_cutover_journal(bytes, algorithm).map(|_| ()),
    "system-family-registry-v1" => decode_system_family_registry(bytes, algorithm).map(|_| ()),
    format => return Err(format!("no production decoder route for {format} ({})", row.expected)),
  };
  Ok(decoded)
}

fn hash_algorithm(name: &str) -> Result<HashAlgorithm, String> {
  match name {
    "blake3-256" => Ok(HashAlgorithm::Blake3_256),
    "sha512" => Ok(HashAlgorithm::Sha512),
    other => Err(format!("unsupported fixture hash algorithm {other}")),
  }
}

fn is_gc_state_fixture(row: &FixtureRow) -> bool {
  [
    "gc:page:candidate:",
    "gc:page:root-expiry:",
    "gc:page:root-candidate:",
    "gc:page:physical-inventory:",
    "gc:directory:candidates:",
    "gc:directory:root-expiry:",
    "gc:directory:root-candidates:",
    "gc:directory:physical-inventory:",
    "gc:delta:candidate:",
    "gc:manifest:root-expiry:",
    "gc:manifest:root-lifecycle:",
    "gc:manifest:physical-inventory:",
    "gc:manifest:quarantine:",
    "gc:commit:root-retirement:",
    "gc:proof:root-object-reclaim:",
    "gc:journal:retirement:",
  ]
  .iter()
  .any(|prefix| row.expected.starts_with(prefix))
}

fn is_gc_mark_fixture(row: &FixtureRow) -> bool {
  row.expected.starts_with("gc:checkpoint:mark-run:") || row.expected.starts_with("gc:journal:mark-mutation:")
}

fn is_sweep_void_fixture(row: &FixtureRow) -> bool {
  [
    "gc:proposal:sweep:",
    "gc:receipt:sweep-",
    "gc:page:void-free-extents:",
    "gc:directory:void-",
    "gc:manifest:void-catalog:",
    "gc:claim:void:",
    "gc:receipt:void-claim-settlement:",
  ]
  .iter()
  .any(|prefix| row.expected.starts_with(prefix))
}

fn is_gc_audit_fixture(row: &FixtureRow) -> bool {
  ["gc:manifest:audit-catalog:", "gc:page:audit-", "gc:directory:audit-", "gc:summary:run:", "gc:evidence:corrupt:", "gc:pin:audit:"]
    .iter()
    .any(|prefix| row.expected.starts_with(prefix))
}
