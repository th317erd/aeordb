# Child 07 Progress: Migration

- **Status:** blocked by start gate
- **Current landing unit:** P3c-1
- **Entry commit:** pending Child 03 shadow roots
- **Last green commit:** not established
- **Owner:** unassigned
- **Start gate:** P3c requires Child 03; P8 requires Children 04 and 06
- **Plan:** [Child 07](../children/07-side-by-side-migration-cutover-and-rollout.md)
- **Owned files:** assign before start
- **Forbidden/hotspot files:** assign before start
- **Hotspot handoff commit:** none
- **Narrow gate:** `timeout 30m cargo test -j 6 -p aeordb-cli --test cutover_fault_spec`
- **Broad gate:** not run
- **Drift/risks:** no production mutation without explicit P8 operator gate
- **Evidence:** none
- **Next action:** wait for P3c gate and assign migration/operations owner
