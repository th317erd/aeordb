use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc::PhysicalIncarnationV1;
use aeordb::engine::v4::gc_void::{
  SweepOutcomeClassV1, SweepProposalV1, SweepProposalWriteV1, SweepReceiptOutcomeWriteV1, SweepReceiptV1, SweepReceiptWriteV1,
  SweepVoidArtifactV1, decode_sweep_void_artifact, encode_sweep_proposal_v1, encode_sweep_receipt_v1,
};

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(name)).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("sweep fixtures cover the two frozen hash widths"),
  }
}

#[test]
fn sweep_proposal_and_receipt_writers_match_both_width_independent_fixtures() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let name = algorithm_name(algorithm);
    let proposal_bytes = fixture(&format!("agca-{name}-sweep-proposal.bin"));
    let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&proposal_bytes, algorithm).unwrap() else {
      panic!("sweep proposal fixture must decode as a proposal")
    };
    let candidates: Vec<_> = proposal.candidate_records(algorithm).unwrap().map(Result::unwrap).collect();
    let encoded_proposal = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
      hash_algorithm: algorithm,
      database_id: proposal.database_id.try_into().unwrap(),
      batch_id: proposal.batch_id.try_into().unwrap(),
      generation: proposal.generation,
      created_at_ms: proposal.created_at_ms,
      quarantine_manifest_hash: proposal.quarantine_manifest_hash,
      candidates: &candidates,
    })
    .unwrap();
    assert_eq!(encoded_proposal.value, proposal_bytes);
    assert_eq!(encoded_proposal.key, proposal.key);

    for (recovered, suffix) in [(false, "commit"), (true, "recovered")] {
      let receipt_bytes = fixture(&format!("agca-{name}-sweep-{suffix}-receipt.bin"));
      let SweepVoidArtifactV1::SweepReceipt(receipt) = decode_sweep_void_artifact(&receipt_bytes, algorithm).unwrap() else {
        panic!("sweep receipt fixture must decode as a receipt")
      };
      assert_eq!(receipt.recovered, recovered);
      let outcomes: Vec<_> = receipt.outcome_records(algorithm).unwrap().map(Result::unwrap).collect();
      let outcome_writes: Vec<_> = outcomes.iter().map(SweepReceiptOutcomeWriteV1::from).collect();
      let encoded_receipt = encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
        hash_algorithm: algorithm,
        recovered,
        database_id: receipt.database_id.try_into().unwrap(),
        batch_id: receipt.batch_id.try_into().unwrap(),
        generation: receipt.generation,
        reclaim_committed_at_ms: receipt.reclaim_committed_at_ms,
        proposal_hash: receipt.proposal_hash,
        void_catalog_hash: receipt.void_catalog_hash,
        outcomes: &outcome_writes,
      })
      .unwrap();
      assert_eq!(encoded_receipt.value, receipt_bytes);
      assert_eq!(encoded_receipt.key, receipt.key);
    }
  }
}

