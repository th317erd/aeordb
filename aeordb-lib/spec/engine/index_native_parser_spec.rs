use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::memory_coordinator::MemoryOwner;
use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
use aeordb::engine::v4::dependency::decode_dependency_table;
use aeordb::engine::v4::index_native_parser::NativeIndexParserExecutorV1;
use aeordb::engine::v4::index_native_source::{NativeIndexFileRevisionSourceV1, NativeIndexSourceLimitsV1};
use aeordb::engine::v4::index_producer_collector::{
  IndexParserExecutionErrorClassV1, IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1,
};
use aeordb::engine::v4::index_producer_source::IndexFileRevisionSourceV1;
use aeordb::engine::v4::parser_plan::decode_parser_resolution_plan;
use aeordb::engine::v4::value_store::{ValueStoreDefinitionV1, decode_value_store_definition};
use aeordb::engine::{HashAlgorithm, RequestContext, StorageEngine};
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let path = directory.path().join("native-index-parser.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

fn source_limits() -> NativeIndexSourceLimitsV1 {
  NativeIndexSourceLimitsV1::new(16 * 1_024 * 1_024, 16 * 1_024 * 1_024, 64).unwrap()
}

fn corrected_definition_bytes() -> Vec<u8> {
  std::fs::read(format!(
    "{}/spec/fixtures/v4/value-store-definition-v1/avst-blake3-256-json-corrected-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap()
}

fn legacy_definition_bytes() -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/value-store-definition-v1/avst-blake3-256-json-legacy-valid.bin", env!("CARGO_MANIFEST_DIR")))
    .unwrap()
}

fn corrected_definition(bytes: &[u8]) -> ValueStoreDefinitionV1<'_> {
  decode_value_store_definition(bytes, ALGORITHM).unwrap()
}

fn parse_file(
  engine: &StorageEngine,
  root: &[u8],
  path: &str,
  definition: &ValueStoreDefinitionV1<'_>,
  maximum_document_input_bytes: u64,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<IndexParserOutcomeV1, aeordb::engine::v4::index_producer_collector::IndexParserExecutionErrorV1> {
  let source = NativeIndexFileRevisionSourceV1::new(engine, source_limits());
  let revision = source.load_file_revision(root, path).unwrap().unwrap();
  let revision = revision.revision();
  NativeIndexParserExecutorV1::new(engine).parse(IndexParserExecutionRequestV1::new(
    root,
    &revision.revision_hash,
    &revision.file_record,
    &definition.parser_plan,
    &definition.dependencies,
    maximum_document_input_bytes,
    is_cancelled,
  ))
}

fn parse_record(
  engine: &StorageEngine,
  root: &[u8],
  revision_hash: &[u8],
  record: &aeordb::engine::file_record::FileRecord,
  definition: &ValueStoreDefinitionV1<'_>,
) -> Result<IndexParserOutcomeV1, aeordb::engine::v4::index_producer_collector::IndexParserExecutionErrorV1> {
  NativeIndexParserExecutorV1::new(engine).parse(IndexParserExecutionRequestV1::new(
    root,
    revision_hash,
    record,
    &definition.parser_plan,
    &definition.dependencies,
    64 * 1_024 * 1_024,
    &|| false,
  ))
}

fn parsed_map(outcome: IndexParserOutcomeV1) -> std::collections::BTreeMap<String, CanonicalConfigValueV1> {
  let IndexParserOutcomeV1::Parsed(CanonicalConfigValueV1::Map(value)) = outcome else {
    panic!("expected a parsed map");
  };
  value
}

#[test]
fn native_parser_reads_the_exact_historical_revision_and_normalizes_json_mime() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations
    .store_file_buffered(
      &context,
      "/docs/messages.json",
      br#"{"messages":[{"user":"first"}]}"#,
      Some("Application/JSON; Charset=\"utf-8\""),
    )
    .unwrap();
  let first_root = engine.head_hash().unwrap();
  operations
    .store_file_buffered(&context, "/docs/messages.json", br#"{"messages":[{"user":"second"}]}"#, Some("application/json"))
    .unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);

  let parsed = parsed_map(parse_file(&engine, &first_root, "/docs/messages.json", &definition, 64 * 1_024 * 1_024, &|| false).unwrap());
  assert_eq!(
    parsed.get("messages"),
    Some(&CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
      "user".to_string(),
      CanonicalConfigValueV1::String("first".to_string()),
    )]))]))
  );
}

