# trigram_v0

- family: converter
- permanent_id: 0x800a
- stability: migration-only-v0-adapter
- purpose: captured legacy trigram behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve TrigramConverter v0 lowercase Unicode alphanumeric word splitting, two-leading/one-trailing space padding, first-occurrence deduplication, and BLAKE3 token scalar interpreted from the first eight digest bytes as little-endian u64/f64.
