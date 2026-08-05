# ordered

- family: strategy
- permanent_id: 0x0002
- stability: corrected-v1
- purpose: typed equality, range, sort, and aggregate order
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Support eq/in/gt/lt/inclusive-between/sort/aggregate. Compare complete typed posting keys; coordinates only narrow candidate pages and endpoint scans widen by predecessor/successor cells.
