# AeorDB v4 Repository-Only Definition-of-Done Evidence

## Boundary And Candidate

- Campaign: `aeordb-v4-nvt-gc-2026-08-03`.
- Exact implementation candidate: `535004f166ad4cfaf8e7ab740458f2ef4733d2bc` (`refresh checkpoint audit inventory`).
- Candidate archive SHA-256: `1f534f95840121689b6ccb36dd13eeca7dc0c0aef7f8332ef036078b73b50f05`.
- Candidate root `Cargo.lock` SHA-256: `06e6c7a8eb6dbccf52a0b97a4ee6edeece7297866305b0930314fd47b987faec`.
- Reviewer/integration owner: Codex, with owner-authorized repository execution and no authorization for production/canary/destructive operations.
- Status: the repository-only implementation, documentation, and qualification boundary is complete. Operational gates listed below remain separately gated and unexecuted.

## Parent Definition Of Done

| Parent obligation | Repository state | Command/report proof |
| --- | --- | --- |
| P0 contract registry and both hash widths pass on Linux, native macOS, and native Windows | Passed | `p0b-contract-registry-report.json`, `p0c-machine-contract-report.json`, `check-v4-contracts.sh`, and the three native rows in `p9-final-qualification.json` |
| Every persisted producer/consumer and route has one owner | Passed | `persisted-producer-consumer-inventory.json`, `route-root-contract-manifest.json`, contract gate |
| No v4 writer precedes capability and reader gates | Passed | Child 01 reader/writer/fixture ledgers and architecture targets |
| Acknowledged writes use one durability/authority path | Passed | `durability-operation-inventory.json`, Child 02 coordinator evidence, strict architecture gates |
| Namespace producers use one mutation/root/event path | Passed | Child 03 progress ledger and namespace architecture matrix |
| V4 formats and state machines satisfy exact fixtures and modeled interruption paths | Passed through the authorized non-destructive boundary | 454-fixture contract gate, `p8-cutover-crash-state-report.json`, Children 01/03/04/05/07 ledgers |
| No v1 index operation materializes a whole index | Passed | Child 05 bounded-page/resource targets and `p9-debt-gate-report.json` |
| Missing or invalid NVT cannot change query results | Passed | Child 05 reference/corruption-fallback targets and Child 06 query matrix |
| Historical authorization and selector concealment pass | Passed | Child 06 route/auth/root/reference and live matrices |
| User acknowledgement excludes derived parser/index/NVT work | Passed | Child 06 acknowledgement/worker proof and live timing evidence |
| RSS and scratch remain inside the ratified envelope | Passed | 8 GiB/no-swap result `54bc98f0061273a1d37faa3b20655b2d5993d6bce3631c7a659d12ba00350899` |
| Copied-production migration and dirty restart | Separately gated; not executed | Child 07 P8-1b authorization boundary |
| Canary and operator acceptance precede production cutover | Separately gated; not executed | Child 07 P8-2b/P8 authorization boundary |
| V3 backup and rollback boundary are explicit | Repository contract passed; operational acceptance remains gated | Child 07 reports and `docs/src/operations/migration.md`, `backup.md`, and `deployment-safety.md` |
| Documentation, CLI, API, SDK, Dashboard, SSE, and bot behavior agree | Passed at the repository candidate | `p9-documentation-report.json`, contract/docs gates, live documentation routes |
| Error-handling and duplicate-path debt are bounded | Passed | 1,505 reviewed entries, 35 policies, 29 architecture tests, `p9-debt-gate-report.json` |
| Canonical DoD and completion reports contain command proof | Passed | This document, [completion-report.md](completion-report.md), and [p9-final-qualification.json](evidence/p9-final-qualification.json) |

The two operational parent items remain unchecked in the frozen parent plan because repository implementation does not waive their explicit authorization. They are not silently converted into passing production evidence.

## Child Completion Map

