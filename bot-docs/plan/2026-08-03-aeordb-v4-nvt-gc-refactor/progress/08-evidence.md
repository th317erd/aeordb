# Child 08 Progress: Evidence

## Landing State

- **Status:** P0 through the safe P8 native candidate gates are complete; P9 repository debt plus operator/documentation closure are green, and strict Clippy is next. Copied-production, canary, install/deploy, cutover/acceptance, and destructive GC remain separately gated.
- **Current landing unit:** P9 migration/operator/bot documentation, precise stale-plan supersession, and the retained hot-dir compatibility contract are green; repository-wide strict Clippy is next.
- **Entry commit:** `30cee29f8deeecda48419ddc31c697178ac375b9` (`record P8 native release qualification`).
- **Last pushed green commit:** `bbe6f265f50ab177bc09b89c5fbd469d4ffbb2f7` on `development` and `origin/development`.
- **Owner:** Codex, campaign integration/evidence owner.
- **Start gate:** Safe P8 migration workspaces, cutover fault target, exact-source native release candidates, live documentation routes, and integrated Linux workspace qualification are green and pushed.
- **Plan:** [Child 08](../children/08-verification-operations-docs-and-debt.md).
- **Owned files:** this ledger; suppression scanner/inventory; architecture target; narrowly required production corrections and regression tests; affected operator documentation.
- **Forbidden files and actions honored:** no v4 service authority, migration/cutover activation, production or evidence database mutation, deployment, `.codex/DETAILS.md`, `.codex/wip.md`, or `downloads/` changes.

## P9 Debt Gate Qualification

- The new checked policy classifies four still-required compatibility surfaces with exact owners, allowed files, non-growing match ceilings, rationales, and removal gates: selected-root v3 reads, explicit legacy-v3 root service mode, the active v0 whole-index manager, and the embedded JSON/base64 query cursor. Three forbidden classes must remain at zero: the dead auto-heal stub, the misleading `legacy_position_component` name, and any v4 `IndexManager::new` bypass. The gate passes at 7 reviewed entries and 87 retained matches and is now part of the independent v4 contract wrapper.
- The opt-in auto-heal module and feature were safely deleted because peer retrieval had no transport and always returned an error. Its two stale suppression rows were removed, shrinking the reviewed ceiling from 1,510 to 1,508, and the persisted producer/consumer inventory now records the retired writer explicitly. Required v3 readers and writers remain intact.
- The v0 whole-index conversion helper is now named for its input representation, and `FromStr` is imported only on Linux. Identical changed-source hashes were verified on Linux, macOS arm64, and Windows x86_64 MSVC; the deleted source is absent on all three. Release-profile library checks pass natively in 49.16 seconds on macOS and 175 seconds on Windows, with the former unused-import warning absent on both.
- The adversarial shell self-test covers the valid policy and every validation family: missing/malformed/schema metadata, duplicate identities/paths, classification and ceiling rules, unsafe/missing/out-of-root paths, invalid patterns, new paths, match growth, forbidden matches, and stale review paths. It passes under an outer 20-second deadline; the standalone gate passes under 120 seconds.
- The affected Linux matrix passes 176 aggregation, suppression-architecture, query-engine, and pagination tests with zero failures. Workspace all-target checking passes with only the historical test-only `require_wasm_parser` warning. The chained independent contract gate passes 454 fixtures, 7 debt entries/87 retained matches, 95 routes, and 38 documentation pages.
- Durable command, failure-first, native, hash, and boundary evidence is recorded in `evidence/p9-debt-gate-report.json`. The unit touches no production or production-derived database, install, download artifact, deployment, canary, service activation, cutover, acceptance, first v4 write, or destructive GC boundary.

## P9 Documentation and Supersession Qualification

- The active 39-page mdBook now states the exact current authority: ordinary service operation remains on the v3 compatibility runtime, no public CLI/HTTP migration or automatic v4 activation exists, and legacy layout convergence plus forced FileRecord backfill are not v3-to-v4 migration. The new operator page freezes preflight, clone/capture/reconciliation, read-only cutover, acceptance, first-write, rollback, evidence, monitoring, and destructive-GC boundaries without exposing an internal command as public API.
- Architecture, storage, indexing, configuration, CLI, installation, backup, deployment, GC, observability, API-versioning, client-release, and served bot guidance agree on the current-v3/staged-v4 boundary. The old external `.aeordb.kv` and hot-file recovery advice is removed from active instructions; the current single file, lock, external spill evidence, and logs are preserved instead.
- Thirty-three identified historical design/implementation documents now carry explicit fully-superseded, partially-incorporated, or active-v3-history banners with valid links to the current campaign. The external hot-file transaction documents are explicitly pre-single-file history rather than current recovery authority.
- Territory tracing found that `--hot-dir`, `storage.hot_dir`, and the public Rust constructor family remain accepted but are ignored by the current engine. Their public descriptions and startup output now say so, and `legacy-hot-dir-option` is a timed compatibility entry with owner, exact five-file production surface, 77-match ceiling, and removal gate. The complete debt policy passes at eight entries and 164 retained matches.
- Failure-first proof records the absent migration page, machine inventory drift, stale external-hot-file claims, and missing debt-policy entry. mdBook, all 39 source links, JSON/YAML syntax, formatting, diff hygiene, 33 banner links, the debt self-test/gate, and the independent 454-fixture/95-route/39-document contract gate pass.
- On `wyatt-desktop`, 64 focused CLI configuration/start/live-route tests pass, workspace all-target checking is green except for the historical test-only macro warning, and the exact release CLI builds in 2m23s. Its generated help preserves the flag without advertising external hot files and exposes no `migrate`/`cutover` command.
- A real release server under `/home/wyatt/.cache` proved healthy startup, embedded docs/migration/CLI/SKILL `200` responses, a missing-doc `404`, byte-exact file PUT/GET, missing-value parser refusal before database creation, no creation of the deliberately nonexistent compatibility hot path, clean SIGINT exit, and offline verification of 195 valid entries with zero reported defects. The first harness attempt is truthfully retained: public health returned `200` with `status=starting` while every protected route returned the expected `503`; the final harness waits for `status=healthy`.
- Durable failure, build, link, contract, live, hash, and authorization evidence is recorded in `evidence/p9-documentation-report.json`. No production or production-derived database, install, download artifact, deployment, canary, cutover, acceptance, first v4 write, or destructive GC boundary was touched.

