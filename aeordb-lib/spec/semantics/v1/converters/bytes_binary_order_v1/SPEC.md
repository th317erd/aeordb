# bytes_binary_order_v1

- family: converter
- permanent_id: 0x0002
- stability: corrected-v1
- purpose: unsigned byte lexicographic order
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Posting key is the exact bytes value. Compare unsigned bytes lexicographically, prefix-shorter first. Coordinate is the first eight key bytes interpreted big-endian and right-padded with zero.