| Child | Repository completion | Primary durable proof |
| --- | --- | --- |
| 01 — formats, capabilities, fixtures | Complete | `progress/01-format.md`; exact fixture/contract/native rows |
| 02 — durability, configuration, memory | Complete | `progress/02-runtime.md`; durability inventory; live/resource evidence |
| 03 — namespace, semantic roots, system families | Complete | `progress/03-namespace.md`; root/mutation/authorization gates |
| 04 — lifecycle, physical inventory, GC, Void | Complete through P4-8; destructive P4-9 activation remains separately gated | `progress/04-gc.md`; model/fault/resource gates |
| 05 — index definitions, pages, sparse NVT | Complete | `progress/05-index.md`; independent reference, corruption fallback, bounded-resource evidence |
| 06 — async coverage, query, APOS, locators | Complete | `progress/06-query.md`; route/auth/reference/live evidence |
| 07 — side-by-side migration and cutover substrate | Complete through synthetic/native repository gates; copied-production/canary/cutover remain separately gated | `progress/07-migration.md`; P8 state/native reports |
| 08 — verification, operations, docs, debt | Complete for the repository-only boundary | `progress/08-evidence.md`; this packet; final integrity audit/index |

## Child 08 Definition Of Done

| Obligation | State | Proof |
| --- | --- | --- |
| Baseline, noise floor, inventories, divergence ledger | Passed | Tracked baseline/inventory/divergence reports and P0 ledger |
| Independent phase oracles and one-command red/green targets | Passed | Child ledgers and named campaign targets |
| Every recent fix has a guard | Passed | `recent-fix-ledger.json`: seven guarded fixes, zero open gaps |
| Error-squelch inventory is complete and shrinking | Passed | Exact 1,505 entries across 35 policies; scanner validation and architecture targets |
| Duplicate/bypass architecture gates cover named classes | Passed | 29 error-handling architecture tests plus contract/debt gates |
| Significant phases have real reopen/verify evidence | Passed | Live release/reopen and per-phase file-backed evidence |
| Linux, native macOS, native Windows qualification | Passed | Exact platform evidence below |
| S1/S2/S3/restart/resource/copy | Passed except separately authorized copied-production | Long/resource evidence below; P8-1b remains closed |
| Docs/API/SDK/UI/SSE/SKILL schemas agree | Passed | P9 documentation report, contract gate, mdBook, live routes |
| Superseded plans cannot be mistaken for authority | Passed | Parent execution banner, supersession markers, reconciled Child 01–07 pointers |
| Transitional debt is deleted or explicitly retained | Passed | `v4-debt-policy.json`, `p9-debt-gate-report.json`, compatibility inventory |
| Canonical reports are complete and reviewable | Passed | This document, completion report, machine qualification record |
| No evidence database, secret, or transient build output is committed | Passed | Tracked-path and staged-diff audit |

## Exact Commands And Results

### Linux

The exact-source runner under `/media/Data/AeorDB/Tests/p9-exact-535004f1/` executed:

```text
cargo fmt --all -- --check
cargo test --locked -p aeordb --all-targets
cargo test --locked -p aeordb-cli --all-targets
cargo test --locked --workspace --all-targets
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p aeordb-error-squelch-audit -- check
cargo test --locked -p aeordb --test error_squelch_architecture_spec
./scripts/plan/check-v4-contracts.sh
./scripts/plan/check-v4-debt.sh
mdbook build docs
cargo build --locked --release -p aeordb-cli --bin aeordb
```

Every long-running stage had a hard deadline. Results: AeorDB 7,080 passed; CLI 202 passed with seven intentional ignores; workspace 7,403 passed with seven intentional ignores; zero failures; strict workspace Clippy, inventory, all 29 architecture tests, contracts, debt, docs, and release builds passed. Result SHA-256 `87d80d19ba3cabd178f9f747f8c1a20cfbd271f3bf2dceaf061c1914ac418984`; artifact manifest `c7dbb076bcb03a9c0de4ca7f89686db58ab40b6790a07e41a0ef6fda66b43fc1`.

### Native macOS arm64

Fresh source was extracted under `/Users/wyatt/.cache/codex/aeordb-p9-535004f1` from the exact archive; no `target` directory was transferred. Archive, lock, portal, and runner hashes were verified before execution. The controller ran:

```text
python3 deadline.py 21600 bash qualify-macos.sh
```

The runner executed formatting, the exact native storage matrix, exact native CLI matrix, and release build with one Cargo job and separate 300/10,800/5,400/5,400-second stage deadlines. It ran from `2026-09-03T14:30:32Z` through `2026-09-03T14:49:13Z` on macOS 26.5.2 build 25F84 with Rust/Cargo 1.95.0. Results: 994 passed, zero failed, seven intentionally ignored. Release binary SHA-256 `d43cf494f797f57e038c8c67de379cef40df55c96fad8b950ece104ad80a857c`; result `f3fe3b5177291c5abd1bf82993c4d4937f14a5ae9f66bdcf37f8dd55909b0809`; preparation manifest `0e1d4f186a9fb9800c3d7a718abab62cac36ea601a7a929d83d11220aa55b9e8`; final manifest `09f9144c6a2d193f4e9db4575bc5807d949bc25c7faf3f48b5ff3e7bff3e61dd`.

