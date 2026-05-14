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
use aeordb::engine::{StorageEngine, DirectoryOps, RequestContext};

// Create or open a database
let engine = StorageEngine::create("my.aeordb").unwrap();
let ctx = RequestContext::system();
let ops = DirectoryOps::new(&engine);
ops.ensure_root_directory(&ctx).unwrap();

// Store a small file (full content in memory — fine for KB-range data)
ops.store_file_buffered(&ctx, "/hello.txt", b"Hello, world!", Some("text/plain")).unwrap();

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
| `store_file_from_reader(ctx, path, reader, content_type)` | Store a file by streaming chunks from any `Read` source. Bounded memory. Use for arbitrary-size content. |
| `read_file_buffered(path)` | Read a file's content into a single `Vec<u8>`. **Buffered — materializes the full file; use only for small payloads.** |
| `read_file_streaming(path)` | Read a file as a streaming iterator of chunks. Bounded memory. Use for arbitrary-size content. |
| `delete_file(ctx, path)` | Delete a file |
| `exists(path)` | Check if a file or directory exists |
| `get_metadata(path)` | Get file metadata without reading content |
| `list_directory(path)` | List immediate children of a directory |
| `create_directory(ctx, path)` | Create an empty directory |

### Directory Listing

```rust,ignore
use aeordb::engine::directory_listing::list_directory_recursive;

// List all files recursively
let entries = list_directory_recursive(&engine, "/assets", -1, None).unwrap();

// List with glob filter
let psds = list_directory_recursive(&engine, "/assets", -1, Some("*.psd")).unwrap();

// List one level deep
let shallow = list_directory_recursive(&engine, "/assets", 1, None).unwrap();
```

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
    false,  // exclude /.system/
).unwrap();

// Get the chunk data for transfer
let chunks = get_needed_chunks(&engine, &diff.chunk_hashes_needed).unwrap();

// On the receiving side: store incoming chunks
let stored = apply_sync_chunks(&engine, &chunks).unwrap();
```

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
use aeordb::engine::{export_version, import_backup};

// Export current HEAD as a .aeordb file
let result = export_version(&engine, &head_hash, "/tmp/backup.aeordb").unwrap();

// Import a backup
let result = import_backup(&engine, &ctx, "/tmp/backup.aeordb").unwrap();
```

## Garbage Collection

```rust,ignore
use aeordb::engine::gc::run_gc;

// Run GC (dry_run = true for preview)
let result = run_gc(&engine, &ctx, false).unwrap();
println!("Reclaimed {} bytes from {} entries", result.reclaimed_bytes, result.garbage_entries);
```

## System Data

System data (users, groups, API keys, config) is stored under `/.system/` and accessed via `system_store`:

```rust,ignore
use aeordb::engine::system_store;

// Store/retrieve config
system_store::store_config(&engine, &ctx, "my_key", b"my_value").unwrap();
let value = system_store::get_config(&engine, "my_key").unwrap();

// User management
let user = aeordb::engine::User::new("alice", "alice@example.com");
system_store::store_user(&engine, &ctx, &user).unwrap();
let users = system_store::list_users(&engine).unwrap();

// API key management
system_store::store_api_key(&engine, &ctx, &key_record).unwrap();
let keys = system_store::list_api_keys(&engine).unwrap();
```

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
