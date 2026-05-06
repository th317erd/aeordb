# Unified Cache Interface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace three duplicated TTL-based caches with a single generic `Cache<L>` backed by a `CacheLoader` trait, add index config caching, and wire up proper eviction on all write paths.

**Architecture:** Generic `Cache<L: CacheLoader>` struct with `RwLock<HashMap>`, no TTL, explicit eviction only. Four domain loaders implement `CacheLoader`. Permissions and index config caches live on `StorageEngine` (engine-layer access); group and API key caches live on `AppState` (server-layer only).

**Tech Stack:** Rust, std::sync::RwLock, std::collections::HashMap

**Spec:** `docs/superpowers/specs/2026-05-05-unified-cache-design.md`

---

### Task 1: Create the generic Cache and CacheLoader trait

**Files:**
- Create: `aeordb-lib/src/engine/cache.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Write the cache module with unit tests**

Create `aeordb-lib/src/engine/cache.rs`:

```rust
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::RwLock;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::storage_engine::StorageEngine;

/// Trait for loading a cache entry on miss. Implementors define the key/value
/// types and how to fetch a value from the engine when it's not cached.
pub trait CacheLoader: Send + Sync {
    type Key: Eq + Hash + Clone + Send + Sync;
    type Value: Clone + Send + Sync;

    fn load(&self, key: &Self::Key, engine: &StorageEngine) -> EngineResult<Self::Value>;
}

/// Generic eviction-based cache. No TTL — entries live until explicitly evicted.
/// Uses RwLock for concurrent reads on the hot path.
pub struct Cache<L: CacheLoader> {
    entries: RwLock<HashMap<L::Key, L::Value>>,
    loader: L,
}

impl<L: CacheLoader> Cache<L> {
    /// Create a new cache with the given loader.
    pub fn new(loader: L) -> Self {
        Cache {
            entries: RwLock::new(HashMap::new()),
            loader,
        }
    }

    /// Get a value by key. Returns the cached value if present, otherwise
    /// calls the loader, caches the result, and returns it.
    /// Errors from the loader are propagated (not cached).
    pub fn get(&self, key: &L::Key, engine: &StorageEngine) -> EngineResult<L::Value> {
        // Fast path: read lock
        {
            let entries = self.entries.read().map_err(|e| {
                EngineError::IoError(std::io::Error::other(format!("Cache read lock poisoned: {}", e)))
            })?;
            if let Some(value) = entries.get(key) {
                return Ok(value.clone());
            }
        }

        // Cache miss: load from source
        let value = self.loader.load(key, engine)?;

        // Store in cache (write lock)
        {
            let mut entries = self.entries.write().map_err(|e| {
                EngineError::IoError(std::io::Error::other(format!("Cache write lock poisoned: {}", e)))
            })?;
            entries.insert(key.clone(), value.clone());
        }

        Ok(value)
    }

