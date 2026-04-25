use std::sync::Arc;
use std::time::Duration;

use aeordb::auth::api_key::{ApiKeyRecord, hash_api_key};
use aeordb::engine::api_key_cache::ApiKeyCache;
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::system_store;
use aeordb::server::create_temp_engine_for_tests;
use chrono::Utc;
use uuid::Uuid;

/// Helper: create and store a test API key record in the engine.
fn store_test_key(engine: &Arc<aeordb::engine::storage_engine::StorageEngine>) -> ApiKeyRecord {
    let key_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let record = ApiKeyRecord {
        key_id,
        key_hash: hash_api_key("test_secret").unwrap(),
        user_id: Some(user_id),
        created_at: Utc::now(),
        is_revoked: false,
        expires_at: Utc::now().timestamp_millis() + 86_400_000,
        label: Some("test-key".to_string()),
        rules: vec![],
    };
    let ctx = RequestContext::system();
    system_store::store_api_key(engine, &ctx, &record).unwrap();
    record
}

// ===========================================================================
// Construction
// ===========================================================================

#[test]
fn new_cache_is_empty() {
    let cache = ApiKeyCache::new(Duration::from_secs(60));
    // evict_all on an empty cache should not panic.
    cache.evict_all();
}

// ===========================================================================
// Cache miss -> loads from engine
// ===========================================================================

#[test]
fn get_key_loads_from_engine_on_miss() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    let result = cache.get_key(&record.key_id.to_string(), &engine).unwrap();
    assert!(result.is_some());
    let cached = result.unwrap();
    assert_eq!(cached.key_id, record.key_id);
    assert_eq!(cached.user_id, record.user_id);
}

// ===========================================================================
// Cache hit -> returns cached value without re-reading engine
// ===========================================================================

#[test]
fn get_key_returns_cached_value_on_hit() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    // First call: cache miss, loads from engine.
    let r1 = cache.get_key(&record.key_id.to_string(), &engine).unwrap();
    assert!(r1.is_some());

    // Second call: should be a cache hit (same result).
    let r2 = cache.get_key(&record.key_id.to_string(), &engine).unwrap();
    assert!(r2.is_some());
    assert_eq!(r2.unwrap().key_id, record.key_id);
}

// ===========================================================================
// Non-existent key returns None
// ===========================================================================

#[test]
fn get_key_returns_none_for_nonexistent_uuid() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    let fake_id = Uuid::new_v4().to_string();
    let result = cache.get_key(&fake_id, &engine).unwrap();
    assert!(result.is_none());
}

// ===========================================================================
// Invalid UUID string returns None (not error)
// ===========================================================================

#[test]
fn get_key_returns_none_for_invalid_uuid() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    let result = cache.get_key("not-a-uuid", &engine).unwrap();
    assert!(result.is_none());
}

#[test]
fn get_key_returns_none_for_empty_string() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    let result = cache.get_key("", &engine).unwrap();
    assert!(result.is_none());
}

// ===========================================================================
// Invalidate removes a cached key
// ===========================================================================

#[test]
fn invalidate_removes_cached_key() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    let key_str = record.key_id.to_string();

    // Load into cache.
    let _ = cache.get_key(&key_str, &engine).unwrap();

    // Invalidate.
    cache.invalidate(&key_str);

    // Next get should re-load from engine (still finds it because engine has it).
    let r = cache.get_key(&key_str, &engine).unwrap();
    assert!(r.is_some());
}

#[test]
fn invalidate_nonexistent_key_does_not_panic() {
    let cache = ApiKeyCache::new(Duration::from_secs(60));
    // Should not panic.
    cache.invalidate("nonexistent-key-id");
}

// ===========================================================================
// evict_all clears the entire cache
// ===========================================================================

#[test]
fn evict_all_clears_all_entries() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let r1 = store_test_key(&engine);
    let r2 = store_test_key(&engine);
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    // Load both.
    cache.get_key(&r1.key_id.to_string(), &engine).unwrap();
    cache.get_key(&r2.key_id.to_string(), &engine).unwrap();

    // Evict all.
    cache.evict_all();

    // Both should re-load (still exist in engine, so still return Some).
    let re1 = cache.get_key(&r1.key_id.to_string(), &engine).unwrap();
    let re2 = cache.get_key(&r2.key_id.to_string(), &engine).unwrap();
    assert!(re1.is_some());
    assert!(re2.is_some());
}

#[test]
fn evict_all_on_empty_cache_does_not_panic() {
    let cache = ApiKeyCache::new(Duration::from_secs(60));
    cache.evict_all();
    cache.evict_all(); // Double evict should also be fine.
}

