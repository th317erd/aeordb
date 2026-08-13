# unicode_trigram_v1

- family: converter
- permanent_id: 0x0009
- stability: corrected-v1
- purpose: AeorTextFoldV1 word and substring trigram expansion
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Apply the frozen Unicode 17.0.0 lowercase and alphanumeric table with BLAKE3 9f1bdd82a6142ddc3824e125c28ab941de2ac9b98fd7eaffaa5b85a3f6f884d2 and no normalization. Emit class 01 word trigrams from alphanumeric words padded with two leading spaces and one trailing space, then class 02 substring trigrams over the complete folded scalar sequence without padding or boundary removal. Deduplicate within one source ordinal in first-occurrence order. Coordinate is the first eight bytes of BLAKE3("aeordb.index.token-coordinate.v1\0" || complete class-prefixed token), interpreted big-endian.
