use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_SLOT_LENGTH, DatabaseHeaderV4, decode_header_region, encode_database_header_slot};
use aeordb::engine::v4::entity::{
  WHOLE_ENTITY_V1_KEY_CAP, WHOLE_ENTITY_V1_VALUE_CAP, WholeEntityWriteV1, checked_whole_entity_encoded_length, decode_whole_entity,
  encode_whole_entity,
};
use aeordb::engine::v4::gc::{
  GcArtifactKindV1, ImmutableGcArtifactWriteV1, checked_immutable_gc_artifact_encoded_length, encode_immutable_gc_artifact,
};
use aeordb::engine::v4::index_artifact::{
  ImmutableIndexArtifactKindV1, ImmutableIndexArtifactWriteV1, checked_immutable_index_artifact_encoded_length,
  encode_immutable_index_artifact,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::{CompressionAlgorithm, HashAlgorithm};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn header_fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join("database-header-v4").join(name)).expect("independent header fixture")
}

fn entity_fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join("whole-entity-v1").join(name)).expect("independent entity fixture")
}

#[test]
fn database_header_writer_matches_both_independent_hash_width_fixtures() {
  for name in ["header-blake3-256-valid-ab.bin", "header-sha512-valid-ab.bin"] {
    let region = header_fixture(name);
    let selected = decode_header_region(&region).expect("valid independent header region");
    let expected =
      &region[selected.selected_slot * DATABASE_HEADER_V4_SLOT_LENGTH..(selected.selected_slot + 1) * DATABASE_HEADER_V4_SLOT_LENGTH];

    let encoded = encode_database_header_slot(&selected.header).expect("fixture-derived header must encode");
    assert_eq!(encoded.as_slice(), expected, "fixture {name}");
  }
}

#[test]
fn whole_entity_writer_matches_both_independent_hash_width_fixtures() {
  for (name, algorithm) in [
    ("entity-blake3-256-directory-root-valid.bin", HashAlgorithm::Blake3_256),
    ("entity-blake3-256-directory-tree-v0-empty-valid.bin", HashAlgorithm::Blake3_256),
    ("entity-sha512-directory-root-valid.bin", HashAlgorithm::Sha512),
    ("entity-sha512-directory-tree-v0-empty-valid.bin", HashAlgorithm::Sha512),
  ] {
    let expected = entity_fixture(name);
    let decoded = decode_whole_entity(&expected, algorithm, u64::MAX).expect("valid independent whole-entity fixture");
    let request = WholeEntityWriteV1 {
      entity_version: decoded.entity_version,
      entry_type: decoded.entry_type,
      flags: decoded.flags,
      hash_algorithm: decoded.hash_algorithm,
      compression_algorithm: decoded.compression_algorithm,
      timestamp_ms: decoded.timestamp_ms,
      write_sequence: decoded.write_sequence,
      key: decoded.key,
      stored_value: decoded.stored_value,
    };

    let encoded = encode_whole_entity(&request).expect("fixture-derived whole entity must encode");
    assert_eq!(encoded, expected, "fixture {name}");
  }
}

#[test]
fn immutable_index_artifact_writer_matches_every_independent_fixture_and_key() {
  let manifest = fixture_manifest();
  let mut immutable_count = 0;
  let mut mutable_pointer_count = 0;

  for fixture in manifest["fixtures"].as_array().unwrap() {
    if fixture["family"].as_str() != Some("IndexArtifactV1") {
      continue;
    }
    let expected = fixture_bytes(fixture);
    let (kind_id, generation, identity, body) = artifact_envelope_fields(&expected);
    let Some(kind) = ImmutableIndexArtifactKindV1::from_u16(kind_id) else {
      assert!(matches!(kind_id, 0x0001..=0x0003), "fixture {}", fixture["id"]);
      mutable_pointer_count += 1;
      continue;
    };
    let encoded = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
      kind,
      hash_algorithm: fixture_hash_algorithm(fixture),
      generation,
      identity,
      body,
    })
    .unwrap();

    assert_eq!(encoded.value, expected, "fixture {}", fixture["id"]);
    assert_eq!(hex::encode(encoded.key), fixture["canonical_key"].as_str().unwrap(), "fixture {}", fixture["id"]);
    immutable_count += 1;
  }

  assert_eq!(immutable_count, 54);
  assert_eq!(mutable_pointer_count, 12);
}

