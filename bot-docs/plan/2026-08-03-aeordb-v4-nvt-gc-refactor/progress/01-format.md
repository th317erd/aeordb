# Child 01 Progress: Format

- **Status:** P0b in progress; P0b-1 complete
- **Current landing unit:** P0b-2b-4 ConverterDefinitionV1 and FieldIndexDefinitionV1
- **Entry commit:** `9eb503b1d8dbee62e2d90493ecf7010075bc2792`
- **Last green commit:** P0b-2b-3e SourceSelectorV1 `3c9e67189293ce3b72ea6fd0d4edc3be5af5b383`
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
- **Next action:** freeze ConverterDefinitionV1 and FieldIndexDefinitionV1, then their immutable manifest families in dependency order

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
- **Independent corpus:** six fixtures cover native deterministic, pure WASM32, and migration-only legacy-stub WASM32 policies under both hash profiles. The legacy profile was added when parser-plan composition exposed that the initial policy corpus rejected a host profile permitted by Round 9.
- **Failure proof:** exact-length/framing, semantic IDs, reserve bytes, native/WASM field applicability, host-profile coupling, finite corrected limits, 64 KiB WASM-page alignment, WASM32 address-space bounds, and zero/sentinel failures are covered. Every byte is rejected structurally or changes the enclosing ValueStore identity input.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 29 tests in 0.16 seconds; strict standalone Clippy passed; fresh generation/verification passed all 52 fixtures; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Boundary:** no executor, plugin ABI, production policy reader/writer, or persistent state changed. Parser candidates and mapper selectors remain writer-disabled until their child records are frozen.

## P0b-2b-3c Dependency Tables

- **Red proof:** the 52-fixture campaign gate first failed with `P0b-2 definition format is absent from the contract registry: dependency-table-v1`.
- **Frozen contracts:** exact `DependencyTableV1` and 96-byte record framing; executable kind, role, ABI, executor, fingerprint, artifact, flag, namespace-ID, SemVer, canonical ordering, deduplication, ordinal, and 256 KiB/1,024-record bounds from Round 9.
- **Independent corpus:** six fixtures cover the canonical empty table, a native parser-resolution dependency, and a corrected WASM mapper dependency under both hash profiles; the complete corpus now contains 58 fixtures.
- **Failure proof:** table/record framing, checked lengths/counts, reserves, ordering/duplicates, flags, canonical IDs/versions, nonzero fingerprints, artifact requirements, and native/WASM role/ABI/executor combinations fail closed.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 33 tests in 0.17 seconds; strict standalone Clippy passed; fresh generation/verification passed all 58 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** dependency records describe immutable executor identity only. No plugin archive, alias, executor, mutable registry, production reader/writer, or persistent database path changed.

## P0b-2b-3d Parser Resolution Plans

- **Red proof:** the 58-fixture campaign gate first failed with `P0b-2 definition format is absent from the contract registry: parser-resolution-plan-v1` before the parser-plan module or fixtures existed.
- **Frozen contracts:** exact 48-byte `ParserResolutionPlanV1` header and 32-byte candidate framing; canonical none, explicit-plugin, and automatic plan shapes; corrected/migration semantic families; registry/raw-JSON/native tier order; dependency ordinals; nested invocation policies; normalized MIME matching; and the 128 KiB/512-registry-candidate bounds from Round 9.
- **Independent corpus:** eight parser fixtures cover none, corrected explicit plugin, corrected automatic registry resolution, and migration-only automatic resolution under both hash profiles. Two additional policy fixtures close the previously omitted migration-only WASM legacy-stub host profile, bringing the complete corpus to 68 fixtures.
- **Failure proof:** plan/candidate framing, checked lengths/counts, reserves, kind/semantic applicability, zero ordinals, nested policy/backend mismatches, corrected-versus-legacy mixing, malformed/uppercase/reserved `application/json` MIME matches, registry duplicate/order failures, exact 512-entry acceptance and 513-entry rejection, and all fixture-byte integrity/identity effects are covered.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 40 tests in 0.18 seconds; strict standalone Clippy passed; fresh generation/verification passed all 68 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** candidate ordinals are structurally validated here. Their exact dependency role/ABI/executor/artifact compatibility is validated only when the enclosing ValueStore definition supplies its dependency table. No production parser registry, executor, reader/writer, or persistent database path changed.

## P0b-2b-3e Source Selectors

- **Red proof:** the 68-fixture campaign gate first failed with `P0b-2 definition format is absent from the contract registry: source-selector-v1` before the selector module or fixtures existed.
- **Frozen contracts:** exact 32-byte `SourceSelectorV1` header; metadata IDs, JSON path segment framing, canonical migration-only always-missing form, corrected and legacy plugin-mapper payloads, nested canonical arguments/policies, and 4 KiB/1,024-segment limits from corrected Round 8A.
- **Independent corpus:** fourteen fixtures cover metadata `@hash`, root JSON, every JSON segment tag, corrected/legacy mapper contracts, always-missing migration, and the exact 4,096-byte boundary under both hash profiles; the complete corpus now contains 82 fixtures.
- **Failure proof:** selector/segment framing, checked lengths/counts, reserves, unknown/inapplicable kinds and semantics, metadata IDs, empty/invalid UTF-8 keys, segment flags/tags, malformed regex, nested config/policy failures, zero mapper ordinals, mapper-policy host mismatch, exact-cap acceptance, oversize rejection, and all fixture-byte integrity/identity effects are covered. AeorRegexV1 is pinned to `regex 1.12.3` with default Unicode features and has search, case-folding, Unicode-class, malformed-pattern, and unsupported-lookaround conformance tests.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 47 tests in 1.49 seconds; strict standalone Clippy passed; fresh generation/verification passed all 82 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** selector-local structure and nested config/policy bytes are authoritative here. Field/plan agreement and exact mapper dependency role/ABI/executor/artifact compatibility remain the enclosing ValueStore definition's responsibility. No production selector evaluator, mapper executor, reader/writer, or persistent database path changed.

## P0b-2b-3f ValueStore Definitions

- **Red proof:** the 82-fixture campaign gate first failed with `P0b-2 definition format is absent from the contract registry: value-store-definition-v1` before the parent codec or fixtures existed.
- **Frozen contracts:** exact hash-width-dependent `ValueStoreDefinitionV1` envelope/body and ValueStoreId domain; field/selector/parser/dependency child framing; corrected and migration semantic families; source/document/traversal limits; metadata/ordinary field agreement; and exact parser, MIME-router, raw/native parser, mapper, and selector dependency roles from corrected Rounds 8A/9.
- **Independent corpus:** fourteen fixtures cover corrected and legacy metadata, corrected JSON and mapper, migration JSON and mapper, and canonical migration-only always-missing definitions under both hash profiles; the complete corpus now contains 96 fixtures.
- **Failure proof:** envelope/formula/cap/reserve/scope/field failures, child allocation amplification, nested decoder errors, corrected/legacy semantic mixing, inapplicable/unbounded limits, field-selector-parser disagreement, out-of-range and cross-role ordinals, corrected/legacy policy mismatch, duplicate selector capabilities, unused dependencies, and all fixture-byte integrity/identity effects are covered.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 54 tests in 5.36 seconds; strict standalone Clippy passed; fresh generation/verification passed all 96 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** this completes the independent ValueStore semantic closure only. No production codec, parser/selector executor, index runtime, writer, migration, route, or database path changed.
