use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::migration_root_map::{
  LegacyRootMapChainVerifierV1, LegacyRootMapControlBodyV1, LegacyRootMapPageBodyV1, LegacyRootMapRowV1, LegacyRootSemanticAvailabilityV1,
  decode_legacy_root_map_control, decode_legacy_root_map_page, encode_legacy_root_map_control, encode_legacy_root_map_page,
  legacy_root_map_page_identity_hash,
};
use aeordb::engine::v4::namespace::SemanticUnavailableReasonV1;

fn bytes(first: u8, length: usize) -> Vec<u8> {
  (0..length).map(|offset| first.wrapping_add(offset as u8)).collect()
}

fn id(first: u8) -> [u8; 16] {
  bytes(first, 16).try_into().unwrap()
}

fn fixture(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/system-control-v1/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn fixture_name(algorithm: HashAlgorithm, suffix: &str) -> String {
  let algorithm = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("fixture helper accepts only the two v4 database hash profiles"),
  };
  format!("control-{algorithm}-{suffix}-valid.bin")
}

fn fixture_row(hash_width: usize) -> LegacyRootMapRowV1 {
  LegacyRootMapRowV1 {
    legacy_root_hash: bytes(0x60, hash_width),
    namespace_root_v1_hash: bytes(0x70, hash_width),
    semantic_availability: LegacyRootSemanticAvailabilityV1::Complete,
    captured_source_write_sequence: 88,
  }
}

fn fixture_page(algorithm: HashAlgorithm) -> LegacyRootMapPageBodyV1 {
  LegacyRootMapPageBodyV1 {
    database_id: id(0x10),
    migration_id: id(0x20),
    logical_database_id: id(0x30),
    source_physical_instance_id: id(0x40),
    destination_physical_instance_id: id(0x50),
    page_ordinal: 0,
    previous_page_hash: vec![0; algorithm.hash_length()],
    next_page_hash: vec![0; algorithm.hash_length()],
    rows: vec![fixture_row(algorithm.hash_length())],
  }
}

fn fixture_control(algorithm: HashAlgorithm) -> LegacyRootMapControlBodyV1 {
  LegacyRootMapControlBodyV1 {
    database_id: id(0x10),
    migration_id: id(0x20),
    logical_database_id: id(0x30),
    source_physical_instance_id: id(0x40),
    destination_physical_instance_id: id(0x50),
    map_generation: 2,
    page_count: 1,
    record_count: 1,
    first_page_hash: bytes(0x60, algorithm.hash_length()),
    last_page_hash: bytes(0x60, algorithm.hash_length()),
    complete_map_digest: bytes(0x70, algorithm.hash_length()),
  }
}

#[test]
fn root_map_codecs_match_independent_fixtures_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let page_fixture = fixture(&fixture_name(algorithm, "legacy-root-map-page"));
    assert_eq!(encode_legacy_root_map_page(&fixture_page(algorithm), algorithm).unwrap(), page_fixture);
    let page = decode_legacy_root_map_page(&page_fixture, algorithm).unwrap();
    assert_eq!(page.sequence, 1);
    assert_eq!(page.body, fixture_page(algorithm));

    let control_fixture = fixture(&fixture_name(algorithm, "legacy-root-map"));
    assert_eq!(encode_legacy_root_map_control(7, &fixture_control(algorithm), algorithm).unwrap(), control_fixture);
    let control = decode_legacy_root_map_control(&control_fixture, algorithm).unwrap();
    assert_eq!(control.sequence, 7);
    assert_eq!(control.body, fixture_control(algorithm));
  }
}

#[test]
fn page_identity_is_stable_nonzero_and_changes_with_every_identity_component() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let baseline = legacy_root_map_page_identity_hash(algorithm, id(0x10), id(0x20), 7).unwrap();
    assert_eq!(baseline.len(), algorithm.hash_length());
    assert!(baseline.iter().any(|byte| *byte != 0));
    assert_eq!(baseline, legacy_root_map_page_identity_hash(algorithm, id(0x10), id(0x20), 7).unwrap());
    assert_ne!(baseline, legacy_root_map_page_identity_hash(algorithm, id(0x11), id(0x20), 7).unwrap());
    assert_ne!(baseline, legacy_root_map_page_identity_hash(algorithm, id(0x10), id(0x21), 7).unwrap());
    assert_ne!(baseline, legacy_root_map_page_identity_hash(algorithm, id(0x10), id(0x20), 8).unwrap());
  }
}

