use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc::{GcArtifactKindV1, ImmutableGcArtifactWriteV1, decode_gc_artifact_envelope, encode_immutable_gc_artifact};
use aeordb::engine::v4::gc_state::{
  GcStateArtifactV1, PhysicalInventoryReferenceModelV1, decode_gc_state_artifact, decode_physical_inventory_manifest_v1,
  decode_physical_inventory_record_v1, physical_inventory_records_v1, validate_gc_directory_child, validate_gc_directory_page,
  validate_physical_inventory_manifest_directory,
};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(relative_path: &str) -> Vec<u8> {
  fs::read(fixture_root().join(relative_path)).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("physical-inventory fixtures cover the two frozen hash widths"),
  }
}

fn inventory_page_bytes(algorithm: HashAlgorithm) -> Vec<u8> {
  fixture(&format!("agca-{}-physical-inventory-page-valid.bin", algorithm_name(algorithm)))
}

fn inventory_manifest_bytes(algorithm: HashAlgorithm, populated: bool) -> Vec<u8> {
  fixture(&format!("agca-{}-physical-inventory-manifest-{}.bin", algorithm_name(algorithm), if populated { "populated" } else { "empty" },))
}

fn inventory_directory_bytes(algorithm: HashAlgorithm) -> Vec<u8> {
  fixture(&format!("agca-{}-physical-inventory-directory-valid.bin", algorithm_name(algorithm)))
}

#[derive(Debug, PartialEq, Eq)]
struct ReferenceInventoryRecord {
  state: u8,
  reason: u8,
  has_replacement: bool,
  discovered_at_ms: u64,
  retirement_sequence: Option<u64>,
  has_receipt: bool,
}

