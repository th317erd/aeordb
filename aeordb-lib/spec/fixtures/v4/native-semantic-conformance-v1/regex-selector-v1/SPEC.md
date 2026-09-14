# AeorRegexV1 selector

Native dependency: /org/aeordev/aeordb/native/aeor-regex-v1, role 4,
version 1.0.0, ABI zero, native deterministic executor profile 1.

The typed SourceSelectorV1 owns segment kinds; this executor does not parse
source aliases, regex-looking configuration strings, or legacy path coercions.
An empty path selects the parsed root. Exact object keys use raw UTF-8 without
normalization. Numeric indexes retain full u64 precision, use checked array
indexing, and use canonical unsigned decimal keys against objects. Fan-out
uses array order or increasing raw UTF-8 object-key order. Nested traversal is
depth-first in prior-candidate order. Missing emits no values; null, duplicate
values, empty strings and empty containers remain present and distinct.

Regex is search matching, not implicit full matching. The explicit selector
flag is case-insensitive; the pattern body has the frozen Rust regex syntax.
Object candidates match their keys, never their values. Array strings match
their contents without JSON quotes. Other JSON candidates match compact JSON
with raw UTF-8 map-key order and canonical escaping. Byte strings have no JSON
representation and cause deterministic whole-document rejection if inspected
as array-regex candidates. No lossy conversion or skipped candidate is allowed.

The production dependency baseline is regex 1.12.3, regex-automata 0.4.14,
regex-syntax 0.8.10, Unicode 16.0.0. Regex uses std, perf and unicode defaults:
perf-backtrack/cache/dfa/inline/literal/onepass and
unicode-age/bool/case/gencat/perl/script/segment. Compiled regex and DFA limits
are each 1,048,576 bytes. A dependency upgrade must emulate this permanent
capability or use a different semantic identity. The independent reference
has separately resolved transitive versions; agreement does not authorize
quietly changing this production baseline.

Charge one work item per candidate/segment evaluation and one per inspected
fan-out or regex child. Terminal serialization does not evaluate another
segment. Regex additionally charges complete examined UTF-8 key or array
candidate-text bytes, including JSON punctuation/escaping when applicable.
Charge and bounds-check candidate text before allocating it. Checked counters,
source-value count, complete canonical-value bytes, and document-input limits
apply to the whole document. A late limit failure discards every earlier value.

Cancellation and unavailable executors are operational errors, not missing or
durable unindexable results. Definitions and already materialized values remain
structurally usable without this executor. Admission uses the shared runtime
memory owner. Conformance tests must exercise direct extraction and the shared
producer/query evaluator; a constructor-only availability check is insufficient.

In fixtures.json, numeric segment values are decimal strings to avoid a JSON
loader narrowing u64. Expected values are ordered complete typed JSON values,
not a set. A missing result is an empty expected_values array, distinct from
one expected null. Limit cases must run their exact limit and one smaller
limit with the named error code, proving no partial output. Each case runs at
both database hash widths. Invalid patterns must fail selector construction;
literal-key fallback is a separate configuration-compiler contract.