#[test]
fn immutable_gc_artifact_writer_matches_every_independent_fixture_and_key() {
  let manifest = fixture_manifest();
  let mut immutable_count = 0;
  let mut mutable_control_count = 0;

  for fixture in manifest["fixtures"].as_array().unwrap() {
    if fixture["family"].as_str() != Some("GcArtifactV1") {
      continue;
    }
    let expected = fixture_bytes(fixture);
    let (kind_id, generation, identity, body) = artifact_envelope_fields(&expected);
    let kind = GcArtifactKindV1::from_u16(kind_id).unwrap();
    let request = ImmutableGcArtifactWriteV1 { kind, hash_algorithm: fixture_hash_algorithm(fixture), generation, identity, body };
    if kind.is_control() {
      let error = encode_immutable_gc_artifact(&request).unwrap_err();
      assert_eq!(error.class(), MalformedInputClass::UnknownTypeKindOrEnum, "fixture {}", fixture["id"]);
      mutable_control_count += 1;
      continue;
    }
    let encoded = encode_immutable_gc_artifact(&request).unwrap();

    assert_eq!(encoded.value, expected, "fixture {}", fixture["id"]);
    assert_eq!(hex::encode(encoded.key), fixture["canonical_key"].as_str().unwrap(), "fixture {}", fixture["id"]);
    immutable_count += 1;
  }

  assert_eq!(immutable_count, 92);
  assert_eq!(mutable_control_count, 24);
}

