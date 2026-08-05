# Child 02 Progress: Runtime

- **Status:** P2a-1 complete; P2a-2 next while native P1c host executions remain pending
- **Current landing unit:** P2a-2 grouped hard-frontier commit and platform failure matrix
- **Entry commit:** `c2f373e`
- **Last green commit:** `c618d87`
- **Owner:** Codex, durability/runtime integration owner
- **Start gate:** Child 01 bounded readers and platform probes green
- **Plan:** [Child 02](../children/02-durability-controls-config-and-memory.md)
- **Owned files:** `aeordb-lib/src/engine/durability_coordinator.rs`; `aeordb-lib/spec/engine/v3_contract_facade_spec.rs`; the corresponding module/target declarations; this ledger; `TODO.md`
- **Forbidden/hotspot files:** no live writer routing or v4 authority activation in P2a-1; preserve `.codex/DETAILS.md`, `.codex/wip.md`, and `downloads/`
- **Hotspot handoff commit:** none
- **Narrow gate:** P2a-2 first failed on four out-of-order/gapped hard-authority proofs, missing native classification APIs, missing group-policy APIs, and missing coordinated header publication. It now passes 23 facade tests plus the exhaustive native-error unit test.
- **Broad gate:** The current landing snapshot passes 24 append/header tests, 16 transaction tests, 9 native/control-store tests, and `cargo check -j 4 --workspace --all-targets` with only the historical unused test macro warning. The prior P2a-1 131-test and 436-case campaign gates remain green evidence and will be rerun at the P2a-2 boundary.
- **Drift/risks:** native Linux P1c execution and Windows cross-compilation are green; native `wyatt-mac` and `win11vm` execution remains unavailable and cannot be waived. A fresh retry still returned no route to `wyatt-mac` and connection refused for `win11vm`. Current-source durability work remains distributed across append writer, DiskKVStore, hot-tail timer/transaction/shutdown, KV expansion/resize, header repair, emergency spill, integrity/cluster/version helpers, and route-level temporary-file publication.
- **Evidence:** `../evidence/durability-operation-inventory.json` freezes current producer/consumer ledgers and known gaps. The coordinator now enforces exact contiguous hard-prefix publication, canonical group order, 64 MiB/100 ms bounded selection, oversized singleton handling, one grouped barrier, all-waiter failure, typed bounded retry, and uncertain-completion no-replay. Native errors preserve OS evidence and map all platform classes explicitly. Every v3 inactive-slot helper call now uses native positional authority write, dependency/authority barriers, and exact byte read-back; `AppendWriter` retains one coordinator across updates. A real read-only descriptor test proves authority-write failure leaves the inactive slot untouched.
- **Next action:** converge transaction, timer, KV flush/resize, direct in-place mutation, and shutdown durability through the shared coordinator; then install a raw-durability architecture gate
