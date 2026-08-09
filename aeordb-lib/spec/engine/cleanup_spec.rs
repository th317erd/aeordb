use chrono::{Duration, Utc};

use aeordb::auth::magic_link::MagicLinkRecord;
use aeordb::auth::refresh::RefreshTokenRecord;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::schema_version::JsonVersioned;
use aeordb::engine::system_store;
use aeordb::engine::{DirectoryOps, EngineError, RequestContext};
use aeordb::server::create_temp_engine_for_tests;

// ===========================================================================
// Helper: store a refresh token record directly into the system store.
// ===========================================================================

fn store_test_refresh_token(engine: &aeordb::engine::StorageEngine, ctx: &RequestContext, token_hash: &str, expired: bool, revoked: bool) {
  let expires_at = if expired { Utc::now() - Duration::hours(1) } else { Utc::now() + Duration::hours(24) };

  let record = RefreshTokenRecord {
    token_hash: token_hash.to_string(),
    user_subject: "test-user".to_string(),
    created_at: Utc::now() - Duration::hours(2),
    expires_at,
    is_revoked: revoked,
    key_id: None,
  };

  system_store::store_refresh_token(engine, ctx, &record).unwrap();
}

// ===========================================================================
// Helper: store a magic link record directly into the system store.
// ===========================================================================

fn store_test_magic_link(engine: &aeordb::engine::StorageEngine, ctx: &RequestContext, code_hash: &str, expired: bool, used: bool) {
  let expires_at = if expired { Utc::now() - Duration::minutes(30) } else { Utc::now() + Duration::minutes(10) };

  let record = MagicLinkRecord {
    code_hash: code_hash.to_string(),
    email: "test@example.com".to_string(),
    created_at: Utc::now() - Duration::hours(1),
    expires_at,
    is_used: used,
  };

  system_store::store_magic_link(engine, ctx, &record).unwrap();
}

// ===========================================================================
// 1. test_cleanup_removes_expired_refresh_tokens
// ===========================================================================

#[test]
fn test_cleanup_removes_expired_refresh_tokens() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Store an expired token
  store_test_refresh_token(&engine, &ctx, "expired-token-hash", true, false);

  // Verify it exists
  let record = system_store::get_refresh_token(&engine, "expired-token-hash").unwrap();
  assert!(record.is_some(), "expired token should exist before cleanup");

  // Run cleanup
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 1, "should have cleaned 1 expired token");
  assert_eq!(links, 0, "should have cleaned 0 links");

  // Verify it's gone
  let record = system_store::get_refresh_token(&engine, "expired-token-hash").unwrap();
  assert!(record.is_none(), "expired token should be removed after cleanup");
}

// ===========================================================================
// 2. test_cleanup_removes_revoked_refresh_tokens
// ===========================================================================

#[test]
fn test_cleanup_removes_revoked_refresh_tokens() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Store a revoked (but not expired) token
  store_test_refresh_token(&engine, &ctx, "revoked-token-hash", false, true);

  // Verify it exists
  let record = system_store::get_refresh_token(&engine, "revoked-token-hash").unwrap();
  assert!(record.is_some(), "revoked token should exist before cleanup");

  // Run cleanup
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 1, "should have cleaned 1 revoked token");
  assert_eq!(links, 0);

  // Verify it's gone
  let record = system_store::get_refresh_token(&engine, "revoked-token-hash").unwrap();
  assert!(record.is_none(), "revoked token should be removed after cleanup");
}

// ===========================================================================
// 3. test_cleanup_preserves_valid_tokens
// ===========================================================================

#[test]
fn test_cleanup_preserves_valid_tokens() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Store a valid, non-revoked, non-expired token
  store_test_refresh_token(&engine, &ctx, "valid-token-hash", false, false);

  // Run cleanup
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 0, "should NOT clean valid tokens");
  assert_eq!(links, 0);

  // Verify it still exists
  let record = system_store::get_refresh_token(&engine, "valid-token-hash").unwrap();
  assert!(record.is_some(), "valid token should be preserved after cleanup");
}