#[test]
fn immutable_artifact_writers_reject_invalid_bounds_before_allocation() {
  for kind in ImmutableIndexArtifactKindV1::ALL {
    let maximum_length = kind.maximum_encoded_length();
    let body_length_at_cap = maximum_length - 32 - 1 - 4;
    assert_eq!(checked_immutable_index_artifact_encoded_length(kind, 1, body_length_at_cap).unwrap(), maximum_length);
    let error = checked_immutable_index_artifact_encoded_length(kind, 1, body_length_at_cap + 1).unwrap_err();
    assert_eq!(error.class(), MalformedInputClass::AllocationAmplification, "kind {kind:?}");
  }
  assert_eq!(
    checked_immutable_index_artifact_encoded_length(ImmutableIndexArtifactKindV1::FieldIndexManifest, 0, 0).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    checked_immutable_index_artifact_encoded_length(ImmutableIndexArtifactKindV1::FieldIndexManifest, 4_097, 0).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
  assert_eq!(
    checked_immutable_index_artifact_encoded_length(ImmutableIndexArtifactKindV1::FieldIndexManifest, 1, usize::MAX).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
  assert!(ImmutableIndexArtifactKindV1::from_u16(0x0001).is_none());
  assert!(ImmutableIndexArtifactKindV1::from_u16(u16::MAX).is_none());

  for kind in GcArtifactKindV1::ALL {
    if kind.is_control() {
      assert!(kind.immutable_maximum_encoded_length().is_none());
      let error = checked_immutable_gc_artifact_encoded_length(kind, 1, 0).unwrap_err();
      assert_eq!(error.class(), MalformedInputClass::UnknownTypeKindOrEnum, "kind {kind:?}");
      continue;
    }
    let maximum_length = kind.immutable_maximum_encoded_length().unwrap();
    let body_length_at_cap = maximum_length - 32 - 1 - 4;
    assert_eq!(checked_immutable_gc_artifact_encoded_length(kind, 1, body_length_at_cap).unwrap(), maximum_length);
    let error = checked_immutable_gc_artifact_encoded_length(kind, 1, body_length_at_cap + 1).unwrap_err();
    assert_eq!(error.class(), MalformedInputClass::AllocationAmplification, "kind {kind:?}");
  }
  assert_eq!(
    checked_immutable_gc_artifact_encoded_length(GcArtifactKindV1::QuarantineManifest, 0, 0).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
  assert_eq!(
    checked_immutable_gc_artifact_encoded_length(GcArtifactKindV1::QuarantineManifest, usize::from(u16::MAX) + 1, 0).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
  assert_eq!(
    checked_immutable_gc_artifact_encoded_length(GcArtifactKindV1::CandidateDelta, 1, usize::MAX).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );
}

#[test]
fn immutable_artifact_kind_registries_freeze_every_ratified_encoded_cap() {
  for kind in ImmutableIndexArtifactKindV1::ALL {
    let expected = match kind {
      ImmutableIndexArtifactKindV1::FieldIndexManifest
      | ImmutableIndexArtifactKindV1::FieldNvtManifest
      | ImmutableIndexArtifactKindV1::ScopeCatalogManifest
      | ImmutableIndexArtifactKindV1::ValueStoreManifest => 1_024 * 1_024,
      ImmutableIndexArtifactKindV1::ArtifactDirectoryNode
      | ImmutableIndexArtifactKindV1::PostingPage
      | ImmutableIndexArtifactKindV1::ValuePage
      | ImmutableIndexArtifactKindV1::NvtTile
      | ImmutableIndexArtifactKindV1::ScopeCatalogPage
      | ImmutableIndexArtifactKindV1::DocumentStatePage
      | ImmutableIndexArtifactKindV1::IndexTaskCheckpoint => 4 * 1_024 * 1_024,
      ImmutableIndexArtifactKindV1::MutationJournalSegment => 16 * 1_024 * 1_024,
    };
    assert_eq!(kind.maximum_encoded_length(), expected, "kind {kind:?}");
  }

  for kind in GcArtifactKindV1::ALL {
    let expected = match kind {
      GcArtifactKindV1::QuarantineActiveControl
      | GcArtifactKindV1::MarkRunActiveControl
      | GcArtifactKindV1::PhysicalInventoryActiveControl
      | GcArtifactKindV1::AuditCatalogActiveControl
      | GcArtifactKindV1::VoidCatalogActiveControl
      | GcArtifactKindV1::RootLifecycleActiveControl => None,
      GcArtifactKindV1::QuarantineManifest
      | GcArtifactKindV1::RootExpiryCatalogManifest
      | GcArtifactKindV1::PhysicalInventoryManifest
      | GcArtifactKindV1::AuditCatalogManifest
      | GcArtifactKindV1::GcRunSummary
      | GcArtifactKindV1::VoidCatalogManifest
      | GcArtifactKindV1::RootLifecycleManifest
      | GcArtifactKindV1::CorruptGcEvidence
      | GcArtifactKindV1::AuditPin
      | GcArtifactKindV1::RootRetirementCommit
      | GcArtifactKindV1::VoidClaimSettlementReceipt
      | GcArtifactKindV1::RootObjectReclaimProof => Some(1_024 * 1_024),
      GcArtifactKindV1::MarkRunCheckpoint => Some(32 + 40 + 256 * 1_024 + 4),
      GcArtifactKindV1::GcArtifactDirectoryNode => Some(4 * 1_024 * 1_024),
      GcArtifactKindV1::CandidatePage
      | GcArtifactKindV1::RootExpiryPage
      | GcArtifactKindV1::RetirementJournalSegment
      | GcArtifactKindV1::PhysicalInventoryPage
      | GcArtifactKindV1::MarkMutationJournalSegment
      | GcArtifactKindV1::VoidExtentPage
      | GcArtifactKindV1::VoidClaim
      | GcArtifactKindV1::RootCandidatePage
      | GcArtifactKindV1::SweepProposal
      | GcArtifactKindV1::SweepCommitReceipt
      | GcArtifactKindV1::RecoveredSweepReceipt
      | GcArtifactKindV1::AuditDetailPage
      | GcArtifactKindV1::AuditSummaryPage => Some(16 * 1_024 * 1_024),
      GcArtifactKindV1::CandidateDelta => Some(64 * 1_024 * 1_024),
    };
    assert_eq!(kind.immutable_maximum_encoded_length(), expected, "kind {kind:?}");
  }
}

#[test]
fn immutable_artifact_writers_reject_zero_generation() {
  let index_error = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind: ImmutableIndexArtifactKindV1::FieldIndexManifest,
    hash_algorithm: HashAlgorithm::Blake3_256,
    generation: 0,
    identity: b"identity",
    body: b"body",
  })
  .unwrap_err();
  assert_eq!(index_error.class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let gc_error = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::QuarantineManifest,
    hash_algorithm: HashAlgorithm::Blake3_256,
    generation: 0,
    identity: b"identity",
    body: b"body",
  })
  .unwrap_err();
  assert_eq!(gc_error.class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let control_error = encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::QuarantineActiveControl,
    hash_algorithm: HashAlgorithm::Blake3_256,
    generation: 0,
    identity: b"identity",
    body: b"body",
  })
  .unwrap_err();
  assert_eq!(control_error.class(), MalformedInputClass::UnknownTypeKindOrEnum);
}

