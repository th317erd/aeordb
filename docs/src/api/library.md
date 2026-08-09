# Library API

AeorDB can be used as an embedded Rust library without the HTTP server. The `aeordb` crate exposes all database operations as direct function calls.

## Quick Start

Add `aeordb` to your `Cargo.toml`:

```toml
[dependencies]
aeordb = { path = "../aeordb/aeordb-lib" }
```

Basic usage:

```rust,ignore
use aeordb::engine::{StorageEngine, DirectoryOps, RequestContext, BufferedFile, JsonMergeFilePatch, MergeDepth, PermissionStore};

// Create or open a database
let engine = StorageEngine::create("my.aeordb").unwrap();
let ctx = RequestContext::system();
let ops = DirectoryOps::new(&engine);
ops.ensure_root_directory(&ctx).unwrap();

// Store a small file (full content in memory — fine for KB-range data)
ops.store_file_buffered(&ctx, "/hello.txt", b"Hello, world!", Some("text/plain")).unwrap();

// Store several small files in one embedded batch
ops.store_files_buffered_batch(&ctx, vec![
    BufferedFile {
        path: "/sync/a.json".to_string(),
        data: br#"{"dirty":false}"#.to_vec(),
        content_type: Some("application/json".to_string()),
    },
    BufferedFile {
        path: "/sync/b.txt".to_string(),
        data: b"short text".to_vec(),
        content_type: Some("text/plain".to_string()),
    },
]).unwrap();

// Merge JSON documents without an HTTP round trip
ops.merge_json_file(&ctx, "/sync/a.json", serde_json::json!({"seen": true}), MergeDepth::Unbounded).unwrap();
ops.merge_json_files_batch(&ctx, vec![
    JsonMergeFilePatch {
        path: "/sync/a.json".to_string(),
        patch: serde_json::json!({"count": 2}),
        depth: MergeDepth::Unbounded,
    },
]).unwrap();

// Atomically grant one group access to several paths
PermissionStore::new(&engine).grant_paths(
    &ctx,
    vec!["/sync/a.json".to_string(), "/sync/b.txt".to_string()],
    vec!["sync-readers".to_string()],
    ".r..l...".to_string(),
).unwrap();

// Read it back into a single Vec
let data = ops.read_file_buffered("/hello.txt").unwrap();
assert_eq!(data, b"Hello, world!");

// For arbitrary-size content, stream from any `Read` source:
let file = std::fs::File::open("big.mp4").unwrap();
ops.store_file_from_reader(&ctx, "/big.mp4", file, Some("video/mp4")).unwrap();

// And read it back chunk-by-chunk without materializing:
let stream = ops.read_file_streaming("/big.mp4").unwrap();
for chunk in stream {
    let chunk = chunk.unwrap();
    // ... write to network / file / hasher / etc.
}
```

## File Operations

All file operations are on `DirectoryOps`:

```rust,ignore
let ops = DirectoryOps::new(&engine);
```

| Function | Description |
|----------|-------------|
| `store_file_buffered(ctx, path, data, content_type)` | Store a file at the given path. **Buffered — loads `data` fully into memory; use only for small payloads.** |
| `store_files_buffered_batch(ctx, files)` | Store multiple fully-buffered small files in one embedded batch. Validates every path before writing, preserves created timestamps on overwrite, and supports trusted system paths. |
| `store_file_from_reader(ctx, path, reader, content_type)` | Store a file by streaming chunks from any `Read` source. Bounded memory. Use for arbitrary-size content. |
| `read_file_buffered(path)` | Read a file's content into a single `Vec<u8>`. **Buffered — materializes the full file; use only for small payloads.** |
| `read_file_streaming(path)` | Read a file as a streaming iterator of chunks. Bounded memory. Use for arbitrary-size content. |
| `merge_json_file(ctx, path, patch, depth)` | Apply an RFC 7396 JSON merge patch to one JSON file. Missing files start as `{}` and are created as `application/json`. |
| `merge_json_files_batch(ctx, patches)` | Apply multiple JSON merge patches, validate/parse every target first, then write the merged documents in one embedded batch. |
| `copy_file(ctx, from, to)` | Copy one file by reusing its content-addressed chunks. |
| `copy_path(ctx, from, to)` | Atomically copy one file, directory closure, or symlink to an exact destination. |
| `copy_paths(ctx, sources, destination)` | Atomically copy multiple source closures into one destination directory. |
| `rename_file(ctx, from, to)` | Atomically move a file without copying its chunks. |
| `rename_symlink(ctx, from, to)` | Atomically move a symlink without changing its target. |
| `delete_file(ctx, path)` | Delete a file |
| `exists(path)` | Check if a file or directory exists |
| `get_metadata(path)` | Get file metadata without reading content |
| `list_directory(path)` | List immediate children for diagnostics, returning readable B-tree branches when other branches are damaged |
| `list_directory_strict(path)` | List immediate children only when the complete directory and every live child path can be proven valid |
| `create_directory(ctx, path)` | Create an empty directory |

