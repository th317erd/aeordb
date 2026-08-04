# Child 06: Async Coverage, Query, Pagination, and Locators

**Parent:** [AeorDB V4, NVT, GC, and Migration Campaign](../../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Landing units:** P6 and P7
**Status:** P6 starts after Children 03 and 05; P7 also requires Child 04 root-state APIs
**Primary owners:** index-runtime owner, then query/API owner
**Compatibility:** coordinated APOS/API cutover; no legacy position decoder

## 1. Outcome

Turn immutable index artifacts into a bounded asynchronous accelerator, then
cut every namespace read over to one selected immutable root, exact query
planning, logical APOS pagination, opt-in search-hit ranges, authorized full-path
SSE events, and exact range/snippet fetch.

User mutations return after durable user authority, not parser/index/NVT work.
Queries remain exact when journals, caches, NVT, or compatible indexes are
missing by using a proven covered-plus-authoritative fallback or a typed
unavailable result.

## 2. Owned Territory

- post-authority soft mutation stream and gap detection;
- bounded index workers, memtables, spill, publication, compaction, cache;
- coverage registry, exact/partial/degraded/fallback planning;
- query/search/filter/sort/aggregate/projection/EXPLAIN execution;
- directory listing and route-specific total order;
- APOS codecs/validation and pagination schemas;
- root-aware file/list/query/search/fetch/download/plugin/share routes;
- on-demand match locations and range/snippet fetch;
- authorized SSE path projection and client/UI adapters; and
- API/SDK/UI test fixtures and draft documentation.

Forbidden without handoff:

- changing namespace/root authority or root lifecycle state;
- changing index/format bytes;
- synchronous parser/index work in user commit;
- physical page/offset/NVT state in public tokens; and
- leaking hidden root/path/cardinality through planner shortcuts.

## 3. P6: Recoverable Soft Mutation Stream

### Authority Boundary

1. Child 03 hard-commits publication sequence, namespace/semantic/control
   authority, and required root-admission evidence.
2. After that success, append one typed soft mutation and fan out cache, SSE,
   index, and diagnostic work.
3. Loss/gap is detected by exact source NamespaceRoot/control identities and
   publication watermarks.
4. Reconstruct with bounded immutable root/SystemFamily diff; rebuild when
   bounded diff is unavailable.
5. Never infer an empty delta from absent soft state.

Persisted coverage uses durable nonzero `coverage_epoch_id` and destination-
local `coverage_publication_sequence`, not a process boot identity. Exact source
NamespaceRoot plus exact definition/dependency identity is correctness authority.

## 4. Bounded Index Runtime

- One `IndexCoordinator` owns task admission, parser/mapper/converter work,
  memtable, spill, page publication, coverage transition, compaction, and
  degraded state for every index kind.
- All kinds use one buffered path with default flush at 262,144 mutations,
  30 seconds, or memory pressure, plus graceful final flush/checkpoint.
- Active/frozen memtables reserve bytes. Dirty state flushes/spills before
  eviction; clean pages/values/NVT evict through Child 02.
- Startup loads registry/control roots only. Pages, values, postings, and tiles
  are lazy and byte-bounded.
- Task checkpoints root unpublished attachments before old state retires.
- Definition/dependency changes build a new shadow generation.
- Whole-HEAD transitions use bounded Merkle reconciliation rather than an
  unbounded mutation batch.
- Parser deterministic failures create frozen unindexable state. Operational
  failures degrade/retry/fallback; they do not become negative matches.

## 5. Coverage Contracts

A planner may use:

1. **Complete compatible generation:** exact source root and definition/
   dependency identities match.
2. **Partial exact generation:** precise covered set plus authoritative scan of
   the complement, with dedupe/recheck proving total result.
3. **Degraded generation:** only where the same complete proof is possible;
   otherwise ignore it.
4. **No compatible generation:** bounded authoritative evaluation or typed
   `HISTORICAL_VIEW_UNAVAILABLE` when exact historical semantics are absent.

No count, aggregate, pagination, score, or EXPLAIN may label partial data
complete. Coverage state is published asynchronously and does not enter the
user-write hard waiter.

## 6. Commit-Latency Contract

Trace ordinary PUT, streamed finalize, merge, raw embedded batch, sync apply,
plugin write, and existing-chunk blob commit. Before acknowledgement they may
perform only user-authority work and required metadata validation. They perform
no parser invocation, content indexing, posting/NVT mutation, derived manifest
publication, or soft-journal sync.

Existing-chunk blob commit reads zero file-content bytes. Its work scales with
the manifest/chunk metadata and required durability barriers, not total index
size or loaded-index count.

## 7. P7: Root-Aware Query Planning

Every namespace operation receives one `ResolvedReadView` from Child 03.
Omitted selector captures HEAD once. Supplied `root_hash` never falls through to
HEAD. Current authorization and concealment apply before root state or data.

The planner:

1. partitions by effective index scope;
2. compiles literals through exact selected-root definitions;
3. derives conservative coordinate points/ranges;
4. estimates page/posting/cardinality/coverage/fallback work;
5. selects a driver by measured cost, not expression order or coordinate width;
6. scans from validated sparse hints/directories;
7. intersects scope-local document ordinals and merges scopes by FileKey;
8. exact-rechecks canonical source values and current selected-root revision;
9. applies authorization/path scope before every observable; and
10. streams/bounds sort, top-K, aggregate, projection, and cancellation.

NVT and physical spans are navigation hints only. EXPLAIN is authorization-
filtered and cannot expose hidden counts, paths, values, or physical layout.

## 8. Root Response Contract

Every successful namespace read identifies the exact root:

```json
{
  "root": {
    "hash": "<full lowercase hex hash>",
    "state": "live",
    "expires_at": null
  },
  "items": []
}
```

Compatibility-sensitive raw/keyed shapes carry equivalent headers:

```http
X-AeorDB-Root-Hash: <hash>
X-AeorDB-Root-State: live
X-AeorDB-Root-Expires-At:
```

For pending roots, `expires_at` is an advisory earliest reclamation boundary.
Reads do not refresh it. Retired roots return `410 ROOT_EXPIRED` even while
physical bytes remain. Request pins protect admitted reads until completion.
The pending boundary is computed with checked arithmetic from
`pending_since + max(grace_at_pending, current configured grace)`; a configured
increase may move the advisory time later, while a decrease cannot move an
existing candidate earlier.

## 9. Route Matrix

The architecture fixture classifies all route registrations and embedded
equivalents:

| Class | Behavior |
| --- | --- |
| Single-root namespace | file/list/query/search/fetch/download/symlink/file-reading plugin/authorized share reads use one `ResolvedReadView` and return root metadata |
| Historical aliases | `root_hash`, snapshot, and version form a mutually exclusive selector union |
| Multi-root | diff/history/comparison use named roots and return a root set/per-result roots |
| Content staging | blob config/check/chunk transport has no namespace selector and grants no reachability |
| Hash retrieval | FileRecord-by-hash requires selected-root reachability; raw Chunk/Directory/internal entries are admin/root diagnostics only |
| Operational/system | no root selector unless the typed operation explicitly names source roots |
| Mutation | generic `root_hash` rejected; route-specific historical source creates a new current root |

POST selectors live in JSON and GET selectors in query parameters. A new route
fails CI until it declares class, selector schema, authorization owner,
response-root shape, and read-view/no-root proof.

## 10. APOS and Pagination

APOS is canonical unpadded base64url of the frozen `APOS` v1 binary record. It
contains route kind, root hash, order fingerprint, canonical sort tuple,
FileKey, RecordRevision, and CRC. It contains no expiry, page, offset, limit,
WAL, NVT, manifest, or physical state.

Legal origins are exactly one of:

- `page` (one-based);
- `offset` (zero-based);
- `after`; or
- `before`.

Each may combine with bounded `limit`; origins cannot combine with one another.
`after`/`before` requires explicit `root_hash`, and APOS root/order must match.
The server resolves the logical position in the authorized selected-root result
universe and recomputes the complete tuple. Aggregate APOS validates its
canonical group tuple and synthetic identities rather than resolving a fake
file.

`before` returns the closest prior page in requested order. Deep page/offset
uses rank metadata or bounded scans, never all-result materialization.

Stable errors include `INVALID_ROOT_HASH`, `INVALID_ROOT_SELECTOR`,
`INVALID_PAGINATION`, `INVALID_POSITION_CURSOR`,
`POSITION_ROOT_MISMATCH`, `POSITION_ORDER_MISMATCH`, `ROOT_EXPIRED`,
`INVALID_NAMESPACE_ROOT`, `HISTORICAL_VIEW_UNAVAILABLE`, and
`DATABASE_CORRUPTION`, subject to route concealment.

## 11. Route Total Orders

Directory listing always orders directories before non-directories, independent
of ascending/descending. Within category it applies requested primary direction,
canonical folded/raw name/path ties, FileKey, and revision. The folder/file
category boundary may be crossed once across pages without omission/duplicate.

Query defaults to canonical path ascending. Present values precede typed null,
which precedes missing; only present values reverse. Multi-value selection uses
semantic minimum/maximum. Canonical path, FileKey, and revision complete the
order.

Fuzzy search defaults score descending then path/FileKey/revision. Aggregate
groups use requested order or count descending then canonical group tuple.

## 12. Bot-Oriented Match Locators and Range Fetch

Search/query clients opt into positions. The engine does not persist positional
indexes. For each authorized hit:

1. pin the selected root and exact RecordRevision/content hash;
2. scan source bytes under configured per-hit/total limits;
3. return matching byte range plus codepoint/character and line metadata where
   valid for the content type;
4. preserve CRLF as one logical newline while retaining exact byte boundaries;
5. identify truncation/continuation and matching semantics; and
6. let the client fetch only requested byte, line, character, or plugin-specific
   range from the same root/revision.

`if_content_hash` and `if_updated_at` are optional caller assertions, not a
replacement for root selection. JSON/JQ-style extraction remains a plugin
invocation; generic text extraction uses the native bounded plugin host read
primitive and default extract plugin.

The real-world acceptance flow is:

```text
search root X -> receive ranges -> fetch selected snippets from root X -> exact bytes
```

## 13. SSE and Client/UI Integration

- Post-authority mutation events include authorized full paths, operation kind,
  root/publication identities, and affected relationship metadata.
- Per-connection filtering applies current permission/path scope before event
  projection. Hidden paths do not leak through counts, event type, timing, or
  relationship IDs.
- Clients can update active directory listings without full-page dim/reload and
  without reloading unrelated previews.
- Server, embedded SDK, bundled client, portal, APOS, and root schemas cut over
  together. Old in-flight APOS may fail; no dual decoder is added.

## 14. Landing Sequence

1. P6-1 soft mutation stream, sequence/gap model, and authoritative diff.
2. P6-2 bounded coordinator/memtable/spill/task/cache runtime.
3. P6-3 immutable page publication, compaction, coverage/fallback activation.
4. P6-4 producer commit traces and latency/resource proof.
5. P7-1 route matrix, `ResolvedReadView` adapters, root response/error contract.
6. P7-2 APOS decoder/validator and route total-order scans.
7. P7-3 query/search/sort/aggregate/EXPLAIN planner/executor conversion.
8. P7-4 locators/range continuity and plugin read integration.
9. P7-5 SSE, embedded SDK, portal/client, and docs schema cutover.

Shared hotspots are serialized and handed off at a recorded commit.

## 15. Verification

Required campaign targets:

```bash
timeout 15m cargo test -j 6 -p aeordb --test coverage_runtime_spec
timeout 15m cargo test -j 6 -p aeordb --test root_api_reference_spec
```

Existing inputs include:

```bash
timeout 10m cargo test -j 6 -p aeordb --test query_engine_spec
timeout 10m cargo test -j 6 -p aeordb --test query_pagination_spec
timeout 10m cargo test -j 6 -p aeordb --test aggregation_spec
timeout 10m cargo test -j 6 -p aeordb --test explain_engine_spec
timeout 10m cargo test -j 6 -p aeordb --test directory_listing_spec
timeout 10m cargo test -j 6 -p aeordb --test directory_listing_http_spec
timeout 10m cargo test -j 6 -p aeordb --test global_search_http_spec
timeout 10m cargo test -j 6 -p aeordb --test query_http_spec
timeout 10m cargo test -j 6 -p aeordb --test upload_commit_spec
timeout 10m cargo test -j 6 -p aeordb --test sse_spec
timeout 10m cargo test -j 6 -p aeordb --test download_spec
timeout 10m cargo test -j 6 -p aeordb --test wasm_query_e2e_spec
```

Independent model tests cover every predicate/boolean/scope/order/pagination/
aggregate combination, current/former/pending/retired/corrupt roots, forged
APOS/hash, concurrent HEAD changes, authorization, partial coverage, missing
journal/index/NVT, cache eviction, and route response shape.

## 16. Real-World and Performance Proof

- Run a real authenticated database under `/tmp/codex` with hundreds of mixed
  folders/files and JSON/text/binary/media content.
- Concurrently mutate while paging both directions; verify no folder/file
  reorder, omission, duplicate, or mixed root.
- Search text, receive byte/codepoint/line ranges, and fetch only exact snippets
  from the same root, including CRLF fixtures.
- Drop/corrupt NVT, cache, journal, and selected derived pages; compare reference
  results/fallback errors.
- Stress many configured indexes and prove commit trace excludes derived work.
- Measure existing-chunk blob commit, point/range query, deep page, aggregate,
  SSE refresh, health latency, page reads, and resident bytes.
- Reopen/verify after seeded kills and client disconnect/cancellation.

## 17. Rollback

Deactivate v1 index registry pointers and use authoritative v4 evaluation.
Disable new APOS/locator feature advertisement only with the coordinated client
release; do not re-enable an old incompatible decoder. Namespace roots/user data
remain valid. A compatible v4 reader is required after capability activation.

## 18. Definition of Done

- [ ] User writes acknowledge before all derived parser/index work.
- [ ] Soft journal gaps reconstruct or rebuild and cannot falsify coverage.
- [ ] Index workers/cache/memtables remain within shared budgets.
- [ ] Complete/partial/degraded/fallback plans are mechanically exact.
- [ ] Every namespace route belongs to the architecture matrix.
- [ ] Every successful read returns exact root metadata.
- [ ] Historical auth precedes every observable.
- [ ] APOS contains only logical root/order/position and exact validation.
- [ ] Directories-first order survives both directions and every page boundary.
- [ ] Hash knowledge alone grants no namespace read authority.
- [ ] Bot search-to-range-fetch reproduces exact bytes with CRLF correctness.
- [ ] Authorized SSE includes full paths without leakage.
- [ ] Real `/tmp` and performance evidence meet the parent gates.
