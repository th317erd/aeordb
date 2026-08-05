# i64_order_v1

- family: converter
- permanent_id: 0x0005
- stability: corrected-v1
- purpose: checked signed integer order
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Accept i64 or u64 at most i64::MAX. Posting key is two's-complement i64 little-endian. Typed comparison decodes the key. Coordinate is decoded bits XOR 0x8000000000000000.
