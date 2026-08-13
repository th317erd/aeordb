# i64_v0

- family: converter
- permanent_id: 0x8006
- stability: migration-only-v0-adapter
- purpose: captured legacy i64 converter behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve I64Converter v0 configured range, big-endian input, short-input 0.0, equal-range 0.5, i128 shift followed by f64 division, and final [0,1] clamp, including reversed configured bounds. Legacy scalar conversion to the fixed u64 coordinate is exact: scalar <= 0 maps to 0, scalar >= 1 maps to u64::MAX, and a finite interior scalar maps to floor(scalar * 2^64) by IEEE-754 decomposition and integer arithmetic. A nonfinite interior result is invalid rather than assigned a coordinate.
