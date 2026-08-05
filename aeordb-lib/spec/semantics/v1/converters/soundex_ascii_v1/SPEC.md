# soundex_ascii_v1

- family: converter
- permanent_id: 0x000a
- stability: corrected-v1
- purpose: Aeor Soundex v1 code expansion
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Tokenize Unicode alphanumeric runs, retain ASCII letters for Aeor Soundex v1, and emit class-prefixed nonempty four-character codes. Deduplicate first occurrence within one source ordinal.
