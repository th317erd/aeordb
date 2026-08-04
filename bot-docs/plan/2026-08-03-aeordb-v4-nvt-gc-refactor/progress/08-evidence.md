# Child 08 Progress: Evidence

- **Status:** complete
- **Current landing unit:** P0a plus entry-baseline stabilization
- **Entry commit:** `a40b0158999650a0e0402011ac41372eff7d4fc2`
- **Last green commit:** GC baseline stabilization `9b96586959bd4f3011e088f22bb5f1df01cfacae`
- **Owner:** Codex, campaign integration/evidence owner
- **Start gate:** explicit implementation authorization
- **Plan:** [Child 08](../children/08-verification-operations-docs-and-debt.md)
- **Owned files:** this ledger; `../evidence/baseline-environment.json`; `../evidence/baseline-behavior-and-performance.json`; `../evidence/intended-divergences.yaml`; `../evidence/persisted-producer-consumer-inventory.json`; `../evidence/route-root-contract-manifest.json`; `../evidence/recent-fix-ledger.json`; P0a evidence validator; test-only regression module in `aeordb-lib/src/server/upload_routes.rs`
- **Forbidden/hotspot files:** production behavior in Rust, Cargo manifests, real/evidence databases, `.codex/DETAILS.md`, `.codex/wip.md`, and `downloads/`
- **Hotspot handoff commit:** `5009cd2d975577a207556c605c4e90fdd1ef18cb` froze the defect before `9b96586959bd4f3011e088f22bb5f1df01cfacae` corrected it
- **Narrow gate:** two blob-admission guards pass; `timeout 2m ./scripts/plan/check-v4-contracts.sh` passes with 93 routes and 36 documentation pages
- **Broad gate:** `timeout 45m env TMPDIR=/home/wyatt/.cache/aeordb-test-tmp cargo test -j 4 --workspace --all-targets -- --test-threads=1` passed after the correction in 40m10.40s at 1,570,876 KiB max RSS; log SHA-256 `9e8739405b2b74b50e0af1f63782c61c3929505090ea48c6c7bf3092efacc9b7`
- **Drift/risks:** the July blob-commit queueing fix now has named guards; the deterministic GC type-confusion defect is corrected and guarded; repository-wide Clippy remains red at entry with 71 library and 75 library-test errors (`/tmp/codex/aeordb-v4-p0a/clippy-baseline.log`, SHA-256 `8335bcd9d498ee46a68ae38dfd99ae45f92e865d2061e907e076afe603df08ca`)
- **Evidence:** ratified decision log, formal plan set, pushed entry commit `a40b0158999650a0e0402011ac41372eff7d4fc2`, P0a evidence commit `5009cd2d975577a207556c605c4e90fdd1ef18cb`, GC stabilization `9b96586959bd4f3011e088f22bb5f1df01cfacae`, current-source inventories, repeatable old-vs-old characterization, focused disposable-database probes, and resource-measured gates under `/tmp/codex/aeordb-v4-p0a/`
- **Next action:** start Child 01 P0b from the green stabilized baseline; track Clippy cleanup as explicit repository debt rather than hiding it inside format work
