# Native parser suite v1

Native dependency: /org/aeordev/aeordb/native/native-suite-v1, role 1,
version 1.0.0, no cross-boundary ABI, native deterministic executor profile 1.

The suite owns text/code, HTML/XML, image, audio, video, PDF, MS Office
(DOCX/XLSX), and ODF (ODT/ODS). fixtures.json records the complete MIME and
extension dispatch table. text/x- is a prefix family; other MIME rules are
exact. Valid nongeneric unknown MIME suppresses extension fallback. Generic
MIME permits the pre-normalized final extension. Raw-JSON precedence belongs
to the surrounding parser plan and is not bypassed by native dispatch.

Corrected parsing preserves the supplied filename, original stored MIME, and
declared file size in metadata. Format metadata describes detected body format,
not an excuse to rewrite the stored MIME. The independent unit vectors use
declared size 1234 deliberately to verify that input's propagation; ordinary
storage integration separately verifies FileRecord/body size integrity.

Text removes a UTF-8 BOM, rejects invalid UTF-8, preserves line endings/text,
counts Unicode scalars, whitespace-separated words, and Rust-style text lines,
and uses the first nonempty trimmed line as title. Language detection retains
the existing exact content-type/filename metadata rules; routing normalization
does not rewrite parser-visible input.

HTML/XML extracts metadata and best-effort text, dropping comments, scripts,
styles and tags and decoding the existing entity set. Image/audio/video
metadata is best-effort: missing optional fields remain absent or null according
to the vectors, not invented values. PDF requires its magic and extracts the
existing best-effort text, version, pages and Info metadata. ZIP-based Office
and ODF parsing distinguishes required entries from absent optional metadata.

Corrected archive expansion, scalar output and structural limits are charged
before expansion/growth and reject the whole document. Claimed malformed input
is deterministic rejection; host admission/allocation and cancellation remain
operational failures. No input truncation or declared numeric field may cause
an indexing panic, unchecked arithmetic wrap, or a partial authoritative result.
All prefixes of the frozen seed bodies participate in that property.

Migration retains legacy malformed-native silent skip and legacy detected-MIME
metadata for Office/ODF. Current and migration plans are exercised separately.
The suite is not a validating general-purpose image/video/PDF decoder; metadata
omission for an incomplete best-effort media body is permitted, unwinding is not.
