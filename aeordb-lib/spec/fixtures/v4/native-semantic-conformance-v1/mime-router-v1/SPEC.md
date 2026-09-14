# MIME router v1

Native dependency: /org/aeordev/aeordb/native/mime-router-v1, role 3,
version 1.0.0, no cross-boundary ABI, native deterministic executor profile 1.

Round 9 media_type_essence_v1 trims leading/trailing ASCII SP and HTAB only.
It validates RFC 9110 media-type syntax, including parameters, and constrains
type/subtype to RFC 6838 restricted names of 1..127 ASCII bytes each. Their
first byte is ALPHA/DIGIT; continuation bytes are ALPHA/DIGIT or !#$&^_.+-.
ASCII lowercase type and subtype separated by one slash form the essence.
Parameters never participate in routing. Original stored MIME bytes remain
parser metadata. Missing, empty, or invalid MIME is generic, as is the valid
application/octet-stream essence.

Parameter names and unquoted values use RFC 9110 token characters, with no
whitespace around equals. SP/HTAB is allowed around semicolon delimiters; an
empty semicolon-delimited parameter is permitted by section 5.6.6. Quoted values
accept quoted-pair escaping and qdtext under section 5.6.4, including HTAB and
UTF-8 obs-text bytes. CR, LF, NUL, DEL and other forbidden control bytes reject
the complete media type even when escaped. A closing quote is required and
cannot be followed by undelimited text. Validation uses constant workspace;
only the at-most-255-byte essence is copied. Conformance checks every ASCII
parameter byte class plus quoted truncation and malformed delimiters.

For generic MIME only, native fallback uses the final nonempty filename suffix
after its last dot, ASCII-lowercased without Unicode folding or percent decoding.
Native format dispatch belongs to the separate native-suite dependency.

application/json and valid application subtypes ending in +json claim raw JSON.
The parser candidate program, not mutable deployment aliases, fixes precedence:
explicit parser; otherwise matched registry parser; otherwise raw JSON; native
suite last. A claimed failure stops; unclaimed input advances.

Migration exact_content_type_v0 instead preserves case/parameter-sensitive
matching, exact application/json handling and case-sensitive extension fallback
only for empty or exact application/octet-stream. Corrected input never silently
selects that behavior. The companion fixtures cover normalization, boundaries,
suffixes and JSON claims; legacy and whole-program precedence require execution
regressions as well.
