# ordered_v0

- family: strategy
- permanent_id: 0x0002
- stability: migration-only-v0-adapter
- purpose: typed equality, range, sort, and aggregate order
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Preserve v0 scalar range candidate behavior and current raw-value/order recheck behavior, including scalar collisions.