### Buffered Batch Writes

`store_files_buffered_batch` is for trusted embedded callers that already have small file bodies in memory. It is not a replacement for streaming uploads of arbitrary-size data.

```rust,ignore
use aeordb::engine::BufferedFile;

let result = ops.store_files_buffered_batch(&ctx, vec![
    BufferedFile {
        path: "/buckets/users.json".to_string(),
        data: br#"{"updated":true}"#.to_vec(),
        content_type: Some("application/json".to_string()),
    },
    BufferedFile {
        path: "/buckets/index.txt".to_string(),
        data: b"user-bucket\n".to_vec(),
        content_type: Some("text/plain".to_string()),
    },
]).unwrap();

assert_eq!(result.committed, 2);
```

Batch validation rejects empty batches, root writes, and duplicate normalized
paths before writing any namespace entries. Chunk payloads are immutable and
may be staged before publication, but every FileRecord, path locator, parent
directory, and HEAD transition is published under one namespace coordinator
operation and one hard durability acknowledgement. A failure cannot expose only
part of the requested path set. Metadata indexes, counters, and SSE fanout run
only after that acknowledgement. Unlike the HTTP `/blobs/commit` endpoint, this
embedded helper supports internal system paths because any caller with direct
`StorageEngine` access is already trusted code.

### Permission Authority

Use `PermissionStore` for grant and revoke read-modify-write operations. Do not
manually read and replace `.aeordb-permissions` when the update depends on its
current contents.

```rust,ignore
use aeordb::engine::{PermissionRevokeResult, PermissionStore};

let permissions = PermissionStore::new(&engine);
let granted = permissions.grant_paths(
    &ctx,
    vec!["/reports/a.json".to_string(), "/reports/b.json".to_string()],
    vec!["analysts".to_string()],
    ".r..l...".to_string(),
).unwrap();
assert_eq!(granted.paths.len(), 2);
assert_eq!(granted.changed_paths.len(), 2);

let revoked = permissions.revoke_path(
    &ctx,
    "/reports/a.json",
    "analysts",
    Some("a.json"),
).unwrap();
assert_eq!(revoked, PermissionRevokeResult::Revoked);
```

`grant_paths` deduplicates normalized paths and groups, validates every target
and every current permission document, and publishes all changed permission
files in one hard-acknowledged namespace batch. `paths` reports the accepted
request paths; `changed_paths` reports the exact subset whose links changed, so
callers can avoid false notifications on retries. An identical grant is a
zero-write success. A request may expand to at most 65,536 path/group link
updates; path, group, and flag metadata is bounded to 16 MiB; each stored
permission document is bounded to 1 MiB; and aggregate serialized output is
bounded to 64 MiB. Retained request and permission-mutation workspaces are
admitted against the process memory policy and a refusal publishes nothing.

Grant and revoke retain namespace authority from selected-root classification
through publication, preventing concurrent read-modify-write updates from
overwriting each other. File-specific revoke requires `path_pattern` to match
the filename in `path`; it can remove a retained permission link even after the
content file has been deleted. Revoke returns `PermissionFileNotFound` or
`LinkNotFound` as typed no-write outcomes. Malformed, oversized, wrong-policy,
or unreadable authority is an error and is never replaced with an empty file.

### JSON Merge Patch

The RFC 7396 merge primitive is exported from the engine layer:

```rust,ignore
use aeordb::engine::{apply_merge_patch, MergeDepth};

let mut target = serde_json::json!({"a": 1, "nested": {"x": 1}});
apply_merge_patch(&mut target, serde_json::json!({"nested": {"y": 2}}), MergeDepth::Unbounded);
assert_eq!(target, serde_json::json!({"a": 1, "nested": {"x": 1, "y": 2}}));
```