#[test]
fn corrected_json_claim_and_policy_failures_are_deterministic() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/duplicate.data", br#"{"a":1,"a":2}"#, Some("application/problem+json")).unwrap();
  operations.store_file_buffered(&context, "/large.json", br#"{"a":1}"#, Some("application/json")).unwrap();
  let duplicate_root = engine.head_hash().unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);

  assert!(matches!(
    parse_file(&engine, &duplicate_root, "/duplicate.data", &definition, 64 * 1_024 * 1_024, &|| false).unwrap(),
    IndexParserOutcomeV1::DeterministicUnindexable(_)
  ));
  let latest_root = engine.head_hash().unwrap();
  assert!(matches!(
    parse_file(&engine, &latest_root, "/large.json", &definition, 1, &|| false).unwrap(),
    IndexParserOutcomeV1::DeterministicUnindexable(_)
  ));
}

#[test]
fn corrected_native_extension_fallback_preserves_stored_metadata_and_no_match_skips_body() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/README.MD", b"# Heading\r\nBody", Some("broken MIME value")).unwrap();
  operations.store_file_buffered(&context, "/docs/blob.unknown", b"\0\xff", Some("application/octet-stream")).unwrap();
  let root = engine.head_hash().unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);

  let parsed = parsed_map(parse_file(&engine, &root, "/docs/README.MD", &definition, 64 * 1_024 * 1_024, &|| false).unwrap());
  let Some(CanonicalConfigValueV1::Map(metadata)) = parsed.get("metadata") else {
    panic!("expected native metadata");
  };
  assert_eq!(metadata.get("content_type"), Some(&CanonicalConfigValueV1::String("broken MIME value".to_string())));

  assert_eq!(
    parse_file(&engine, &root, "/docs/blob.unknown", &definition, 64 * 1_024 * 1_024, &|| false).unwrap(),
    IndexParserOutcomeV1::NotApplicable
  );
}

#[test]
fn corrected_mime_routing_enforces_normalization_and_name_boundaries() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&RequestContext::system(), "/docs/blob.unknown", b"\0\xff", Some("application/octet-stream")).unwrap();
  let root = engine.head_hash().unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, source_limits());
  let revision = source.load_file_revision(&root, "/docs/blob.unknown").unwrap().unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);

  for content_type in ["text/plain", "\tTEXT/PLAIN ", "Text/Plain; charset=\"utf-8\"", "text/plain;charset=utf-8;charset=us-ascii"] {
    let mut record = revision.revision().file_record.clone();
    record.content_type = Some(content_type.to_string());
    let error = parse_record(&engine, &root, &revision.revision().revision_hash, &record, &definition).unwrap_err();
    assert_eq!(error.class(), IndexParserExecutionErrorClassV1::DependencyUnavailable, "{content_type}");
  }

  for content_type in ["", " \t", "text", "text/*", "te'xt/plain", "text/plain; broken"] {
    let mut record = revision.revision().file_record.clone();
    record.content_type = Some(content_type.to_string());
    assert_eq!(
      parse_record(&engine, &root, &revision.revision().revision_hash, &record, &definition).unwrap(),
      IndexParserOutcomeV1::NotApplicable,
      "{content_type}"
    );
  }

  let valid_subtype = format!("x-{}", "a".repeat(125));
  let mut valid = revision.revision().file_record.clone();
  valid.content_type = Some(format!("text/{valid_subtype}"));
  assert!(matches!(
    parse_record(&engine, &root, &revision.revision().revision_hash, &valid, &definition).unwrap(),
    IndexParserOutcomeV1::DeterministicUnindexable(_)
  ));

  let invalid_subtype = format!("x-{}", "a".repeat(126));
  let mut invalid = revision.revision().file_record.clone();
  invalid.content_type = Some(format!("text/{invalid_subtype}"));
  assert_eq!(
    parse_record(&engine, &root, &revision.revision().revision_hash, &invalid, &definition).unwrap(),
    IndexParserOutcomeV1::NotApplicable
  );
}

