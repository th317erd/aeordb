# Phonetic Indexing/Matching Algorithms — Comprehensive Research

**Date:** 2026-04-03
**Purpose:** Research report for AeorDB phonetic index support via ScalarConverter + NVT

---

## 1. Soundex

### History
Patented in 1918 by Robert C. Russell and Margaret King Odell. Adopted by the US Census Bureau in 1880, 1900, and 1910. The most widely known phonetic algorithm.

### Algorithm Steps
1. Retain the first letter of the name
2. Replace consonants with digits (after position 1):
   - `B, F, P, V` → **1** (labials)
   - `C, G, J, K, Q, S, X, Z` → **2** (gutturals/sibilants)
   - `D, T` → **3** (dentals)
   - `L` → **4** (liquid)
   - `M, N` → **5** (nasals)
   - `R` → **6** (rhotic)
3. Drop all occurrences of `A, E, I, O, U, H, W, Y` (except retained first letter)
4. Collapse adjacent identical digits to one digit (including when separated by H or W)
5. If a vowel separates two consonants with the same code, both are coded (the vowel acts as a separator)
6. Pad with trailing zeros or truncate to exactly 4 characters (1 letter + 3 digits)

### Examples
| Name | Code | Notes |
|------|------|-------|
| Robert | R163 | R, b→1, r→6, t→3 |
| Rupert | R163 | R, p→1, r→6, t→3 — same as Robert |
| Smith | S530 | S, m→5, t→3, pad 0 |
| Smythe | S530 | Same as Smith |
| Ashcraft | A261 | A, s→2, c→ (same code as s, separated by H so collapsed), r→6, f→1 |
| Tymczak | T522 | T, m→5, c→2, z→(same code as k, but vowel A separates them), k→2 |
| Pfister | P236 | P, f→(same group as P, collapsed), s→2, t→3, r→6 |
| Hermann | H655 | H, r→6, m→5, n→5 (collapsed with m) — actually H655 per Stanford IR book |

### Strengths
- Extremely simple and fast (O(n) single pass)
- Fixed-length output (4 chars) — easy to store and index
- Well-understood, implemented everywhere
- Good enough for basic English surname matching

### Weaknesses
- Fixed 4-character code loses information for long names
- Consonant groupings are too coarse (C, G, J, K, Q, S, X, Z all map to 2)
- First letter is preserved literally — "Katherine" and "Catherine" do NOT match (K000 vs C365)
- Vowels are completely discarded (except first letter position)
- Terrible for non-English names
- High false positive rate (~0.36 precision in benchmarks)
- No handling of silent letters, digraphs (PH, GH, etc.), or context-dependent pronunciation

---

## 2. Metaphone

### History
Published in 1990 by Lawrence Philips in *Computer Language* magazine. Designed as a direct improvement over Soundex.

### Key Improvements Over Soundex
- Uses information about English spelling/pronunciation inconsistencies
- Variable-length output (not fixed to 4 characters)
- Handles consonant clusters and silent letters
- Considers the entire word, not just the first letter + consonants
- Reduces output to 16 consonant sounds: `B F H J K L M N P R S T W X Y 0` (where `0` = "th")

### Algorithm Rules (Selected)

**Initial silent consonant handling:**
- Drop first letter if word starts with: `KN, GN, PN, AE, WR`
- Example: "Knight" → treat as "Night", "Gnome" → treat as "Nome", "Write" → treat as "Rite"

**Context-dependent consonant mapping:**
- `C` → `X` (sh sound) if followed by `-cia-` or `-ch-`
- `C` → `S` if followed by `-ci-`, `-ce-`, or `-cy-`
- `C` → silent if preceded by `S` and followed by `-ci-`, `-ce-`, `-cy-`
- `C` → `K` otherwise (including in `-sch-`)
- `D` → `J` if before `-ge-`, `-gy-`, `-gi-`
- `D` → `T` otherwise
- `G` → silent in `-gh-` (not at end, not before vowel), `-gn`, `-gned`
- `PH` → `F`
- `SCH` → `SKH` (C→K within SCH)
- `TH` → `0` (the theta sound)

### Examples
| Name | Soundex | Metaphone | Notes |
|------|---------|-----------|-------|
| Smith | S530 | SM0 | 0 = th sound |
| Schmidt | S530 | SXMT | Better differentiation |
| Phone | P500 | FN | PH→F handled |
| Knight | K523 | NT | Silent K handled |
| Wright | W623 | RT | Silent W handled |