#[test]
fn selected_chain_verifier_streams_multiple_pages_and_rejects_every_closure_break() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let first_hash = legacy_root_map_page_identity_hash(algorithm, id(0x10), id(0x20), 0).unwrap();
    let second_hash = legacy_root_map_page_identity_hash(algorithm, id(0x10), id(0x20), 1).unwrap();
    let mut first = fixture_page(algorithm);
    first.next_page_hash = second_hash.clone();
    let mut second = fixture_page(algorithm);
    second.page_ordinal = 1;
    second.previous_page_hash = first_hash.clone();
    second.rows[0].legacy_root_hash = bytes(0x80, algorithm.hash_length());
    second.rows[0].namespace_root_v1_hash = bytes(0x90, algorithm.hash_length());
    second.rows[0].semantic_availability =
      LegacyRootSemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyDependencyCannotBeProven };
    let first_bytes = encode_legacy_root_map_page(&first, algorithm).unwrap();
    let second_bytes = encode_legacy_root_map_page(&second, algorithm).unwrap();

    let mut provisional = LegacyRootMapControlBodyV1 {
      page_count: 2,
      record_count: 2,
      first_page_hash: first_hash,
      last_page_hash: second_hash,
      complete_map_digest: vec![1; algorithm.hash_length()],
      ..fixture_control(algorithm)
    };
    let mut digest = LegacyRootMapChainVerifierV1::digest_builder(&provisional, algorithm).unwrap();
    digest.push_page(&first_bytes).unwrap();
    digest.push_page(&second_bytes).unwrap();
    provisional.complete_map_digest = digest.finish().unwrap();
    let control = decode_legacy_root_map_control(&encode_legacy_root_map_control(1, &provisional, algorithm).unwrap(), algorithm).unwrap();
    let mut verifier = LegacyRootMapChainVerifierV1::new(&control, algorithm).unwrap();
    verifier.push_page(&first_bytes).unwrap();
    verifier.push_page(&second_bytes).unwrap();
    assert_eq!(verifier.finish().unwrap(), 2);

    let mut wrong_link = second.clone();
    wrong_link.previous_page_hash[0] ^= 1;
    let wrong_link = encode_legacy_root_map_page(&wrong_link, algorithm).unwrap();
    let mut verifier = LegacyRootMapChainVerifierV1::new(&control, algorithm).unwrap();
    verifier.push_page(&first_bytes).unwrap();
    assert_eq!(verifier.push_page(&wrong_link).unwrap_err().code(), "legacy_root_map_chain_link");

    let mut duplicate = second.clone();
    duplicate.rows[0].legacy_root_hash = first.rows[0].legacy_root_hash.clone();
    let duplicate = encode_legacy_root_map_page(&duplicate, algorithm).unwrap();
    let mut verifier = LegacyRootMapChainVerifierV1::new(&control, algorithm).unwrap();
    verifier.push_page(&first_bytes).unwrap();
    assert_eq!(verifier.push_page(&duplicate).unwrap_err().code(), "legacy_root_map_chain_order");

    let mut verifier = LegacyRootMapChainVerifierV1::new(&control, algorithm).unwrap();
    verifier.push_page(&first_bytes).unwrap();
    assert_eq!(verifier.finish().unwrap_err().code(), "legacy_root_map_chain_incomplete");

    let mut corrupt_control = control.clone();
    corrupt_control.body.complete_map_digest[0] ^= 1;
    let mut verifier = LegacyRootMapChainVerifierV1::new(&corrupt_control, algorithm).unwrap();
    verifier.push_page(&first_bytes).unwrap();
    verifier.push_page(&second_bytes).unwrap();
    assert_eq!(verifier.finish().unwrap_err().code(), "legacy_root_map_chain_digest");
  }
}

#[test]
fn selected_chain_verifier_accepts_the_canonical_empty_map() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let empty = LegacyRootMapControlBodyV1 {
      page_count: 0,
      record_count: 0,
      first_page_hash: vec![0; algorithm.hash_length()],
      last_page_hash: vec![0; algorithm.hash_length()],
      complete_map_digest: vec![0; algorithm.hash_length()],
      ..fixture_control(algorithm)
    };
    let control = decode_legacy_root_map_control(&encode_legacy_root_map_control(1, &empty, algorithm).unwrap(), algorithm).unwrap();
    assert_eq!(LegacyRootMapChainVerifierV1::new(&control, algorithm).unwrap().finish().unwrap(), 0);
  }
}

#[test]
fn codecs_reject_invalid_semantics_order_links_counts_and_hash_widths() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut page = fixture_page(algorithm);
  page.rows[0].semantic_availability =
    LegacyRootSemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured };
  page.rows.push(page.rows[0].clone());
  assert_eq!(encode_legacy_root_map_page(&page, algorithm).unwrap_err().code(), "legacy_root_map_page_order");

  let mut page = fixture_page(algorithm);
  page.page_ordinal = 1;
  assert_eq!(encode_legacy_root_map_page(&page, algorithm).unwrap_err().code(), "legacy_root_map_page_link");

  let mut page = fixture_page(algorithm);
  page.page_ordinal = u64::MAX;
  page.previous_page_hash.fill(1);
  assert_eq!(encode_legacy_root_map_page(&page, algorithm).unwrap_err().code(), "legacy_root_map_page_ordinal");

  let mut page = fixture_page(algorithm);
  page.previous_page_hash.pop();
  assert_eq!(encode_legacy_root_map_page(&page, algorithm).unwrap_err().code(), "legacy_root_map_page_hash_width");

  let mut control = fixture_control(algorithm);
  control.page_count = 2;
  assert_eq!(encode_legacy_root_map_control(1, &control, algorithm).unwrap_err().code(), "legacy_root_map_control_counts");

  let mut malformed = fixture(&fixture_name(algorithm, "legacy-root-map-page"));
  let row_offset = 32 + 96 + 2 * algorithm.hash_length();
  malformed[row_offset + 2 * algorithm.hash_length() + 2] = 1;
  let crc = crc32fast::hash(&malformed[..malformed.len() - 4]);
  let crc_offset = malformed.len() - 4;
  malformed[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  assert_eq!(decode_legacy_root_map_page(&malformed, algorithm).unwrap_err().code(), "legacy_root_map_page_row_kind");
}
