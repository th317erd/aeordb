# Semantic compiler profile v1

This profile compiles explicitly versioned corrected source controls into exact
v1 semantic definitions. It does not interpret legacy controls, publish a root,
execute a plugin, or change retained definitions. Rounds 8 through 16 of the
AeorDB v4 decision contract govern the existing wire formats. This document
freezes the source-language and compiler choices added on September 14, 2026.
Any changed normative bytes or meaning require a different profile identity.

## Identity and corpus framing

For each registered database hash algorithm H, the profile identity is:

```
H(ASCII("aeordb.semantic-compiler-profile.v1") || 00
  || u64_le(len(SPEC.md))        || exact SPEC.md bytes
  || u64_le(len(invalid.bin))    || exact invalid.bin bytes
  || u64_le(len(properties.json))|| exact properties.json bytes
  || u64_le(len(vectors.bin))    || exact vectors.bin bytes)
```

The order is fixed. Filenames themselves are not added to the frame. Text uses
UTF-8 and LF, including the final newline. No build identity, timestamp, source
FileRecord identity, operational registry hash, or corpus fingerprint enters
the corpus. `fingerprints.json` contains the five independent expected hashes
outside the four hashed members. The producer slot is H-wide, not fixed at 32
bytes. Artifact/native/converter fingerprints retain their separate widths.

`vectors.bin` starts with ASCII `SCV1`; `invalid.bin` starts with `SCI1`.
Both continue with a u32LE record count and exactly that many records, each
u32LE byte length followed by UTF-8 JSON. There is no trailing padding. The JSON
record bytes, not a reformatted interpretation, participate in profile hashing.
Each valid record supplies name, owner input, optional registry source text,
optional index source text, a captured test module fingerprint byte, and exact
outputs for database algorithms 1 through 5. Source null means proven registry
absence or a registry-only test, never a source read failure. Outputs record
complete definition payloads, class, complete ID, field relationships and
dependency closure as lowercase hex. They omit physical envelopes/locators and
SemanticStateRoot to avoid self-reference. Invalid records supply source text,
registry-versus-index mode and the expected typed failure class.

The fixture module is test-only: kind 1, flags 4, artifact kind 1, length 123,
ID `/org/example/shared`, version `1.2.3`, repeated supplied fingerprint byte.
Parser role 1 uses ABI 3; mapper role 2 uses ABI 4; both use executor profile 2
and raw-module fingerprint semantics 1. Alias `second` adds one to the supplied
fingerprint byte; `missing` is absent; other fixture aliases resolve identically.
This is captured dependency evidence, not an executable module or permission
to skip real module verification at task admission.

## Strict ingress and ownership

Index source requires integer `$v: 1` and array `indexes`, including empty.
Optional top-level members are string `glob`, string `parser`, boolean `logging`,
string `compression`, byte-size string `parser_memory_limit`, and object
`parser_policies`. Unknown or duplicate members, trailing JSON, wrong types,
numeric coercions and null-as-omission fail. Logging/compression are excluded
from semantic projection; their operational validity belongs to the control
owner. Missing configuration files are resolved by the captured source owner;
this compiler never replaces a read failure or absence with defaults.

Parser registry source requires integer `$v: 1` and object `parsers`, with no
additional members. Keys are parameter-free MIME essences, normalized to ASCII
lowercase by corrected MIME rules; duplicates after normalization fail.
At most 512 entries are allowed. An alias is 1..4096 UTF-8 bytes without control
characters. A captured alias must resolve to the corrected parser role/ABI/
executor and a structurally valid complete dependency. Proven registry absence
and an explicit empty registry have the same empty canonical projection.
Alias failures, missing dependencies and operational refusal remain distinct.

The canonical registry projection is a CanonicalConfigValue map from normalized
MIME essence to Bytes containing the complete dependency record, ordered by raw
UTF-8 essence bytes. Alias spelling, map order and raw JSON formatting do not
enter its identity. Different roles of one module remain different definitions.

## Scope and fields

Owner paths use the established absolute-path normalization: remove NUL, trim
outer whitespace, collapse empty slash segments, discard `.`, resolve `..`
without escaping root, prepend `/`. An empty result is `/`. Source glob removes
empty slash segments but rejects dot/dot-dot segments, NUL and an empty result.
No glob means direct children. A glob means relative-path matching under the
existing ScopeDefinition v1 semantics. Empty index rows still produce a real
scope and projection, preserving nearer-scope masking.

Each row requires string `name` and `type`: a canonical corrected converter name
or a nonempty array of those names (permanent converter IDs 1..12). Optional
members are `source`, `source_limits`, `converter_limits`, `field_limits`.
There are no legacy converter aliases, min/max ranges or alternate field keys.
`@file_name` canonicalizes to `@filename`. Metadata fields use exactly their
registered fixed selector and forbid explicit source overrides. Unknown `@`
fields fail. Canonical field names have 1..4096 UTF-8 bytes and no NUL.

