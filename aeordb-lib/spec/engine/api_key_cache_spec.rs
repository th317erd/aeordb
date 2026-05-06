use std::sync::Arc;

use aeordb::auth::api_key::{ApiKeyRecord, hash_api_key};
use aeordb::engine::cache::Cache;
use aeordb::engine::cache_loaders::ApiKeyLoader;
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
    let cache = Cache::new(ApiKeyLoader);
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
    let cache = Cache::new(ApiKeyLoader);

    let result = cache.get(&record.key_id.to_string(), &engine).unwrap();
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
    let cache = Cache::new(ApiKeyLoader);

    // First call: cache miss, loads from engine.
    let r1 = cache.get(&record.key_id.to_string(), &engine).unwrap();
    assert!(r1.is_some());

    // Second call: should be a cache hit (same result).
    let r2 = cache.get(&record.key_id.to_string(), &engine).unwrap();
    assert!(r2.is_some());
    assert_eq!(r2.unwrap().key_id, record.key_id);
}

// ===========================================================================
// Non-existent key returns None
// ===========================================================================

#[test]
fn get_key_returns_none_for_nonexistent_uuid() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let cache = Cache::new(ApiKeyLoader);

    let fake_id = Uuid::new_v4().to_string();
    let result = cache.get(&fake_id, &engine).unwrap();
    assert!(result.is_none());
}

// ===========================================================================
// Invalid UUID string returns None (not error)
// ===========================================================================

#[test]
fn get_key_returns_none_for_invalid_uuid() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let cache = Cache::new(ApiKeyLoader);

    let result = cache.get(&"not-a-uuid".to_string(), &engine).unwrap();
    assert!(result.is_none());
}

#[test]
fn get_key_returns_none_for_empty_string() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let cache = Cache::new(ApiKeyLoader);

    let result = cache.get(&String::new(), &engine).unwrap();
    assert!(result.is_none());
}

// ===========================================================================
// Evict removes a cached key
// ===========================================================================

#[test]
fn evict_removes_cached_key() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = Cache::new(ApiKeyLoader);

    let key_str = record.key_id.to_string();

    // Load into cache.
    let _ = cache.get(&key_str, &engine).unwrap();

    // Evict.
    cache.evict(&key_str);

    // Next get should re-load from engine (still finds it because engine has it).
    let r = cache.get(&key_str, &engine).unwrap();
    assert!(r.is_some());
}

#[test]
fn evict_nonexistent_key_does_not_panic() {
    let cache = Cache::new(ApiKeyLoader);
    // Should not panic.
    cache.evict(&"nonexistent-key-id".to_string());
}

// ===========================================================================
// evict_all clears the entire cache
// ===========================================================================

#[test]
fn evict_all_clears_all_entries() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let r1 = store_test_key(&engine);
    let r2 = store_test_key(&engine);
    let cache = Cache::new(ApiKeyLoader);

    // Load both.
    cache.get(&r1.key_id.to_string(), &engine).unwrap();
    cache.get(&r2.key_id.to_string(), &engine).unwrap();

    // Evict all.
    cache.evict_all();

    // Both should re-load (still exist in engine, so still return Some).
    let re1 = cache.get(&r1.key_id.to_string(), &engine).unwrap();
    let re2 = cache.get(&r2.key_id.to_string(), &engine).unwrap();
    assert!(re1.is_some());
    assert!(re2.is_some());
}

#[test]
fn evict_all_on_empty_cache_does_not_panic() {
    let cache = Cache::new(ApiKeyLoader);
    cache.evict_all();
    cache.evict_all(); // Double evict should also be fine.
}

// ===========================================================================
// Multiple distinct keys
// ===========================================================================

#[test]
fn multiple_keys_cached_independently() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let r1 = store_test_key(&engine);
    let r2 = store_test_key(&engine);
    let cache = Cache::new(ApiKeyLoader);

    let c1 = cache.get(&r1.key_id.to_string(), &engine).unwrap().unwrap();
    let c2 = cache.get(&r2.key_id.to_string(), &engine).unwrap().unwrap();

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
    let cache = Arc::new(Cache::new(ApiKeyLoader));
    let key_str = record.key_id.to_string();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let cache = Arc::clone(&cache);
            let engine = Arc::clone(&engine);
            let ks = key_str.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    let result = cache.get(&ks, &engine).unwrap();
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
fn concurrent_evict_and_get_do_not_panic() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let record = store_test_key(&engine);
    let cache = Arc::new(Cache::new(ApiKeyLoader));
    let key_str = record.key_id.to_string();

    // Pre-load.
    cache.get(&key_str, &engine).unwrap();

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let cache = Arc::clone(&cache);
            let engine = Arc::clone(&engine);
            let ks = key_str.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    if i % 2 == 0 {
                        cache.evict(&ks);
                    } else {
                        let _ = cache.get(&ks, &engine);
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
    let cache = Arc::new(Cache::new(ApiKeyLoader));
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
                        let _ = cache.get(&ks, &engine);
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
// Revoked key is still cached (cache doesn't check revocation)
// ===========================================================================

#[test]
fn revoked_key_is_still_returned_from_cache() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let mut record = store_test_key(&engine);

    let cache = Cache::new(ApiKeyLoader);
    let key_str = record.key_id.to_string();

    // Load into cache.
    cache.get(&key_str, &engine).unwrap();

    // Now revoke the key in the engine.
    record.is_revoked = true;
    let ctx = RequestContext::system();
    system_store::store_api_key(&engine, &ctx, &record).unwrap();

    // Cache still holds the old (non-revoked) version until evicted.
    let cached = cache.get(&key_str, &engine).unwrap().unwrap();
    assert!(!cached.is_revoked, "cached version should still show non-revoked until evicted");

    // After explicit eviction, next fetch sees the revoked version.
    cache.evict(&key_str);
    let refreshed = cache.get(&key_str, &engine).unwrap().unwrap();
    assert!(refreshed.is_revoked, "after eviction, should see revoked=true");
}