#[test]
fn parser_refuses_unknown_dependencies_and_preserves_cancellation() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/value.json", br#"{"a":1}"#, Some("application/json")).unwrap();
  let root = engine.head_hash().unwrap();

  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);
  let cancelled = parse_file(&engine, &root, "/docs/value.json", &definition, 64 * 1_024 * 1_024, &|| true).unwrap_err();
  assert_eq!(cancelled.class(), IndexParserExecutionErrorClassV1::Cancelled);

  let fingerprint = definition.dependencies.records[definition.parser_plan.mime_dependency_ordinal as usize - 1].fingerprint;
  let mut unknown_dependency = definition_bytes.clone();
  let position = unknown_dependency.windows(fingerprint.len()).position(|candidate| candidate == fingerprint).unwrap();
  unknown_dependency[position] ^= 0x01;
  let unknown_definition = corrected_definition(&unknown_dependency);
  let error = parse_file(&engine, &root, "/docs/value.json", &unknown_definition, 64 * 1_024 * 1_024, &|| false).unwrap_err();
  assert_eq!(error.class(), IndexParserExecutionErrorClassV1::DependencyUnavailable);
}

#[test]
fn legacy_resolution_preserves_exact_registry_and_last_key_wins_json() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  operations
    .store_file_buffered(
      &RequestContext::system(),
      "/docs/value.data",
      br#"{"messages":[{"user":"first"}],"messages":[{"user":"last"}]}"#,
      Some("application/octet-stream"),
    )
    .unwrap();
  let root = engine.head_hash().unwrap();
  let bytes = legacy_definition_bytes();
  let definition = corrected_definition(&bytes);

  let parsed = parsed_map(parse_file(&engine, &root, "/docs/value.data", &definition, u64::MAX, &|| false).unwrap());
  assert_eq!(
    parsed.get("messages"),
    Some(&CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Map(std::collections::BTreeMap::from([(
      "user".to_string(),
      CanonicalConfigValueV1::String("last".to_string()),
    )]))]))
  );

  let source = NativeIndexFileRevisionSourceV1::new(&engine, source_limits());
  let revision = source.load_file_revision(&root, "/docs/value.data").unwrap().unwrap();
  let mut exact_registry = revision.revision().file_record.clone();
  exact_registry.content_type = Some("Text/Plain; charset=UTF-8".to_string());
  let error = parse_record(&engine, &root, &revision.revision().revision_hash, &exact_registry, &definition).unwrap_err();
  assert_eq!(error.class(), IndexParserExecutionErrorClassV1::DependencyUnavailable);
}

