# f64_v0

- family: converter
- permanent_id: 0x8007
- stability: migration-only-v0-adapter
- purpose: captured legacy f64 converter behavior
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve F64Converter v0 configured range and big-endian input: short input and NaN map to 0.0, equal min/max maps to 0.5, arithmetic uses f64, and infinities/out-of-range values clamp to [0,1].
