# Child 02 Progress: Runtime

- **Status:** P2a-1 complete; P2a-2 next while native P1c host executions remain pending
- **Current landing unit:** P2a-2 grouped hard-frontier commit and platform failure matrix
- **Entry commit:** `c2f373e`
- **Last green commit:** `c2f373e`
- **Owner:** Codex, durability/runtime integration owner
- **Start gate:** Child 01 bounded readers and platform probes green
- **Plan:** [Child 02](../children/02-durability-controls-config-and-memory.md)
- **Owned files:** `aeordb-lib/src/engine/durability_coordinator.rs`; `aeordb-lib/spec/engine/v3_contract_facade_spec.rs`; the corresponding module/target declarations; this ledger; `TODO.md`
- **Forbidden/hotspot files:** no live writer routing or v4 authority activation in P2a-1; preserve `.codex/DETAILS.md`, `.codex/wip.md`, and `downloads/`
- **Hotspot handoff commit:** none
- **Narrow gate:** `timeout 2m cargo test -j 4 -p aeordb --test v3_contract_facade_spec` passed 12 tests; the target first failed on the absent module as required
- **Broad gate:** 131 tests passed across facade, append writer, transaction, shutdown, v3 header/restart, and native durability targets; `cargo check -j 4 --workspace --all-targets` passed with the one historical unused test macro warning; the 436-case campaign contract gate passed
- **Drift/risks:** native Linux P1c execution and Windows cross-compilation are green; native `wyatt-mac` and `win11vm` execution remains unavailable and cannot be waived. A fresh retry still returned no route to `wyatt-mac` and connection refused for `win11vm`. Current-source durability work remains distributed across append writer, DiskKVStore, hot-tail timer/transaction/shutdown, KV expansion/resize, header repair, emergency spill, integrity/cluster/version helpers, and route-level temporary-file publication.
- **Evidence:** `../evidence/durability-operation-inventory.json` freezes current producer/consumer ledgers and known gaps. The shell enforces all three classes, all fifteen frozen operation IDs, fail-closed plan ordering, monotonic ticket ownership, bounded ledger retention, panic/failure evidence, terminal waiter retirement, concurrent out-of-order proof, and a hard frontier that cannot cross an earlier unproven or failed authority. Strict Clippy remains at the exact historical 72 library diagnostics and reported no diagnostic in the new module/spec.
- **Next action:** add red grouped-commit/waiter/failure-classification tests, then route the first v3 hard authority wave through the coordinator without changing v3 bytes
