# trigram

- family: strategy
- permanent_id: 0x0003
- stability: corrected-v1
- purpose: contains, similar, fuzzy, and match candidate expansion with recheck
- authority: complete posting/value recheck; normalized coordinates are hints only
- byte_order: little-endian except explicitly byte-comparable keys

## Normative Behavior

Support contains/similar/fuzzy/match. Trigrams produce candidates only. Recheck folded complete text and score with the frozen Dice, OSA, or Jaro-Winkler rules.
