# unicode_trigram_v1

- family: converter
- permanent_id: 0x0009
- stability: corrected-v1
- purpose: AeorTextFoldV1 word and substring trigram expansion
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Apply frozen Unicode lowercase with no normalization. Emit class 01 word trigrams from alphanumeric words padded with two leading spaces and one trailing space, then class 02 substring trigrams over the complete folded scalar sequence without padding or boundary removal. Deduplicate within one source ordinal in first-occurrence order. Coordinate hashes the complete class-prefixed token.