// ===========================================================================
// TTL expiration
// ===========================================================================

#[test]
fn expired_entry_is_reloaded_from_engine() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);

    // Use a very short TTL so it expires immediately.
    let cache = ApiKeyCache::new(Duration::from_nanos(1));

    let key_str = record.key_id.to_string();

    // First call loads from engine.
    let r1 = cache.get_key(&key_str, &engine).unwrap();
    assert!(r1.is_some());

    // Entry should already be expired by the time we call again.
    // The second call should still succeed (reloads from engine).
    let r2 = cache.get_key(&key_str, &engine).unwrap();
    assert!(r2.is_some());
    assert_eq!(r2.unwrap().key_id, record.key_id);
}

// ===========================================================================
// Multiple distinct keys
// ===========================================================================

#[test]
fn multiple_keys_cached_independently() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let r1 = store_test_key(&engine);
    let r2 = store_test_key(&engine);
    let cache = ApiKeyCache::new(Duration::from_secs(300));

    let c1 = cache.get_key(&r1.key_id.to_string(), &engine).unwrap().unwrap();
    let c2 = cache.get_key(&r2.key_id.to_string(), &engine).unwrap().unwrap();

    assert_eq!(c1.key_id, r1.key_id);
    assert_eq!(c2.key_id, r2.key_id);
    assert_ne!(c1.key_id, c2.key_id);
}

// ===========================================================================
// Concurrent access (basic thread safety)
// ===========================================================================

#[test]
fn concurrent_reads_do_not_panic() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = Arc::new(ApiKeyCache::new(Duration::from_secs(300)));
    let key_str = record.key_id.to_string();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let cache = Arc::clone(&cache);
            let engine = Arc::clone(&engine);
            let ks = key_str.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    let result = cache.get_key(&ks, &engine).unwrap();
                    assert!(result.is_some());
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn concurrent_invalidate_and_get_do_not_panic() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = Arc::new(ApiKeyCache::new(Duration::from_secs(300)));
    let key_str = record.key_id.to_string();

    // Pre-load.
    cache.get_key(&key_str, &engine).unwrap();

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let cache = Arc::clone(&cache);
            let engine = Arc::clone(&engine);
            let ks = key_str.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    if i % 2 == 0 {
                        cache.invalidate(&ks);
                    } else {
                        let _ = cache.get_key(&ks, &engine);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn concurrent_evict_all_and_get_do_not_panic() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = Arc::new(ApiKeyCache::new(Duration::from_secs(300)));
    let key_str = record.key_id.to_string();

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let cache = Arc::clone(&cache);
            let engine = Arc::clone(&engine);
            let ks = key_str.clone();
            std::thread::spawn(move || {
                for _ in 0..5 {
                    if i == 0 {
                        cache.evict_all();
                    } else {
                        let _ = cache.get_key(&ks, &engine);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

// ===========================================================================
// Zero-second TTL
// ===========================================================================

#[test]
fn zero_ttl_always_reloads() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = ApiKeyCache::new(Duration::ZERO);

    let key_str = record.key_id.to_string();

    // Every call is effectively a cache miss because TTL is zero.
    for _ in 0..3 {
        let r = cache.get_key(&key_str, &engine).unwrap();
        assert!(r.is_some());
        assert_eq!(r.unwrap().key_id, record.key_id);
    }
}

// ===========================================================================
// Revoked key is still cached (cache doesn't check revocation)
// ===========================================================================

#[test]
fn revoked_key_is_still_returned_from_cache() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let mut record = store_test_key(&engine);

    let cache = ApiKeyCache::new(Duration::from_secs(300));
    let key_str = record.key_id.to_string();

    // Load into cache.
    cache.get_key(&key_str, &engine).unwrap();

    // Now revoke the key in the engine.
    record.is_revoked = true;
    let ctx = RequestContext::system();
    system_store::store_api_key(&engine, &ctx, &record).unwrap();

    // Cache still holds the old (non-revoked) version until TTL expires or invalidated.
    let cached = cache.get_key(&key_str, &engine).unwrap().unwrap();
    assert!(!cached.is_revoked, "cached version should still show non-revoked until invalidated");

    // After explicit invalidation, next fetch sees the revoked version.
    cache.invalidate(&key_str);
    let refreshed = cache.get_key(&key_str, &engine).unwrap().unwrap();
    assert!(refreshed.is_revoked, "after invalidation, should see revoked=true");
}
