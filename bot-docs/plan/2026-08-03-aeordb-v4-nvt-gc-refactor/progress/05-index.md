# Child 05 Progress: Index

- **Status:** active
- **Current landing unit:** P5-1
- **Entry commit:** `6754f6c`
- **Last green commit:** `6754f6c`
- **Owner:** Codex, legacy/KV/v0 NVT separation
- **Start gate:** satisfied; Child 01 registries and Child 03 immutable shadow roots are green
- **Plan:** [Child 05](../children/05-index-definitions-pages-and-nvt.md)
- **Owned files:** `engine/nvt.rs`, new focused legacy/KV/v0 NVT modules, `engine/index_store.rs`, `engine/kv_store.rs`, `engine/disk_kv_store.rs`, `engine/kv_snapshot.rs`, their focused specs, and this progress ledger
- **Forbidden/hotspot files:** no v1 active-pointer publication, query/API activation, namespace/root authority, hard commit waiters, GC sweep/Void policy, v3 source mutation, or KV ordering/page-layout changes
- **Hotspot handoff commit:** none
- **Narrow gate:** `TMPDIR=/home/wyatt/.cache/codex/aeordb-tests/tmp timeout 15m cargo test -j4 -p aeordb --test index_v1_reference_spec -- --test-threads=4`
- **Broad gate:** 5,644 tests across 232 result groups, zero failures
- **Drift/risks:** the current `NormalizedVectorTable` concrete type is shared by KV, v0 field indexes, and public compatibility consumers; wrappers must preserve exact v0/KV bytes and must not leak into v1 index code. No production index activation in this child.
- **Evidence:** 2026-08-10 territory inventory retained at `/home/wyatt/.cache/codex/aeordb-tests/logs/p5-1-nvt-territory.txt`. Combined pre-edit characterization passed 176 tests across `nvt_spec`, `nvt_ops_spec`, `kv_store_spec`, `kv_snapshot_spec`, `disk_kv_store_spec`, and `index_store_spec`. An initial run without the required home-backed `TMPDIR` hit tmpfs `ENOSPC`; the exact home-backed rerun passed, proving an environmental false start rather than a code regression.
- **P5-1 proof:** `index_v1_reference_spec` first failed to compile because the three explicit owners did not exist, then passed all five independent ownership and complete-object byte fixtures. The affected NVT/KV/v0 field-index matrix passes 227 tests. Workspace all-target checking, Rust formatting, diff hygiene, the 436-fixture/95-route/38-document contract gate, and all 28 error-squelch architecture tests pass. The suppression inventory remains exactly 1,477 reviewed occurrences; the moved converter clone invariant retains its prior reviewed classification. Strict library Clippy remains exactly 110 historical diagnostics and reports none in the new ownership modules; retained log SHA-256 is `b10b6d2188637c384bc84f10d35b7d65f2500131e4225dfbc138ac116cdaf41b`.
- **P5-1 broad and live proof:** the complete package/all-target gate passes 5,644 tests across 232 result groups with zero failures; retained log SHA-256 is `2edce8f3d9cb1d2b0459b7acb23de8e2ace205754df404c601b599ee9fb8b864`. A fresh auth-disabled CLI database at `/home/wyatt/.cache/codex/aeordb-tests/real-world/p5-1-20260812-1/live.aeordb` passed text/JSON writes, v0 `@filename` search, graceful shutdown, restart, exact file reads, repeated search, and offline verification. Verification found 198 valid entries with zero corrupt hashes/headers, missing/dangling/unlisted entries, B-tree issues, invalid offsets, or invalid voids. HTTP and verify log SHA-256 values are `08eb44aa4f70c1a926468740ba4dba6bf11155a085b8ee99fb3fc48eb6299eca` and `b0cedc386825edaf90fddbafc7fb5569443134afa8240d1490d0c0ecfc1e5916`. Production source contains no `NormalizedVectorTable` consumer outside the compatibility facade/re-export, no v1 field-index code uses a compatibility NVT, and no active v1 index pointer was introduced.
- **Next action:** review and land P5-1, then begin P5-2 with the converter/strategy/source/definition registry reference contract.
