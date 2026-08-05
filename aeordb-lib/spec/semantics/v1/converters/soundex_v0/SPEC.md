# soundex_v0

- family: converter
- permanent_id: 0x800b
- stability: migration-only-v0-adapter
- purpose: captured legacy Soundex behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve whitespace tokenization, Aeor Soundex v0, sorted/deduplicated codes, and BLAKE3 code scalar interpreted little-endian.
