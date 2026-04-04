# Fuzzy Search, Trigram Indexing & Phonetic Matching — Unified Research Report

**Date:** 2026-04-03
**Purpose:** Research foundation for adding trigram, fuzzy, and phonetic search to AeorDB
**Detailed research files:** `trigram-indexing-research.md`, `phonetic-indexing-research.md` (same directory)

---

## Executive Summary

Three complementary search strategies, each solving a different class of string matching:

| Strategy | Catches | Example |
|----------|---------|---------|
| **Trigram index** | Typos, substring, character transposition | "Smtih" → "Smith", `LIKE '%atch%'` |
| **Phonetic index** | Sound-alike misspellings, name variants | "Schmidt" → "Smith", "Schwartz" → "Shwartz" |
| **Fuzzy scoring** | Ranked results by similarity | "restaurant" with edit distance 2 → "restaruant", "resteraunt" |

Production systems combine all three in a **two-stage pipeline**: cheap candidate generation (trigram/phonetic indexes) followed by precise ranking (edit distance/Jaro-Winkler scoring).

---

## 1. Trigram Indexing

### How It Works

Decompose strings into overlapping 3-character subsequences. Build an inverted index mapping each trigram to the documents containing it.

```
"hello" → {"hel", "ell", "llo"}
"catch" → {"cat", "atc", "tch"}

With padding (PostgreSQL convention — 2-space prefix, 1-space suffix):
"cat"   → {"  c", " ca", "cat", "at "}
```

**Query process:**
1. Decompose query into trigrams
2. Look up each trigram's posting list in the inverted index
3. **Substring search (AND):** Intersect all posting lists — candidate must contain every trigram
4. **Similarity search (OR + threshold):** Count shared trigrams per candidate, filter by Jaccard/Dice threshold
5. **Recheck:** Verify actual string match against candidates (trigrams are necessary but not sufficient)

### Similarity Scoring

| Metric | Formula | Default? |
|--------|---------|----------|
| Jaccard | \|A∩B\| / \|A∪B\| | — |
| Dice (Sorensen) | 2\|A∩B\| / (\|A\|+\|B\|) | PostgreSQL pg_trgm uses this |
| Overlap | \|A∩B\| / min(\|A\|,\|B\|) | Good for short-vs-long matching |

PostgreSQL default threshold: **0.3** (Dice coefficient).

### False Negative Guarantees

For query length `q` and max edit distance `d`, a valid match must share at least `(q - 2) - 3d` trigrams. This gives a mathematically guaranteed threshold for zero false negatives within edit distance d.

**Example:** Query "restaurant" (10 chars), distance 2: threshold = 8 - 6 = 2 trigrams minimum.

### Performance

- PostgreSQL GIN trigram index: 47x–3600x speedup over sequential scan
- Google Code Search: 36,972 files → 25 candidates for a regex query
- Space overhead: 1x–5x column data (GIN), ~20% of source (Google)

### Edge Cases

- **Strings < 3 chars:** Without padding, invisible to the index. Padding is essential.
- **Unicode:** Must operate on codepoints, not bytes. UTF-8 multi-byte chars produce garbage byte-level trigrams.
- **Case:** Lowercase before extraction (case-insensitive by default, like pg_trgm).
- **Repetition:** `"aaa"` and `"aaaaaaaaa"` produce the same single trigram `{"aaa"}`.

---

## 2. Fuzzy Search Algorithms

### Edit Distance Family

| Algorithm | Operations | When to Use |
|-----------|-----------|-------------|
| **Levenshtein** | Insert, delete, substitute | General purpose, O(m*n) |
| **Damerau-Levenshtein** | + transposition | User-typed input (80%+ of typos are these 4 operations) |
| **Jaro-Winkler** | Ratio-based, prefix bonus | Short strings, name matching, AML/KYC screening |

### Jaro-Winkler

Produces a similarity score in [0, 1] instead of an edit count. The prefix bonus exploits the fact that people rarely misspell the first few characters. Better than edit distance for short strings — edit distance 1 on a 3-char string is 33%, but Jaro-Winkler handles this gracefully.

```
"MARTHA" vs "MARHTA" → Jaro = 0.944, Jaro-Winkler = 0.961 (prefix "MAR" boosts score)
```

### Index Acceleration Strategies

| Technique | Candidates | Speed |
|-----------|-----------|-------|
| **Trigram inverted index** | 50–200 | Standard, well-understood |
| **SymSpell** (delete-only pre-computation) | 1–50 | Sub-millisecond, ~1,870x faster than BK-tree |
| **Levenshtein automata + FST** | Exact set | Gold standard if you have an FST/trie |
| **BK-tree** | 10K–50K (1–5% of dict) | Simple to implement, moderate speed |