#[test]
fn database_header_writer_rejects_identity_capability_hash_and_region_errors() {
  let region = header_fixture("header-blake3-256-valid-ab.bin");
  let valid = decode_header_region(&region).unwrap().header;

  assert_header_error(mutate_header(&valid, |header| header.database_id = [0; 16]), "zero_identity");
  assert_header_error(mutate_header(&valid, |header| header.physical_instance_id = [0; 16]), "zero_identity");
  assert_header_error(mutate_header(&valid, |header| header.slot_sequence = 0), "zero_header_sequence");
  assert_header_error(mutate_header(&valid, |header| header.write_sequence_high_water = 0), "zero_header_sequence");
  assert_header_error(mutate_header(&valid, |header| header.writer_fence_epoch = 0), "zero_registry_or_fence");
  assert_header_error(mutate_header(&valid, |header| header.system_family_registry_version = 0), "zero_registry_or_fence");
  assert_header_error(mutate_header(&valid, |header| header.required_reader_capabilities[3] = 1), "unsupported_required_capability");
  assert_header_error(
    mutate_header(&valid, |header| {
      header.head_hash.pop();
    }),
    "hash_width",
  );
  assert_header_error(mutate_header(&valid, |header| header.kv_block_version = 2), "unsupported_region_version");
  assert_header_error(mutate_header(&valid, |header| header.nvt_version = 2), "unsupported_region_version");
  assert_header_error(mutate_header(&valid, |header| header.kv_block_stage = 10), "kv_stage");
  assert_header_error(mutate_header(&valid, |header| header.resize_target_stage = 10), "resize_target_stage");
  assert_header_error(mutate_header(&valid, |header| header.resize_target_stage = 1), "resize_state");
  assert_header_error(
    mutate_header(&valid, |header| {
      header.resize_in_progress = true;
      header.resize_target_stage = 0;
    }),
    "resize_state",
  );
  assert_header_error(mutate_header(&valid, |header| header.kv_block_length += 1), "kv_stage_length");
  assert_header_error(mutate_header(&valid, |header| header.backup_type = 3), "backup_type");
  assert_header_error(mutate_header(&valid, |header| header.kv_block_offset = 1), "region_overlap");
  assert_header_error(mutate_header(&valid, |header| header.kv_block_length = u64::MAX), "offset_overflow");
}

#[test]
fn database_header_reader_rejects_crc_valid_unchecked_stage_state() {
  for (offset, value, expected_code) in [(107, 10, "kv_stage"), (109, 10, "resize_target_stage"), (109, 1, "resize_state")] {
    let mut region = header_fixture("header-blake3-256-valid-ab.bin");
    for slot_index in 0..2 {
      let slot_start = slot_index * DATABASE_HEADER_V4_SLOT_LENGTH;
      region[slot_start + offset] = value;
      let crc_offset = slot_start + DATABASE_HEADER_V4_SLOT_LENGTH - 4;
      let crc = crc32fast::hash(&region[slot_start..crc_offset]);
      region[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
    }
    assert_eq!(decode_header_region(&region).unwrap_err().code(), expected_code);
  }
}

#[test]
fn whole_entity_writer_rejects_invalid_flags_and_unreserved_sequence() {
  let request = WholeEntityWriteV1 {
    entity_version: 1,
    entry_type: aeordb::engine::v4::entity::EntryTypeV4::FileRecord,
    flags: 0,
    hash_algorithm: HashAlgorithm::Blake3_256,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms: 1_700_000_000_000,
    write_sequence: 1,
    key: b"key",
    stored_value: b"value",
  };

  let mut invalid_flags = request.clone();
  invalid_flags.flags = 0x80;
  let error = encode_whole_entity(&invalid_flags).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::UnknownTypeKindOrEnum);
  assert_eq!(error.code(), "unknown_entity_flags");

  let mut zero_sequence = request.clone();
  zero_sequence.write_sequence = 0;
  let error = encode_whole_entity(&zero_sequence).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);
  assert_eq!(error.code(), "unreserved_write_sequence");

  let encoded = encode_whole_entity(&request).unwrap();
  let decoded = decode_whole_entity(&encoded, request.hash_algorithm, request.write_sequence).unwrap();
  assert_eq!(decoded.key, request.key);
  assert_eq!(decoded.stored_value, request.stored_value);
}

