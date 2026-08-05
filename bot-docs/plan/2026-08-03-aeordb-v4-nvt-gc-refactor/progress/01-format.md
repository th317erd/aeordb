# Child 01 Progress: Format

- **Status:** P0b in progress; P0b-1 complete
- **Current landing unit:** P0b-2b-3 CanonicalConfigValueV1, ValueStoreDefinitionV1, source-selector, and dependency framing
- **Entry commit:** `9eb503b1d8dbee62e2d90493ecf7010075bc2792`
- **Last green commit:** P0b-2b-1 index pointer formats `f418e88536f3adc5029cd0519a045fcb8f4b6ae6`
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
- **Next action:** freeze the corrected Round 8/8A `ValueStoreDefinitionV1` graph, including canonical config values, selectors, parser/dependency child framing, and both hash widths before field converter/index definitions

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

## P0b-2b-2 Canonical Scope Definition

- **Red proof:** after the prior campaign gate passed all 36 fixtures, `timeout 2m ./scripts/plan/check-v4-contracts.sh` failed with `P0b-2 definition format is absent from the contract registry: scope-definition-v1`.
- **Frozen contracts:** common 32-byte semantic-definition envelope, exact `ScopeDefinitionV1` body, `ScopeId` domain, path/glob canonicalization, byte-oriented glob matching, FileKey derivation, and protected internal-path policy from approved Round 7.
- **Independent corpus:** six new fixtures cover direct root and normalized non-root glob definitions at both hash widths plus the exact 65,536-byte maximum for each; the complete corpus now contains 42 fixtures.
- **Malformed/resource proof:** fixed framing, checked lengths, enum/semantic IDs, reserve bytes, mode/length coupling, UTF-8, canonical path/glob, overflow/oversize, byte-oriented Unicode glob behavior, and structural-or-semantic mutation protection are covered. Exhaustive byte mutation applies to ordinary fixtures; maximum-size fixtures mutate every structural byte and distributed payload boundaries to avoid a quadratic verifier.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 18 tests in 0.49 seconds; strict standalone Clippy passed; fresh generation/verification passed all 42 fixtures; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Boundary:** no production AeorDB codec, reader, writer, namespace resolver, index runtime, route, or database changed. The reference tool remains outside the workspace with no AeorDB crate dependency.
