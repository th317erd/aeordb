# Child 07 Progress: Migration

- **Status:** active; Child 03 start gate is complete and P3c is restored as the required P6 activation prerequisite
- **Current landing unit:** P3c-1
- **Entry commit:** pending the P6-2d-c3 shared-owner preparation landing
- **Last green commit:** not established
- **Owner:** current Codex thread after the shared-owner handoff; migration/control hotspots remain serialized
- **Start gate:** satisfied for P3c because Child 03 is complete; P8 still requires Children 04 and 06
- **Plan:** [Child 07](../children/07-side-by-side-migration-cutover-and-rollout.md)
- **Owned files:** assign before start
- **Forbidden/hotspot files:** assign before start
- **Hotspot handoff commit:** none
- **Narrow gate:** `timeout 30m cargo test -j 6 -p aeordb-cli --test cutover_fault_spec`
- **Broad gate:** not run
- **Drift/risks:** no production mutation without explicit P8 operator gate
- **Ratification:** on 2026-08-13 the operator ratified the remaining recommendations and policy decisions. This authorizes the disconnected P3c substrate and later planned implementation gates, but not deployment, destructive migration, production cutover, or first-v4-write acceptance.
- **Evidence:** none
- **Dependency correction:** the parent graph requires P3c's separate v4 shadow authority before P6 can install its native recovery store into lifecycle. A live v3 `StorageEngine` cannot safely stand in for that authority. No source file, production database, deployment, or cutover is authorized by this correction.
- **Next action:** land the shared-owner preparation, assign P3c-1 owned files, refresh the preflight/identity/control territory, and add the smallest failing migration-state test before production code.
