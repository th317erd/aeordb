# timestamp_v0

- family: converter
- permanent_id: 0x8009
- stability: migration-only-v0-adapter
- purpose: captured legacy timestamp fallback behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve TimestampConverter v0 configured range and parsing order: exact eight bytes as big-endian i64, then RFC3339, naive seconds, naive fractional seconds, date-only UTC, numeric i64 text, and finally epoch zero. Normalize with the captured i64 range using f64 and clamp.