    /// Evict a single entry by key.
    pub fn evict(&self, key: &L::Key) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(key);
        }
    }

    /// Flush the entire cache.
    pub fn evict_all(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock loader that counts how many times load() is called.
    struct CountingLoader {
        load_count: AtomicUsize,
    }

    impl CountingLoader {
        fn new() -> Self {
            CountingLoader { load_count: AtomicUsize::new(0) }
        }
        fn count(&self) -> usize {
            self.load_count.load(Ordering::Relaxed)
        }
    }

    impl CacheLoader for CountingLoader {
        type Key = String;
        type Value = String;

        fn load(&self, key: &String, _engine: &StorageEngine) -> EngineResult<String> {
            self.load_count.fetch_add(1, Ordering::Relaxed);
            Ok(format!("value-for-{}", key))
        }
    }

    fn make_test_engine() -> StorageEngine {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.aeordb");
        let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
        // Leak temp dir so it lives long enough
        std::mem::forget(temp);
        engine
    }

    #[test]
    fn test_get_loads_on_miss() {
        let engine = make_test_engine();
        let cache = Cache::new(CountingLoader::new());

        let val = cache.get(&"foo".to_string(), &engine).unwrap();
        assert_eq!(val, "value-for-foo");
        assert_eq!(cache.loader.count(), 1);
    }

    #[test]
    fn test_get_returns_cached_on_hit() {
        let engine = make_test_engine();
        let cache = Cache::new(CountingLoader::new());

        cache.get(&"foo".to_string(), &engine).unwrap();
        cache.get(&"foo".to_string(), &engine).unwrap();
        cache.get(&"foo".to_string(), &engine).unwrap();

        // Loader called only once
        assert_eq!(cache.loader.count(), 1);
    }

    #[test]
    fn test_evict_causes_reload() {
        let engine = make_test_engine();
        let cache = Cache::new(CountingLoader::new());

        cache.get(&"foo".to_string(), &engine).unwrap();
        assert_eq!(cache.loader.count(), 1);

        cache.evict(&"foo".to_string());

        cache.get(&"foo".to_string(), &engine).unwrap();
        assert_eq!(cache.loader.count(), 2);
    }

    #[test]
    fn test_evict_all_flushes_everything() {
        let engine = make_test_engine();
        let cache = Cache::new(CountingLoader::new());

        cache.get(&"a".to_string(), &engine).unwrap();
        cache.get(&"b".to_string(), &engine).unwrap();
        assert_eq!(cache.loader.count(), 2);

        cache.evict_all();

        cache.get(&"a".to_string(), &engine).unwrap();
        cache.get(&"b".to_string(), &engine).unwrap();
        assert_eq!(cache.loader.count(), 4);
    }

    #[test]
    fn test_evict_nonexistent_key_is_noop() {
        let cache = Cache::new(CountingLoader::new());
        cache.evict(&"nonexistent".to_string()); // Should not panic
    }
}
```

- [ ] **Step 2: Register the module in mod.rs**

In `aeordb-lib/src/engine/mod.rs`, add after line 8 (after `pub mod backup;`):

```rust
pub mod cache;
```

And add the public export after the existing exports (around line 87):

```rust
pub use cache::{Cache, CacheLoader};
```

- [ ] **Step 3: Run tests to verify**

Run: `cargo test -p aeordb --lib engine::cache::tests -- --nocapture`
Expected: All 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/cache.rs aeordb-lib/src/engine/mod.rs
git commit -m "feat: generic Cache<L: CacheLoader> with eviction-based invalidation"
```

---

### Task 2: Create the four domain loaders

**Files:**
- Create: `aeordb-lib/src/engine/cache_loaders.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Create cache_loaders.rs with all four loaders**

Create `aeordb-lib/src/engine/cache_loaders.rs`:

```rust
use uuid::Uuid;

use crate::auth::api_key::ApiKeyRecord;
use crate::engine::cache::CacheLoader;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::group::Group;
use crate::engine::index_config::PathIndexConfig;
use crate::engine::permissions::PathPermissions;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::system_store;
use crate::engine::user::User;

/// Loads `.aeordb-permissions` files from directory paths.
pub struct PermissionsLoader;

impl CacheLoader for PermissionsLoader {
    type Key = String;
    type Value = Option<PathPermissions>;

