//! Corrected source compilation, not a generic JSON canonicalization round trip.
use std::cell::{Cell, RefCell};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::dependency::DependencyRecordV1;
use aeordb::engine::v4::namespace::decode_semantic_definition_record;
use aeordb::engine::v4::parser_registry_compiler::{
  CompiledParserRegistryV1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1,
  compile_parser_registry_v1,
};
use sha2::Digest;

const WORKSPACE: usize = 32 * 1024 * 1024;
const ALGORITHMS: [HashAlgorithm; 5] =
  [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512];

fn dependency() -> DependencyRecordV1<'static> {
  DependencyRecordV1 {
    kind: 1,
    role: 1,
    flags: 4,
    abi: 3,
    executor_profile: 2,
    fingerprint_semantics: 1,
    artifact_kind: 1,
    artifact_length: 123,
    fingerprint: [0x42; 32],
    dependency_id: "/org/example/parser",
    version: "1.2.3",
  }
}

struct Snapshot<'a> {
  record: DependencyRecordV1<'a>,
  calls: RefCell<Vec<String>>,
  fail: bool,
  cancelled: Cell<bool>,
  cancel_on_lookup: bool,
  memory: Option<MemoryCoordinator>,
}

impl Default for Snapshot<'_> {
  fn default() -> Self {
    Self {
      record: dependency(),
      calls: RefCell::new(Vec::new()),
      fail: false,
      cancelled: Cell::new(false),
      cancel_on_lookup: false,
      memory: None,
    }
  }
}

impl ParserAliasSnapshotV1 for Snapshot<'_> {
  fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.calls.borrow_mut().push(alias.to_string());
    if self.cancel_on_lookup {
      self.cancelled.set(true);
    }
    if let Some(memory) = &self.memory {
      memory.reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
    }
    if self.fail {
      return Err(SemanticCompilationErrorV1::Operational { path: "/snapshot", message: "injected read failure".into() });
    }
    Ok((alias != "missing").then(|| self.record.clone()))
  }
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(96 * 1024 * 1024, 128 * 1024 * 1024, 32 * 1024 * 1024, 8 * 1024 * 1024).unwrap())
}

fn request(source: Option<&[u8]>, algorithm: HashAlgorithm) -> ParserRegistryCompilationRequestV1<'_> {
  ParserRegistryCompilationRequestV1 {
    source,
    hash_algorithm: algorithm,
    maximum_source_bytes: WORKSPACE,
    maximum_workspace_bytes: WORKSPACE,
  }
}

fn compile(
  source: Option<&[u8]>,
  algorithm: HashAlgorithm,
  snapshot: &Snapshot<'_>,
) -> Result<CompiledParserRegistryV1, SemanticCompilationErrorV1> {
  compile_parser_registry_v1(request(source, algorithm), snapshot, &memory(), &|| snapshot.cancelled.get())
}

fn source(entries: &[(&str, &str)]) -> Vec<u8> {
  let members: Vec<_> = entries
    .iter()
    .map(|(key, alias)| format!("{}:{}", serde_json::to_string(key).unwrap(), serde_json::to_string(alias).unwrap()))
    .collect();
  format!("{{\"$v\":1,\"parsers\":{{{}}}}}", members.join(",")).into_bytes()
}

fn digest(algorithm: HashAlgorithm, bytes: &[u8]) -> Vec<u8> {
  match algorithm {
    HashAlgorithm::Blake3_256 => blake3::hash(bytes).as_bytes().to_vec(),
    HashAlgorithm::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
    HashAlgorithm::Sha512 => sha2::Sha512::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_256 => sha3::Sha3_256::digest(bytes).to_vec(),
    HashAlgorithm::Sha3_512 => sha3::Sha3_512::digest(bytes).to_vec(),
  }
}

fn record_bytes(record: &DependencyRecordV1<'_>) -> Vec<u8> {
  // Independent Round9 record framing, not the production encoder.
  let mut bytes = vec![0; 96];
  bytes[..4].copy_from_slice(&((96 + record.dependency_id.len() + record.version.len()) as u32).to_le_bytes());
  for (offset, value) in [
    (4, record.kind),
    (6, record.role),
    (12, record.abi),
    (14, record.executor_profile),
    (16, record.fingerprint_semantics),
    (18, record.artifact_kind),
  ] {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
  }
  bytes[8..12].copy_from_slice(&record.flags.to_le_bytes());
  bytes[20..24].copy_from_slice(&(record.dependency_id.len() as u32).to_le_bytes());
  bytes[24..28].copy_from_slice(&(record.version.len() as u32).to_le_bytes());
  bytes[32..40].copy_from_slice(&record.artifact_length.to_le_bytes());
  bytes[40..72].copy_from_slice(&record.fingerprint);
  bytes.extend_from_slice(record.dependency_id.as_bytes());
  bytes.extend_from_slice(record.version.as_bytes());
  bytes
}

