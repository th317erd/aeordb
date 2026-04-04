# Fuzzy Search, Trigram Indexing & Phonetic Matching — Implementation Spec

**Date:** 2026-04-03
**Status:** Approved
**Research:** `bot-docs/docs/fuzzy-search-research.md`, `trigram-indexing-research.md`, `phonetic-indexing-research.md`

---

## 1. Overview

Three complementary search strategies added to the existing ScalarConverter + NVT + bitmap compositing architecture:

| Strategy | Catches | Index Type | Score Type |
|----------|---------|-----------|------------|
| Trigram | Typos, substrings, transpositions | Inverted trigram index via NVT | Dice coefficient [0.0, 1.0] |
| Phonetic | Sound-alike variants | Phonetic code hash via NVT | Binary (1.0 = match) |
| Fuzzy scoring | Ranked similarity | Recheck on trigram candidates | DL distance or Jaro-Winkler [0.0, 1.0] |

All three use the existing two-tier execution model: NVT bitmap ops for candidate generation, then a new recheck phase for verification and scoring.

---

## 2. Multi-Index Per Field

### Current Limitation

One index per field per path. Path is deterministic: `{dir}/.indexes/{field_name}.idx`.

### New Naming Scheme

```
{dir}/.indexes/{field_name}.{strategy}.idx
```

Examples:
```
users/.indexes/name.string.idx            # exact/range (existing behavior)
users/.indexes/name.trigram.idx           # trigram similarity
users/.indexes/name.dmetaphone.idx        # Double Metaphone primary
users/.indexes/name.dmetaphone_alt.idx    # Double Metaphone alternate
users/.indexes/name.soundex.idx           # Soundex
```

### Backward Compatibility

Existing `.idx` files without a strategy suffix are loaded as strategy `"string"`. No migration needed.

### ScalarConverter Trait Addition

```rust
trait ScalarConverter {
    fn to_scalar(&self, value: &[u8]) -> f64;
    fn is_order_preserving(&self) -> bool;
    fn name(&self) -> &str;
    fn strategy(&self) -> &str;        // NEW: "string", "trigram", "dmetaphone", etc.
    fn expand_value(&self, value: &[u8]) -> Vec<Vec<u8>> {
        vec![value.to_vec()]           // NEW: default = one entry per value
    }
    // existing serialize/deserialize methods unchanged
}
```

`expand_value()` enables multi-entry converters:
- `TrigramConverter::expand_value("hello")` returns `[" h", " he", "hel", "ell", "llo", "lo "]` (6 entries)
- `PhoneticConverter::expand_value("Schmidt")` returns `["XMT"]` (1 entry for DM primary)
- `DaitchMokotoffConverter::expand_value("Bierschbach")` returns up to 32 phonetic codes
- All existing converters return `vec![value.to_vec()]` (unchanged behavior)

The indexing pipeline calls `expand_value()` first, then `to_scalar()` on each expanded value. Each produces a separate `IndexEntry` pointing to the same `file_hash`.

### IndexManager Changes

```rust
// Old
fn index_file_path(path: &str, field_name: &str) -> String

// New
fn index_file_path(path: &str, field_name: &str, strategy: &str) -> String

// New methods
fn load_indexes_for_field(path: &str, field_name: &str) -> Vec<FieldIndex>
fn save_index(path: &str, index: &FieldIndex) -> EngineResult<()>  // uses index.converter.strategy()
```

### Index Configuration

```json
{
  "indexes": [
    { "field_name": "name", "converter_type": "string" },
    { "field_name": "name", "converter_type": "trigram" },
    { "field_name": "name", "converter_type": "phonetic", "algorithm": "double_metaphone" },
    { "field_name": "name", "converter_type": "phonetic", "algorithm": "soundex" },
    { "field_name": "age", "converter_type": "u64" }
  ]
}
```

Multiple converters on the same field are allowed. Each produces its own `.idx` file keyed by strategy name.

---

## 3. QueryResult with Scoring

### Current

```rust
pub struct QueryResult {
    pub file_hash: Vec<u8>,
    pub file_record: FileRecord,
}
```

### New

```rust
pub struct QueryResult {
    pub file_hash: Vec<u8>,
    pub file_record: FileRecord,
    pub score: f64,              // 0.0-1.0, higher = better match
    pub matched_by: Vec<String>, // which index strategies matched (e.g., ["trigram", "dmetaphone"])
}
```

### Score Computation