#[test]
fn whole_entity_preserves_per_type_v0_and_v1_versions_and_rejects_unknown_versions() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let key = vec![0x5a; algorithm.hash_length()];
    let base = WholeEntityWriteV1 {
      entity_version: 0,
      entry_type: aeordb::engine::v4::entity::EntryTypeV4::DirectoryIndex,
      flags: 0,
      hash_algorithm: algorithm,
      compression_algorithm: CompressionAlgorithm::None,
      timestamp_ms: 1_700_000_000_000,
      write_sequence: 7,
      key: &key,
      stored_value: b"legacy directory bytes",
    };

    let encoded_v0 = encode_whole_entity(&base).expect("v4 framing must preserve a v0 per-type entity");
    let decoded_v0 = decode_whole_entity(&encoded_v0, algorithm, base.write_sequence).unwrap();
    assert_eq!(decoded_v0.entity_version, 0);
    assert_eq!(encoded_v0[4], 0);

    let mut v1 = base.clone();
    v1.entity_version = 1;
    let encoded_v1 = encode_whole_entity(&v1).unwrap();
    let decoded_v1 = decode_whole_entity(&encoded_v1, algorithm, v1.write_sequence).unwrap();
    assert_eq!(decoded_v1.entity_version, 1);
    assert_eq!(encoded_v1[4], 1);
    assert_ne!(decoded_v0.integrity_hash, decoded_v1.integrity_hash);

    let mut unknown = base.clone();
    unknown.entity_version = 2;
    let error = encode_whole_entity(&unknown).unwrap_err();
    assert_eq!(error.class(), MalformedInputClass::UnknownMagicOrVersion);
    assert_eq!(error.code(), "unsupported_entity_version");

    let mut unsupported_pair = base.clone();
    unsupported_pair.entity_version = 1;
    unsupported_pair.entry_type = aeordb::engine::v4::entity::EntryTypeV4::Chunk;
    let error = encode_whole_entity(&unsupported_pair).unwrap_err();
    assert_eq!(error.class(), MalformedInputClass::UnknownMagicOrVersion);
    assert_eq!(error.code(), "unsupported_entry_type_entity_version");
  }
}

#[test]
fn whole_entity_length_preflight_is_exact_and_rejects_oversized_components_without_allocation() {
  assert_eq!(checked_whole_entity_encoded_length(HashAlgorithm::Blake3_256, 3, 5).unwrap(), 117);
  assert_eq!(checked_whole_entity_encoded_length(HashAlgorithm::Sha512, 3, 5).unwrap(), 149);

  let error = checked_whole_entity_encoded_length(HashAlgorithm::Blake3_256, WHOLE_ENTITY_V1_KEY_CAP + 1, 0).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  assert_eq!(error.code(), "entity_component_exceeds_cap");

  let error = checked_whole_entity_encoded_length(HashAlgorithm::Blake3_256, 0, WHOLE_ENTITY_V1_VALUE_CAP + 1).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AllocationAmplification);
  assert_eq!(error.code(), "entity_component_exceeds_cap");
}

