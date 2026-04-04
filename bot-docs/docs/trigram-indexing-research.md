# Trigram Indexing: Comprehensive Research Report

**Purpose:** Research foundation for adding trigram-based fuzzy/substring search to AeorDB
**Date:** 2026-04-03

---

## 1. What Trigram Indexes Are

A **trigram** (or 3-gram) is a contiguous subsequence of three characters extracted from a string. A trigram index is an **inverted index** that maps each unique trigram to the set of documents (or rows) containing that trigram.

### Trigram Decomposition

Given the string `"hello"`, the trigrams are:

```
h-e-l  →  "hel"
e-l-l  →  "ell"
l-l-o  →  "llo"
```

The string `"hello"` produces 3 trigrams (length - 2 = 3).

**General formula:** A string of length N produces `max(0, N - 2)` trigrams.

### With Padding (PostgreSQL Convention)

PostgreSQL's pg_trgm prepends **two spaces** and appends **one space** to each word before extracting trigrams. This captures word boundaries:

```
"cat" → "  c", " ca", "cat", "at "
         ^^pad  ^pad   raw   pad^

"hello" → "  h", " he", "hel", "ell", "llo", "lo "
```

With padding, a word of length N produces `N + 2` trigrams (from the padded length of N+3, minus 2).

### Non-Alphanumeric Handling

PostgreSQL strips non-word characters and treats them as word boundaries:

```
"foo|bar" → words: "foo", "bar"
         → trigrams: {"  f", " fo", "foo", "oo ", "  b", " ba", "bar", "ar "}
```

### The Inverted Index

The trigram index is a mapping:

```
trigram → [doc_id_1, doc_id_7, doc_id_42, ...]
```

For a corpus:
```
doc1: "cat"     → trigrams: {"  c", " ca", "cat", "at "}
doc2: "catch"   → trigrams: {"  c", " ca", "cat", "atc", "tch", "ch "}
doc3: "dog"     → trigrams: {"  d", " do", "dog", "og "}
```

The inverted index becomes:
```
"  c" → [doc1, doc2]
" ca" → [doc1, doc2]
"cat" → [doc1, doc2]
"at " → [doc1]
"atc" → [doc2]
"tch" → [doc2]
"ch " → [doc2]
"  d" → [doc3]
" do" → [doc3]
"dog" → [doc3]
"og " → [doc3]
```

---

## 2. How Trigram Indexes Are Built

### Data Structures

**Option A: HashMap/BTreeMap (in-memory)**
```rust
// Simple in-memory approach
trigram_index: HashMap<[u8; 3], Vec<DocumentId>>
```
Each trigram key maps to a sorted list of document IDs (posting list).

**Option B: GIN (Generalized Inverted Index) — PostgreSQL**
PostgreSQL's GIN stores the inverted index as a B-tree of trigram keys, where each key points to a posting list (compressed list of row TIDs). GIN is exact — every trigram is stored, and lookups return precise candidate sets.

**Option C: GiST with Bitmap Signatures — PostgreSQL**
PostgreSQL's GiST approach compresses the full trigram set of each row into a fixed-size **bitmap signature**:
- Default: 12 bytes = 96 bits
- Configurable via `siglen` parameter (1–2024 bytes)
- Each trigram is hashed to a bit position; that bit is set to 1
- Multiple trigrams may collide on the same bit (lossy compression)
- This is essentially a **Bloom filter** per row

**Option D: Bloom Filter per Document**
Each document's trigram set is represented as a Bloom filter of fixed size. Queries hash query trigrams into the same filter space and check bits. False positives require post-filtering.

### Storage Strategies

| Strategy | Space | Precision | Query Speed |
|----------|-------|-----------|-------------|
| Full inverted index (GIN) | Large (3x-5x overhead typical) | Exact candidates | Fast lookup, posting list intersection |
| Bitmap signatures (GiST) | Compact (12-256 bytes/row) | Lossy, needs recheck | Very fast bitwise ops, but more heap fetches |
| Bloom filters per doc | Very compact | Lossy, tunable FPR | Scan all filters, bitwise check |