fn reference_record(row: &[u8], algorithm: HashAlgorithm) -> ReferenceInventoryRecord {
  let hash_width = algorithm.hash_length();
  let physical_length = 24 + 2 * hash_width;
  let flags = u16::from_le_bytes(row[physical_length + 2..physical_length + 4].try_into().unwrap());
  let tail = physical_length + 4 + physical_length;
  let retirement_sequence = u64::from_le_bytes(row[tail + 8..tail + 16].try_into().unwrap());
  ReferenceInventoryRecord {
    state: row[physical_length],
    reason: row[physical_length + 1],
    has_replacement: flags & 1 != 0,
    discovered_at_ms: u64::from_le_bytes(row[tail..tail + 8].try_into().unwrap()),
    retirement_sequence: (retirement_sequence != 0).then_some(retirement_sequence),
    has_receipt: flags & 2 != 0,
  }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn inventory_fence(row: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  let hash_width = algorithm.hash_length();
  let physical_length = 24 + 2 * hash_width;
  let mut fence = Vec::with_capacity(8 + physical_length);
  fence.extend_from_slice(&row[2 * hash_width..2 * hash_width + 8]);
  fence.extend_from_slice(&row[..physical_length]);
  fence
}

fn multi_child_inventory_directory(algorithm: HashAlgorithm, level: u16, overlap: bool) -> Vec<u8> {
  let page_bytes = inventory_page_bytes(algorithm);
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let hash_width = algorithm.hash_length();
  let row_length = 68 + 5 * hash_width;
  let rows: Vec<_> = page.records.chunks_exact(row_length).collect();
  let lower_fences = [inventory_fence(rows[0], algorithm), inventory_fence(rows[2], algorithm)];
  let upper_fences = [inventory_fence(rows[1], algorithm), inventory_fence(rows[4], algorithm)];
  let lower_fences = if overlap { [lower_fences[0].clone(), upper_fences[0].clone()] } else { lower_fences };
  let fixed_descriptor_length = if level == 0 { 72 + hash_width } else { 88 + hash_width };
  let entries_length: usize = (0..2).map(|index| fixed_descriptor_length + lower_fences[index].len() + upper_fences[index].len()).sum();
  let mut body = vec![0u8; 80 + lower_fences[0].len() + upper_fences[1].len() + entries_length];
  put_u16(&mut body, 0, level);
  put_u16(&mut body, 2, 3);
  put_u32(&mut body, 4, 2);
  put_u32(&mut body, 16, lower_fences[0].len() as u32);
  put_u32(&mut body, 20, upper_fences[1].len() as u32);
  put_u64(&mut body, 24, 5);
  put_u64(&mut body, 40, 2);
  put_u64(&mut body, 48, page.records.len() as u64);
  put_u64(&mut body, 56, 1);
  put_u64(&mut body, 64, 2);
  put_u32(&mut body, 72, entries_length as u32);
  let outer_lower_end = 80 + lower_fences[0].len();
  let outer_upper_end = outer_lower_end + upper_fences[1].len();
  body[80..outer_lower_end].copy_from_slice(&lower_fences[0]);
  body[outer_lower_end..outer_upper_end].copy_from_slice(&upper_fences[1]);

  let mut cursor = outer_upper_end;
  for index in 0..2 {
    let descriptor_start = cursor;
    put_u32(&mut body, descriptor_start, lower_fences[index].len() as u32);
    put_u32(&mut body, descriptor_start + 4, upper_fences[index].len() as u32);
    let fields = if level == 0 {
      put_u64(&mut body, descriptor_start + 8, (index + 1) as u64);
      body[descriptor_start + 16..descriptor_start + 16 + hash_width].fill(0xa0 + index as u8);
      descriptor_start + 16 + hash_width
    } else {
      body[descriptor_start + 8..descriptor_start + 8 + hash_width].fill(0xa0 + index as u8);
      descriptor_start + 8 + hash_width
    };
    put_u64(&mut body, fields, 19);
    put_u64(&mut body, fields + 8, if index == 0 { 2 } else { 3 });
    if level == 0 {
      put_u64(&mut body, fields + 24, if index == 0 { (2 * row_length) as u64 } else { (3 * row_length) as u64 });
    } else {
      put_u64(&mut body, fields + 24, 1);
      put_u64(&mut body, fields + 32, if index == 0 { (2 * row_length) as u64 } else { (3 * row_length) as u64 });
      put_u64(&mut body, fields + 40, (index + 1) as u64);
      put_u64(&mut body, fields + 48, (index + 1) as u64);
    }
    cursor = descriptor_start + fixed_descriptor_length;
    body[cursor..cursor + lower_fences[index].len()].copy_from_slice(&lower_fences[index]);
    cursor += lower_fences[index].len();
    body[cursor..cursor + upper_fences[index].len()].copy_from_slice(&upper_fences[index]);
    cursor += upper_fences[index].len();
  }
  assert_eq!(cursor, body.len());

  let mut identity = Vec::with_capacity(34);
  identity.extend(0x31u8..=0x40);
  identity.extend(0x80u8..=0x8f);
  identity.extend_from_slice(&3u16.to_le_bytes());
  encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::GcArtifactDirectoryNode,
    hash_algorithm: algorithm,
    generation: 20,
    identity: &identity,
    body: &body,
  })
  .unwrap()
  .value
}

fn mutate_gc_directory_body(bytes: &[u8], algorithm: HashAlgorithm, mutate: impl FnOnce(&mut [u8])) -> Vec<u8> {
  let artifact = decode_gc_artifact_envelope(bytes).unwrap();
  let mut body = artifact.body.to_vec();
  mutate(&mut body);
  encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: artifact.kind,
    hash_algorithm: algorithm,
    generation: artifact.generation,
    identity: artifact.identity,
    body: &body,
  })
  .unwrap()
  .value
}