fn projection(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
  let mut payload = (entries.len() as u32).to_le_bytes().to_vec();
  for (essence, record) in entries {
    payload.extend_from_slice(&(essence.len() as u32).to_le_bytes());
    payload.extend_from_slice(essence.as_bytes());
    payload.push(8);
    payload.extend_from_slice(&(record.len() as u32).to_le_bytes());
    payload.extend_from_slice(record);
  }
  let mut bytes = vec![10];
  bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&payload);
  bytes
}

#[test]
fn absent_and_explicit_empty_sources_compile_to_the_same_independent_projection_for_every_hash() {
  for algorithm in ALGORITHMS {
    let snapshot = Snapshot::default();
    let missing = compile(None, algorithm, &snapshot).unwrap();
    let empty = compile(Some(&source(&[])), algorithm, &snapshot).unwrap();
    let expected = projection(&[]);
    assert!(missing.entries().is_empty());
    assert_eq!(missing.projection().object.value, empty.projection().object.value);
    assert_eq!(
      missing.projection().semantic_id,
      digest(algorithm, &[b"aeordb.semantic.parser-registry-projection.v1\0".as_slice(), &expected].concat())
    );
    let decoded = decode_semantic_definition_record(&missing.projection().object.value, algorithm).unwrap();
    assert_eq!(decoded.class, 2);
    assert_eq!(decoded.definition, expected);
    assert!(snapshot.calls.borrow().is_empty());
  }
}