// ===========================================================================
// 4. test_cleanup_removes_used_magic_links
// ===========================================================================

#[test]
fn test_cleanup_removes_used_magic_links() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Store a used (but not expired) magic link
  store_test_magic_link(&engine, &ctx, "used-link-hash", false, true);

  // Verify it exists
  let record = system_store::get_magic_link(&engine, "used-link-hash").unwrap();
  assert!(record.is_some(), "used magic link should exist before cleanup");

  // Run cleanup
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 0);
  assert_eq!(links, 1, "should have cleaned 1 used magic link");

  // Verify it's gone
  let record = system_store::get_magic_link(&engine, "used-link-hash").unwrap();
  assert!(record.is_none(), "used magic link should be removed after cleanup");
}

// ===========================================================================
// 5. test_cleanup_removes_expired_magic_links
// ===========================================================================

#[test]
fn test_cleanup_removes_expired_magic_links() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Store an expired (but not used) magic link
  store_test_magic_link(&engine, &ctx, "expired-link-hash", true, false);

  // Run cleanup
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 0);
  assert_eq!(links, 1, "should have cleaned 1 expired magic link");

  // Verify it's gone
  let record = system_store::get_magic_link(&engine, "expired-link-hash").unwrap();
  assert!(record.is_none(), "expired magic link should be removed after cleanup");
}

// ===========================================================================
// 6. test_cleanup_preserves_unused_valid_links
// ===========================================================================

#[test]
fn test_cleanup_preserves_unused_valid_links() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Store a valid, unused, non-expired magic link
  store_test_magic_link(&engine, &ctx, "valid-link-hash", false, false);

  // Run cleanup
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 0);
  assert_eq!(links, 0, "should NOT clean valid magic links");

  // Verify it still exists
  let record = system_store::get_magic_link(&engine, "valid-link-hash").unwrap();
  assert!(record.is_some(), "valid magic link should be preserved after cleanup");
}

// ===========================================================================
// 7. test_cleanup_empty — no tokens/links exist
// ===========================================================================

#[test]
fn test_cleanup_empty() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Nothing stored — cleanup should return (0, 0)
  let before = engine.durability_snapshot().unwrap().next_sequence;
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 0);
  assert_eq!(links, 0);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before, "empty cleanup must not consume a durability ticket");
}

// ===========================================================================
// 8. test_cleanup_mixed — mix of valid, expired, revoked, used
// ===========================================================================

#[test]
fn test_cleanup_mixed() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // Refresh tokens: 1 valid, 1 expired, 1 revoked
  store_test_refresh_token(&engine, &ctx, "token-valid", false, false);
  store_test_refresh_token(&engine, &ctx, "token-expired", true, false);
  store_test_refresh_token(&engine, &ctx, "token-revoked", false, true);

  // Magic links: 1 valid, 1 expired, 1 used
  store_test_magic_link(&engine, &ctx, "link-valid", false, false);
  store_test_magic_link(&engine, &ctx, "link-expired", true, false);
  store_test_magic_link(&engine, &ctx, "link-used", false, true);

  let before = engine.durability_snapshot().unwrap().next_sequence;
  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 2, "should clean expired + revoked tokens");
  assert_eq!(links, 2, "should clean expired + used links");
  assert_eq!(
    engine.durability_snapshot().unwrap().next_sequence,
    before + 1,
    "one bounded mixed cleanup batch must have one hard acknowledgement"
  );

  // Valid ones should survive
  assert!(system_store::get_refresh_token(&engine, "token-valid").unwrap().is_some());
  assert!(system_store::get_magic_link(&engine, "link-valid").unwrap().is_some());

  // Cleaned ones should be gone
  assert!(system_store::get_refresh_token(&engine, "token-expired").unwrap().is_none());
  assert!(system_store::get_refresh_token(&engine, "token-revoked").unwrap().is_none());
  assert!(system_store::get_magic_link(&engine, "link-expired").unwrap().is_none());
  assert!(system_store::get_magic_link(&engine, "link-used").unwrap().is_none());
}

