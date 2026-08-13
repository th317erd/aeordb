# f64_v0

- family: converter
- permanent_id: 0x8007
- stability: migration-only-v0-adapter
- purpose: captured legacy f64 converter behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve F64Converter v0 configured range and big-endian input: short input and NaN map to 0.0, equal min/max maps to 0.5, arithmetic uses f64, and infinities/out-of-range values clamp to [0,1]. Legacy scalar conversion to the fixed u64 coordinate is exact: scalar <= 0 maps to 0, scalar >= 1 maps to u64::MAX, and a finite interior scalar maps to floor(scalar * 2^64) by IEEE-754 decomposition and integer arithmetic. A nonfinite interior result is invalid rather than assigned a coordinate.
