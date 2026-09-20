//! Independent existing WholeEntity fixtures expose framing/version confusion.
use super::*;
use aeordb::engine::v4::entity::decode_whole_entity;
use aeordb::engine::v4::gc::decode_physical_incarnation;
use aeordb::engine::v4::gc_retirement::{
  PhysicalInventoryAuditBoundaryV1, PhysicalInventoryRetirementClassificationV1, PhysicalInventoryRetirementObservationV1,
  RetirementJournalCheckpointReconcilerV1,
};
use aeordb::engine::v4::gc_quarantine_transition::{
  PhysicalQuarantineObservationV1, PhysicalQuarantineReachabilityV1, PhysicalQuarantineTransitionContextV1,
  PhysicalQuarantineTransitionModelV1, PhysicalQuarantineTransitionV1,
};

fn with_verified_entity<T>(algorithm: HashAlgorithm, version_zero: bool, run: impl FnOnce(PhysicalIncarnationV1<'_>, Vec<u8>) -> T) -> T {
  let body = if version_zero { "directory-tree-v0-empty" } else { "directory-root" };
  let bytes =
    fs::read(fixture_root().parent().unwrap().join(format!("whole-entity-v1/entity-{}-{body}-valid.bin", algorithm_name(algorithm))))
      .unwrap();
  let entity = decode_whole_entity(&bytes, algorithm, u64::MAX).unwrap();
  assert_eq!(entity.entity_version, u8::from(!version_zero));
  assert!(entity.write_sequence > 0, "v4 physical framing always has a reserved sequence, even for version0 bodies");
  let physical = PhysicalIncarnationV1 {
    logical_key: entity.key,
    integrity_or_legacy_digest: entity.integrity_hash,
    wal_offset: 8192,
    write_sequence: entity.write_sequence,
    entity_length: bytes.len() as u32,
    entry_type: entity.entry_type.to_u8(),
    entity_version: entity.entity_version,
  };
  // Literal Round12 offsets, independent of production incarnation encoding.
  let width = algorithm.hash_length();
  let mut encoded = vec![0; 24 + 2 * width];
  encoded[..width].copy_from_slice(entity.key);
  encoded[width..2 * width].copy_from_slice(entity.integrity_hash);
  encoded[2 * width..2 * width + 8].copy_from_slice(&physical.wal_offset.to_le_bytes());
  encoded[2 * width + 8..2 * width + 16].copy_from_slice(&entity.write_sequence.to_le_bytes());
  encoded[2 * width + 16..2 * width + 20].copy_from_slice(&physical.entity_length.to_le_bytes());
  encoded[2 * width + 20] = entity.entry_type.to_u8();
  encoded[2 * width + 21] = entity.entity_version;
  run(physical, encoded)
}

#[test]
fn physical_framing_characterizes_version_one_and_legacy_zero_rows() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_verified_entity(algorithm, false, |physical, encoded| {
      assert_eq!(decode_physical_incarnation(&encoded, algorithm).unwrap(), physical);
      let mut legacy = encoded;
      let width = algorithm.hash_length();
      legacy[2 * width + 8..2 * width + 16].fill(0);
      legacy[2 * width + 21] = 0;
      let decoded = decode_physical_incarnation(&legacy, algorithm).unwrap();
      assert_eq!((decoded.entity_version, decoded.write_sequence), (0, 0));
      // This only characterizes structural rows; the legacy digest still needs physical verification.
    });
  }
}

#[test]
fn physical_framing_keeps_legacy_sequence_and_range_refusals() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_verified_entity(algorithm, false, |_, encoded| {
      let width = algorithm.hash_length();
      let mut zero_sequence = encoded.clone();
      zero_sequence[2 * width + 8..2 * width + 16].fill(0);
      assert_eq!(decode_physical_incarnation(&zero_sequence, algorithm).unwrap_err().code(), "physical_incarnation_fields");
      for range in [0..width, width..2 * width, 2 * width..2 * width + 8, 2 * width + 16..2 * width + 20] {
        let mut invalid = encoded.clone();
        invalid[range].fill(0);
        assert!(decode_physical_incarnation(&invalid, algorithm).is_err());
      }
      let mut overflow = encoded;
      overflow[2 * width..2 * width + 8].copy_from_slice(&u64::MAX.to_le_bytes());
      assert!(decode_physical_incarnation(&overflow, algorithm).is_err());
    });
  }
}