#[test]
fn sweep_writers_reject_empty_unsorted_cross_width_and_inconsistent_outcomes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let proposal_bytes = fixture("agca-blake3-256-sweep-proposal.bin");
  let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&proposal_bytes, algorithm).unwrap() else {
    panic!("sweep proposal fixture must decode as a proposal")
  };
  let candidates: Vec<_> = proposal.candidate_records(algorithm).unwrap().map(Result::unwrap).collect();
  let mut reversed = candidates.clone();
  reversed.reverse();
  let base = SweepProposalWriteV1 {
    hash_algorithm: algorithm,
    database_id: proposal.database_id.try_into().unwrap(),
    batch_id: proposal.batch_id.try_into().unwrap(),
    generation: proposal.generation,
    created_at_ms: proposal.created_at_ms,
    quarantine_manifest_hash: proposal.quarantine_manifest_hash,
    candidates: &candidates,
  };
  assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { candidates: &[], ..base }).is_err());
  assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { candidates: &reversed, ..base }).is_err());
  assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { quarantine_manifest_hash: &[1; 64], ..base }).is_err());

  let receipt_bytes = fixture("agca-blake3-256-sweep-commit-receipt.bin");
  let SweepVoidArtifactV1::SweepReceipt(receipt) = decode_sweep_void_artifact(&receipt_bytes, algorithm).unwrap() else {
    panic!("sweep receipt fixture must decode as a receipt")
  };
  let outcomes: Vec<_> = receipt.outcome_records(algorithm).unwrap().map(Result::unwrap).collect();
  let mut outcome_writes: Vec<_> = outcomes.iter().map(SweepReceiptOutcomeWriteV1::from).collect();
  outcome_writes[0].outcome = SweepOutcomeClassV1::SkippedReachable;
  assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
    hash_algorithm: algorithm,
    recovered: false,
    database_id: receipt.database_id.try_into().unwrap(),
    batch_id: receipt.batch_id.try_into().unwrap(),
    generation: receipt.generation,
    reclaim_committed_at_ms: receipt.reclaim_committed_at_ms,
    proposal_hash: receipt.proposal_hash,
    void_catalog_hash: receipt.void_catalog_hash,
    outcomes: &outcome_writes,
  })
  .is_err());
}

#[test]
fn sweep_receipt_writer_covers_every_outcome_class_at_both_hash_widths() {
  let classes = [
    SweepOutcomeClassV1::Reclaimed,
    SweepOutcomeClassV1::SkippedReachable,
    SweepOutcomeClassV1::SkippedChanged,
    SweepOutcomeClassV1::SkippedPinned,
    SweepOutcomeClassV1::SkippedPolicy,
    SweepOutcomeClassV1::FailedIo,
    SweepOutcomeClassV1::FailedCorrupt,
  ];

  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let hash_width = algorithm.hash_length();
    let logical_key = vec![0x31; hash_width];
    let integrity_digest = vec![0x52; hash_width];
    let proposal_hash = vec![0x73; hash_width];
    let void_catalog_hash = vec![0x94; hash_width];
    let database_id = [0x15; 16];
    let batch_id = [0x26; 16];
    let outcomes: Vec<_> = classes
      .iter()
      .enumerate()
      .map(|(index, outcome)| {
        let wal_offset = 1_000 + index as u64 * 100;
        let entity_length = 64 + index as u32;
        let reclaimed = *outcome == SweepOutcomeClassV1::Reclaimed;
        SweepReceiptOutcomeWriteV1 {
          incarnation: PhysicalIncarnationV1 {
            logical_key: &logical_key,
            integrity_or_legacy_digest: &integrity_digest,
            wal_offset,
            write_sequence: 11 + index as u64,
            entity_length,
            entry_type: 1,
            entity_version: 1,
          },
          outcome: *outcome,
          stable_reason_detail: if reclaimed { 0 } else { 100 + index as u16 },
          resulting_void_offset: if reclaimed { wal_offset } else { 0 },
          resulting_void_length: if reclaimed { entity_length } else { 0 },
        }
      })
      .collect();

    let encoded = encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
      hash_algorithm: algorithm,
      recovered: false,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: 17,
      reclaim_committed_at_ms: 1_784_000_000_000,
      proposal_hash: &proposal_hash,
      void_catalog_hash: &void_catalog_hash,
      outcomes: &outcomes,
    })
    .unwrap();
    let SweepVoidArtifactV1::SweepReceipt(receipt) = decode_sweep_void_artifact(&encoded.value, algorithm).unwrap() else {
      panic!("encoded sweep receipt must decode as a receipt")
    };
    assert_eq!((receipt.reclaimed_count, receipt.reclaimed_bytes), (1, 64));
    assert_eq!((receipt.skipped_count, receipt.failed_count), (4, 2));
    let decoded_classes: Vec<_> = receipt.outcome_records(algorithm).unwrap().map(|row| row.unwrap().outcome).collect();
    assert_eq!(decoded_classes, classes);
  }
}

