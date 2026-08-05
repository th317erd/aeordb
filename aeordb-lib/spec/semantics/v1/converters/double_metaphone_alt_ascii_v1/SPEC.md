# double_metaphone_alt_ascii_v1

- family: converter
- permanent_id: 0x000c
- stability: corrected-v1
- purpose: distinct Aeor Double Metaphone alternate v1 code
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Tokenize Unicode alphanumeric runs and emit only a nonempty alternate Aeor Double Metaphone v1 code that differs from primary. No primary fallback is emitted.
