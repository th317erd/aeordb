use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

const DOMAIN: &[u8] = b"aeordb.builtin-semantics.v1\0";
const BUNDLE_FILES: [&str; 4] = ["SPEC.md", "invalid.bin", "properties.json", "vectors.bin"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BundleKind {
  Converter,
  Strategy,
}

#[derive(Clone, Copy, Debug)]
pub struct BundleDescriptor {
  pub kind: BundleKind,
  pub id: u16,
  pub name: &'static str,
  pub corrected: bool,
  pub purpose: &'static str,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct FingerprintRegistry {
  schema_version: u8,
  domain: String,
  file_order: Vec<String>,
  bundles: Vec<FingerprintRow>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct FingerprintRow {
  kind: BundleKind,
  id: u16,
  name: String,
  corrected: bool,
  fingerprint_blake3: String,
}

pub fn converter_descriptor(id: u16) -> Option<BundleDescriptor> {
  let (name, corrected, purpose) = match id {
    0x0001 => ("typed_exact_blake3_v1", true, "typed structural equality candidate key with authoritative recheck"),
    0x0002 => ("bytes_binary_order_v1", true, "unsigned byte lexicographic order"),
    0x0003 => ("utf8_binary_order_v1", true, "raw UTF-8 byte order without locale collation"),
    0x0004 => ("u64_order_v1", true, "checked unsigned integer order"),
    0x0005 => ("i64_order_v1", true, "checked signed integer order"),
    0x0006 => ("f64_finite_order_v1", true, "finite IEEE-754 order with canonical signed zero"),
    0x0007 => ("timestamp_ms_order_v1", true, "strict RFC 3339 or integer UTC Unix milliseconds"),
    0x0008 => ("bool_order_v1", true, "false-before-true order"),
    0x0009 => ("unicode_trigram_v1", true, "AeorTextFoldV1 word and substring trigram expansion"),
    0x000a => ("soundex_ascii_v1", true, "Aeor Soundex v1 code expansion"),
    0x000b => ("double_metaphone_primary_ascii_v1", true, "Aeor Double Metaphone primary v1 code"),
    0x000c => ("double_metaphone_alt_ascii_v1", true, "distinct Aeor Double Metaphone alternate v1 code"),
    0x8001 => ("hash_v0", false, "captured legacy hash converter behavior"),
    0x8002 => ("u8_v0", false, "captured legacy u8 converter behavior"),
    0x8003 => ("u16_v0", false, "captured legacy u16 converter behavior"),
    0x8004 => ("u32_v0", false, "captured legacy u32 converter behavior"),
    0x8005 => ("u64_v0", false, "captured legacy u64 converter behavior"),
    0x8006 => ("i64_v0", false, "captured legacy i64 converter behavior"),
    0x8007 => ("f64_v0", false, "captured legacy f64 converter behavior"),
    0x8008 => ("string_v0", false, "captured legacy first-byte-plus-length string behavior"),
    0x8009 => ("timestamp_v0", false, "captured legacy timestamp fallback behavior"),
    0x800a => ("trigram_v0", false, "captured legacy trigram behavior"),
    0x800b => ("soundex_v0", false, "captured legacy Soundex behavior"),
    0x800c => ("dmetaphone_primary_v0", false, "captured legacy Double Metaphone primary behavior"),
    0x800d => ("dmetaphone_alt_v0", false, "captured legacy alternate-code fallback behavior"),
    _ => return None,
  };
  Some(BundleDescriptor { kind: BundleKind::Converter, id, name, corrected, purpose })
}

pub fn strategy_descriptor(id: u16, corrected: bool) -> Option<BundleDescriptor> {
  let name = match (id, corrected) {
    (1, true) => "exact",
    (2, true) => "ordered",
    (3, true) => "trigram",
    (4, true) => "soundex",
    (5, true) => "dmetaphone",
    (6, true) => "dmetaphone_alt",
    (1, false) => "exact_v0",
    (2, false) => "ordered_v0",
    (3, false) => "trigram_v0_strategy",
    (4, false) => "soundex_v0_strategy",
    (5, false) => "dmetaphone_v0_strategy",
    (6, false) => "dmetaphone_alt_v0_strategy",
    _ => return None,
  };
  let purpose = match id {
    1 => "eq and in candidate lookup with complete value recheck",
    2 => "typed equality, range, sort, and aggregate order",
    3 => "contains, similar, fuzzy, and match candidate expansion with recheck",
    4..=6 => "phonetic and match candidate expansion with recheck",
    _ => unreachable!(),
  };
  Some(BundleDescriptor { kind: BundleKind::Strategy, id, name, corrected, purpose })
}

pub fn all_descriptors() -> Vec<BundleDescriptor> {
  let mut values = (0x0001..=0x000c).filter_map(converter_descriptor).collect::<Vec<_>>();
  values.extend((0x8001..=0x800d).filter_map(converter_descriptor));
  values.extend((1..=6).filter_map(|id| strategy_descriptor(id, true)));
  values.extend((1..=6).filter_map(|id| strategy_descriptor(id, false)));
  values
}

pub fn fingerprint(descriptor: BundleDescriptor) -> [u8; 32] {
  let files = bundle_files(descriptor);
  let mut input = Vec::new();
  input.extend_from_slice(DOMAIN);
  input.extend_from_slice(&descriptor.id.to_le_bytes());
  for name in BUNDLE_FILES {
    let bytes = files.iter().find(|(candidate, _)| *candidate == name).expect("complete bundle").1.as_bytes();
    input.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    input.extend_from_slice(bytes);
  }
  *blake3::hash(&input).as_bytes()
}

pub fn generate(spec_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
  let root = semantics_root(spec_root);
  for descriptor in all_descriptors() {
    let directory = root.join(bundle_directory(descriptor));
    fs::create_dir_all(&directory)?;
    for (name, bytes) in bundle_files(descriptor) {
      fs::write(directory.join(name), bytes)?;
    }
  }
  fs::write(root.join("fingerprint-registry.json"), serde_json::to_vec_pretty(&expected_registry())?)?;
  Ok(())
}

pub fn verify(spec_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
  let root = semantics_root(spec_root);
  for descriptor in all_descriptors() {
    let directory = root.join(bundle_directory(descriptor));
    for (name, expected) in bundle_files(descriptor) {
      let actual = fs::read(directory.join(name))?;
      if actual != expected.as_bytes() {
        return Err(format!("semantic bundle differs from canonical source: {} {name}", descriptor.name).into());
      }
    }
  }
  let actual: FingerprintRegistry = serde_json::from_slice(&fs::read(root.join("fingerprint-registry.json"))?)?;
  if actual != expected_registry() {
    return Err("semantic fingerprint registry differs from canonical bundles".into());
  }
  Ok(())
}

fn semantics_root(spec_root: &Path) -> std::path::PathBuf {
  spec_root.join("semantics/v1")
}

fn bundle_directory(descriptor: BundleDescriptor) -> String {
  let family = match descriptor.kind {
    BundleKind::Converter => "converters",
    BundleKind::Strategy => "strategies",
  };
  format!("{family}/{}", descriptor.name)
}

fn bundle_files(descriptor: BundleDescriptor) -> Vec<(&'static str, String)> {
  let family = match descriptor.kind {
    BundleKind::Converter => "converter",
    BundleKind::Strategy => "strategy",
  };
  let stability = if descriptor.corrected { "corrected-v1" } else { "migration-only-v0-adapter" };
  vec![
    (
      "SPEC.md",
      format!(
        "# {}\n\n- family: {family}\n- permanent_id: 0x{:04x}\n- stability: {stability}\n- purpose: {}\n- authority: complete posting/value recheck; normalized coordinates are hints only\n- byte_order: little-endian except explicitly byte-comparable keys\n\n## Normative Behavior\n\n{}\n",
        descriptor.name,
        descriptor.id,
        descriptor.purpose,
        semantic_spec(descriptor)
      ),
    ),
    (
      "invalid.bin",
      format!(
        "AEOR-SEMANTICS-INVALID-V1\nname={}\n{}\n",
        descriptor.name,
        invalid_vectors(descriptor)
      ),
    ),
    (
      "properties.json",
      format!(
        "{{\n  \"schema_version\": 1,\n  \"name\": \"{}\",\n  \"properties\": [\"deterministic\", \"bounded\", \"locale_independent\", \"total_posting_order\", \"collision_safe_recheck\", \"no_coordinate_false_negative\"]\n}}\n",
        descriptor.name,
      ),
    ),
    (
      "vectors.bin",
      format!(
        "AEOR-SEMANTICS-VECTORS-V1\nname={}\n{}\n",
        descriptor.name,
        valid_vectors(descriptor)
      ),
    ),
  ]
}

fn semantic_spec(descriptor: BundleDescriptor) -> &'static str {
  match (descriptor.kind, descriptor.id, descriptor.corrected) {
    (BundleKind::Converter, 0x0001, true) => {
      "Posting key is source type tag followed by BLAKE3(\"aeordb.typed-exact-posting.v1\\0\" || complete CanonicalSourceValueV1). The key is candidate routing only; eq/in recheck complete typed values. Coordinate is the first eight bytes of BLAKE3(\"aeordb.index.exact-coordinate.v1\\0\" || complete key), interpreted big-endian."
    }
    (BundleKind::Converter, 0x0002, true) => {
      "Posting key is the exact bytes value. Compare unsigned bytes lexicographically, prefix-shorter first. Coordinate is the first eight key bytes interpreted big-endian and right-padded with zero."
    }
    (BundleKind::Converter, 0x0003, true) => {
      "Accept valid UTF-8 only. Posting key is its exact encoded bytes with no normalization or locale collation. Compare unsigned bytes lexicographically, prefix-shorter first. Coordinate uses the first eight key bytes big-endian, right-padded with zero."
    }
    (BundleKind::Converter, 0x0004, true) => {
      "Accept u64 or nonnegative i64 in range. Posting key is the u64 little-endian. Typed comparison decodes the key. Coordinate is the numeric u64 value."
    }
    (BundleKind::Converter, 0x0005, true) => {
      "Accept i64 or u64 at most i64::MAX. Posting key is two's-complement i64 little-endian. Typed comparison decodes the key. Coordinate is decoded bits XOR 0x8000000000000000."
    }
    (BundleKind::Converter, 0x0006, true) => {
      "Accept finite f64 and exactly round-tripping integers. Canonicalize -0.0 to +0.0 and store IEEE-754 bits little-endian. Reject NaN and infinities. Coordinate is the standard sortable transform: negative bits invert, nonnegative bits XOR the sign bit."
    }
    (BundleKind::Converter, 0x0007, true) => {
      "Accept an in-range integer millisecond value or strict RFC 3339 text with explicit Z or numeric offset. Reject naive dates, numeric strings, parse failure, and overflow. Store checked UTC Unix milliseconds as i64 little-endian and coordinate as sign-bit-flipped decoded bits."
    }
    (BundleKind::Converter, 0x0008, true) => {
      "Accept bool only. Posting key 00 is false and 01 is true. Coordinate is zero for false and u64::MAX for true."
    }
    (BundleKind::Converter, 0x0009, true) => {
      "Apply frozen Unicode lowercase with no normalization. Emit class 01 word trigrams from alphanumeric words padded with two leading spaces and one trailing space, then class 02 substring trigrams over the complete folded scalar sequence without padding or boundary removal. Deduplicate within one source ordinal in first-occurrence order. Coordinate hashes the complete class-prefixed token."
    }
    (BundleKind::Converter, 0x000a, true) => {
      "Tokenize Unicode alphanumeric runs, retain ASCII letters for Aeor Soundex v1, and emit class-prefixed nonempty four-character codes. Deduplicate first occurrence within one source ordinal."
    }
    (BundleKind::Converter, 0x000b, true) => {
      "Tokenize Unicode alphanumeric runs, retain ASCII letters, and emit the nonempty Aeor Double Metaphone v1 primary code. Deduplicate first occurrence within one source ordinal."
    }
    (BundleKind::Converter, 0x000c, true) => {
      "Tokenize Unicode alphanumeric runs and emit only a nonempty alternate Aeor Double Metaphone v1 code that differs from primary. No primary fallback is emitted."
    }
    (BundleKind::Converter, 0x8001, false) => {
      "Preserve HashConverter v0: inputs shorter than eight bytes map to scalar 0.0; otherwise the first eight bytes are big-endian u64 divided in f64 by u64::MAX. It is not order preserving."
    }
    (BundleKind::Converter, 0x8002..=0x8005, false) => {
      "Preserve the configured unsigned v0 range and big-endian input width. Short input maps to 0.0. Equal min/max maps every input to 0.5. Otherwise saturating(value-min)/(max-min) is evaluated in f64; reversed configured bounds remain captured rather than normalized."
    }
    (BundleKind::Converter, 0x8006, false) => {
      "Preserve I64Converter v0 configured range, big-endian input, short-input 0.0, equal-range 0.5, i128 shift followed by f64 division, and final [0,1] clamp, including reversed configured bounds."
    }
    (BundleKind::Converter, 0x8007, false) => {
      "Preserve F64Converter v0 configured range and big-endian input: short input and NaN map to 0.0, equal min/max maps to 0.5, arithmetic uses f64, and infinities/out-of-range values clamp to [0,1]."
    }
    (BundleKind::Converter, 0x8008, false) => {
      "Preserve StringConverter v0: empty maps to 0.0; scalar is clamp((first byte / 255)*0.7 + min(byte length/max_length,1)*0.3). max_length is the captured nonzero u32 parameter. This is not exact lexical order."
    }
    (BundleKind::Converter, 0x8009, false) => {
      "Preserve TimestampConverter v0 configured range and parsing order: exact eight bytes as big-endian i64, then RFC3339, naive seconds, naive fractional seconds, date-only UTC, numeric i64 text, and finally epoch zero. Normalize with the captured i64 range using f64 and clamp."
    }
    (BundleKind::Converter, 0x800a, false) => {
      "Preserve TrigramConverter v0 lowercase Unicode alphanumeric word splitting, two-leading/one-trailing space padding, first-occurrence deduplication, and BLAKE3 token scalar interpreted from the first eight digest bytes as little-endian u64/f64."
    }
    (BundleKind::Converter, 0x800b, false) => {
      "Preserve whitespace tokenization, Aeor Soundex v0, sorted/deduplicated codes, and BLAKE3 code scalar interpreted little-endian."
    }
    (BundleKind::Converter, 0x800c, false) => {
      "Preserve whitespace tokenization, Aeor Double Metaphone primary v0, sorted/deduplicated codes, and BLAKE3 code scalar interpreted little-endian."
    }
    (BundleKind::Converter, 0x800d, false) => {
      "Preserve whitespace tokenization and alternate v0 behavior: use alternate when present, otherwise fall back to primary; sort/deduplicate codes and use the legacy little-endian BLAKE3 scalar."
    }
    (BundleKind::Strategy, 1, true) => "Support eq/in only. Candidate keys never establish equality; recheck the complete typed source value in the pinned ValueStore generation.",
    (BundleKind::Strategy, 2, true) => "Support eq/in/gt/lt/inclusive-between/sort/aggregate. Compare complete typed posting keys; coordinates only narrow candidate pages and endpoint scans widen by predecessor/successor cells.",
    (BundleKind::Strategy, 3, true) => "Support contains/similar/fuzzy/match. Trigrams produce candidates only. Recheck folded complete text and score with the frozen Dice, OSA, or Jaro-Winkler rules.",
    (BundleKind::Strategy, 4..=6, true) => "Support phonetic/match. Codes produce candidates only and complete source text is rechecked under the exact requested available strategy.",
    (BundleKind::Strategy, 1, false) => "Preserve v0 hash candidate selection and exact raw-value recheck behavior.",
    (BundleKind::Strategy, 2, false) => "Preserve v0 scalar range candidate behavior and current raw-value/order recheck behavior, including scalar collisions.",
    (BundleKind::Strategy, 3, false) => "Preserve v0 trigram candidate and fuzzy recheck/scoring behavior.",
    (BundleKind::Strategy, 4..=6, false) => "Preserve the named v0 phonetic candidate, fallback, and recheck behavior.",
    _ => unreachable!("complete permanent semantic registry"),
  }
}

fn valid_vectors(descriptor: BundleDescriptor) -> &'static str {
  match (descriptor.kind, descriptor.id, descriptor.corrected) {
    (BundleKind::Converter, 0x0002, true) => "in=00ff;key=00ff;coordinate=00ff000000000000\nin=6162;key=6162;coordinate=6162000000000000",
    (BundleKind::Converter, 0x0003, true) => "in_utf8=41;key=41;coordinate=4100000000000000\nin_utf8=c3a9;key=c3a9;coordinate=c3a9000000000000",
    (BundleKind::Converter, 0x0004, true) => "in_u64=0;key=0000000000000000;coordinate=0000000000000000\nin_u64=18446744073709551615;key=ffffffffffffffff;coordinate=ffffffffffffffff",
    (BundleKind::Converter, 0x0005, true) => "in_i64=-9223372036854775808;key=0000000000000080;coordinate=0000000000000000\nin_i64=0;key=0000000000000000;coordinate=8000000000000000",
    (BundleKind::Converter, 0x0006, true) => "in_f64=-0;key=0000000000000000;coordinate=8000000000000000\nin_f64=1;key=000000000000f03f;coordinate=bff0000000000000",
    (BundleKind::Converter, 0x0007, true) => "in=1970-01-01T00:00:00Z;key=0000000000000000;coordinate=8000000000000000\nin=1970-01-01T01:00:00+01:00;key=0000000000000000;coordinate=8000000000000000",
    (BundleKind::Converter, 0x0008, true) => "in=false;key=00;coordinate=0000000000000000\nin=true;key=01;coordinate=ffffffffffffffff",
    (BundleKind::Converter, 0x0009, true) => "in=A.B;word=0120206120,0120206220;substring=02612e62\nin=aaaa;word=01202061,01206161,01616161,01616120;substring=02616161",
    (BundleKind::Converter, 0x000a, true) => "in=Robert;codes=R163\nin=Rupert;codes=R163\nin=Ashcraft;codes=A261",
    (BundleKind::Converter, 0x000b, true) => "in=empty;codes=\nin=Smith;codes=SM0",
    (BundleKind::Converter, 0x000c, true) => "in=Smith;codes=\nin=Schmidt;codes=SMTT",
    (BundleKind::Converter, 0x8001, false) => "in=;scalar_bits=0000000000000000\nin=ffffffffffffffff;scalar_bits=000000000000f03f",
    (BundleKind::Converter, 0x8002..=0x8009, false) => "capture=default-range;at_min=0.0;at_max=1.0;short=0.0;equal_range=0.5",
    (BundleKind::Converter, 0x800a..=0x800d, false) => "capture=production-v0-tokenizer-and-little-endian-digest-scalar;empty=no_tokens;duplicates=sorted_unique",
    (BundleKind::Strategy, _, _) => "empty=defined\nduplicate_documents=deduplicated_after_boolean_composition\ncoordinate_collision=complete-key-and-value-recheck",
    _ => "empty=defined\nboundary=min|max\nduplicate=preserve-source-ordinal",
  }
}

