mod config;
mod core;
mod definitions;
mod dependency;
mod field_index;
mod index;
mod index_pages;
mod parser;
mod policy;
mod selector;
mod semantics;
mod value_store;

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::path::Component;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use config::ConfigFormat;
use core::{CoreFormat, HashProfile};
use dependency::DependencyFormat;
use field_index::FieldIndexFormat;
use definitions::DefinitionFormat;
use index::IndexFormat;
use parser::ParserFormat;
use policy::PolicyFormat;
use selector::SelectorFormat;
use value_store::ValueStoreFormat;

const CAMPAIGN_ID: &str = "aeordb-v4-nvt-gc-2026-08-03";
const TOOL_REVISION: &str = "p0b2-ordered-pages-v1";
const SLOT_LENGTH: usize = 1_024;
const HEADER_REGION_LENGTH: usize = SLOT_LENGTH * 2;
const CRC_OFFSET: usize = 1_020;
const INITIAL_CAPABILITIES: &[u8; 32] = &[
  0x7f, 0x00, 0x6c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

type DynResult<T> = Result<T, Box<dyn Error>>;

#[derive(Clone, Copy)]
enum FixtureFormat {
  DatabaseHeaderV4,
  Config(ConfigFormat),
  Core(CoreFormat),
  Dependency(DependencyFormat),
  Definition(DefinitionFormat),
  FieldIndex(FieldIndexFormat),
  Index(IndexFormat),
  Parser(ParserFormat),
  Policy(PolicyFormat),
  Selector(SelectorFormat),
  ValueStore(ValueStoreFormat),
}

impl FixtureFormat {
  fn id(self) -> &'static str {
    match self {
      Self::DatabaseHeaderV4 => "database-header-v4",
      Self::Config(format) => format.id(),
      Self::Core(format) => format.id(),
      Self::Dependency(format) => format.id(),
      Self::Definition(format) => format.id(),
      Self::FieldIndex(format) => format.id(),
      Self::Index(format) => format.id(),
      Self::Parser(format) => format.id(),
      Self::Policy(format) => format.id(),
      Self::Selector(format) => format.id(),
      Self::ValueStore(format) => format.id(),
    }
  }

  fn family(self) -> &'static str {
    match self {
      Self::DatabaseHeaderV4 => "DatabaseHeaderV4",
      Self::Config(format) => format.family(),
      Self::Core(format) => format.family(),
      Self::Dependency(format) => format.family(),
      Self::Definition(format) => format.family(),
      Self::FieldIndex(format) => format.family(),
      Self::Index(format) => format.family(),
      Self::Parser(format) => format.family(),
      Self::Policy(format) => format.family(),
      Self::Selector(format) => format.family(),
      Self::ValueStore(format) => format.family(),
    }
  }
}

#[derive(Clone)]
struct HeaderFields {
  profile: HashProfile,
  sequence: u64,
  updated_at_ms: u64,
  physical_instance_id: [u8; 16],
  extra_reader_capability_bit: Option<usize>,
  extra_writer_capability_bit: Option<usize>,
  nonzero_reserved: bool,
  nonzero_hash_padding: bool,
}

impl HeaderFields {
  fn canonical(profile: HashProfile, sequence: u64) -> Self {
    Self {
      profile,
      sequence,
      updated_at_ms: 1_700_000_000_000 + sequence,
      physical_instance_id: sequence_bytes(0xa0),
      extra_reader_capability_bit: None,
      extra_writer_capability_bit: None,
      nonzero_reserved: false,
      nonzero_hash_padding: false,
    }
  }
}