### Native Windows x86_64 MSVC

Fresh exact-source preparation and the one-job native storage/CLI/release runner passed 990 tests, zero failed, and seven intentionally ignored. Result SHA-256 `88e387a3c392e4a9afd8748d93dfa024eb5e762abd73fdfc91968ee8bc6c81cc`; release binary `167a64b97e830e9aea8231df9734036f349c2ca57810310322c503e042e75cf3`; preparation manifest `411f495b6a35e8c6623e0f1200542084a3770edf2a6390322531ead5f8ac05e5`; final manifest `708d34aebbd70d7578298a5e2fa7f87b33263197b7648d7dca2b4e0497aa5928`.

### Live, resource, duration, and restart

| Gate | Result |
| --- | --- |
| Live release/reopen/docs/API/offline verify | Passed; result `d5be2aa00b2bed936dd2a9f5dd933a6b8fb082e43d632f70b47065cce8552d85` |
| 8 GiB/no-swap overlap | Passed; peak 1,611,886,592 bytes; swap zero; p99 81.018 ms; result `54bc98f0061273a1d37faa3b20655b2d5993d6bce3631c7a659d12ba00350899` |
| S1 12-hour | Passed; 25,632 writes, 12,808 reads, 4,125 deletes; manifest `fbb043f84000308db76155ebc1eaa94bae48736f280e4cf6b9ef79baa2f53309` |
| S2 12-hour | Passed; 61 planned cycles, zero issue cycles; manifest `fb364ad795a8e335b52ab21ad9c61d801cd67486a36e9ff0636bb3f67e47bf99` |
| S3 12-hour | Passed; 69,194 checkpoint records; manifest `c96290dfd87064495349fcb2cfbd203dd53ab25a5a312e2f75d3dcb93dff0008` |
| Exact successor restart-resilience | 100/100 passed; result `c4230b0c2f854167575cbefe3383e1d5de25858a46b3a7fcd04c3acf9f08ce08`; manifest `fa64f09dd2d7a0d39f8bdbd67368cf25d41e80272e248801de3617a9a75b8fb2` |

Long-duration evidence is carried forward only because Git-object proof establishes that the complete engine tree and long-run scripts are byte-identical from `9a71d4ce` through `535004f1`. The rebuilt final worker was not assumed equivalent: all 100 restart-resilience cases were rerun on `535004f1`.

### Whole-packet integrity

After every evidence writer exited, the final verifier ran on `wyatt-desktop` with a 1,800-second deadline. It checked every referenced manifest and every listed artifact, separately asserted that the known invalid first source-equivalence seal remains invalid/rejected, and returned:

```text
verified_manifests=29 known_rejected_manifests=1
```

Audit result SHA-256 `ed87c06ca35b6fdbc1839d2c2560805b54a6091cadb797edd5e047755eb002b4`; audit manifest `3a0ca376d724ecbb2c88abe3c5210ab232cfa25cbb81b54e97cd960a2212fdab`. The final 31-row centralized index was then independently resolved back to every manifest/reconstruction path and hashed as `f4f6048be6286492bf106bddc96bf417068c7ed133e008e52f9b57b1c2c3a72b`.

## Truthful Rejected Evidence

Failed, interrupted, setup-only, and coverage-incomplete attempts remain preserved. They are indexed as evidence but never counted as passing gates. In particular, the first source-equivalence manifest `8e01d8de7f81fd70dfdba97c71b19460cebb7b1170220e5158dcc87a7dbc1221` is known invalid because output changed a sealed file after manifest creation; the immutable rerun manifest `fe5b5d673bffeba1002dab137e4b11689775ec077000e75e7687a56258bbcd52` is the accepted proof.

## Operational Gates Not Executed

- Production-derived copied-database rehearsal and read-only verification window.
- Canary operation under real clients/load and operator acceptance.
- Local installation or production deployment.
- Operational cutover, first acknowledged v4 write, and post-cutover monitoring.
- Destructive GC activation and real reclaim/reuse on operational data.

These require new explicit authorization. Their frozen parent/child checklist items remain visibly open; repository completion does not imply operational completion.