For stored JSON files, use the `DirectoryOps` helpers:

```rust,ignore
use aeordb::engine::{JsonMergeFilePatch, MergeDepth};

let single = ops.merge_json_file(
    &ctx,
    "/state/session.json",
    serde_json::json!({"title": "Scratch"}),
    MergeDepth::Unbounded,
).unwrap();
assert!(single.created);

let batch = ops.merge_json_files_batch(&ctx, vec![
    JsonMergeFilePatch {
        path: "/state/session.json".to_string(),
        patch: serde_json::json!({"count": 7}),
        depth: MergeDepth::Unbounded,
    },
]).unwrap();
assert_eq!(batch.merged, 1);
```

Existing target files must contain valid JSON. If any target in
`merge_json_files_batch` is invalid, the batch fails before writing any merged
output. The authoritative read, RFC 7396 application, and namespace publication
run while holding the same namespace mutation authority. Concurrent committed
patches therefore cannot lose disjoint fields: overlapping fields retain normal
last-applied-writer semantics.

### Copy And Rename

`copy_paths` validates every source and destination, recursively visits large
directories without collecting each directory into a second full listing, and
publishes the complete result atomically. It preserves empty directories and
symlink targets. Planning uses the process memory coordinator; a copy that
cannot fit its bounded planning workspace returns `ResourceExhausted` before
publishing a destination.

File and symlink rename operations publish source retirement, destination
creation, both parent-directory changes, and HEAD under one hard durability
acknowledgement. Their deleted and created SSE relationship events deliberately
remain separate for client compatibility, but share one operation ID and
publication sequence.

## Embedded Sync

The engine exports the same typed sync primitives used by the HTTP peer
orchestrator:

| Function | Description |
|----------|-------------|
| `compute_sync_diff(engine, since_root_hash, paths_filter, include_system)` | Compute a strict full or incremental diff under client-sync or peer-replication SystemFamily policy. Missing/corrupt roots and protected-state violations fail instead of returning a partial diff. |
| `get_needed_chunks(engine, hashes)` | Read the requested chunks for an embedded trusted caller. Missing hashes are omitted; storage errors propagate. |
| `apply_sync_chunks(engine, chunks)` | Validate hash width, bytes, duplicates, and existing chunk authority for the complete input, then store missing immutable chunks in one batch. |
| `apply_merge_operations(engine, ctx, operations)` | Preflight and publish one bounded set of file, symlink, and typed-delete operations under one namespace receipt. |
| `list_conflicts_typed(engine)` | Return typed local conflict evidence; malformed evidence fails the listing rather than disappearing. |
| `SyncEngine::sync_with_local_engine(peer_node_id, remote_engine)` | Perform the complete three-way merge, chunk transfer, conflict receipt, and dual-root checkpoint workflow without HTTP. |

`SyncFileEntry` carries the immutable file identity, optional stored whole-file
`content_hash`, size, content type, original timestamps, and ordered chunk
hashes. A legacy FileRecord without `content_hash` is accepted only after the
validated chunk closure is available, at which point the receiver computes and
stores the current-format whole-file hash.

Sync is deliberately split into two durability domains. `apply_sync_chunks`
may leave valid but unreferenced immutable chunks if a later merge fails; this
does not expose a path and GC may reclaim them. `apply_merge_operations`
publishes all namespace operations atomically, treats only a genuinely missing
delete target as an idempotent no-op, and emits post-acknowledgement counters,
index work, and SSE events. The higher-level `SyncEngine` additionally stores
conflict evidence in that same receipt and advances `PeerSyncState` only after
success. `PeerSyncState` v1 records both the remote acknowledged root and local
post-merge root; its reader also accepts legacy v0 state.

### Directory Listing

```rust,ignore
use aeordb::engine::directory_listing::list_directory_recursive;

// List all files recursively
let entries = list_directory_recursive(&engine, "/assets", -1, None, None).unwrap();

// List with glob filter
let psds = list_directory_recursive(&engine, "/assets", -1, Some("*.psd"), None).unwrap();

// List with a recursive path-shaped glob under the requested directory
let frames = list_directory_recursive(&engine, "/sessions", -1, Some("**/frames/*.json"), None).unwrap();

// List one level deep
let shallow = list_directory_recursive(&engine, "/assets", 1, None, None).unwrap();
```