#[derive(Clone)]
struct FixtureCase {
  id: &'static str,
  format: FixtureFormat,
  profile: HashProfile,
  expected: &'static str,
  relation: Option<&'static str>,
  canonical_key: Option<String>,
  bytes: Vec<u8>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FixtureManifest {
  schema_version: u8,
  campaign_id: String,
  stage: String,
  reference_tool: ReferenceTool,
  contract_registry: String,
  fixture_count: usize,
  fixtures: Vec<FixtureManifestEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ReferenceTool {
  name: String,
  revision: String,
  production_dependencies: Vec<String>,
  provenance: String,
  reviewer_status: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct FixtureManifestEntry {
  id: String,
  format_id: String,
  family: String,
  hash_algorithm: String,
  hash_width: usize,
  binary: String,
  annotated_hex: String,
  byte_length: usize,
  sha256: String,
  blake3: String,
  canonical_key: Option<String>,
  expected: String,
  relation: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ResultLedger {
  schema_version: u8,
  campaign_id: String,
  tool_revision: String,
  results: Vec<ResultLedgerEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ResultLedgerEntry {
  fixture_id: String,
  expected: String,
  observed: String,
  result: String,
}

#[derive(Debug, PartialEq, Eq)]
struct SelectedSlot {
  sequence: u64,
  redundancy_degraded: bool,
}

fn main() -> DynResult<()> {
  let mut args = env::args().skip(1);
  let command = args.next().ok_or("usage: aeordb-v4-reference <generate|verify> <fixture-root>")?;
  let fixture_root = PathBuf::from(args.next().ok_or("missing fixture root")?);
  if args.next().is_some() {
    return Err("unexpected extra argument".into());
  }

  match command.as_str() {
    "generate" => generate(&fixture_root),
    "verify" => verify(&fixture_root),
    _ => Err(format!("unknown command: {command}").into()),
  }
}

fn generate(fixture_root: &Path) -> DynResult<()> {
  let spec_root = fixture_root.parent().and_then(Path::parent).ok_or("fixture root must be below spec/fixtures")?;
  semantics::generate(spec_root)?;
  let cases = fixture_cases();

  let mut entries = Vec::with_capacity(cases.len());
  let mut results = Vec::with_capacity(cases.len());
  for case in &cases {
    let fixture_directory = fixture_root.join(case.format.id());
    fs::create_dir_all(&fixture_directory)?;
    let binary_rel = format!("{}/{}.bin", case.format.id(), case.id);
    let annotation_rel = format!("{}/{}.hex", case.format.id(), case.id);
    fs::write(fixture_root.join(&binary_rel), &case.bytes)?;
    fs::write(fixture_root.join(&annotation_rel), annotated_hex(case))?;

    let (observed, observed_key) = observed_result(case, &case.bytes);
    let result = if observed == case.expected { "pass" } else { "fail" };
    entries.push(FixtureManifestEntry {
      id: case.id.to_string(),
      format_id: case.format.id().to_string(),
      family: case.format.family().to_string(),
      hash_algorithm: case.profile.label().to_string(),
      hash_width: case.profile.width(),
      binary: binary_rel,
      annotated_hex: annotation_rel,
      byte_length: case.bytes.len(),
      sha256: sha256_hex(&case.bytes),
      blake3: blake3::hash(&case.bytes).to_hex().to_string(),
      canonical_key: observed_key,
      expected: case.expected.to_string(),
      relation: case.relation.map(str::to_string),
    });
    results.push(ResultLedgerEntry {
      fixture_id: case.id.to_string(),
      expected: case.expected.to_string(),
      observed,
      result: result.to_string(),
    });
  }

  let manifest = FixtureManifest {
    schema_version: 1,
    campaign_id: CAMPAIGN_ID.to_string(),
    stage: "p0b-2-ordered-pages".to_string(),
    reference_tool: ReferenceTool {
      name: "aeordb-v4-reference".to_string(),
      revision: TOOL_REVISION.to_string(),
      production_dependencies: Vec::new(),
      provenance: "Independent implementation of ratified decision-log Rounds 7-9 and 10-15; no AeorDB crate dependency".to_string(),
      reviewer_status: "pending-owner-review-before-production-writer".to_string(),
    },
    contract_registry: "format-contract-registry.json".to_string(),
    fixture_count: entries.len(),
    fixtures: entries,
  };
  let ledger = ResultLedger { schema_version: 1, campaign_id: CAMPAIGN_ID.to_string(), tool_revision: TOOL_REVISION.to_string(), results };

  fs::write(fixture_root.join("format-fixture-manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
  fs::write(fixture_root.join("reference-result-ledger.json"), serde_json::to_vec_pretty(&ledger)?)?;
  verify(fixture_root)
}

fn verify(fixture_root: &Path) -> DynResult<()> {
  let spec_root = fixture_root.parent().and_then(Path::parent).ok_or("fixture root must be below spec/fixtures")?;
  semantics::verify(spec_root)?;
  let manifest_path = fixture_root.join("format-fixture-manifest.json");
  let manifest: FixtureManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
  if manifest.schema_version != 1 || manifest.campaign_id != CAMPAIGN_ID || manifest.stage != "p0b-2-ordered-pages" {
    return Err("fixture manifest identity mismatch".into());
  }
  if manifest.reference_tool.revision != TOOL_REVISION || !manifest.reference_tool.production_dependencies.is_empty() {
    return Err("fixture manifest reference-tool provenance mismatch".into());
  }
  if manifest.fixture_count != manifest.fixtures.len() {
    return Err("fixture_count does not match fixture rows".into());
  }
  ensure_safe_relative_path(&manifest.contract_registry)?;
  if !fixture_root.join(&manifest.contract_registry).is_file() {
    return Err("contract registry named by fixture manifest is missing".into());
  }

  let expected_cases: BTreeMap<&str, FixtureCase> = fixture_cases().into_iter().map(|case| (case.id, case)).collect();
  if expected_cases.len() != manifest.fixtures.len() {
    return Err("fixture manifest does not cover the complete declared fixture set".into());
  }

  let mut ledger_results = Vec::with_capacity(manifest.fixtures.len());
  let mut seen_ids = BTreeMap::new();
  for entry in &manifest.fixtures {
    if seen_ids.insert(entry.id.as_str(), ()).is_some() {
      return Err(format!("duplicate fixture id: {}", entry.id).into());
    }
    ensure_safe_relative_path(&entry.binary)?;
    ensure_safe_relative_path(&entry.annotated_hex)?;
    let case = expected_cases.get(entry.id.as_str()).ok_or_else(|| format!("unknown fixture row: {}", entry.id))?;
    let actual = fs::read(fixture_root.join(&entry.binary))?;
    if actual != case.bytes {
      return Err(format!("fixture bytes differ from independent reference construction: {}", entry.id).into());
    }
    if entry.byte_length != actual.len()
      || entry.format_id != case.format.id()
      || entry.family != case.format.family()
      || entry.hash_width != case.profile.width()
      || entry.hash_algorithm != case.profile.label()
      || entry.sha256 != sha256_hex(&actual)
      || entry.blake3 != blake3::hash(&actual).to_hex().as_str()
      || entry.canonical_key != case.canonical_key
      || entry.expected != case.expected
      || entry.relation.as_deref() != case.relation
    {
      return Err(format!("fixture metadata mismatch: {}", entry.id).into());
    }
    let annotation = fs::read_to_string(fixture_root.join(&entry.annotated_hex))?;
    if annotation != annotated_hex(case) {
      return Err(format!("annotated hex mismatch: {}", entry.id).into());
    }

    let (observed, observed_key) = observed_result(case, &actual);
    if observed_key != case.canonical_key {
      return Err(format!("fixture canonical key mismatch: {}", entry.id).into());
    }
    let result = if observed == case.expected { "pass" } else { "fail" };
    if result != "pass" {
      return Err(format!("fixture outcome mismatch: {} expected {} observed {}", entry.id, case.expected, observed).into());
    }
    ledger_results.push(ResultLedgerEntry {
      fixture_id: entry.id.clone(),
      expected: entry.expected.clone(),
      observed,
      result: result.to_string(),
    });
  }

  let ledger: ResultLedger = serde_json::from_slice(&fs::read(fixture_root.join("reference-result-ledger.json"))?)?;
  if ledger.schema_version != 1 || ledger.campaign_id != CAMPAIGN_ID || ledger.tool_revision != TOOL_REVISION {
    return Err("reference result ledger identity mismatch".into());
  }
  if serde_json::to_value(&ledger.results)? != serde_json::to_value(&ledger_results)? {
    return Err("reference result ledger does not match a fresh verification".into());
  }

  println!("v4 reference fixtures: PASS ({} independent cases)", manifest.fixtures.len());
  Ok(())
}

fn fixture_cases() -> Vec<FixtureCase> {
  let valid_32 = header_region(HeaderFields::canonical(HashProfile::Blake3_256, 41), HeaderFields::canonical(HashProfile::Blake3_256, 42));
  let valid_64 = header_region(HeaderFields::canonical(HashProfile::Sha512, 41), HeaderFields::canonical(HashProfile::Sha512, 42));

  let mut one_valid = valid_32.clone();
  one_valid[200] ^= 0x80;

  let equal_slot = build_slot(&HeaderFields::canonical(HashProfile::Blake3_256, 42));
  let mut equal_identical = Vec::with_capacity(HEADER_REGION_LENGTH);
  equal_identical.extend_from_slice(&equal_slot);
  equal_identical.extend_from_slice(&equal_slot);

  let mut ambiguous_a = HeaderFields::canonical(HashProfile::Blake3_256, 42);
  ambiguous_a.updated_at_ms = 1_700_000_000_042;
  let mut ambiguous_b = ambiguous_a.clone();
  ambiguous_b.updated_at_ms += 1;
  let equal_ambiguous = header_region(ambiguous_a, ambiguous_b);

  let mut no_valid = valid_32.clone();
  no_valid[200] ^= 0x80;
  no_valid[SLOT_LENGTH + 200] ^= 0x80;

  let mut unknown_a = HeaderFields::canonical(HashProfile::Blake3_256, 41);
  unknown_a.extra_reader_capability_bit = Some(24);
  let mut unknown_b = HeaderFields::canonical(HashProfile::Blake3_256, 42);
  unknown_b.extra_reader_capability_bit = Some(24);
  let unknown_capability = header_region(unknown_a, unknown_b);

  let mut reserved_a = HeaderFields::canonical(HashProfile::Blake3_256, 41);
  reserved_a.nonzero_reserved = true;
  let mut reserved_b = HeaderFields::canonical(HashProfile::Blake3_256, 42);
  reserved_b.nonzero_reserved = true;
  let nonzero_reserved = header_region(reserved_a, reserved_b);

  let mut padding_a = HeaderFields::canonical(HashProfile::Blake3_256, 41);
  padding_a.nonzero_hash_padding = true;
  let mut padding_b = HeaderFields::canonical(HashProfile::Blake3_256, 42);
  padding_b.nonzero_hash_padding = true;
  let nonzero_hash_padding = header_region(padding_a, padding_b);

  let mut adopted_a = HeaderFields::canonical(HashProfile::Blake3_256, 43);
  adopted_a.physical_instance_id = sequence_bytes(0xb0);
  let mut adopted_b = HeaderFields::canonical(HashProfile::Blake3_256, 44);
  adopted_b.physical_instance_id = sequence_bytes(0xb0);
  let adopted = header_region(adopted_a, adopted_b);

  let mut cases = vec![
    header_fixture("header-blake3-256-valid-ab", HashProfile::Blake3_256, "selected:42", Some("physical-instance-original"), valid_32),
    header_fixture("header-sha512-valid-ab", HashProfile::Sha512, "selected:42", None, valid_64),
    header_fixture("header-blake3-256-one-valid-slot", HashProfile::Blake3_256, "selected:42:redundancy-degraded", None, one_valid),
    header_fixture("header-blake3-256-equal-identical", HashProfile::Blake3_256, "selected:42", None, equal_identical),
    header_fixture("header-blake3-256-equal-ambiguous", HashProfile::Blake3_256, "error:ambiguous_equal_sequence", None, equal_ambiguous),
    header_fixture("header-blake3-256-no-valid-slot", HashProfile::Blake3_256, "error:no_valid_slot", None, no_valid),
    header_fixture(
      "header-blake3-256-unknown-capability",
      HashProfile::Blake3_256,
      "error:unsupported_required_capability",
      None,
      unknown_capability,
    ),
    header_fixture("header-blake3-256-nonzero-reserved", HashProfile::Blake3_256, "error:reserved_nonzero", None, nonzero_reserved),
    header_fixture(
      "header-blake3-256-nonzero-hash-padding",
      HashProfile::Blake3_256,
      "error:hash_padding_nonzero",
      None,
      nonzero_hash_padding,
    ),
    header_fixture(
      "header-blake3-256-adopted-physical-id",
      HashProfile::Blake3_256,
      "selected:44",
      Some("adopts:header-blake3-256-valid-ab"),
      adopted,
    ),
  ];
  cases.extend(config::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Config(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(dependency::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Dependency(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(core::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Core(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(definitions::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Definition(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(index::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Index(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(field_index::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::FieldIndex(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(policy::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Policy(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(parser::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Parser(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(selector::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::Selector(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases.extend(value_store::fixture_cases().into_iter().map(|case| FixtureCase {
    id: case.id,
    format: FixtureFormat::ValueStore(case.format),
    profile: case.profile,
    expected: case.expected,
    relation: case.relation,
    canonical_key: case.canonical_key,
    bytes: case.bytes,
  }));
  cases
}

fn header_fixture(
  id: &'static str,
  profile: HashProfile,
  expected: &'static str,
  relation: Option<&'static str>,
  bytes: Vec<u8>,
) -> FixtureCase {
  FixtureCase { id, format: FixtureFormat::DatabaseHeaderV4, profile, expected, relation, canonical_key: None, bytes }
}

fn header_region(slot_a: HeaderFields, slot_b: HeaderFields) -> Vec<u8> {
  let mut region = Vec::with_capacity(HEADER_REGION_LENGTH);
  region.extend_from_slice(&build_slot(&slot_a));
  region.extend_from_slice(&build_slot(&slot_b));
  region
}

fn build_slot(fields: &HeaderFields) -> [u8; SLOT_LENGTH] {
  let mut slot = [0u8; SLOT_LENGTH];
  slot[0..4].copy_from_slice(b"AEOR");
  slot[4] = 4;
  put_u16(&mut slot, 5, SLOT_LENGTH as u16);
  slot[7] = 0;
  put_u16(&mut slot, 8, fields.profile.algorithm_id());
  put_u64(&mut slot, 10, fields.sequence);
  put_u64(&mut slot, 18, 1_700_000_000_000);
  put_u64(&mut slot, 26, fields.updated_at_ms);
  slot[34..50].copy_from_slice(&sequence_bytes(0x10));
  put_u64(&mut slot, 50, 4_096);
  slot[58..90].copy_from_slice(INITIAL_CAPABILITIES);
  if let Some(bit) = fields.extra_reader_capability_bit {
    set_capability(&mut slot[58..90], bit);
  }
  put_u64(&mut slot, 90, 2_048);
  put_u64(&mut slot, 98, 65_536);
  slot[106] = 1;
  slot[107] = 0;
  slot[108] = 0;
  slot[109] = 0;
  put_u64(&mut slot, 110, 67_584);
  put_u64(&mut slot, 118, 65_536);
  slot[126] = 1;
  slot[127] = 0;
  put_u64(&mut slot, 128, 133_120);
  put_u64(&mut slot, 136, 0);
  put_u64(&mut slot, 144, 0);
  put_u64(&mut slot, 152, 7);
  put_hash_slot(&mut slot, 160, fields.profile.width(), 0x20);
  put_hash_slot(&mut slot, 224, fields.profile.width(), 0x40);
  put_hash_slot(&mut slot, 288, fields.profile.width(), 0x60);
  slot[352..384].copy_from_slice(INITIAL_CAPABILITIES);
  if let Some(bit) = fields.extra_writer_capability_bit {
    set_capability(&mut slot[352..384], bit);
  }
  put_u16(&mut slot, 384, 1);
  put_hash_slot(&mut slot, 392, fields.profile.width(), 0x80);
  put_u64(&mut slot, 456, 9);
  slot[464..480].copy_from_slice(&fields.physical_instance_id);
  if fields.nonzero_reserved {
    slot[480] = 1;
  }
  if fields.nonzero_hash_padding && fields.profile.width() < 64 {
    slot[160 + fields.profile.width()] = 1;
  }
  write_crc(&mut slot);
  slot
}

fn select_header_region(region: &[u8]) -> Result<SelectedSlot, &'static str> {
  if region.len() != HEADER_REGION_LENGTH {
    return Err("header_region_length");
  }
  let a = decode_slot(&region[..SLOT_LENGTH]);
  let b = decode_slot(&region[SLOT_LENGTH..]);
  match (a, b) {
    (Ok(a), Ok(b)) => {
      if a.sequence > b.sequence {
        Ok(SelectedSlot { sequence: a.sequence, redundancy_degraded: false })
      } else if b.sequence > a.sequence {
        Ok(SelectedSlot { sequence: b.sequence, redundancy_degraded: false })
      } else if region[..SLOT_LENGTH] == region[SLOT_LENGTH..] {
        Ok(SelectedSlot { sequence: a.sequence, redundancy_degraded: false })
      } else {
        Err("ambiguous_equal_sequence")
      }
    }
    (Ok(a), Err(_)) => Ok(SelectedSlot { sequence: a.sequence, redundancy_degraded: true }),
    (Err(_), Ok(b)) => Ok(SelectedSlot { sequence: b.sequence, redundancy_degraded: true }),
    (Err(left), Err(right)) if left == right && left != "crc_mismatch" => Err(left),
    (Err(_), Err(_)) => Err("no_valid_slot"),
  }
}

fn decode_slot(slot: &[u8]) -> Result<SelectedSlot, &'static str> {
  if slot.len() != SLOT_LENGTH {
    return Err("slot_length");
  }
  let stored_crc = read_u32(slot, CRC_OFFSET)?;
  if stored_crc != crc32fast::hash(&slot[..CRC_OFFSET]) {
    return Err("crc_mismatch");
  }
  if &slot[0..4] != b"AEOR" {
    return Err("magic");
  }
  if slot[4] != 4 || read_u16(slot, 5)? as usize != SLOT_LENGTH {
    return Err("version_or_slot_length");
  }
  if slot[7] != 0 || slot[386..392].iter().any(|byte| *byte != 0) || slot[480..CRC_OFFSET].iter().any(|byte| *byte != 0) {
    return Err("reserved_nonzero");
  }
  let hash_width = match read_u16(slot, 8)? {
    0x0001 | 0x0002 | 0x0004 => 32,
    0x0003 | 0x0005 => 64,
    _ => return Err("hash_algorithm"),
  };
  if slot[58 + 3..90].iter().any(|byte| *byte != 0) || slot[352 + 3..384].iter().any(|byte| *byte != 0) {
    return Err("unsupported_required_capability");
  }
  if slot[108] > 1 {
    return Err("noncanonical_boolean");
  }
  for offset in [160, 224, 288, 392] {
    if slot[offset + hash_width..offset + 64].iter().any(|byte| *byte != 0) {
      return Err("hash_padding_nonzero");
    }
  }
  if slot[34..50].iter().all(|byte| *byte == 0) || slot[464..480].iter().all(|byte| *byte == 0) {
    return Err("zero_identity");
  }
  if read_u16(slot, 384)? == 0 || read_u64(slot, 456)? == 0 {
    return Err("zero_registry_or_fence");
  }
  let kv_end = read_u64(slot, 90)?.checked_add(read_u64(slot, 98)?).ok_or("offset_overflow")?;
  let nvt_start = read_u64(slot, 110)?;
  let nvt_end = nvt_start.checked_add(read_u64(slot, 118)?).ok_or("offset_overflow")?;
  if kv_end > nvt_start || nvt_end > read_u64(slot, 128)? {
    return Err("region_overlap");
  }
  Ok(SelectedSlot { sequence: read_u64(slot, 10)?, redundancy_degraded: false })
}

fn observed_result(case: &FixtureCase, bytes: &[u8]) -> (String, Option<String>) {
  match case.format {
    FixtureFormat::DatabaseHeaderV4 => {
      let observed = match select_header_region(bytes) {
        Ok(selected) if selected.redundancy_degraded => format!("selected:{}:redundancy-degraded", selected.sequence),
        Ok(selected) => format!("selected:{}", selected.sequence),
        Err(error) => format!("error:{error}"),
      };
      (observed, None)
    }
    FixtureFormat::Config(_) => config::observe(case.profile, bytes),
    FixtureFormat::Core(format) => core::observe(format, case.profile, bytes),
    FixtureFormat::Dependency(_) => dependency::observe(case.profile, bytes),
    FixtureFormat::Definition(_) => definitions::observe(case.profile, bytes),
    FixtureFormat::FieldIndex(format) => field_index::observe(format, case.profile, bytes),
    FixtureFormat::Index(_) => index::observe(case.profile, bytes),
    FixtureFormat::Parser(_) => parser::observe(case.profile, bytes),
    FixtureFormat::Policy(_) => policy::observe(case.profile, bytes),
    FixtureFormat::Selector(_) => selector::observe(case.profile, bytes),
    FixtureFormat::ValueStore(_) => value_store::observe(case.profile, bytes),
  }
}

fn annotated_hex(case: &FixtureCase) -> String {
  let mut output = String::new();
  output.push_str(&format!("# fixture: {}\n", case.id));
  match case.format {
    FixtureFormat::DatabaseHeaderV4 => output.push_str("# contract: DatabaseHeaderV4, two 1024-byte slots, data offset 2048\n"),
    FixtureFormat::Config(_)
    | FixtureFormat::Core(_)
    | FixtureFormat::Dependency(_)
    | FixtureFormat::Definition(_)
    | FixtureFormat::FieldIndex(_)
    | FixtureFormat::Index(_)
    | FixtureFormat::Parser(_)
    | FixtureFormat::Policy(_)
    | FixtureFormat::Selector(_)
    | FixtureFormat::ValueStore(_) => {
      output.push_str(&format!("# contract: {}\n", case.format.family()));
    }
  }
  output.push_str(&format!("# hash: {} ({} bytes)\n", case.profile.label(), case.profile.width()));
  output.push_str(&format!("# expected: {}\n", case.expected));
  if let Some(key) = &case.canonical_key {
    output.push_str(&format!("# canonical key: {key}\n"));
  }
  if let Some(relation) = case.relation {
    output.push_str(&format!("# relation: {relation}\n"));
  }
  match case.format {
    FixtureFormat::DatabaseHeaderV4 => {
      output.push_str("# hex offsets are absolute within the 2048-byte header region\n");
      output.push_str("# slot A starts 0x000; slot B starts 0x400; field offsets below are slot-relative\n");
      for (offset, length, name) in header_field_annotations() {
        output.push_str(&format!("# field +0x{offset:03x} len {length:>3}: {name}\n"));
      }
    }
    FixtureFormat::Config(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in config::annotation_lines(&case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::Core(format) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in core::annotation_lines(format, case.profile, &case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::Definition(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in definitions::annotation_lines(&case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::FieldIndex(format) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in field_index::annotation_lines(format, case.profile, &case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::Dependency(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in dependency::annotation_lines(&case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::Index(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in index::annotation_lines(case.profile, &case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::Policy(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in policy::annotation_lines() {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::Parser(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in parser::annotation_lines(&case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::Selector(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in selector::annotation_lines(&case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
    FixtureFormat::ValueStore(_) => {
      output.push_str("# hex offsets are absolute within this fixture\n");
      for line in value_store::annotation_lines(case.profile, &case.bytes) {
        output.push_str(&format!("# {line}\n"));
      }
    }
  }
  for (line, chunk) in case.bytes.chunks(16).enumerate() {
    output.push_str(&format!("{:08x}: {}\n", line * 16, hex::encode(chunk)));
  }
  output
}

fn header_field_annotations() -> &'static [(usize, usize, &'static str)] {
  &[
    (0, 4, "magic"),
    (4, 1, "header_version"),
    (5, 2, "slot_length"),
    (7, 1, "header_flags"),
    (8, 2, "hash_algorithm"),
    (10, 8, "slot_sequence"),
    (18, 8, "created_at_ms"),
    (26, 8, "updated_at_ms"),
    (34, 16, "database_id"),
    (50, 8, "write_sequence_high_water"),
    (58, 32, "required_reader_capabilities"),
    (90, 8, "kv_block_offset"),
    (98, 8, "kv_block_length"),
    (106, 1, "kv_block_version"),
    (107, 1, "kv_block_stage"),
    (108, 1, "resize_in_progress"),
    (109, 1, "resize_target_stage"),
    (110, 8, "nvt_offset"),
    (118, 8, "nvt_length"),
    (126, 1, "nvt_version"),
    (127, 1, "backup_type"),
    (128, 8, "hot_tail_offset"),
    (136, 8, "buffer_kvs_offset"),
    (144, 8, "buffer_nvt_offset"),
    (152, 8, "entry_count"),
    (160, 64, "head_hash"),
    (224, 64, "base_hash"),
    (288, 64, "target_hash"),
    (352, 32, "required_writer_capabilities"),
    (384, 2, "system_family_registry_version"),
    (386, 6, "reserved"),
    (392, 64, "system_family_registry_fingerprint"),
    (456, 8, "writer_fence_epoch"),
    (464, 16, "physical_instance_id"),
    (480, 540, "reserved"),
    (1020, 4, "slot_crc32"),
  ]
}

fn ensure_safe_relative_path(path: &str) -> DynResult<()> {
  let path = Path::new(path);
  if path.is_absolute() || path.components().any(|component| !matches!(component, Component::Normal(_))) {
    return Err(format!("fixture path is not a safe relative path: {}", path.display()).into());
  }
  Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
  hex::encode(Sha256::digest(bytes))
}

fn sequence_bytes(start: u8) -> [u8; 16] {
  let mut output = [0u8; 16];
  for (index, byte) in output.iter_mut().enumerate() {
    *byte = start.wrapping_add(index as u8);
  }
  output
}

fn put_hash_slot(slot: &mut [u8], offset: usize, width: usize, start: u8) {
  for index in 0..width {
    slot[offset + index] = start.wrapping_add(index as u8);
  }
}

fn set_capability(bytes: &mut [u8], bit: usize) {
  bytes[bit / 8] |= 1 << (bit % 8);
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  let raw = bytes.get(offset..offset + 2).ok_or("truncated")?;
  Ok(u16::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let raw = bytes.get(offset..offset + 4).ok_or("truncated")?;
  Ok(u32::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  let raw = bytes.get(offset..offset + 8).ok_or("truncated")?;
  Ok(u64::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn write_crc(slot: &mut [u8; SLOT_LENGTH]) {
  let crc = crc32fast::hash(&slot[..CRC_OFFSET]);
  slot[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn crc32_iso_hdlc_known_vector() {
    assert_eq!(crc32fast::hash(b"123456789"), 0xcbf4_3926);
  }

  #[test]
  fn valid_seed_profiles_select_newer_slot() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let region = header_region(HeaderFields::canonical(profile, 1), HeaderFields::canonical(profile, 2));
      assert_eq!(select_header_region(&region), Ok(SelectedSlot { sequence: 2, redundancy_degraded: false }));
    }
  }

  #[test]
  fn exactly_hash_sized_slots_have_zero_padding() {
    let slot_32 = build_slot(&HeaderFields::canonical(HashProfile::Blake3_256, 1));
    assert!(slot_32[160 + 32..224].iter().all(|byte| *byte == 0));
    let slot_64 = build_slot(&HeaderFields::canonical(HashProfile::Sha512, 1));
    assert!(slot_64[160..224].iter().any(|byte| *byte != 0));
  }

  #[test]
  fn fixture_cases_match_their_expected_outcomes() {
    for case in fixture_cases() {
      let (observed, canonical_key) = observed_result(&case, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(canonical_key, case.canonical_key, "fixture {} key", case.id);
    }
  }
}
