# soundex_ascii_v1

- family: converter
- permanent_id: 0x000a
- stability: corrected-v1
- purpose: Aeor Soundex v1 code expansion
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Tokenize Unicode alphanumeric runs, retain ASCII letters for Aeor Soundex v1, and emit class 03 followed by each nonempty four-character code. Deduplicate first occurrence within one source ordinal. Coordinate is the first eight bytes of BLAKE3("aeordb.index.token-coordinate.v1\0" || complete class-prefixed token), interpreted big-endian.
