# Native semantic identities v1

Each component has a reviewed `SPEC.md` and an independent `fixtures.json`.
The canonical conformance manifest is this unambiguous byte sequence:

```text
ASCII("aeordb.native-semantic-conformance.v1") || 00
|| u64_le(SPEC.md byte length) || exact SPEC.md UTF-8 bytes
|| u64_le(fixtures.json byte length) || exact fixtures.json UTF-8 bytes
```

Files use LF line endings, including the final newline. The native dependency
fingerprint is BLAKE3-256 of that sequence, independent of database hash width.
The specification includes component ID, version, role and execution profile.
These two files are frozen semantic inputs: do not reformat them as routine
source cleanup. Source code, Cargo build output, timestamps, this README and
test harness implementation do not participate in the fingerprint.

Execution tests recompute identities independently and compare frozen literals.
They exercise the actual parser, MIME and selector owners, including complete
output vectors, limits, malformed inputs, ordering, legacy routing and retained
definition availability. Fingerprint agreement alone is not behavioral proof;
the campaign ledger records execution and platform qualification separately.

Historical byte-format fixtures retain their old placeholder fingerprints as
reader fixtures. Execution tests bind test-owned copies to the reviewed native
identities; no historical fixture or persisted database is silently rewritten,
and an unavailable old identity must not execute the current implementation.