fn invalid_vectors(descriptor: BundleDescriptor) -> &'static str {
  match (descriptor.kind, descriptor.id, descriptor.corrected) {
    (BundleKind::Converter, 0x0003 | 0x0009..=0x000c, true) => "reject=invalid-utf8|resource-bound|fingerprint-mismatch",
    (BundleKind::Converter, 0x0004..=0x0007, true) => {
      "reject=wrong-type|malformed-width|overflow|precision-loss|nonfinite|ambiguous-time|resource-bound|fingerprint-mismatch"
    }
    (BundleKind::Converter, _, true) => "reject=unknown-type|malformed-width|resource-bound|fingerprint-mismatch",
    (BundleKind::Converter, _, false) => {
      "reject=unknown-adapter|malformed-parameter-framing|resource-bound|fingerprint-mismatch;legacy-data-fallbacks-remain-valid"
    }
    (BundleKind::Strategy, _, _) => {
      "reject=unsupported-operation|wrong-converter|incomplete-closure-without-fallback|resource-bound|fingerprint-mismatch"
    }
  }
}

fn expected_registry() -> FingerprintRegistry {
  FingerprintRegistry {
    schema_version: 1,
    domain: String::from_utf8_lossy(DOMAIN).into_owned(),
    file_order: BUNDLE_FILES.iter().map(|name| (*name).to_string()).collect(),
    bundles: all_descriptors()
      .into_iter()
      .map(|descriptor| FingerprintRow {
        kind: descriptor.kind,
        id: descriptor.id,
        name: descriptor.name.to_string(),
        corrected: descriptor.corrected,
        fingerprint_blake3: hex::encode(fingerprint(descriptor)),
      })
      .collect(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registry_is_complete_unique_and_stable() {
    let descriptors = all_descriptors();
    assert_eq!(descriptors.len(), 37);
    let mut names = descriptors.iter().map(|row| row.name).collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), descriptors.len());
    assert!(descriptors.iter().all(|row| fingerprint(*row) != [0; 32]));
  }

  #[test]
  fn fingerprint_binds_every_file_and_permanent_id() {
    let descriptor = converter_descriptor(1).unwrap();
    let baseline = fingerprint(descriptor);
    let changed_id = BundleDescriptor { id: 2, ..descriptor };
    assert_ne!(baseline, fingerprint(changed_id));
    let strategy = strategy_descriptor(1, true).unwrap();
    assert_ne!(baseline, fingerprint(strategy));
  }
}
