use chrono::{Duration, Utc};

use aeordb::auth::magic_link::MagicLinkRecord;
use aeordb::auth::refresh::RefreshTokenRecord;
use aeordb::engine::system_store;
use aeordb::engine::RequestContext;
use aeordb::server::create_temp_engine_for_tests;

// ===========================================================================
// Helper: store a refresh token record directly into the system store.
// ===========================================================================

fn store_test_refresh_token(
    engine: &aeordb::engine::StorageEngine,
    ctx: &RequestContext,
    token_hash: &str,
    expired: bool,
    revoked: bool,
) {
    let expires_at = if expired {
        Utc::now() - Duration::hours(1)
    } else {
        Utc::now() + Duration::hours(24)
    };

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

fn store_test_magic_link(
    engine: &aeordb::engine::StorageEngine,
    ctx: &RequestContext,
    code_hash: &str,
    expired: bool,
    used: bool,
) {
    let expires_at = if expired {
        Utc::now() - Duration::minutes(30)
    } else {
        Utc::now() + Duration::minutes(10)
    };

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
    let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
    assert_eq!(tokens, 0);
    assert_eq!(links, 0);
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

    let (tokens, links) = system_store::cleanup_expired_tokens(&engine, &ctx).unwrap();
    assert_eq!(tokens, 2, "should clean expired + revoked tokens");
    assert_eq!(links, 2, "should clean expired + used links");

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
