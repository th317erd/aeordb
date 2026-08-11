use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::entity::EntryTypeV4;
use aeordb::engine::v4::gc::{GcArtifactKindV1, PhysicalIncarnationV1};
use aeordb::engine::v4::gc_retirement::{
  PhysicalInventoryAuditBoundaryV1, PhysicalInventoryRetirementClassificationV1, PhysicalInventoryRetirementObservationV1,
  RetirementJournalCheckpointReconcilerV1,
};
use aeordb::engine::v4::gc_state::{decode_physical_inventory_manifest_v1, decode_retirement_journal_segment_v1};
use tokio_util::sync::CancellationToken;

const AUDITED_WAL_OFFSET: u64 = 2_000_000;
const AUDITED_WRITE_SEQUENCE: u64 = 3_000;
const RETIREMENT_WRITE_SEQUENCE: u64 = 2_999;

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
    _ => unreachable!("inventory retirement fixtures cover the two frozen hash widths"),
  }
}

fn manifest_fixture(algorithm: HashAlgorithm, populated: bool) -> Vec<u8> {
  fixture(&format!("agca-{}-physical-inventory-manifest-{}.bin", algorithm_name(algorithm), if populated { "populated" } else { "empty" },))
}

fn journal_fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  fixture(&format!("agca-{}-retirement-journal-segment-valid.bin", algorithm_name(algorithm)))
}

fn bytes(width: usize, first: u8) -> Vec<u8> {
  let mut value = vec![first; width];
  value[width - 1] = first.wrapping_add(1);
  value
}

fn incarnation<'a>(
  key: &'a [u8],
  integrity: &'a [u8],
  wal_offset: u64,
  entity_length: u32,
  write_sequence: u64,
  entry_type: EntryTypeV4,
) -> PhysicalIncarnationV1<'a> {
  PhysicalIncarnationV1 {
    logical_key: key,
    integrity_or_legacy_digest: integrity,
    wal_offset,
    write_sequence,
    entity_length,
    entry_type: entry_type.to_u8(),
    entity_version: 1,
  }
}

fn boundary(
  database_id: [u8; 16],
  scan_start_wal_offset: u64,
  audited_wal_offset: u64,
  audited_write_sequence: u64,
  maximum_physical_entities: u64,
  maximum_retirement_records: u64,
) -> PhysicalInventoryAuditBoundaryV1 {
  PhysicalInventoryAuditBoundaryV1 {
    database_id,
    scan_start_wal_offset,
    audited_wal_offset,
    audited_write_sequence,
    maximum_physical_entities,
    maximum_retirement_records,
  }
}

fn observe_five_entity_fixture(
  reconciler: &mut RetirementJournalCheckpointReconcilerV1<'_>,
  algorithm: HashAlgorithm,
  segment: &aeordb::engine::v4::gc_state::RetirementJournalSegmentV1<'_>,
) {
  let hash_width = algorithm.hash_length();
  let integrity = bytes(hash_width, 0x91);
  let ordinary_key = bytes(hash_width, 0x51);
  let start = AUDITED_WAL_OFFSET - 500;
  for index in 0..5u64 {
    let (key, entry_type, sequence, classification) = if index == 3 {
      (
        segment.key.as_slice(),
        EntryTypeV4::GcArtifact,
        RETIREMENT_WRITE_SEQUENCE,
        PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(segment),
      )
    } else {
      (
        ordinary_key.as_slice(),
        EntryTypeV4::Chunk,
        if index == 4 { AUDITED_WRITE_SEQUENCE } else { index + 1 },
        PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
      )
    };
    reconciler
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(key, &integrity, start + index * 100, 100, sequence, entry_type),
        classification,
      })
      .unwrap();
  }
}