fn wrap_gc_directory(child_bytes: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  let GcStateArtifactV1::Directory(child) = decode_gc_state_artifact(child_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let hash_width = algorithm.hash_length();
  let fixed_descriptor_length = 88 + hash_width;
  let entries_length = fixed_descriptor_length + child.lower_fence.len() + child.upper_fence.len();
  let mut body = vec![0u8; 80 + child.lower_fence.len() + child.upper_fence.len() + entries_length];
  put_u16(&mut body, 0, child.level + 1);
  put_u16(&mut body, 2, child.role as u16);
  put_u32(&mut body, 4, 1);
  put_u32(&mut body, 16, child.lower_fence.len() as u32);
  put_u32(&mut body, 20, child.upper_fence.len() as u32);
  put_u64(&mut body, 24, child.live_count);
  put_u64(&mut body, 32, child.tombstone_count);
  put_u64(&mut body, 40, child.page_count);
  put_u64(&mut body, 48, child.logical_bytes);
  put_u64(&mut body, 56, child.minimum_page_id);
  put_u64(&mut body, 64, child.maximum_page_id);
  put_u32(&mut body, 72, entries_length as u32);
  let lower_end = 80 + child.lower_fence.len();
  let upper_end = lower_end + child.upper_fence.len();
  body[80..lower_end].copy_from_slice(child.lower_fence);
  body[lower_end..upper_end].copy_from_slice(child.upper_fence);
  let descriptor = upper_end;
  put_u32(&mut body, descriptor, child.lower_fence.len() as u32);
  put_u32(&mut body, descriptor + 4, child.upper_fence.len() as u32);
  body[descriptor + 8..descriptor + 8 + hash_width].copy_from_slice(&child.key);
  let fields = descriptor + 8 + hash_width;
  put_u64(&mut body, fields, child.generation);
  put_u64(&mut body, fields + 8, child.live_count);
  put_u64(&mut body, fields + 16, child.tombstone_count);
  put_u64(&mut body, fields + 24, child.page_count);
  put_u64(&mut body, fields + 32, child.logical_bytes);
  put_u64(&mut body, fields + 40, child.minimum_page_id);
  put_u64(&mut body, fields + 48, child.maximum_page_id);
  let fences = descriptor + fixed_descriptor_length;
  body[fences..fences + child.lower_fence.len()].copy_from_slice(child.lower_fence);
  body[fences + child.lower_fence.len()..].copy_from_slice(child.upper_fence);
  let mut identity = Vec::with_capacity(34);
  identity.extend_from_slice(child.database_id);
  identity.extend_from_slice(child.catalog_id);
  identity.extend_from_slice(&(child.role as u16).to_le_bytes());
  encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::GcArtifactDirectoryNode,
    hash_algorithm: algorithm,
    generation: child.generation + 1,
    identity: &identity,
    body: &body,
  })
  .unwrap()
  .value
}

#[test]
fn typed_inventory_records_match_the_independent_both_width_fixture_model() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = inventory_page_bytes(algorithm);
    let artifact = decode_gc_state_artifact(&bytes, algorithm).unwrap();
    let GcStateArtifactV1::Page(page) = artifact else {
      panic!("fixture must decode as a physical-inventory page");
    };
    let row_length = 68 + 5 * algorithm.hash_length();
    let expected: Vec<_> = page.records.chunks_exact(row_length).map(|row| reference_record(row, algorithm)).collect();
    let observed: Vec<_> = physical_inventory_records_v1(&page, algorithm)
      .unwrap()
      .map(|record| {
        let record = record.unwrap();
        ReferenceInventoryRecord {
          state: record.state.code(),
          reason: record.reason,
          has_replacement: record.replacement.is_some(),
          discovered_at_ms: record.discovered_at_ms,
          retirement_sequence: record.retirement_sequence,
          has_receipt: record.receipt_hash.is_some(),
        }
      })
      .collect();

    assert_eq!(observed, expected);
    assert_eq!(observed.len(), 5);
    assert_eq!(observed.iter().map(|record| record.state).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
    assert_eq!(observed[0].retirement_sequence, None);
    assert!(observed[1].has_replacement);
    assert!(observed[4].has_receipt);
  }
}

