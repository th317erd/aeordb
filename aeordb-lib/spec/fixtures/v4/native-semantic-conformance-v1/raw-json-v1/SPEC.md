# Strict raw JSON v1

Native dependency: /org/aeordev/aeordb/native/raw-json-v1, role 1,
version 1.0.0, no cross-boundary ABI, native deterministic executor profile 1.

Corrected execution decodes one complete UTF-8 JSON value, allowing only JSON
whitespace around it and no trailing tokens. No BOM stripping is performed.
Duplicate object keys are rejected after escape decoding. Object ordering is
raw UTF-8 key order; array order is retained.

Null, booleans, strings, arrays and maps map to their canonical configuration
value types. In-range integers through i64::MAX use i64; larger nonnegative
integers through u64::MAX use u64. Floating values must be finite and cannot
encode negative zero at canonical JSON ingress. Expected canonical byte vectors
in fixtures.json are hand-authored and never regenerated from production.

Successful canonical JSON wins regardless of MIME. application/json and valid
application subtypes ending in +json claim input before probing; syntax, UTF-8,
duplicate-key and semantic errors therefore reject deterministically. Without
that MIME claim, ordinary syntax/UTF-8 mismatch is not_claimed. A recognized
JSON root with duplicate-key or canonical-value violation rejects instead of
falling through to native text. Structural/resource exhaustion is deterministic
whole-value rejection, not partial values or native fallback.

InvocationPolicyV1 controls complete output bytes, nodes, UTF-8 scalar/key bytes,
container members and depth. Effective depth is the minimum of structure depth,
value-stack height and recursion depth. Counters use checked arithmetic.
Cancellation and host allocation/admission failure remain operational errors.

Migration execution preserves serde v0 last-key-wins and syntax-fallthrough
behavior through an explicit migration plan; it does not redefine corrected
canonical values. Execution tests must protect both families separately.
