# Child 01 Progress: Format

- **Status:** P0b in progress; P0b-1 complete
- **Current landing unit:** P0b-2b IndexArtifact envelope, definition, manifest, and active-pointer families
- **Entry commit:** `9eb503b1d8dbee62e2d90493ecf7010075bc2792`
- **Last green commit:** P0b-2a core formats `d0ef3250ffe38dfb8e0b73143a00d25847a4413a`
- **Owner:** Codex, persistent-format/reference owner
- **Start gate:** Child 08 P0a inventory/baseline accepted
- **Plan:** [Child 01](../children/01-format-capabilities-and-fixtures.md)
- **Owned files:** this ledger; `tools/v4-reference/`; `aeordb-lib/spec/fixtures/v4/`; P0b/P0c clauses in `scripts/plan/check-v4-contracts.sh`
- **Forbidden/hotspot files:** production Rust codecs/readers/writers, root Cargo workspace membership, GC/query/index/namespace behavior, real/evidence databases, `.codex/DETAILS.md`, `.codex/wip.md`, and `downloads/`
- **Hotspot handoff commit:** none
- **Narrow gate:** `timeout 2m ./scripts/plan/check-v4-contracts.sh`
- **Broad gate:** P0a stabilized workspace gate passed at entry; P0b-1 is reference/fixture-only and passed its standalone tool tests and static analysis
- **Drift/risks:** repository-wide Clippy is red at entry and tracked by Child 08; independent fixture review remains distinct from later production codec authorship
- **Evidence:** Child 08 P0a evidence commit `5009cd2d975577a207556c605c4e90fdd1ef18cb`, GC stabilization `9b96586959bd4f3011e088f22bb5f1df01cfacae`, and ledger commit `9eb503b1d8dbee62e2d90493ecf7010075bc2792`. P0b-1 first failed because the independent reference manifest was absent, then passed with 10 annotated `DatabaseHeaderV4` fixtures covering 32- and 64-byte hashes, A/B selection, degraded redundancy, equal-sequence ambiguity, CRC rejection, unknown capability, reserved/padding rejection, and physical-ID adoption. `cargo test` passed 4 tests; standalone strict Clippy passed; fresh generation and verification passed; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Next action:** freeze the complete IndexArtifact registry and both-width fixture graph, beginning with the common envelope, definitions, immutable manifests, and A/B pointer identities before page/journal bodies

## P0b-2a Core Framing and Semantic Authority

- **Red proof:** `timeout 2m ./scripts/plan/check-v4-contracts.sh` failed with `P0b-2 core format is absent from the contract registry: whole-entity-v1` before the new reference codecs or fixtures existed.
- **Frozen contracts:** corrected 12-byte `WholeEntityV1` prefix and integrity domain, `DirectoryIndexV1`/`NamespaceRootV1`, and all four `SemanticObjectV1` kinds.
- **Independent corpus:** 24 total fixtures now cover the existing 10 `DatabaseHeaderV4` cases plus valid whole-entity, namespace-root, semantic state (complete and content-only), definition, catalog leaf, and catalog internal objects at both 32- and 64-byte hash widths.
- **Integrity proof:** a standalone mutation test flips every byte in every core fixture and requires deterministic rejection through header CRC, value CRC, integrity hash, identity, or canonical-structure validation.
- **Green commands:** standalone `cargo test -j4` passed 7 tests; standalone `cargo clippy -j4 --all-targets -- -D warnings` passed; fresh `generate` and `verify` passed all 24 fixtures; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Boundary:** no AeorDB production crate dependency, reader, writer, route, database, or root authority changed. Reviewer sign-off remains explicitly pending before the production-writer phase.

## P0b-2b-1 Index Envelope and Active Pointers

- **Red proof:** `timeout 2m ./scripts/plan/check-v4-contracts.sh` failed with `P0b-2 index format is absent from the contract registry: index-artifact-v1` before the index reference module and fixtures existed.
- **Frozen contracts:** common `IndexArtifactV1`/`AIDX` envelope, permanent 15-kind registry, content versus stable pointer key domains, and exact shared body for `FieldIndexActivePointer`, `FieldNvtActivePointer`, and `ScopeCatalogActivePointer`.
- **Independent corpus:** 12 new fixtures cover all three pointer kinds, slots A and B, both hash widths, pointer sequence 1, and pointer sequence `u64::MAX`; the complete corpus now contains 36 fixtures.
- **Behavior proof:** byte-flip mutation rejects every changed pointer byte; pair selection covers highest sequence, equal sequence with identical target selecting A, and equal sequence with different targets failing ambiguous.
- **Green commands:** standalone `cargo test -j4` passed 10 tests; strict standalone Clippy passed; fresh generation/verification passed 36 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** all non-pointer IndexArtifact kind IDs are registered but remain explicitly pending body fixtures and writer-disabled. No production serializer, reader, pointer, or index runtime changed.
