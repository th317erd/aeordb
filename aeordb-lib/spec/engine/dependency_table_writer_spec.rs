//! Canonical definition writer regressions against independent frozen bytes.
use aeordb::engine::v4::dependency::{decode_dependency_table, encode_dependency_record, encode_dependency_table, DependencyRecordV1};
use aeordb::engine::v4::native_semantics::NativeSemanticComponentV1;
use aeordb::engine::v4::reader::MalformedInputClass;

fn fixture(profile: &str, suffix: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/dependency-table-v1/adpt-{profile}-{suffix}-valid.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn independent_record(bytes: &[u8]) -> DependencyRecordV1<'_> {
  // Exact Round 9 offsets, independently of the production table decoder.
  let read_u16 = |offset| u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
  let read_u32 = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
  let read_u64 = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
  let id_end = 96 + read_u32(20) as usize;
  let end = id_end + read_u32(24) as usize;
  assert_eq!(end, bytes.len());
  assert_eq!(read_u32(0) as usize, end);
  DependencyRecordV1 {
    kind: read_u16(4),
    role: read_u16(6),
    flags: read_u32(8),
    abi: read_u16(12),
    executor_profile: read_u16(14),
    fingerprint_semantics: read_u16(16),
    artifact_kind: read_u16(18),
    artifact_length: read_u64(32),
    fingerprint: bytes[40..72].try_into().unwrap(),
    dependency_id: std::str::from_utf8(&bytes[96..id_end]).unwrap(),
    version: std::str::from_utf8(&bytes[id_end..end]).unwrap(),
  }
}

#[test]
fn dependency_writer_matches_all_six_independent_table_fixtures() {
  for profile in ["blake3-256", "sha512"] {
    assert_eq!(encode_dependency_table(&[]).unwrap(), fixture(profile, "empty"));
    for suffix in ["native-parser-resolution", "wasm-mapper"] {
      let expected = fixture(profile, suffix);
      let record = independent_record(&expected[32..]);
      assert_eq!(encode_dependency_record(&record).unwrap(), expected[32..], "{profile}/{suffix}/record");
      assert_eq!(encode_dependency_table(&[record]).unwrap(), expected, "{profile}/{suffix}");
    }
  }
}

#[test]
fn dependency_writer_requires_canonical_order_without_sorting_or_deduplicating_ordinals() {
  let parser = NativeSemanticComponentV1::RawJson.dependency_record();
  let selector = NativeSemanticComponentV1::RegexSelector.dependency_record();
  let encoded = encode_dependency_table(&[parser.clone(), selector.clone()]).unwrap();
  assert_eq!(decode_dependency_table(&encoded).unwrap().records, vec![parser.clone(), selector.clone()]);
  for records in [vec![selector, parser.clone()], vec![parser.clone(), parser]] {
    assert_eq!(encode_dependency_table(&records).unwrap_err().class(), MalformedInputClass::NoncanonicalOrderOrDuplicate);
  }
}