### SymSpell: The Speed King

Pre-compute all deletions within max edit distance for every dictionary term. At query time, generate query deletions and intersect. A delete of the query and a delete of a dictionary term converge on the same shortened form.

- Lookup speed: **0.033ms** (distance 2), **0.180ms** (distance 3)
- Memory: ~10-50x dictionary expansion
- Best when: dictionary is stable, sub-millisecond lookups needed

### The Elasticsearch Fuzziness Rule

```rust
fn auto_fuzziness(len: usize) -> usize {
    match len {
        0..=2 => 0,   // exact only
        3..=5 => 1,   // one typo
        _     => 2,   // two typos (max — Lucene caps here)
    }
}
```

This is the industry standard. Higher distances "match a significant amount of the term dictionary."

### Practical Thresholds

| Use Case | Method | Threshold |
|----------|--------|-----------|
| Search box typo correction | Damerau-Levenshtein | AUTO (0/1/2) |
| Name matching | Jaro-Winkler | >= 0.85 |
| Address matching | Normalize first, then token-level distance 1-2 | — |
| Product search | Trigram Jaccard | >= 0.3 |
| Email/username lookup | Distance 1, prefix_length >= 2 | Strict |

---

## 3. Phonetic Indexing

### Algorithm Comparison

| Algorithm | Best For | Code Format | Multi-Code | False Positive Rate |
|-----------|----------|-------------|------------|---------------------|
| **Soundex** | Basic English surnames | Letter + 3 digits (fixed) | No | High (~0.36) |
| **Metaphone** | English names/words | Letters (variable) | No | Medium |
| **Double Metaphone** | English + international | Letters (variable) | Yes (2) | Low-Medium |
| **NYSIIS** | Names + addresses | Letters (6 chars) | No | Medium |
| **Caverphone 2.0** | NZ/AU English | 10 chars (fixed) | No | Low |
| **Cologne Phonetics** | German names | Digits (variable) | No | Low-Medium |
| **Daitch-Mokotoff** | Slavic/Yiddish names | 6 digits (fixed) | Yes (up to 32!) | Low |
| **Beider-Morse** | International, genealogy | Variable | Yes (many) | Very low |

### Recommended Defaults

- **General-purpose default:** Double Metaphone (de facto standard in Elasticsearch/Lucene/Solr)
- **Include for compatibility:** Soundex (simple, everywhere)
- **For non-English names:** Daitch-Mokotoff (Slavic/Yiddish), Cologne Phonetics (German)
- **Defer initially:** Beider-Morse (highest quality but enormous complexity)

### Key Examples

```
"Smith" / "Smythe"    → Soundex: S530 / S530 ✓ (match)
"Katherine" / "Catherine" → Soundex: K365 / C365 ✗ (MISS — first letter preserved literally)

"Schmidt" / "Smith"   → Double Metaphone: (XMT, SMT) / (SM0, XMT) ✓ (share XMT)
"Schwarz" / "Shvartz"  → Double Metaphone: (XRTS, SRTS) / (XFRTS, SFRTS) — both recognized

"John"                → Daitch-Mokotoff: {160000, 460000} — two pronunciations
"Bierschbach"          → Daitch-Mokotoff: 8 different codes — pronunciation ambiguity
```

### Edge Cases

