# hash_v0

- family: converter
- permanent_id: 0x8001
- stability: migration-only-v0-adapter
- purpose: captured legacy hash converter behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve HashConverter v0: inputs shorter than eight bytes map to scalar 0.0; otherwise the first eight bytes are big-endian u64 divided in f64 by u64::MAX. It is not order preserving. Legacy scalar conversion to the fixed u64 coordinate is exact: scalar <= 0 maps to 0, scalar >= 1 maps to u64::MAX, and a finite interior scalar maps to floor(scalar * 2^64) by IEEE-754 decomposition and integer arithmetic. A nonfinite interior result is invalid rather than assigned a coordinate.
