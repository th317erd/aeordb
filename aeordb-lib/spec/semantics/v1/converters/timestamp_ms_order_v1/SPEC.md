# timestamp_ms_order_v1

- family: converter
- permanent_id: 0x0007
- stability: corrected-v1
- purpose: strict RFC 3339 or integer UTC Unix milliseconds
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Accept an in-range integer millisecond value or strict RFC 3339 text with explicit Z or numeric offset. Reject naive dates, numeric strings, parse failure, and overflow. Store checked UTC Unix milliseconds as i64 little-endian and coordinate as sign-bit-flipped decoded bits.
