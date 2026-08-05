# Child 01 Progress: Format

- **Status:** P0b in progress; P0b-1 complete
- **Current landing unit:** P0b-2c-4 APOS logical position tokens
- **Entry commit:** `9eb503b1d8dbee62e2d90493ecf7010075bc2792`
- **Last green commit:** P0b-2c-3 index task artifacts `7096489c63128a95760848e76ec6a63357e795ed`
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
- **Next action:** freeze the common `GcArtifactV1` envelope and every bounded-mark, quarantine, sweep, reclaim, lifecycle, and audit body from Round 13 at both hash widths

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

## P0b-2b-4 Converter and Field-Index Definitions

- **Red proof:** after the 96-fixture ValueStore gate passed, the campaign gate failed with `P0b-2 definition format is absent from the contract registry: converter-definition-v1` before the semantic bundles or codecs existed.
- **Frozen contracts:** exact `ConverterDefinitionV1` and hash-width-dependent `FieldIndexDefinitionV1` framing; every corrected and migration-only converter ID; all six corrected and migration strategy families; permanent operation bits; accepted source masks; fixed-point-coordinate/recheck authority; semantic limits; and exact converter/strategy bundle fingerprints from Round 11.
- **Behavior bundles:** 37 checked-in bundles contain canonical specs, valid and invalid vectors, and named properties for 25 converters/adapters and 12 strategy semantics. The independent verifier recomputes all BLAKE3 fingerprints and rejects changed or missing bundle files. Legacy numeric, timestamp, floating-point, and string definitions preserve their captured range/max-length parameters rather than collapsing distinct v0 behavior.
- **Independent corpus:** 100 new fixtures cover every converter definition and its legal strategy binding under both 32- and 64-byte hash profiles, bringing the complete corpus to 196 fixtures.
- **Failure proof:** exact envelope/length/reserve checks, unknown IDs and type masks, corrected/migration semantic mixing, malformed fingerprints, wrong converter/strategy/operation/name combinations, zero/excess limits, corrected parameter injection, legacy parameter framing, and all-byte structural-or-identity mutation are covered. Reversed legacy ranges remain readable by design while corrected definitions cannot encode them.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 61 tests; strict standalone Clippy passed; fresh generation and verification passed all 196 fixtures; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Boundary:** this freezes identities and semantic closure only. No production converter, query compiler, index builder, persistent reader/writer, migration path, route, or database changed. Immutable index manifests remain the next dependency.
- **Post-gate correction:** the cross-round ID review caught an initial helper spelling of `aeordb.index.field-index-definition.v1\0`; the approved Round 5 domain is `aeordb.index.field-definition.v1\0`. A dedicated red/green domain test now pins that exact literal, and every AFIX fixture key was regenerated before any manifest or production reader consumed it.

## P0b-2b-5 Immutable Index Manifests

- **Red proof:** after the 196-fixture definition gate passed, the campaign gate failed with `P0b-2 immutable manifest lacks both hash-width fixtures: index:manifest:scope-catalog:` before any manifest body fixture existed.
- **Frozen contracts:** exact `ScopeCatalogManifestV1`, `ValueStoreManifestV1`, `FieldIndexManifestV1`, and non-authoritative `FieldNvtManifestV1` identities/bodies; immutable artifact keys; generation equality; codec/capability fields; Round 15 portable coverage names at the approved offsets; explicit root presence; count/high-water implications; and bounded embedded definitions from Round 6.
- **Independent corpus:** empty and populated variants of all four manifests under both 32- and 64-byte hash profiles add 16 fixtures, bringing the complete corpus to 212. The graph fixtures bind ValueStore to its exact ScopeCatalog artifact and ScopeId, FieldIndex to its exact ValueStore artifact and ValueStoreId, byte-identical correctness coverage across the chain, and NVT basis metadata as a non-GC hint.
- **Failure proof:** full-byte CRC mutation, envelope/identity generation disagreement, unknown capabilities, root/presence/count mismatch, owner/definition disagreement, codec/reserve/definition length failures, high-water/first/last rules, NVT power-of-two/divisibility/count bounds, and exact prior-closure references are covered.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 65 tests; strict standalone Clippy passed; fresh generation and verification passed all 212 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** manifests remain independent reference bytes only. No production artifact reader/writer, pointer selector, index cache, GC traversal, query path, migration, route, or database changed. Directory/page/journal/checkpoint bodies remain writer-disabled and are next.