#[test]
fn physical_framing_accepts_verified_version_zero_body_with_v4_sequence() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_verified_entity(algorithm, true, |physical, encoded| {
      assert_eq!(
        decode_physical_incarnation(&encoded, algorithm).expect("typed version0 does not imply legacy physical framing"),
        physical
      );
    });
  }
}

#[test]
fn physical_framing_encodes_quarantine_candidate_for_verified_version_zero_body() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_verified_entity(algorithm, true, |physical, encoded| {
      let row = encode_physical_quarantine_candidate_v1(&PhysicalQuarantineCandidateWriteV1 {
        hash_algorithm: algorithm,
        incarnation: physical,
        class: PhysicalQuarantineCandidateClassV1::UnreachableActiveLocator,
        pending_since_ms: 1,
        first_unreachable_generation: 1,
        grace_at_pending_ms: 0,
      })
      .expect("quarantine rows retain the actual typed version and reserved v4 sequence");
      assert_eq!(&row[..encoded.len()], encoded);
      assert_eq!(decode_physical_quarantine_candidate_v1(&row, algorithm, false).unwrap().incarnation, physical);
    });
  }
}

#[test]
fn physical_framing_inventory_accepts_verified_version_zero_body() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_verified_entity(algorithm, true, |physical, _| {
      let cancellation = CancellationToken::new();
      let mut reconciler = RetirementJournalCheckpointReconcilerV1::new(
        algorithm,
        PhysicalInventoryAuditBoundaryV1 {
          database_id: [0x31; 16],
          scan_start_wal_offset: physical.wal_offset,
          audited_wal_offset: physical.wal_offset + u64::from(physical.entity_length),
          audited_write_sequence: physical.write_sequence,
          maximum_physical_entities: 1,
          maximum_retirement_records: 1,
        },
        None,
        &cancellation,
      )
      .unwrap();
      reconciler
        .observe(PhysicalInventoryRetirementObservationV1 {
          incarnation: physical,
          classification: PhysicalInventoryRetirementClassificationV1::NonGcArtifact,
        })
        .expect("physical inventory must accept a verified v4 version0 directory");
      let result = reconciler.finish().unwrap();
      assert_eq!(result.physical_entity_count(), 1);
      assert_eq!(result.audited_write_sequence(), physical.write_sequence);
    });
  }
}

#[test]
fn physical_framing_transition_accepts_verified_version_zero_body() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    with_verified_entity(algorithm, true, |physical, _| {
      with_effective_fixture(algorithm, &[], false, |request, _, _, _, _| {
        let prior = request.manifest;
        let cancellation = CancellationToken::new();
        let mut model = PhysicalQuarantineTransitionModelV1::new(
          PhysicalQuarantineTransitionContextV1 {
            hash_algorithm: algorithm,
            prior_manifest: prior,
            mark_generation: prior.mark_generation + 1,
            completed_at_ms: prior.completed_at_ms + 1,
            current_configured_grace_ms: 0,
            authority_root_set_digest: prior.authority_root_set_digest,
            semantic_state_digest: prior.semantic_state_digest,
            kv_layout_fingerprint: prior.kv_layout_fingerprint,
            mark_result_digest: prior.mark_result_digest,
            captured_root_lifecycle_manifest: prior.captured_root_lifecycle_manifest,
            maximum_incarnations: 1,
            maximum_candidates: 1,
            mark_complete: true,
            destructive_gc_enabled: true,
            mark_authority_healthy: true,
            physical_inventory_healthy: true,
            root_lifecycle_healthy: true,
          },
          &cancellation,
        )
        .unwrap();
        let transition = model
          .observe(PhysicalQuarantineObservationV1 {
            incarnation: physical,
            prior_candidate: None,
            reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
              class: PhysicalQuarantineCandidateClassV1::UnreachableActiveLocator,
            },
          })
          .expect("quarantine transition must retain the actual version0 body identity");
        assert!(matches!(transition, PhysicalQuarantineTransitionV1::CandidateStarted(_)));
        assert_eq!(model.finish().unwrap().started_count, 1);
      });
    });
  }
}
