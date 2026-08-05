# u64_order_v1

- family: converter
- permanent_id: 0x0004
- stability: corrected-v1
- purpose: checked unsigned integer order
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Accept u64 or nonnegative i64 in range. Posting key is the u64 little-endian. Typed comparison decodes the key. Coordinate is the numeric u64 value.