#[test]
fn dependency_writer_enforces_exact_table_byte_boundary() {
  // 32 + 64 * (96 + 5) + 32 * 3994 + 32 * 3995 = 262144.
  let identifiers: Vec<_> = (0..64)
    .map(|index| {
      let length = if index < 32 { 3994 } else { 3995 };
      format!("/d/{index:04}/{}", "x".repeat(length - 8))
    })
    .collect();
  let mut records: Vec<_> = identifiers
    .iter()
    .map(|identifier| {
      let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
      record.dependency_id = identifier;
      record
    })
    .collect();
  let encoded = encode_dependency_table(&records).unwrap();
  assert_eq!(encoded.len(), 262144);
  assert_eq!(decode_dependency_table(&encoded).unwrap().records.len(), 64);
  let oversized = format!("{}x", identifiers.last().unwrap());
  records.last_mut().unwrap().dependency_id = &oversized;
  assert_eq!(encode_dependency_table(&records).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn dependency_writer_enforces_record_and_component_limits() {
  let identifiers: Vec<_> = (0..1025).map(|index| format!("/d/{index:04}")).collect();
  let mut records: Vec<_> = identifiers
    .iter()
    .map(|identifier| {
      let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
      record.dependency_id = identifier;
      record
    })
    .collect();
  assert_eq!(encode_dependency_table(&records).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
  records.pop();
  assert_eq!(decode_dependency_table(&encode_dependency_table(&records).unwrap()).unwrap().records.len(), 1024);
  for length in [4096, 4097] {
    let identifier = format!("/{}", "a".repeat(length - 1));
    let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
    record.dependency_id = &identifier;
    assert_eq!(encode_dependency_table(&[record]).is_ok(), length == 4096);
  }
  for length in [256, 257] {
    let version = format!("1.0.0+{}", "a".repeat(length - 6));
    assert_eq!(version.len(), length);
    let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
    record.version = &version;
    assert_eq!(encode_dependency_table(&[record]).is_ok(), length == 256);
  }
}

#[test]
fn dependency_writer_preserves_structural_unknown_executors_without_claiming_availability() {
  for (field, unknown) in [(0, 5), (0, u16::MAX), (1, 4), (1, u16::MAX)] {
    let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
    if field == 0 {
      record.abi = unknown;
    } else {
      record.executor_profile = unknown;
    }
    let encoded = encode_dependency_table(std::slice::from_ref(&record)).unwrap();
    assert_eq!(decode_dependency_table(&encoded).unwrap().records, vec![record]);
  }
}

#[test]
fn dependency_writer_rejects_each_invalid_record_field_without_emitting_bytes() {
  for field in 0..12 {
    let mut record = NativeSemanticComponentV1::RawJson.dependency_record();
    match field {
      0 => record.kind = 0,
      1 => record.role = 2,
      2 => record.flags = 8,
      3 => record.abi = 1,
      4 => record.executor_profile = 0,
      5 => record.fingerprint_semantics = 1,
      6 => record.artifact_kind = 1,
      7 => record.artifact_length = 1,
      8 => record.fingerprint = [0; 32],
      9 => record.dependency_id = "/a/../b",
      10 => record.version = "01.0.0",
      11 => record.dependency_id = "",
      _ => unreachable!(),
    }
    assert!(encode_dependency_record(&record).is_err(), "record field {field}");
    assert!(encode_dependency_table(&[record]).is_err(), "table field {field}");
  }
}

#[test]
fn dependency_writer_rejects_wasm_role_artifact_and_executor_conflicts() {
  let expected = fixture("blake3-256", "wasm-mapper");
  for field in 0..14 {
    let mut record = independent_record(&expected[32..]);
    match field {
      0 => record.kind = 3,
      1 => record.role = 3,
      2 => record.flags = 0,
      3 => record.flags |= 8,
      4 => record.abi = 0,
      5 => record.abi = 3,
      6 => record.executor_profile = 1,
      7 => record.executor_profile = 3,
      8 => record.fingerprint_semantics = 2,
      9 => record.artifact_kind = 0,
      10 => record.artifact_length = 0,
      11 => record.fingerprint = [0; 32],
      12 => record.version = "",
      13 => record.dependency_id = "/a\0b",
      _ => unreachable!(),
    }
    assert!(encode_dependency_record(&record).is_err(), "record field {field}");
    assert!(encode_dependency_table(&[record]).is_err(), "table field {field}");
  }
}

#[test]
fn dependency_writer_preserves_explicit_migration_identity_flags() {
  let expected = fixture("blake3-256", "wasm-mapper");
  let mut record = independent_record(&expected[32..]);
  record.abi = 2;
  record.executor_profile = 3;
  record.flags = 7;
  record.dependency_id = "01234567-89ab-cdef-0123-456789abcdef";
  record.version = "";
  let table = encode_dependency_table(std::slice::from_ref(&record)).unwrap();
  let standalone = encode_dependency_record(&record).unwrap();
  assert_eq!(table[32..], standalone);
  assert_eq!(decode_dependency_table(&table).unwrap().records, vec![record.clone()]);
  record.flags = 6;
  assert!(encode_dependency_record(&record).is_err(), "missing version needs its explicit migration flag");
  record.flags = 5;
  assert!(encode_dependency_record(&record).is_err(), "opaque legacy ID needs its explicit migration flag");
}

#[test]
fn dependency_writer_preserves_two_roles_of_the_same_module_as_distinct_records() {
  let expected = fixture("blake3-256", "wasm-mapper");
  let mapper = independent_record(&expected[32..]);
  let mut parser = mapper.clone();
  parser.role = 1;
  parser.abi = 3;
  let records = [parser, mapper];
  assert_eq!(records[0].fingerprint, records[1].fingerprint);
  let table = encode_dependency_table(&records).unwrap();
  assert_eq!(decode_dependency_table(&table).unwrap().records, records);
  assert_ne!(encode_dependency_record(&records[0]).unwrap(), encode_dependency_record(&records[1]).unwrap());
  // This exercises record/table encoding only. It does not authorize a new
  // semantic-catalog key for the two roles; that owner ruling is still pending.
}

#[test]
fn dependency_writer_enforces_the_complete_identity_flag_matrix() {
  let bytes = fixture("blake3-256", "wasm-mapper");
  for (abi, executor, permits_legacy) in
    [(4u16, 2u16, false), (4, u16::MAX, false), (u16::MAX, 2, false), (2, 3, true), (u16::MAX, u16::MAX, true)]
  {
    for flags in 0..16 {
      let mut record = independent_record(&bytes[32..]);
      record.abi = abi;
      record.executor_profile = executor;
      record.flags = flags;
      record.version = if flags & 1 != 0 { "" } else { "1.0.0" };
      record.dependency_id = if flags & 2 != 0 { "legacy-opaque-id" } else { "/org/example/plugin" };
      let valid = flags < 8 && flags & 4 != 0 && (permits_legacy || flags == 4);
      assert_eq!(encode_dependency_record(&record).is_ok(), valid, "ABI {abi}, executor {executor}, flags {flags}");
      assert_eq!(encode_dependency_table(&[record]).is_ok(), valid, "ABI {abi}, executor {executor}, flags {flags}");
    }
  }
  for flags in 0..16 {
    let mut native = NativeSemanticComponentV1::RawJson.dependency_record();
    native.flags = flags;
    assert_eq!(encode_dependency_record(&native).is_ok(), flags == 0);
  }
}