## P0b-2c-1 Artifact Directories and Ordered Pages

- **Red proof:** with the manifest corpus green at 212 fixtures, the campaign gate now fails at `index:directory:scope-ordinal:` and requires all six correctness-bearing ordered directory roles plus all six ordered page/owner-role combinations under both hash widths. The seventh NVT-hint role is deliberately paired with the next NvtTile slice.
- **Frozen contracts:** the exact 80-byte ArtifactDirectory body and leaf/internal descriptors; all six correctness-bearing directory roles; the shared 96-byte ordered-page prefix; PostingPage, ValuePage, both ScopeCatalog directions, and both DocumentState owner classes; role-aware little-endian comparators; stable state stages/reasons; and immutable keys/birth generations.
- **Independent corpus:** 26 hand-constructed artifacts across 32- and 64-byte hashes add one page and leaf directory for every ordered role plus an internal posting directory, bringing the corpus to 238 fixtures. Every fixture byte is CRC protected and included in the immutable artifact key.
- **Cross-record proof:** leaf descriptors name the exact child artifact key and birth generation; internal posting descriptors name the exact child directory; owner/role/key-codec/page-kind combinations are fixed; and scope ordinal/reverse rows must form an exact live FileKey/document-ordinal bijection.
- **Failure and bounds proof:** repaired-CRC corruption tests reject wrong owner class, role codec, future child generation, fences, ranks, counts, record order, path/FileKey identity, state owner/stage/reason, and malformed typed values. Physical WAL locators are correctly treated as optional hints: partial/stale hints decode but fail the exact coalescing predicate.
- **Shared-codec correction:** `CanonicalSourceValueV1` now reuses the canonical structural decoder with its own 1 MiB typed-value bounds and permits typed `u64` across the full domain; it no longer inherits the 256 KiB config cap or JSON number canonicalization rule.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 72 tests; strict standalone Clippy passed; fresh generation and verification passed all 238 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** these remain independent reference bytes and validation oracles. No production page reader/writer, cache, planner, query, index mutation, migration, GC path, route, or database changed. NVT tiles and role 7 remain writer-disabled and are next.

## P0b-2c-2 NVT Tiles and Hint Directory

- **Red proof:** the 238-fixture ordered-page gate passes, then the campaign gate fails at the absent `index:nvt-tile:` fixture before any NVT tile constructor or decoder exists.
- **Frozen contracts:** exact IndexId/tile-start identity, 64-byte tile body, 40-byte sparse entries, fixed-point cell mapping, aligned power-of-two tile spans, basis generation, approximate-count sum, strict relative-cell order, and presence/zero PageId flags.
- **Independent corpus:** one populated sparse tile plus its exact role-7 ArtifactDirectory leaf under each hash width adds four fixtures, bringing the corpus to 242.
- **Hint-only proof:** lookup scans backward within the tile; no prior cell falls back to a predecessor tile or first posting page; and a missing, corrupt, out-of-range, or absent PageId always falls back to the pinned posting directory. Basis generations, PageIds, samples within the same cell, and approximate counts may change as identity-protected hint metadata without becoming corruption merely for changing.
- **Failure proof:** repaired-CRC mutations reject misaligned spans, bad resolution/count formulas, empty tiles, invalid flags/presence, unsorted/out-of-range cells, sample coordinates mapped to another cell, approximate-count disagreement, reserves, and identity/body start mismatch. Full-byte fixture mutation remains covered by the common immutable-artifact test.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 77 tests; strict standalone Clippy passed; fresh generation and verification passed all 242 fixtures; and the campaign gate passed with 93 routes and 36 docs.
- **Boundary:** NVT remains explicitly disposable acceleration state. No production query, cache, index builder, fallback scanner, migration, route, or database changed. Mutation journals and task checkpoints are next.

## P0b-2c-3 Mutation Journals and Index Task Checkpoints

