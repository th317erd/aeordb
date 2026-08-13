# dmetaphone_alt_v0

- family: converter
- permanent_id: 0x800d
- stability: migration-only-v0-adapter
- purpose: captured legacy alternate-code fallback behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve whitespace tokenization and alternate v0 behavior: use alternate when present, otherwise fall back to primary; sort/deduplicate codes and use the legacy little-endian BLAKE3 scalar. Legacy scalar conversion to the fixed u64 coordinate is exact: scalar <= 0 maps to 0, scalar >= 1 maps to u64::MAX, and a finite interior scalar maps to floor(scalar * 2^64) by IEEE-754 decomposition and integer arithmetic. A nonfinite interior result is invalid rather than assigned a coordinate.