### Build Process

1. **Tokenize:** For each document/row, extract the text value
2. **Pad:** Apply padding convention (e.g., 2-space prefix, 1-space suffix per word)
3. **Normalize:** Lowercase (if case-insensitive), strip non-alphanumerics
4. **Extract trigrams:** Slide a 3-character window across each word
5. **Deduplicate:** Each document contributes each unique trigram at most once
6. **Insert into index:** Add document ID to each trigram's posting list

### Index Size

Google Code Search reported index sizes approximately **20% of source material** (77 MB index for 420 MB of Linux kernel sources). This is for a pure trigram inverted index without padding. PostgreSQL GIN indexes are typically **1x-5x the column data size** depending on text length distribution.

---

## 3. How Trigram Queries Work

### Equality/Similarity Search

To find rows similar to the query string `"catch"`:

1. **Decompose query into trigrams:**
   ```
   "catch" → {"  c", " ca", "cat", "atc", "tch", "ch "}
   ```

2. **Look up each trigram in the index** to get posting lists:
   ```
   "  c" → [doc1, doc2, doc5, doc9]
   "cat" → [doc1, doc2, doc5]
   "atc" → [doc2, doc8]
   "tch" → [doc2, doc8]
   ...
   ```

3. **For exact/substring match:** Intersect ALL posting lists (AND). Only documents containing every query trigram are candidates.

4. **For similarity/fuzzy match:** Union posting lists, then score each candidate by how many of the query's trigrams it shares.

5. **Recheck:** Verify actual string match/similarity against candidates (trigram match is necessary but not sufficient).

### Substring/LIKE Search

For `WHERE name LIKE '%atch%'`:

1. Extract trigrams from the pattern: `"atc"`, `"tch"` (no padding applied to wildcarded patterns)
2. Intersect posting lists: candidates must contain both `"atc"` AND `"tch"`
3. Recheck: run the actual LIKE predicate on candidates only

### Boolean Query Algebra (Google Code Search)

Russ Cox's seminal work on Google Code Search formalized trigram query extraction from regular expressions:

