# double_metaphone_primary_ascii_v1

- family: converter
- permanent_id: 0x000b
- stability: corrected-v1
- purpose: Aeor Double Metaphone primary v1 code
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Tokenize Unicode alphanumeric runs, retain ASCII letters, and emit class 04 followed by each nonempty Aeor Double Metaphone v1 primary code. Deduplicate first occurrence within one source ordinal. Coordinate is the first eight bytes of BLAKE3("aeordb.index.token-coordinate.v1\0" || complete class-prefixed token), interpreted big-endian.