`list_directory` and `list_directory_recursive` are diagnostic, best-effort
interfaces: a damaged B-tree can return its readable branches while recording
warnings. Code that will make an authorization, backup, repair, deletion, or
other authoritative decision must use `list_directory_strict` or the owning
subsystem's strict traversal service. Strict traversal returns an error for a
malformed directory, an unexplained missing child, or an unknown protected
SystemFamily instead of returning a partial result as complete.

## Symlinks

```rust,ignore
// Create a symlink
ops.store_symlink(&ctx, "/latest", "/v2/logo.psd").unwrap();

// Read symlink metadata
let record = ops.get_symlink("/latest").unwrap();

// Resolve a symlink (follows chains, detects cycles)
use aeordb::engine::symlink_resolver::{resolve_symlink, ResolvedTarget};
match resolve_symlink(&engine, "/latest").unwrap() {
    ResolvedTarget::File(record) => println!("Points to file: {}", record.path),
    ResolvedTarget::Directory(path) => println!("Points to dir: {}", path),
}

// Delete a symlink (not its target)
ops.delete_symlink(&ctx, "/latest").unwrap();
```

## Versioning

### Snapshots

```rust,ignore
use aeordb::engine::VersionManager;
use std::collections::HashMap;

let vm = VersionManager::new(&engine);

// Create a snapshot
let snapshot = vm.create_snapshot(&ctx, "v1.0", HashMap::new()).unwrap();

// List snapshots
let snapshots = vm.list_snapshots().unwrap();

// Restore a snapshot (replaces HEAD)
vm.restore_snapshot(&ctx, "v1.0").unwrap();

// Delete a snapshot
vm.delete_snapshot(&ctx, "v1.0").unwrap();
```

### Forks

```rust,ignore
// Create a fork from current HEAD
vm.create_fork(&ctx, "experiment", None).unwrap();

// Create a fork from a snapshot
vm.create_fork(&ctx, "experiment", Some("v1.0")).unwrap();

// List forks
let forks = vm.list_forks().unwrap();

// Promote a fork to HEAD
vm.promote_fork(&ctx, "experiment").unwrap();

// Abandon a fork
vm.abandon_fork(&ctx, "experiment").unwrap();
```

### File-Level Version Access

```rust,ignore
use aeordb::engine::{file_history, file_restore_from_version};
use aeordb::engine::version_access::{resolve_file_at_version, read_file_at_version};

// Read a file as it was at a specific snapshot
let snapshot = vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();
let data = read_file_at_version(&engine, &snapshot.root_hash, "/doc.txt").unwrap();

// Get file change history across all snapshots
let history = file_history(&engine, "/doc.txt").unwrap();
for entry in &history {
    println!("{}: {} ({})", entry.snapshot, entry.change_type,
        entry.size.unwrap_or(0));
}

// Restore a file from a snapshot (creates auto-safety-snapshot)
let (auto_snap, size) = file_restore_from_version(
    &engine, &ctx, "/doc.txt", Some("v1"), None,
).unwrap();
```

If lifecycle configuration has `snapshot_writes_enabled` set to `false`, `file_restore_from_version` skips the auto-safety snapshot and returns an empty `auto_snap` string while still restoring the file.

## Authentication Authority

Embedded servers that provide a custom `AuthProvider` must implement the
complete authority contract. In particular, `authority_engine` identifies the
engine that owns API-key records, `store_api_key_with_root_authority` is the
explicit authenticated-root creation path, and API-key label and revocation
changes run through the provider's typed transition methods. There is no
default fallback from authenticated root creation to the initial bootstrap
escape hatch. Router construction rejects an enabled provider that does not
expose an authority engine; only disabled authentication uses the data engine as
its inert cache context. With `FileAuthProvider`, `--auth=self` returns the data
engine and `--auth=file://...` returns the separate identity engine.

One-time credential changes are also available as typed embedded operations:

| Function | Outcomes |
|----------|----------|
| `system_store::consume_magic_link(engine, ctx, code_hash, now)` | `Consumed(record)`, `AlreadyUsed`, `Expired`, or `NotFound` |
| `system_store::claim_refresh_token_rotation(engine, ctx, token_hash, now)` | `Claimed(record)`, `AlreadyRevoked`, `Expired`, or `NotFound` |

Each operation reads and conditionally replaces its bounded versioned record
under one namespace authority window. Exactly one concurrent caller can receive
the record and mint replacement credentials. Missing, expired, and already-used
or revoked outcomes perform no write. Malformed, wrong-type, or oversized stored
authority is an error rather than an ordinary authentication miss.

