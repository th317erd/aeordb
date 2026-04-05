# AeorDB — TODO

## Recently Completed
- [x] Users, Groups, Permissions (crudlify) — 1,008 tests
- [x] Selective zstd compression — 35 tests
- [x] Auth Provider URI (--auth flag) — 41 tests
- [x] NVT bitmap compositing query engine — 78 tests
- [x] Custom storage engine — 273 tests
- [x] Unified indexing (ScalarConverter + NVT) — 136 tests

- [x] HTTP Portal Dashboard (stats API + embedded UI) — 17 tests

## Total: 1,310 tests, all passing

## In Progress: Fuzzy Search, Trigram Indexing & Phonetic Matching
- [x] Phase 1: Multi-Index Foundation (strategy(), expand_value(), scored QueryResult, IndexManager changes) — 30 tests
- [x] Phase 2: Trigram Indexing (fuzzy.rs, TrigramConverter, Dice similarity) — 43 tests
- [x] Phase 3: Phonetic Indexing (phonetic.rs, PhoneticConverter, Soundex + Double Metaphone) — 71 tests
- [x] Phase 4: Fuzzy Scoring + Recheck (DL, JW, auto fuzziness, score-based sorting) — 37 tests
- [x] Phase 5: Composite Match + Polish (match op, HTTP updates, E2E) — 22 tests

## In Progress: Document Parsers
- [x] Tasks 1-2: Config rename (field_name→name, converter_type→type) + test migration
- [x] Task 3: Source path resolution module — 39 tests
- [ ] Task 4: Recursive guard for system directories
- [ ] Task 5: Extract IndexingPipeline from store_file_with_indexing
- [ ] Tasks 6-7: .logs/ system + source resolution integration
- [ ] Tasks 8-9: Parser plugin invocation + content-type registry
- [ ] Tasks 10-11: Plugin mapper + WASM log host function
- [ ] Tasks 12-13: Wire PluginManager + E2E test
- [ ] Tasks 14-15: HTTP cleanup + final docs

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
