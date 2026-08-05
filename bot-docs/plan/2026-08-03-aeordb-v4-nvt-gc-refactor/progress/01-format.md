# Child 01 Progress: Format

- **Status:** P0b in progress; P0b-1 complete
- **Current landing unit:** P0b-2b-3c ParserResolutionPlanV1, DependencyTableV1, SourceSelectorV1, and ValueStoreDefinitionV1
- **Entry commit:** `9eb503b1d8dbee62e2d90493ecf7010075bc2792`
- **Last green commit:** P0b-2b-3a CanonicalConfigValueV1 `f31a5b2d1f4004c7499b88673a682be01e6829c0`
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
- **Next action:** freeze the remaining corrected Round 8A/9 ValueStore closure in dependency order: dependency records and empty table, parser plan/candidates, source selectors, then the parent `ValueStoreDefinitionV1`

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

## P0b-2b-3a Canonical Configuration Values

- **Red proof:** the 42-fixture campaign gate first failed with `P0b-2 definition format is absent from the contract registry: canonical-config-value-v1`.
- **Frozen contracts:** all ten permanent `CanonicalConfigValueV1` tags, exact self-framing, integer/floating-point normalization, UTF-8 and arbitrary bytes, ordered arrays, raw-UTF-8-sorted unique maps, JSON conversion, and the 256 KiB/64 KiB/65,535-member/32-container limits from approved Round 7.
- **Independent corpus:** six fixtures cover all permanent tags, signed/unsigned/floating numeric boundaries, and the exact 65,536-byte string boundary under both hash profiles; the complete corpus now contains 48 fixtures.
- **Failure proof:** malformed tags/lengths/scalars/UTF-8, small-u64 aliases, negative zero, NaN/infinity, array counts, duplicate or unordered map keys, trailing bytes, oversize values, excess members/depth, JSON duplicate keys, trailing JSON, and integer overflow are rejected. A red test exposed `serde_json` coercing an integer above `u64::MAX` to `f64`; lexical numeric preflight now rejects that class change before deserialization.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 25 tests in 0.40 seconds; strict standalone Clippy passed; fresh generation/verification passed all 48 fixtures; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Boundary:** this is an independent reference codec and fixture contract only. No production parser, serializer, semantic compiler, API, or persistent writer is active.

## P0b-2b-3b Invocation Policies

- **Red proof:** the 48-fixture campaign gate first failed with `P0b-2 definition format is absent from the contract registry: invocation-policy-v1`.
- **Frozen contracts:** exact 128-byte `InvocationPolicyV1` framing, native and WASM32 backend IDs, host profiles, semantic IDs, request/response/linear-memory/fuel/table limits, decoded-structure bounds, Store resource counts, meter limits, and reserve bytes from binding Round 9.
- **Independent corpus:** four fixtures cover native deterministic and pure WASM32 policies under both hash profiles; the complete corpus now contains 52 fixtures.
- **Failure proof:** exact-length/framing, semantic IDs, reserve bytes, native/WASM field applicability, host-profile coupling, finite corrected limits, 64 KiB WASM-page alignment, WASM32 address-space bounds, and zero/sentinel failures are covered. Every byte is rejected structurally or changes the enclosing ValueStore identity input.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 29 tests in 0.16 seconds; strict standalone Clippy passed; fresh generation/verification passed all 52 fixtures; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Boundary:** no executor, plugin ABI, production policy reader/writer, or persistent state changed. Parser candidates and mapper selectors remain writer-disabled until their child records are frozen.