#[test]
fn checkpoint_handoff_matches_both_independent_inventory_fixtures() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let manifest_bytes = manifest_fixture(algorithm, true);
    let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
    let journal_bytes = journal_fixture(algorithm);
    let segment = decode_retirement_journal_segment_v1(&journal_bytes, algorithm).unwrap();
    let cancellation = CancellationToken::new();
    let mut reconciler = RetirementJournalCheckpointReconcilerV1::new(
      algorithm,
      boundary(manifest.database_id.try_into().unwrap(), AUDITED_WAL_OFFSET - 500, AUDITED_WAL_OFFSET, AUDITED_WRITE_SEQUENCE, 5, 1),
      None,
      &cancellation,
    )
    .unwrap();

    observe_five_entity_fixture(&mut reconciler, algorithm, &segment);
    let handoff = reconciler.finish().unwrap();
    assert_eq!(handoff.database_id(), <[u8; 16]>::try_from(manifest.database_id).unwrap());
    assert_eq!(handoff.audited_wal_offset(), AUDITED_WAL_OFFSET);
    assert_eq!(handoff.audited_write_sequence(), AUDITED_WRITE_SEQUENCE);
    assert_eq!(handoff.retirement_journal_through_sequence(), RETIREMENT_WRITE_SEQUENCE);
    assert_eq!(handoff.physical_entity_count(), 5);
    assert_eq!(handoff.journal().unwrap().segment_count, 1);
    assert_eq!(handoff.journal().unwrap().record_count, 1);
    handoff.validate_candidate_manifest(&manifest).unwrap();
  }
}

#[test]
fn orphaned_crash_prefix_is_not_journal_authority_but_current_retry_closes_prior_watermark() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = manifest_fixture(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let journal_bytes = journal_fixture(algorithm);
  let segment = decode_retirement_journal_segment_v1(&journal_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let mut reconciler = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), AUDITED_WAL_OFFSET - 300, AUDITED_WAL_OFFSET, AUDITED_WRITE_SEQUENCE, 3, 1),
    Some(&manifest),
    &cancellation,
  )
  .unwrap();
  let integrity = bytes(algorithm.hash_length(), 0x81);
  let ordinary_key = bytes(algorithm.hash_length(), 0x41);

  reconciler
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(
        segment.key.as_slice(),
        &integrity,
        AUDITED_WAL_OFFSET - 300,
        100,
        RETIREMENT_WRITE_SEQUENCE,
        EntryTypeV4::GcArtifact,
      ),
      classification: PhysicalInventoryRetirementClassificationV1::NoncurrentGcArtifact,
    })
    .unwrap();
  reconciler
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(
        segment.key.as_slice(),
        &integrity,
        AUDITED_WAL_OFFSET - 200,
        100,
        RETIREMENT_WRITE_SEQUENCE,
        EntryTypeV4::GcArtifact,
      ),
      classification: PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(&segment),
    })
    .unwrap();
  reconciler
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(&ordinary_key, &integrity, AUDITED_WAL_OFFSET - 100, 100, AUDITED_WRITE_SEQUENCE, EntryTypeV4::Chunk),
      classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
    })
    .unwrap();

  let handoff = reconciler.finish().unwrap();
  assert_eq!(handoff.retirement_journal_through_sequence(), RETIREMENT_WRITE_SEQUENCE);
  assert_eq!(handoff.journal().unwrap().segment_count, 1);
}

#[test]
fn missing_prior_watermark_or_incomplete_physical_extent_never_advances_checkpoint() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = manifest_fixture(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let journal_bytes = journal_fixture(algorithm);
  let segment = decode_retirement_journal_segment_v1(&journal_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let integrity = bytes(algorithm.hash_length(), 0x71);

  let mut missing = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), AUDITED_WAL_OFFSET - 100, AUDITED_WAL_OFFSET, AUDITED_WRITE_SEQUENCE, 1, 1),
    Some(&manifest),
    &cancellation,
  )
  .unwrap();
  missing
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(
        segment.key.as_slice(),
        &integrity,
        AUDITED_WAL_OFFSET - 100,
        100,
        AUDITED_WRITE_SEQUENCE,
        EntryTypeV4::GcArtifact,
      ),
      classification: PhysicalInventoryRetirementClassificationV1::NoncurrentGcArtifact,
    })
    .unwrap();
  assert_eq!(missing.finish().unwrap_err().code(), "retirement_checkpoint_prior_watermark_missing");

  let mut gap = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_300, AUDITED_WRITE_SEQUENCE, 2, 1),
    None,
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    gap
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(segment.key.as_slice(), &integrity, 1_100, 100, RETIREMENT_WRITE_SEQUENCE, EntryTypeV4::GcArtifact),
        classification: PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(&segment),
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_physical_gap",
  );
  assert_eq!(
    gap
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(segment.key.as_slice(), &integrity, 1_000, 100, RETIREMENT_WRITE_SEQUENCE, EntryTypeV4::GcArtifact),
        classification: PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(&segment),
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_failed",
  );
}

