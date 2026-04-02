# AeorDB — TODO

## Current: NVT Bitmap Compositing Query Engine

### Batch 1 (Foundation)
- [ ] Task 1: FieldIndex backed by NVT + reader-correction + coordinator swap
- [ ] Task 2: NVTMask bitmap operations (packed u64 bitset)

### Batch 2 (Query Building)
- [ ] Task 3: Direct scalar jumps for simple queries (Tier 1)
- [ ] Task 4: Typed convenience methods (gt_u64, eq_str, etc.)
- [ ] Task 5: QueryNode tree with boolean logic (AND, OR, NOT)
- [ ] Task 9: Strided / progressive execution
- [ ] Task 10: Index serialization with NVT

### Batch 3 (Execution + API)
- [ ] Task 6: Two-tier query execution engine
- [ ] Task 7: Memory-bounded joins (streaming mask construction)
- [ ] Task 8: HTTP query API with boolean logic + sugar

## Test Count: 701 existing + ~30 new target
