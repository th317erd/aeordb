# u8_v0

- family: converter
- permanent_id: 0x8002
- stability: migration-only-v0-adapter
- purpose: captured legacy u8 converter behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve the configured unsigned v0 range and big-endian input width. Short input maps to 0.0. Equal min/max maps every input to 0.5. Otherwise saturating(value-min)/(max-min) is evaluated in f64; reversed configured bounds remain captured rather than normalized.