#[test]
fn inventory_page_iteration_borrows_rows_and_preserves_wal_order() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = inventory_page_bytes(algorithm);
  let artifact = decode_gc_state_artifact(&bytes, algorithm).unwrap();
  let GcStateArtifactV1::Page(page) = artifact else {
    unreachable!();
  };
  let records_start = page.records.as_ptr() as usize;
  let records_end = records_start + page.records.len();
  let records: Vec<_> = physical_inventory_records_v1(&page, algorithm).unwrap().map(Result::unwrap).collect();

  assert_eq!(records.len(), page.record_count as usize);
  assert!(records.windows(2).all(|pair| pair[0].incarnation.wal_offset < pair[1].incarnation.wal_offset));
  for record in records {
    let key_pointer = record.incarnation.logical_key.as_ptr() as usize;
    let digest_pointer = record.incarnation.integrity_or_legacy_digest.as_ptr() as usize;
    assert!((records_start..records_end).contains(&key_pointer));
    assert!((records_start..records_end).contains(&digest_pointer));
  }
}

#[test]
fn standalone_inventory_decoder_exposes_optional_lineage_without_fabricating_it() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = inventory_page_bytes(algorithm);
  let artifact = decode_gc_state_artifact(&bytes, algorithm).unwrap();
  let GcStateArtifactV1::Page(page) = artifact else {
    unreachable!();
  };
  let row_length = 68 + 5 * algorithm.hash_length();
  let rows: Vec<_> = page.records.chunks_exact(row_length).collect();

  let active = decode_physical_inventory_record_v1(rows[0], algorithm).unwrap();
  assert!(active.state.is_active());
  assert_eq!(active.reason, 0);
  assert!(active.replacement.is_none());
  assert!(active.retirement_sequence.is_none());
  assert!(active.receipt_hash.is_none());

  let replaced = decode_physical_inventory_record_v1(rows[1], algorithm).unwrap();
  assert!(!replaced.state.is_active());
  assert_eq!(replaced.reason, 2);
  assert!(replaced.replacement.is_some());
  assert_eq!(replaced.retirement_sequence, Some(2_002));
  assert!(replaced.receipt_hash.is_none());

  let receipt_backed = decode_physical_inventory_record_v1(rows[4], algorithm).unwrap();
  assert_eq!(receipt_backed.state.code(), 5);
  assert_eq!(receipt_backed.retirement_sequence, Some(2_005));
  assert_eq!(receipt_backed.receipt_hash.unwrap().len(), algorithm.hash_length());
}

#[test]
fn inventory_decoder_rejects_malformed_optional_fields_and_wrong_widths() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = inventory_page_bytes(algorithm);
  let artifact = decode_gc_state_artifact(&bytes, algorithm).unwrap();
  let GcStateArtifactV1::Page(page) = artifact else {
    unreachable!();
  };
  let row_length = 68 + 5 * algorithm.hash_length();
  let mut row = page.records[row_length..2 * row_length].to_vec();
  let physical_length = 24 + 2 * algorithm.hash_length();

  row[physical_length + 2..physical_length + 4].copy_from_slice(&0u16.to_le_bytes());
  assert_eq!(decode_physical_inventory_record_v1(&row, algorithm).unwrap_err().code(), "inventory_row_replacement");
  assert_eq!(
    decode_physical_inventory_record_v1(&page.records[..row_length], HashAlgorithm::Sha512).unwrap_err().code(),
    "inventory_row_length"
  );
}