### Strengths
- Much better English phonetic accuracy than Soundex
- Handles silent letters and digraphs
- Variable length preserves more information
- Lower false positive rate than Soundex

### Weaknesses
- English-only — rules are hardcoded for English pronunciation
- No handling of non-English origin names
- Single output per input — ambiguous pronunciations get one answer

---

## 3. Double Metaphone

### History
Published in 2000 by Lawrence Philips in *C/C++ Users Journal*. A major revision of Metaphone.

### The "Double" Concept — Why Two Codes
Many names, especially those of non-English origin, have two plausible pronunciations in English. Double Metaphone returns:
- **Primary code**: The most likely English pronunciation
- **Alternate code**: An alternative pronunciation based on the word's possible language of origin

Two names match if *either* of their codes match any code of the other name. This dramatically improves recall for international names.

### Language-Awareness
Double Metaphone recognizes spelling patterns from:
- Slavic languages (CZ, WR, etc.)
- Germanic languages (SCH, W as V, etc.)
- Celtic languages (GH patterns)
- Greek (PH, PS, etc.)
- French (silent consonants, nasal vowels)
- Italian (GN as NY, etc.)
- Spanish (J as H, LL, etc.)
- Chinese (transliterations)

It tests approximately **100 different contexts for the letter C alone**.

### Examples
| Name | Primary | Alternate | Notes |
|------|---------|-----------|-------|
| Smith | SM0 | XMT | Primary=English, Alt=Germanic |
| Schmidt | XMT | SMT | Primary=Germanic, Alt=Anglicized |
| Schwarz | XRTS | SRTS | German origin recognized |
| Peter | PTR | PTR | Same — unambiguous |
| Czar | SR | XR | Slavic CZ recognized |
| José | HS | — | Spanish J recognized |
| Cabrillo | KPRL | KPR | Spanish LL recognized |

**Cross-matching example:** "SMITH" (SM0, XMT) and "SCHMIDT" (XMT, SMT) match because both share the alternate/primary code `XMT`.

### Strengths
- Handles names from many language origins
- Two-code system dramatically improves recall for international names
- Much more comprehensive rule set than Metaphone
- De facto standard in modern search engines

### Weaknesses
- More complex to implement and slower than Metaphone/Soundex
- Still fundamentally English-pronunciation-centric
- Rule set is large but still finite — some languages poorly covered
- Two codes mean more index entries per name (storage/lookup cost)

---

## 4. NYSIIS (New York State Identification and Intelligence System)

### History
Developed in 1970 by the New York State Division of Criminal Justice Services. Claimed to be 2.7% more accurate than Soundex in their studies.

### How It Differs from Soundex
1. **Output format**: Produces pronounceable alphabetic strings, not letter+digits
2. **Preserves vowel positions**: Vowels mapped to `A` rather than discarded entirely
3. **Multi-character n-grams**: Handles digraphs (PH, KN, SCH, etc.)
4. **Transcodes both start and end** of name (Soundex only processes after first letter)

### Algorithm Steps

**Step 1 — Transcode first characters:**
| Pattern | Replacement |
|---------|-------------|
| MAC | MCC |
| KN | NN |
| K | C |
| PH, PF | FF |
| SCH | SSS |

**Step 2 — Transcode last characters:**
| Pattern | Replacement |
|---------|-------------|
| EE, IE | Y |
| DT, RT, RD, NT, ND | D |

**Step 3 — Set first character of key to first character of name**

**Step 4 — Translate remaining characters:**
| Pattern | Replacement | Condition |
|---------|-------------|-----------|
| EV | AF | — |
| A, E, I, O, U | A | All vowels → A |
| Q | G | — |
| Z | S | — |
| M | N | — |
| KN | N | — |
| K | C | (when not preceded by N) |
| SCH | SSS | — |
| PH | FF | — |
| H | (previous char) | If previous or next is non-vowel |
| W | (previous char) | If previous is vowel |

**Step 5-9 — Finalization:**
- Append current character to key only if it differs from last key character
- If last character is `S`, remove it
- If last two characters are `AY`, replace with `Y`
- If last character is `A`, remove it
- Truncate to 6 characters (original NYSIIS)

### Examples
| Name | Soundex | NYSIIS | Notes |
|------|---------|--------|-------|
| Mackenzie | M252 | MCANSY | MAC→MCC preserved |
| McDonald | M235 | MCDANL | Better prefix handling |
| O'Brien | O165 | OBRAN | |
| Phillips | P412 | FFALAP | PH→FF at start |