// ===========================================================================
// 9. test_cleanup_idempotent — running twice should be safe
// ===========================================================================

#[test]
fn test_cleanup_idempotent() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  store_test_refresh_token(&engine, &ctx, "token-exp", true, false);
  store_test_magic_link(&engine, &ctx, "link-used", false, true);

  // First cleanup
  let (tokens1, links1) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens1, 1);
  assert_eq!(links1, 1);

  // Second cleanup — should find nothing
  let (tokens2, links2) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens2, 0);
  assert_eq!(links2, 0);
}

// ===========================================================================
// 10. test_cleanup_both_expired_and_revoked_token
// ===========================================================================

#[test]
fn test_cleanup_both_expired_and_revoked_token() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // A token that is both expired AND revoked (should still be cleaned once)
  store_test_refresh_token(&engine, &ctx, "double-bad-token", true, true);

  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 1);
  assert_eq!(links, 0);

  assert!(system_store::get_refresh_token(&engine, "double-bad-token").unwrap().is_none());
}

// ===========================================================================
// 11. test_cleanup_both_expired_and_used_link
// ===========================================================================

#[test]
fn test_cleanup_both_expired_and_used_link() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  // A magic link that is both expired AND used
  store_test_magic_link(&engine, &ctx, "double-bad-link", true, true);

  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
  assert_eq!(tokens, 0);
  assert_eq!(links, 1);

  assert!(system_store::get_magic_link(&engine, "double-bad-link").unwrap().is_none());
}

#[test]
fn cleanup_rejects_malformed_first_batch_without_partial_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  store_test_refresh_token(&engine, &ctx, "a-expired", true, false);
  ops
    .store_file_buffered(
      &ctx,
      "/.aeordb-system/refresh-tokens/z-malformed",
      br#"{"$v":0,"token_hash":"z-malformed""#,
      Some("application/json"),
    )
    .unwrap();

  let before = engine.durability_snapshot().unwrap().next_sequence;
  let error = system_store::cleanup_expired_tokens(&engine, &ctx).expect_err("malformed authority must fail cleanup");
  assert!(!matches!(error, EngineError::PartialOperation { .. }), "no mutation occurred, so the original corruption must be preserved");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before, "first-batch validation failure must publish nothing");
  assert!(system_store::get_refresh_token(&engine, "a-expired").unwrap().is_some(), "preflight failure must preserve earlier candidates");
}

#[test]
fn cleanup_reports_exact_partial_outcome_after_one_acknowledged_batch() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  for index in 0..128 {
    store_test_refresh_token(&engine, &ctx, &format!("a-expired-{index:03}"), true, false);
  }
  ops
    .store_file_buffered(
      &ctx,
      "/.aeordb-system/refresh-tokens/z-malformed",
      br#"{"$v":0,"token_hash":"z-malformed""#,
      Some("application/json"),
    )
    .unwrap();

  let before = engine.durability_snapshot().unwrap().next_sequence;
  let error = system_store::cleanup_expired_tokens(&engine, &ctx).expect_err("later malformed authority must fail cleanup");
  let EngineError::PartialOperation { operation, completed, failed, evidence } = error else {
    panic!("acknowledged cleanup work must be retained as an exact partial outcome");
  };
  assert_eq!(operation, "credential cleanup");
  assert_eq!(completed, 128);
  assert_eq!(failed, 1);
  assert!(evidence.contains("tokens_cleaned=128"), "partial evidence lost exact token count: {evidence}");
  assert!(evidence.contains("links_cleaned=0"), "partial evidence lost exact link count: {evidence}");
  assert!(evidence.contains("z-malformed"), "partial evidence lost the failing path: {evidence}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before + 1, "one acknowledged batch must consume one hard ticket");
  assert!(system_store::get_refresh_token(&engine, "a-expired-000").unwrap().is_none());
}