| Query Op | Score Formula | Notes |
|----------|-------------|-------|
| `eq` | 1.0 | Binary match |
| `gt`, `lt`, `between`, `in` | 1.0 | Binary match |
| `contains` | 1.0 if recheck passes | Trigram candidates, substring recheck |
| `similar` | Dice coefficient: `2*\|A∩B\| / (\|A\|+\|B\|)` | Trigram similarity |
| `phonetic` | 1.0 | Binary code match |
| `fuzzy` (DL) | `1.0 - (distance / max(len_a, len_b))` | Normalized edit distance |
| `fuzzy` (JW) | Jaro-Winkler similarity | Native [0.0, 1.0] |
| `match` (composite) | `max(score_per_strategy)` | Score fusion |

### Default Sort Order

Results sorted by `score` descending. Existing exact/range queries all score 1.0 — unordered among themselves (unchanged behavior).

### Score Fusion for Composite Queries

When multiple indexes match the same file, take the **max score** across strategies. `matched_by` lists all strategies that contributed.

---

## 4. Recheck Phase

### Why

NVT candidate generation has false positives from hash collisions and bucket imprecision. Trigram/fuzzy/phonetic queries require verification against actual values.

### Pipeline

```
Stage 1: NVT Candidate Generation
    - Existing two-tier model (flat AND or bitmap compositing)
    - Returns HashSet<Vec<u8>> of candidate file_hashes
    ↓
Stage 2: Recheck + Scoring (NEW)
    - Load candidate file's JSON data from engine
    - Extract the queried field value
    - Compute precise score using appropriate algorithm
    - Filter by threshold (similarity >= threshold, distance <= max_distance)
    - Discard false positives
    ↓
Sort by score descending → return Vec<QueryResult>
```

### Where It Lives

New method in `QueryEngine`:

```rust
fn recheck_and_score(
    &self,
    candidates: HashSet<Vec<u8>>,
    field_name: &str,
    query_value: &str,
    op: &QueryOp,
    options: &QueryOptions,
) -> EngineResult<Vec<QueryResult>>
```

Existing query paths (`eq`, `gt`, `lt`, `between`, `in`) skip recheck — no false positives, no scoring needed. They set `score: 1.0` and return immediately.

---

## 5. New Converter Types

### TrigramConverter (type tag: 0x0B)

```rust
pub const CONVERTER_TYPE_TRIGRAM: u8 = 0x0B;

pub struct TrigramConverter;

impl ScalarConverter for TrigramConverter {
    fn to_scalar(&self, value: &[u8]) -> f64 {
        // value is a single trigram (3 bytes after padding/extraction)
        let hash = u64::from_le_bytes(blake3::hash(value).as_bytes()[..8].try_into().unwrap());
        hash as f64 / u64::MAX as f64
    }

    fn expand_value(&self, value: &[u8]) -> Vec<Vec<u8>> {
        let text = std::str::from_utf8(value).unwrap_or("");
        extract_trigrams(text)
            .into_iter()
            .map(|t| t.to_vec())
            .collect()
    }

    fn strategy(&self) -> &str { "trigram" }
    fn is_order_preserving(&self) -> bool { false }
    fn name(&self) -> &str { "trigram" }
    fn type_tag(&self) -> u8 { CONVERTER_TYPE_TRIGRAM }
}
```

### PhoneticConverter (type tag: 0x0C)

```rust
pub const CONVERTER_TYPE_PHONETIC: u8 = 0x0C;

pub enum PhoneticAlgorithm {
    Soundex              = 0,
    DoubleMetaphonePrimary = 1,
    DoubleMetaphoneAlt   = 2,
    Nysiis               = 3,
    ColognePhonetics     = 4,
    DaitchMokotoff       = 5,
}

pub struct PhoneticConverter {
    algorithm: PhoneticAlgorithm,
}

impl ScalarConverter for PhoneticConverter {
    fn to_scalar(&self, value: &[u8]) -> f64 {
        // value is a phonetic code string (from expand_value)
        let hash = u64::from_le_bytes(blake3::hash(value).as_bytes()[..8].try_into().unwrap());
        hash as f64 / u64::MAX as f64
    }

    fn expand_value(&self, value: &[u8]) -> Vec<Vec<u8>> {
        let text = std::str::from_utf8(value).unwrap_or("");
        match self.algorithm {
            PhoneticAlgorithm::Soundex => vec![soundex(text).into_bytes()],
            PhoneticAlgorithm::DoubleMetaphonePrimary => vec![dmetaphone_primary(text).into_bytes()],
            PhoneticAlgorithm::DoubleMetaphoneAlt => {
                match dmetaphone_alt(text) {
                    Some(code) => vec![code.into_bytes()],
                    None => vec![dmetaphone_primary(text).into_bytes()], // fallback to primary
                }
            }
            PhoneticAlgorithm::DaitchMokotoff => {
                daitch_mokotoff(text).into_iter().map(|c| c.into_bytes()).collect()
                // Up to 32 codes per name
            }
            // ... other algorithms
        }
    }

    fn strategy(&self) -> &str {
        match self.algorithm {
            PhoneticAlgorithm::Soundex => "soundex",
            PhoneticAlgorithm::DoubleMetaphonePrimary => "dmetaphone",
            PhoneticAlgorithm::DoubleMetaphoneAlt => "dmetaphone_alt",
            PhoneticAlgorithm::Nysiis => "nysiis",
            PhoneticAlgorithm::ColognePhonetics => "cologne",
            PhoneticAlgorithm::DaitchMokotoff => "daitch_mokotoff",
        }
    }

    fn is_order_preserving(&self) -> bool { false }
    fn name(&self) -> &str { "phonetic" }
    fn type_tag(&self) -> u8 { CONVERTER_TYPE_PHONETIC }
}
```