## P9 Next Action

Eliminate the repository-wide strict-Clippy backlog without suppressions or weakened lint policy, then run the final broad/native/crash-soak/resource qualification. Keep copied-production, canary, installation/deployment, operational cutover, acceptance, monitoring, and destructive GC behind their exact separate authorization gates.

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
- P3a-4 publishes every validated mutable and immutable SystemControlV1 payload through one typed FileRecord v1 adapter, shared namespace authority, and hard durability acknowledgement. Legacy v0 wrappers remain migration input only. The exact native ten-target matrix passes 203 tests on both macOS arm64 and Windows x86_64 MSVC, and the final Linux workspace closure passes 5,478 tests across 208 result groups with zero failures and seven intentional ignores.
- P3a-5 advertises only complete common-format writers: WholeEntityV1 bit 0 and SystemControlV1 bit 4. Partial immutable-only IndexArtifactV1 and GcArtifactV1 writers do not advertise bits 7/12 because their mutable pointer/control families remain absent. Current writable and peer admission still fail closed on the nine missing baseline writer capabilities.
- No service authority, route, migration destination, deployment, or production database byte is active.

## Next Action

Begin Child 03 P3b-2c with an exhaustive first-authority visibility map and a failure-first transaction target. Keep prepared roots unadmitted, then publish the authority selector and admission witness through one shared visibility/durability boundary without selecting HEAD or activating service reads. Keep capability bits 1, 2, and 5 disabled, read-view/lifecycle/pins in P3b-3, and migration work behind its P7 gate.

## P3b-2b Semantic Object Storage Closure

- Exact commit `0b739c71b434101f35660c051bd657f73148681e` adds disconnected, deterministic, write-once v4 semantic-object publication through the ordinary system FileRecord v1 namespace and hard-durability authorities. It activates no HEAD, service, startup, route, migration, or capability caller.
- The focused target passes 8 tests; the adjacent twelve-target v4 matrix passes 267. The final Linux workspace/all-target run passes 5,501 tests across 210 harness groups with zero failures, seven intentional ignores, and 126 filtered tests. Preserved broad log SHA-256: `e66b2627c53974c7dc7293100d4ee0fa5fdfbee9f6de20349dca3e769de6beab`.
- Workspace checking, both formatters, mdBook, `git diff --check`, the independent 436-fixture/95-route/38-document contract gate, the exact 1,420-entry suppression check, and all 27 suppression architecture tests are green. Strict focused Clippy remains blocked only by the established 110-diagnostic crate baseline and reports nothing in the changed semantic-store/spec files; preserved non-strict log SHA-256 is `dde4e74b7b9ab0d5ac613bcd8d058eead4e4df9889abcffeb52a3b9335168b9d`.
- The exact twelve-target, 267-test matrix passes on native macOS arm64 and Windows x86_64 MSVC using lockfile SHA-256 `b9fa3f1afdb6eac58ff877e942381bc92e45e81a3a2e51a20c671024333793de`. macOS also passes the portable contract gate. Retained native log SHA-256 values are `8094691c6e3ba24ac552b03f71c764d290faa685b24444c212f31bdc53b255f4` and `6b33820ae59540f0b2ef00ed1ae49b9c4be18f27e3452a9026ab158b46453471`.
- Real-storage proof creates actual engine files, hard-publishes every semantic kind, shuts down cleanly, reopens, and loads exact bytes on all three native platforms. The isolated native worktrees, build targets, logs, and disposable tooling were removed after qualification; no server, Cargo process, or user checkout was left active or modified.

## P3b-1 Immutable Root Authority Closure

