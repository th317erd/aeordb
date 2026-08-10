use crate::engine::config_resolver::ConfigurationFamily;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::EngineError;
use crate::engine::memory_coordinator::MemoryOwner;
use crate::engine::RequestContext;

use super::search_locators::{generate_locators, generate_locators_with_budget, try_generate_locators_with_budget, LocatorOptions, LocatorTerm};

fn locator_options() -> LocatorOptions {
  LocatorOptions {
    include_matches: true,
    max_matches_per_result: 5,
    snippet_chars: 64,
    match_context_lines: 2,
    max_locator_scan_bytes: 1024 * 1024,
  }
}

#[test]
fn convenience_locator_entrypoint_requires_query_admission() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/convenience.txt", b"find the needle", Some("text/plain")).unwrap();
  let file_record = ops.get_metadata("/convenience.txt").unwrap().unwrap();

  let generated = generate_locators(
    &engine,
    &file_record,
    &[LocatorTerm { field: "text".to_string(), operator: "contains".to_string(), literal: "needle".to_string() }],
    &locator_options(),
  )
  .unwrap();

  assert_eq!(generated.locator_status, "complete");
  assert_eq!(generated.matches.len(), 1);
  assert_eq!(engine.query_runtime_snapshot().unwrap().active_requests, 0);
}

#[test]
fn plain_text_locator_streams_across_buffer_and_crlf_boundaries() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let runtime = br#"{"schema_version":1,"query":{"position_scan_buffer_bytes":262144}}"#;
  engine.replace_configuration_document(ConfigurationFamily::Runtime, runtime).unwrap();

  let buffer_limit = 262_144usize;
  let mut content = b"first\r\n".to_vec();
  content.resize(buffer_limit - 3, b'a');
  let match_start = content.len();
  content.extend_from_slice(b"Needle\r\nlast");
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/crossing.txt", &content, Some("text/plain")).unwrap();
  let file_record = ops.get_metadata("/crossing.txt").unwrap().unwrap();
  let request_budget = engine.start_query_request_budget().unwrap();

  let generated = generate_locators_with_budget(
    &engine,
    &file_record,
    &[LocatorTerm { field: "text".to_string(), operator: "contains".to_string(), literal: "needle".to_string() }],
    &locator_options(),
    Some(&request_budget),
  );

  assert_eq!(generated.locator_status, "complete");
  assert!(!generated.matches_truncated);
  assert_eq!(generated.matches.len(), 1);
  let locator = &generated.matches[0];
  assert_eq!(locator.matched_text, "Needle");
  assert_eq!(locator.range.byte.as_ref().unwrap().start, match_start as u64);
  assert_eq!(locator.range.byte.as_ref().unwrap().end, match_start as u64 + 6);
  assert_eq!(locator.range.line.as_ref().unwrap().start, 2);
  assert_eq!(locator.range.column.as_ref().unwrap().start, (match_start - b"first\r\n".len()) as u64);
  assert_eq!(locator.range.char.as_ref().unwrap().start, match_start as u64 - 1, "CRLF must count as one logical break");
  assert!(locator.snippet.text.contains("Needle"));
  assert_eq!(engine.query_runtime_snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(engine.memory_coordinator_snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  drop(request_budget);
  assert_eq!(engine.query_runtime_snapshot().unwrap().active_requests, 0);
}

#[test]
fn invalid_utf8_locator_scan_is_unsupported_and_releases_admission() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/invalid.txt", &[b'a', 0xff, b'b'], Some("text/plain")).unwrap();
  let file_record = ops.get_metadata("/invalid.txt").unwrap().unwrap();
  let request_budget = engine.start_query_request_budget().unwrap();

  let generated = generate_locators_with_budget(
    &engine,
    &file_record,
    &[LocatorTerm { field: "text".to_string(), operator: "contains".to_string(), literal: "a".to_string() }],
    &locator_options(),
    Some(&request_budget),
  );

  assert_eq!(generated.locator_status, "unsupported");
  assert!(generated.matches.is_empty());
  assert_eq!(engine.query_runtime_snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(engine.memory_coordinator_snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn locator_scan_honors_the_callers_existing_per_request_charge() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/shared-budget.txt", b"find the needle", Some("text/plain")).unwrap();
  let file_record = ops.get_metadata("/shared-budget.txt").unwrap().unwrap();
  let policy = engine.query_runtime_snapshot().unwrap().policy.expect("default query runtime is configured");
  let request_budget = engine.start_query_request_budget().unwrap();
  let held = request_budget.reserve(policy.per_request_memory_bytes - policy.position_scan_buffer_bytes + 1).unwrap();

  let error = try_generate_locators_with_budget(
    &engine,
    &file_record,
    &[LocatorTerm { field: "text".to_string(), operator: "contains".to_string(), literal: "needle".to_string() }],
    &locator_options(),
    &request_budget,
  )
  .expect_err("the position scan must join the caller's existing per-request charge");
  assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected locator error: {error}");

  drop(held);
  let generated = try_generate_locators_with_budget(
    &engine,
    &file_record,
    &[LocatorTerm { field: "text".to_string(), operator: "contains".to_string(), literal: "needle".to_string() }],
    &locator_options(),
    &request_budget,
  )
  .unwrap();
  assert_eq!(generated.locator_status, "complete");
  assert_eq!(generated.matches.len(), 1);
}

#[test]
fn streaming_locator_propagates_namespace_read_failures() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/source.txt", b"find the needle", Some("text/plain")).unwrap();
  let mut file_record = ops.get_metadata("/source.txt").unwrap().unwrap();
  file_record.path = "/missing.txt".to_string();
  let request_budget = engine.start_query_request_budget().unwrap();

  let error = try_generate_locators_with_budget(
    &engine,
    &file_record,
    &[LocatorTerm { field: "text".to_string(), operator: "contains".to_string(), literal: "needle".to_string() }],
    &locator_options(),
    &request_budget,
  )
  .expect_err("a missing stored file must not be reported as an unsupported locator format");

  assert!(matches!(error, EngineError::NotFound(ref path) if path == "/missing.txt"), "unexpected locator error: {error}");
}

#[test]
fn buffered_json_locator_propagates_namespace_read_failures() {
  let (engine, _directory) = crate::server::create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  ops
    .store_file_buffered(&RequestContext::system(), "/source.json", br#"{"message":"find the needle"}"#, Some("application/json"))
    .unwrap();
  let mut file_record = ops.get_metadata("/source.json").unwrap().unwrap();
  file_record.path = "/missing.json".to_string();
  let request_budget = engine.start_query_request_budget().unwrap();

  let error = try_generate_locators_with_budget(
    &engine,
    &file_record,
    &[LocatorTerm { field: "message".to_string(), operator: "contains".to_string(), literal: "needle".to_string() }],
    &locator_options(),
    &request_budget,
  )
  .expect_err("a missing JSON file must not be reported as an unsupported locator format");

  assert!(matches!(error, EngineError::NotFound(ref path) if path == "/missing.json"), "unexpected locator error: {error}");
}