### Serialization

Phonetic converter serializes as: `[CONVERTER_TYPE_PHONETIC, algorithm_byte]`.
Trigram converter serializes as: `[CONVERTER_TYPE_TRIGRAM]`.

Both deserialize via the existing `deserialize_converter()` match.

---

## 6. Query API

### New Operations

```json
// Trigram substring search (LIKE '%catch%')
{ "field": "name", "op": "contains", "value": "catch" }

// Trigram similarity (character overlap, Dice coefficient)
{ "field": "name", "op": "similar", "value": "Jon", "threshold": 0.3 }

// Phonetic match (sound-alike)
{ "field": "name", "op": "phonetic", "value": "Schmidt" }

// Fuzzy match (edit distance or Jaro-Winkler)
{ "field": "name", "op": "fuzzy", "value": "restaurant" }
{ "field": "name", "op": "fuzzy", "value": "restaurant", "fuzziness": "auto" }
{ "field": "name", "op": "fuzzy", "value": "Jon", "algorithm": "jaro_winkler" }
{ "field": "name", "op": "fuzzy", "value": "restaurant", "algorithm": "damerau_levenshtein" }

// Composite: run all matching indexes, union results, score-fuse
{ "field": "name", "op": "match", "value": "Schmidt" }
```

### Index Selection

Operations automatically select the appropriate index strategy:

| Operation | Uses Index Strategy | Fallback |
|-----------|-------------------|----------|
| `eq`, `gt`, `lt`, `between` | `string`, numeric converters | Error if no matching index |
| `contains` | `trigram` | Error if no trigram index |
| `similar` | `trigram` | Error if no trigram index |
| `phonetic` | All phonetic indexes on field | Error if no phonetic index |
| `fuzzy` | `trigram` (for candidates) | Error if no trigram index |
| `match` | All indexes on field | At least one index required |

### Explicit Index Targeting

The user can force a specific index:

```json
{ "field": "name", "op": "phonetic", "value": "Schmidt", "index": "dmetaphone" }
{ "field": "name", "op": "phonetic", "value": "Schmidt", "index": "soundex" }
{ "field": "name", "op": "similar", "value": "Jon", "index": "trigram" }
```

When `"index"` is omitted:
- `phonetic`: uses ALL phonetic indexes on the field, unions results
- `similar`/`contains`/`fuzzy`: uses `trigram` index
- `match`: uses ALL indexes on the field

### Fuzziness

```json
{ "fuzziness": "auto" }   // 0 edits for 1-2 chars, 1 for 3-5, 2 for 6+
{ "fuzziness": 0 }        // exact (no fuzzy)
{ "fuzziness": 1 }        // max 1 edit
{ "fuzziness": 2 }        // max 2 edits (capped — higher is rarely useful)
```

Default: `"auto"` when `"op": "fuzzy"` is used.

### Fuzzy Algorithm

```json
{ "algorithm": "damerau_levenshtein" }   // default — insert/delete/substitute/transpose
{ "algorithm": "jaro_winkler" }          // prefix-weighted ratio, better for short strings/names
```

Default: `"damerau_levenshtein"` when omitted. No auto-selection based on string length — the user picks the tool.

### Threshold

```json
{ "threshold": 0.3 }   // minimum score to include in results (Dice, Jaro-Winkler, etc.)
```

Default: `0.3` for `similar`, `0.0` for `fuzzy` (controlled by fuzziness instead).

---

## 7. Query Execution Flow

### `contains` (substring)