## Sync / Replication

The library exposes the same sync primitives as the HTTP endpoints, enabling embedded clients to replicate without HTTP overhead.

```rust,ignore
use aeordb::engine::{
    compute_sync_diff, get_needed_chunks, apply_sync_chunks,
    SyncDiff, ChunkData,
};

// Compute what changed since a known state
let diff = compute_sync_diff(
    &engine,
    Some(&last_known_hash),  // None for full sync
    Some(&["/assets/**".to_string()]),  // path filter
    false,  // client-sync policy (true selects peer-replication policy)
).unwrap();

// Get the chunk data for transfer
let chunks = get_needed_chunks(&engine, &diff.chunk_hashes_needed).unwrap();

// On the receiving side: store incoming chunks
let stored = apply_sync_chunks(&engine, &chunks).unwrap();
```

The final `compute_sync_diff` argument is retained as a compatibility adapter,
not a raw system-path switch. `true` selects the registry's peer-replication
policy; `false` selects client-sync policy. Both modes reject unknown protected
families. Peer mode includes portable system state but omits node-local secrets,
credentials, controls, conflicts, logs, and derived indexes. Client mode includes
ordinary data and required namespace metadata, then HTTP callers receive an
additional authorization filter.

### Conflict Management

```rust,ignore
use aeordb::engine::{
    list_conflicts_typed, ConflictRecord,
};
use aeordb::engine::conflict_store::{resolve_conflict, dismiss_conflict};

// List unresolved conflicts
let conflicts = list_conflicts_typed(&engine).unwrap();

// Resolve by picking winner or loser
resolve_conflict(&engine, &ctx, "/contested/file.psd", "winner").unwrap();

// Or accept the auto-winner
dismiss_conflict(&engine, &ctx, "/other/file.txt").unwrap();
```

## Querying

```rust,ignore
use aeordb::engine::{QueryEngine, QueryBuilder};

let qe = QueryEngine::new(&engine);

// Build and execute a query
let query = QueryBuilder::new("/users")
    .field("name").contains("Alice")
    .build();

let results = qe.execute(&query).unwrap();
```

## Backup & Export

```rust,ignore
use aeordb::engine::{create_patch, export_version, import_backup};

// Export current HEAD as a .aeordb file
let result = export_version(&engine, &head_hash, "/tmp/backup.aeordb", false).unwrap();

// Import a backup
let result = import_backup(&ctx, &engine, "/tmp/backup.aeordb", false, false, false).unwrap();

// Patches require real retained roots; missing hashes are not empty trees.
let patch = create_patch(&engine, &base_hash, &head_hash, "/tmp/change.aeordb").unwrap();
```

`result.version_hash` is the root stored in the export. It normally equals
`head_hash`; user-only export can replace a legacy root that still names a
protected system tree with a normalized root. Full import validates and
rebuilds the selected SystemFamily closure before the first target mutation;
the final `result.version_hash` is the imported root after that policy pass.
The final boolean selects privileged import authorization, but it never permits
credentials, secrets, node-local controls, logs, GC state, or derived indexes.
Full-import counts describe logical objects in each imported root: chunks count
only newly stored payloads, while files, directories, and symlinks count the
selected objects processed for HEAD and each imported snapshot. Runtime write
metrics use the same logical operations and count chunk payload bytes once.
Patch production and import use the same registry authority. Production omits
non-transferable families before writing leaves and the complete selected base
and target directory closures. Import validates a patch-first,
target-fallback overlay and returns the rebuilt selected root. Unchanged target
entries are neither copied nor counted as sparse mutations, and deletion replay
evidence remains durable across restart.

## Garbage Collection

```rust,ignore
use aeordb::engine::gc::run_gc;

// Run GC (dry_run = true for preview)
let result = run_gc(&engine, &ctx, false).unwrap();
println!("Reclaimed {} bytes from {} entries", result.reclaimed_bytes, result.garbage_entries);
```

## System Data

System data (users, groups, API keys, config) is stored under `/.aeordb-system/` and accessed via `system_store`:

