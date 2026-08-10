# Child 08 Progress: Evidence

## Landing State

- **Status:** P0a and P2f are complete; continuous phase evidence remains active through P9.
- **Current landing unit:** P2f landed; handoff to Child 01 P1c native completion and then P3a/P3b.
- **Entry commit:** `5e0dc2a` (`fix: return metrics initialization conflicts`).
- **Last pushed green commit:** P2f exit `8f2ab42` on `development` and `origin/development`.
- **Owner:** Codex, campaign integration/evidence owner.
- **Start gate:** P2a-P2e are green and pushed; P2f must land before any P3 writer activates.
- **Plan:** [Child 08](../children/08-verification-operations-docs-and-debt.md).
- **Owned files:** this ledger; suppression scanner/inventory; architecture target; narrowly required production corrections and regression tests; affected operator documentation.
- **Forbidden files and actions honored:** no v4 writer activation, migration/cutover activation, production or evidence database mutation, deployment, `.codex/DETAILS.md`, `.codex/wip.md`, or `downloads/` changes.

## Inventory Contract

- Added the syntax-aware `aeordb-error-squelch-audit-v1` workspace tool and exact checked JSON inventory.
- The scanner covers discarded results, `Result`-to-`Option`, default-on-error, broad error patterns, success-only conditionals, status/variant probes, error conversion/recovery, logged continuation, and panic methods/macros. It scans nested macro expressions while excluding error-preserving `map_err` closures and test-only source.
- Stable identities do not depend on leading line shifts. The validator rejects unknown classifications, duplicate identities, pending reviews, stale or unused entries, missing policy metadata, and any ceiling above the exact discovered count.
- Every retained occurrence receives a named semantic policy containing class, bounded rationale, owner, guarding test, and removal condition. The reviewed ceiling may shrink but cannot grow without an explicit override.
- Final review/check found exactly **1,420** reviewed production occurrences with no baseline growth.

## Correctness Work

The audit corrected high-risk suppression classes instead of merely documenting them:

- durability admission, permits, tickets, WAL/hot-tail reads and rebuild, transaction state, spill preservation, and poisoned authority;
- namespace, permission, cancellation, reindex, repair, backup/import, GC/Void, KV page, cache, and lifecycle invariants;
- malformed FileRecord/binary/deletion data, locator/traversal omissions, future versions, wrong hash widths, and offset overflow;
- CLI prompt/checkpoint/deployment/soak evidence, startup/shutdown, logging, CORS, metrics, health, and database-lock failures;
- plugin SDK host envelopes, WASM query options, parser/registry configuration, and runtime action state machines; and
- typed SSE `stream_gap` publication so lag is observable and clients can refresh instead of silently remaining stale.

Optional cleanup and telemetry failures retain the acknowledged primary result only where the error is bounded and visible through warning, event, metric, or debt evidence.

## Regressions Found During Verification

- Forced reindex originally treated task/system JSON records sharing the legacy `KV_TYPE_FILE_RECORD` bucket as malformed FileRecords. Current-file discovery now starts from the strict live namespace, supplements it only with canonical decodable FileRecord keys, and still fails closed on malformed live records. All 34 reindex specs pass.
- Embedded routers initialized `AppState.db_path` to an empty string and relied on the CLI to inject a path extension. `AppState` now obtains the real engine database path, while an explicit extension remains a supported override. All 31 portal specs pass, including the deliberate missing-path failure.
- A v3 architecture fixture depended on a local variable being mutable even though the contract does not. The fixture boundary now recognizes the correct immutable spelling without weakening its behavior assertions. All 66 v3 facade specs pass.

## Verification Evidence

- Scanner: `cargo run -j4 -p aeordb-error-squelch-audit -- review` and `check` both pass at 1,420 exact entries.
- Architecture: `cargo test -j4 -p aeordb --test error_squelch_architecture_spec -- --test-threads=4` passes all 27 tests.
- Storage matrix: 238 tests pass across backup, cache, directory operations, emergency spill, GC, KV snapshot, lifecycle, and reindex targets.
- Boundary matrix: logging 27, SSE 48, records 24, auth provider 21, auth middleware 37, and CORS 54 all pass.
- Runtime and contract matrix: health 36, metrics pulse 14, portal 31, RSS sampler 2, v3 facade 66, virtual clock 21, WASM query E2E 30, library unit harness 405, CLI 164, plugin SDK 122, and 221 remaining affected integration tests pass.
- Final broad workspace gate after the reviewed fixture and ledger edits: 5,434 tests pass across 205 harness groups, zero fail, and seven pre-existing stress cases remain intentionally ignored. Preserved log SHA-256: `36b33a3b330dbad8e001776fcfe143eb438a17ac42be086b3cd79d69bfbbbaec`.
- Formatting, scanner-tool Clippy, plugin-SDK Clippy, and mdBook are green. mdBook log SHA-256: `dc137df35b17344ae33785de9240f26750b43bbe9c9f952b0cd56621b18aa8f4`.
- Full workspace Clippy with `-D warnings` remains red on historical repository debt: 117 diagnostics spanning long-standing argument-count, large-error, type-complexity, documentation, and style lints outside this landing unit. No broad `allow` attributes or unrelated refactors were used to hide it. Preserved log SHA-256: `95634245816dfadbaa037115407989866eb49bffee81684f00e36a5138c87ed0`; final P9 qualification still owns a green repository-wide lint gate.

## Release And Real-World Proof

- A final exact-tree `cargo build -j4 --release --bin aeordb` produced `aeordb 0.9.5`, SHA-256 `fa2581b9134567ffba1d49783d9b838a25c451229759c41b82c366f8f30de2fc`. Build log SHA-256: `f6fdd3e31fb3cdd22f1777aa833fee634d3ea809488e7ec6989f867d11806113`.
- A disposable release-mode database at `/home/wyatt/.cache/codex/aeordb-tests/p2f-live-7OlLSb/live.aeordb` proved malformed `AEORDB_LOG` fails before creating a database, valid startup reaches ready, text and JSON writes list/fetch byte-exactly, `/system/stats` reports real engine state, and SSE emits `server_ready` only after readiness.
- SIGTERM completed graceful worker/engine shutdown with exit code 0. Reopening the same database returned byte-identical content and shut down cleanly again.
- Offline verify reports 198 valid entries, zero corrupt headers/hashes, zero missing/dangling/B-tree/unlisted entries, zero stale/missing/invalid KV entries, and `Status: OK`. Verify log SHA-256: `47edec058973b719a207bac77ca1671916052f3dd37e0bf56b649fe0c8908bce`.
- The disposable server was stopped; no Cargo, Rust compiler, AeorDB, or soak process remains active. No transient artifact, secret, evidence database, or production data is part of the landing diff.

## Next Action

Complete the still-open Child 01 P1c native macOS and Windows durability/fixture gates. Only after that green pushed boundary may Child 01 P3a writers and Child 03 P3b immutable roots/read views activate; P7 migration work remains forbidden until its own start gate.
