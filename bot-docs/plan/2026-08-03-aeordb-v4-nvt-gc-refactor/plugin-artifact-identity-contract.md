# Captured plugin artifact identity — U1 prerequisite

Entry77e15019ead2bd5b45835777dfd090fc38e8144c. Direct implementation owner:
Codex. Continues the ratified Round9 artifact/alias contract and
[ledger11](progress/11-user-facing-v4.md), after the qualified APAL/APWM readers.
No new persistent bytes, capability, service writer or deployment is introduced.

## Territory and evidence boundary

`plugin_identity.rs` validates borrowed metadata, not the actual module. The
parser-registry and index-configuration compiler snapshot traits currently have
no production implementation. `index_native_parser.rs` explicitly returns WASM
dependency-unavailable; `index_source.rs` requires a supplied mapper executor.
The existing `plugins/wasm_runtime.rs` is the separate legacy host-function/handle
runtime and must not silently become corrected parser/mapper profile2.

Pinned local source inspection: wasmi0.42.1 already depends on wasmparser0.227.1.
The latter's Parser checks outer section framing and returns lazy section/body
readers; merely consuming `parse_all` is **not full bytecode validation**.
`wasmi::Module::validate` validates function bodies but neither proves translation
nor provides the required cooperative cancellation/shared-memory accounting.
Reuse the pinned parser for bounded identity inspection, not a homegrown general
WASM decoder. Full profile2 validation/compilation, pure imports, ABI exports,
module-owned memory and finite execution policy belong to executor admission.

The new private-field result is **PluginArtifactIdentityV1**, not an executable
module, admitted dependency, captured-source pin or activation permit. It must
not implement either compiler snapshot trait or construct DependencyRecordV1.
A following executor owner must validate actual bytecode/profile/exports before
constructing an executable dependency; source/GC owners must separately retain
the exact archive and mutable-alias snapshot through task publication/recovery.

## Input and equality contract

The caller supplies immutable borrowed alias bytes, alias protected path, raw
module bytes and artifact protected path, plus operational byte/workspace limits,
the shared memory coordinator and cancellation callback. Upstream read failures
are errors, never missing/default module bytes. This pure helper performs no I/O,
archive creation, alias replacement, selected-root change or guest execution.

1. Validate APAL with the existing codec and exact alias key. Version-absent or
   opaque-legacy alias flags require the explicit legacy adapter; do not invent
   corrected defaults. Author absence remains valid when manifest agrees.
2. Require raw bytes1..64MiB and exact APAL artifact length. A smaller caller
   operational ceiling yields Resource, not corrupt persisted data.
3. Hash **all** raw bytes with BLAKE3-256 in at most64KiB updates. Verify exact
   APAL fingerprint and `/.aeordb-system/plugin-artifacts/blake3/<64 lowercase
   hex>` key. This is fixed32-byte artifact identity, not a database-H key.
4. Traverse the complete core-module outer envelope with the pinned standard
   parser. Require module encoding/version1; reject unknown outer sections,
   truncation, invalid section/name lengths, invalid custom-name UTF-8 and
   trailing framing errors. Do not claim lazy core section entries or bytecode
   were validated by this traversal.
5. Require exactly one custom section named `aeordb.plugin.v1`; decode its payload
   with APWM. Other custom sections remain opaque and contribute to raw identity.
   Compare copied ID, name, version and optional author byte-for-byte with APAL.
   Preserve role/ABI pairs from the manifest without claiming installed support.

Borrow the module and metadata; never clone a64MiB source, allocate a section
collection or retain more than the single manifest view. Reserve a fixed bounded
workspace before hash/parser work and retain only the small result's lease after
return. Captured input buffers/pins keep their separate caller-owned admission.
Check cancellation and current admission at every hash chunk and parser event,
including before returning. Every error releases this helper's reservation.
Preserve source error details through the established SemanticCompilationErrorV1
categories. Generic diagnostic-string allocation is not promised universally
fallible; successful inspection must allocate no input-sized memory.

Implementation workspace is16KiB, with a compile-time size check covering the
hasher, parser, payload, result and4KiB extra scratch; the result retains its
`size_of` charge only. Tests separately measure zero successful heap allocations
for small modules,1MiB opaque data and20,000 custom sections. Source-buffer
accounting is explicitly separate. Core-only traversal rejects the component
header before a nested component parser can grow its stack. Test fixtures also
include a correctly framed identity with invalid lazy bytecode: admission MUST
still be performed by the following executor owner.

The direct parser dependency is pinned `=0.227.1`, default features disabled.
The existing wasmi dependency already selects this exact package; the reviewed
root lock delta adds only that edge to aeordb, not another package or version.
An all-platform offline metadata attempt reported a missing cached Windows-only
package; Linux-filtered offline/locked metadata succeeded. This is not Windows
build evidence and does not relax its pending native gate.

## Falsifying tests and landing

Before implementation, fail positive identity/borrowing/resource cases against
a fail-closed scaffold. Construct whole-module envelopes independently using
literal section framing and frozen reference payloads; use WAT/wasmi as additional
test-side checks for valid core modules, not a production serializer oracle.

Test unique/missing/duplicate manifests; every truncation; repaired alias CRCs;
wrong module length/digest/path; every copied metadata mismatch; changed opaque
custom sections; alias replacement versus old retained artifact; UTF-8 and
overflowing section/name lengths; component/wrong-version input; operational
limits/cancellation/admission/retry/release; zero-copy views and measured bounded
workspace. Preserve the absence of executor/snapshot/writer authority explicitly.
Exercise at least parser, mapper and both-role manifests. Validate exact maximum
module length with a bounded opaque section without writing a large test DB.

Run reader/resource, legacy runtime and affected compiler/format/admission tests,
full library/reference, format, strict Clippy, contracts, audit/debt, matching Mac
tests and source-bound evidence. Windows remains pending and gates new runtime
writers; do not replace its native proof with Linux/Mac results. Heavy work stays
on the existing guarded desktop runner, two jobs/6GiB/no swap, existing disk
floors,30-minute stages and ordinary checks about5minutes apart. Sync exact source
manifests only; never target trees. Preserve all earlier proofs and failure data.

The legacy `wasm_query_e2e_spec` target requires the checked-in echo plugin to
be built first on each qualification host. In the isolated source, its ignored
`aeordb-plugins/echo-plugin/target` is a symlink to that host's test target area
(Data on Linux, user cache on Mac); it is never synchronized. With the exact
fixture lock and installed `wasm32-unknown-unknown` target, the guarded command
is `cargo build --offline --locked -j 2 --release --target
wasm32-unknown-unknown --manifest-path aeordb-plugins/echo-plugin/Cargo.toml`,
using the dedicated echo target through `CARGO_TARGET_DIR` (one job on Mac).
Keep source and same-host fixture digests in the evidence. A missing fixture is
a failing setup gate, not a skipped test or permission to remove the E2E target.

## Verified outcome, September16

The [executed proof](evidence/user-facing-v4-u1-plugin-artifact-proof-20260916.json)
binds initial failing-first evidence, both hosts' missing-fixture failures and
their subsequent green runs. Linux:161 narrow,520 affected,713 library and181
reference tests,496 fixtures and all static gates. Mac:292 production and181
reference tests plus contracts. Zero successful inspector heap allocation is
measured, not inferred from borrowed signatures. No prior format/fixture changes
or new error-suppression exemptions were needed. Native Windows remains pending;
this is not executable-plugin, durable-capture or full-runtime qualification.