#[test]
fn inventory_iterator_rejects_non_inventory_pages_and_count_disagreement() {
  let algorithm = HashAlgorithm::Blake3_256;
  let candidate_bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(candidate_page) = decode_gc_state_artifact(&candidate_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  assert_eq!(physical_inventory_records_v1(&candidate_page, algorithm).unwrap_err().code(), "physical_inventory_page_role");

  let bytes = inventory_page_bytes(algorithm);
  let artifact = decode_gc_state_artifact(&bytes, algorithm).unwrap();
  let GcStateArtifactV1::Page(mut page) = artifact else {
    unreachable!();
  };
  page.record_count += 1;
  assert_eq!(physical_inventory_records_v1(&page, algorithm).unwrap_err().code(), "physical_inventory_page_count");
}

#[test]
fn typed_inventory_manifest_preserves_the_complete_checkpoint_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = inventory_manifest_bytes(algorithm, true);
    let manifest = decode_physical_inventory_manifest_v1(&bytes, algorithm).unwrap();

    assert_eq!(manifest.database_id, (0x31u8..=0x40).collect::<Vec<_>>());
    assert_eq!(manifest.generation, 302);
    assert_eq!(manifest.completed_at_ms, 1_700_000_030_000);
    assert_eq!(manifest.kv_layout_fingerprint.len(), algorithm.hash_length());
    assert_eq!(manifest.audited_wal_offset, 2_000_000);
    assert_eq!(manifest.audited_write_sequence, 3_000);
    assert_eq!(manifest.retirement_journal_through_sequence, 2_999);
    assert_eq!(manifest.directory_root.unwrap().len(), algorithm.hash_length());
    assert_eq!(manifest.next_page_id, 32);
    assert_eq!(manifest.active_count, 1);
    assert_eq!(manifest.retired_count, 1);
    assert_eq!(manifest.orphan_count, 1);
    assert_eq!(manifest.quarantined_count, 1);
    assert_eq!(manifest.reclaimed_count, 1);
    assert_eq!(manifest.record_count(), 5);
    assert_eq!(manifest.inventoried_bytes, (5 * (68 + 5 * algorithm.hash_length())) as u64);

    let empty_bytes = inventory_manifest_bytes(algorithm, false);
    let empty = decode_physical_inventory_manifest_v1(&empty_bytes, algorithm).unwrap();
    assert!(empty.directory_root.is_none());
    assert_eq!(empty.record_count(), 0);
    assert_eq!(empty.inventoried_bytes, 0);
    assert_eq!(empty.next_page_id, 1);
  }
}

#[test]
fn streaming_inventory_model_closes_exact_manifest_counts_without_collecting_rows() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let manifest_bytes = inventory_manifest_bytes(algorithm, true);
    let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
    let page_bytes = inventory_page_bytes(algorithm);
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    let cancellation = CancellationToken::new();
    let mut model = PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 5).unwrap();

    model.observe_page(&page).unwrap();
    let summary = model.finish().unwrap();

    assert_eq!(summary.page_count, 1);
    assert_eq!(summary.record_count, 5);
    assert_eq!(summary.inventoried_bytes, manifest.inventoried_bytes);
    assert_eq!(summary.maximum_page_id, 31);
    assert_eq!(summary.catalog_id, Some((0x80u8..=0x8f).collect::<Vec<_>>().try_into().unwrap()));
  }
}

#[test]
fn inventory_model_handles_empty_state_and_latches_cancellation_or_bound_failures() {
  let algorithm = HashAlgorithm::Blake3_256;
  let empty_bytes = inventory_manifest_bytes(algorithm, false);
  let empty = decode_physical_inventory_manifest_v1(&empty_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let summary = PhysicalInventoryReferenceModelV1::new(&empty, algorithm, &cancellation, 0).unwrap().finish().unwrap();
  assert_eq!(summary.record_count, 0);
  assert_eq!(summary.catalog_id, None);

  let manifest_bytes = inventory_manifest_bytes(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  assert_eq!(
    PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 4).unwrap_err().code(),
    "physical_inventory_record_limit",
  );

  let page_bytes = inventory_page_bytes(algorithm);
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let cancellation = CancellationToken::new();
  let mut model = PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 5).unwrap();
  cancellation.cancel();
  assert_eq!(model.observe_page(&page).unwrap_err().code(), "physical_inventory_canceled");
  assert_eq!(model.finish().unwrap_err().code(), "physical_inventory_failed");
}