#[test]
fn sweep_writers_reject_invalid_identity_time_hash_count_and_incarnation_fields() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let name = algorithm_name(algorithm);
    let proposal_bytes = fixture(&format!("agca-{name}-sweep-proposal.bin"));
    let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&proposal_bytes, algorithm).unwrap() else {
      panic!("sweep proposal fixture must decode as a proposal")
    };
    let candidates: Vec<_> = proposal.candidate_records(algorithm).unwrap().map(Result::unwrap).collect();
    let database_id: &[u8; 16] = proposal.database_id.try_into().unwrap();
    let batch_id: &[u8; 16] = proposal.batch_id.try_into().unwrap();
    let base = SweepProposalWriteV1 {
      hash_algorithm: algorithm,
      database_id,
      batch_id,
      generation: proposal.generation,
      created_at_ms: proposal.created_at_ms,
      quarantine_manifest_hash: proposal.quarantine_manifest_hash,
      candidates: &candidates,
    };
    let zero_id = [0; 16];
    let zero_hash = vec![0; algorithm.hash_length()];
    let wrong_width_hash = vec![1; algorithm.hash_length() + 1];
    let too_many_candidates = vec![candidates[0]; 4_097];
    let mut invalid_candidate = candidates[0];
    invalid_candidate.wal_offset = 0;

    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { database_id: &zero_id, ..base }).is_err());
    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { batch_id: &zero_id, ..base }).is_err());
    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { generation: 0, ..base }).is_err());
    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { created_at_ms: 0, ..base }).is_err());
    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { quarantine_manifest_hash: &zero_hash, ..base }).is_err());
    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { quarantine_manifest_hash: &wrong_width_hash, ..base }).is_err());
    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { candidates: &too_many_candidates, ..base }).is_err());
    assert!(encode_sweep_proposal_v1(&SweepProposalWriteV1 { candidates: &[invalid_candidate], ..base }).is_err());

    let receipt_bytes = fixture(&format!("agca-{name}-sweep-commit-receipt.bin"));
    let SweepVoidArtifactV1::SweepReceipt(receipt) = decode_sweep_void_artifact(&receipt_bytes, algorithm).unwrap() else {
      panic!("sweep receipt fixture must decode as a receipt")
    };
    let decoded_outcomes: Vec<_> = receipt.outcome_records(algorithm).unwrap().map(Result::unwrap).collect();
    let outcomes: Vec<_> = decoded_outcomes.iter().map(SweepReceiptOutcomeWriteV1::from).collect();
    let receipt_base = SweepReceiptWriteV1 {
      hash_algorithm: algorithm,
      recovered: false,
      database_id,
      batch_id,
      generation: receipt.generation,
      reclaim_committed_at_ms: receipt.reclaim_committed_at_ms,
      proposal_hash: receipt.proposal_hash,
      void_catalog_hash: receipt.void_catalog_hash,
      outcomes: &outcomes,
    };
    let too_many_outcomes = vec![outcomes[0]; 4_097];
    let mut invalid_outcome = outcomes[0];
    invalid_outcome.incarnation.entity_length = 0;

    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { database_id: &zero_id, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { batch_id: &zero_id, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { generation: 0, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { reclaim_committed_at_ms: 0, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { proposal_hash: &zero_hash, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { proposal_hash: &wrong_width_hash, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { void_catalog_hash: &zero_hash, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { outcomes: &[], ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { outcomes: &too_many_outcomes, ..receipt_base }).is_err());
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { outcomes: &[invalid_outcome], ..receipt_base }).is_err());
  }
}

#[test]
fn sweep_receipt_writer_rejects_duplicate_order_and_every_inconsistent_result_shape() {
  let algorithm = HashAlgorithm::Blake3_256;
  let receipt_bytes = fixture("agca-blake3-256-sweep-commit-receipt.bin");
  let SweepVoidArtifactV1::SweepReceipt(receipt) = decode_sweep_void_artifact(&receipt_bytes, algorithm).unwrap() else {
    panic!("sweep receipt fixture must decode as a receipt")
  };
  let decoded_outcomes: Vec<_> = receipt.outcome_records(algorithm).unwrap().map(Result::unwrap).collect();
  let outcomes: Vec<_> = decoded_outcomes.iter().map(SweepReceiptOutcomeWriteV1::from).collect();
  let base = SweepReceiptWriteV1 {
    hash_algorithm: algorithm,
    recovered: false,
    database_id: receipt.database_id.try_into().unwrap(),
    batch_id: receipt.batch_id.try_into().unwrap(),
    generation: receipt.generation,
    reclaim_committed_at_ms: receipt.reclaim_committed_at_ms,
    proposal_hash: receipt.proposal_hash,
    void_catalog_hash: receipt.void_catalog_hash,
    outcomes: &outcomes,
  };
  let duplicate = vec![outcomes[0], outcomes[0]];
  let mut reversed = outcomes.clone();
  reversed.reverse();
  assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { outcomes: &duplicate, ..base }).is_err());
  assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { outcomes: &reversed, ..base }).is_err());

  let mut reclaimed_reason = outcomes[0];
  reclaimed_reason.stable_reason_detail = 1;
  let mut reclaimed_offset = outcomes[0];
  reclaimed_offset.resulting_void_offset += 1;
  let mut skipped_without_reason = outcomes[1];
  skipped_without_reason.stable_reason_detail = 0;
  let mut skipped_with_void = outcomes[1];
  skipped_with_void.resulting_void_offset = skipped_with_void.incarnation.wal_offset;
  skipped_with_void.resulting_void_length = skipped_with_void.incarnation.entity_length;
  for invalid in [reclaimed_reason, reclaimed_offset, skipped_without_reason, skipped_with_void] {
    assert!(encode_sweep_receipt_v1(&SweepReceiptWriteV1 { outcomes: &[invalid], ..base }).is_err());
  }
}