#[test]
fn explicit_wasm_plan_fails_closed_before_reading_file_chunks() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  operations.store_file_buffered(&RequestContext::system(), "/docs/value.data", b"payload", None).unwrap();
  let root = engine.head_hash().unwrap();
  let source = NativeIndexFileRevisionSourceV1::new(&engine, source_limits());
  let revision = source.load_file_revision(&root, "/docs/value.data").unwrap().unwrap();
  let parser_bytes = std::fs::read(format!(
    "{}/spec/fixtures/v4/parser-resolution-plan-v1/aprp-blake3-256-explicit-plugin-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap();
  let dependency_bytes = std::fs::read(format!(
    "{}/spec/fixtures/v4/dependency-table-v1/adpt-blake3-256-native-parser-resolution-valid.bin",
    env!("CARGO_MANIFEST_DIR")
  ))
  .unwrap();
  let parser_plan = decode_parser_resolution_plan(&parser_bytes).unwrap();
  let dependencies = decode_dependency_table(&dependency_bytes).unwrap();
  let mut missing_chunks = revision.revision().file_record.clone();
  missing_chunks.chunk_hashes[0] = vec![0x99; ALGORITHM.hash_length()];

  let error = NativeIndexParserExecutorV1::new(&engine)
    .parse(IndexParserExecutionRequestV1::new(
      &root,
      &revision.revision().revision_hash,
      &missing_chunks,
      &parser_plan,
      &dependencies,
      64 * 1_024 * 1_024,
      &|| false,
    ))
    .unwrap_err();
  assert_eq!(error.class(), IndexParserExecutionErrorClassV1::DependencyUnavailable);
}

#[test]
fn native_parser_architecture_uses_only_frozen_request_authority() {
  let source = include_str!("../../src/engine/v4/index_native_parser.rs");
  let collector = include_str!("../../src/engine/v4/index_producer_collector.rs");
  assert!(source.contains("request.parser_plan()"));
  assert!(source.contains("request.dependencies()"));
  assert!(source.contains("CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1"));
  assert!(collector.contains("CANONICAL_CONFIG_VALUE_MAX_RETAINED_BYTES_PER_NODE_V1"));
  assert!(!source.contains("IndexingPipeline"));
  assert!(!source.contains("parsers.json"));
  assert!(!source.contains("plugin_manager"));
  assert!(!source.contains("parser_registry"));
}

#[test]
fn native_parser_releases_body_memory_after_success_and_failure() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  operations.store_file_buffered(&context, "/docs/value.json", br#"{"a":1}"#, Some("application/json")).unwrap();
  let root = engine.head_hash().unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);
  let before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().reserved_bytes;
  let parser_before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes;

  parse_file(&engine, &root, "/docs/value.json", &definition, 64 * 1_024 * 1_024, &|| false).unwrap();
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().reserved_bytes, before);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes, parser_before);

  let source = NativeIndexFileRevisionSourceV1::new(&engine, source_limits());
  let revision = source.load_file_revision(&root, "/docs/value.json").unwrap().unwrap();
  let mut corrupt_record = revision.revision().file_record.clone();
  corrupt_record.chunk_hashes[0] = vec![0x99; ALGORITHM.hash_length()];
  let error = NativeIndexParserExecutorV1::new(&engine)
    .parse(IndexParserExecutionRequestV1::new(
      &root,
      &revision.revision().revision_hash,
      &corrupt_record,
      &definition.parser_plan,
      &definition.dependencies,
      64 * 1_024 * 1_024,
      &|| false,
    ))
    .unwrap_err();
  assert_eq!(error.class(), IndexParserExecutionErrorClassV1::HostFailure);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().reserved_bytes, before);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes, parser_before);

  let mut wrong_content_hash = revision.revision().file_record.clone();
  wrong_content_hash.content_hash[0] ^= 0x01;
  let error = parse_record(&engine, &root, &revision.revision().revision_hash, &wrong_content_hash, &definition).unwrap_err();
  assert_eq!(error.class(), IndexParserExecutionErrorClassV1::HostFailure);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().reserved_bytes, before);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes, parser_before);
}

#[test]
fn native_parser_cancels_between_chunks_and_releases_all_parser_memory() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let body = vec![0xa5; 700 * 1_024];
  operations.store_file_buffered(&context, "/docs/large.unknown", &body, Some("application/octet-stream")).unwrap();
  let root = engine.head_hash().unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);
  let streaming_before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().reserved_bytes;
  let parser_before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes;
  let checks = AtomicUsize::new(0);
  let cancelled = || checks.fetch_add(1, Ordering::SeqCst) >= 3;

  let error = parse_file(&engine, &root, "/docs/large.unknown", &definition, 64 * 1_024 * 1_024, &cancelled).unwrap_err();
  assert_eq!(error.class(), IndexParserExecutionErrorClassV1::Cancelled);
  assert!(checks.load(Ordering::SeqCst) >= 4);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().reserved_bytes, streaming_before);
  assert_eq!(engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::ParserPlugin).unwrap().reserved_bytes, parser_before);
}

#[test]
fn corrected_native_archive_parsers_return_canonical_values_and_reject_malformed_archives() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let docx =
    build_zip(&[("word/document.xml", b"<w:document><w:body><w:p><w:r><w:t>Hello corrected DOCX</w:t></w:r></w:p></w:body></w:document>")]);
  let odt = build_zip(&[
    ("mimetype", b"application/vnd.oasis.opendocument.text"),
    ("content.xml", b"<office:document-content><text:p>Hello corrected ODT</text:p></office:document-content>"),
  ]);
  operations
    .store_file_buffered(
      &context,
      "/docs/value.docx",
      &docx,
      Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    )
    .unwrap();
  operations.store_file_buffered(&context, "/docs/value.odt", &odt, Some("application/vnd.oasis.opendocument.text")).unwrap();
  operations
    .store_file_buffered(
      &context,
      "/docs/malformed.docx",
      b"not a ZIP archive",
      Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    )
    .unwrap();
  let root = engine.head_hash().unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);

  let docx = parsed_map(parse_file(&engine, &root, "/docs/value.docx", &definition, 64 * 1_024 * 1_024, &|| false).unwrap());
  assert_eq!(docx.get("text"), Some(&CanonicalConfigValueV1::String("Hello corrected DOCX".to_string())));
  let odt = parsed_map(parse_file(&engine, &root, "/docs/value.odt", &definition, 64 * 1_024 * 1_024, &|| false).unwrap());
  assert_eq!(odt.get("text"), Some(&CanonicalConfigValueV1::String("Hello corrected ODT".to_string())));
  assert!(matches!(
    parse_file(&engine, &root, "/docs/malformed.docx", &definition, 64 * 1_024 * 1_024, &|| false).unwrap(),
    IndexParserOutcomeV1::DeterministicUnindexable(_)
  ));
}

