# typed_exact_blake3_v1

- family: converter
- permanent_id: 0x0001
- stability: corrected-v1
- purpose: typed structural equality candidate key with authoritative recheck
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Posting key is source type tag followed by BLAKE3("aeordb.typed-exact-posting.v1\0" || complete CanonicalSourceValueV1). The key is candidate routing only; eq/in recheck complete typed values. Coordinate is the first eight bytes of BLAKE3("aeordb.index.exact-coordinate.v1\0" || complete key), interpreted big-endian.
