# AeorDB — TODO

## Recently Completed
- [x] Users, Groups, Permissions (crudlify) — 1,008 tests
- [x] Selective zstd compression — 35 tests
- [x] Auth Provider URI (--auth flag) — 41 tests
- [x] NVT bitmap compositing query engine — 78 tests
- [x] Custom storage engine — 273 tests
- [x] Unified indexing (ScalarConverter + NVT) — 136 tests

- [x] HTTP Portal Dashboard (stats API + embedded UI) — 17 tests

## In Progress: Version Export, Patch & Import
- [x] Phase 1: File header extension — 21 tests
- [x] Phase 2: Tree walker utility — 21 tests
- [x] Phase 3: Export operation + CLI — 17 tests
- [x] Phase 4: Diff/Patch operation + CLI — 20 tests
- [x] Phase 5: Import operation + CLI + promote — 18 tests
- [x] Phase 6: HTTP API endpoints — 13 tests
- [x] Fix: dual-key directory storage for immutable snapshots — 25 tests
- [ ] Phase 7: E2E verification (manual testing)

## In Progress: Event System
- [x] Phase 1: EventBus + EngineEvent + RequestContext types — 32 tests
- [x] Phase 2: Thread ctx through all engine methods + callers + tests (45 files)
- [ ] Phase 3: Emit events from engine methods
- [ ] Phase 4: Heartbeat task
- [ ] Phase 5: SSE endpoint
- [ ] Phase 6: Webhooks

## Total: 1,606 tests, all passing

## In Progress: Fuzzy Search, Trigram Indexing & Phonetic Matching
- [x] Phase 1: Multi-Index Foundation (strategy(), expand_value(), scored QueryResult, IndexManager changes) — 30 tests
- [x] Phase 2: Trigram Indexing (fuzzy.rs, TrigramConverter, Dice similarity) — 43 tests
- [x] Phase 3: Phonetic Indexing (phonetic.rs, PhoneticConverter, Soundex + Double Metaphone) — 71 tests
- [x] Phase 4: Fuzzy Scoring + Recheck (DL, JW, auto fuzziness, score-based sorting) — 37 tests
- [x] Phase 5: Composite Match + Polish (match op, HTTP updates, E2E) — 22 tests

## In Progress: Document Parsers
- [x] Tasks 1-2: Config rename (field_name→name, converter_type→type) + test migration
- [x] Task 3: Source path resolution module — 39 tests
- [x] Task 4: Recursive guard for system directories
- [x] Task 5: IndexingPipeline extraction — 18 tests
- [x] Tasks 6-7: .logs/ system + source resolution integration (folded into Task 5)
- [x] Tasks 8-9: Parser plugin invocation + content-type registry — 31 tests
- [x] Tasks 10-11: Plugin mapper + WASM log host function
- [x] Task 12: Wire PluginManager (done in Tasks 8-11)
- [ ] Task 13: E2E test with real WASM parser (deferred — needs compiled WASM binary)
- [x] Tasks 14-15: HTTP verified + docs updated

## Remaining Future Plans
- [ ] Server-side compilation + in-database SDK + schema-as-code
- [ ] Chunk ownership & garbage collection
- [ ] Cron/background task system
- [ ] Pre-hashed client uploads
- [ ] Fork merging (true merge, not just fast-forward)
- [ ] Concurrent parallel writers (coordinator pattern)
- [ ] Large directory optimization
- [ ] File defragmentation
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
- [ ] Query engine enhancements (aggregations, sorting, pagination, etc.)