#[test]
fn cleanup_memory_refusal_before_first_batch_preserves_authority() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  store_test_refresh_token(&engine, &ctx, "expired-under-pressure", true, false);

  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.expect("test engine must have a memory policy");
  let pressure_bytes = policy.soft_limit_bytes.saturating_sub(snapshot.accounted_bytes);
  let pressure = coordinator.reserve(MemoryOwner::Query, pressure_bytes, AdmissionClass::Workload).unwrap();
  let before = engine.durability_snapshot().unwrap().next_sequence;

  let error = system_store::cleanup_expired_tokens(&engine, &ctx).expect_err("cleanup must honor maintenance memory admission");
  assert!(matches!(error, EngineError::ResourceExhausted(_)));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert!(system_store::get_refresh_token(&engine, "expired-under-pressure").unwrap().is_some());
  drop(pressure);
}

#[test]
fn cleanup_rejects_wrong_entry_type_without_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_symlink(&ctx, "/.aeordb-system/refresh-tokens/not-a-token", "/.aeordb-system/refresh-tokens/elsewhere").unwrap();
  let before = engine.durability_snapshot().unwrap().next_sequence;

  let error = system_store::cleanup_expired_tokens(&engine, &ctx).expect_err("non-file credential authority must fail closed");
  assert!(matches!(error, EngineError::CorruptEntry { .. }));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert_eq!(
    ops.get_symlink("/.aeordb-system/refresh-tokens/not-a-token").unwrap().unwrap().target,
    "/.aeordb-system/refresh-tokens/elsewhere"
  );
}

#[test]
fn cleanup_publishes_exactly_two_bounded_batches_for_129_candidates() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  for index in 0..129 {
    store_test_refresh_token(&engine, &ctx, &format!("expired-{index:03}"), true, false);
  }
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let task_memory_before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;

  let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();

  assert_eq!((tokens, links), (129, 0));
  assert_eq!(
    engine.durability_snapshot().unwrap().next_sequence,
    sequence_before + 2,
    "129 candidates must publish one full 128-record batch and one final batch"
  );
  assert!(system_store::get_refresh_token(&engine, "expired-000").unwrap().is_none());
  assert!(system_store::get_refresh_token(&engine, "expired-128").unwrap().is_none());
  assert_eq!(
    engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes,
    task_memory_before,
    "successful cleanup must release its operation and candidate reservations"
  );
}

#[test]
fn cleanup_rejects_identifier_mismatch_before_first_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  store_test_refresh_token(&engine, &ctx, "a-expired", true, false);
  let mismatched = RefreshTokenRecord {
    token_hash: "different-identity".to_string(),
    user_subject: "test-user".to_string(),
    created_at: Utc::now() - Duration::hours(2),
    expires_at: Utc::now() - Duration::hours(1),
    is_revoked: false,
    key_id: None,
  };
  ops
    .store_file_buffered(
      &ctx,
      "/.aeordb-system/refresh-tokens/z-path-identity",
      &mismatched.serialize_versioned(),
      Some("application/json"),
    )
    .unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let task_memory_before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;

  let error = system_store::cleanup_expired_tokens(&engine, &ctx).expect_err("path and record identities must agree");

  assert!(matches!(error, EngineError::CorruptEntry { .. }));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(system_store::get_refresh_token(&engine, "a-expired").unwrap().is_some());
  assert_eq!(
    engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes,
    task_memory_before,
    "failed cleanup must release its operation and candidate reservations"
  );
}

#[test]
fn cleanup_rejects_oversized_persisted_record_without_buffering_or_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  store_test_refresh_token(&engine, &ctx, "a-expired", true, false);
  let oversized = vec![b'x'; 1024 * 1024 + 1];
  ops.store_file_buffered(&ctx, "/.aeordb-system/refresh-tokens/z-oversized", &oversized, Some("application/json")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let task_memory_before = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes;

  let error = system_store::cleanup_expired_tokens(&engine, &ctx).expect_err("oversized authority must fail before body buffering");

  assert!(matches!(error, EngineError::ResourceExhausted(_)));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(system_store::get_refresh_token(&engine, "a-expired").unwrap().is_some());
  assert_eq!(
    engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes,
    task_memory_before,
    "oversized-record refusal must release cleanup memory"
  );
}