### When to Prefer NYSIIS
- Working with street names or addresses (better performance than Metaphone per studies)
- When you need pronounceable output codes (for human review)
- Mixed name/address datasets
- When Soundex is too coarse but Double Metaphone is overkill

---

## 5. Caverphone

### History
Created in 2002 by David Hood for the Caversham Project at the University of Otago, New Zealand. Revised to version 2.0 in 2004.

### Purpose
Built to match names in late 19th / early 20th century New Zealand electoral rolls, where names only needed to be in a "commonly recognisable form." Optimized for southern New Zealand English accents.

### What Makes It Different
- **Accent-specific**: Tuned for New Zealand English vowel shifts and consonant patterns
- **10-character output** (Caverphone 2.0): Much longer than Soundex's 4-char code, so fewer false positives
- **Consecutive rule application**: Rules applied as a series of string replacements in sequence (not a single-pass mapping)
- **Version 2.0** is described as a "general purpose English phonetic matching system"

### Key Rules (Caverphone 2.0)
1. Convert to lowercase
2. Remove non-alphabetic characters
3. Handle special endings: `e$` → remove, `mb$` → `m2`
4. Apply prefix rules: `cq` → `2q`, `ci` → `si`, `ce` → `se`, `cy` → `sy`, etc.
5. Apply digraph rules: `tch` → `2ch`, `ph` → `fh`, `th` → `0h`, `sch` → `s2h`, etc.
6. Strip vowels (after position 1, they become padding)
7. Pad to 10 characters with `1`s

### Strengths
- Very effective for New Zealand and Australian English names
- Longer code = lower false positive rate
- Version 2.0 is reasonably general for English

### Weaknesses
- Not well-suited for non-English names
- Relatively obscure — less library support than Metaphone/Soundex
- The accent-specific tuning can produce unexpected results for American/British names

---

## 6. Beider-Morse Phonetic Matching (BMPM)

### History
Developed by Alexander Beider and Stephen P. Morse. Originally designed for Jewish genealogy but applicable to any multi-language name matching problem.

### The Multi-Language Approach
BMPM operates in three stages:

**Stage 1 — Language Detection:**
Approximately 200 rules analyze the spelling to determine the likely language(s) of origin. If the language cannot be determined with confidence, generic rules are applied.

**Stage 2 — Language-Specific Phonetic Rules:**
Once the language is identified, language-specific transliteration rules convert the name to a phonetic alphabet. Each supported language has its own rule set.

**Stage 3 — Language-Independent Normalization:**
A final pass applies universal phonetic rules (voiced/unvoiced consonant equivalence, vowel reduction) to improve cross-language matching.

### Supported Languages
Russian (Cyrillic and transliterated), Polish, German, Romanian, Hungarian, Hebrew, French, Spanish, English. Plans for Lithuanian, Latvian, Italian, Greek, Turkish.

### Configuration Options
- **Rule type**: `exact` (strict matching) or `approx` (approximate matching, default)
- **Name type**: `ashkenazi`, `sephardic`, or `generic` (default)
- **Language set**: Explicit language(s) or auto-detect

### Example: The Name "Schwarz"
Can appear in documents as: Schwartz, Shwartz, Shvartz, Szwarc, Szwartz, Svarc, Chvarts, Chvartz, and various Hebrew/Yiddish spellings. BMPM recognizes all of these as phonetically equivalent.

### Strengths
- Dramatically lower false positive rate than Soundex/Metaphone
- True multi-language support with language detection
- Handles transliteration between scripts (Hebrew, Cyrillic → Latin)
- Configurable precision (exact vs. approximate)

### Weaknesses
- Most complex algorithm to implement (large rule tables)
- Slowest of all phonetic algorithms
- Can produce multiple phonetic codes per input (branching)
- Rule tables need maintenance as languages are added
- Overkill for English-only use cases

---

## 7. Cologne Phonetics (Kolner Phonetik)

### History
Published in 1969 by Hans Joachim Postel. Specifically designed for the German language.