An omitted ordinary source means one source segment equal to the exact field
name, not dotted-path splitting. A source array preserves segment order.
Unsigned integer segments are full-u64 indices; an empty string is fan-out.
Slash-delimited valid regex strings use the frozen Regex selector, with `i`
enabling case-insensitivity. Invalid regex syntax is a literal key; a valid
pattern that exceeds compilation/encoding bounds is refusal, not a literal.
An empty array selects the root. No generic object/boolean/null source segment
coercion is allowed. Existing segment count, regex and selector byte caps apply.

A mapper source is an object with required alias string `plugin`, optional
`args`, and optional object `policy`. Omitted args is typed null. Arguments use
CanonicalConfigValue v1: raw-UTF8 map order, ordered arrays, canonical numeric
representation, duplicate-key rejection and its existing config bounds. The
mapper uses the captured role-2 dependency and corrected mapper contract 2.

Repeated rows with the same canonical field name merge only when their complete
ValueStore definitions agree. Conflicting source/context/limits fail. Field
indexes form a set ordered by complete raw IndexId; exact duplicates collapse.
Different complete bytes under the same complete ID are an operational collision
failure, never silently selected by insertion order. Fields sort by raw UTF-8.

## Parser, dependency and invocation policy

Metadata has no parser plan or dependency references beyond the canonical empty
table/none program. An unused explicit parser alias is not looked up or emitted;
syntactically invalid unused policy remains invalid. Ordinary fields select an
explicit captured parser if configured; otherwise they capture the canonical
registry tier, then raw JSON, then native suite. The automatic program pins the
MIME router. JSON selectors pin Regex; mappers pin their module. Full dependency
records deduplicate and sort by the existing v1 tuple before one-based ordinals
are assigned. No later mutable alias lookup changes these meanings.

`parser_policies` permits `wasm`, `raw_json`, `native_suite`; mapper `policy`
belongs only to its concrete call. Each is a partial object of the 14 numeric
InvocationPolicy v1 fields, excluding derived `kind`. Policy objects reject
unknown/duplicate keys and invalid integer types/widths. Explicit and registry
parsers use `wasm`; native tiers have distinct native policies.

Defaults and exact numeric field order are in `properties.json`. Native request,
linear memory, fuel, table elements and instance/memory/table counts are zero;
all common structural limits remain finite and nonzero. Pure WASM requires
nonzero finite limits, 64KiB-aligned linear memory, request/linear-memory at most
64MiB, response at most 16MiB and fuel at most 10,000,000. Existing codec integer
and intrinsic bounds apply. A default is not a new hard maximum unless specified.

`parser_memory_limit` retains the existing case-insensitive byte-size spelling
(bytes, kb, mb, gb; powers of 1024, surrounding whitespace allowed). It
sets only the WASM parser linear-memory field. If the explicit numeric spelling
is also present, they must agree; neither silently overrides the other.

Native dependencies bind the four already frozen MIME/router, raw-JSON,
native-suite and Regex conformance identities. The 37 converter/strategy bundles
remain separate and unchanged; their fingerprint-registry SHA-256 is
`65dc4781a5c7f0edeb1b76a2119e64abcbbfee6900244cfe71ed25550a3c19f3`.
An unavailable executor cannot be compiled as available. Retained definitions
remain structurally interpretable independently of producer-profile recognition;
this profile introduces no new rejection policy for historical reads.

## Definition limits, defaults and projection

The 13 optional definition-limit fields, numeric defaults and ceilings are in
`properties.json`. Applicable limits are positive and bounded. Metadata document
input and selector work/examined limits are canonical zero; mapper selector work/
examined limits are zero. Explicit nonzero values for inapplicable limits fail.
All corrected converters have empty parameter payloads and own their existing
strategy, type masks, semantic versions and behavior fingerprint.

The class-1 projection is exactly a canonical map with keys `fields` and
`scope_id`. ScopeId is Bytes. Fields is a canonical map from field name to a map
with keys `indexes` (array of distinct sorted complete IndexIds as Bytes) and
`value_store_id` (Bytes). Complete definition IDs bind all transitive meanings.
Raw control-file identities, logging, compression, source formatting and shared
operational budgets are excluded. Scope/value/index IDs and dependency-definition
IDs use selected database H and the already frozen class-specific domains.

New-database bootstrap explicitly supplies recursive `**/*` and all 12 fields:
the eight registered metadata fields with the Round 11 eight-value/thirteen-index
recipe, plus text (trigram), title (UTF-8 order and trigram), metadata.format
(UTF-8 order, source `["metadata","format"]`) and metadata.duration (finite f64
order, source `["metadata","duration_seconds"]`). These explicit nested sources
correct the old literal-key miss only for new v1 definitions, never retained v0.

## Operational boundary

Compilation reserves before memory-amplifying work, checks cancellation and live
admission, releases transient children, and retains output under a shared owner.
Source/workspace refusal cannot change semantic identity or publish authority.
Actual allocator refusal is not deterministic unindexability. This document
does not claim every generic allocator call is recoverable. Tests qualify the
explicit allocation and aggregate-child admission boundaries separately.

Task capture, module archival, full semantic catalog closure, conflict recheck,
atomic mixed mutation visibility and dependency-first root activation remain
the coordinated mutation owner's obligations. A valid profile fingerprint is
necessary provenance, not proof of a completed task or production readiness.
