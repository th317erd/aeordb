# AeorDB — TODO

## Sprint 2 — COMPLETE

- [x] Task 1: Soft-delete cleanup
- [x] Task 2: Chunk headers (format version + timestamps)
- [x] Task 3: Custom B-tree (built, backed up — available for indexing)
- [x] Task 3R: redb directory layer (table-per-directory)
- [x] Task 4R: Path resolver (redb-native, streaming, mkdir-p)
- [x] Task 5: HTTP wiring (/fs/* routes with streaming responses)
- [x] Task 6R: Version management (redb persistent savepoints)

## Previous Work
- [x] Sprint 1 / Phases 1-4: Storage, HTTP, Auth, Plugins, Indexing, Replication, Versioning

## Test Count: 486 (all passing, zero clippy warnings)

## Next Steps (Not Started)
- [ ] Wire parsers to filesystem (extract fields on write, feed to indexes)
- [ ] Implement permissions resolution (crudlify, proximity walk)
- [ ] Connect indexing engine to filesystem writes
- [ ] Build the CLI (aeordb-cli: start server, manage keys, info)
- [ ] Deprecate old document CRUD routes (replaced by /fs/*)
- [ ] Performance benchmarking
