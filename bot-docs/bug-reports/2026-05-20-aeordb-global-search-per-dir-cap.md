# Bug Report — `global_search` silently caps results at 20 per directory

**Filed by:** xenocept-client team
**Target:** aeordb (`aeordb-lib/src/engine/search.rs` + `query_engine.rs`)
**Severity:** Medium — silent data loss on common queries; user-visible regression on xenocept-client's session search
**Date:** 2026-05-20

---

## TL;DR

`global_search`'s `broad_search` helper builds per-directory queries with `limit: None` and a comment that reads `// no per-directory limit; we paginate globally`. But the `QueryEngine.execute` path treats `None` as **"use `DEFAULT_QUERY_LIMIT = 20`"**, not as "unlimited" — so every per-directory query is silently capped at 20 hits before the outer global pagination ever sees them. Common terms that match hundreds of documents return only a handful of results.

**One-line fix candidate** at `aeordb-lib/src/engine/search.rs:174`:

```diff
-      limit: None, // no per-directory limit; we paginate globally
+      limit: Some(usize::MAX), // no per-directory limit; we paginate globally
```

(See "Recommended fix" below for the alternative — change `QueryEngine` to treat `None` as unlimited.)

---

## Repro on xenocept-client

xenocept-client wraps `global_search` for session search at `/api/v1/sessions/search?q=<term>`. The index config glob is `*/session.json` at `/sessions/`, with trigram indexes on `id`, `comment_text`, `bubble_text`, `text_text`, `ocr_text`, `alternative_description`.

The store has **105 sessions**. Most of them contain the literal text `"xenocept"` in their OCR or annotations (the user is screenshotting their own app).

```bash
$ curl -s 'http://127.0.0.1:9500/api/v1/sessions/search?q=xenocept&limit=500' | jq 'length'
16

$ curl -s 'http://127.0.0.1:9500/api/v1/sessions/search?q=button&limit=500' | jq 'length'
26

$ curl -s 'http://127.0.0.1:9500/api/v1/sessions/search?q=session&limit=500' | jq 'length'
34
```

Even at `limit=500`, the search caps at ~16–34 unique results regardless of how many documents in the corpus actually match. Documents that should match are completely missing — e.g. session `session-2026-05-21-03-03-06.290` is reachable by searching `button` (its OCR contains the word), but searching `xenocept` returns 0 hits for that session ID, even though "Xenocept" appears multiple times in its OCR.

The xenocept-client user reported the symptom as "search only returns sessions from 5/12 and before" — that's because the older, less common sessions had distinct enough trigram fingerprints to surface in the (very limited) per-directory result window, while newer sessions all share the same dominant trigrams and lost the relevance tie-break inside the 20-cap.

---

## Trace of the bug

### Step 1 — Outer paginator looks correct

`global_search()` in `aeordb-lib/src/engine/search.rs:60-128` does:

1. Discover indexed directories under `base_path`.
2. Call `broad_search(...)` (or `structured_search`) to fill `all_results: Vec<SearchResult>`.
3. Dedupe by path, sort by score desc.
4. Skip / take based on `offset` / `limit`.

That looks fine — the outer paginator never caps anything; it just slices what `broad_search` produced.

### Step 2 — Inner per-directory search passes `limit: None`

`broad_search()` at `aeordb-lib/src/engine/search.rs:144-200`:

```rust
for field_name in &fuzzy_fields {
  let q = Query {
    path: dir.clone(),
    field_queries: vec![],
    node: Some(QueryNode::Field(FieldQuery {
      field_name: field_name.clone(),
      operation: QueryOp::Match(query_str.to_string()),
    })),
    limit: None, // no per-directory limit; we paginate globally    // ← comment promises unlimited
    offset: None,
    ...
  };

  match query_engine.execute(&q) {
    Ok(qr_results) => {
      for qr in qr_results {
        out.push(query_result_to_search_result(qr, dir, field_name));
      }
    }
    ...
  }
}
```

The intent stated in the comment is clear: every match from every directory should land in `out`; pagination happens later in `global_search`.

### Step 3 — `QueryEngine.execute` silently substitutes `DEFAULT_QUERY_LIMIT`

`aeordb-lib/src/engine/query_engine.rs:557` and again at `:578`:

```rust
let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);  // 20
```

And `:153`:

```rust
pub const DEFAULT_QUERY_LIMIT: usize = 20;
```

So `limit: None` from `broad_search` means *exactly* "I want 20 results," not "I want unlimited." The comment in `broad_search` is a lie that aeordb-lib makes about itself.

### Step 4 — Consequence on the score-sorted output

After per-directory truncation, each directory contributes up to 20 score-sorted matches. Across the 105-session corpus that lives under one directory (`/sessions/`), `broad_search` will collect at most:

```
20  results  ×  N_fuzzy_fields
```

