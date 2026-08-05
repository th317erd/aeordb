# utf8_binary_order_v1

- family: converter
- permanent_id: 0x0003
- stability: corrected-v1
- purpose: raw UTF-8 byte order without locale collation
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Accept valid UTF-8 only. Posting key is its exact encoded bytes with no normalization or locale collation. Compare unsigned bytes lexicographically, prefix-shorter first. Coordinate uses the first eight key bytes big-endian, right-padded with zero.
