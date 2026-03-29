# AeorDB — TODO

## Current: Sprint 2 — The Real Filesystem Layer

### Task 1: Soft-Delete Cleanup
- [ ] Remove is_deleted from Document and MetadataUpdates
- [ ] Remove soft-delete from redb_backend serialization
- [ ] Make delete actually remove records
- [ ] Remove include_deleted from list_documents
- [ ] Update HTTP handlers
- [ ] Update ALL affected tests
- [ ] Verify all 380 tests still pass (minus removed soft-delete tests)

### Task 2: Chunk Header Revision
- [ ] New ChunkHeader: format_version (u8) + created_at (i64) + updated_at (i64) + reserved (16 bytes) = 33 bytes
- [ ] Update Chunk struct — no next/prev pointers
- [ ] Hash covers data only (already the case)
- [ ] Update chunk tests

### Task 3: COW B-Tree with File Storage
- [ ] filesystem/mod.rs — module declarations
- [ ] filesystem/index_entry.rs — IndexEntry, EntryType, ChunkList (inline/overflow)
- [ ] filesystem/directory.rs — COW B-tree (nodes as chunks, split/merge, COW on write)
- [ ] B-tree handles BOTH directory structure AND file chunk ordering
- [ ] Streaming reads only (no full-file memory loads)
- [ ] Tests: create, insert, get, remove, list, split, merge, COW, large directory

### Task 4: Path Resolver
- [ ] filesystem/path_resolver.rs — resolve paths segment by segment
- [ ] Auto-create intermediate directories (mkdir -p)
- [ ] store_file, read_file (streaming), delete_file, list_directory
- [ ] Tests: resolve, store, read, delete, list, deep paths, dot-paths

### Task 5: HTTP Wiring
- [ ] Replace redb document storage with filesystem in HTTP handlers
- [ ] Path-based routes (not database:table concatenation)
- [ ] System tables stay in redb
- [ ] Update HTTP tests

### Task 6: Versioning
- [ ] Base+diff (I-frame/P-frame) version management
- [ ] B-tree root hash = entire database state
- [ ] Create base, create diff, restore version
- [ ] Tests: snapshot, restore, diff, multi-version history

## Previous Work (Complete)
- [x] Phase 1: Storage + HTTP + Auth (120 tests)
- [x] Phase 2: WASM plugins + SDK + Native (40 tests)
- [x] Phase 3: Magic links + Refresh + Scoping + Rules (56 tests)
- [x] Phase 4.1: Content-addressed chunk store (44 tests)
- [x] Phase 4.2: Scalar ratio indexing (59 tests)
- [x] Phase 4.3: openraft integration (32 tests)
- [x] Phase 4.4: Versioning via hash maps (28 tests)
- [x] Code review + fixes (19 new tests)