    fn load(&self, path: &String, engine: &StorageEngine) -> EngineResult<Option<PathPermissions>> {
        let ops = DirectoryOps::new(engine);
        let permissions_path = if path == "/" || path.ends_with('/') {
            format!("{}.aeordb-permissions", path)
        } else {
            format!("{}/.aeordb-permissions", path)
        };

        match ops.read_file(&permissions_path) {
            Ok(data) => {
                let permissions = PathPermissions::deserialize(&data)?;
                Ok(Some(permissions))
            }
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Loads group memberships for a user by user_id.
pub struct GroupLoader;

impl CacheLoader for GroupLoader {
    type Key = Uuid;
    type Value = Vec<String>;

    fn load(&self, user_id: &Uuid, engine: &StorageEngine) -> EngineResult<Vec<String>> {
        let user: User = match system_store::get_user(engine, user_id)? {
            Some(user) => user,
            None => return Ok(Vec::new()),
        };

        let all_groups: Vec<Group> = system_store::list_groups(engine)?;

        let mut member_groups = Vec::new();
        for group in &all_groups {
            if group.evaluate_membership(&user) {
                member_groups.push(group.name.clone());
            }
        }

        Ok(member_groups)
    }
}

/// Loads API key records by key_id string.
pub struct ApiKeyLoader;

impl CacheLoader for ApiKeyLoader {
    type Key = String;
    type Value = Option<ApiKeyRecord>;

    fn load(&self, key_id: &String, engine: &StorageEngine) -> EngineResult<Option<ApiKeyRecord>> {
        let key_uuid = match Uuid::parse_str(key_id) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };

        let all_keys = system_store::list_api_keys(engine)?;
        Ok(all_keys.into_iter().find(|k| k.key_id == key_uuid))
    }
}

/// Loads `.aeordb-config/indexes.json` from directory paths.
pub struct IndexConfigLoader;

impl CacheLoader for IndexConfigLoader {
    type Key = String;
    type Value = Option<PathIndexConfig>;

    fn load(&self, path: &String, engine: &StorageEngine) -> EngineResult<Option<PathIndexConfig>> {
        let ops = DirectoryOps::new(engine);
        let config_path = if path.ends_with('/') {
            format!("{}.aeordb-config/indexes.json", path)
        } else {
            format!("{}/.aeordb-config/indexes.json", path)
        };

        match ops.read_file(&config_path) {
            Ok(data) => PathIndexConfig::deserialize(&data).map(Some),
            Err(EngineError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
```

- [ ] **Step 2: Register the module in mod.rs**

In `aeordb-lib/src/engine/mod.rs`, add after the `pub mod cache;` line:

```rust
pub mod cache_loaders;
```

And add the public export:

```rust
pub use cache_loaders::{PermissionsLoader, GroupLoader, ApiKeyLoader, IndexConfigLoader};
```

- [ ] **Step 3: Build to verify compilation**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Expected: No errors.

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/cache_loaders.rs aeordb-lib/src/engine/mod.rs
git commit -m "feat: domain-specific CacheLoader implementations for permissions, groups, API keys, index configs"
```

---

### Task 3: Add caches to StorageEngine and AppState

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs:120-240`
- Modify: `aeordb-lib/src/server/state.rs:20-39`
- Modify: `aeordb-lib/src/server/mod.rs:200-230`

- [ ] **Step 1: Add cache fields to StorageEngine**

In `aeordb-lib/src/engine/storage_engine.rs`, add imports at the top of the file:

```rust
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{PermissionsLoader, IndexConfigLoader};
```

Add two fields to `pub struct StorageEngine` (after the `_file_lock` field, before the closing `}`):

```rust
  pub permissions_cache: Arc<Cache<PermissionsLoader>>,
  pub index_config_cache: Arc<Cache<IndexConfigLoader>>,
```

In `create_with_hot_dir`, add the cache fields to the `StorageEngine` struct literal (before `_file_lock: lock_file,`):

```rust
      permissions_cache: Arc::new(Cache::new(PermissionsLoader)),
      index_config_cache: Arc::new(Cache::new(IndexConfigLoader)),
```

Find all other places where `StorageEngine { ... }` is constructed (the `open_with_hot_dir` method has one too) and add the same two fields.

- [ ] **Step 2: Update AppState — replace permissions_cache, keep group and API key caches**

In `aeordb-lib/src/server/state.rs`, replace the three cache imports and fields.

Remove from imports:
```rust
// Remove these use statements if present:
// use crate::engine::PermissionsCache;
// use crate::engine::GroupCache;
// use crate::engine::ApiKeyCache;
```

Add imports:
```rust
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{GroupLoader, ApiKeyLoader};
```

In `pub struct AppState`, replace the cache fields:

```rust
  // Old:
  // pub group_cache: Arc<GroupCache>,
  // pub permissions_cache: Arc<PermissionsCache>,
  // pub api_key_cache: Arc<ApiKeyCache>,

  // New:
  pub group_cache: Arc<Cache<GroupLoader>>,
  pub api_key_cache: Arc<Cache<ApiKeyLoader>>,
```

Remove the `permissions_cache` field entirely — it now lives on `StorageEngine`.

- [ ] **Step 3: Update server construction in mod.rs**

In `aeordb-lib/src/server/mod.rs`, in the `create_app_with_all` function (around line 200-230):

Remove the old cache construction:
```rust
  // Remove these lines:
  // let cache_ttl = Duration::from_secs(DEFAULT_CACHE_TTL_SECONDS);
  // let group_cache = Arc::new(GroupCache::new(cache_ttl));
  // let permissions_cache = Arc::new(PermissionsCache::new(cache_ttl));
  // let api_key_cache = Arc::new(ApiKeyCache::new(cache_ttl));
```

Add the new construction:
```rust
  let group_cache = Arc::new(Cache::new(GroupLoader));
  let api_key_cache = Arc::new(Cache::new(ApiKeyLoader));
```

Update the `AppState` struct literal to remove `permissions_cache` and use the new types for `group_cache` and `api_key_cache`.

Also remove `DEFAULT_CACHE_TTL_SECONDS` constant and the `use std::time::Duration` if no longer needed.

Add imports at the top:
```rust
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{GroupLoader, ApiKeyLoader};
```

Remove old imports:
```rust
// Remove: GroupCache, PermissionsCache, ApiKeyCache from the engine use statement
```

- [ ] **Step 4: Build to verify — expect compilation errors from call sites**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -20`
Expected: Errors at call sites that still reference old types/methods. These are fixed in the next tasks.

- [ ] **Step 5: Commit (WIP — call sites not yet updated)**

```bash
git add aeordb-lib/src/engine/storage_engine.rs aeordb-lib/src/server/state.rs aeordb-lib/src/server/mod.rs
git commit -m "wip: add Cache fields to StorageEngine and AppState, remove old cache types"
```

---

### Task 4: Update call sites — PermissionResolver and permission_middleware

**Files:**
- Modify: `aeordb-lib/src/engine/permission_resolver.rs`
- Modify: `aeordb-lib/src/auth/permission_middleware.rs`
- Modify: `aeordb-lib/src/server/engine_routes.rs` (permission-related calls)

- [ ] **Step 1: Update PermissionResolver to use engine.permissions_cache**

In `aeordb-lib/src/engine/permission_resolver.rs`:

Replace imports:
```rust
// Remove:
// use crate::engine::group_cache::GroupCache;
// use crate::engine::permissions_cache::PermissionsCache;

// Add:
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{PermissionsLoader, GroupLoader};
```

Update the struct and constructor:
```rust
pub struct PermissionResolver<'a> {
  engine: &'a StorageEngine,
  group_cache: &'a Cache<GroupLoader>,
}

impl<'a> PermissionResolver<'a> {
  pub fn new(
    engine: &'a StorageEngine,
    group_cache: &'a Cache<GroupLoader>,
  ) -> Self {
    PermissionResolver {
      engine,
      group_cache,
    }
  }
```

Update `check_permission` method — change the two cache calls:
```rust
    // Old: let user_groups = self.group_cache.get_groups(user_id, self.engine)?;
    let user_groups = self.group_cache.get(user_id, self.engine)?;

    // Old: let permissions = self.permissions_cache.get_permissions(level, self.engine)?;
    let permissions = self.engine.permissions_cache.get(&level.to_string(), self.engine)?;
```

Note: `level` is already a `String` from `path_levels()`, so `&level` works. But since `get` takes `&L::Key` which is `&String`, we need `&level` which is `&String`. Check the `path_levels` return type — it returns `Vec<String>`, so `level` in the for loop is `&String`. Pass it directly.

- [ ] **Step 2: Update permission_middleware.rs — API key cache calls**

In `aeordb-lib/src/auth/permission_middleware.rs`:

Find all `state.api_key_cache.get_key(key_id, &state.engine)` calls and replace with:
```rust
state.api_key_cache.get(&key_id.to_string(), &state.engine)
```

Find all `PermissionResolver::new(engine, group_cache, permissions_cache)` calls and replace with:
```rust
PermissionResolver::new(engine, group_cache)
```

Remove any references to `state.permissions_cache` — the resolver accesses it through `engine.permissions_cache` now.

- [ ] **Step 3: Update engine_routes.rs — attach_effective_permissions call**

In `aeordb-lib/src/server/engine_routes.rs`, find the `attach_effective_permissions` function call (around line 749) and any `PermissionResolver::new` calls. Update them to pass only `engine` and `group_cache` (remove `permissions_cache` parameter).

- [ ] **Step 4: Update share_routes.rs — group_cache and permissions eviction calls**

In `aeordb-lib/src/server/share_routes.rs`:

Replace `state.permissions_cache.evict_path(&perm_dir)` with:
```rust
state.engine.permissions_cache.evict(&perm_dir);
```

Replace `state.group_cache.get_groups(&caller_id, &state.engine)` with:
```rust
state.group_cache.get(&caller_id, &state.engine)
```

- [ ] **Step 5: Update other call sites**

Search for any remaining references to old cache types:
```bash
grep -rn "get_key\|get_groups\|get_permissions\|evict_path\|invalidate\|GroupCache\|PermissionsCache\|ApiKeyCache" aeordb-lib/src/ | grep -v "mod.rs" | grep -v "cache.rs" | grep -v "cache_loaders.rs"
```

Update each remaining call site:
- `sync_routes.rs`: `api_key_cache.get_key(...)` → `api_key_cache.get(...)`
- `sse_routes.rs`: `api_key_cache.get_key(...)` → `api_key_cache.get(...)`
- `api_key_self_service_routes.rs`: `api_key_cache.invalidate(...)` → `api_key_cache.evict(...)`
- `share_link_routes.rs`: `api_key_cache.invalidate(...)` → `api_key_cache.evict(...)`
- `routes.rs`: `api_key_cache.invalidate(...)` → `api_key_cache.evict(...)`

- [ ] **Step 6: Build to verify compilation**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -20`
Expected: No errors. If errors remain, fix remaining call sites.

- [ ] **Step 7: Run existing tests**

Run: `cargo test -p aeordb --lib 2>&1 | tail -5`
Expected: All tests pass.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "refactor: migrate all cache call sites to unified Cache<L> interface"
```

---

### Task 5: Wire up IndexConfigLoader in IndexingPipeline

**Files:**
- Modify: `aeordb-lib/src/engine/indexing_pipeline.rs:279-292`

- [ ] **Step 1: Replace load_config with cache lookup**

In `aeordb-lib/src/engine/indexing_pipeline.rs`, update the `load_config` method (around line 279):

```rust
  fn load_config(&self, parent: &str) -> EngineResult<Option<PathIndexConfig>> {
      self.engine.index_config_cache.get(&parent.to_string(), self.engine)
  }
```

This replaces the previous implementation that called `DirectoryOps::read_file` directly. The `IndexConfigLoader` handles the path construction and file reading on cache miss.

- [ ] **Step 2: Build and test**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Expected: No errors.

Run: `cargo test -p aeordb --lib 2>&1 | tail -5`
Expected: All tests pass.

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/engine/indexing_pipeline.rs
git commit -m "perf: index config lookups use Cache<IndexConfigLoader> instead of disk reads"
```

---

### Task 6: Wire up eviction on write/delete/rename paths

**Files:**
- Modify: `aeordb-lib/src/server/engine_routes.rs`

- [ ] **Step 1: Add a helper function for system path eviction**

Add this function near the top of `engine_routes.rs` (after the imports):

```rust
/// Evict cache entries when a system file is written, deleted, or renamed.
fn evict_caches_for_path(state: &AppState, path: &str) {
    let normalized = crate::engine::path_utils::normalize_path(path);

    if normalized.ends_with("/.aeordb-permissions") || normalized == "/.aeordb-permissions" {
        // Evict permissions cache for the parent directory
        let parent = crate::engine::path_utils::parent_path(&normalized)
            .unwrap_or_else(|| "/".to_string());
        state.engine.permissions_cache.evict(&parent);
    }

    if normalized.ends_with("/.aeordb-config/indexes.json") {
        // Evict index config cache: /X/.aeordb-config/indexes.json → evict key /X
        // Strip "/.aeordb-config/indexes.json" from the end
        if let Some(dir) = normalized.strip_suffix("/.aeordb-config/indexes.json") {
            let key = if dir.is_empty() { "/".to_string() } else { dir.to_string() };
            state.engine.index_config_cache.evict(&key);
        }
    }

    if normalized.starts_with("/.aeordb-system/api-keys/") {
        // Evict API key cache for this key_id
        if let Some(key_id) = crate::engine::path_utils::file_name(&normalized) {
            state.api_key_cache.evict(&key_id.to_string());
        }
    }

    if normalized.starts_with("/.aeordb-system/groups/")
        || normalized.starts_with("/.aeordb-system/users/")
    {
        // Group membership is cross-cutting — flush the whole cache
        state.group_cache.evict_all();
    }
}
```

- [ ] **Step 2: Add eviction call to engine_store_file (write handler)**

In `engine_store_file` (around line 198), after the successful write response is constructed (the `Ok(record)` arm), add:

```rust
evict_caches_for_path(&state, &path);
```

Find the exact location by looking for where the success JSON response is built (the `"created": true` or `"updated": true` response).

- [ ] **Step 3: Add eviction call to engine_delete_file (delete handler)**

In `engine_delete_file` (around line 941), after the successful delete response (`"deleted": true`), add:

```rust
evict_caches_for_path(&state, &path);
```

- [ ] **Step 4: Add eviction call to engine_rename (rename handler)**

In `engine_rename` (around line 2240), after the successful rename response, evict both source and destination:

```rust
evict_caches_for_path(&state, &path);         // old path
evict_caches_for_path(&state, destination);    // new path
```

- [ ] **Step 5: Add evict_all on snapshot restore**

In `snapshot_restore` (around line 1417), after the successful restore response (`"restored": true`), add:

```rust
state.engine.permissions_cache.evict_all();
state.engine.index_config_cache.evict_all();
state.group_cache.evict_all();
state.api_key_cache.evict_all();
```

- [ ] **Step 6: Build and test**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Expected: No errors.

Run: `cargo test -p aeordb --lib 2>&1 | tail -5`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add aeordb-lib/src/server/engine_routes.rs
git commit -m "fix: evict caches on write/delete/rename of system files and snapshot restore"
```

---

### Task 7: Delete old cache implementations and clean up mod.rs

**Files:**
- Delete: `aeordb-lib/src/engine/permissions_cache.rs`
- Delete: `aeordb-lib/src/engine/group_cache.rs`
- Delete: `aeordb-lib/src/engine/api_key_cache.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Remove old module declarations and re-exports from mod.rs**

In `aeordb-lib/src/engine/mod.rs`:

Remove these module declarations:
```rust
pub mod api_key_cache;
pub mod group_cache;
pub mod permissions_cache;
```

Remove these re-exports:
```rust
pub use api_key_cache::ApiKeyCache;
pub use group_cache::GroupCache;
pub use permissions_cache::PermissionsCache;
```

- [ ] **Step 2: Delete the old files**

```bash
rm aeordb-lib/src/engine/permissions_cache.rs
rm aeordb-lib/src/engine/group_cache.rs
rm aeordb-lib/src/engine/api_key_cache.rs
```

- [ ] **Step 3: Remove DEFAULT_CACHE_TTL_SECONDS if unused**

In `aeordb-lib/src/server/mod.rs`, check if `DEFAULT_CACHE_TTL_SECONDS` is still referenced. If not, remove the constant (around line 46):

```rust
// Remove: const DEFAULT_CACHE_TTL_SECONDS: u64 = 60;
```

- [ ] **Step 4: Fix any remaining compilation errors**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -20`

Fix any remaining references to old types. Common ones:
- `use crate::engine::GroupCache` → remove
- `use crate::engine::PermissionsCache` → remove
- `use crate::engine::ApiKeyCache` → remove

- [ ] **Step 5: Run full test suite**

Run: `cargo test -p aeordb 2>&1 | tail -10`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: remove old TTL-based cache implementations, replaced by unified Cache<L>"
```

---

### Task 8: Build and smoke-test the server

**Files:** None (verification only)

- [ ] **Step 1: Build release binary**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Compiles with only pre-existing warnings.

- [ ] **Step 2: Start the server and verify basic operations**

```bash
./target/release/aeordb start -D "/path/to/test.aeordb" --port 6830
```

Verify:
- Dashboard loads at `http://localhost:6830`
- File listing works (tests permissions cache)
- File upload works (tests index config cache + eviction)
- File delete works (tests eviction)

- [ ] **Step 3: Final commit**

```bash
git add -A
git commit -m "chore: unified cache interface — verified working"
```