#[test]
fn inventory_model_rejects_cross_page_identity_order_overlap_and_aggregate_disagreement() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = inventory_manifest_bytes(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let page_bytes = inventory_page_bytes(algorithm);
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!();
  };
  let cancellation = CancellationToken::new();
  let mut repeated = PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 10).unwrap();
  repeated.observe_page(&page).unwrap();
  assert_eq!(repeated.observe_page(&page).unwrap_err().code(), "physical_inventory_record_order");

  let wrong_database = [0x42; 16];
  let mut detached_page = page.clone();
  detached_page.database_id = &wrong_database;
  let mut detached = PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 5).unwrap();
  assert_eq!(detached.observe_page(&detached_page).unwrap_err().code(), "physical_inventory_database");

  let wrong_catalog = [0x43; 16];
  let mut detached_page = page.clone();
  detached_page.catalog_id = &wrong_catalog;
  let mut detached = PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 10).unwrap();
  detached.observe_page(&page).unwrap();
  assert_eq!(detached.observe_page(&detached_page).unwrap_err().code(), "physical_inventory_catalog");

  let records: Vec<_> = physical_inventory_records_v1(&page, algorithm).unwrap().map(Result::unwrap).collect();
  let last = records.last().unwrap();
  let row_length = 68 + 5 * algorithm.hash_length();
  let mut overlapping_row = page.records[..row_length].to_vec();
  let overlapping_offset = last.incarnation.wal_offset + u64::from(last.incarnation.entity_length) - 1;
  overlapping_row[2 * algorithm.hash_length()..2 * algorithm.hash_length() + 8].copy_from_slice(&overlapping_offset.to_le_bytes());
  let mut overlapping_page = page.clone();
  overlapping_page.page_id += 1;
  overlapping_page.record_count = 1;
  overlapping_page.logical_bytes = row_length as u64;
  overlapping_page.records = &overlapping_row;
  let mut overlap = PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 6).unwrap();
  overlap.observe_page(&page).unwrap();
  assert_eq!(overlap.observe_page(&overlapping_page).unwrap_err().code(), "physical_inventory_extent_overlap");

  let mut wrong_bytes_page = page.clone();
  wrong_bytes_page.logical_bytes += 1;
  let mut wrong_bytes = PhysicalInventoryReferenceModelV1::new(&manifest, algorithm, &cancellation, 5).unwrap();
  wrong_bytes.observe_page(&wrong_bytes_page).unwrap();
  assert_eq!(wrong_bytes.finish().unwrap_err().code(), "physical_inventory_manifest_aggregate");
}

#[test]
fn gc_directory_reader_accepts_bounded_multi_child_leaf_and_internal_nodes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for level in [0, 1] {
      let bytes = multi_child_inventory_directory(algorithm, level, false);
      let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&bytes, algorithm).unwrap() else {
        unreachable!();
      };

      assert_eq!(directory.level, level);
      assert_eq!(directory.entries.len(), 2);
      assert_eq!(directory.live_count, 5);
      assert_eq!(directory.page_count, 2);
      assert_eq!(directory.minimum_page_id, 1);
      assert_eq!(directory.maximum_page_id, 2);
      assert!(directory.entries.windows(2).all(|pair| pair[0].upper_fence < pair[1].lower_fence));
      assert!(directory.entries.iter().all(|entry| !entry.physical_hint.is_complete()));
    }
  }
}

#[test]
fn gc_directory_reader_rejects_cross_child_overlap() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for level in [0, 1] {
      let bytes = multi_child_inventory_directory(algorithm, level, true);
      assert_eq!(decode_gc_state_artifact(&bytes, algorithm).unwrap_err().code(), "gc_directory_child_order");
    }
  }
}

