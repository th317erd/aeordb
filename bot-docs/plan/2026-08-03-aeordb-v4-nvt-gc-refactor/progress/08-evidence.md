# Child 08 Progress: Evidence

## Landing State

- **Status:** P0a and P2f are complete; continuous phase evidence remains active through P9.
- **Current landing unit:** P3a-4 ControlStore FileRecord publication is locally green; exact native-host qualification remains before closure.
- **Entry commit:** `5e0dc2a` (`fix: return metrics initialization conflicts`).
- **Last pushed green commit:** P3a-3 immutable artifact writer commit `5b7f89c` on `development` and `origin/development`.
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

## Current P3a Evidence

- P3a-1's checked common encoders byte-match both independent hash-width fixtures and remain disconnected from production authority.
- P3a-2's serialized header publisher uses the shared durability coordinator, exact inactive-slot publication, full barriers/read-back, and fail-closed two-slot fencing/adoption. Its fault suite covers roughly 6,150 crash/failure prefixes.
- Native qualification found and corrected a Windows shared-cursor regression in positional reads. Exact commit `e41e45d` passes four internal fault tests and the 216-test adjacent matrix on Linux, macOS arm64, and Windows x86_64 MSVC. Both native hosts pass the 436-fixture, 95-route, and 38-document contract gate.
- The corrected Linux workspace/all-target closure passes 5,463 tests across 207 result groups with zero failures and seven intentionally ignored stress cases. Its log SHA-256 is `417c3bfe37314d69f7a51e975373f12e733c9fee75281eca4d1dd4433deaec88`; the suppression inventory remains exactly 1,420 reviewed occurrences after refreshing four line-only metadata entries.
- P3a-3 adds disconnected immutable AIDX/AGCA envelope writers only. All 54 immutable index and 92 immutable GC fixtures byte- and key-match the independent oracle; every mutable pointer/control fixture is excluded or rejected; every kind cap and allocation boundary is frozen independently. The complete workspace/all-target closure passes 5,468 tests across 207 result groups with zero failures and seven intentionally ignored crash-injection cases. Its log SHA-256 is `36fe2ba3e784e61ee19890602dcfcc3c927900e35a91ba7bcfb41d2b31a14878`; the reviewed suppression inventory remains exactly 1,420 entries.
- No writer capability, service authority, route, migration destination, deployment, or live database byte is active.

## Next Action

Qualify the exact P3a-4 landing commit on macOS arm64 and Windows x86_64 MSVC, record phase evidence, and close the landing unit. Keep capability activation in P3a-5 and Child 03 P3b immutable roots/read views behind their own start gates; P7 migration work remains forbidden until its gate.

## P3a-4 Entry Territory

- **Authority:** P2's `V3TransitionControlStore` is the sole current control producer and intentionally writes system FileRecord v0 wrappers. Its typed context owns the outer namespace guard, canonical A/B path derivation, one shared namespace transaction, hard acknowledgement, read-back, and selected-state verification. Generic file, sync, plugin, and index APIs remain unable to mint this typed authority.
- **Target representation:** P3a-4 must preserve each validated SystemControlV1 payload byte-for-byte while publishing the frozen ordinary FileRecord v1 wrapper with `FLAG_SYSTEM`, `application/vnd.aeordb.system-control`, normal path/content/identity locators, and current content hash. Mutable kinds use inactive A/B rollover; immutable kinds use idempotent `i.ctrl` publication at sequence one.
- **Compatibility:** the new shadow writer may accept a validated legacy FileRecord v0 slot as migration input, but every new slot it publishes is v1. Two ordinary mutable publications therefore converge both A/B wrappers without an out-of-band rewrite. The v3 transition writer and every current service caller remain unchanged until a later activation gate.
- **Proof order:** fail first on the absent v4 writer/context; cover all twenty kinds, strict metadata and body validation before mutation, exact payload read-back, one hard acknowledgement, legacy rollover, immutable idempotency, concurrent mutable sequence selection, restart, protected typed authority, and zero production callers; then run adjacent transition/namespace/durability/system-family gates before broad closure.

## P3a-4 Local Qualification

- **Red proof:** `v4_control_store_writer_spec` first failed only on the absent `V4ControlStore`, typed publication context, and frozen system-control MIME constant. The preserved failure log is `/home/wyatt/.cache/codex/aeordb-tests/logs/p3a4-control-writer-red.log`.
- **Implementation:** the disconnected `V4ControlStore` validates complete SystemControlV1 bytes before acquiring namespace authority; publishes every mutable and immutable kind through one typed `DirectoryOps` FileRecord v1 adapter; derives the canonical A/B/I path only from the typed context; uses the shared namespace/hard-durability coordinator; reads back the exact captured FileRecord chunk inventory; requires the frozen MIME, empty metadata, exact content hash, body and wrapper bounds; accepts FileRecord v0 only as validated migration input; and leaves the existing v3 transition writer unchanged and v0-only.
- **Focused proof:** all 10 writer tests pass. They cover all twenty kinds, exact wrappers and payloads, one hard sequence per publication, mutable v0 A/B rollover, immutable v0 rollover and v1 idempotency, shutdown/reopen, invalid database/kind/mutability/CRC input before mutation, sixteen concurrent publishers with exactly one winner, noncanonical MIME/metadata/path/hash/version/flags/lengths, missing chunks, torn control payloads, typed authority, and zero production callers.
- **Adjacent proof:** the ten-target storage/control matrix passes 203 tests; the internal library harness passes 409. The complete workspace/all-target suite passes 5,478 tests across 208 result groups with zero failures and seven intentionally ignored crash-injection cases. Preserved broad log SHA-256: `a91c46d0e06d1a9bbcbb477c6c2df9bf0dbeec1f84661fa822c13f484b153614`.
- **Static and contract proof:** workspace/all-target checking, both Rust formatters, mdBook, `git diff --check`, the 436-fixture/95-route/38-document contract gate, the exact 1,420-entry suppression check, and all 27 suppression architecture tests are green. The suppression inventory changed only 31 source line numbers after adapter insertion; normalized records and every stable ID remain byte-equivalent.
- **Boundary:** no v4 writer capability, service/storage caller, startup admission, route, migration destination, deployment, or live database byte is active. Native macOS/Windows qualification is the only remaining P3a-4 closure gate.