#[test]
fn corrupt_current_chain_and_resource_bounds_latch_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = manifest_fixture(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let journal_bytes = journal_fixture(algorithm);
  let segment = decode_retirement_journal_segment_v1(&journal_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let integrity = bytes(algorithm.hash_length(), 0x61);

  let mut limited = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_100, RETIREMENT_WRITE_SEQUENCE, 1, 0),
    None,
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    limited
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(segment.key.as_slice(), &integrity, 1_000, 100, RETIREMENT_WRITE_SEQUENCE, EntryTypeV4::GcArtifact),
        classification: PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(&segment),
      })
      .unwrap_err()
      .code(),
    "retirement_journal_record_limit",
  );

  let mut duplicate = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_200, RETIREMENT_WRITE_SEQUENCE, 2, 2),
    None,
    &cancellation,
  )
  .unwrap();
  for index in 0..2u64 {
    let result = duplicate.observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(
        segment.key.as_slice(),
        &integrity,
        1_000 + index * 100,
        100,
        RETIREMENT_WRITE_SEQUENCE,
        EntryTypeV4::GcArtifact,
      ),
      classification: PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(&segment),
    });
    if index == 0 {
      result.unwrap();
    } else {
      assert_eq!(result.unwrap_err().code(), "retirement_journal_unexpected_reset");
    }
  }
}

#[test]
fn cancellation_entity_limits_and_audit_high_water_are_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = manifest_fixture(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let key = bytes(algorithm.hash_length(), 0x31);
  let integrity = bytes(algorithm.hash_length(), 0x51);
  let cancellation = CancellationToken::new();
  let mut limited = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_200, AUDITED_WRITE_SEQUENCE, 1, 1),
    None,
    &cancellation,
  )
  .unwrap();
  limited
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(&key, &integrity, 1_000, 100, 1, EntryTypeV4::Chunk),
      classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
    })
    .unwrap();
  assert_eq!(
    limited
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(&key, &integrity, 1_100, 100, AUDITED_WRITE_SEQUENCE, EntryTypeV4::Chunk),
        classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_entity_limit",
  );

  let cancellation = CancellationToken::new();
  let mut canceled = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_100, AUDITED_WRITE_SEQUENCE, 1, 1),
    None,
    &cancellation,
  )
  .unwrap();
  cancellation.cancel();
  assert_eq!(
    canceled
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(&key, &integrity, 1_000, 100, AUDITED_WRITE_SEQUENCE, EntryTypeV4::Chunk),
        classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_cancelled",
  );

  let cancellation = CancellationToken::new();
  let mut short = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_100, AUDITED_WRITE_SEQUENCE, 1, 1),
    None,
    &cancellation,
  )
  .unwrap();
  short
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(&key, &integrity, 1_000, 100, AUDITED_WRITE_SEQUENCE - 1, EntryTypeV4::Chunk),
      classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
    })
    .unwrap();
  assert_eq!(short.finish().unwrap_err().code(), "retirement_checkpoint_audit_boundary");
}

#[test]
fn complete_inventory_without_retirements_preserves_zero_watermark() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = manifest_fixture(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let key = bytes(algorithm.hash_length(), 0x29);
  let integrity = bytes(algorithm.hash_length(), 0x49);
  let cancellation = CancellationToken::new();
  let mut reconciler = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_300, 2, 3, 1),
    None,
    &cancellation,
  )
  .unwrap();
  let mut legacy = incarnation(&key, &integrity, 1_000, 100, 0, EntryTypeV4::Chunk);
  legacy.entity_version = 0;
  reconciler
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: legacy,
      classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
    })
    .unwrap();
  reconciler
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(&key, &integrity, 1_100, 100, 1, EntryTypeV4::GcArtifact),
      classification: PhysicalInventoryRetirementClassificationV1::CurrentOtherGcArtifact(GcArtifactKindV1::CandidatePage),
    })
    .unwrap();
  reconciler
    .observe(PhysicalInventoryRetirementObservationV1 {
      incarnation: incarnation(&key, &integrity, 1_200, 100, 2, EntryTypeV4::Chunk),
      classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
    })
    .unwrap();
  let handoff = reconciler.finish().unwrap();
  assert_eq!(handoff.retirement_journal_through_sequence(), 0);
  assert!(handoff.journal().is_none());
}