#[test]
fn gc_directory_physical_hints_are_optional_and_reserve_bytes_fail_closed() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for level in [0, 1] {
      let base = multi_child_inventory_directory(algorithm, level, false);
      let hash_width = algorithm.hash_length();
      let complete = mutate_gc_directory_body(&base, algorithm, |body| {
        let lower_length = u32::from_le_bytes(body[16..20].try_into().unwrap()) as usize;
        let upper_length = u32::from_le_bytes(body[20..24].try_into().unwrap()) as usize;
        let descriptor = 80 + lower_length + upper_length;
        let fields = descriptor + if level == 0 { 16 + hash_width } else { 8 + hash_width };
        let hint = fields + if level == 0 { 32 } else { 56 };
        put_u64(body, hint, 44_000);
        put_u32(body, hint + 8, 512);
        put_u64(body, hint + 16, 91);
      });
      let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&complete, algorithm).unwrap() else {
        unreachable!();
      };
      assert!(directory.entries[0].physical_hint.is_complete());
      assert_eq!(directory.entries[0].physical_hint.wal_offset, 44_000);
      assert_eq!(directory.entries[0].physical_hint.write_sequence, 91);

      let partial = mutate_gc_directory_body(&base, algorithm, |body| {
        let lower_length = u32::from_le_bytes(body[16..20].try_into().unwrap()) as usize;
        let upper_length = u32::from_le_bytes(body[20..24].try_into().unwrap()) as usize;
        let descriptor = 80 + lower_length + upper_length;
        let fields = descriptor + if level == 0 { 16 + hash_width } else { 8 + hash_width };
        let hint = fields + if level == 0 { 32 } else { 56 };
        put_u64(body, hint, 44_000);
        put_u64(body, hint + 16, 91);
      });
      let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&partial, algorithm).unwrap() else {
        unreachable!();
      };
      assert!(!directory.entries[0].physical_hint.is_complete());

      let reserved = mutate_gc_directory_body(&base, algorithm, |body| {
        let lower_length = u32::from_le_bytes(body[16..20].try_into().unwrap()) as usize;
        let upper_length = u32::from_le_bytes(body[20..24].try_into().unwrap()) as usize;
        let descriptor = 80 + lower_length + upper_length;
        let fields = descriptor + if level == 0 { 16 + hash_width } else { 8 + hash_width };
        let hint = fields + if level == 0 { 32 } else { 56 };
        put_u32(body, hint + 12, 1);
      });
      assert_eq!(decode_gc_state_artifact(&reserved, algorithm).unwrap_err().code(), "gc_directory_physical_hint");
    }
  }
}

#[test]
fn inventory_manifest_directory_and_page_edges_close_without_trusting_hints() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let manifest_bytes = inventory_manifest_bytes(algorithm, true);
    let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
    let directory_bytes = inventory_directory_bytes(algorithm);
    let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    let page_bytes = inventory_page_bytes(algorithm);
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
      unreachable!();
    };

    validate_physical_inventory_manifest_directory(&manifest, &directory).unwrap();
    validate_gc_directory_page(&directory, &page).unwrap();

    let hinted_directory_bytes = mutate_gc_directory_body(&directory_bytes, algorithm, |body| {
      let lower_length = u32::from_le_bytes(body[16..20].try_into().unwrap()) as usize;
      let upper_length = u32::from_le_bytes(body[20..24].try_into().unwrap()) as usize;
      let descriptor = 80 + lower_length + upper_length;
      let hint = descriptor + 16 + algorithm.hash_length() + 32;
      put_u64(body, hint, 99_000);
      put_u32(body, hint + 8, 4_096);
      put_u64(body, hint + 16, 777);
    });
    let GcStateArtifactV1::Directory(hinted_directory) = decode_gc_state_artifact(&hinted_directory_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    validate_gc_directory_page(&hinted_directory, &page).unwrap();

    let parent_bytes = wrap_gc_directory(&directory_bytes, algorithm);
    let GcStateArtifactV1::Directory(parent) = decode_gc_state_artifact(&parent_bytes, algorithm).unwrap() else {
      unreachable!();
    };
    validate_gc_directory_child(&parent, &directory).unwrap();

    let wrong_catalog = [0x55; 16];
    let mut detached = directory.clone();
    detached.catalog_id = &wrong_catalog;
    assert_eq!(validate_gc_directory_child(&parent, &detached).unwrap_err().code(), "gc_directory_child_closure");

    let mut wrong_aggregate = directory.clone();
    wrong_aggregate.live_count += 1;
    assert_eq!(
      validate_physical_inventory_manifest_directory(&manifest, &wrong_aggregate).unwrap_err().code(),
      "physical_inventory_manifest_directory",
    );
  }
}
