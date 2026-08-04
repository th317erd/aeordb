# Formalization Review

**Campaign:** [AeorDB V4, NVT, GC, and Migration](../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Review date:** 2026-08-03
**Baseline:** `5d3e284652f9fec7a5c843f1946132574af4d469`
**Result:** PASS; formal plan set is ready for separate implementation authorization

## Scope

This review converted the ratified Rounds 10 through 15 into one parent, eight
child worker briefs, eight progress ledgers, and supersession/incorporation
banners. It did not implement production code, create v4 bytes, run migration,
or touch a database.

## Baseline Evidence

- Local branch: `development`.
- `origin/development` matched the baseline after fetch.
- Axum `.route(` registrations: 93.
- Rust `*_spec.rs` files: 161.
- Ten named engine/server hotspots: 18,846 lines.
- Existing July 16 plan: 1,894 lines and superseded in full.

The implementation P0a inventory remains mandatory because these counts may
change after formalization.

## Generated Artifacts

- parent campaign plan;
- Children 01 through 08;
- progress ledgers 01 through 08;
- this formalization review; and
- full/partial supersession banners on incorporated historical plans.

The ratified `.codex/conversation.md` is an explicit tracked prerequisite before
implementation P0 begins.

## Mechanical Checks

| Check | Result |
| --- | --- |
| `git diff --check` | PASS |
| Campaign Markdown fence parity | PASS |
| Campaign relative Markdown links | PASS |
| Modified YAML parse after banner comments | PASS |
| All eight children contain Outcome, Owned Territory, Landing Sequence, Verification, Rollback, and DoD | PASS |
| Required existing test target names found in Cargo manifests | PASS |
| Planned target names classified explicitly as future red/green obligations | PASS |
| Superseded-assumption scan | PASS, zero matches |
| Unresolved-marker scan | PASS, zero matches |

The superseded-assumption scan covered obsolete header/data offsets, encoded
root tokens, removed lifetime settings, process-local coverage identity, old
APOS transition wording, the incorrect SweepProposal fixed term, and a hard-
batched derived mutation journal.

## AGIS Adversarial Findings Corrected

1. **P3a ownership conflict:** the first generated dependency graph assigned
   P3a to Child 03 even though v4 format writers belong to Child 01. The graph
   now names Child 01's format-writer slice and Child 03's root/semantic slice.
2. **Decision-source durability:** formal plans summarized exact schemas but did
   not initially require the ratified decision source to be tracked. The parent
   now makes that a pre-P0 start gate.
3. **Execution-state ownership:** child plans existed without concrete progress
   paths. Eight ledgers now record owner, start gate, last-green commit, hotspot
   handoff, tests, drift, evidence, and next action.
4. **Lifecycle default precision:** the first draft omitted the exact 24-hour
   grace default, fixed exactly-two-mark invariant, safety-asymmetric existing-
   candidate formula, and GC scratch reserve expression. They are now frozen in
   the parent and owning children.
5. **Uniform child executability:** several children used equivalent but
   inconsistent section names. Every child now has the same six required
   execution sections and explicit rollback.

## Dependency and Ownership Review

The final graph has no dependency cycle:

```text
08:P0a -> 01 -> 02 -> 03 -> 07:P3c
                         -> 04
                         -> 05 -> 06
                    04 + 06 -> 07:P8 -> 08:P9
```

Shared hotspots and registries have one integration owner and handoff commits.
ControlStore responsibilities are intentionally layered: Child 01 owns bytes,
Child 02 owns durability/config/latch semantics, and Child 03 owns protected
namespace publication. No child independently owns all three.

## Contract Coverage Review

The plan set explicitly preserves:

- 1,024-byte v4 header slots, byte 2,048 data start, and physical identity;
- protected ControlStore representation and all live SystemFamily corrections;
- post-authority recoverable-soft mutation and asynchronous indexing;
- exact root-hash selection, auth intersection, APOS, route matrix, and hash
  retrieval restrictions;
- definition-owned exact comparison, immutable pages, and sparse disposable NVT;
- logical retirement before physical reclaim, bounded marks, receipts, immutable
  Void claims, and conservative corruption behavior;
- 6/8 GiB memory envelope, strict config precedence, and full diagnostics;
- bounded capture/final reconciliation, source-GC suspension, copied-production
  rehearsal, and first-v4-write rollback boundary; and
- native Linux/macOS/Windows, real `/tmp`, crash/soak, docs, debt, and evidence
  gates.

No new owner policy decision was introduced during formalization.

## Commands Not Run

No Cargo suite or release build was run because this was a planning-only change.
The new P0-P9 target names do not exist yet by design. Each child requires its
first implementation commit to create the target in a known-red state before
production behavior is changed.

## Verdict

The formal artifact set satisfies the planning-cap executability test: a new
worker can identify P0 inputs, exact authority, owned and forbidden territory,
start/exit gates, narrow commands, rollback, evidence, and next action without
reconstructing the campaign from chat. Implementation remains gated on a
separate user request and a refreshed P0a baseline.
