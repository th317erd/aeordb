# Child 06 Progress: Query

- **Status:** blocked by start gate
- **Current landing unit:** P6-1
- **Entry commit:** pending Children 03 and 05
- **Last green commit:** not established
- **Owner:** unassigned
- **Start gate:** P6 requires Children 03/05; P7 also requires Child 04 root state
- **Plan:** [Child 06](../children/06-async-coverage-query-pagination-and-locators.md)
- **Owned files:** assign before start
- **Forbidden/hotspot files:** assign before start
- **Hotspot handoff commit:** none
- **Narrow gate:** `timeout 15m cargo test -j 6 -p aeordb --test coverage_runtime_spec`
- **Broad gate:** not run
- **Drift/risks:** coordinated API/client cutover required
- **Evidence:** none
- **Next action:** wait for start gate and assign runtime/query owners
