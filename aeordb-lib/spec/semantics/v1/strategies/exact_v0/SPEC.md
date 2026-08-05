# exact_v0

- family: strategy
- permanent_id: 0x0001
- stability: migration-only-v0-adapter
- purpose: eq and in candidate lookup with complete value recheck
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve v0 hash candidate selection and exact raw-value recheck behavior.
