# dmetaphone_alt_v0

- family: converter
- permanent_id: 0x800d
- stability: migration-only-v0-adapter
- purpose: captured legacy alternate-code fallback behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve whitespace tokenization and alternate v0 behavior: use alternate when present, otherwise fall back to primary; sort/deduplicate codes and use the legacy little-endian BLAKE3 scalar.
