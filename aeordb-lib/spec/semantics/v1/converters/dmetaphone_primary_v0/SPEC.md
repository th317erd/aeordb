# dmetaphone_primary_v0

- family: converter
- permanent_id: 0x800c
- stability: migration-only-v0-adapter
- purpose: captured legacy Double Metaphone primary behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve whitespace tokenization, Aeor Double Metaphone primary v0, sorted/deduplicated codes, and BLAKE3 code scalar interpreted little-endian. Legacy scalar conversion to the fixed u64 coordinate is exact: scalar <= 0 maps to 0, scalar >= 1 maps to u64::MAX, and a finite interior scalar maps to floor(scalar * 2^64) by IEEE-754 decomposition and integer arithmetic. A nonfinite interior result is invalid rather than assigned a coordinate.
