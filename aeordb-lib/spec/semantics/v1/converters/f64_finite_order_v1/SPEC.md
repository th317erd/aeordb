# f64_finite_order_v1

- family: converter
- permanent_id: 0x0006
- stability: corrected-v1
- purpose: finite IEEE-754 order with canonical signed zero
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Accept finite f64 and exactly round-tripping integers. Canonicalize -0.0 to +0.0 and store IEEE-754 bits little-endian. Reject NaN and infinities. Coordinate is the standard sortable transform: negative bits invert, nonnegative bits XOR the sign bit.
