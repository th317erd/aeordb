# double_metaphone_alt_ascii_v1

- family: converter
- permanent_id: 0x000c
- stability: corrected-v1
- purpose: distinct Aeor Double Metaphone alternate v1 code
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Tokenize Unicode alphanumeric runs and emit class 05 followed only by each nonempty alternate Aeor Double Metaphone v1 code that differs from primary. No primary fallback is emitted. Deduplicate first occurrence within one source ordinal. Coordinate is the first eight bytes of BLAKE3("aeordb.index.token-coordinate.v1\0" || complete class-prefixed token), interpreted big-endian.
