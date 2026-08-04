# Child 08 Progress: Evidence

- **Status:** complete
- **Current landing unit:** P0a
- **Entry commit:** `a40b0158999650a0e0402011ac41372eff7d4fc2`
- **Last green commit:** ratified plan baseline `a40b0158999650a0e0402011ac41372eff7d4fc2`
- **Owner:** Codex, campaign integration/evidence owner
- **Start gate:** explicit implementation authorization
- **Plan:** [Child 08](../children/08-verification-operations-docs-and-debt.md)
- **Owned files:** this ledger; `../evidence/baseline-environment.json`; `../evidence/baseline-behavior-and-performance.json`; `../evidence/intended-divergences.yaml`; `../evidence/persisted-producer-consumer-inventory.json`; `../evidence/route-root-contract-manifest.json`; `../evidence/recent-fix-ledger.json`; P0a evidence validator; test-only regression module in `aeordb-lib/src/server/upload_routes.rs`
- **Forbidden/hotspot files:** production behavior in Rust, Cargo manifests, real/evidence databases, `.codex/DETAILS.md`, `.codex/wip.md`, and `downloads/`
- **Hotspot handoff commit:** none
- **Narrow gate:** two blob-admission guards pass; `timeout 2m ./scripts/plan/check-v4-contracts.sh` passes with 93 routes and 36 documentation pages
- **Broad gate:** the serial, disk-backed workspace run reached one deterministic entry-baseline regression: `symlink_ecosystem_spec::test_gc_with_symlink_and_snapshot`; default parallel fixtures separately demonstrated `/tmp` tmpfs exhaustion and the affected KV target passed 48/48 in isolation
- **Drift/risks:** the July blob-commit queueing fix had no named concurrency guard at entry, now covered without altering production admission behavior; GC mistakes an exactly hash-sized symlink payload for a hard-link target, recorded as a blocker for a separate correction before P0b
- **Evidence:** ratified decision log, formal plan set, pushed entry commit `a40b0158999650a0e0402011ac41372eff7d4fc2`, current-source inventories, repeatable old-vs-old characterization, focused disposable-database probes, and resource-measured broad-gate attempts under `/tmp/codex/aeordb-v4-p0a/`
- **Next action:** land P0a evidence, correct the deterministic GC type-confusion defect independently, and rerun the affected and broad gates before Child 01 P0b