#[test]
fn production_writer_surface_advertises_only_complete_codecs_and_remains_disconnected_from_service_activation() {
  let reference_manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/v4-reference/Cargo.toml")).unwrap();
  assert!(!reference_manifest.contains("aeordb-lib"));
  assert!(!reference_manifest.contains("path = \"../../aeordb-lib\""));

  let reference_sources = fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/v4-reference/src"))
    .unwrap()
    .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
    .collect::<Vec<_>>()
    .join("\n");
  assert!(!reference_sources.contains("encode_database_header_slot"));
  assert!(!reference_sources.contains("encode_whole_entity"));
  assert!(!reference_sources.contains("encode_immutable_index_artifact"));
  assert!(!reference_sources.contains("encode_index_manifest"));
  assert!(!reference_sources.contains("encode_immutable_gc_artifact"));

  let admission = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/admission.rs")).unwrap();
  assert!(admission.contains(".with_known_bit(capability_bit::WHOLE_ENTITY_V1)"));
  assert!(admission.contains(".with_known_bit(capability_bit::SYSTEM_CONTROL_V1)"));
  assert!(!admission.contains("capability_bit::INDEX_ARTIFACT_V1"));
  assert!(!admission.contains("capability_bit::GC_ARTIFACT_V1"));
  let storage_engine = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/storage_engine.rs")).unwrap();
  assert!(!storage_engine.contains("encode_database_header_slot"));
  assert!(!storage_engine.contains("encode_whole_entity"));
  assert!(!storage_engine.contains("encode_immutable_index_artifact"));
  assert!(!storage_engine.contains("encode_index_manifest"));
  assert!(!storage_engine.contains("encode_immutable_gc_artifact"));

  let production_sources = rust_sources(Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
  let index_artifact = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/index_artifact.rs")).unwrap();
  assert_eq!(index_artifact.matches("encode_immutable_index_artifact(").count(), 2);
  let index_page = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/index_page.rs")).unwrap();
  assert_eq!(index_page.matches("encode_immutable_index_artifact(").count(), 2);
  assert_eq!(index_page.matches("pub fn encode_artifact_directory(").count(), 1);
  assert_eq!(index_page.matches("pub fn encode_ordered_page(").count(), 1);
  assert_eq!(index_page.matches("pub fn encode_posting_record(").count(), 1);
  let index_nvt = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/index_nvt.rs")).unwrap();
  assert_eq!(index_nvt.matches("encode_immutable_index_artifact(").count(), 1);
  assert_eq!(index_nvt.matches("pub fn encode_nvt_tile(").count(), 1);
  let index_task = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/index_task.rs")).unwrap();
  assert_eq!(index_task.matches("encode_immutable_index_artifact(").count(), 2);
  assert_eq!(index_task.matches("pub fn encode_mutation_journal(").count(), 1);
  assert_eq!(index_task.matches("pub fn encode_index_task_checkpoint(").count(), 1);
  assert_eq!(production_sources.matches("encode_immutable_index_artifact(").count(), 7);
  assert_eq!(production_sources.matches("pub fn encode_index_manifest(").count(), 1);
  let expected_gc_writer_surface = [
    ("gc.rs", 1),
    ("gc_audit.rs", 1),
    ("gc_lifecycle.rs", 3),
    ("gc_mark.rs", 2),
    ("gc_quarantine.rs", 2),
    ("gc_retirement.rs", 1),
    ("gc_state.rs", 2),
    ("gc_void.rs", 6),
  ];
  assert_eq!(
    production_sources.matches("encode_immutable_gc_artifact(").count(),
    expected_gc_writer_surface.iter().map(|(_, expected_count)| expected_count).sum::<usize>()
  );
  for (owner, expected_count) in expected_gc_writer_surface {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4").join(owner)).unwrap();
    assert_eq!(
      source.matches("encode_immutable_gc_artifact(").count(),
      expected_count,
      "unexpected immutable GC encoder surface in {owner}"
    );
  }
}

fn rust_sources(path: PathBuf) -> String {
  let mut source = String::new();
  for entry in fs::read_dir(path).unwrap() {
    let entry = entry.unwrap();
    if entry.file_type().unwrap().is_dir() {
      source.push_str(&rust_sources(entry.path()));
    } else if entry.path().extension().and_then(|extension| extension.to_str()) == Some("rs") {
      source.push_str(&fs::read_to_string(entry.path()).unwrap());
    }
  }
  source
}

fn fixture_manifest() -> serde_json::Value {
  serde_json::from_slice(&fs::read(fixture_root().join("format-fixture-manifest.json")).unwrap()).unwrap()
}

fn fixture_bytes(fixture: &serde_json::Value) -> Vec<u8> {
  fs::read(fixture_root().join(fixture["binary"].as_str().unwrap())).unwrap()
}

fn fixture_hash_algorithm(fixture: &serde_json::Value) -> HashAlgorithm {
  match fixture["hash_algorithm"].as_str().unwrap() {
    "blake3-256" => HashAlgorithm::Blake3_256,
    "sha512" => HashAlgorithm::Sha512,
    other => panic!("unexpected fixture hash algorithm {other}"),
  }
}

fn artifact_envelope_fields(value: &[u8]) -> (u16, u64, &[u8], &[u8]) {
  let kind = u16::from_le_bytes(value[6..8].try_into().unwrap());
  let identity_length = usize::from(u16::from_le_bytes(value[16..18].try_into().unwrap()));
  let body_length = usize::try_from(u32::from_le_bytes(value[20..24].try_into().unwrap())).unwrap();
  let generation = u64::from_le_bytes(value[24..32].try_into().unwrap());
  let identity_end = 32 + identity_length;
  let body_end = identity_end + body_length;
  assert_eq!(body_end + 4, value.len());
  (kind, generation, &value[32..identity_end], &value[identity_end..body_end])
}

fn mutate_header(header: &DatabaseHeaderV4, mutate: impl FnOnce(&mut DatabaseHeaderV4)) -> DatabaseHeaderV4 {
  let mut changed = header.clone();
  mutate(&mut changed);
  changed
}

fn assert_header_error(header: DatabaseHeaderV4, expected_code: &str) {
  let error = encode_database_header_slot(&header).unwrap_err();
  assert_eq!(error.code(), expected_code);
}
