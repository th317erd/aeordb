# Child 08 Progress: Evidence

- **Status:** P0a is complete; P2f repository-wide error-squelch classification and gate is in progress
- **Current landing unit:** P2f Error-Squelch Inventory And Shrinking Allowlist
- **Entry commit:** `1a57eeb`
- **Last green commit:** P2e Wave 5 exit `1a57eeb`
- **Owner:** Codex, campaign integration/evidence owner
- **Start gate:** P2a-P2e are green and pushed; P2f must land before any P3 writer activates
- **Plan:** [Child 08](../children/08-verification-operations-docs-and-debt.md)
- **Owned files:** this ledger; new P2f suppression inventory/allowlist and checker; error-squelch architecture target; P2f evidence reports; narrowly required production corrections after classification
- **Forbidden/hotspot files:** v4 persistent registries/bytes, P3 writers, P4 GC model, P5 indexes/NVT, P6/P7 API cutover, migration/cutover activation, real/evidence databases, `.codex/DETAILS.md`, `.codex/wip.md`, and `downloads/`
- **Hotspot handoff commit:** `1a57eeb` from completed P2e producer convergence into the P2f repository audit
- **Narrow gate:** planned checked suppression inventory and architecture target; every retained occurrence must have class, rationale, owner, guarding test, and removal condition
- **Broad gate:** entry source passes 5,232 workspace/all-target tests across 198 groups with zero failures and seven intentional ignores; log SHA-256 `13535fca81bdc18297953da9ae27f020fd73e3682e25557a77dfd99b1f3e5c86`. The 436-fixture/95-route/38-doc contract gate, workspace check, formatting, and mdBook are green.
- **Drift/risks:** P0a recorded pattern counts but did not create the ratified shrinking checked allowlist. Text-only matching can miss semantic suppression or overcount tests/comments, so the inventory needs stable source identities, explicit production scope, syntax-aware review where practical, and exact stale-entry detection. Known pre-GC snapshot listing/deletion/retention warnings are first-class audit candidates. Repository-wide Clippy remains historical debt and is not a substitute for this semantic audit.
- **Evidence:** P0a baseline artifacts remain valid; P2e landed at `1a57eeb` with exact maintenance failure direction. P2f starts from current source and must preserve both truthful failures and optional-cleanup primary-result precedence.
- **Next action:** map every production suppression form and named high-risk hotspot, freeze the inventory schema/scanner contract, and create the first failing gate before correcting or allowlisting any occurrence
