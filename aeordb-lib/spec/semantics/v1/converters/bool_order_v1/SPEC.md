# bool_order_v1

- family: converter
- permanent_id: 0x0008
- stability: corrected-v1
- purpose: false-before-true order
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Accept bool only. Posting key 00 is false and 01 is true. Coordinate is zero for false and u64::MAX for true.