```
1. Extract trigrams from query value
2. For each trigram: hash → NVT bucket → NVTMask
3. AND all masks → candidate set
4. Recheck: load each candidate's field value, verify query is an actual substring
5. Score: 1.0 for matches, discard non-matches
6. Return sorted by score (all 1.0, so effectively unsorted)
```

### `similar` (trigram similarity)

```
1. Extract trigrams from query value
2. For each trigram: hash → NVT bucket → NVTMask
3. OR all masks → candidate set (broader than AND — catches partial overlap)
4. Recheck: extract trigrams from each candidate's value, compute Dice coefficient
5. Filter: discard candidates below threshold
6. Return sorted by Dice score descending
```

### `phonetic`

```
1. Compute phonetic code(s) of query value (e.g., Double Metaphone → primary + alternate)
2. For each code: hash → NVT bucket → NVTMask
3. OR masks across all phonetic indexes on the field → candidate set
4. Recheck: compute phonetic code of each candidate's value, verify code match
5. Score: 1.0 for matches
6. Return sorted by score (all 1.0)
```

### `fuzzy` (edit distance / Jaro-Winkler)

```
1. Uses trigram index for candidate generation (same as `similar` step 1-3)
2. Recheck: compute DL distance or JW similarity between query and each candidate
3. Filter by fuzziness (DL: distance <= max_edits) or threshold (JW: similarity >= threshold)
4. Score: DL → 1.0 - (distance / max_len), JW → raw similarity
5. Return sorted by score descending
```

### `match` (composite)

```
1. Identify all indexes on the field: [string, trigram, dmetaphone, dmetaphone_alt, soundex, ...]
2. Run appropriate operations per index type:
   - string: exact equality check
   - trigram: similarity query (Dice scoring)
   - phonetic: phonetic code match
3. Union all candidate sets
4. For candidates appearing in multiple strategies: score = max(scores), matched_by = all strategies
5. Return sorted by score descending
```

---

## 8. New Modules

### `engine/fuzzy.rs` — String Matching Utilities

```rust
// Trigram extraction with PostgreSQL-style padding (2-space prefix, 1-space suffix per word)
pub fn extract_trigrams(s: &str) -> Vec<Vec<u8>>;

// Trigram similarity (Dice coefficient)
pub fn trigram_similarity(a: &str, b: &str) -> f64;

// Damerau-Levenshtein distance (insert, delete, substitute, transpose)
pub fn damerau_levenshtein(a: &str, b: &str) -> usize;

// Jaro-Winkler similarity [0.0, 1.0] with prefix bonus
pub fn jaro_winkler(a: &str, b: &str) -> f64;

// Auto fuzziness threshold (Elasticsearch convention)
pub fn auto_fuzziness(len: usize) -> usize {
    match len {
        0..=2 => 0,
        3..=5 => 1,
        _ => 2,
    }
}
```

Unicode-aware: operates on codepoints, not bytes. Lowercases before trigram extraction (case-insensitive matching).

### `engine/phonetic.rs` — Phonetic Algorithms

Phase 1 (implement now):
```rust
pub fn soundex(s: &str) -> String;
pub fn dmetaphone_primary(s: &str) -> String;
pub fn dmetaphone_alt(s: &str) -> Option<String>;
```

Phase 2 (later):
```rust
pub fn nysiis(s: &str) -> String;
pub fn cologne_phonetics(s: &str) -> String;
pub fn daitch_mokotoff(s: &str) -> Vec<String>;
```

Implementation options:
- Use `rphonetic` crate if quality is sufficient
- Hand-implement Soundex (~60 lines) and Double Metaphone (~300 lines) if crate is lacking
- Soundex and Double Metaphone cover 90% of use cases

---

## 9. NVT Bucket Sizing

Different index types benefit from different bucket counts:

| Index Type | Recommended Buckets | Rationale |
|-----------|-------------------|-----------|
| Numeric (u8-u64, f64) | 1,024 (default) | Continuous distribution, well-spread |
| String | 1,024 (default) | Hash-based, uniform |
| Trigram | 4,096 | Many entries per document, need separation |
| Soundex | 8,192 | ~8,918 possible codes, match cardinality |
| Double Metaphone | 16,384 | ~10K-50K distinct codes in practice |
| Daitch-Mokotoff | 65,536 | 10^6 possible codes, need headroom |

Bucket count is configurable per index in the config:

```json
{ "field_name": "name", "converter_type": "trigram", "buckets": 4096 }
```

Default: 1,024 (existing behavior). Converter can override via a new trait method:

```rust
fn recommended_bucket_count(&self) -> usize { 1024 } // default
```

---

## 10. Edge Cases

### Short Strings (< 3 characters)

Trigram padding ensures short strings produce trigrams:
- `"a"` → `"  a"`, `" a "` → 2 trigrams
- `"ab"` → `"  a"`, `" ab"`, `"ab "` → 3 trigrams
- `""` → 0 trigrams (no index entries, never matches)

### Unicode

- Trigram extraction operates on Unicode codepoints, not bytes
- Lowercase normalization before extraction (case-insensitive)
- Non-alphanumeric characters treated as word boundaries (PostgreSQL convention)
- Phonetic algorithms: Latin-only input. Non-Latin characters produce empty/sentinel codes.

### Non-Latin Scripts (Phonetic)

If input contains non-Latin characters:
- Phonetic converter returns empty code → sentinel scalar (0.0)
- Document is still stored but won't match phonetic queries
- Trigram indexing still works for any script

### Name Prefixes

Not handled automatically in Phase 1. Future: configurable prefix stripping per phonetic index.

### Empty/Numeric Input

- Empty string: no trigrams extracted, no phonetic code → no index entries
- Numeric string: trigrams work normally, phonetic algorithms produce minimal/empty codes

---

## 11. Implementation Phases

### Phase 1 — Multi-Index Foundation

- Add `strategy()` and `expand_value()` to `ScalarConverter` trait (with defaults)
- Change `IndexManager::index_file_path` to include strategy suffix
- Update `IndexManager` to load/save/list multi-strategy indexes
- Add `score: f64` and `matched_by: Vec<String>` to `QueryResult`
- Backward compatibility for old `.idx` files (no strategy suffix → "string")
- Update `store_file_with_indexing` to call `expand_value` and insert multiple entries
- Update `delete_file_with_indexing` to remove from all indexes on field
- Tests: multi-index config, load/save, backward compat

### Phase 2 — Trigram Indexing

- `engine/fuzzy.rs`: `extract_trigrams`, `trigram_similarity` (Dice), Unicode normalization
- `TrigramConverter` (0x0B): `expand_value` decomposes to padded trigrams
- Wire into `deserialize_converter` and `create_converter_from_config`
- `contains` query op: AND compositing + substring recheck
- `similar` query op: OR compositing + Dice scoring + threshold filter
- NVT bucket count override (4,096 for trigram indexes)
- Tests: extraction, similarity scoring, substring search, Unicode, short strings, edge cases

### Phase 3 — Phonetic Indexing

- `engine/phonetic.rs`: Soundex, Double Metaphone (primary + alternate)
- `PhoneticConverter` (0x0C): serialization includes algorithm byte
- Double Metaphone: two FieldIndex entries per field (strategy: `dmetaphone`, `dmetaphone_alt`)
- `phonetic` query op: compute query code → NVT lookup → recheck
- NVT bucket count override (8K for Soundex, 16K for DM)
- Tests: Soundex correctness, DM cross-matching, non-English names, non-Latin input

### Phase 4 — Fuzzy Scoring + Recheck

- `engine/fuzzy.rs`: `damerau_levenshtein`, `jaro_winkler`, `auto_fuzziness`
- Recheck phase in `QueryEngine`: load field values, compute precise scores
- `fuzzy` query op: trigram candidates → DL/JW scoring → fuzziness filter
- `"algorithm"` and `"fuzziness"` query parameters
- Score-based result sorting (descending)
- Tests: DL correctness, JW correctness, auto fuzziness, scoring, ranking order

### Phase 5 — Composite Match + Polish

- `match` meta-operation: runs all matching indexes, unions, score-fuses
- `"index"` query parameter for explicit index targeting
- `"threshold"` query parameter
- Multi-strategy `matched_by` population
- HTTP query endpoint updates for new operations
- Tests: composite queries, explicit targeting, score fusion, full E2E through HTTP

---

## 12. Non-Goals (Deferred)

- SymSpell pre-computation (optimize later if single-term fuzzy is too slow)
- Levenshtein automata / FST intersection (requires sorted term dictionary infrastructure)
- Beider-Morse phonetic matching (enormous complexity, defer to Phase 2+ of phonetic)
- Regex → trigram boolean query extraction (Russ Cox approach — advanced, later)
- GPU-offloaded NVT compositing (future hardware optimization)
- LIKE pattern → trigram query conversion (can be added after `contains` works)
- Additional phonetic algorithms (NYSIIS, Cologne, Daitch-Mokotoff) — Phase 2 of phonetic
- Configurable prefix stripping for phonetic indexes