#[test]
fn sweep_record_iterators_latch_the_first_malformed_record() {
  let algorithm = HashAlgorithm::Blake3_256;
  let proposal_bytes = fixture("agca-blake3-256-sweep-proposal.bin");
  let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&proposal_bytes, algorithm).unwrap() else {
    panic!("sweep proposal fixture must decode as a proposal")
  };
  let mut malformed_candidates = proposal.candidates.to_vec();
  malformed_candidates[24 + 2 * algorithm.hash_length() - 1] = 1;
  let malformed_proposal = SweepProposalV1 { candidates: &malformed_candidates, ..proposal.clone() };
  let mut candidates = malformed_proposal.candidate_records(algorithm).unwrap();
  assert!(candidates.next().unwrap().is_err());
  assert!(candidates.next().is_none());

  let receipt_bytes = fixture("agca-blake3-256-sweep-commit-receipt.bin");
  let SweepVoidArtifactV1::SweepReceipt(receipt) = decode_sweep_void_artifact(&receipt_bytes, algorithm).unwrap() else {
    panic!("sweep receipt fixture must decode as a receipt")
  };
  let mut malformed_outcomes = receipt.outcomes.to_vec();
  let outcome_offset = 24 + 2 * algorithm.hash_length();
  malformed_outcomes[outcome_offset..outcome_offset + 2].copy_from_slice(&99u16.to_le_bytes());
  let malformed_receipt = SweepReceiptV1 { outcomes: &malformed_outcomes, ..receipt.clone() };
  let mut outcomes = malformed_receipt.outcome_records(algorithm).unwrap();
  assert!(outcomes.next().unwrap().is_err());
  assert!(outcomes.next().is_none());
}

#[test]
fn sweep_codec_remains_disconnected_from_live_gc_void_and_locator_removal() {
  let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/gc_void.rs")).unwrap();
  for forbidden in ["VoidManager", "remove_entry", "remove_locator", "run_gc", "server::", "DirectoryOps", "StorageEngine"] {
    assert!(!source.contains(forbidden), "sweep codec unexpectedly references {forbidden}");
  }
}