```rust,ignore
use aeordb::engine::system_store;

// Store/retrieve config
system_store::store_config(&engine, &ctx, "my_key", b"my_value").unwrap();
let value = system_store::get_config(&engine, "my_key").unwrap();

// User management
let user = aeordb::engine::User::new("alice", Some("alice@example.com"));
system_store::store_user(&engine, &ctx, &user).unwrap();
let users = system_store::list_users(&engine).unwrap();

// API key management
system_store::store_api_key(&engine, &ctx, &key_record).unwrap();
let keys = system_store::list_api_keys(&engine).unwrap();
```

Credential state transitions use bounded typed operations rather than a split
read/replace sequence. `revoke_api_key_with_policy` accepts `Any`, `OwnedBy`,
or `ShareLink`; `update_api_key_label`, `mark_magic_link_used`, and
`revoke_refresh_token` read, validate, and optionally replace one versioned
record under the namespace authority lock. Missing, policy-mismatched, and
already-applied outcomes perform no write. Malformed, unsupported, or records
above the bounded transition limit fail before publication and preserve the
stored bytes. Credential creation enforces the same 1 MiB serialized-record
ceiling, preventing a newly stored key, magic link, or refresh token from
becoming too large for its later typed transition.

`store_user` publishes the user and its deterministic `user:{uuid}` automatic
group under one hard namespace acknowledgement. `delete_user` retires the same
pair under one acknowledgement; if the user is absent it returns `false` and
does not delete an orphan companion, while an already-absent automatic group is
accepted for compatibility. Each successful compound operation emits one
`system_write` event and counts as one logical write.

## Cron Scheduling

Embedded callers use typed cron operations rather than editing the protected
configuration file or running callbacks while namespace authority is held:

```rust,ignore
use aeordb::engine::{
    create_cron_schedule, delete_cron_schedule, update_cron_schedule,
    CronSchedule, CronScheduleUpdate,
};

create_cron_schedule(&engine, CronSchedule {
    id: "nightly-gc".to_string(),
    task_type: "gc".to_string(),
    schedule: "0 3 * * *".to_string(),
    args: serde_json::json!({"dry_run": false}),
    enabled: true,
}).unwrap();

update_cron_schedule(&engine, "nightly-gc", CronScheduleUpdate {
    enabled: Some(false),
    ..CronScheduleUpdate::default()
}).unwrap();

delete_cron_schedule(&engine, "nightly-gc").unwrap();
```

Each typed mutation is an atomic read-validate-write operation under one
namespace acknowledgement. Concurrent callers cannot lose accepted schedules.
Malformed, duplicate-ID, invalid, unreadable, or oversized stored authority is
returned as an error and remains unchanged. `save_cron_config` remains available
for callers deliberately replacing the complete schedule document.

## Virtual Clock

For replication, the virtual clock provides synchronized timestamps:

```rust,ignore
use aeordb::engine::{SystemClock, VirtualClock, PeerClockTracker};

let clock = SystemClock::new(node_id);
let now = clock.now_ms();

// For testing, use MockClock
use aeordb::engine::MockClock;
let mock = MockClock::new(1, 1000);
mock.advance(500);
assert_eq!(mock.now_ms(), 1500);
```

## Event Bus

Subscribe to database events programmatically:

```rust,ignore
use aeordb::engine::EventBus;

let bus = EventBus::new();
let mut receiver = bus.subscribe();

// Events are emitted automatically on file operations
// Listen in a separate task:
tokio::spawn(async move {
    while let Ok(event) = receiver.recv().await {
        println!("Event: {} on {}", event.event_type, event.source);
    }
});
```

## Key Types

| Type | Module | Description |
|------|--------|-------------|
| `StorageEngine` | `engine` | The database engine |
| `DirectoryOps` | `engine` | File/directory operations |
| `VersionManager` | `engine` | Snapshot/fork management |
| `RequestContext` | `engine` | Context for write operations |
| `FileRecord` | `engine` | File metadata |
| `SymlinkRecord` | `engine` | Symlink metadata |
| `ChildEntry` | `engine` | Directory listing entry |
| `ListingEntry` | `engine` | Recursive listing entry |
| `SyncDiff` | `engine` | Sync diff result |
| `ChunkData` | `engine` | Chunk hash + data pair |
| `ConflictRecord` | `engine` | Typed conflict entry |
| `QueryEngine` | `engine` | Query execution |
| `EventBus` | `engine` | Event pub/sub |
| `PeerManager` | `engine` | Cluster peer management |
| `VirtualClock` | `engine` | Clock trait for timestamps |