- **Red proof:** after all 242 NVT fixtures pass, the campaign gate fails with `P0b-2 index task artifact lacks both hash-width fixtures: index:journal:task:` before a journal or checkpoint constructor/decoder exists.
- **Territory:** planned journal producers are async index mutation capture, reconciliation, migration, repair, and compaction; planned consumers are resumable task recovery, control publication, GC reachability, backup/replication, verify, and diagnostics. The current JSON `TaskQueue` checkpoint remains legacy runtime state and is not a v4 format implementation.
- **Frozen contracts:** exact `MutationJournalSegmentV1` identity/body/record framing, fixed system stream ID, reset and linked-chain rules, canonical mutation ordering, complete namespace-batch semantics, path/FileKey/revision presence coupling, and exact `IndexTaskCheckpointV1` task/state/phase registries, bounds, journal coverage, typed attachments, and node-local external descriptor.
- **Mechanical registry resolution:** Round 12 described validation reports and spill-run metadata as attachment roles but assigned no immutable artifact kinds to them. The permanent role registry therefore contains only typed edges to registered IndexArtifact kinds; validation status remains role-specific checkpoint resume state and spill-run metadata uses the explicit external descriptor. This avoids converting arbitrary hashes into GC roots.
- **Independent corpus:** task-owned and fixed-system mutation journals plus embedded and external task checkpoints under both hash profiles add eight fixtures, bringing the complete corpus to 250.
- **Failure proof:** repaired-CRC tests reject bad owner/codec/flags/identity boundaries, broken record lengths/order/batches/presence/path identities, unknown task/state/phase/capability/attachment roles, unsorted attachments, incoherent journal coverage/progress/lengths, and malformed external workspace/digest/path framing. Linked-journal tests require exact owner, generation, predecessor artifact, ordinal, root, and sequence continuity.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 86 tests in 5.33 seconds; standalone strict Clippy passed; fresh generation and verification passed all 250 fixtures; and `timeout 2m ./scripts/plan/check-v4-contracts.sh` passed with 93 routes and 36 docs.
- **Boundary:** these are independent reference bytes and validation oracles only. No production journal/checkpoint reader, writer, task queue, index coordinator, GC traversal, migration path, route, or database changed. APOS is next.

## P0b-2c-4 APOS Logical Position Tokens

- **Red proof:** with 250 prior fixtures green, the campaign gate fails with `P0b-2 APOS lacks both hash-width fixtures: position:directory-listing:` before the APOS module or fixture family exists.
- **Territory:** future producers are directory-listing, query, global-search, and aggregate pagination; consumers are root-aware planners plus HTTP, SDK, UI, and bot clients. APOS carries logical position only and cannot encode authorization, expiry, limits, offsets, pages, WAL offsets, NVT cells, manifests, or physical plans.
- **Frozen contracts:** exact canonical unpadded base64url spelling, 24 + 4H + T decoded framing, all four route IDs, hash-profile binding, order/root/FileKey/revision identities, CRC, complete tuple components, null/missing states, comparator tags 2 through 8, and the 1 MiB decoded/1,398,102-byte encoded preflight bounds.
- **Order definition:** `CanonicalRouteOrderDefinitionV1` reuses one bounded `CanonicalConfigValueV1` map with nine required keys. Its domain-separated fingerprint includes route, sort/direction/comparator rows, directories-first, collation, null/missing, multi-value, score, ties, and semantic fingerprints while excluding root and request-window/physical-plan parameters.
- **Independent corpus:** all four route kinds plus the exact 1 MiB decoded maximum under both hash profiles add ten fixtures, bringing the corpus to 260. Maximum tokens exercise all 32 components and a near-1 MiB bytes payload.
- **Failure proof:** tests reject malformed, padded, noncanonical, or overlong base64url; wrong magic/version/route/hash profile/length/count/flags; zero root/order/ties; CRC failures; malformed tuple lengths; unknown/reserved states/tags; invalid UTF-8, numeric, finite-f64, signed-zero, and bool payloads; and one byte beyond the hard cap. Unsigned-token context tests require exact route, root, order, resolved FileKey/revision, and recomputed tuple, returning the approved mismatch classes.
- **Green commands:** standalone `cargo test -j 4 --locked` passed 94 tests in 5.34 seconds (246,696 KiB maximum RSS); strict standalone Clippy passed; fresh generation/verification passed all 260 fixtures; and the campaign gate passed with 93 routes and 36 docs using the documented home-volume target override because `/tmp` was full. The final oracle review also added exact nine-key order-definition validation, including route, tie, policy, sort-row, fingerprint, unknown-key, and duplicate-key rejection without changing the valid fixture bytes.
- **Boundary:** this freezes only the independent public wire oracle. No production cursor decoder/encoder, route ordering, pagination, API schema, SDK, UI, authorization, query planner, or database changed. GcArtifact is next.