- **Concatenation:** `match(e1 e2) = match(e1) AND match(e2)`
- **Alternation:** `match(e1 | e2) = match(e1) OR match(e2)`
- **Repetition:** `match(e*) = ANY` (matches everything — can't extract useful trigrams)

Example:
```
regex: /hel(lo|p)/
→ trigrams("hel") AND (trigrams("llo") OR trigrams("elp"))
→ ("hel") AND (("llo") OR ("elp"))
```

The query is run against the trigram index to get candidate documents, then the full regex is run against candidates only. This two-phase approach (filter then verify) is fundamental to all trigram indexing.

### Posting List Intersection

For AND queries, intersect sorted posting lists:
```
list_a: [1, 3, 5, 7, 9, 12]
list_b: [2, 3, 7, 8, 12, 15]
result: [3, 7, 12]
```

**Algorithm:** Two-pointer merge in O(|A| + |B|). For k lists, pick the shortest list first and intersect pairwise, smallest to largest.

**Optimization:** Skip pointers / galloping search — if one list is much larger than the other, use binary search to skip ahead rather than linear scan. Reduces from O(|A| + |B|) to O(|A| * log(|B|)) when |A| << |B|.

---

## 4. Similarity Scoring

Given two strings, decompose each into trigram sets and compute set similarity.

### Jaccard Similarity

```
J(A, B) = |A ∩ B| / |A ∪ B|
```

Example:
```
A = trigrams("cat")  = {"  c", " ca", "cat", "at "}       — 4 trigrams
B = trigrams("cart") = {"  c", " ca", "car", "art", "rt "} — 5 trigrams

A ∩ B = {"  c", " ca"}                                     — 2 shared
A ∪ B = {"  c", " ca", "cat", "at ", "car", "art", "rt "}  — 7 total

J(A, B) = 2/7 ≈ 0.286
```

**Properties:**
- Range: [0.0, 1.0]
- Symmetric: J(A,B) = J(B,A)
- 1.0 = identical trigram sets
- 0.0 = no shared trigrams
- Penalizes size differences (large union dilutes score)

### Dice Coefficient (Sorensen-Dice)

```
D(A, B) = 2 * |A ∩ B| / (|A| + |B|)
```

Example (same strings):
```
D(A, B) = 2 * 2 / (4 + 5) = 4/9 ≈ 0.444
```

**Properties:**
- Range: [0.0, 1.0]
- Symmetric
- Always >= Jaccard for the same sets
- Equivalent to F1-score
- Less sensitive to size differences than Jaccard
- **This is what PostgreSQL's `similarity()` function uses**

### Overlap Coefficient

```
O(A, B) = |A ∩ B| / min(|A|, |B|)
```

Example (same strings):
```
O(A, B) = 2 / min(4, 5) = 2/4 = 0.5
```

**Properties:**
- Range: [0.0, 1.0]
- 1.0 when the smaller set is a complete subset of the larger
- Good for matching short strings against long strings
- Not penalized by size asymmetry

### PostgreSQL Word Similarity

PostgreSQL adds `word_similarity()` which finds the best matching *contiguous extent* of trigrams in the second string that matches the first string's trigrams:

```sql
SELECT word_similarity('word', 'two words');
-- Returns 0.8
-- Finds the best window in 'two words' that overlaps with trigrams of 'word'
```

This handles the "needle in a haystack" problem — searching for a short word within a long text.

### Comparison Summary

| Metric | Formula | Best For |
|--------|---------|----------|
| Jaccard | \|A∩B\| / \|A∪B\| | General similarity, balanced lengths |
| Dice | 2\|A∩B\| / (\|A\|+\|B\|) | Default choice, less size-sensitive |
| Overlap | \|A∩B\| / min(\|A\|,\|B\|) | Short-vs-long matching |
| Word Similarity | Best window overlap | Substring-within-text |

---

## 5. Performance Characteristics

### Space Overhead

| Component | Formula/Estimate |
|-----------|-----------------|
| Trigrams per string | N + 2 (with padding) or N - 2 (without) |
| Bytes per trigram | 3 bytes (ASCII) or 3-12 bytes (UTF-8) |
| Posting list entry | 4-8 bytes (document ID) |
| GIN index total | 1x-5x column data size |
| GiST bitmap signature | 12-256 bytes per row (configurable) |
| Google Code Search index | ~20% of source data |
| Bloom filter per doc | ~10 bits per unique trigram |

### Query Time Complexity

| Operation | Complexity |
|-----------|-----------|
| Trigram extraction from query | O(Q) where Q = query length |
| Single trigram lookup | O(1) hash or O(log T) B-tree, T = unique trigrams |
| Posting list intersection (2 lists) | O(\|A\| + \|B\|) merge, or O(\|A\| log \|B\|) galloping |
| k-way intersection | O(k * \|shortest\|) best case |
| GiST bitmap check | O(siglen) per row — bitwise AND |
| Similarity recheck | O(N) per candidate string |

### Index Build Time

| Operation | Complexity |
|-----------|-----------|
| Extract trigrams from one string | O(N) |
| Build full inverted index | O(D * avg_len) where D = document count |
| Sort posting lists | O(D * log D) per trigram |

### Practical Benchmarks (from PostgreSQL studies)

- **Sequential scan (no index):** 94+ seconds on ~10K rows with text columns
- **GiST trigram index (siglen=256):** ~2 seconds (47x speedup)
- **Optimized expression index:** 39-113ms (up to 3600x speedup)
- **Google Code Search:** Narrowed 36,972 files to 25 candidates for a regex query (~100x faster than brute force)

### Precision Degradation

**Critical caveat:** Trigram precision degrades as text length increases. For a query with few trigrams against documents with many trigrams, nearly every document will match at least some query trigrams. Short queries against long texts = low precision = many false positives = expensive rechecks.

---

## 6. Wildcard and Substring Search

### How Trigrams Enable `LIKE '%foo%'`

Without trigram indexes, `LIKE '%foo%'` requires a **full table scan** — the database must check every row. B-tree indexes are useless here because the pattern has a leading wildcard.

With trigrams:

```sql
-- Query: WHERE name LIKE '%catch%'
-- 1. Extract trigrams from 'catch': {"cat", "atc", "tch"}
--    (no padding — wildcard implies no word boundary)
-- 2. Look up posting lists for "cat", "atc", "tch"
-- 3. Intersect: only rows containing ALL three trigrams
-- 4. Recheck: run LIKE '%catch%' on candidates only
```

### Regex Queries

```sql
-- Query: WHERE name ~ 'hel(lo|p)me'
-- 1. Parse regex, extract trigram boolean query:
--    "hel" AND (("ell" AND "llo" AND "lom") OR ("elp" AND "lpm" AND "pme"))
-- 2. Execute boolean query against index
-- 3. Recheck: run full regex on candidates
```

### Pattern Length Requirements

- **Patterns < 3 characters** cannot use trigram index (no trigrams extractable)
- `LIKE '%ab%'` → no extractable trigrams → full scan
- `LIKE '%abc%'` → one trigram `"abc"` → usable
- `LIKE '%abcdef%'` → four trigrams → very selective

### Leading/Trailing Patterns

- `LIKE 'abc%'` → trigrams include padded prefix `"  a"`, `" ab"`, `"abc"` — very selective
- `LIKE '%abc'` → trigrams include padded suffix `"abc"`, `"bc "` — selective
- `LIKE '%abc%'` → only interior trigrams — less selective but still useful

---

## 7. Practical Implementations

### PostgreSQL pg_trgm

**The reference implementation.** Available as a contrib extension since PostgreSQL 9.1.

**Key design decisions:**
- **Padding:** 2-space prefix, 1-space suffix per word
- **Normalization:** Lowercase, strip non-alphanumerics (word boundaries)
- **Similarity metric:** Count shared trigrams / (count trigrams in A + count trigrams in B) — effectively Dice coefficient
- **Default threshold:** 0.3 for the `%` (similarity) operator

**Two index types:**

| Feature | GIN | GiST |
|---------|-----|------|
| Stores | Exact trigram→TID mapping | Bitmap signature per row |
| Precision | Exact (no false positives from index) | Lossy (recheck required) |
| Distance queries | Not supported | Supported (ORDER BY distance) |
| LIKE/regex | Supported | Supported |
| Similarity `%` | Supported | Supported |
| Build time | Slower | Faster |
| Index size | Larger | Smaller (tunable via siglen) |
| Update cost | Higher | Lower |

**GiST siglen tuning:**
```sql
-- Default: 12 bytes (96 bits) — small but imprecise
CREATE INDEX idx ON t USING GIST (col gist_trgm_ops);

-- Larger signature: more precise, larger index
CREATE INDEX idx ON t USING GIST (col gist_trgm_ops(siglen=256));
```

Benchmark: siglen=64 → ~4.2s query, siglen=256 → ~2s query, for marginal index size increase.

### Elasticsearch N-gram Tokenizer

Elasticsearch implements trigrams at the **analyzer** level, not as a separate index type:

```json
{
  "settings": {
    "analysis": {
      "tokenizer": {
        "trigram_tokenizer": {
          "type": "ngram",
          "min_gram": 3,
          "max_gram": 3,
          "token_chars": ["letter", "digit"]
        }
      },
      "analyzer": {
        "trigram_analyzer": {
          "type": "custom",
          "tokenizer": "trigram_tokenizer"
        }
      }
    }
  }
}
```

**Key differences from PostgreSQL:**
- Trigrams are indexed as terms in the standard inverted index (Lucene)
- Index-time tokenization generates all trigrams per document
- Search-time can use a different analyzer (e.g., standard tokenizer for query)
- **Major warning:** N-gram tokenization dramatically increases index size and memory usage. Each word of length N generates N-2 terms. Elasticsearch experts warn against indiscriminate use.

**Alternatives Elasticsearch recommends:**
- `edge_ngram` filter for prefix search (produces only prefix n-grams)
- `search_as_you_type` field type (optimized autocomplete)
- Fuzzy queries (edit distance based, no trigrams)

### Google Code Search (Russ Cox, 2006-2012)

**Architecture:**
1. Build trigram inverted index from all source files
2. Convert regex to boolean trigram query using formal extraction rules
3. Execute trigram query to get candidate files
4. Run full regex against candidates only

**Key metrics:**
- Index size: ~20% of source (77 MB for 420 MB kernel)
- Regex search: 36,972 files → 25 candidates, ~100x speedup

**Boolean query extraction rules:**
- Concatenation → AND
- Alternation → OR
- Kleene star → ANY (wildcard, matches all)
- Strings < 3 chars → ANY

### CockroachDB

Implements trigram indexes compatible with PostgreSQL's pg_trgm, using the same padding and similarity conventions. Stored as inverted indexes internally.

### McObject eXtremeDB

Commercial embedded database with native trigram index support. Uses standard 3-character sliding window.

### trilite (SQLite extension)

Open-source inverted trigram index for SQLite, providing accelerated string matching via `LIKE` and `GLOB` operators.

---

## 8. Edge Cases

### Short Strings (< 3 Characters)

- A string of length 0 produces **0 trigrams** (even with padding, `"   "` is all spaces)
- A string of length 1 like `"a"` with padding → `"  a"`, `" a "` — 2 trigrams
- A string of length 2 like `"ab"` with padding → `"  a"`, `" ab"`, `"ab "` — 3 trigrams
- **Without padding:** strings shorter than 3 characters produce 0 trigrams and are effectively invisible to the index

**Implication:** Padding is critical for indexing short strings. Without it, 1-2 character values cannot participate in trigram matching at all.

### Padding Strategies

| Strategy | Prefix | Suffix | Trigrams from "ab" |
|----------|--------|--------|-------------------|
| No padding | — | — | 0 (invisible!) |
| PostgreSQL | 2 spaces | 1 space | `"  a"`, `" ab"`, `"ab "` = 3 |
| Symmetric | 2 spaces | 2 spaces | `"  a"`, `" ab"`, `"ab "`, `"b  "` = 4 |
| Null byte | 2 NUL | 1 NUL | Same count, avoids space collision |

**Recommendation:** Use padding. PostgreSQL's convention (2 prefix, 1 suffix spaces) is well-tested and widely understood. Consider using a sentinel character (not space) if spaces are meaningful in the data.

### Unicode Handling

**Byte-level trigrams (naive):**
- UTF-8 characters are 1-4 bytes
- Byte-level trigrams will split multi-byte characters: `"日本語"` in UTF-8 is 9 bytes → 7 byte-trigrams, all nonsensical
- Fast but meaningless for non-ASCII text

**Character-level trigrams (correct):**
- Operate on Unicode codepoints, not bytes
- `"日本語"` → `"日本語"` (one trigram)
- `"café"` → `"caf"`, `"afé"` (but `"é"` may be 1 or 2 codepoints depending on normalization)

**Grapheme cluster trigrams (thorough):**
- Handle combining characters, emoji sequences, etc.
- `"café"` (if `é` = `e` + combining accent) treated as single character
- Most complex, most correct

**PostgreSQL approach:** Operates on characters (codepoints), not bytes. Non-alphanumeric characters are stripped, so most combining characters and punctuation are filtered out naturally.

### Case Sensitivity

- **PostgreSQL:** Lowercases all text before trigram extraction → case-insensitive by default
- **Google Code Search:** Indexed case-sensitively; case-insensitive search generated less precise trigram queries (lowered all trigrams → more false positives)
- **Design choice:** Case-insensitive is more useful for most applications but doubles the false positive rate for case-sensitive data

### Identical Trigrams from Different Sources

The string `"abcabc"` produces trigrams `{"abc", "bca", "cab"}` — each appears once despite the repetition. Trigram sets are **sets**, not multisets. Frequency information is typically discarded. This means `"aaa"` and `"aaaaaaaaa"` produce the same single trigram `{"aaa"}` and appear identical to the trigram index.

---

## 9. Optimizations

### Bloom Filters on Trigram Sets

Instead of storing exact trigram posting lists, represent each document's trigram set as a Bloom filter:

- **Space:** ~10 bits per unique trigram (vs. 3 bytes per trigram + posting overhead)
- **Query:** Hash query trigrams → check bits → candidates where all bits set
- **Trade-off:** False positives (tunable via filter size), but dramatically reduced memory

**Bit-parallel matching:** Store Bloom filters as fixed-width bit vectors. Query = compute query's Bloom filter, then AND with each document's filter. Documents where `(query_filter & doc_filter) == query_filter` are candidates.

This is essentially what PostgreSQL's GiST trigram index does — the `siglen` bitmap signature IS a Bloom filter.

### Minimum Trigram Match Threshold

For similarity queries, don't require ALL trigrams to match. Set a minimum overlap:

```
threshold = pg_trgm.similarity_threshold (default 0.3)
If shared_trigrams / total_trigrams >= threshold → candidate
```

**Optimization:** Sort posting lists by length. Start with the rarest trigrams (shortest posting lists). If even the rarest trigrams match too many documents, the query is too vague — short-circuit and do a full scan instead.

### Posting List Pruning

- **Frequency-based pruning:** Skip trigrams that appear in > X% of documents (too common to be selective). Similar to stop-word elimination.
- **Length-based pruning:** Skip posting lists longer than a threshold; they contribute little selectivity.
- **IDF weighting:** Weight trigrams by inverse document frequency. Rare trigrams are more valuable for filtering.

### Query Optimization

- **Trigram ordering:** Process trigrams from rarest to most common. Intersect the shortest posting lists first.
- **Early termination:** If the candidate set after the first few intersections is already small enough, skip remaining trigrams and go straight to recheck.
- **Adaptive strategy:** For queries with many trigrams, pick the top-k rarest and intersect only those.

### Compression

- **Delta encoding:** Store posting list entries as deltas from previous entry. Since lists are sorted, deltas are small integers.
- **Variable-byte encoding (VByte):** Encode deltas using 1-4 bytes depending on magnitude.
- **Roaring Bitmaps:** For dense posting lists, use compressed bitmap representation instead of sorted integer lists.

### Batch Similarity

When computing similarity of a query against many documents:
- Pre-compute the query's trigram set once
- For each candidate, count the intersection using bitwise AND on bitmap representations
- Use SIMD (AVX2/AVX-512) for parallel bitwise operations across multiple candidates

---

## 10. Comparison with Other Approaches

### B-Tree Substring Indexes

| Aspect | B-Tree | Trigram |
|--------|--------|--------|
| `WHERE x = 'exact'` | Excellent (O(log N)) | Slower (trigram lookup + recheck) |
| `WHERE x LIKE 'prefix%'` | Excellent (range scan) | Good (padded trigrams capture prefix) |
| `WHERE x LIKE '%suffix'` | Useless (full scan) | Good (padded trigrams capture suffix) |
| `WHERE x LIKE '%middle%'` | Useless (full scan) | Good (trigram intersection) |
| Fuzzy/similarity search | Not supported | Core strength |
| Space overhead | 1x-2x data | 1x-5x data |
| Build time | O(N log N) | O(N * avg_len) |

**Verdict:** B-trees and trigram indexes are complementary. Use B-trees for exact and prefix matching, trigrams for substring and fuzzy matching.

### Suffix Arrays

| Aspect | Suffix Array | Trigram |
|--------|-------------|--------|
| Exact substring search | Excellent (O(M log N)) | Good (intersection + recheck) |
| Space | 4x-8x data | 1x-5x data (posting lists) or <1x (Bloom) |
| Build time | O(N) to O(N log N) | O(N) |
| Regex support | Limited (via suffix array + LCP) | Good (Boolean trigram queries) |
| Fuzzy/similarity | Not native | Core strength |
| Updateability | Poor (rebuild required) | Good (append to posting lists) |
| Cache behavior | Excellent (sorted, sequential) | Poor (random access to posting lists) |

**Verdict:** Suffix arrays are better for exact substring location in static text. Trigram indexes are better for fuzzy matching, updateable data, and regex-style queries.

### Full-Text / Inverted Word Indexes

| Aspect | Word Inverted Index | Trigram |
|--------|-------------------|--------|
| Word search | Excellent | Overkill |
| Partial word match | Not supported | Core strength |
| Typo tolerance | Via stemming/synonyms | Native (fuzzy similarity) |
| Space | Compact (one entry per word) | Larger (many trigrams per word) |
| `LIKE '%foo%'` within words | Not supported | Supported |

**Verdict:** Word-level inverted indexes for full-text search, trigram indexes for partial-word and fuzzy matching. They complement each other.

---

## 11. Mapping Trigrams to AeorDB's NVT Architecture

### The Core Idea

AeorDB's NVT (Normalized Vector Table) maps values to scalars in [0.0, 1.0] via `ScalarConverter`, then uses bitmap compositing for query execution. Trigram indexing can map naturally onto this:

### Approach: Trigram → Hash → Scalar → NVT

```
1. Extract trigrams from a string value
2. Hash each trigram to a scalar in [0.0, 1.0] using HashConverter
3. Store each (trigram_scalar, document_id) pair in a shared Trigram NVT
4. At query time:
   a. Extract trigrams from query string
   b. Hash each to scalar
   c. Look up each scalar's bucket in the NVT
   d. Get NVTMasks for each trigram bucket
   e. AND masks together → candidate set
   f. Recheck candidates against actual string
```

### Why This Works with NVT

- **HashConverter** is already designed for non-order-preserving lookups (equality). Trigram matching IS equality matching on individual trigrams.
- **NVTMask AND compositing** is exactly posting list intersection — but in bitmap space, which is O(bucket_count/64) regardless of data size.
- **The two-tier execution model** fits perfectly: Tier 1 for single-trigram lookups, Tier 2 for multi-trigram boolean compositing.

### Proposed ScalarConverter for Trigrams

```rust
pub struct TrigramConverter {
    // No range tracking needed — hash distribution is already uniform
}

impl ScalarConverter for TrigramConverter {
    fn to_scalar(&self, value: &[u8]) -> f64 {
        // value is a 3-byte trigram (or padded equivalent)
        // Hash to [0.0, 1.0] using a fast hash (FxHash, xxHash)
        let hash = fast_hash(value);
        (hash as f64) / (u64::MAX as f64)
    }

    fn is_order_preserving(&self) -> bool {
        false  // Hash-based, no ordering
    }

    fn name(&self) -> &str {
        "trigram"
    }
}
```

### Index Structure

Instead of one NVT per field (like numeric indexes), a trigram index would use:

**Option A: One NVT per trigram field**
- Each entry in the NVT is a (trigram_hash, document_id) pair
- Query = multiple lookups in same NVT, AND the resulting masks
- Simpler, fits existing architecture

**Option B: One NVT per unique trigram** (like PostgreSQL GIN)
- Each NVT is a posting list for one trigram
- Enormous number of NVTs (up to 16M+ for full Unicode trigrams)
- Not practical

**Option A is clearly the right fit.** The NVT's bucket structure naturally handles the "many keys mapping to document sets" pattern.

### Similarity Queries on NVT

For similarity (fuzzy) queries:
1. Extract N trigrams from query
2. For each trigram, get an NVTMask (bucket hit)
3. Instead of AND-all, count how many masks each document appears in
4. Score = count / N (or use Dice: 2*count / (N + doc_trigram_count))
5. Return documents above threshold

**This maps to OR compositing + popcount:**
```
mask_a = NVTMask for trigram_1
mask_b = NVTMask for trigram_2
mask_c = NVTMask for trigram_3
...
combined = mask_a | mask_b | mask_c  // All possible candidates

// But we need per-document counts, not just presence.
// Options:
// 1. Multi-pass: AND subsets, count survivors across passes
// 2. Weighted scoring: use the masks at the recheck phase
// 3. Bitmap popcount across mask layers (one mask per trigram)
```

**Practical approach:** Use bitmap AND for substring/exact queries (high precision, the common case). For similarity queries, use the trigram masks to identify a candidate set (OR all masks), then compute exact similarity scores during the recheck phase against the actual string values. The NVT eliminates the vast majority of documents; the recheck handles scoring.

### Handling Edge Cases in NVT

- **Short strings:** Padding ensures they produce trigrams and get indexed
- **Hash collisions:** Multiple different trigrams may hash to the same NVT bucket. This is analogous to PostgreSQL's GiST bitmap signatures. The recheck phase handles false positives.
- **Bucket resolution:** More NVT buckets = fewer collisions = fewer false positives. The existing NVT resolution scaling handles this naturally.

### Proposed API

```json
{
  "path": "/users/",
  "where": {
    "field": "name",
    "op": "trigram_similar",
    "value": "Jon",
    "threshold": 0.3
  }
}
```

```json
{
  "path": "/users/",
  "where": {
    "field": "name",
    "op": "contains",
    "value": "catch"
  }
}
```

### Implementation Tasks (Estimated)

1. **TrigramConverter** — hash trigrams to scalar [0.0, 1.0]
2. **Trigram extraction utility** — padding, normalization, Unicode-aware decomposition
3. **Trigram field index** — one NVT stores all (trigram_hash → doc_id) entries for a field
4. **Substring query (AND compositing)** — extract trigrams, AND masks, recheck
5. **Similarity query** — extract trigrams, score candidates, recheck with Dice/Jaccard
6. **LIKE pattern support** — convert LIKE patterns to trigram boolean queries
7. **Regex support** — Russ Cox-style regex → trigram boolean query extraction (advanced, later)

---

## Sources

- [PostgreSQL pg_trgm Documentation](https://www.postgresql.org/docs/current/pgtrgm.html)
- [Russ Cox — Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html)
- [CockroachDB — Trigram Indexes](https://www.cockroachlabs.com/docs/stable/trigram-indexes)
- [CockroachDB — Use Cases for Trigram Indexes](https://www.cockroachlabs.com/blog/use-cases-trigram-indexes/)
- [Elasticsearch — N-gram Tokenizer](https://www.elastic.co/guide/en/elasticsearch/reference/current/analysis-ngram-tokenizer.html)
- [Sease — When and How to Use N-grams in Elasticsearch](https://sease.io/2023/12/when-and-how-to-use-n-grams-in-elasticsearch.html)
- [Alex Klibisz — Optimizing Postgres Text Search with Trigrams](https://alexklibisz.com/2022/02/18/optimizing-postgres-trigram-search)
- [pganalyze — Optimizing Postgres Text Search with Trigrams and GiST Indexes](https://pganalyze.com/blog/5mins-postgres-optimizing-postgres-text-search-trigrams-gist-indexes)
- [Ben Boyter — Bloom Filters: Much More Than a Space Efficient Hashmap](https://boyter.org/posts/bloom-filter/)
- [Francesco Tomaselli — Search Engine in Rust](https://tomfran.github.io/posts/search-engine/)
- [trilite — Inverted Trigram Index for SQLite](https://github.com/jonasfj/trilite)
- [Hexops — Postgres Trigram Search Learnings](https://devlog.hexops.org/2021/postgres-trigram-search-learnings/)
- [Trigram Vector Search (GitHub)](https://github.com/ranfysvalle02/trigram-vector-search)
- [F-scores, Dice, and Jaccard Set Similarity](https://brenocon.com/blog/2012/04/f-scores-dice-and-jaccard-set-similarity/)
- [NVIDIA — Similarity in Graphs: Jaccard vs Overlap Coefficient](https://developer.nvidia.com/blog/similarity-in-graphs-jaccard-versus-the-overlap-coefficient/)
