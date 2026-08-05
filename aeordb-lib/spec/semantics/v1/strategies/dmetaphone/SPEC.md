# dmetaphone

- family: strategy
- permanent_id: 0x0005
- stability: corrected-v1
- purpose: phonetic and match candidate expansion with recheck
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Support phonetic/match. Codes produce candidates only and complete source text is rechecked under the exact requested available strategy.
