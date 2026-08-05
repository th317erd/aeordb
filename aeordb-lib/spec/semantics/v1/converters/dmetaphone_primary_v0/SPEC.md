# dmetaphone_primary_v0

- family: converter
- permanent_id: 0x800c
- stability: migration-only-v0-adapter
- purpose: captured legacy Double Metaphone primary behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve whitespace tokenization, Aeor Double Metaphone primary v0, sorted/deduplicated codes, and BLAKE3 code scalar interpreted little-endian.