where `N_fuzzy_fields` is the count of trigram-capable fields on `session.json`. For xenocept-client that's 5 fields (comment_text, bubble_text, text_text, ocr_text, alternative_description) — call it 100 raw `SearchResult` records max. After `deduplicate_by_path`, paths that matched on multiple fields collapse, so you end up with ~16–34 unique sessions even for terms that should match all 105.

The "5/12 and before" symptom is a red herring — those happen to be the sessions whose tf-idf-ish scores survive the 20-cap on each field, mostly because they're temporally first (and aeordb's tie-break favors something deterministic that correlates with insertion order).

---

## Recommended fix

Two equivalent options; (A) is the minimal touch:

### Option A — Caller honors its own comment (1-line patch)

`aeordb-lib/src/engine/search.rs:174`:

```diff
-      limit: None, // no per-directory limit; we paginate globally
+      limit: Some(usize::MAX), // no per-directory limit; we paginate globally
```

The same fix probably also belongs in `structured_search` (`aeordb-lib/src/engine/search.rs:209`), which has the same pattern. Worth grepping the file for `limit: None` in any `Query` construction.

### Option B — `QueryEngine` honors `limit: None` as unlimited

Less localized but more correct semantically. In `query_engine.rs:557` and `:578`:

```diff
-    let effective_limit = query.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
+    let effective_limit = query.limit.unwrap_or(usize::MAX);
```

Then `DEFAULT_QUERY_LIMIT` becomes a callers'-default-not-engine-default convention. This changes the contract — every existing direct caller of `QueryEngine.execute` with `limit: None` would suddenly become unlimited. Audit those callers before committing this; some of them probably WANT the 20-default.

A safer hybrid: keep the engine default, but add an explicit `Query::unlimited()` constructor / `Some(0) → unlimited` sentinel that the engine recognizes. Then `broad_search` opts in explicitly. More changes, but no behavior change for incumbent callers.

---

## Tests to add

After the fix lands, the regression test should:

1. Insert 100 documents, each containing a common trigram fingerprint (e.g. a word that all 100 share + one differentiating token).
2. Trigram-search the shared word.
3. Assert that all 100 paths appear in `results` (after default-pagination), not just 20.
4. Repeat with a single-directory corpus (sanity: in-directory pagination should respect explicit `limit`) and with a multi-directory corpus (cross-directory dedup still works).

Existing tests in `search.rs` around `deduplicate_by_path` are useful but don't cover the per-directory cap path.

---

## Out of scope for this report

- Whether `DEFAULT_QUERY_LIMIT = 20` is the right default for end-user APIs (it's reasonable; the bug is `broad_search` accidentally inheriting it).
- Trigram tf-idf scoring quality — separate concern, only mentioned above as the reason behind the "5/12 and before" appearance of the symptom.
- Pagination beyond the first window when `total_count > effective_limit` on the outer global_search call.

---

## Contact

Reproduction binary + corpus: xenocept-client running with `~/.local/share/xenocept/xenocept.aeordb` (105 sessions). The xenocept-client team can hand off a frozen aeordb snapshot if useful for regression-testing.

---

## DB-team resolution (2026-05-20)

**Status:** Fixed — applied Option A as written in this report. Confirmed reproduction in an isolated test, then confirmed the fix lifts the cap.

### Fix shipped

`aeordb-lib/src/engine/search.rs`:

```diff
@@ broad_search ───────────────────────────────────────────────
-      limit: None, // no per-directory limit; we paginate globally
+      limit: Some(usize::MAX),

@@ structured_search ──────────────────────────────────────────
-      limit: None,
+      limit: Some(usize::MAX),
```

Both spots now match the comment that was already there. The QueryEngine still treats `None → DEFAULT_QUERY_LIMIT = 20` so we don't change behavior for direct callers — Option B was deferred for the reasons the report itself flagged.

### Regression test

`aeordb-lib/spec/http/global_search_http_spec.rs::test_per_directory_cap_is_lifted`:

- Single directory `/sessions`, trigram + string index on `name`.
- 50 documents whose `name` all contain `"xenocept session N"` — same trigram fingerprint pattern as the xenocept-client corpus.
- POST `/files/search` with `{"query":"xenocept","limit":500}`, assert `results.len() == 50`.

Verified the test FAILS on the pre-fix code (returns far fewer than 50) and PASSES with the fix.

### What xenocept-client should see after upgrading

- `/api/v1/sessions/search?q=xenocept&limit=500` should return all 105 matching sessions (after dedup) rather than capping at ~16.
- The temporal "5/12 and before" symptom should disappear — those older sessions were just the ones surviving the 20-cap tie-break.
- No client-side changes required; the wrapper around `global_search` is unaffected.

### Not changed (still open)

- The `DEFAULT_QUERY_LIMIT = 20` engine default is unchanged; direct `QueryEngine.execute(...)` callers passing `limit: None` still get 20. Caller-side audit deferred per the report's own caveat.
- No new explicit `Query::unlimited()` constructor — `Some(usize::MAX)` is the documented "unlimited" sentinel for now. If we ever want a more idiomatic spelling, that's a separate refactor.

— DB team
