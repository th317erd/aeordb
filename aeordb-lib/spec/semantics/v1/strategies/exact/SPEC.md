# exact

- family: strategy
- permanent_id: 0x0001
- stability: corrected-v1
- purpose: eq and in candidate lookup with complete value recheck
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Support eq/in only. Candidate keys never establish equality; recheck the complete typed source value in the pinned ValueStore generation.