### Coding Table
| Letter | Context | Code |
|--------|---------|------|
| A, E, I, J, O, U, Y | — | 0 |
| H | — | (ignored) |
| B | — | 1 |
| P | not before H | 1 |
| D, T | not before C, S, Z | 2 |
| F, V, W | — | 3 |
| P | before H | 3 |
| G, K, Q | — | 4 |
| C | onset before A,H,K,L,O,Q,R,U,X | 4 |
| C | before A,H,K,O,Q,U,X (not after S,Z) | 4 |
| X | not after C, K, Q | 48 |
| L | — | 5 |
| M, N | — | 6 |
| R | — | 7 |
| S, Z | — | 8 |
| C | after S,Z; or not before A,H,K,O,Q,U,X | 8 |
| D, T | before C, S, Z | 8 |
| X | after C, K, Q | 8 |

### Algorithm
1. Map each letter to its code using the context-dependent table above
2. Collapse consecutive duplicate codes to single code
3. Remove all `0` codes (except at beginning, but that is also then removed)

### Example
`Muller-Ludenscheidt` → `65752682`

### Key Differences from Soundex
- Variable-length output (not fixed to 4 characters)
- Context-dependent mappings (letter code depends on neighbors)
- Optimized for German phonology (handles umlauts, German consonant clusters)
- Letters like X get two-digit codes depending on context

### When to Use
- German name matching
- German address matching
- Any application dealing primarily with German-language text

---

## 8. Daitch-Mokotoff Soundex (Bonus — Important for Completeness)

### History
Invented in 1985 by Gary Mokotoff and Randy Daitch for Jewish genealogy with Eastern European names.