- **Non-Latin scripts:** Most algorithms are Latin-only. Skip phonetic indexing or transliterate first.
- **Name prefixes (Mc, O', von, de):** Configurable prefix stripping needed.
- **UTF-8:** PostgreSQL's `soundex`/`metaphone`/`dmetaphone` are NOT UTF-8 safe. Only `daitch_mokotoff` and `levenshtein` are. AeorDB must handle UTF-8 correctly from the start.
- **Empty/numeric strings:** Return a sentinel value.

---

## 4. How This Maps to AeorDB's NVT Architecture

### Trigram Index → NVT

Each trigram is hashed via `HashConverter` to a scalar in [0.0, 1.0], stored in a single NVT per field. One field produces many index entries (one per trigram).

```
"hello" → trigrams: {"hel", "ell", "llo"}
       → hash each → 3 entries in the NVT
       → all point to the same file hash
```

**Substring query (AND compositing):**
1. Extract trigrams from query
2. For each trigram, get NVTMask (bucket hit)
3. AND all masks → candidate set
4. Recheck candidates against actual string

**Similarity query:**
1. OR all trigram masks → all possible candidates
2. Recheck phase computes Dice/Jaccard score
3. Return candidates above threshold

This maps directly to the existing two-tier execution model. NVTMask AND IS posting list intersection in bitmap space — O(bucket_count/64) regardless of data size.

### Phonetic Index → NVT

A `PhoneticConverter` implements `ScalarConverter`:
1. Decode bytes to UTF-8
2. Compute phonetic code (e.g., Double Metaphone → "XMT")
3. Hash the code to u64
4. Normalize to [0.0, 1.0]

Same bucket = same phonetic code = match. `is_order_preserving()` returns `false`.

**Double Metaphone:** Two `FieldIndex` entries per field (primary + alternate). Query checks both, unions results.

**Daitch-Mokotoff:** Multiple inserts per document (one per code). Existing `FieldIndex::insert` already supports this.

**Bucket sizing:** Match the phonetic code space cardinality:
- Soundex: ~8K buckets
- Double Metaphone: ~16K buckets
- Daitch-Mokotoff: ~64K buckets

### Fuzzy Scoring → Recheck Phase

The NVT handles candidate generation (cheap, bitmap ops). Fuzzy scoring happens in the recheck phase:
- Trigram candidates → rank by Dice/Jaccard coefficient
- Phonetic candidates → already matched (binary match)
- Combined → rank by Damerau-Levenshtein distance or Jaro-Winkler similarity

### Multi-Strategy Query

```json
{
  "path": "/users/",
  "where": {
    "or": [
      { "field": "name", "op": "phonetic", "value": "Schmidt" },
      { "field": "name", "op": "trigram_similar", "value": "Schmidt", "threshold": 0.3 }
    ]
  }
}
```

Results from phonetic and trigram indexes are unioned. Score fusion: candidates appearing in multiple strategies get higher confidence.

---

## 5. Proposed Implementation Roadmap

### Phase 1 — Trigram Foundation
1. Trigram extraction utility (padding, normalization, Unicode-aware)
2. `TrigramConverter` — hash trigrams to scalar [0.0, 1.0]
3. Trigram field index (one NVT, many entries per document)
4. Substring query via AND compositing + recheck
5. Similarity query via OR + Dice/Jaccard scoring in recheck

### Phase 2 — Phonetic Foundation
6. `PhoneticConverter` as new `ScalarConverter` variant
7. Implement Soundex and Double Metaphone (start simple)
8. Wire into index configuration system
9. Double Metaphone two-index pattern (primary + alternate)

### Phase 3 — Fuzzy Scoring
10. Damerau-Levenshtein distance function (for recheck ranking)
11. Jaro-Winkler similarity function (for name fields)
12. `fuzziness: "auto"` in query API (Elasticsearch semantics)

### Phase 4 — Advanced
13. Additional phonetic algorithms (NYSIIS, Cologne, Daitch-Mokotoff)
14. LIKE pattern → trigram boolean query conversion
15. Multi-index score fusion (phonetic + trigram + exact)
16. SymSpell for single-field exact-term fuzzy lookup
17. Regex → trigram boolean query extraction (Russ Cox approach)

### Rust Crates to Evaluate
- `rphonetic` — Soundex, Metaphone, NYSIIS, Caverphone implementations
- `strsim` — Levenshtein, Damerau-Levenshtein, Jaro-Winkler
- `triple_accel` — SIMD-accelerated edit distance
- Soundex/Metaphone are < 100 lines each if hand-implementing

---

## Sources

Full source lists are in the detailed research files. Key references:
- [PostgreSQL pg_trgm](https://www.postgresql.org/docs/current/pgtrgm.html)
- [PostgreSQL fuzzystrmatch](https://www.postgresql.org/docs/current/fuzzystrmatch.html)
- [Russ Cox — Trigram Regex Search](https://swtch.com/~rsc/regexp/regexp4.html)
- [Elasticsearch Fuzzy Search](https://www.elastic.co/blog/found-fuzzy-search)
- [Elasticsearch Phonetic Plugin](https://www.elastic.co/docs/reference/elasticsearch/plugins/analysis-phonetic)
- [SymSpell — Wolf Garbe](https://github.com/wolfgarbe/SymSpell)
- [Lucene FuzzyQuery 100x faster](https://blog.mikemccandless.com/2011/03/lucenes-fuzzyquery-is-100-times-faster.html)
- [Levenshtein Automata — Schulz & Mihov](https://link.springer.com/article/10.1007/s10032-002-0082-8)
