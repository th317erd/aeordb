# string_v0

- family: converter
- permanent_id: 0x8008
- stability: migration-only-v0-adapter
- purpose: captured legacy first-byte-plus-length string behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve StringConverter v0: empty maps to 0.0; scalar is clamp((first byte / 255)*0.7 + min(byte length/max_length,1)*0.3). max_length is the captured nonzero u32 parameter. This is not exact lexical order. Legacy scalar conversion to the fixed u64 coordinate is exact: scalar <= 0 maps to 0, scalar >= 1 maps to u64::MAX, and a finite interior scalar maps to floor(scalar * 2^64) by IEEE-754 decomposition and integer arithmetic. A nonfinite interior result is invalid rather than assigned a coordinate.
