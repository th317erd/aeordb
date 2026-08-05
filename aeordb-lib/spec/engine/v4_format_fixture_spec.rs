use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::database_header::{DatabaseHeaderVersion, decode_header_region, probe_header_version, read_header_region};
use aeordb::engine::v4::reader::{BoundedReader, MalformedInputClass};
use serde::Deserialize;

#[derive(Deserialize)]
struct FixtureManifest {
  fixtures: Vec<FixtureRow>,
}

#[derive(Deserialize)]
struct FixtureRow {
  id: String,
  format_id: String,
  binary: String,
  expected: String,
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn manifest() -> FixtureManifest {
  serde_json::from_slice(&fs::read(fixture_root().join("format-fixture-manifest.json")).unwrap()).unwrap()
}

#[test]
fn every_database_header_fixture_matches_the_independent_oracle() {
  let root = fixture_root();
  let rows: Vec<_> = manifest().fixtures.into_iter().filter(|row| row.format_id == "database-header-v4").collect();
  assert_eq!(rows.len(), 10);

  for row in rows {
    let bytes = fs::read(root.join(row.binary)).unwrap();
    let observed = match decode_header_region(&bytes) {
      Ok(selected) if selected.redundancy_degraded => format!("selected:{}:redundancy-degraded", selected.header.slot_sequence),
      Ok(selected) => format!("selected:{}", selected.header.slot_sequence),
      Err(error) => format!("error:{}", error.code()),
    };
    assert_eq!(observed, row.expected, "fixture {}", row.id);
  }
}

#[test]
fn header_probe_distinguishes_v3_and_v4_without_writing() {
  let v4 = fs::read(fixture_root().join("database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  assert_eq!(probe_header_version(&v4[..8]).unwrap(), DatabaseHeaderVersion::V4);

  let mut v3 = [0u8; 8];
  v3[..4].copy_from_slice(b"AEOR");
  v3[4] = 3;
  assert_eq!(probe_header_version(&v3).unwrap(), DatabaseHeaderVersion::V3);
}

#[test]
fn reading_a_header_region_does_not_modify_the_file() {
  let source = fs::read(fixture_root().join("database-header-v4/header-blake3-256-valid-ab.bin")).unwrap();
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("probe.aeordb");
  let mut file = fs::File::create(&path).unwrap();
  file.write_all(&source).unwrap();
  drop(file);

  let before = fs::read(&path).unwrap();
  let mut file = fs::File::open(&path).unwrap();
  let selected = read_header_region(&mut file).unwrap();
  assert_eq!(selected.header.slot_sequence, 42);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn bounded_reader_rejects_lengths_before_allocation() {
  let mut bytes = Vec::new();
  bytes.extend_from_slice(&u32::MAX.to_le_bytes());
  let mut reader = BoundedReader::new(&bytes, 1_024).unwrap();
  let error = reader.read_u32_length_prefixed(128).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  assert_eq!(reader.allocated_bytes(), 0);
}

#[test]
fn bounded_reader_rejects_overflow_truncation_and_trailing_bytes() {
  assert_eq!(
    BoundedReader::checked_array_bytes(usize::MAX, 2, 1_024).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );

  let mut truncated = BoundedReader::new(&[1, 2, 3], 3).unwrap();
  assert_eq!(truncated.read_exact(4).unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);

  let mut trailing = BoundedReader::new(&[1, 2], 2).unwrap();
  trailing.read_u8().unwrap();
  assert_eq!(trailing.finish().unwrap_err().class(), MalformedInputClass::TruncationOrTrailingBytes);
}

#[test]
fn bounded_reader_accepts_exact_limits() {
  let bytes = [3u8, 0, 0, 0, b'a', b'b', b'c'];
  let mut reader = BoundedReader::new(&bytes, bytes.len()).unwrap();
  assert_eq!(reader.read_u32_length_prefixed(3).unwrap(), b"abc");
  reader.finish().unwrap();
  assert_eq!(reader.allocated_bytes(), 3);
}