### Key Features
- **6-digit codes** (vs. Soundex's 4 characters) — higher precision
- First letter IS coded (unlike American Soundex which preserves it literally)
- Handles multi-character n-grams (CZ, SZ, ZS, SCH, etc.)
- **Multiple codes per name**: Can return up to 32 different encodings for ambiguous names
- Designed for Slavic and Yiddish surnames

### Examples (from PostgreSQL docs)
```
daitch_mokotoff('George')          → {595000}
daitch_mokotoff('John')            → {160000, 460000}
daitch_mokotoff('Bierschbach')     → {794575, 794574, 794750, 794740, 745750, 745740, 747500, 747400}
daitch_mokotoff('Schwartzenegger') → {479465}
```

Note "John" produces TWO codes, and "Bierschbach" produces EIGHT — reflecting pronunciation ambiguity.

---

## 9. Comparison Table

| Algorithm | Best For | Code Format | Code Length | Multi-Code | Order-Preserving | Language Support | False Positive Rate | Complexity |
|-----------|----------|-------------|-------------|------------|------------------|------------------|---------------------|------------|
| Soundex | Basic English surnames | Letter + digits | 4 (fixed) | No | No | English only | High (~0.36 precision) | Very low |
| Metaphone | English names/words | Letters | Variable | No | No | English only | Medium | Low |
| Double Metaphone | English + international names | Letters | Variable (typ. 4) | Yes (2) | No | Multi-language via rules | Low-Medium | Medium |
| NYSIIS | Names + addresses | Letters | 6 (truncated) | No | No | English-focused | Medium (2.7% better than Soundex) | Low |
| Caverphone 2.0 | NZ/AU English names | Letters + digits | 10 (fixed) | No | No | NZ/English | Low (long code) | Low |
| Beider-Morse | International names, genealogy | Phonetic tokens | Variable | Yes (many) | No | 10+ languages | Very low | Very high |
| Cologne Phonetics | German names/text | Digits | Variable | No | No | German | Low-Medium | Low |
| Daitch-Mokotoff | Slavic/Yiddish names | Digits | 6 (fixed) | Yes (up to 32) | No | Eastern European focus | Low | Medium |

### Use Case Recommendations

| Use Case | Recommended Algorithm(s) |
|----------|-------------------------|
| English surnames (US Census style) | Soundex or Metaphone |
| English names + general text | Double Metaphone |
| International names (diverse origins) | Beider-Morse or Double Metaphone |
| Eastern European / Jewish names | Daitch-Mokotoff or Beider-Morse |
| German names / German text | Cologne Phonetics |
| NZ/Australian names | Caverphone 2.0 |
| Street addresses | NYSIIS |
| Low false positives, high precision | Beider-Morse |
| Simplicity, speed, low memory | Soundex |
| General-purpose database default | Double Metaphone |

---

## 10. Phonetic Indexing in Databases

### PostgreSQL — fuzzystrmatch Extension

PostgreSQL provides the `fuzzystrmatch` module with these phonetic functions:

| Function | Signature | Notes |
|----------|-----------|-------|
| `soundex(text)` | → `text` | Returns 4-char Soundex code |
| `difference(text, text)` | → `int` | 0-4 score of Soundex similarity |
| `metaphone(text, int)` | → `text` | Metaphone code, max length param |
| `dmetaphone(text)` | → `text` | Double Metaphone primary code |
| `dmetaphone_alt(text)` | → `text` | Double Metaphone alternate code |
| `daitch_mokotoff(text)` | → `text[]` | Array of D-M codes (multi-code!) |

**Important limitation:** `soundex`, `metaphone`, `dmetaphone`, `dmetaphone_alt` do NOT work well with multibyte encodings (UTF-8). Only `daitch_mokotoff` and `levenshtein` are UTF-8 safe.

**Indexing with Daitch-Mokotoff:**
```sql
-- Create GIN index on D-M codes
CREATE INDEX ix_names_dm ON names USING gin (daitch_mokotoff(name)) WITH (fastupdate = off);

-- Query: find phonetic matches
SELECT * FROM names WHERE daitch_mokotoff(name) && daitch_mokotoff('Swartzenegger');
```

The `&&` operator checks array overlap — if any code from the stored name matches any code from the query term, it is a match. The GIN index makes this fast.

**Full-text search integration:**
PostgreSQL allows wrapping D-M codes in `tsvector`/`tsquery` for integration with full-text search indexes, enabling combined phonetic + text search.

### Elasticsearch — Phonetic Analysis Plugin

Elasticsearch provides a phonetic token filter plugin supporting 12 algorithms:

**Supported encoders:** `metaphone` (default), `double_metaphone`, `soundex`, `refined_soundex`, `caverphone1`, `caverphone2`, `cologne`, `nysiis`, `koelnerphonetik`, `haasephonetik`, `beider_morse`, `daitch_mokotoff`

**Key configuration:**
```json
{
  "filter": {
    "my_phonetic": {
      "type": "phonetic",
      "encoder": "double_metaphone",
      "replace": false,
      "max_code_len": 4
    }
  }
}
```

- `replace: true` (default) — replaces original token with phonetic code
- `replace: false` — keeps original token AND adds phonetic code at same position (stacked tokens)
- `max_code_len` — max length of emitted code (Double Metaphone specific)

**Beider-Morse specific settings:**
- `rule_type`: `exact` or `approx`
- `name_type`: `ashkenazi`, `sephardic`, or `generic`
- `languageset`: array of language codes, or auto-detect

**Best practice (from Elastic docs):** Use separate fields for phonetic and non-phonetic analysis. This avoids complications with stacked tokens in fuzzy queries and enables flexible boosting.

### Apache Lucene / Solr — PhoneticFilter

Lucene's `PhoneticFilter` (in `analyzers-phonetic` module) is the foundation for both Solr and Elasticsearch phonetic support:

- Uses Apache Commons Codec for encoding
- `PhoneticFilterFactory` configures the encoder and `inject` parameter
- `inject=true`: Adds phonetic token as synonym at same position
- `inject=false`: Replaces original with phonetic code
- Supported encoders: DoubleMetaphone, Metaphone, Soundex, RefinedSoundex, Caverphone2, ColognePhonetic, Nysiis, DaitchMokotoffSoundex, BeiderMorseEncoder

---

## 11. Combining Phonetic with Other Indexes

### Multi-Strategy Fuzzy Matching

No single algorithm catches all types of name/text variation. Production systems typically combine:

1. **Phonetic index** — catches sound-alike misspellings ("Schmidt" → "Smith")
2. **Trigram (n-gram) index** — catches typos and character transpositions ("Smtih" → "Smith")
3. **Edit distance (Levenshtein)** — catches single-character insertions/deletions/substitutions
4. **Exact match** — for when the user knows the precise spelling

### Architecture Pattern: Multi-Index Lookup with Score Fusion

```
Query: "Shmidt"
  ├── Exact match:    → (no results)
  ├── Phonetic (DM):  → {Schmidt, Smith, Schmit} (phonetic code match)
  ├── Trigram:         → {Schmidt, Shmid, Schmid} (character overlap)
  └── Levenshtein:     → {Schmidt} (edit distance 1)

Score fusion: Schmidt appears in 3/3 strategies → highest confidence
```

### PostgreSQL Example: Combined Indexes
```sql
-- Phonetic index (Daitch-Mokotoff via GIN)
CREATE INDEX ix_name_dm ON people USING gin (daitch_mokotoff(name));

-- Trigram index (pg_trgm via GIN)
CREATE INDEX ix_name_trgm ON people USING gin (name gin_trgm_ops);

-- Combined query
SELECT name,
       daitch_mokotoff(name) && daitch_mokotoff('Shmidt') AS phonetic_match,
       similarity(name, 'Shmidt') AS trigram_score
FROM people
WHERE daitch_mokotoff(name) && daitch_mokotoff('Shmidt')
   OR similarity(name, 'Shmidt') > 0.3
ORDER BY trigram_score DESC;
```

---

## 12. Implementation Considerations for AeorDB

### How Phonetic Codes Map to NVT + ScalarConverter

The existing AeorDB architecture provides a natural fit for phonetic indexing:

```
Input value (e.g., "Schmidt")
    ↓
PhoneticScalarConverter.to_scalar(value_bytes)
    ↓
  1. Decode bytes to UTF-8 string
  2. Compute phonetic code (e.g., Double Metaphone → "XMT")
  3. Hash the phonetic code → u64 (using xxhash, siphash, etc.)
  4. Normalize: hash as f64 / u64::MAX as f64 → scalar in [0.0, 1.0]
    ↓
NVT bucket lookup (O(1))
    ↓
All entries in that bucket share the same phonetic code
```

This is essentially the same pattern as `HashConverter` — phonetic codes are NOT order-preserving, so `is_order_preserving()` returns `false`. Range queries make no sense on phonetic codes; only equality matching (same bucket = same phonetic code).

### Proposed Converter Design

```rust
pub const CONVERTER_TYPE_PHONETIC: u8 = 0x0B;  // next available tag

pub enum PhoneticAlgorithm {
    Soundex           = 0,
    Metaphone         = 1,
    DoubleMetaphone   = 2,  // primary code
    DoubleMetaphoneAlt = 3, // alternate code
    Nysiis            = 4,
    Caverphone2       = 5,
    ColognePhonetics  = 6,
    DaitchMokotoff    = 7,
    // BeiderMorse omitted initially — too complex for v1
}

pub struct PhoneticConverter {
    algorithm: PhoneticAlgorithm,
}

impl ScalarConverter for PhoneticConverter {
    fn to_scalar(&self, value: &[u8]) -> f64 {
        let text = std::str::from_utf8(value).unwrap_or("");
        let code = match self.algorithm {
            PhoneticAlgorithm::Soundex => soundex(text),
            PhoneticAlgorithm::DoubleMetaphone => double_metaphone_primary(text),
            PhoneticAlgorithm::DoubleMetaphoneAlt => double_metaphone_alt(text),
            // ... etc
        };
        // Hash the phonetic code to a scalar
        let hash = hash_bytes(code.as_bytes());
        hash as f64 / u64::MAX as f64
    }

    fn is_order_preserving(&self) -> bool { false }
    fn name(&self) -> &str { "phonetic" }

    fn serialize(&self) -> Vec<u8> {
        vec![CONVERTER_TYPE_PHONETIC, self.algorithm as u8]
    }

    fn type_tag(&self) -> u8 { CONVERTER_TYPE_PHONETIC }
}
```

### Double Metaphone: Two Indexes, One Field

For Double Metaphone, the primary and alternate codes should be stored as **two separate FieldIndex entries** on the same field:

```
Field: "last_name"
  ├── FieldIndex (converter: PhoneticConverter { DoubleMetaphone })      ← primary
  └── FieldIndex (converter: PhoneticConverter { DoubleMetaphoneAlt })   ← alternate
```

A phonetic query on `last_name` would check both indexes and union the results. This mirrors how Elasticsearch uses `replace: false` to stack tokens.

### Daitch-Mokotoff: Multiple Codes Per Value

D-M can produce up to 32 codes per name. Two approaches:

**Option A — Multiple index entries per document:**
When inserting "Bierschbach", compute all 8 D-M codes, and insert 8 entries into the FieldIndex, all pointing to the same file hash. At query time, compute all D-M codes for the query term and check all of them.

**Option B — Store only the first code:**
Simpler but loses the multi-pronunciation benefit. Not recommended.

Option A is correct. The `FieldIndex::insert` method already supports multiple insertions per document — just call it once per code.

### Handling Updates

When a document is updated:
1. Remove all old index entries for that file hash (existing `FieldIndex::remove` handles this)
2. Recompute phonetic code(s) for the new value
3. Insert new entries

This is no different from how existing indexes handle updates. The NVT dirty flag + rebuild mechanism handles bucket redistribution.

### Storage Costs

| Algorithm | Codes Per Name | Code Size | Index Entries Per Doc |
|-----------|---------------|-----------|----------------------|
| Soundex | 1 | 4 bytes | 1 |
| Metaphone | 1 | 2-8 bytes | 1 |
| Double Metaphone | 2 | 2-4 bytes each | 2 |
| NYSIIS | 1 | 6 bytes | 1 |
| Caverphone 2.0 | 1 | 10 bytes | 1 |
| Cologne | 1 | Variable | 1 |
| Daitch-Mokotoff | 1-32 | 6 digits each | 1-32 |
| Beider-Morse | Many | Variable | Many |

The phonetic code itself is never stored directly — it is hashed to a scalar and only the scalar + file_hash are stored in the FieldIndex entries (same as every other index type). So the actual per-entry storage cost is `f64 (8 bytes) + file_hash (32 bytes) = 40 bytes` regardless of algorithm.

The key cost difference is the NUMBER of entries: Double Metaphone doubles entries per document, and Daitch-Mokotoff can multiply them by up to 32x.

### Multi-Algorithm Support

A user should be able to create multiple phonetic indexes on the same field with different algorithms:

```json
{
  "indexes": {
    "last_name_soundex": { "field": "last_name", "type": "phonetic", "algorithm": "soundex" },
    "last_name_dmetaphone": { "field": "last_name", "type": "phonetic", "algorithm": "double_metaphone" },
    "last_name_trigram": { "field": "last_name", "type": "trigram" }
  }
}
```

The query layer would allow specifying which index to use, or could auto-select based on the query type.

### False Positive Rates and Bucket Sizing

Since phonetic codes produce discrete values (not a continuous distribution), the NVT bucket count matters:
- Too few buckets → multiple different phonetic codes land in the same bucket → false positives from the NVT layer
- Too many buckets → wasted memory, sparse buckets

The phonetic code space is relatively small:
- Soundex: 26 * 7^3 = ~8,918 possible codes
- Double Metaphone: roughly ~10,000-50,000 distinct codes in practice
- Daitch-Mokotoff: 10^6 = 1,000,000 possible codes (6 digits, each 0-9)

**Recommendation:** For phonetic indexes, use a bucket count that matches the cardinality of the code space:
- Soundex: 8,192 buckets (8K)
- Metaphone / Double Metaphone: 16,384 buckets (16K)
- Daitch-Mokotoff: 65,536 buckets (64K) or even higher
- The HashConverter's uniform distribution property means phonetic code hashes will spread evenly across buckets

---

## 13. Edge Cases

### Non-Latin Scripts
- Most algorithms (Soundex, Metaphone, NYSIIS, Caverphone, Cologne) are **strictly Latin-alphabet-only**
- They will produce garbage or empty output for Chinese, Arabic, Cyrillic, etc.
- **Beider-Morse** handles Hebrew and Cyrillic Russian (to Latin transliteration)
- **Recommendation for AeorDB:** If the input contains non-Latin characters, either:
  - Skip phonetic indexing for that value (return a sentinel scalar like 0.0)
  - Transliterate to Latin first (using a library like `unidecode`)
  - Use a language-specific algorithm (e.g., Pinyin for Chinese)

### Mixed-Language Text
- A field containing "Jean-Pierre Muller" has French first name, German surname
- No single algorithm handles both well
- **Approach:** Tokenize on whitespace/hyphens, index each token separately with per-token phonetic codes
- Or use Beider-Morse with `generic` name type for best cross-language coverage

### Name Prefixes
| Prefix | Origin | Challenge |
|--------|--------|-----------|
| Mc, Mac | Irish/Scottish | Some algorithms (NYSIIS) handle MAC→MCC. Others treat "Mc" as literal consonants. |
| O' | Irish | Apostrophe is stripped, then "O" becomes the first letter. "O'Brien" and "Brien" produce different Soundex codes. |
| von, van | German/Dutch | Usually treated as separate tokens. "von Braun" — phonetic code of "von" is useless; index "Braun" separately. |
| de, de la, del | French/Spanish | Same issue as von/van. Strip these prefixes before phonetic encoding, or index both with and without. |
| St., Saint | English | "St. John" vs "Saint John" — normalize before encoding. |
| bin, bint, ibn | Arabic | Patronymic particles. Should be stripped or treated as separate tokens. |

**Recommendation for AeorDB:** Provide a configurable prefix-stripping list per phonetic index. The converter could accept an optional `strip_prefixes: Vec<String>` configuration.

### Other Edge Cases
- **Empty strings:** Return a sentinel scalar (0.0 or 0.5)
- **Numeric strings:** Most phonetic algorithms produce empty/undefined output for "12345". Return sentinel.
- **Very short strings:** Single-character names (e.g., "X") produce minimal codes. This is fine — they are legitimate (Chinese surnames like "Li" → very short codes).
- **Very long strings:** Most algorithms have natural truncation (Soundex=4, NYSIIS=6, Caverphone=10). For unbounded algorithms (Metaphone, Cologne), consider truncating the phonetic code before hashing to keep hash distribution uniform.
- **Accented characters:** "Muller" vs "Muller" (with umlaut). Strip diacritics before phonetic encoding. The `unicode-normalization` crate + ASCII folding handles this.
- **Case sensitivity:** All algorithms operate case-insensitively. Normalize to uppercase or lowercase as first step.

---

## 14. Recommended Implementation Roadmap for AeorDB

### Phase 1 — Foundation
1. Implement `PhoneticConverter` as a new `ScalarConverter` variant
2. Start with **Soundex** (simplest, well-understood) and **Double Metaphone** (best general-purpose)
3. Add `CONVERTER_TYPE_PHONETIC` to the deserializer
4. Wire into the existing index configuration system

### Phase 2 — Multi-Algorithm
5. Add NYSIIS, Cologne Phonetics, Daitch-Mokotoff
6. Handle D-M's multi-code output (multiple inserts per document)
7. Add prefix stripping configuration

### Phase 3 — Advanced
8. Add Beider-Morse (complex but highest quality)
9. Add trigram index support as a separate `ScalarConverter` variant (for combined strategies)
10. Add query-layer support for multi-index fusion (phonetic + trigram + exact)

### Rust Crates to Evaluate
- `rphonetic` — Rust implementations of Soundex, Metaphone, NYSIIS, Caverphone, etc.
- `phonetics` — Another Rust phonetics crate
- If no suitable crate exists, Soundex and Metaphone are simple enough to implement from scratch (< 100 lines each)

---

## Sources

- [Soundex - Wikipedia](https://en.wikipedia.org/wiki/Soundex)
- [Metaphone - Wikipedia](https://en.wikipedia.org/wiki/Metaphone)
- [NYSIIS - Wikipedia](https://en.wikipedia.org/wiki/New_York_State_Identification_and_Intelligence_System)
- [Caverphone - Wikipedia](https://en.wikipedia.org/wiki/Caverphone)
- [Cologne Phonetics - Wikipedia](https://en.wikipedia.org/wiki/Cologne_phonetics)
- [Daitch-Mokotoff Soundex - Wikipedia](https://en.wikipedia.org/wiki/Daitch%E2%80%93Mokotoff_Soundex)
- [Beider-Morse Phonetic Matching](https://stevemorse.org/phoneticinfo.htm)
- [PostgreSQL fuzzystrmatch Docs](https://www.postgresql.org/docs/current/fuzzystrmatch.html)
- [Elasticsearch Phonetic Analysis Plugin](https://www.elastic.co/docs/reference/elasticsearch/plugins/analysis-phonetic)
- [Elasticsearch Phonetic Token Filter](https://www.elastic.co/docs/reference/elasticsearch/plugins/analysis-phonetic-token-filter)
- [Apache Solr Phonetic Matching](https://solr.apache.org/guide/solr/latest/indexing-guide/phonetic-matching.html)
- [Stanford IR Book — Phonetic Correction](https://nlp.stanford.edu/IR-book/html/htmledition/phonetic-correction-1.html)
- [Apache Commons Codec — ColognePhonetic](https://commons.apache.org/codec/apidocs/org/apache/commons/codec/language/ColognePhonetic.html)
- [Phonetic Algorithms — Splink](https://moj-analytical-services.github.io/splink/topic_guides/comparisons/phonetic.html)
- [Phonetic Matching Algorithms — Medium](https://medium.com/@ievgenii.shulitskyi/phonetic-matching-algorithms-50165e684526)
- [Double Metaphone — Datablist](https://www.datablist.com/learn/data-cleaning/double-metaphone)
- [ElasticSearch Phonetic Algorithms — Medium](https://medium.com/@ranallo/elasticsearch-phonetic-algorithms-7862e76a1d1e)
- [PostgreSQL pg_trgm Docs](https://www.postgresql.org/docs/current/pgtrgm.html)