#[test]
fn corrected_json_enforces_scalar_member_depth_and_global_node_boundaries_during_parse() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let exact_scalar = json_string(64 * 1_024);
  let oversized_scalar = json_string(64 * 1_024 + 1);
  let exact_members = json_null_array(65_535);
  let oversized_members = json_null_array(65_536);
  let exact_nodes = high_node_json(49_999, 1);
  let oversized_nodes = high_node_json(49_999, 2);
  let oversized_depth = nested_json(33);
  for (path, body) in [
    ("/docs/exact-scalar.json", exact_scalar.as_slice()),
    ("/docs/oversized-scalar.json", oversized_scalar.as_slice()),
    ("/docs/exact-members.json", exact_members.as_slice()),
    ("/docs/oversized-members.json", oversized_members.as_slice()),
    ("/docs/exact-nodes.json", exact_nodes.as_slice()),
    ("/docs/oversized-nodes.json", oversized_nodes.as_slice()),
    ("/docs/oversized-depth.json", oversized_depth.as_slice()),
  ] {
    operations.store_file_buffered(&context, path, body, Some("application/json")).unwrap();
  }
  let root = engine.head_hash().unwrap();
  let definition_bytes = corrected_definition_bytes();
  let definition = corrected_definition(&definition_bytes);

  for path in ["/docs/exact-scalar.json", "/docs/exact-members.json", "/docs/exact-nodes.json"] {
    assert!(matches!(
      parse_file(&engine, &root, path, &definition, 64 * 1_024 * 1_024, &|| false).unwrap(),
      IndexParserOutcomeV1::Parsed(_)
    ));
  }
  for path in ["/docs/oversized-scalar.json", "/docs/oversized-members.json", "/docs/oversized-nodes.json", "/docs/oversized-depth.json"] {
    assert!(matches!(
      parse_file(&engine, &root, path, &definition, 64 * 1_024 * 1_024, &|| false).unwrap(),
      IndexParserOutcomeV1::DeterministicUnindexable(_)
    ));
  }
}

fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
  let cursor = Cursor::new(Vec::new());
  let mut writer = zip::ZipWriter::new(cursor);
  for (name, value) in entries {
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    writer.start_file(*name, options).unwrap();
    writer.write_all(value).unwrap();
  }
  writer.finish().unwrap().into_inner()
}

fn json_string(length: usize) -> Vec<u8> {
  let mut value = Vec::with_capacity(length + 2);
  value.push(b'"');
  value.resize(length + 1, b'x');
  value.push(b'"');
  value
}

fn json_null_array(members: usize) -> Vec<u8> {
  let mut value = Vec::with_capacity(members.saturating_mul(5).saturating_add(1));
  value.push(b'[');
  for member in 0..members {
    if member > 0 {
      value.push(b',');
    }
    value.extend_from_slice(b"null");
  }
  value.push(b']');
  value
}

fn high_node_json(singleton_arrays: usize, trailing_nulls: usize) -> Vec<u8> {
  let mut value = Vec::with_capacity(singleton_arrays.saturating_mul(7).saturating_add(trailing_nulls.saturating_mul(5)));
  value.push(b'[');
  let mut members = 0usize;
  for _ in 0..singleton_arrays {
    if members > 0 {
      value.push(b',');
    }
    value.extend_from_slice(b"[null]");
    members += 1;
  }
  for _ in 0..trailing_nulls {
    if members > 0 {
      value.push(b',');
    }
    value.extend_from_slice(b"null");
    members += 1;
  }
  value.push(b']');
  value
}

fn nested_json(depth: usize) -> Vec<u8> {
  let mut value = Vec::with_capacity(depth.saturating_mul(2).saturating_add(4));
  value.resize(depth, b'[');
  value.extend_from_slice(b"null");
  value.resize(depth.saturating_mul(2).saturating_add(4), b']');
  value
}
