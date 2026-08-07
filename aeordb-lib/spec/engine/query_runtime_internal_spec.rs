use std::sync::Arc;

use super::query_engine::{ExplainMode, Query, QueryEngine, QueryStrategy};
use super::query_runtime::{QueryRuntime, QueryRuntimePolicy};
use super::storage_engine::StorageEngine;
use super::EngineError;

fn policy(per_request_memory_bytes: u64, global_memory_bytes: u64, position_scan_buffer_bytes: u64) -> QueryRuntimePolicy {
  QueryRuntimePolicy::new(per_request_memory_bytes, global_memory_bytes, position_scan_buffer_bytes).unwrap()
}

#[test]
fn request_reservations_are_counted_and_released_exactly() {
  let runtime = Arc::new(QueryRuntime::new(policy(16 * 1024, 32 * 1024, 4096)));
  let request = runtime.start_request().unwrap();
  let mut reservation = request.reserve(8192).unwrap();

  reservation.grow(4096).unwrap();
  reservation.shrink(2048).unwrap();
  let held = runtime.snapshot().unwrap();
  assert_eq!(held.active_requests, 1);
  assert_eq!(held.reserved_bytes, 10 * 1024);
  assert_eq!(reservation.bytes(), 10 * 1024);

  drop(reservation);
  drop(request);
  let released = runtime.snapshot().unwrap();
  assert_eq!(released.active_requests, 0);
  assert_eq!(released.reserved_bytes, 0);
}

#[test]
fn global_rejection_rolls_back_the_request_counter() {
  let runtime = Arc::new(QueryRuntime::new(policy(16 * 1024, 20 * 1024, 4096)));
  let first_request = runtime.start_request().unwrap();
  let first = first_request.reserve(12 * 1024).unwrap();
  let second_request = runtime.start_request().unwrap();

  let error = second_request.reserve(9 * 1024).err().expect("global limit must reject the reservation");
  assert!(error.to_string().contains("global query memory"));
  let second = second_request.reserve(8 * 1024).unwrap();
  assert_eq!(runtime.snapshot().unwrap().reserved_bytes, 20 * 1024);

  drop(second);
  drop(second_request);
  drop(first);
  drop(first_request);
  assert_eq!(runtime.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn policy_decrease_blocks_growth_until_old_reservations_drain() {
  let runtime = Arc::new(QueryRuntime::new(policy(16 * 1024, 32 * 1024, 4096)));
  let old_request = runtime.start_request().unwrap();
  let mut old_reservation = old_request.reserve(12 * 1024).unwrap();

  runtime.reconfigure(policy(8 * 1024, 8 * 1024, 2048)).unwrap();
  assert_eq!(old_request.position_scan_buffer_bytes(), 4096, "running requests must retain their captured scan policy");
  assert!(old_reservation.grow(1).is_err(), "a lowered global ceiling must reject new growth while old work drains");

  let new_request = runtime.start_request().unwrap();
  assert!(new_request.reserve(8 * 1024).is_err(), "existing reservations must remain charged against the new global ceiling");
  assert!(new_request.reserve(8 * 1024 + 1).is_err(), "new requests must capture the lowered per-request ceiling");

  drop(old_reservation);
  drop(old_request);
  let replacement = new_request.reserve(8 * 1024).unwrap();
  assert_eq!(runtime.snapshot().unwrap().reserved_bytes, 8 * 1024);
  drop(replacement);
  drop(new_request);
  assert_eq!(runtime.snapshot().unwrap().active_requests, 0);
}

#[test]
fn malformed_policies_fail_before_publication() {
  assert!(QueryRuntimePolicy::new(0, 1, 1).is_err());
  assert!(QueryRuntimePolicy::new(2, 1, 1).is_err());
  assert!(QueryRuntimePolicy::new(1, 1, 0).is_err());
}

#[test]
fn disabled_runtime_reports_its_reason_and_rejects_new_requests() {
  let runtime = Arc::new(QueryRuntime::disabled("query policy is unresolved".to_string()));

  let snapshot = runtime.snapshot().unwrap();
  assert_eq!(snapshot.policy, None);
  assert_eq!(snapshot.disabled_reason.as_deref(), Some("query policy is unresolved"));
  let error = runtime.start_request().err().expect("disabled query owner must reject new work");
  assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected error: {error}");
  assert!(error.to_string().contains("query policy is unresolved"), "{error}");
  assert_eq!(runtime.snapshot().unwrap().active_requests, 0);
}

#[test]
fn query_engine_honors_a_caller_owned_request_budget() {
  let directory = tempfile::tempdir().unwrap();
  let database_path = directory.path().join("shared-query-budget.aeordb");
  let engine = StorageEngine::create(database_path.to_str().unwrap()).unwrap();
  let policy = engine.query_runtime_snapshot().unwrap().policy.expect("default query runtime is configured");
  let request_budget = engine.start_query_request_budget().unwrap();
  let held = request_budget.reserve(policy.per_request_memory_bytes - 4095).unwrap();
  let query = Query {
    path: "/".to_string(),
    field_queries: Vec::new(),
    node: None,
    limit: Some(1),
    offset: None,
    order_by: Vec::new(),
    after: None,
    before: None,
    include_total: false,
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Off,
  };

  let error = QueryEngine::with_request_budget(&engine, request_budget.clone())
    .execute(&query)
    .expect_err("the query workspace must join the caller's existing per-request charge");
  assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected query error: {error}");

  drop(held);
  QueryEngine::with_request_budget(&engine, request_budget.clone()).execute(&query).unwrap();
  drop(request_budget);
  assert_eq!(engine.query_runtime_snapshot().unwrap().active_requests, 0);
}