#[test]
fn alias_order_formatting_and_mime_case_compile_to_identical_exact_dependency_pins() {
  for algorithm in ALGORITHMS {
    let snapshot = Snapshot::default();
    let first = compile(Some(br#"{ "parsers": {"TEXT/PLAIN":"old", "application/pdf":"second"}, "$v":1 }"#), algorithm, &snapshot).unwrap();
    let second = compile(Some(&source(&[("application/pdf", "new"), ("text/plain", "renamed")])), algorithm, &snapshot).unwrap();
    assert_eq!(first.projection().object.value, second.projection().object.value);
    let record = record_bytes(&snapshot.record);
    let expected = projection(&[("application/pdf", record.clone()), ("text/plain", record.clone())]);
    assert_eq!(decode_semantic_definition_record(&first.projection().object.value, algorithm).unwrap().definition, expected);
    assert_eq!(
      first.projection().semantic_id,
      digest(algorithm, &[b"aeordb.semantic.parser-registry-projection.v1\0".as_slice(), &expected].concat())
    );
    assert_eq!(
      first.projection().object.object_id,
      digest(
        algorithm,
        &[b"aeordb.semantic-object.immutable.v1\0".as_slice(), &4u16.to_le_bytes(), &first.projection().object.value].concat()
      )
    );
    assert_eq!(first.entries().len(), 2);
    assert_eq!(first.entries()[0].essence(), "application/pdf");
    assert_eq!(first.entries()[0].dependency_bytes(), record);
  }
}

#[test]
fn every_corrected_dependency_identity_change_changes_the_projection() {
  let bytes = source(&[("text/plain", "alias")]);
  let baseline = compile(Some(&bytes), HashAlgorithm::Sha512, &Snapshot::default()).unwrap();
  for field in 0..4 {
    let mut snapshot = Snapshot::default();
    match field {
      0 => snapshot.record.fingerprint[31] ^= 1,
      1 => snapshot.record.artifact_length += 1,
      2 => snapshot.record.dependency_id = "/org/example/other",
      3 => snapshot.record.version = "1.2.4",
      _ => unreachable!(),
    }
    let changed = compile(Some(&bytes), HashAlgorithm::Sha512, &snapshot).unwrap();
    assert_ne!(baseline.projection().semantic_id, changed.projection().semantic_id, "field {field}");
  }
  let changed = compile(Some(&source(&[("text/html", "alias")])), HashAlgorithm::Sha512, &Snapshot::default()).unwrap();
  assert_ne!(baseline.projection().semantic_id, changed.projection().semantic_id);
}

#[test]
fn malformed_sources_never_become_empty_or_trigger_dependency_lookup() {
  let invalid: &[&[u8]] = &[
    b"",
    b"null",
    b"[]",
    b"{}",
    b"{",
    b"\xff",
    br#"{"text/plain":"v0"}"#,
    br#"{"$v":0,"parsers":{}}"#,
    br#"{"$v":2,"parsers":{}}"#,
    br#"{"$v":1.0,"parsers":{}}"#,
    br#"{"$v":"1","parsers":{}}"#,
    br#"{"$v":1,"parsers":null}"#,
    br#"{"$v":1,"parsers":[]}"#,
    br#"{"$v":1,"parsers":{"text/plain":3}}"#,
    br#"{"$v":1,"parsers":{},"logging":true}"#,
    br#"{"$v":1,"$v":1,"parsers":{}}"#,
    br#"{"$v":1,"parsers":{},"parsers":{}}"#,
    br#"{"$v":1,"parsers":{}} {}"#,
    br#"{"$v":1,"parsers":{"text/plain":"a","text/plain":"a"}}"#,
    br#"{"$v":1,"parsers":{"TEXT/PLAIN":"a","text/plain":"b"}}"#,
    br#"{"$v":1,"parsers":{"text/pl\u0061in":"a","text/plain":"b"}}"#,
  ];
  for bytes in invalid {
    let snapshot = Snapshot::default();
    let error = compile(Some(bytes), HashAlgorithm::Blake3_256, &snapshot).err().expect("invalid source");
    assert!(matches!(error, SemanticCompilationErrorV1::InvalidSource { .. }), "{bytes:?}: {error}");
    assert!(error.to_string().contains("/.aeordb-config/parsers.json"));
    assert!(snapshot.calls.borrow().is_empty());
  }
}

#[test]
fn invalid_mime_keys_and_reserved_json_require_an_explicit_scope_parser() {
  for key in [
    "",
    "text",
    "/plain",
    "text/",
    "*/plain",
    "text/*",
    "text/pläin",
    "text/plain; charset=utf-8",
    "text/plain;",
    "text/plain\n",
    "text /plain",
    "application/json",
    "APPLICATION/JSON",
  ] {
    let snapshot = Snapshot::default();
    let error = compile(Some(&source(&[(key, "alias")])), HashAlgorithm::Blake3_256, &snapshot).err().expect("invalid MIME");
    assert!(matches!(error, SemanticCompilationErrorV1::InvalidSource { .. }), "{key}: {error}");
    if key.eq_ignore_ascii_case("application/json") {
      assert!(error.to_string().contains("explicit per-scope parser"));
    }
    assert!(snapshot.calls.borrow().is_empty());
  }
  for key in ["\t Text/Plain \t", "application/problem+json", "application/octet-stream"] {
    assert!(compile(Some(&source(&[(key, "alias")])), HashAlgorithm::Blake3_256, &Snapshot::default()).is_ok());
  }
}

#[test]
fn registry_and_alias_limits_use_decoded_entries_and_utf8_bytes() {
  let keys: Vec<_> = (0..513).map(|index| format!("application/x-{index:03}")).collect();
  let entries: Vec<_> = keys.iter().map(|key| (key.as_str(), "same")).collect();
  let snapshot = Snapshot::default();
  let result = compile(Some(&source(&entries[..512])), HashAlgorithm::Blake3_256, &snapshot).unwrap();
  assert_eq!(result.entries().len(), 512);
  assert!(compile(Some(&source(&entries)), HashAlgorithm::Blake3_256, &Snapshot::default()).is_err());
  for alias in ["".to_string(), "x\0y".into(), "x\ny".into(), "x\u{0085}y".into(), "é".repeat(2048) + "a"] {
    assert!(compile(Some(&source(&[("text/plain", &alias)])), HashAlgorithm::Blake3_256, &Snapshot::default()).is_err());
  }
  assert!(compile(Some(&source(&[("text/plain", &"é".repeat(2048))])), HashAlgorithm::Blake3_256, &Snapshot::default()).is_ok());
  let maximum = format!("{}/{}", "x".repeat(127), "y".repeat(127));
  assert!(compile(Some(&source(&[(&maximum, "alias")])), HashAlgorithm::Blake3_256, &Snapshot::default()).is_ok());
  assert!(compile(Some(&source(&[(&(maximum + "y"), "alias")])), HashAlgorithm::Blake3_256, &Snapshot::default()).is_err());
}

#[test]
fn missing_or_operational_dependencies_are_not_silent_registry_omissions() {
  let snapshot = Snapshot::default();
  let error = compile(Some(&source(&[("text/plain", "missing")])), HashAlgorithm::Blake3_256, &snapshot).err().unwrap();
  assert!(matches!(error, SemanticCompilationErrorV1::DependencyUnavailable { .. }));
  let snapshot = Snapshot { fail: true, ..Snapshot::default() };
  let error = compile(Some(&source(&[("text/plain", "alias")])), HashAlgorithm::Blake3_256, &snapshot).err().unwrap();
  assert!(matches!(error, SemanticCompilationErrorV1::Operational { path: "/snapshot", .. }));
}

#[test]
fn migrated_malformed_wrong_role_or_unknown_executor_records_cannot_be_compiled_as_corrected_parsers() {
  for field in 0..12 {
    let mut snapshot = Snapshot::default();
    match field {
      0 => snapshot.record.kind = 2,
      1 => snapshot.record.role = 2,
      2 => snapshot.record.flags = 5,
      3 => snapshot.record.abi = 1,
      4 => snapshot.record.executor_profile = 99,
      5 => snapshot.record.fingerprint_semantics = 2,
      6 => snapshot.record.artifact_kind = 0,
      7 => snapshot.record.artifact_length = 0,
      8 => snapshot.record.fingerprint = [0; 32],
      9 => snapshot.record.dependency_id = "/bad/../path",
      10 => snapshot.record.version = "not-semver",
      11 => snapshot.record.abi = 99,
      _ => unreachable!(),
    }
    assert!(compile(Some(&source(&[("text/plain", "alias")])), HashAlgorithm::Blake3_256, &snapshot).is_err(), "field {field}");
  }
}

#[test]
fn caller_source_and_workspace_bounds_fail_before_lookup_and_do_not_change_semantics() {
  let bytes = source(&[("text/plain", "alias")]);
  let snapshot = Snapshot::default();
  for limit in 0..2 {
    let mut request = request(Some(&bytes), HashAlgorithm::Blake3_256);
    if limit == 0 {
      request.maximum_source_bytes = bytes.len() - 1;
    } else {
      request.maximum_workspace_bytes = 1;
    }
    assert!(matches!(
      compile_parser_registry_v1(request, &snapshot, &memory(), &|| false),
      Err(SemanticCompilationErrorV1::Resource { .. })
    ));
    assert!(snapshot.calls.borrow().is_empty());
  }
  let mut request = request(Some(&bytes), HashAlgorithm::Blake3_256);
  request.maximum_source_bytes = bytes.len();
  let exact = compile_parser_registry_v1(request, &snapshot, &memory(), &|| false).unwrap();
  assert_eq!(exact.projection().semantic_id, compile(Some(&bytes), HashAlgorithm::Blake3_256, &snapshot).unwrap().projection().semantic_id);
}

#[test]
fn cancellation_before_and_during_alias_resolution_releases_the_shared_reservation() {
  for before in [true, false] {
    let snapshot = Snapshot { cancelled: Cell::new(before), cancel_on_lookup: !before, ..Snapshot::default() };
    let memory = memory();
    let bytes = source(&[("text/plain", "alias")]);
    let result =
      compile_parser_registry_v1(request(Some(&bytes), HashAlgorithm::Blake3_256), &snapshot, &memory, &|| snapshot.cancelled.get());
    assert!(matches!(result, Err(SemanticCompilationErrorV1::Cancelled)));
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
    assert_eq!(snapshot.calls.borrow().len(), usize::from(!before));
  }
}

#[test]
fn unavailable_and_revoked_memory_admission_fail_without_a_partial_projection() {
  let bytes = source(&[("text/plain", "alias")]);
  let snapshot = Snapshot::default();
  assert!(matches!(
    compile_parser_registry_v1(request(Some(&bytes), HashAlgorithm::Blake3_256), &snapshot, &MemoryCoordinator::without_policy(), &|| {
      false
    }),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  assert!(snapshot.calls.borrow().is_empty());
  let memory = memory();
  let snapshot = Snapshot { memory: Some(memory.clone()), ..Snapshot::default() };
  assert!(matches!(
    compile_parser_registry_v1(request(Some(&bytes), HashAlgorithm::Blake3_256), &snapshot, &memory, &|| false),
    Err(SemanticCompilationErrorV1::Resource { .. })
  ));
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn compiled_projection_retains_its_memory_admission_until_the_output_is_dropped() {
  let memory = memory();
  let output = compile_parser_registry_v1(request(None, HashAlgorithm::Sha512), &Snapshot::default(), &memory, &|| false).unwrap();
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes > 0);
  drop(output);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn compiled_projection_enforces_the_exact_frozen_byte_cap_without_partial_output() {
  let identifier = format!("/{}", "a".repeat(3967));
  let mut snapshot = Snapshot::default();
  snapshot.record.dependency_id = &identifier;
  let mut keys: Vec<_> = (0..64).map(|index| format!("application/x-{index:03}")).collect();
  keys.last_mut().unwrap().push_str(&"x".repeat(55));
  let entries: Vec<_> = keys.iter().map(|key| (key.as_str(), "alias")).collect();
  let compiled = compile(Some(&source(&entries)), HashAlgorithm::Sha512, &snapshot).unwrap();
  assert_eq!(
    decode_semantic_definition_record(&compiled.projection().object.value, HashAlgorithm::Sha512).unwrap().definition.len(),
    262144
  );
  keys.last_mut().unwrap().push('x');
  let entries: Vec<_> = keys.iter().map(|key| (key.as_str(), "alias")).collect();
  let memory = memory();
  let bytes = source(&entries);
  let result = compile_parser_registry_v1(request(Some(&bytes), HashAlgorithm::Sha512), &snapshot, &memory, &|| false);
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}
