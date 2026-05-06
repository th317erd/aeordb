# Unified Cache Interface

**Date:** 2026-05-05
**Status:** Approved

## Problem

The codebase has three independent cache implementations (`PermissionsCache`, `GroupCache`, `ApiKeyCache`) that each duplicate the same pattern: `RwLock<HashMap>` + TTL-based expiry + domain-specific load logic. The caches use 60-second TTL, which means stale reads are possible for up to a minute after a system file changes. Permissions eviction only happens from share link routes — writing a `.aeordb-permissions` file via the general file API does not evict the cache. Index configs have no caching at all, causing disk reads on every ancestor walk during file writes and deletes.

## Design

### Core Interface

A generic `Cache<L>` backed by a `CacheLoader` trait:

```rust
pub trait CacheLoader {
    type Key: Eq + Hash + Clone;
    type Value: Clone;

    fn load(&self, key: &Self::Key, engine: &StorageEngine) -> EngineResult<Self::Value>;
}

pub struct Cache<L: CacheLoader> {
    entries: RwLock<HashMap<L::Key, L::Value>>,
    loader: L,
}
```

**Methods on `Cache`:**

- `new(loader: L) -> Self` — construct with a loader
- `get(key: &L::Key, engine: &StorageEngine) -> EngineResult<L::Value>` — return cached value or call loader on miss, cache the result
- `evict(key: &L::Key)` — remove a single entry
- `evict_all()` — flush the entire cache

The `RwLock` allows concurrent reads on the hot path. Write lock is only taken on cache miss (to insert) or eviction.

No TTL. Entries live until explicitly evicted. If data hasn't changed, the cache is correct indefinitely.

### Domain Loaders

Four loaders, each a unit struct implementing `CacheLoader`:

#### PermissionsLoader

- **Key:** `String` (directory path, e.g. `/Pictures`)
- **Value:** `Option<PathPermissions>`
- **Load:** reads `{path}/.aeordb-permissions` via `DirectoryOps::read_file`, deserializes. Returns `None` if not found.

#### GroupLoader

- **Key:** `Uuid` (user_id)
- **Value:** `Vec<String>` (group names the user belongs to)
- **Load:** calls `system_store::list_groups`, filters to groups containing the user_id.

#### ApiKeyLoader

- **Key:** `String` (key_id)
- **Value:** `Option<ApiKeyRecord>`
- **Load:** calls `system_store::get_api_key`, returns the record or `None`.

#### IndexConfigLoader

- **Key:** `String` (directory path, e.g. `/Pictures`)
- **Value:** `Option<PathIndexConfig>`
- **Load:** reads `{path}/.aeordb-config/indexes.json` via `DirectoryOps::read_file`, deserializes. Returns `None` if not found.

### Eviction Strategy

Eviction is event-driven — triggered at the write path when system files change. No TTL.

**In `engine_routes::engine_write_file`, `engine_routes::engine_delete_file`, and `engine_routes::engine_rename_file`**, after a successful operation, check the path and evict. For renames, check both the source and destination paths:

| Path pattern | Cache to evict | Key |
|---|---|---|
| `*/.aeordb-permissions` | `permissions_cache` | parent directory path |
| `*/.aeordb-config/indexes.json` | `index_config_cache` | directory containing `.aeordb-config/` (e.g. `/Pictures/.aeordb-config/indexes.json` → evict key `/Pictures`) |
| `/.aeordb-system/api-keys/*` | `api_key_cache` | filename (key_id) |
| `/.aeordb-system/groups/*` | `group_cache` | `evict_all()` (group membership is cross-cutting) |
| `/.aeordb-system/users/*` | `group_cache` | `evict_all()` (user changes can affect group resolution) |

**On snapshot restore** (`version_manager::restore_snapshot`): call `evict_all()` on every cache. A restore can change any file in the database.

**Existing eviction in `share_routes.rs`**: keep as-is, already calls `evict_path` which maps to `evict(key)`.

### Integration

#### Cache Placement

The `index_config_cache` and `permissions_cache` are used by engine-layer code (`IndexingPipeline`, `PermissionResolver`) that doesn't have access to `AppState`. These caches live on `StorageEngine` so both the engine layer and server layer can access them:

```rust
// On StorageEngine:
pub permissions_cache: Arc<Cache<PermissionsLoader>>,
pub index_config_cache: Arc<Cache<IndexConfigLoader>>,
```

The `group_cache` and `api_key_cache` are server-layer concerns (auth middleware, share routes). These stay on `AppState`:

```rust
// On AppState:
pub group_cache: Arc<Cache<GroupLoader>>,
pub api_key_cache: Arc<Cache<ApiKeyLoader>>,
```

#### Call site changes (11 total, minimal):

- `permissions_cache.get_permissions(path, engine)` → `permissions_cache.get(&path, engine)`
- `group_cache.get_groups(user_id, engine)` → `group_cache.get(user_id, engine)`
- `api_key_cache.get_key(key_id, engine)` → `api_key_cache.get(&key_id, engine)`
- `permissions_cache.evict_path(path)` → `permissions_cache.evict(&path)`
- `api_key_cache.invalidate(key_id)` → `api_key_cache.evict(&key_id)`

#### IndexingPipeline

`IndexingPipeline::find_config_for_path`: the `load_config(dir)` method changes from `DirectoryOps::read_file` to `self.engine.index_config_cache.get(&dir, engine)`. The ancestor walk logic stays the same. First request walks the tree with disk reads; subsequent requests for files in the same directories hit the cache.

#### PermissionResolver

`PermissionResolver` currently receives `&PermissionsCache` as a constructor parameter. This changes to accessing `engine.permissions_cache` directly, removing the parameter.

### File Structure

- **New:** `engine/cache.rs` — generic `Cache<L>` struct and `CacheLoader` trait
- **New:** `engine/cache_loaders.rs` — all four loader implementations
- **Delete:** `engine/permissions_cache.rs` — replaced by `PermissionsLoader`
- **Delete:** `engine/group_cache.rs` — replaced by `GroupLoader`
- **Delete:** `engine/api_key_cache.rs` — replaced by `ApiKeyLoader`
- **Modified:** `engine/mod.rs` — update exports
- **Modified:** `engine/storage_engine.rs` — add `permissions_cache` and `index_config_cache` fields
- **Modified:** `engine/indexing_pipeline.rs` — use `engine.index_config_cache` instead of direct disk reads
- **Modified:** `engine/permission_resolver.rs` — access `engine.permissions_cache` directly, remove cache constructor parameter
- **Modified:** `server/engine_routes.rs` — add eviction on write/delete/rename of system paths
- **Modified:** `server/mod.rs` — construct `group_cache` and `api_key_cache` on AppState; `permissions_cache` and `index_config_cache` constructed with engine
- **Modified:** `server/share_routes.rs` — update eviction calls
- **Modified:** `auth/permission_middleware.rs` — update API key cache calls

### Error Handling

The loader returns `EngineResult`. On load failure:
- `Cache::get()` propagates the error to the caller (no caching of errors)
- A subsequent `get()` for the same key will retry the load

This means transient disk errors don't poison the cache with stale `None` values.

### Testing

- Unit test `Cache<L>` with a mock loader: verify get-on-miss calls loader, get-on-hit returns cached value, evict causes reload, evict_all flushes everything.
- Integration test: write a `.aeordb-permissions` file via API, verify the permission change takes effect immediately (no stale read).
