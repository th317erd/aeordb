# AeorDB — TODO

## Current: Unified Indexing Implementation

### Task 1: ScalarConverter trait + built-in converters
- [ ] ScalarConverter trait (to_scalar, is_order_preserving, name)
- [ ] HashConverter, U8/U16/U32/U64Converter, I64Converter
- [ ] F64Converter (with min/max clamping, NaN/Inf handling)
- [ ] StringConverter (multi-stage, rough lexicographic)
- [ ] TimestampConverter
- [ ] Range tracking (observed_min/max, self-adapting)
- [ ] Edge cases: div-by-zero, empty input, wrong-size input
- [ ] Tests (~20)

### Task 2: Refactor NVT to use ScalarConverter
- [ ] NVT takes Box<dyn ScalarConverter> instead of hardcoded hash_to_scalar
- [ ] KVS uses NVT with HashConverter (regression — same behavior)
- [ ] Update all NVT tests
- [ ] Tests (~9)

### Task 3: Remove old src/indexing/ module
- [ ] Delete src/indexing/ (replaced by unified design)
- [ ] Remove test entries from Cargo.toml
- [ ] Fix any broken imports

### Task 4: Index file storage
- [ ] Index stored as FileRecord at .indexes/{field}.idx
- [ ] Index file contains: converter state + NVT + sorted entries
- [ ] Serialize/deserialize index files
- [ ] Tests (~14)

### Task 5: Write pipeline integration
- [ ] store_file → parse → index
- [ ] Parser extracts fields, indexer updates index
- [ ] Handle: no parsers, parser but no indexes, multiple parsers
- [ ] Tests (~8)

### Task 6: Query pipeline
- [ ] Query → converter → NVT → candidates → results
- [ ] Exact, range (gt/lt/between), limit, cursor
- [ ] Multi-field intersection
- [ ] Tests (~9)

### Task 7: Wire to HTTP query endpoints
- [ ] POST /query endpoint
- [ ] JSON query body → parse → execute → return results
- [ ] Tests

### Task 8: WASM converter + batch API
- [ ] WasmConverter implementing ScalarConverter
- [ ] Batch API: N values → N scalars in one WASM call
- [ ] Tests

## Test Count Target: 586 existing + ~60 new = ~646+
