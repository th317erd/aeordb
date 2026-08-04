# Child 05 Progress: Index

- **Status:** blocked by start gate
- **Current landing unit:** P5-1
- **Entry commit:** pending Child 01 registries and Child 03 roots
- **Last green commit:** not established
- **Owner:** unassigned
- **Start gate:** format registries and immutable shadow roots green
- **Plan:** [Child 05](../children/05-index-definitions-pages-and-nvt.md)
- **Owned files:** assign before start
- **Forbidden/hotspot files:** assign before start
- **Hotspot handoff commit:** none
- **Narrow gate:** `timeout 15m cargo test -j 6 -p aeordb --test index_v1_reference_spec`
- **Broad gate:** not run
- **Drift/risks:** no production index activation in this child
- **Evidence:** none
- **Next action:** wait for start gate and assign index storage owner