#[test]
fn classification_prior_regression_and_candidate_mismatch_are_rejected() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = manifest_fixture(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let empty_manifest_bytes = manifest_fixture(algorithm, false);
  let empty_manifest = decode_physical_inventory_manifest_v1(&empty_manifest_bytes, algorithm).unwrap();
  let journal_bytes = journal_fixture(algorithm);
  let segment = decode_retirement_journal_segment_v1(&journal_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  assert_eq!(
    RetirementJournalCheckpointReconcilerV1::new(
      algorithm,
      boundary(manifest.database_id.try_into().unwrap(), AUDITED_WAL_OFFSET - 500, AUDITED_WAL_OFFSET - 1, AUDITED_WRITE_SEQUENCE, 5, 1,),
      Some(&manifest),
      &cancellation,
    )
    .unwrap_err()
    .code(),
    "retirement_checkpoint_prior_regression",
  );
  let wrong_database = [0xA5; 16];
  assert_eq!(
    RetirementJournalCheckpointReconcilerV1::new(
      algorithm,
      boundary(wrong_database, AUDITED_WAL_OFFSET - 500, AUDITED_WAL_OFFSET, AUDITED_WRITE_SEQUENCE, 5, 1),
      Some(&manifest),
      &cancellation,
    )
    .unwrap_err()
    .code(),
    "retirement_checkpoint_prior_manifest",
  );

  let integrity = bytes(algorithm.hash_length(), 0x21);
  let mut mismatched = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_100, RETIREMENT_WRITE_SEQUENCE, 1, 1),
    None,
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    mismatched
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(segment.key.as_slice(), &integrity, 1_000, 100, RETIREMENT_WRITE_SEQUENCE, EntryTypeV4::Chunk),
        classification: PhysicalInventoryRetirementClassificationV1::CurrentRetirementSegment(&segment),
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_classification",
  );

  let mut hidden = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_100, RETIREMENT_WRITE_SEQUENCE, 1, 1),
    None,
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    hidden
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(segment.key.as_slice(), &integrity, 1_000, 100, RETIREMENT_WRITE_SEQUENCE, EntryTypeV4::GcArtifact,),
        classification: PhysicalInventoryRetirementClassificationV1::CurrentOtherGcArtifact(GcArtifactKindV1::RetirementJournalSegment,),
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_classification",
  );

  let mut valid = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), AUDITED_WAL_OFFSET - 500, AUDITED_WAL_OFFSET, AUDITED_WRITE_SEQUENCE, 5, 1),
    None,
    &cancellation,
  )
  .unwrap();
  observe_five_entity_fixture(&mut valid, algorithm, &segment);
  let handoff = valid.finish().unwrap();
  assert_eq!(handoff.validate_candidate_manifest(&empty_manifest).unwrap_err().code(), "retirement_checkpoint_candidate_manifest");
}

#[test]
fn malformed_physical_incarnations_and_extent_overflow_latch_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = manifest_fixture(algorithm, true);
  let manifest = decode_physical_inventory_manifest_v1(&manifest_bytes, algorithm).unwrap();
  let key = bytes(algorithm.hash_length(), 0x19);
  let integrity = bytes(algorithm.hash_length(), 0x39);
  let cancellation = CancellationToken::new();
  assert_eq!(
    RetirementJournalCheckpointReconcilerV1::new(
      algorithm,
      boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_000, 1, 1, 1),
      None,
      &cancellation,
    )
    .unwrap_err()
    .code(),
    "retirement_checkpoint_boundary",
  );
  let mut malformed = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), 1_000, 1_100, 1, 1, 1),
    None,
    &cancellation,
  )
  .unwrap();
  let mut bad = incarnation(&key, &integrity, 1_000, 100, 1, EntryTypeV4::Chunk);
  bad.entity_length = 0;
  assert_eq!(
    malformed
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: bad,
        classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_physical_incarnation",
  );

  let mut overflow = RetirementJournalCheckpointReconcilerV1::new(
    algorithm,
    boundary(manifest.database_id.try_into().unwrap(), u64::MAX - 50, u64::MAX, 1, 1, 1),
    None,
    &cancellation,
  )
  .unwrap();
  assert_eq!(
    overflow
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(&key, &integrity, u64::MAX - 50, 100, 1, EntryTypeV4::Chunk),
        classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_arithmetic",
  );
  assert_eq!(
    overflow
      .observe(PhysicalInventoryRetirementObservationV1 {
        incarnation: incarnation(&key, &integrity, u64::MAX - 50, 1, 1, EntryTypeV4::Chunk),
        classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
      })
      .unwrap_err()
      .code(),
    "retirement_checkpoint_failed",
  );
}
