# Plugin identity readers — missing Round 9 prerequisite

Entry: `4a04be77838c12875435f1105e39b22a463236a2`. Direct implementation owner:
Codex. This extends U1's prerequisite inventory and Child01's reader/fixture
work; it does not change the already-ratified Round9 bytes or start deployment.
Execution/evidence remain in [ledger11](progress/11-user-facing-v4.md).

## Observed gap and ownership

The production plugin manager still uses JSON PluginRecord and live engine
lookups. Neither that representation nor an alias string is corrected parser
authority. The compiler's ParserAliasSnapshotV1 and
IndexConfigurationAliasSnapshotV1 traits have no production implementation.
Production and independent-reference source inventories contain no APAL or APWM
reader. The machine registry has24 fixture families/478 cases and omits both.
SystemFamily0x0031 reserves plugin-alias paths, but does not decode their bytes.

Round9 (decision conversation sections5–6) already freezes the following two
formats. Add structural readers and independent fixtures first. Preserve legacy
PluginRecord, all existing fixture meanings and the opaque/future dependency
retention rules. Do not make a codec result into an executable dependency.

Owned perimeter: new plugin identity reader/spec modules, their module/test
registrations, shared dependency text validation where necessary, independent
reference counterpart/tests, fixture/contract manifests, generated constants,
strict registry gates, progress and evidence. Preserve unrelated work and all
sealed evidence. No service route, plugin deployment, native publication, GC,
task writer, namespace activation or capability advertisement changes.

## APAL: PluginAliasRecordV1

Little endian; exactly128 header bytes, five strings, trailing CRC-32/ISO-HDLC
over every preceding byte. Total `128 + A + I + N + V + U + 4`, at most16,772.

| Offset | Field | Rule |
| --- | --- | --- |
| 0 | magic[4] | APAL |
| 4,6 | version/header u16 | 1/128 |
| 8 | total u32 | Exact complete byte length |
| 12 | flags u32 | bit0 version absent, bit1 author absent, bit2 opaque legacy ID; others zero |
| 16,20,24 | alias/ID/name lengths u32 | Each1..4096 |
| 28,32 | version/author lengths u32 | 0..256 / 0..4096; absence agrees with flags |
| 36,38 | plugin/artifact kinds u16 | WASM1 / raw-module1 |
| 40 | artifact fingerprint[32] | BLAKE3-256 of exact raw module bytes |
| 72 | artifact length u64 | 1..64MiB |
| 80,88 | created/updated i64 | Signed millisecond timestamps; do not invent extra time normalization |
| 96 | reserved[32] | Zero |
| 128 | strings | alias, ID, name, version, author; exact UTF-8 |

Alias bytes are case-sensitive, nonempty, without NUL/control characters. A
nonopaque ID uses the existing canonical absolute dependency-ID contract.
Present version is canonical SemVer. Display bytes are not silently trimmed,
normalized or substituted. An alias's canonical protected path is
`/.aeordb-system/plugin-aliases/<BLAKE3(alias UTF-8), lowercase hex>`; verify both
the hash and embedded alias rather than treating the path digest as authority.
Artifact and alias-name fingerprints intentionally remain fixed32-byte BLAKE3;
they are not the database-H FileRecord IDs or complete dependency catalog keys.

Legacy absence/opaque flags permit retained migration metadata, not corrected
execution. Manifest/alias metadata agreement and exact module digest/length
verification belong to the following captured-artifact binding owner.

## APWM: AeorPluginManifestV1 payload

Little endian; no additional checksum or envelope is invented. Exactly
`64 + I + N + V + A + 8R` bytes, bounded by12,672 from the frozen field ceilings.

| Offset | Field | Rule |
| --- | --- | --- |
| 0 | magic[4] | APWM |
| 4,6 | version/header u16 | 1/64 |
| 8 | total u32 | Exact complete payload length |
| 12 | flags u32 | Zero |
| 16,20,24,28 | ID/name/version/author lengths u32 | 1..4096,1..4096,1..256,0..4096 |
| 32 | role count u16 | 1..8; actual records must satisfy the closed role/ABI registry |
| 34 | reserved[30] | Zero |
| 64 | strings | Canonical absolute ID, display name, canonical SemVer, optional author |
| after strings | role records | role u16, ABI u16, zero flags u32; strictly ordered, no duplicates |

The only v1 pairs are parser1/ABI3 and mapper2/ABI4. The8-record framing ceiling
does not permit extra roles or duplicate records. Borrow fields/role records;
do not allocate a record collection from the claimed count.

This reader handles a payload, not a whole WASM module. The following module
owner must prove exactly one `aeordb.plugin.v1` custom section, bound/validate
module and section framing, compare archived bytes/digest/length and copied
alias metadata, and enforce current role/ABI/executor availability. A module
without the section can be legacy input only through its explicit adapter.
Other custom sections affect raw-module identity but add no capabilities.
Module execution, guest ABI codecs/SDK and deployment remain later runtime work.

## Metadata validation and independent proof

Reuse the canonical dependency-ID meaning. The existing production SemVer
check allocates a parsed version and formatted string; the reference check is
weaker (it checks the core triplet and trailing separators). Before changing
either, prove allocation behavior and a concrete invalid-prerelease divergence
with a failing target. Prefer one bounded borrowed production validator shared
by dependency and plugin readers, with the independently implemented reference
and semver crate as additional oracles. Preserve the accepted canonical set,
u64 core-number bounds and existing ADPT bytes/error classes; this is not a
SemVer normalization change. Do not infer reference correctness from agreement
on the existing small fixture set.

## Ordered verification and landing

1. Refresh consumer/registry/dependency inventory and pin the new entry revision.
   Write independent payload/path fixtures and positive/negative tests against
   fail-closed reader scaffolds. A compile failure does not count as behavioral RED.
2. Cover every truncation boundary, repaired-CRC alias field mutation, reserve,
   flags/kinds, arithmetic/count/length overflow, UTF-8/control/path/ID errors,
   SemVer prerelease/build/core limits, absent metadata, role/ABI pairing/order,
   exact canonical key and trailing bytes. Validate borrowing and measured
   bounded allocation. Include signed timestamp cases without unstated rules.
3. Implement only the readers and necessary shared metadata validation. Add
   reference families and registry rows without changing an old fixture's bytes
   or malformed meaning. Generate constants only from the reviewed registry;
   audit hash roles and reader-only capability boundaries before any writer use.
4. Run bounded narrow/spec/resource tests, all affected dependency/compiler/
   format/admission consumers, full library/reference suites, independent fixture
   checks, formatting, strict workspace Clippy, unchanged debt/audit gates, and
   matching native Mac tests. Native Windows stays an explicit pending gate;
   codec-only Linux/Mac green does not authorize writers or task activation.
5. Preserve actual REDs, exact inputs/locks/source archives and same-host binary
   identities before rebuild. Update ledger11 and land one coherent green unit.
   Then implement/prove the actual module capture/binding owner and durable
   complete-source enumeration/retention; payload-reader completion is not that
   integration or full U1 completion.

Heavy builds run on wyatt-desktop using the existing2-job/6GiB/no-swap guarded
runner and disk floors. Ordinary status checks are about5minutes apart; each
Cargo stage has a30-minute timeout. Source sync is manifest-scoped, checksum
based and excludes every target tree. Original host checkouts and the retained
corrupt database remain untouched.