- Commit `ba60b4d9447e89576a0ce3665b0570c41566c34a` adds disconnected immutable namespace-root, namespace-tree, semantic-state, and admission-witness readers plus an independent reference model. No startup, route, HEAD selector, lifecycle, authorization, migration, or service caller is activated.
- The focused target passes 11 tests; the complete adjacent v4 matrix passes 121 tests on Linux, macOS arm64, and Windows x86_64 MSVC. macOS passes the independent 436-fixture/95-route/38-document contract wrapper; Windows natively exercises all 436 fixtures but does not claim the POSIX wrapper because its `jq` and Python/PyYAML dependencies are absent.
- The final Linux workspace/all-target gate passes 5,490 tests across 209 result groups with zero failures and seven intentional ignores. Preserved log SHA-256: `d5eff5ef36265578aa2eb3b95cdef6c93532e234c32cc2328c26fc4f95f8e13a`.
- Workspace checking, both Rust formatters, mdBook, `git diff --check`, the exact 1,420-entry suppression check, all 27 suppression architecture tests, and the independent contract gate are green. Real storage proof performs store, clean shutdown, reopen, bounded verified reads, authority resolution, and final shutdown on a disposable database.

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
- **Native falsifier:** the first Windows adjacent matrix exposed a mixed-separator expectation in `validated_external_spills_seed_one_persistent_recovery_incident`. Production returned the canonical native path; the test had constructed one path component containing a literal slash. Commit `14fbace` corrects only the independent oracle to join `earlier` and `manifest.json` as separate components. The corrected v3 transition target passes 20 tests locally and on Windows.
- **Native proof:** exact commit `14fbacec8ebfdff5e51b89b633a887d1ff5a2fa7` passes the same ten-target, 203-test matrix on macOS arm64 and Windows x86_64 MSVC. Both hosts pass all ten v4 ControlStore writer tests, including storage-engine create, hard publication, shutdown, reopen, exact read-back, malformed input, concurrency, and legacy rollover. macOS also passes the portable contract gate with 436 independent fixtures, 95 routes, and 38 documentation pages. The Windows native behavior/fixture matrix is green; the POSIX contract wrapper is not claimed there because that host lacks its `jq`, `rg`, and Python/PyYAML command dependencies.
- **Exact Linux closure:** the final `14fbace` workspace/all-target run passes 5,478 tests across 208 result groups with zero failures and seven intentionally ignored crash-injection cases. Preserved log SHA-256: `84dd209b4a042db380cf50dbfd65ba41f4e37f5d3310206e86c66e1e455d1cc8`.
- **Real storage boundary:** the disconnected writer has deliberately no HTTP, startup, migration, or service caller, so an HTTP live test would falsely activate authority. The focused target instead exercises the real `StorageEngine` and namespace/durability coordinators against disposable database files, including shutdown/reopen and byte-exact read-back on all three native platforms.
- **Boundary:** P3a-4 activates no v4 writer capability, service/storage caller, startup admission, route, migration destination, deployment, or production database byte. P3a-5 owns the capability-profile change.

## P3a-5 Capability Profile Qualification

- **Red proof:** the first exact profile test failed because the binary advertised no writers. An adversarial contract pass then rejected the initial proposed `[0, 4, 7, 12]` profile: bits 7/12 cover complete artifact families, while P3a-3 deliberately has no mutable A/B pointer/control writers. A second preserved red run proves those extra bits were present before the profile was tightened. Logs: `/home/wyatt/.cache/codex/aeordb-tests/logs/p3a5-capability-profile-red.log` and `/home/wyatt/.cache/codex/aeordb-tests/logs/p3a5-partial-artifact-capability-red.log`.
- **Exact profile:** `BinaryCapabilityProfileV1::current()` continues to advertise all 24 readers and now advertises only WholeEntityV1 bit 0 and SystemControlV1 bit 4. Generated capability constants feed one private const bit builder. All other bits remain absent; IndexArtifactV1 and GcArtifactV1 stay disabled until their complete mutable families exist.
- **Fail-closed proof:** writable admission reports the exact missing baseline bits `[1, 2, 3, 5, 6, 18, 19, 21, 22]`. A peer source/destination pair using the current profile fails as `peer_destination_writer_capability_mismatch`. Source architecture proves no production `admit_v4_header` or current-profile caller exists, and the storage engine still has no P3 writer caller.
- **Qualification:** the eight-target P3a matrix passes 150 tests locally, on macOS arm64, and on Windows x86_64 MSVC at exact commit `ec20bd4a9627f48d52ddf38d0f4a723fa4203d4a`. The complete Linux workspace passes 5,479 tests across 208 groups with zero failures and seven intentional ignores; log SHA-256 is `2bba8e296ce453d2ff7387ec5c77e385f749a4e539dd498865a87304ced6a6d7`. Workspace/all-target checking, both Rust formatters, mdBook, `git diff --check`, the 436-fixture/95-route/38-page contract gate, all 27 suppression architecture tests, and the exact 1,420-entry suppression check pass. Three suppression rows moved by line only; normalized entry SHA-256 remains `16b0368f9d415224028baa5657d16975964da4559717731d144fca526de205a8`.
- **Boundary:** capability self-description changes, but no v4 database header is selected by service startup and no service, storage, route, peer, migration, index, GC, or root authority caller consumes the profile. No persistent byte or production database changed.
