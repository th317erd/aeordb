use super::*;

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Barrier, mpsc};
use std::time::Duration;

use crate::engine::hot_tail::{HotTailPayload, read_hot_tail_checked};
use crate::engine::kv_stages::initial_block_size;
use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use crate::engine::native_durability::{sync_file_all_native, sync_file_data_native};
use crate::engine::v4::database_header::{
  DATABASE_HEADER_V4_DATA_OFFSET, DATABASE_HEADER_V4_REGION_LENGTH, DATABASE_HEADER_V4_SLOT_LENGTH, encode_database_header_slot,
};
use crate::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1, RetirementJournalRecordWriteV1};
use crate::engine::v4::gc::{
  EncodedGcActiveControlV1, EncodedImmutableGcArtifactV1, GcActiveControlWriteV1, GcArtifactKindV1, PhysicalIncarnationV1,
  encode_gc_active_control,
};
use crate::engine::v4::gc_lifecycle::{
  RootExpiryManifestWriteV1, RootExpiryRecordWriteV1, RootLifecycleManifestWriteV1, RootLifecycleSupportClosureBuilderV1,
  RootLifecycleSupportLimitsV1, RootRetirementCommitWriteV1, decode_root_expiry_manifest_v1, decode_root_lifecycle_manifest_v1,
  decode_root_object_reclaim_proof_v1, decode_root_retirement_commit_v1, encode_root_expiry_manifest_v1, encode_root_expiry_record_v1,
  encode_root_lifecycle_manifest_v1, encode_root_retirement_commit_v1,
};
use crate::engine::v4::gc_mark::{MarkRunCheckpointWriteV1, encode_mark_run_checkpoint};
use crate::engine::v4::gc_quarantine::{
  CandidateDeltaWriteV1, PhysicalQuarantineCandidateClassV1, QuarantineClosureLimitsV1, QuarantineClosureValidatorV1,
  QuarantineManifestWriteV1, decode_physical_quarantine_candidate_v1, decode_quarantine_manifest_v1, encode_candidate_delta_v1,
  encode_physical_quarantine_candidate_v1, encode_quarantine_manifest_v1,
};
use crate::engine::v4::gc_quarantine_publication::{
  PhysicalQuarantinePublicationQualificationRequestV1, qualify_physical_quarantine_publication_v1,
};
use crate::engine::v4::gc_quarantine_transition::{
  PhysicalQuarantineObservationV1, PhysicalQuarantineReachabilityV1, PhysicalQuarantineTransitionContextV1,
  PhysicalQuarantineTransitionModelV1, PhysicalQuarantineTransitionV1,
};
use crate::engine::v4::gc_sweep::{SweepProposalQualificationRequestV1, qualify_sweep_proposal_v1};
use crate::engine::v4::gc_sweep_removal::{
  SweepLocatorRemovalAuthorityErrorV1, SweepLocatorRemovalAuthorityRequestV1, SweepLocatorRemovalAuthoritySnapshotV1,
  SweepLocatorRemovalAuthorityV1, SweepLocatorRemovalBatchOutcomeV1, SweepLocatorRemovalOutcomeV1, complete_sweep_locator_removal_v1,
  reserve_sweep_locator_removal_results_v1,
};
use crate::engine::v4::gc_sweep_reconciliation::{
  ExistingSweepReceiptAuthorityV1, SweepReceiptRecoveryIdentityV1, SweepReceiptReconciliationSourceV1, SweepReceiptVoidAuthorityErrorV1,
  SweepReceiptVoidAuthorityRequestV1, SweepReceiptVoidAuthoritySnapshotV1, SweepReceiptVoidAuthorityV1,
  prepare_sweep_receipt_reconciliation_v1, reserve_sweep_receipt_reconciliation_v1, validate_existing_sweep_receipt_v1,
};
use crate::engine::v4::gc_void::{
  SweepOutcomeClassV1, SweepProposalWriteV1, SweepReceiptOutcomeWriteV1, SweepReceiptWriteV1, SweepVoidArtifactV1,
  VoidCatalogManifestWriteV1, VoidClaimExtentV1, VoidClaimSettlementOutcomeV1, VoidClaimSettlementWriteV1, VoidClaimWriteV1,
  VoidExtentPageWriteV1, VoidExtentRecordV1, decode_sweep_void_artifact, encode_sweep_proposal_v1, encode_sweep_receipt_v1,
  encode_void_catalog_manifest_v1, encode_void_claim_settlement_v1, encode_void_claim_v1, encode_void_extent_page_v1,
};
use crate::engine::v4::gc_void_claim::{
  VoidClaimAdmissionAuthorityErrorV1, VoidClaimAdmissionAuthorityRequestV1, VoidClaimAdmissionAuthoritySnapshotV1,
  VoidClaimAdmissionAuthorityV1, VoidClaimTransitionLimitsV1,
};
use crate::engine::v4::gc_void_publication::{
  VoidCatalogClosureLimitsV1, VoidCatalogPublicationAuthorityErrorV1, VoidCatalogPublicationAuthorityRequestV1,
  VoidCatalogPublicationAuthoritySnapshotV1, VoidCatalogPublicationAuthorityV1,
};
use crate::engine::v4::gc_void_runtime::{
  VoidReclaimReceiptAuthorityErrorV1, VoidReclaimReceiptAuthorityRequestV1, VoidReclaimReceiptAuthoritySnapshotV1,
  VoidReclaimReceiptAuthorityV1, VoidReusableStateLimitsV1,
};
use crate::engine::v4::gc_void_settlement::{
  VoidClaimAllocationDispositionV1, VoidClaimAllocationLimitsV1, VoidClaimAllocationOwnerV1, VoidClaimAllocationSinkV1,
  VoidClaimConsumptionOutcomeV1, VoidClaimDurableUseV1, VoidClaimSettlementAuthorityErrorV1, VoidClaimSettlementAuthorityRequestV1,
  VoidClaimSettlementAuthoritySnapshotV1, VoidClaimSettlementAuthorityV1, VoidClaimSettlementPublicationRequestV1,
  VoidClaimSettlementTransitionLimitsV1, VoidClaimSettlementTransitionValidatorV1, VoidClaimSubrangeV1, VoidClaimWriteFailureV1,
};
use crate::engine::v4::gc_root_reclaim::{
  RootExpiryRetentionContextV1, RootExpiryRetentionModelV1, RootExpiryRetentionPermitV1, RootExpiryRetentionSelectionV1,
  RootObjectReclaimEvidenceVerificationErrorV1, RootObjectReclaimEvidenceVerificationRequestV1, RootObjectReclaimEvidenceVerifierV1,
  RootObjectReclaimQualificationRequestV1, qualify_root_object_reclaim_v1,
};
use crate::engine::v4::gc_mark_workspace::{
  DurableMarkWorkspaceClosureV1, DurableMarkWorkspaceV1, MarkWorkspaceBasisV1, MarkWorkspaceIdentityV1, MarkWorkspaceOptionsV1,
};
use crate::engine::v4::header_publication::HeaderPublicationIo;
use crate::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcPhysicalHintV1, GcStateArtifactV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryWriteV1, GcStatePageWriteV1,
  RootExpiryStateV1, decode_gc_state_artifact, decode_physical_inventory_manifest_v1, decode_root_expiry_record_v1,
  encode_gc_state_directory_v1, encode_gc_state_page_v1,
};
use crate::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use crate::engine::v4::read_view::RootLifecycleObservationV1;
use tokio_util::sync::CancellationToken;

struct TestVoidCatalogPublicationAuthorityV1 {
  snapshot: VoidCatalogPublicationAuthoritySnapshotV1,
  fail_recheck: bool,
  recheck_calls: usize,
}

impl VoidCatalogPublicationAuthorityV1 for TestVoidCatalogPublicationAuthorityV1 {
  fn recheck_void_catalog_publication_authority(
    &mut self,
    request: VoidCatalogPublicationAuthorityRequestV1<'_>,
  ) -> Result<VoidCatalogPublicationAuthoritySnapshotV1, VoidCatalogPublicationAuthorityErrorV1> {
    self.recheck_calls += 1;
    assert_eq!(request.selected_prior_manifest_hash, None);
    assert_eq!(request.selected_prior_control_sequence, 0);
    assert!(request.closure.free_extent_count > 0);
    if self.fail_recheck {
      return Err(VoidCatalogPublicationAuthorityErrorV1::new(
        "void_publication_test_recheck",
        "injected caller-owned Void publication authority failure",
      ));
    }
    Ok(self.snapshot.clone())
  }
}

fn test_void_catalog_publication_authority() -> TestVoidCatalogPublicationAuthorityV1 {
  TestVoidCatalogPublicationAuthorityV1 {
    snapshot: VoidCatalogPublicationAuthoritySnapshotV1 {
      selected_prior_manifest_hash: None,
      selected_prior_control_sequence: 0,
      exact_locator_removal_completion_current: true,
      prior_free_extents_preserved: true,
      prior_outstanding_claims_preserved: true,
      no_unexplained_free_extents_added: true,
      allocator_admission_blocked: true,
      receipt_reconciliation_required: true,
      conflicting_receipt_count: 0,
      repair_latch_clear: true,
    },
    fail_recheck: false,
    recheck_calls: 0,
  }
}

#[derive(Default)]
struct TestVoidReclaimReceiptAuthorityV1 {
  recheck_calls: usize,
  fail_recheck: bool,
}

impl VoidReclaimReceiptAuthorityV1 for TestVoidReclaimReceiptAuthorityV1 {
  fn recheck_void_reclaim_receipt_authority(
    &mut self,
    request: VoidReclaimReceiptAuthorityRequestV1<'_>,
  ) -> Result<VoidReclaimReceiptAuthoritySnapshotV1, VoidReclaimReceiptAuthorityErrorV1> {
    self.recheck_calls += 1;
    if self.fail_recheck {
      return Err(VoidReclaimReceiptAuthorityErrorV1::new(
        "void_runtime_test_recheck",
        "injected receipt-authority reconstruction failure",
      ));
    }
    let reclaim_commit_sequence = request.extent.reclaim_commit_sequence;
    Ok(VoidReclaimReceiptAuthoritySnapshotV1 {
      database_id: request.database_id.try_into().unwrap(),
      selected_manifest_key: request.selected_manifest_key.to_vec(),
      selected_generation: request.selected_generation,
      origin_sweep_proposal_hash: request.extent.origin_sweep_proposal_hash.to_vec(),
      origin_quarantine_manifest_hash: request.extent.origin_quarantine_manifest_hash.to_vec(),
      reclaimed_incarnation_digest: request.extent.reclaimed_incarnation_digest.to_vec(),
      proposal_write_sequence: reclaim_commit_sequence - 1,
      receipt_hash: digest_parts(request.hash_algorithm, &[b"runtime receipt", request.extent.origin_sweep_proposal_hash]),
      receipt_write_sequence: reclaim_commit_sequence + 1,
      reclaim_commit_sequence,
      receipt_reclaimed_offset: request.extent.offset,
      receipt_reclaimed_length: request.extent.length,
      exact_proposal_receipt_current: true,
      locator_removal_durable: true,
      replacement_lineage_complete: true,
      receipt_search_complete: true,
      conflicting_receipt_count: 0,
      repair_latch_clear: true,
    })
  }
}

fn void_runtime_limits() -> VoidReusableStateLimitsV1 {
  VoidReusableStateLimitsV1 { maximum_support_artifacts: 8, maximum_outstanding_claim_extents: 8, maximum_candidate_extents: 8 }
}

struct TestVoidClaimAdmissionAuthorityV1 {
  snapshot: VoidClaimAdmissionAuthoritySnapshotV1,
  expected_source_manifest_hash: Vec<u8>,
  expected_source_control_sequence: u64,
  fail_recheck: bool,
  recheck_calls: usize,
}

impl VoidClaimAdmissionAuthorityV1 for TestVoidClaimAdmissionAuthorityV1 {
  fn recheck_void_claim_admission_authority(
    &mut self,
    request: VoidClaimAdmissionAuthorityRequestV1<'_>,
  ) -> Result<VoidClaimAdmissionAuthoritySnapshotV1, VoidClaimAdmissionAuthorityErrorV1> {
    self.recheck_calls += 1;
    assert_eq!(request.source_manifest.key, self.expected_source_manifest_hash);
    assert_eq!(request.selected_source_control_sequence, self.expected_source_control_sequence);
    assert_eq!(request.transition.claim_key, request.claim.key);
    if self.fail_recheck {
      return Err(VoidClaimAdmissionAuthorityErrorV1::new(
        "void_claim_admission_test_recheck",
        "injected caller-owned Void claim authority failure",
      ));
    }
    Ok(self.snapshot.clone())
  }
}

fn test_void_claim_admission_authority(source_manifest_hash: &[u8], source_control_sequence: u64) -> TestVoidClaimAdmissionAuthorityV1 {
  TestVoidClaimAdmissionAuthorityV1 {
    snapshot: VoidClaimAdmissionAuthoritySnapshotV1 {
      selected_source_manifest_hash: source_manifest_hash.to_vec(),
      selected_source_control_sequence: source_control_sequence,
      source_catalog_receipt_backed: true,
      source_catalog_closure_current: true,
      allocator_admission_excluded: true,
      no_other_claim_admission_active: true,
      in_memory_void_authority_current: true,
      conflicting_receipt_count: 0,
      repair_latch_clear: true,
    },
    expected_source_manifest_hash: source_manifest_hash.to_vec(),
    expected_source_control_sequence: source_control_sequence,
    fail_recheck: false,
    recheck_calls: 0,
  }
}

struct TestVoidClaimSettlementAuthorityV1 {
  snapshot: VoidClaimSettlementAuthoritySnapshotV1,
  expected_source_manifest_hash: Vec<u8>,
  expected_source_control_sequence: u64,
  cancel_during_recheck: Option<CancellationToken>,
  fail_recheck: bool,
  recheck_calls: usize,
}

impl VoidClaimSettlementAuthorityV1 for TestVoidClaimSettlementAuthorityV1 {
  fn recheck_void_claim_settlement_authority(
    &mut self,
    request: VoidClaimSettlementAuthorityRequestV1<'_>,
  ) -> Result<VoidClaimSettlementAuthoritySnapshotV1, VoidClaimSettlementAuthorityErrorV1> {
    self.recheck_calls += 1;
    assert_eq!(request.source_manifest.key, self.expected_source_manifest_hash);
    assert_eq!(request.consumption.source_control_sequence(), self.expected_source_control_sequence);
    assert_eq!(request.transition.claim_key, request.claim.key);
    assert_eq!(request.transition.evidence_digest, request.consumption.evidence_digest());
    if let Some(cancellation) = self.cancel_during_recheck.as_ref() {
      cancellation.cancel();
    }
    if self.fail_recheck {
      return Err(VoidClaimSettlementAuthorityErrorV1::new(
        "void_claim_settlement_test_recheck",
        "injected caller-owned Void settlement authority failure",
      ));
    }
    Ok(self.snapshot.clone())
  }
}

fn test_void_claim_settlement_authority(source_manifest_hash: &[u8], source_control_sequence: u64) -> TestVoidClaimSettlementAuthorityV1 {
  TestVoidClaimSettlementAuthorityV1 {
    snapshot: VoidClaimSettlementAuthoritySnapshotV1 {
      selected_source_manifest_hash: source_manifest_hash.to_vec(),
      selected_source_control_sequence: source_control_sequence,
      source_catalog_receipt_backed: true,
      source_catalog_closure_current: true,
      claim_outstanding_exact: true,
      durable_used_locators_exact: true,
      uncertain_ranges_quarantined: true,
      replacement_lineage_complete: true,
      allocator_settlement_excluded: true,
      no_other_settlement_active: true,
      memory_coordinator_current: true,
      receipt_search_complete: true,
      conflicting_receipt_count: 0,
      existing_receipt: None,
      repair_latch_clear: true,
    },
    expected_source_manifest_hash: source_manifest_hash.to_vec(),
    expected_source_control_sequence: source_control_sequence,
    cancel_during_recheck: None,
    fail_recheck: false,
    recheck_calls: 0,
  }
}

struct TestSweepLocatorRemovalAuthorityV1 {
  snapshot: SweepLocatorRemovalAuthoritySnapshotV1,
  outcomes: Vec<SweepLocatorRemovalOutcomeV1>,
  fail_recheck: bool,
  cancel_during_recheck: bool,
  cancel_during_remove: bool,
  recheck_barriers: Option<(Arc<Barrier>, Arc<Barrier>)>,
  recheck_calls: usize,
  remove_calls: usize,
  observed_proposal_hash: Vec<u8>,
  observed_proposal_write_sequence: u64,
  observed_candidate_count: u32,
}

impl SweepLocatorRemovalAuthorityV1 for TestSweepLocatorRemovalAuthorityV1 {
  fn recheck_sweep_locator_removal_authority(
    &mut self,
    request: SweepLocatorRemovalAuthorityRequestV1<'_>,
  ) -> Result<SweepLocatorRemovalAuthoritySnapshotV1, SweepLocatorRemovalAuthorityErrorV1> {
    self.recheck_calls += 1;
    self.observed_proposal_hash = request.proposal_hash.to_vec();
    self.observed_proposal_write_sequence = request.proposal_write_sequence;
    self.observed_candidate_count = request.proposal.candidate_count;
    if let Some((entered, release)) = &self.recheck_barriers {
      entered.wait();
      release.wait();
    }
    if self.cancel_during_recheck {
      request.cancellation.cancel();
    }
    if self.fail_recheck {
      return Err(SweepLocatorRemovalAuthorityErrorV1::new("sweep_removal_test_recheck", "injected caller-owned sweep authority failure"));
    }
    Ok(self.snapshot.clone())
  }

  fn remove_sweep_locators(&mut self, request: SweepLocatorRemovalAuthorityRequestV1<'_>) -> SweepLocatorRemovalBatchOutcomeV1 {
    self.remove_calls += 1;
    if self.cancel_during_remove {
      request.cancellation.cancel();
    }
    SweepLocatorRemovalBatchOutcomeV1 { reclaim_commit_sequence: request.proposal_write_sequence + 1, outcomes: self.outcomes.clone() }
  }
}

fn test_sweep_locator_removal_authority(
  quarantine_manifest_hash: &[u8],
  generation: u64,
  outcomes: Vec<SweepLocatorRemovalOutcomeV1>,
) -> TestSweepLocatorRemovalAuthorityV1 {
  TestSweepLocatorRemovalAuthorityV1 {
    snapshot: SweepLocatorRemovalAuthoritySnapshotV1 {
      selected_quarantine_manifest_hash: quarantine_manifest_hash.to_vec(),
      selected_mark_generation: generation,
      lifecycle_current: true,
      all_candidates_still_grace_eligible: true,
      all_candidate_incarnations_exact_and_unreachable: true,
      all_locator_and_replacement_states_match: true,
      replacement_lineage_complete: true,
      all_physical_ranges_valid: true,
      request_pin_coordinator_current: true,
      task_and_audit_pins_absent: true,
      protected_family_policy_allows: true,
      repair_latch_clear: true,
    },
    outcomes,
    fail_recheck: false,
    cancel_during_recheck: false,
    cancel_during_remove: false,
    recheck_barriers: None,
    recheck_calls: 0,
    remove_calls: 0,
    observed_proposal_hash: Vec::new(),
    observed_proposal_write_sequence: 0,
    observed_candidate_count: 0,
  }
}

struct TestSweepReceiptVoidAuthorityV1 {
  snapshot: SweepReceiptVoidAuthoritySnapshotV1,
  recovery_outcomes: Vec<SweepLocatorRemovalOutcomeV1>,
  fail_recheck: bool,
  fail_recovery: bool,
  cancel_during_recheck: bool,
  cancel_during_recovery: bool,
  recheck_calls: usize,
  recovery_calls: usize,
}

impl SweepReceiptVoidAuthorityV1 for TestSweepReceiptVoidAuthorityV1 {
  fn recheck_sweep_receipt_void_authority(
    &mut self,
    request: SweepReceiptVoidAuthorityRequestV1<'_>,
  ) -> Result<SweepReceiptVoidAuthoritySnapshotV1, SweepReceiptVoidAuthorityErrorV1> {
    self.recheck_calls += 1;
    if self.cancel_during_recheck {
      request.cancellation.cancel();
    }
    if self.fail_recheck {
      return Err(SweepReceiptVoidAuthorityErrorV1::new("sweep_receipt_test_recheck", "injected selected-Void authority failure"));
    }
    Ok(self.snapshot.clone())
  }

  fn recover_sweep_receipt_outcomes(
    &mut self,
    request: SweepReceiptVoidAuthorityRequestV1<'_>,
  ) -> Result<Vec<SweepLocatorRemovalOutcomeV1>, SweepReceiptVoidAuthorityErrorV1> {
    self.recovery_calls += 1;
    if self.cancel_during_recovery {
      request.cancellation.cancel();
    }
    if self.fail_recovery {
      return Err(SweepReceiptVoidAuthorityErrorV1::new("sweep_receipt_test_recovery", "injected outcome recovery failure"));
    }
    Ok(self.recovery_outcomes.clone())
  }
}

fn test_sweep_receipt_void_authority(
  void_catalog_hash: &[u8],
  outcomes: Vec<SweepLocatorRemovalOutcomeV1>,
) -> TestSweepReceiptVoidAuthorityV1 {
  TestSweepReceiptVoidAuthorityV1 {
    snapshot: SweepReceiptVoidAuthoritySnapshotV1 {
      selected_void_catalog_hash: void_catalog_hash.to_vec(),
      selected_void_catalog_generation: 7,
      reclaim_committed_at_ms: 1_700_000_090_000,
      selected_void_catalog_current: true,
      proposal_catalog_closure_complete: true,
      reclaimed_extents_exact: true,
      nonreclaimed_extents_absent: true,
      locator_removals_durable: true,
      replacement_lineage_complete: true,
      memory_coordinator_current: true,
      allocator_admission_blocked: true,
      receipt_search_complete: true,
      conflicting_receipt_count: 0,
      existing_receipt: None,
      repair_latch_clear: true,
    },
    recovery_outcomes: outcomes,
    fail_recheck: false,
    fail_recovery: false,
    cancel_during_recheck: false,
    cancel_during_recovery: false,
    recheck_calls: 0,
    recovery_calls: 0,
  }
}

#[test]
fn sweep_locator_removal_completion_preserves_both_frozen_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let database_id = [0x31; 16];
    let batch_id = [0x91; 16];
    let logical_key = digest_parts(algorithm, &[b"sweep completion logical key"]);
    let integrity = digest_parts(algorithm, &[b"sweep completion integrity"]);
    let quarantine_manifest_hash = digest_parts(algorithm, &[b"sweep completion quarantine"]);
    let candidates = [PhysicalIncarnationV1 {
      logical_key: &logical_key,
      integrity_or_legacy_digest: &integrity,
      wal_offset: 8_192,
      write_sequence: 77,
      entity_length: 512,
      entry_type: 1,
      entity_version: 1,
    }];
    let artifact = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: 103,
      created_at_ms: 1_700_000_080_001,
      quarantine_manifest_hash: &quarantine_manifest_hash,
      candidates: &candidates,
    })
    .unwrap();
    let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&artifact.value, algorithm).unwrap() else {
      panic!("encoded sweep proposal must decode as a proposal")
    };
    let cancellation = CancellationToken::new();
    let request = SweepLocatorRemovalAuthorityRequestV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: 103,
      proposal_hash: &artifact.key,
      proposal_write_sequence: 417,
      quarantine_manifest_hash: &quarantine_manifest_hash,
      proposal: &proposal,
      cancellation: &cancellation,
    };
    let memory = MemoryCoordinator::new(MemoryPolicy::new(1 << 20, 2 << 20, 1, 1 << 16).unwrap());
    let reservation = reserve_sweep_locator_removal_results_v1(&memory, proposal.candidate_count).unwrap();
    let completion = complete_sweep_locator_removal_v1(
      request,
      SweepLocatorRemovalBatchOutcomeV1 {
        reclaim_commit_sequence: request.proposal_write_sequence + 1,
        outcomes: vec![SweepLocatorRemovalOutcomeV1 {
          ordinal: 0,
          outcome: SweepOutcomeClassV1::Reclaimed,
          stable_reason_detail: 0,
          resulting_void_offset: 8_192,
          resulting_void_length: 512,
        }],
      },
      reservation,
    )
    .unwrap();
    assert_eq!(completion.hash_algorithm(), algorithm);
    assert_eq!(completion.proposal_hash(), artifact.key);
    assert_eq!(completion.quarantine_manifest_hash(), quarantine_manifest_hash);
    assert_eq!(completion.proposal_write_sequence(), 417);
    assert_eq!(completion.reclaim_commit_sequence(), 418);
    assert_eq!(completion.outcomes().len(), 1);
    let invalid_sequence_memory = reserve_sweep_locator_removal_results_v1(&memory, proposal.candidate_count).unwrap();
    let invalid_sequence = complete_sweep_locator_removal_v1(
      request,
      SweepLocatorRemovalBatchOutcomeV1 { reclaim_commit_sequence: 417, outcomes: completion.outcomes().to_vec() },
      invalid_sequence_memory,
    )
    .unwrap_err();
    assert_eq!(invalid_sequence.code(), "sweep_removal_commit_sequence");

    let void_catalog_hash = digest_parts(algorithm, &[b"both-width sweep receipt catalog"]);
    let authority = test_sweep_receipt_void_authority(&void_catalog_hash, completion.outcomes().to_vec());
    let receipt_request = SweepReceiptVoidAuthorityRequestV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: 103,
      proposal_hash: &artifact.key,
      proposal_write_sequence: 417,
      proposal: &proposal,
      recovery: false,
      cancellation: &cancellation,
    };
    let receipt_memory = reserve_sweep_receipt_reconciliation_v1(algorithm, proposal.candidate_count, &memory).unwrap();
    let prepared =
      prepare_sweep_receipt_reconciliation_v1(receipt_request, &authority.snapshot, completion.outcomes(), receipt_memory).unwrap();
    let SweepVoidArtifactV1::SweepReceipt(receipt) = decode_sweep_void_artifact(&prepared.artifact.value, algorithm).unwrap() else {
      panic!("prepared sweep receipt must decode as a receipt")
    };
    assert!(!receipt.recovered);
    assert_eq!(receipt.proposal_hash, artifact.key);
    assert_eq!(receipt.void_catalog_hash, void_catalog_hash);

    let conflicting_outcomes = [SweepReceiptOutcomeWriteV1 {
      incarnation: candidates[0],
      outcome: SweepOutcomeClassV1::SkippedChanged,
      stable_reason_detail: 17,
      resulting_void_offset: 0,
      resulting_void_length: 0,
    }];
    let conflicting_artifact = encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
      hash_algorithm: algorithm,
      recovered: false,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: 103,
      reclaim_committed_at_ms: authority.snapshot.reclaim_committed_at_ms,
      proposal_hash: &artifact.key,
      void_catalog_hash: &void_catalog_hash,
      outcomes: &conflicting_outcomes,
    })
    .unwrap();
    let SweepVoidArtifactV1::SweepReceipt(conflicting_receipt) =
      decode_sweep_void_artifact(&conflicting_artifact.value, algorithm).unwrap()
    else {
      panic!("conflicting sweep receipt must decode as a receipt")
    };
    assert_eq!(
      validate_existing_sweep_receipt_v1(receipt_request, &authority.snapshot, &conflicting_receipt, Some(completion.outcomes()))
        .unwrap_err()
        .code(),
      "sweep_receipt_existing_conflict"
    );
  }
}

struct PreparedVoidCatalogPublicationV1 {
  database_id: [u8; 16],
  completion: SweepLocatorRemovalCompletionPermitV1,
  extent_page: EncodedImmutableGcArtifactV1,
  directory: EncodedImmutableGcArtifactV1,
  manifest: EncodedImmutableGcArtifactV1,
  control: EncodedGcActiveControlV1,
}

impl PreparedVoidCatalogPublicationV1 {
  fn request<'a>(
    &'a self,
    cancellation: &'a CancellationToken,
    memory: &'a MemoryCoordinator,
    maximum_support_artifacts: u64,
    monotonic_now_ms: u64,
  ) -> VoidCatalogPublicationRequestV1<'a> {
    VoidCatalogPublicationRequestV1 {
      completion: &self.completion,
      manifest: &self.manifest,
      control: &self.control,
      publication_timestamp_ms: 1_700_000_080_012,
      monotonic_now_ms,
      cancellation,
      memory,
      closure_limits: VoidCatalogClosureLimitsV1 { maximum_support_artifacts },
    }
  }
}

fn prepare_void_catalog_publication(
  publisher: &V4FirstAuthorityPublisher,
  completion_memory: &MemoryCoordinator,
  reclaim_commit_sequence_delta: u64,
) -> PreparedVoidCatalogPublicationV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let batch_id = [0x91; 16];
  let catalog_id = [0x81; 16];
  let logical_key = digest_parts(algorithm, &[b"Void publication logical key"]);
  let integrity = digest_parts(algorithm, &[b"Void publication integrity"]);
  let quarantine_manifest_hash = digest_parts(algorithm, &[b"Void publication quarantine"]);
  let candidate = PhysicalIncarnationV1 {
    logical_key: &logical_key,
    integrity_or_legacy_digest: &integrity,
    wal_offset: 8_192,
    write_sequence: 77,
    entity_length: 512,
    entry_type: 1,
    entity_version: 1,
  };
  let proposal_artifact = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: 103,
    created_at_ms: 1_700_000_080_001,
    quarantine_manifest_hash: &quarantine_manifest_hash,
    candidates: &[candidate],
  })
  .unwrap();
  let proposal_write_sequence = publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::SweepProposal,
        database_id: &database_id,
        artifact_key: &proposal_artifact.key,
        value: &proposal_artifact.value,
        minimum_timestamp_ms: 1_700_000_080_001,
        committed_postcondition_code: "test_void_proposal_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&proposal_artifact.value, algorithm).unwrap() else {
    panic!("encoded sweep proposal must decode")
  };
  let cancellation = CancellationToken::new();
  let completion = complete_sweep_locator_removal_v1(
    SweepLocatorRemovalAuthorityRequestV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: proposal.generation,
      proposal_hash: &proposal_artifact.key,
      proposal_write_sequence,
      quarantine_manifest_hash: &quarantine_manifest_hash,
      proposal: &proposal,
      cancellation: &cancellation,
    },
    SweepLocatorRemovalBatchOutcomeV1 {
      reclaim_commit_sequence: proposal_write_sequence + 1,
      outcomes: vec![SweepLocatorRemovalOutcomeV1 {
        ordinal: 0,
        outcome: SweepOutcomeClassV1::Reclaimed,
        stable_reason_detail: 0,
        resulting_void_offset: candidate.wal_offset,
        resulting_void_length: candidate.entity_length,
      }],
    },
    reserve_sweep_locator_removal_results_v1(&completion_memory, 1).unwrap(),
  )
  .unwrap();
  let mut encoded_incarnation = vec![0u8; 24 + 2 * algorithm.hash_length()];
  crate::engine::v4::gc::encode_physical_incarnation_into(&mut encoded_incarnation, &candidate, algorithm).unwrap();
  let incarnation_digest = digest_parts(algorithm, &[&encoded_incarnation]);
  let extent_page = encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    catalog_id: &catalog_id,
    generation: 1,
    page_id: 1,
    extents: &[VoidExtentRecordV1 {
      offset: candidate.wal_offset,
      length: candidate.entity_length,
      origin_sweep_proposal_hash: &proposal_artifact.key,
      origin_quarantine_manifest_hash: &quarantine_manifest_hash,
      reclaimed_incarnation_digest: &incarnation_digest,
      reclaim_commit_sequence: completion.reclaim_commit_sequence().checked_add(reclaim_commit_sequence_delta).unwrap(),
      void_generation: 1,
    }],
  })
  .unwrap();
  let SweepVoidArtifactV1::VoidExtentPage(decoded_page) = decode_sweep_void_artifact(&extent_page.value, algorithm).unwrap() else {
    panic!("encoded Void page must decode")
  };
  let lower_fence = decoded_page.lower_offset.to_le_bytes();
  let upper_fence = decoded_page.upper_offset.to_le_bytes();
  let directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::FreeExtents,
    database_id: &database_id,
    catalog_id: &catalog_id,
    generation: 1,
    level: 0,
    entries: &[GcStateDirectoryEntryWriteV1 {
      lower_fence: &lower_fence,
      upper_fence: &upper_fence,
      child_hash: &extent_page.key,
      child_generation: 1,
      live_count: 1,
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: u64::from(candidate.entity_length),
      minimum_page_id: 1,
      maximum_page_id: 1,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  let manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: 1,
    published_at_ms: 1_700_000_080_011,
    free_root: Some(&directory.key),
    claim_root: None,
    next_page_id: 2,
    free_count: 1,
    free_bytes: u64::from(candidate.entity_length),
    claim_count: 0,
    claimed_bytes: 0,
    previous_control_sequence: 0,
  })
  .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 0,
    sequence: 1,
    generation: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  PreparedVoidCatalogPublicationV1 { database_id, completion, extent_page, directory, manifest, control }
}

fn publish_prepared_void_catalog_support(publisher: &V4FirstAuthorityPublisher, prepared: &PreparedVoidCatalogPublicationV1) {
  for artifact in [&prepared.extent_page, &prepared.directory] {
    publisher
      .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
        database_id: &prepared.database_id,
        artifact,
        publication_timestamp_ms: 1_700_000_080_010,
      })
      .unwrap();
  }
}

fn selected_void_catalog_manifest_key(publisher: &V4FirstAuthorityPublisher) -> Option<Vec<u8>> {
  let observation = publisher.observe().unwrap();
  let kv = publisher.lock_kv().unwrap();
  select_void_catalog_control(&publisher.file, &kv, &observation.selected.header).unwrap().map(|control| control.target_manifest_hash)
}

#[test]
fn selected_void_runtime_reconstructs_source_and_restart_without_v3_evidence() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("void-runtime-source", None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut absent_authority = TestVoidReclaimReceiptAuthorityV1::default();
  assert!(publisher
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &memory, limits: void_runtime_limits() },
      &mut absent_authority,
    )
    .unwrap()
    .is_none());
  assert_eq!(absent_authority.recheck_calls, 0);

  let source = prepare_void_catalog_publication(&publisher, &memory, 0);
  publish_prepared_void_catalog_support(&publisher, &source);
  let mut publication_authority = test_void_catalog_publication_authority();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let _publication_receipt = publisher
    .publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut publication_authority, &mut retirement_owner)
    .unwrap();
  let mut receipt_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let state = publisher
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &memory, limits: void_runtime_limits() },
      &mut receipt_authority,
    )
    .unwrap()
    .unwrap();
  assert_eq!(state.selected_manifest_key(), source.manifest.key);
  assert_eq!(state.free_count(), 1);
  assert_eq!(state.free_bytes(), 512);
  assert_eq!(state.outstanding_claim_count(), 0);
  assert_eq!(state.candidate_extents().len(), 1);
  assert_eq!(receipt_authority.recheck_calls, 1);
  drop(state);
  let canceled = CancellationToken::new();
  canceled.cancel();
  let mut canceled_authority = TestVoidReclaimReceiptAuthorityV1::default();
  assert_eq!(
    publisher
      .reconstruct_void_reusable_state(
        VoidReusableStateReconstructionRequestV1 { cancellation: &canceled, memory: &memory, limits: void_runtime_limits() },
        &mut canceled_authority,
      )
      .unwrap_err()
      .code(),
    "void_runtime_canceled"
  );
  assert_eq!(canceled_authority.recheck_calls, 0);
  let mut failed_authority = TestVoidReclaimReceiptAuthorityV1 { fail_recheck: true, ..Default::default() };
  assert_eq!(
    publisher
      .reconstruct_void_reusable_state(
        VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &memory, limits: void_runtime_limits() },
        &mut failed_authority,
      )
      .unwrap_err()
      .code(),
    "void_runtime_free_support"
  );
  assert_eq!(failed_authority.recheck_calls, 1);
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, reopened) = reopen(&path);
  let restart_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let mut restart_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let restarted = reopened
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &restart_memory, limits: void_runtime_limits() },
      &mut restart_authority,
    )
    .unwrap()
    .unwrap();
  assert_eq!(restarted.selected_manifest_key(), source.manifest.key);
  assert_eq!(restarted.free_count(), 1);
  assert_eq!(restarted.candidate_extents().len(), 1);
  assert_eq!(restart_authority.recheck_calls, 1);
}

#[test]
fn selected_void_runtime_refuses_corrupt_support_without_receipt_authority() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("void-runtime-corrupt-support", None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let source = prepare_void_catalog_publication(&publisher, &memory, 0);
  publish_prepared_void_catalog_support(&publisher, &source);
  let mut publication_authority = test_void_catalog_publication_authority();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let _publication_receipt = publisher
    .publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut publication_authority, &mut retirement_owner)
    .unwrap();
  corrupt_last_entity_byte(&publisher, &source.directory.key);
  let reserved_before_reconstruction = memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes;

  let mut receipt_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let error = publisher
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &memory, limits: void_runtime_limits() },
      &mut receipt_authority,
    )
    .unwrap_err();
  assert!(matches!(error.code(), "integrity_hash_mismatch" | "void_runtime_free_support"));
  assert_eq!(receipt_authority.recheck_calls, 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, reserved_before_reconstruction);
}

struct PreparedVoidClaimAdmissionV1 {
  claim: EncodedImmutableGcArtifactV1,
  claim_directory: EncodedImmutableGcArtifactV1,
  result_manifest: EncodedImmutableGcArtifactV1,
  result_control: EncodedGcActiveControlV1,
}

impl PreparedVoidClaimAdmissionV1 {
  fn request<'a>(
    &'a self,
    cancellation: &'a CancellationToken,
    memory: &'a MemoryCoordinator,
    maximum_support_artifacts_per_catalog: u64,
    monotonic_now_ms: u64,
  ) -> VoidClaimAdmissionRequestV1<'a> {
    VoidClaimAdmissionRequestV1 {
      claim: &self.claim,
      result_manifest: &self.result_manifest,
      result_control: &self.result_control,
      publication_timestamp_ms: 1_700_000_080_015,
      monotonic_now_ms,
      cancellation,
      memory,
      transition_limits: VoidClaimTransitionLimitsV1 { maximum_support_artifacts_per_catalog },
    }
  }
}

fn prepare_void_claim_admission(source: &PreparedVoidCatalogPublicationV1) -> PreparedVoidClaimAdmissionV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let claim_id = [0xa1; 16];
  let requesting_boot_id = [0xb1; 16];
  let requesting_task_or_batch_id = [0xc1; 16];
  let claims_catalog_id = [0xd1; 16];
  let SweepVoidArtifactV1::VoidExtentPage(source_page) = decode_sweep_void_artifact(&source.extent_page.value, algorithm).unwrap() else {
    panic!("prepared source extent page must decode")
  };
  let source_extent = source_page.extent_records().unwrap().next().unwrap().unwrap();
  let claim = encode_void_claim_v1(&VoidClaimWriteV1 {
    hash_algorithm: algorithm,
    database_id: &source.database_id,
    claim_id: &claim_id,
    generation: 2,
    created_at_ms: 1_700_000_080_013,
    requesting_boot_id: &requesting_boot_id,
    requesting_task_or_batch_id: &requesting_task_or_batch_id,
    source_manifest_hash: &source.manifest.key,
    extents: &[VoidClaimExtentV1 {
      offset: source_extent.offset,
      length: source_extent.length,
      origin_sweep_proposal_hash: source_extent.origin_sweep_proposal_hash,
    }],
  })
  .unwrap();
  let claim_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::Claims,
    database_id: &source.database_id,
    catalog_id: &claims_catalog_id,
    generation: 2,
    level: 0,
    entries: &[GcStateDirectoryEntryWriteV1 {
      lower_fence: &claim_id,
      upper_fence: &claim_id,
      child_hash: &claim.key,
      child_generation: 2,
      live_count: 1,
      tombstone_count: 0,
      page_count: 0,
      logical_bytes: u64::try_from(claim.value.len()).unwrap(),
      minimum_page_id: 0,
      maximum_page_id: 0,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  let result_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &source.database_id,
    generation: 2,
    published_at_ms: 1_700_000_080_014,
    free_root: None,
    claim_root: Some(&claim_directory.key),
    next_page_id: 2,
    free_count: 0,
    free_bytes: 0,
    claim_count: 1,
    claimed_bytes: u64::from(source_extent.length),
    previous_control_sequence: 1,
  })
  .unwrap();
  let result_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogActiveControl,
    hash_algorithm: algorithm,
    database_id: &source.database_id,
    slot: 1,
    sequence: 2,
    generation: 2,
    target_manifest_hash: &result_manifest.key,
  })
  .unwrap();
  PreparedVoidClaimAdmissionV1 { claim, claim_directory, result_manifest, result_control }
}

#[test]
fn void_catalog_support_publishes_before_selector_and_retry_remains_reuse_blocked() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("void-catalog-publication", None);
  publish_first_authority(&publisher);
  let completion_memory = MemoryCoordinator::new(MemoryPolicy::new(4 << 20, 8 << 20, 1, 1 << 20).unwrap());
  let prepared = prepare_void_catalog_publication(&publisher, &completion_memory, 0);
  let publication_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let mut authority = test_void_catalog_publication_authority();
  let cancellation = CancellationToken::new();
  publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &prepared.database_id,
      artifact: &prepared.extent_page,
      publication_timestamp_ms: 1_700_000_080_010,
    })
    .unwrap();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    prepared.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &publication_memory,
  )
  .unwrap();
  let canceled = CancellationToken::new();
  canceled.cancel();
  let canceled_error = publisher
    .publish_void_catalog(prepared.request(&canceled, &publication_memory, 2, 1), &mut authority, &mut retirement_owner)
    .unwrap_err();
  assert_eq!(canceled_error.code(), "void_publication_canceled");
  assert_eq!(authority.recheck_calls, 0);

  let publication_request = prepared.request(&cancellation, &publication_memory, 2, 1);
  let missing_error = publisher.publish_void_catalog(publication_request, &mut authority, &mut retirement_owner).unwrap_err();
  assert_eq!(missing_error.code(), "void_publication_support_missing");
  assert_eq!(authority.recheck_calls, 0);
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.control.key).unwrap().is_none());

  publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &prepared.database_id,
      artifact: &prepared.directory,
      publication_timestamp_ms: 1_700_000_080_010,
    })
    .unwrap();
  let limited_error = publisher
    .publish_void_catalog(prepared.request(&cancellation, &publication_memory, 1, 1), &mut authority, &mut retirement_owner)
    .unwrap_err();
  assert_eq!(limited_error.code(), "void_closure_artifact_limit");
  assert_eq!(authority.recheck_calls, 0);
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.control.key).unwrap().is_none());

  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
  let pressure_error = publisher
    .publish_void_catalog(prepared.request(&cancellation, &constrained_memory, 2, 1), &mut authority, &mut retirement_owner)
    .unwrap_err();
  assert!(matches!(pressure_error.code(), "void_publication_digest_memory" | "void_publication_support_memory"));
  assert_eq!(authority.recheck_calls, 0);
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.control.key).unwrap().is_none());

  authority.snapshot.allocator_admission_blocked = false;
  let authority_error = publisher
    .publish_void_catalog(prepared.request(&cancellation, &publication_memory, 2, 1), &mut authority, &mut retirement_owner)
    .unwrap_err();
  assert_eq!(authority_error.code(), "void_publication_authority_changed");
  assert_eq!(authority.recheck_calls, 1);
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.control.key).unwrap().is_none());
  authority.snapshot.allocator_admission_blocked = true;

  let error = publisher
    .publish_void_catalog_with_control_observer(
      prepared.request(&cancellation, &publication_memory, 2, 1),
      &mut authority,
      &mut retirement_owner,
      &mut FailingPostCommitObserver,
    )
    .unwrap_err();
  let receipt = error.committed_receipt().expect("a selected Void catalog must return its exact receipt after post-commit failure");
  assert_eq!(error.code(), "gc_control_committed_postcondition_failure");
  assert_eq!(authority.recheck_calls, 2);
  assert!(receipt.receipt_reconciliation_required);
  assert!(receipt.reuse_blocked);
  assert!(!receipt.idempotent);
  assert_eq!(receipt.manifest_key, prepared.manifest.key);
  assert_eq!(receipt.control_key, prepared.control.key);
  drop(retirement_owner);
  drop(publisher);

  let (_coordinator, mut reopened) = reopen(&path);
  let retry_cancellation = CancellationToken::new();
  let retry_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    prepared.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &retry_cancellation,
    &retry_memory,
  )
  .unwrap();
  let mut retry_authority = test_void_catalog_publication_authority();
  retry_authority.fail_recheck = true;
  let retry = reopened
    .publish_void_catalog(prepared.request(&retry_cancellation, &retry_memory, 2, 2), &mut retry_authority, &mut retry_owner)
    .unwrap();
  assert_eq!(retry_authority.recheck_calls, 0, "exact selected retry must not rerun stale external authority");
  assert!(retry.idempotent);
  assert!(retry.receipt_reconciliation_required);
  assert!(retry.reuse_blocked);
}

#[test]
fn void_claim_selects_source_minus_claim_before_returning_exact_allocation_authority() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("void-claim-admission", None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let source = prepare_void_catalog_publication(&publisher, &memory, 0);
  publish_prepared_void_catalog_support(&publisher, &source);
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut source_authority = test_void_catalog_publication_authority();
  let source_receipt =
    publisher.publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut source_authority, &mut retirement_owner).unwrap();
  assert_eq!(source_receipt.manifest_key, source.manifest.key);
  assert_eq!(selected_void_catalog_manifest_key(&publisher), Some(source.manifest.key.clone()));
  let mut source_runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let source_runtime_state = publisher
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &memory, limits: void_runtime_limits() },
      &mut source_runtime_authority,
    )
    .unwrap()
    .unwrap();
  assert_eq!(source_runtime_state.selected_manifest_key(), source.manifest.key);
  assert_eq!(source_runtime_state.free_count(), 1);
  drop(source_runtime_state);

  let prepared = prepare_void_claim_admission(&source);
  let bypass_error = publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &source.database_id,
      artifact: &prepared.claim,
      publication_timestamp_ms: 1_700_000_080_015,
    })
    .unwrap_err();
  assert_eq!(bypass_error.code(), "void_support_claim_owner");
  assert!(publisher.locator(&prepared.claim.key).unwrap().is_none());
  publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &source.database_id,
      artifact: &prepared.claim_directory,
      publication_timestamp_ms: 1_700_000_080_015,
    })
    .unwrap();

  let mut claim_authority = test_void_claim_admission_authority(&source.manifest.key, 1);
  claim_authority.snapshot.allocator_admission_excluded = false;
  let refused =
    publisher.admit_void_claim(prepared.request(&cancellation, &memory, 4, 2), &mut claim_authority, &mut retirement_owner).unwrap_err();
  assert_eq!(refused.code(), "void_claim_admission_authority_incomplete");
  assert_eq!(claim_authority.recheck_calls, 1);
  assert!(publisher.locator(&prepared.claim.key).unwrap().is_some(), "failed admission may retain only immutable claim evidence");
  assert!(publisher.locator(&prepared.result_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.result_control.key).unwrap().is_none());
  assert_eq!(selected_void_catalog_manifest_key(&publisher), Some(source.manifest.key.clone()));

  claim_authority.snapshot.allocator_admission_excluded = true;
  let permit =
    publisher.admit_void_claim(prepared.request(&cancellation, &memory, 4, 3), &mut claim_authority, &mut retirement_owner).unwrap();
  assert_eq!(claim_authority.recheck_calls, 2);
  assert_eq!(permit.database_id(), source.database_id);
  assert_eq!(permit.claim_key(), prepared.claim.key);
  assert_eq!(permit.source_manifest_key(), source.manifest.key);
  assert_eq!(permit.result_manifest_key(), prepared.result_manifest.key);
  assert_eq!(permit.result_control_key(), prepared.result_control.key);
  assert_eq!(permit.claimed_extents().len(), 1);
  assert_eq!(permit.claimed_extents()[0].offset, 8_192);
  assert_eq!(permit.claimed_extents()[0].length, 512);
  assert_eq!(permit.claimed_bytes(), 512);
  assert_eq!(permit.generation(), 2);
  assert!(!permit.idempotent());
  assert_eq!(selected_void_catalog_manifest_key(&publisher), Some(prepared.result_manifest.key.clone()));
  let mut runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let outstanding_state = publisher
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &memory, limits: void_runtime_limits() },
      &mut runtime_authority,
    )
    .unwrap()
    .unwrap();
  assert_eq!(outstanding_state.selected_manifest_key(), prepared.result_manifest.key);
  assert_eq!(outstanding_state.free_count(), 0);
  assert_eq!(outstanding_state.outstanding_claim_count(), 1);
  assert_eq!(outstanding_state.claimed_bytes(), 512);
  assert!(outstanding_state.candidate_extents().is_empty());
  assert_eq!(runtime_authority.recheck_calls, 0);
  drop(outstanding_state);
  drop(permit);
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  let retry_cancellation = CancellationToken::new();
  let retry_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &retry_cancellation,
    &retry_memory,
  )
  .unwrap();
  let mut retry_authority = test_void_claim_admission_authority(&source.manifest.key, 1);
  retry_authority.fail_recheck = true;
  let retry =
    reopened.admit_void_claim(prepared.request(&retry_cancellation, &retry_memory, 4, 4), &mut retry_authority, &mut retry_owner).unwrap();
  assert_eq!(retry_authority.recheck_calls, 0, "exact selected retry must not rerun stale source authority");
  assert!(retry.idempotent());
  assert_eq!(retry.claim_key(), prepared.claim.key);
  assert_eq!(selected_void_catalog_manifest_key(&reopened), Some(prepared.result_manifest.key.clone()));
  let mut runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let restarted = reopened
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 { cancellation: &retry_cancellation, memory: &retry_memory, limits: void_runtime_limits() },
      &mut runtime_authority,
    )
    .unwrap()
    .unwrap();
  assert_eq!(restarted.selected_manifest_key(), prepared.result_manifest.key);
  assert_eq!(restarted.outstanding_claim_count(), 1);
  assert!(restarted.candidate_extents().is_empty());
  assert_eq!(runtime_authority.recheck_calls, 0);
}

struct MixedVoidClaimAllocationSinkV1 {
  calls: u32,
}

impl VoidClaimAllocationSinkV1 for MixedVoidClaimAllocationSinkV1 {
  fn consume_void_claim_subrange(&mut self, request: VoidClaimSubrangeV1<'_>) -> Result<VoidClaimDurableUseV1, VoidClaimWriteFailureV1> {
    let call = self.calls;
    self.calls += 1;
    match call {
      0 => Ok(VoidClaimDurableUseV1 {
        logical_key: vec![0x51; 32],
        integrity_digest: vec![0x52; 32],
        wal_offset: request.offset,
        write_sequence: 91,
        entity_length: request.length,
        entry_type: 1,
        entity_version: 1,
      }),
      1 => Err(VoidClaimWriteFailureV1::DefinitelyUnwritten { reason_code: 7 }),
      2 => Err(VoidClaimWriteFailureV1::PossiblyWritten { reason_code: 9, evidence_digest: vec![0x53; 32] }),
      _ => panic!("unexpected allocation call"),
    }
  }
}

fn prepare_void_claim_allocation_permit(
  name: &str,
  hash_algorithm: HashAlgorithm,
) -> (tempfile::TempDir, MemoryCoordinator, CancellationToken, VoidClaimAdmissionPermitV1) {
  let (directory, _path, _coordinator, mut publisher) = create_environment(name, None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let source = prepare_void_catalog_publication(&publisher, &memory, 0);
  publish_prepared_void_catalog_support(&publisher, &source);
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut source_authority = test_void_catalog_publication_authority();
  let _ =
    publisher.publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut source_authority, &mut retirement_owner).unwrap();
  let prepared = prepare_void_claim_admission(&source);
  publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &source.database_id,
      artifact: &prepared.claim_directory,
      publication_timestamp_ms: 1_700_000_080_015,
    })
    .unwrap();
  let mut claim_authority = test_void_claim_admission_authority(&source.manifest.key, 1);
  let mut permit =
    publisher.admit_void_claim(prepared.request(&cancellation, &memory, 4, 2), &mut claim_authority, &mut retirement_owner).unwrap();
  if hash_algorithm != HashAlgorithm::Blake3_256 {
    let hash_width = hash_algorithm.hash_length();
    permit.hash_algorithm = hash_algorithm;
    permit.claim_key = vec![0x61; hash_width];
    permit.source_manifest_key = vec![0x62; hash_width];
    permit.result_manifest_key = vec![0x63; hash_width];
    permit.result_control_key = vec![0x64; hash_width];
    for extent in permit.claimed_extents.iter_mut() {
      extent.origin_sweep_proposal_hash = vec![0x65; hash_width];
      extent.origin_quarantine_manifest_hash = vec![0x66; hash_width];
      extent.reclaimed_incarnation_digest = vec![0x67; hash_width];
    }
  }
  (directory, memory, cancellation, permit)
}

struct DurableThenUnusedVoidClaimSinkV1 {
  hash_algorithm: HashAlgorithm,
  calls: u32,
}

impl VoidClaimAllocationSinkV1 for DurableThenUnusedVoidClaimSinkV1 {
  fn consume_void_claim_subrange(&mut self, request: VoidClaimSubrangeV1<'_>) -> Result<VoidClaimDurableUseV1, VoidClaimWriteFailureV1> {
    self.calls += 1;
    if self.calls == 1 {
      return Ok(VoidClaimDurableUseV1 {
        logical_key: vec![0x71; self.hash_algorithm.hash_length()],
        integrity_digest: vec![0x72; self.hash_algorithm.hash_length()],
        wal_offset: request.offset,
        write_sequence: 101,
        entity_length: request.length,
        entry_type: 1,
        entity_version: 1,
      });
    }
    Err(VoidClaimWriteFailureV1::DefinitelyUnwritten { reason_code: 7 })
  }
}

struct InvalidDurableVoidClaimSinkV1;

impl VoidClaimAllocationSinkV1 for InvalidDurableVoidClaimSinkV1 {
  fn consume_void_claim_subrange(&mut self, request: VoidClaimSubrangeV1<'_>) -> Result<VoidClaimDurableUseV1, VoidClaimWriteFailureV1> {
    Ok(VoidClaimDurableUseV1 {
      logical_key: vec![0x81; 32],
      integrity_digest: vec![0x82; 32],
      wal_offset: request.offset + 1,
      write_sequence: 102,
      entity_length: request.length,
      entry_type: 1,
      entity_version: 1,
    })
  }
}

struct InvalidFailureVoidClaimSinkV1;

impl VoidClaimAllocationSinkV1 for InvalidFailureVoidClaimSinkV1 {
  fn consume_void_claim_subrange(&mut self, _request: VoidClaimSubrangeV1<'_>) -> Result<VoidClaimDurableUseV1, VoidClaimWriteFailureV1> {
    Err(VoidClaimWriteFailureV1::DefinitelyUnwritten { reason_code: 0 })
  }
}

struct CancelingUncertainVoidClaimSinkV1 {
  cancellation: CancellationToken,
  hash_width: usize,
}

impl VoidClaimAllocationSinkV1 for CancelingUncertainVoidClaimSinkV1 {
  fn consume_void_claim_subrange(&mut self, _request: VoidClaimSubrangeV1<'_>) -> Result<VoidClaimDurableUseV1, VoidClaimWriteFailureV1> {
    self.cancellation.cancel();
    Err(VoidClaimWriteFailureV1::PossiblyWritten { reason_code: 9, evidence_digest: vec![0x91; self.hash_width] })
  }
}

#[test]
fn void_claim_allocator_is_deterministic_and_bounded_at_both_hash_widths() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, memory, cancellation, permit) =
      prepare_void_claim_allocation_permit(&format!("void-claim-both-widths-{hash_algorithm:?}"), hash_algorithm);
    let reserved_before_owner = memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes;
    let mut owner =
      VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 2 }, &memory, cancellation).unwrap();
    let mut sink = DurableThenUnusedVoidClaimSinkV1 { hash_algorithm, calls: 0 };
    assert!(matches!(
      owner.consume(128, &mut sink).unwrap(),
      VoidClaimAllocationDispositionV1::Durable { ordinal: 0, wal_offset: 8_192, entity_length: 128, write_sequence: 101 }
    ));
    assert!(matches!(
      owner.consume(64, &mut sink).unwrap(),
      VoidClaimAllocationDispositionV1::DefinitelyUnused { ordinal: 1, offset: 8_320, length: 64, reason_code: 7 }
    ));
    assert_eq!(owner.consume(1, &mut sink).unwrap_err().code(), "void_claim_allocation_limit");
    let consumption = owner.finish().unwrap();
    assert_eq!(consumption.outcome(), VoidClaimConsumptionOutcomeV1::Settled);
    assert_eq!(consumption.used_bytes(), 128);
    assert_eq!(consumption.returned_bytes(), 384);
    assert_eq!(consumption.uncertain_bytes(), 0);
    assert_eq!(consumption.evidence_digest().len(), hash_algorithm.hash_length());
    drop(consumption);
    assert!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes < reserved_before_owner);
  }
}

#[test]
fn void_claim_zero_durable_use_abandons_the_complete_claim_to_quarantine_at_both_hash_widths() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, memory, cancellation, permit) =
      prepare_void_claim_allocation_permit(&format!("void-claim-abandon-{hash_algorithm:?}"), hash_algorithm);
    let owner =
      VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 4 }, &memory, cancellation).unwrap();
    let consumption = owner.finish().unwrap();
    assert_eq!(consumption.outcome(), VoidClaimConsumptionOutcomeV1::AbandonedToQuarantine);
    assert!(consumption.durable_uses().is_empty());
    assert!(consumption.returned_extents().is_empty());
    assert_eq!(consumption.returned_bytes(), 0);
    assert_eq!(consumption.uncertain_bytes(), consumption.claimed_bytes());
    assert_eq!(consumption.evidence_digest().len(), hash_algorithm.hash_length());
  }
}

#[test]
fn void_claim_malformed_or_ambiguous_sink_results_latch_the_owner_and_never_return_bytes() {
  let cases: [(&str, Box<dyn VoidClaimAllocationSinkV1>); 2] =
    [("durable", Box::new(InvalidDurableVoidClaimSinkV1)), ("failure", Box::new(InvalidFailureVoidClaimSinkV1))];
  for (name, mut sink) in cases {
    let (_directory, memory, cancellation, permit) = prepare_void_claim_allocation_permit(name, HashAlgorithm::Blake3_256);
    let mut owner =
      VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 4 }, &memory, cancellation).unwrap();
    let error = owner.consume(64, sink.as_mut()).unwrap_err();
    assert!(matches!(error.code(), "void_claim_allocation_durable_receipt" | "void_claim_allocation_sink_failure"));
    assert_eq!(owner.consume(1, sink.as_mut()).unwrap_err().code(), "void_claim_allocation_failed");
    assert_eq!(owner.finish().unwrap_err().code(), "void_claim_allocation_failed");
  }
}

#[test]
fn void_claim_cancellation_after_a_possible_write_retains_the_entire_outstanding_claim() {
  let (_directory, memory, cancellation, permit) = prepare_void_claim_allocation_permit("void-claim-cancel", HashAlgorithm::Blake3_256);
  let mut owner =
    VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 4 }, &memory, cancellation.clone()).unwrap();
  let mut sink = CancelingUncertainVoidClaimSinkV1 { cancellation, hash_width: HashAlgorithm::Blake3_256.hash_length() };
  assert!(matches!(
    owner.consume(64, &mut sink).unwrap(),
    VoidClaimAllocationDispositionV1::Uncertain { ordinal: 0, offset: 8_192, length: 64, reason_code: 9 }
  ));
  assert_eq!(owner.finish().unwrap_err().code(), "void_claim_allocation_canceled");
}

#[test]
fn void_claim_allocation_limits_and_memory_pressure_refuse_before_reusable_authority_exists() {
  for maximum_allocations in [0, 4_097] {
    let (_directory, memory, cancellation, permit) =
      prepare_void_claim_allocation_permit(&format!("void-claim-limit-{maximum_allocations}"), HashAlgorithm::Blake3_256);
    assert_eq!(
      VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations }, &memory, cancellation,)
        .unwrap_err()
        .code(),
      "void_claim_allocation_limits"
    );
  }

  let (_directory, permit_memory, cancellation, permit) =
    prepare_void_claim_allocation_permit("void-claim-memory", HashAlgorithm::Blake3_256);
  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
  assert_eq!(
    VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 4_096 }, &constrained_memory, cancellation,)
      .unwrap_err()
      .code(),
    "void_claim_allocation_memory"
  );
  assert_eq!(permit_memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);
  assert_eq!(constrained_memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);

  let (_directory, memory, cancellation, mut malformed_permit) =
    prepare_void_claim_allocation_permit("void-claim-malformed-permit", HashAlgorithm::Blake3_256);
  malformed_permit.claimed_extents[0].origin_quarantine_manifest_hash.fill(0);
  assert_eq!(
    VoidClaimAllocationOwnerV1::new(malformed_permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 1 }, &memory, cancellation,)
      .unwrap_err()
      .code(),
    "void_claim_allocation_permit"
  );

  let (_directory, memory, cancellation, mut oversized_permit) =
    prepare_void_claim_allocation_permit("void-claim-oversized-permit", HashAlgorithm::Blake3_256);
  oversized_permit.claimed_extents = vec![oversized_permit.claimed_extents[0].clone(); 4_097].into_boxed_slice();
  assert_eq!(
    VoidClaimAllocationOwnerV1::new(oversized_permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 1 }, &memory, cancellation,)
      .unwrap_err()
      .code(),
    "void_claim_allocation_permit"
  );

  let (_directory, memory, cancellation, permit) =
    prepare_void_claim_allocation_permit("void-claim-pre-canceled", HashAlgorithm::Blake3_256);
  cancellation.cancel();
  assert_eq!(
    VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 1 }, &memory, cancellation,)
      .unwrap_err()
      .code(),
    "void_claim_allocation_canceled"
  );
}

#[test]
fn void_claim_allocator_partitions_durable_returned_and_uncertain_bytes_without_live_v3_state() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("void-claim-allocation", None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let source = prepare_void_catalog_publication(&publisher, &memory, 0);
  publish_prepared_void_catalog_support(&publisher, &source);
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut source_authority = test_void_catalog_publication_authority();
  let _source_receipt =
    publisher.publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut source_authority, &mut retirement_owner).unwrap();

  let prepared = prepare_void_claim_admission(&source);
  publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &source.database_id,
      artifact: &prepared.claim_directory,
      publication_timestamp_ms: 1_700_000_080_015,
    })
    .unwrap();
  let mut claim_authority = test_void_claim_admission_authority(&source.manifest.key, 1);
  let permit =
    publisher.admit_void_claim(prepared.request(&cancellation, &memory, 4, 2), &mut claim_authority, &mut retirement_owner).unwrap();
  assert_ne!(permit.claimed_extents()[0].reclaim_commit_sequence, 0);
  assert_eq!(permit.claimed_extents()[0].void_generation, 1);
  assert_eq!(permit.claimed_extents()[0].origin_quarantine_manifest_hash.len(), 32);
  assert!(permit.claimed_extents()[0].origin_quarantine_manifest_hash.iter().any(|byte| *byte != 0));
  assert_eq!(permit.claimed_extents()[0].reclaimed_incarnation_digest.len(), 32);
  assert!(permit.claimed_extents()[0].reclaimed_incarnation_digest.iter().any(|byte| *byte != 0));

  let reserved_before_owner = memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes;
  let mut owner =
    VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 4 }, &memory, cancellation.clone()).unwrap();
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes > reserved_before_owner);
  let mut sink = MixedVoidClaimAllocationSinkV1 { calls: 0 };
  assert!(matches!(
    owner.consume(128, &mut sink).unwrap(),
    VoidClaimAllocationDispositionV1::Durable { wal_offset: 8_192, entity_length: 128, write_sequence: 91, .. }
  ));
  assert!(matches!(
    owner.consume(64, &mut sink).unwrap(),
    VoidClaimAllocationDispositionV1::DefinitelyUnused { offset: 8_320, length: 64, reason_code: 7, .. }
  ));
  assert!(matches!(
    owner.consume(32, &mut sink).unwrap(),
    VoidClaimAllocationDispositionV1::Uncertain { offset: 8_384, length: 32, reason_code: 9, .. }
  ));
  let consumption = owner.finish().unwrap();
  assert_eq!(consumption.outcome(), VoidClaimConsumptionOutcomeV1::Settled);
  assert_eq!(consumption.used_bytes(), 128);
  assert_eq!(consumption.returned_bytes(), 352);
  assert_eq!(consumption.uncertain_bytes(), 32);
  assert_eq!(consumption.returned_extents().len(), 2);
  assert_eq!(consumption.uncertain_extents().len(), 1);
  assert_eq!(consumption.durable_uses().len(), 1);
  assert_eq!(consumption.used_bytes() + consumption.returned_bytes() + consumption.uncertain_bytes(), consumption.claimed_bytes());
  assert_eq!(consumption.evidence_digest().len(), 32);

  let result_catalog_id = [0xe1; 16];
  let returned_records: Vec<_> = consumption.returned_extents().iter().map(|extent| extent.as_record()).collect();
  let result_page = encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    catalog_id: &result_catalog_id,
    generation: 3,
    page_id: 2,
    extents: &returned_records,
  })
  .unwrap();
  let lower_fence = returned_records.first().unwrap().offset.to_le_bytes();
  let upper_fence = returned_records.last().unwrap().offset.to_le_bytes();
  let result_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: GcDirectoryRoleV1::FreeExtents,
    database_id: &source.database_id,
    catalog_id: &result_catalog_id,
    generation: 3,
    level: 0,
    entries: &[GcStateDirectoryEntryWriteV1 {
      lower_fence: &lower_fence,
      upper_fence: &upper_fence,
      child_hash: &result_page.key,
      child_generation: 3,
      live_count: 2,
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: consumption.returned_bytes(),
      minimum_page_id: 2,
      maximum_page_id: 2,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  let result_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    generation: 3,
    published_at_ms: 1_700_000_080_016,
    free_root: Some(&result_directory.key),
    claim_root: None,
    next_page_id: 3,
    free_count: 2,
    free_bytes: consumption.returned_bytes(),
    claim_count: 0,
    claimed_bytes: 0,
    previous_control_sequence: consumption.source_control_sequence(),
  })
  .unwrap();
  let source_artifact = decode_sweep_void_artifact(&prepared.result_manifest.value, HashAlgorithm::Blake3_256).unwrap();
  let result_artifact = decode_sweep_void_artifact(&result_manifest.value, HashAlgorithm::Blake3_256).unwrap();
  let claim_artifact = decode_sweep_void_artifact(&prepared.claim.value, HashAlgorithm::Blake3_256).unwrap();
  let mut transition = VoidClaimSettlementTransitionValidatorV1::new(
    &source_artifact,
    &result_artifact,
    &claim_artifact,
    &consumption,
    CancellationToken::new(),
    VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
    &memory,
  )
  .unwrap();
  transition.observe_source_encoded(&prepared.claim.value).unwrap();
  transition.observe_source_encoded(&prepared.claim_directory.value).unwrap();
  transition.finish_source().unwrap();
  transition.observe_result_encoded(&result_page.value).unwrap();
  transition.observe_result_encoded(&result_directory.value).unwrap();
  let summary = transition.finish().unwrap();
  assert_eq!(summary.used_count, 1);
  assert_eq!(summary.unused_count, 2);
  assert_eq!(summary.returned_bytes, 352);
  assert_eq!(summary.uncertain_bytes, 32);
  assert_eq!(summary.result_closure.outstanding_claim_count, 0);

  for artifact in [&result_page, &result_directory] {
    publisher
      .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
        database_id: &source.database_id,
        artifact,
        publication_timestamp_ms: 1_700_000_080_017,
      })
      .unwrap();
  }
  let result_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogActiveControl,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    slot: 0,
    sequence: 3,
    generation: 3,
    target_manifest_hash: &result_manifest.key,
  })
  .unwrap();
  let settlement = encode_void_claim_settlement_v1(&VoidClaimSettlementWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    claim_id: &consumption.claim_id(),
    generation: 3,
    outcome: VoidClaimSettlementOutcomeV1::Settled,
    settled_at_ms: 1_700_000_080_017,
    source_manifest_hash: consumption.source_manifest_key(),
    result_manifest_hash: &result_manifest.key,
    used_count: u32::try_from(consumption.durable_uses().len()).unwrap(),
    unused_count: u32::try_from(consumption.returned_extents().len()).unwrap(),
    used_bytes: consumption.used_bytes(),
    returned_bytes: consumption.returned_bytes(),
    evidence_digest: consumption.evidence_digest(),
  })
  .unwrap();
  let generic_error = publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &source.database_id,
      artifact: &settlement,
      publication_timestamp_ms: 1_700_000_080_017,
    })
    .unwrap_err();
  assert_eq!(generic_error.code(), "void_support_kind");
  assert!(publisher.locator(&settlement.key).unwrap().is_none());
  let settlement_cancellation = CancellationToken::new();
  let publication_request = VoidClaimSettlementPublicationRequestV1 {
    result_manifest: &result_manifest,
    result_control: &result_control,
    settlement: &settlement,
    publication_timestamp_ms: 1_700_000_080_017,
    monotonic_now_ms: 3,
    cancellation: &settlement_cancellation,
    memory: &memory,
    transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
  };
  let mut settlement_authority =
    test_void_claim_settlement_authority(consumption.source_manifest_key(), consumption.source_control_sequence());
  let receipt = publisher.settle_void_claim(&consumption, publication_request, &mut settlement_authority, &mut retirement_owner).unwrap();
  assert_eq!(settlement_authority.recheck_calls, 1);
  assert_eq!(receipt.result_manifest_key, result_manifest.key);
  assert_eq!(receipt.settlement_key, settlement.key);
  assert_ne!(receipt.settlement_write_sequence, 0);
  assert_eq!(receipt.outcome, VoidClaimConsumptionOutcomeV1::Settled);
  assert!(!receipt.idempotent);
  assert_eq!(selected_void_catalog_manifest_key(&publisher), Some(result_manifest.key.clone()));
  assert!(publisher.locator(&settlement.key).unwrap().is_some());

  let mut conflicting_receipt_authority =
    test_void_claim_settlement_authority(consumption.source_manifest_key(), consumption.source_control_sequence());
  conflicting_receipt_authority.snapshot.existing_receipt =
    Some(crate::engine::v4::gc_void_settlement::ExistingVoidClaimSettlementReceiptV1 {
      receipt_hash: settlement.key.clone(),
      receipt_write_sequence: receipt.settlement_write_sequence + 1,
    });
  let conflict =
    publisher.settle_void_claim(&consumption, publication_request, &mut conflicting_receipt_authority, &mut retirement_owner).unwrap_err();
  assert_eq!(conflict.code(), "void_claim_settlement_existing_conflict");
  assert!(conflict.committed_receipt().is_none());

  settlement_authority.snapshot.existing_receipt = Some(crate::engine::v4::gc_void_settlement::ExistingVoidClaimSettlementReceiptV1 {
    receipt_hash: settlement.key.clone(),
    receipt_write_sequence: receipt.settlement_write_sequence,
  });
  drop(retirement_owner);
  drop(publisher);
  let (_restart_coordinator, mut reopened) = reopen(&path);
  let mut retry_retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &settlement_cancellation,
    &memory,
  )
  .unwrap();
  let retry =
    reopened.settle_void_claim(&consumption, publication_request, &mut settlement_authority, &mut retry_retirement_owner).unwrap();
  assert_eq!(settlement_authority.recheck_calls, 2);
  assert_eq!(retry.settlement_write_sequence, receipt.settlement_write_sequence);
  assert!(retry.idempotent);
  assert_eq!(selected_void_catalog_manifest_key(&reopened), Some(result_manifest.key.clone()));
  drop(consumption);
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes < reserved_before_owner);
}

struct PreparedVoidClaimSettlementHarnessV1 {
  _directory: tempfile::TempDir,
  path: PathBuf,
  coordinator: Arc<DurabilityCoordinator>,
  publisher: V4FirstAuthorityPublisher,
  memory: MemoryCoordinator,
  cancellation: CancellationToken,
  database_id: [u8; 16],
  consumption: VoidClaimConsumptionPermitV1,
  result_manifest: EncodedImmutableGcArtifactV1,
  result_control: EncodedGcActiveControlV1,
  settlement: EncodedImmutableGcArtifactV1,
}

fn prepare_void_claim_settlement_harness(name: &str) -> PreparedVoidClaimSettlementHarnessV1 {
  let (directory, path, coordinator, mut publisher) = create_environment(name, None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let source = prepare_void_catalog_publication(&publisher, &memory, 0);
  publish_prepared_void_catalog_support(&publisher, &source);
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut source_authority = test_void_catalog_publication_authority();
  let _source_receipt =
    publisher.publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut source_authority, &mut retirement_owner).unwrap();
  let prepared_claim = prepare_void_claim_admission(&source);
  publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &source.database_id,
      artifact: &prepared_claim.claim_directory,
      publication_timestamp_ms: 1_700_000_080_015,
    })
    .unwrap();
  let mut claim_authority = test_void_claim_admission_authority(&source.manifest.key, 1);
  let permit =
    publisher.admit_void_claim(prepared_claim.request(&cancellation, &memory, 4, 2), &mut claim_authority, &mut retirement_owner).unwrap();
  let mut allocation_owner =
    VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 4 }, &memory, cancellation.clone()).unwrap();
  let mut sink = MixedVoidClaimAllocationSinkV1 { calls: 0 };
  let _durable = allocation_owner.consume(128, &mut sink).unwrap();
  let _unused = allocation_owner.consume(64, &mut sink).unwrap();
  let _uncertain = allocation_owner.consume(32, &mut sink).unwrap();
  let consumption = allocation_owner.finish().unwrap();

  let result_catalog_id = [0xe2; 16];
  let returned_records: Vec<_> = consumption.returned_extents().iter().map(|extent| extent.as_record()).collect();
  let result_page = encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    catalog_id: &result_catalog_id,
    generation: 3,
    page_id: 2,
    extents: &returned_records,
  })
  .unwrap();
  let lower_fence = returned_records.first().unwrap().offset.to_le_bytes();
  let upper_fence = returned_records.last().unwrap().offset.to_le_bytes();
  let result_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: GcDirectoryRoleV1::FreeExtents,
    database_id: &source.database_id,
    catalog_id: &result_catalog_id,
    generation: 3,
    level: 0,
    entries: &[GcStateDirectoryEntryWriteV1 {
      lower_fence: &lower_fence,
      upper_fence: &upper_fence,
      child_hash: &result_page.key,
      child_generation: 3,
      live_count: u64::try_from(returned_records.len()).unwrap(),
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: consumption.returned_bytes(),
      minimum_page_id: 2,
      maximum_page_id: 2,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  let result_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    generation: 3,
    published_at_ms: 1_700_000_080_017,
    free_root: Some(&result_directory.key),
    claim_root: None,
    next_page_id: 3,
    free_count: u64::try_from(returned_records.len()).unwrap(),
    free_bytes: consumption.returned_bytes(),
    claim_count: 0,
    claimed_bytes: 0,
    previous_control_sequence: consumption.source_control_sequence(),
  })
  .unwrap();
  for artifact in [&result_page, &result_directory] {
    publisher
      .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
        database_id: &source.database_id,
        artifact,
        publication_timestamp_ms: 1_700_000_080_017,
      })
      .unwrap();
  }
  let result_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogActiveControl,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    slot: 0,
    sequence: 3,
    generation: 3,
    target_manifest_hash: &result_manifest.key,
  })
  .unwrap();
  let settlement = encode_void_claim_settlement_v1(&VoidClaimSettlementWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    claim_id: &consumption.claim_id(),
    generation: 3,
    outcome: VoidClaimSettlementOutcomeV1::Settled,
    settled_at_ms: 1_700_000_080_017,
    source_manifest_hash: consumption.source_manifest_key(),
    result_manifest_hash: &result_manifest.key,
    used_count: u32::try_from(consumption.durable_uses().len()).unwrap(),
    unused_count: u32::try_from(consumption.returned_extents().len()).unwrap(),
    used_bytes: consumption.used_bytes(),
    returned_bytes: consumption.returned_bytes(),
    evidence_digest: consumption.evidence_digest(),
  })
  .unwrap();
  drop(retirement_owner);
  PreparedVoidClaimSettlementHarnessV1 {
    _directory: directory,
    path,
    coordinator,
    publisher,
    memory,
    cancellation,
    database_id: source.database_id,
    consumption,
    result_manifest,
    result_control,
    settlement,
  }
}

fn stored_gc_artifact_write_sequence(publisher: &V4FirstAuthorityPublisher, key: &[u8], kind: GcArtifactKindV1) -> u64 {
  let observation = publisher.observe().unwrap();
  let header = &observation.selected.header;
  let kv = publisher.lock_kv().unwrap();
  let bytes = read_entity_bounded(
    &publisher.file,
    &kv,
    key,
    crate::engine::v4::entity::checked_whole_entity_encoded_length(
      header.hash_algorithm,
      key.len(),
      kind.immutable_maximum_encoded_length().unwrap(),
    )
    .unwrap(),
    header.write_sequence_high_water,
  )
  .unwrap()
  .unwrap();
  decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water).unwrap().write_sequence
}

#[test]
fn selected_void_runtime_reconstructs_settled_returned_space_after_restart() {
  let mut harness = prepare_void_claim_settlement_harness("void-runtime-settled");
  let source_manifest_key = harness.consumption.source_manifest_key().to_vec();
  let mut settlement_authority = test_void_claim_settlement_authority(&source_manifest_key, harness.consumption.source_control_sequence());
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    harness.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &harness.cancellation,
    &harness.memory,
  )
  .unwrap();
  let _settlement_receipt = harness
    .publisher
    .settle_void_claim(
      &harness.consumption,
      VoidClaimSettlementPublicationRequestV1 {
        result_manifest: &harness.result_manifest,
        result_control: &harness.result_control,
        settlement: &harness.settlement,
        publication_timestamp_ms: 1_700_000_080_017,
        monotonic_now_ms: 3,
        cancellation: &harness.cancellation,
        memory: &harness.memory,
        transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
      },
      &mut settlement_authority,
      &mut retirement_owner,
    )
    .unwrap();
  let mut runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let state = harness
    .publisher
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 {
        cancellation: &harness.cancellation,
        memory: &harness.memory,
        limits: void_runtime_limits(),
      },
      &mut runtime_authority,
    )
    .unwrap()
    .unwrap();
  assert_eq!(state.selected_manifest_key(), harness.result_manifest.key);
  assert_eq!(state.free_count(), u64::try_from(harness.consumption.returned_extents().len()).unwrap());
  assert_eq!(state.free_bytes(), harness.consumption.returned_bytes());
  assert_eq!(state.outstanding_claim_count(), 0);
  assert_eq!(state.candidate_extents().len(), harness.consumption.returned_extents().len());
  assert!(!state.candidate_window_truncated());
  assert_eq!(runtime_authority.recheck_calls, harness.consumption.returned_extents().len());
  for (candidate, returned) in state.candidate_extents().iter().zip(harness.consumption.returned_extents()) {
    assert_eq!(candidate.offset, returned.offset);
    assert_eq!(candidate.length, returned.length);
    assert_eq!(candidate.origin_sweep_proposal_hash, returned.origin_sweep_proposal_hash);
    assert_eq!(candidate.origin_quarantine_manifest_hash, returned.origin_quarantine_manifest_hash);
    assert_eq!(candidate.reclaimed_incarnation_digest, returned.reclaimed_incarnation_digest);
    assert_eq!(candidate.reclaim_commit_sequence, returned.reclaim_commit_sequence);
    assert_eq!(candidate.void_generation, returned.void_generation);
  }
  drop(state);
  drop(retirement_owner);
  let path = harness.path.clone();
  let result_manifest_key = harness.result_manifest.key.clone();
  let expected_free_count = u64::try_from(harness.consumption.returned_extents().len()).unwrap();
  drop(harness.publisher);

  let (_restart_coordinator, reopened) = reopen(&path);
  let restart_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let mut restart_authority = TestVoidReclaimReceiptAuthorityV1::default();
  let restarted = reopened
    .reconstruct_void_reusable_state(
      VoidReusableStateReconstructionRequestV1 {
        cancellation: &harness.cancellation,
        memory: &restart_memory,
        limits: void_runtime_limits(),
      },
      &mut restart_authority,
    )
    .unwrap()
    .unwrap();
  assert_eq!(restarted.selected_manifest_key(), result_manifest_key);
  assert_eq!(restarted.free_count(), expected_free_count);
  assert_eq!(restarted.outstanding_claim_count(), 0);
  assert!(!restarted.candidate_window_truncated());
  assert_eq!(restart_authority.recheck_calls, usize::try_from(expected_free_count).unwrap());
  for (candidate, returned) in restarted.candidate_extents().iter().zip(harness.consumption.returned_extents()) {
    assert_eq!(candidate.offset, returned.offset);
    assert_eq!(candidate.length, returned.length);
    assert_eq!(candidate.origin_sweep_proposal_hash, returned.origin_sweep_proposal_hash);
    assert_eq!(candidate.origin_quarantine_manifest_hash, returned.origin_quarantine_manifest_hash);
    assert_eq!(candidate.reclaimed_incarnation_digest, returned.reclaimed_incarnation_digest);
    assert_eq!(candidate.reclaim_commit_sequence, returned.reclaim_commit_sequence);
    assert_eq!(candidate.void_generation, returned.void_generation);
  }
}

#[test]
fn void_claim_settlement_refuses_every_stale_or_incomplete_caller_authority_before_selection() {
  type AuthorityMutation = fn(&mut TestVoidClaimSettlementAuthorityV1);
  let cases: [(&str, AuthorityMutation, &str); 14] = [
    ("source-manifest", |authority| authority.snapshot.selected_source_manifest_hash.fill(0x99), "void_claim_settlement_source_authority"),
    ("source-sequence", |authority| authority.snapshot.selected_source_control_sequence += 1, "void_claim_settlement_source_authority"),
    ("receipt-backed", |authority| authority.snapshot.source_catalog_receipt_backed = false, "void_claim_settlement_authority_incomplete"),
    ("closure", |authority| authority.snapshot.source_catalog_closure_current = false, "void_claim_settlement_authority_incomplete"),
    ("claim", |authority| authority.snapshot.claim_outstanding_exact = false, "void_claim_settlement_authority_incomplete"),
    ("durable-locators", |authority| authority.snapshot.durable_used_locators_exact = false, "void_claim_settlement_authority_incomplete"),
    (
      "uncertain-quarantine",
      |authority| authority.snapshot.uncertain_ranges_quarantined = false,
      "void_claim_settlement_authority_incomplete",
    ),
    ("lineage", |authority| authority.snapshot.replacement_lineage_complete = false, "void_claim_settlement_authority_incomplete"),
    (
      "allocator-exclusion",
      |authority| authority.snapshot.allocator_settlement_excluded = false,
      "void_claim_settlement_authority_incomplete",
    ),
    ("sole-settlement", |authority| authority.snapshot.no_other_settlement_active = false, "void_claim_settlement_authority_incomplete"),
    ("memory", |authority| authority.snapshot.memory_coordinator_current = false, "void_claim_settlement_authority_incomplete"),
    ("receipt-search", |authority| authority.snapshot.receipt_search_complete = false, "void_claim_settlement_authority_incomplete"),
    ("receipt-conflict", |authority| authority.snapshot.conflicting_receipt_count = 1, "void_claim_settlement_authority_incomplete"),
    ("repair", |authority| authority.snapshot.repair_latch_clear = false, "void_claim_settlement_authority_incomplete"),
  ];
  for (name, mutate_authority, expected_code) in cases {
    let mut harness = prepare_void_claim_settlement_harness(&format!("void-settlement-authority-{name}"));
    let source_manifest_key = harness.consumption.source_manifest_key().to_vec();
    let mut authority = test_void_claim_settlement_authority(&source_manifest_key, harness.consumption.source_control_sequence());
    mutate_authority(&mut authority);
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      harness.database_id,
      1,
      1,
      RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
      &harness.cancellation,
      &harness.memory,
    )
    .unwrap();
    let request = VoidClaimSettlementPublicationRequestV1 {
      result_manifest: &harness.result_manifest,
      result_control: &harness.result_control,
      settlement: &harness.settlement,
      publication_timestamp_ms: 1_700_000_080_017,
      monotonic_now_ms: 3,
      cancellation: &harness.cancellation,
      memory: &harness.memory,
      transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
    };

    let error = harness.publisher.settle_void_claim(&harness.consumption, request, &mut authority, &mut retirement_owner).unwrap_err();

    assert_eq!(error.code(), expected_code, "authority case {name}");
    assert_eq!(authority.recheck_calls, 1, "authority case {name}");
    assert!(harness.publisher.locator(&harness.result_manifest.key).unwrap().is_none(), "authority case {name}");
    assert!(harness.publisher.locator(&harness.settlement.key).unwrap().is_none(), "authority case {name}");
    assert_eq!(selected_void_catalog_manifest_key(&harness.publisher), Some(source_manifest_key), "authority case {name}");
  }
}

#[test]
fn every_void_claim_settlement_publication_failure_restarts_as_source_or_exact_result() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for target_publication in 1..=5 {
    for failure in failures {
      let mut harness = prepare_void_claim_settlement_harness(&format!("void-settlement-{target_publication}-{failure:?}"));
      harness.publisher.header_publisher = DatabaseHeaderPublisherV4::with_io(
        harness.coordinator.clone(),
        Arc::new(NthHeaderPublicationFaultIo::new(failure, target_publication)),
      );
      let source_manifest_key = harness.consumption.source_manifest_key().to_vec();
      let result_manifest_key = harness.result_manifest.key.clone();
      let settlement_key = harness.settlement.key.clone();
      let mut authority = test_void_claim_settlement_authority(&source_manifest_key, harness.consumption.source_control_sequence());
      let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
        HashAlgorithm::Blake3_256,
        harness.database_id,
        1,
        1,
        RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
        &harness.cancellation,
        &harness.memory,
      )
      .unwrap();
      let request = VoidClaimSettlementPublicationRequestV1 {
        result_manifest: &harness.result_manifest,
        result_control: &harness.result_control,
        settlement: &harness.settlement,
        publication_timestamp_ms: 1_700_000_080_017,
        monotonic_now_ms: 3,
        cancellation: &harness.cancellation,
        memory: &harness.memory,
        transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
      };

      let error = match harness.publisher.settle_void_claim(&harness.consumption, request, &mut authority, &mut retirement_owner) {
        Ok(receipt) => panic!("target {target_publication}, failure {failure:?} did not inject: {receipt:?}"),
        Err(error) => error,
      };

      assert!(harness.coordinator.hard_failure().unwrap().is_some(), "target {target_publication}, failure {failure:?}");
      drop(retirement_owner);
      drop(harness.publisher);

      let (_restart_coordinator, mut reopened) = reopen(&harness.path);
      let selected = selected_void_catalog_manifest_key(&reopened).unwrap();
      assert!(
        selected == source_manifest_key || selected == result_manifest_key,
        "target {target_publication}, failure {failure:?} selected neither complete authority"
      );
      let selector_committed = selected == result_manifest_key;
      if target_publication <= 2 {
        assert!(!selector_committed, "target {target_publication}, failure {failure:?}");
      }
      if target_publication > 4 {
        assert!(selector_committed, "target {target_publication}, failure {failure:?}");
      }
      assert_eq!(
        error.committed_receipt().is_some(),
        selector_committed,
        "target {target_publication}, failure {failure:?}, error {error:?}"
      );
      let mut restart_runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
      let restart_state = reopened
        .reconstruct_void_reusable_state(
          VoidReusableStateReconstructionRequestV1 {
            cancellation: &harness.cancellation,
            memory: &harness.memory,
            limits: void_runtime_limits(),
          },
          &mut restart_runtime_authority,
        )
        .unwrap()
        .unwrap();
      if selector_committed {
        assert_eq!(restart_state.free_count(), u64::try_from(harness.consumption.returned_extents().len()).unwrap());
        assert_eq!(restart_state.free_bytes(), harness.consumption.returned_bytes());
        assert_eq!(restart_state.outstanding_claim_count(), 0);
        assert_eq!(restart_state.candidate_extents().len(), harness.consumption.returned_extents().len());
        assert_eq!(restart_runtime_authority.recheck_calls, harness.consumption.returned_extents().len());
      } else {
        assert_eq!(restart_state.free_count(), 0);
        assert_eq!(restart_state.outstanding_claim_count(), 1);
        assert!(restart_state.candidate_extents().is_empty());
        assert_eq!(restart_runtime_authority.recheck_calls, 0);
      }
      drop(restart_state);
      let existing_settlement_write_sequence = if reopened.locator(&settlement_key).unwrap().is_some() {
        Some(stored_gc_artifact_write_sequence(&reopened, &settlement_key, GcArtifactKindV1::VoidClaimSettlementReceipt))
      } else {
        None
      };
      let mut retry_authority = test_void_claim_settlement_authority(&source_manifest_key, harness.consumption.source_control_sequence());
      if let Some(receipt_write_sequence) = existing_settlement_write_sequence {
        retry_authority.snapshot.existing_receipt = Some(crate::engine::v4::gc_void_settlement::ExistingVoidClaimSettlementReceiptV1 {
          receipt_hash: settlement_key.clone(),
          receipt_write_sequence,
        });
      }
      let mut retry_owner = RetirementJournalOwnerV1::new_chain(
        HashAlgorithm::Blake3_256,
        harness.database_id,
        1,
        1,
        RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
        &harness.cancellation,
        &harness.memory,
      )
      .unwrap();
      let retry_request = VoidClaimSettlementPublicationRequestV1 {
        result_manifest: &harness.result_manifest,
        result_control: &harness.result_control,
        settlement: &harness.settlement,
        publication_timestamp_ms: 1_700_000_080_017,
        monotonic_now_ms: 4,
        cancellation: &harness.cancellation,
        memory: &harness.memory,
        transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
      };
      let retry = reopened.settle_void_claim(&harness.consumption, retry_request, &mut retry_authority, &mut retry_owner).unwrap();
      assert_eq!(retry.idempotent, existing_settlement_write_sequence.is_some(), "target {target_publication}, failure {failure:?}");
      assert_eq!(
        selected_void_catalog_manifest_key(&reopened),
        Some(result_manifest_key),
        "target {target_publication}, failure {failure:?}"
      );
      assert!(reopened.locator(&settlement_key).unwrap().is_some(), "target {target_publication}, failure {failure:?}");
      let mut retry_runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
      let retry_state = reopened
        .reconstruct_void_reusable_state(
          VoidReusableStateReconstructionRequestV1 {
            cancellation: &harness.cancellation,
            memory: &harness.memory,
            limits: void_runtime_limits(),
          },
          &mut retry_runtime_authority,
        )
        .unwrap()
        .unwrap();
      assert_eq!(retry_state.free_count(), u64::try_from(harness.consumption.returned_extents().len()).unwrap());
      assert_eq!(retry_state.free_bytes(), harness.consumption.returned_bytes());
      assert_eq!(retry_state.outstanding_claim_count(), 0);
      assert_eq!(retry_state.candidate_extents().len(), harness.consumption.returned_extents().len());
      assert_eq!(retry_runtime_authority.recheck_calls, harness.consumption.returned_extents().len());
      for (candidate, returned) in retry_state.candidate_extents().iter().zip(harness.consumption.returned_extents()) {
        assert_eq!(candidate.offset, returned.offset);
        assert_eq!(candidate.length, returned.length);
        assert_eq!(candidate.origin_sweep_proposal_hash, returned.origin_sweep_proposal_hash);
        assert_eq!(candidate.origin_quarantine_manifest_hash, returned.origin_quarantine_manifest_hash);
        assert_eq!(candidate.reclaimed_incarnation_digest, returned.reclaimed_incarnation_digest);
        assert_eq!(candidate.reclaim_commit_sequence, returned.reclaim_commit_sequence);
        assert_eq!(candidate.void_generation, returned.void_generation);
      }
    }
  }
}

#[test]
fn void_claim_settlement_cancellation_corrupt_support_and_memory_pressure_keep_the_claim_selected() {
  for case in ["canceled", "canceled-after-recheck", "corrupt-support", "memory-pressure"] {
    let mut harness = prepare_void_claim_settlement_harness(&format!("void-settlement-{case}"));
    let source_manifest_key = harness.consumption.source_manifest_key().to_vec();
    let request_cancellation = CancellationToken::new();
    if case == "canceled" {
      request_cancellation.cancel();
    }
    if case == "corrupt-support" {
      let result = decode_sweep_void_artifact(&harness.result_manifest.value, HashAlgorithm::Blake3_256).unwrap();
      let SweepVoidArtifactV1::VoidCatalog(result_manifest) = result else {
        panic!("prepared settlement result did not decode as a Void catalog");
      };
      corrupt_last_entity_byte(&harness.publisher, result_manifest.free_root);
    }
    let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
    let request_memory = if case == "memory-pressure" { &constrained_memory } else { &harness.memory };
    let owner_cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      harness.database_id,
      1,
      1,
      RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
      &owner_cancellation,
      &harness.memory,
    )
    .unwrap();
    let request = VoidClaimSettlementPublicationRequestV1 {
      result_manifest: &harness.result_manifest,
      result_control: &harness.result_control,
      settlement: &harness.settlement,
      publication_timestamp_ms: 1_700_000_080_017,
      monotonic_now_ms: 3,
      cancellation: &request_cancellation,
      memory: request_memory,
      transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
    };
    let mut authority = test_void_claim_settlement_authority(&source_manifest_key, harness.consumption.source_control_sequence());
    if case == "canceled-after-recheck" {
      authority.cancel_during_recheck = Some(request_cancellation.clone());
    }

    let error = harness.publisher.settle_void_claim(&harness.consumption, request, &mut authority, &mut retirement_owner).unwrap_err();

    let expected_code = match case {
      "canceled" => "void_claim_settlement_canceled",
      "canceled-after-recheck" => "void_claim_settlement_canceled",
      "corrupt-support" => "void_claim_settlement_result_support",
      "memory-pressure" => "void_claim_settlement_source_support",
      _ => unreachable!(),
    };
    assert_eq!(error.code(), expected_code, "case {case}: {error:?}");
    assert!(error.committed_receipt().is_none(), "case {case}");
    assert_eq!(authority.recheck_calls, usize::from(case == "canceled-after-recheck"), "case {case}");
    assert!(harness.publisher.locator(&harness.result_manifest.key).unwrap().is_none(), "case {case}");
    assert!(harness.publisher.locator(&harness.settlement.key).unwrap().is_none(), "case {case}");
    assert_eq!(selected_void_catalog_manifest_key(&harness.publisher), Some(source_manifest_key), "case {case}");
    assert_eq!(constrained_memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0, "case {case}");
  }
}

#[test]
fn void_claim_abandonment_selects_no_reusable_bytes_and_restarts_idempotently() {
  let (directory, path, _coordinator, mut publisher) = create_environment("void-settlement-abandonment", None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let source = prepare_void_catalog_publication(&publisher, &memory, 0);
  publish_prepared_void_catalog_support(&publisher, &source);
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut source_authority = test_void_catalog_publication_authority();
  let _ =
    publisher.publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut source_authority, &mut retirement_owner).unwrap();
  let prepared = prepare_void_claim_admission(&source);
  publisher
    .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
      database_id: &source.database_id,
      artifact: &prepared.claim_directory,
      publication_timestamp_ms: 1_700_000_080_015,
    })
    .unwrap();
  let mut claim_authority = test_void_claim_admission_authority(&source.manifest.key, 1);
  let permit =
    publisher.admit_void_claim(prepared.request(&cancellation, &memory, 4, 2), &mut claim_authority, &mut retirement_owner).unwrap();
  let owner =
    VoidClaimAllocationOwnerV1::new(permit, VoidClaimAllocationLimitsV1 { maximum_allocations: 4 }, &memory, cancellation.clone()).unwrap();
  let consumption = owner.finish().unwrap();
  assert_eq!(consumption.outcome(), VoidClaimConsumptionOutcomeV1::AbandonedToQuarantine);
  assert_eq!(consumption.used_bytes(), 0);
  assert_eq!(consumption.returned_bytes(), 0);
  assert_eq!(consumption.uncertain_bytes(), consumption.claimed_bytes());

  let source_artifact = decode_sweep_void_artifact(&prepared.result_manifest.value, HashAlgorithm::Blake3_256).unwrap();
  let SweepVoidArtifactV1::VoidCatalog(source_manifest) = source_artifact else {
    panic!("selected claim catalog did not decode as a Void catalog");
  };
  let result_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    generation: 3,
    published_at_ms: 1_700_000_080_017,
    free_root: None,
    claim_root: None,
    next_page_id: source_manifest.next_page_id,
    free_count: 0,
    free_bytes: 0,
    claim_count: 0,
    claimed_bytes: 0,
    previous_control_sequence: consumption.source_control_sequence(),
  })
  .unwrap();
  let result_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogActiveControl,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    slot: 1 - consumption.source_control_slot(),
    sequence: consumption.source_control_sequence() + 1,
    generation: 3,
    target_manifest_hash: &result_manifest.key,
  })
  .unwrap();
  let settlement = encode_void_claim_settlement_v1(&VoidClaimSettlementWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &source.database_id,
    claim_id: &consumption.claim_id(),
    generation: 3,
    outcome: VoidClaimSettlementOutcomeV1::AbandonedToQuarantine,
    settled_at_ms: 1_700_000_080_017,
    source_manifest_hash: consumption.source_manifest_key(),
    result_manifest_hash: &result_manifest.key,
    used_count: 0,
    unused_count: 0,
    used_bytes: 0,
    returned_bytes: 0,
    evidence_digest: consumption.evidence_digest(),
  })
  .unwrap();
  let request = VoidClaimSettlementPublicationRequestV1 {
    result_manifest: &result_manifest,
    result_control: &result_control,
    settlement: &settlement,
    publication_timestamp_ms: 1_700_000_080_017,
    monotonic_now_ms: 3,
    cancellation: &cancellation,
    memory: &memory,
    transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
  };
  let mut authority = test_void_claim_settlement_authority(consumption.source_manifest_key(), consumption.source_control_sequence());
  let receipt = publisher.settle_void_claim(&consumption, request, &mut authority, &mut retirement_owner).unwrap();
  assert_eq!(receipt.outcome, VoidClaimConsumptionOutcomeV1::AbandonedToQuarantine);
  assert_eq!(receipt.result_manifest_key, result_manifest.key);
  assert_ne!(receipt.settlement_write_sequence, 0);
  assert_eq!(selected_void_catalog_manifest_key(&publisher), Some(result_manifest.key.clone()));

  authority.snapshot.existing_receipt = Some(crate::engine::v4::gc_void_settlement::ExistingVoidClaimSettlementReceiptV1 {
    receipt_hash: settlement.key.clone(),
    receipt_write_sequence: receipt.settlement_write_sequence,
  });
  drop(retirement_owner);
  drop(publisher);
  let (_restart_coordinator, mut reopened) = reopen(&path);
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    source.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retry = reopened.settle_void_claim(&consumption, request, &mut authority, &mut retry_owner).unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry.settlement_write_sequence, receipt.settlement_write_sequence);
  assert_eq!(selected_void_catalog_manifest_key(&reopened), Some(result_manifest.key));
  drop(directory);
}

#[test]
fn void_claim_settlement_rejects_closure_valid_substituted_returned_ranges_before_authority() {
  let mut harness = prepare_void_claim_settlement_harness("void-settlement-substituted-range");
  let source_manifest_key = harness.consumption.source_manifest_key().to_vec();
  let catalog_id = [0xe3; 16];
  let mut returned_extents = harness.consumption.returned_extents().to_vec();
  returned_extents[0].offset += 1;
  let returned_records: Vec<_> = returned_extents.iter().map(|extent| extent.as_record()).collect();
  let page = encode_void_extent_page_v1(&VoidExtentPageWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &harness.database_id,
    catalog_id: &catalog_id,
    generation: 3,
    page_id: 22,
    extents: &returned_records,
  })
  .unwrap();
  let lower_fence = returned_records.first().unwrap().offset.to_le_bytes();
  let upper_fence = returned_records.last().unwrap().offset.to_le_bytes();
  let directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    role: GcDirectoryRoleV1::FreeExtents,
    database_id: &harness.database_id,
    catalog_id: &catalog_id,
    generation: 3,
    level: 0,
    entries: &[GcStateDirectoryEntryWriteV1 {
      lower_fence: &lower_fence,
      upper_fence: &upper_fence,
      child_hash: &page.key,
      child_generation: 3,
      live_count: u64::try_from(returned_records.len()).unwrap(),
      tombstone_count: 0,
      page_count: 1,
      logical_bytes: harness.consumption.returned_bytes(),
      minimum_page_id: 22,
      maximum_page_id: 22,
      physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }],
  })
  .unwrap();
  for artifact in [&page, &directory] {
    harness
      .publisher
      .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
        database_id: &harness.database_id,
        artifact,
        publication_timestamp_ms: 1_700_000_080_017,
      })
      .unwrap();
  }
  let result_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &harness.database_id,
    generation: 3,
    published_at_ms: 1_700_000_080_017,
    free_root: Some(&directory.key),
    claim_root: None,
    next_page_id: 23,
    free_count: u64::try_from(returned_records.len()).unwrap(),
    free_bytes: harness.consumption.returned_bytes(),
    claim_count: 0,
    claimed_bytes: 0,
    previous_control_sequence: harness.consumption.source_control_sequence(),
  })
  .unwrap();
  let result_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogActiveControl,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &harness.database_id,
    slot: 1 - harness.consumption.source_control_slot(),
    sequence: harness.consumption.source_control_sequence() + 1,
    generation: 3,
    target_manifest_hash: &result_manifest.key,
  })
  .unwrap();
  let settlement = encode_void_claim_settlement_v1(&VoidClaimSettlementWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &harness.database_id,
    claim_id: &harness.consumption.claim_id(),
    generation: 3,
    outcome: VoidClaimSettlementOutcomeV1::Settled,
    settled_at_ms: 1_700_000_080_017,
    source_manifest_hash: harness.consumption.source_manifest_key(),
    result_manifest_hash: &result_manifest.key,
    used_count: u32::try_from(harness.consumption.durable_uses().len()).unwrap(),
    unused_count: u32::try_from(harness.consumption.returned_extents().len()).unwrap(),
    used_bytes: harness.consumption.used_bytes(),
    returned_bytes: harness.consumption.returned_bytes(),
    evidence_digest: harness.consumption.evidence_digest(),
  })
  .unwrap();
  let mut authority = test_void_claim_settlement_authority(&source_manifest_key, harness.consumption.source_control_sequence());
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    harness.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &harness.cancellation,
    &harness.memory,
  )
  .unwrap();
  let request = VoidClaimSettlementPublicationRequestV1 {
    result_manifest: &result_manifest,
    result_control: &result_control,
    settlement: &settlement,
    publication_timestamp_ms: 1_700_000_080_017,
    monotonic_now_ms: 3,
    cancellation: &harness.cancellation,
    memory: &harness.memory,
    transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
  };

  let error = harness.publisher.settle_void_claim(&harness.consumption, request, &mut authority, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "void_settlement_transition_result");
  assert_eq!(authority.recheck_calls, 0);
  assert!(harness.publisher.locator(&result_manifest.key).unwrap().is_none());
  assert!(harness.publisher.locator(&settlement.key).unwrap().is_none());
  assert_eq!(selected_void_catalog_manifest_key(&harness.publisher), Some(source_manifest_key));
}

#[test]
fn void_claim_settlement_rejects_a_stale_consumption_permit_after_durable_authority_advances() {
  let mut harness = prepare_void_claim_settlement_harness("void-settlement-stale-permit");
  let stale_source_manifest_key = harness.consumption.source_manifest_key().to_vec();
  let alternate_manifest = encode_void_catalog_manifest_v1(&VoidCatalogManifestWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &harness.database_id,
    generation: 3,
    published_at_ms: 1_700_000_080_017,
    free_root: None,
    claim_root: None,
    next_page_id: 30,
    free_count: 0,
    free_bytes: 0,
    claim_count: 0,
    claimed_bytes: 0,
    previous_control_sequence: harness.consumption.source_control_sequence(),
  })
  .unwrap();
  harness
    .publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::VoidCatalogManifest,
        database_id: &harness.database_id,
        artifact_key: &alternate_manifest.key,
        value: &alternate_manifest.value,
        minimum_timestamp_ms: 1_700_000_080_017,
        committed_postcondition_code: "void_settlement_stale_test_manifest",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let alternate_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::VoidCatalogActiveControl,
    hash_algorithm: HashAlgorithm::Blake3_256,
    database_id: &harness.database_id,
    slot: 1 - harness.consumption.source_control_slot(),
    sequence: harness.consumption.source_control_sequence() + 1,
    generation: 3,
    target_manifest_hash: &alternate_manifest.key,
  })
  .unwrap();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    harness.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(1, 1 << 20, 30_000),
    &harness.cancellation,
    &harness.memory,
  )
  .unwrap();
  let outcome = harness
    .publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::VoidCatalogActiveControl,
        encoded_control: &alternate_control,
        publication_timestamp_ms: 1_700_000_080_017,
        monotonic_now_ms: 3,
      },
      &mut retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  assert!(matches!(outcome, GcControlPublicationOutcomeV1::Complete(_)));
  retirement_owner.flush(&mut harness.publisher).unwrap();
  assert_eq!(selected_void_catalog_manifest_key(&harness.publisher), Some(alternate_manifest.key.clone()));

  let mut authority = test_void_claim_settlement_authority(&stale_source_manifest_key, harness.consumption.source_control_sequence());
  let request = VoidClaimSettlementPublicationRequestV1 {
    result_manifest: &harness.result_manifest,
    result_control: &harness.result_control,
    settlement: &harness.settlement,
    publication_timestamp_ms: 1_700_000_080_017,
    monotonic_now_ms: 4,
    cancellation: &harness.cancellation,
    memory: &harness.memory,
    transition_limits: VoidClaimSettlementTransitionLimitsV1 { maximum_support_artifacts_per_catalog: 4 },
  };
  let error = harness.publisher.settle_void_claim(&harness.consumption, request, &mut authority, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "void_claim_settlement_source_changed");
  assert_eq!(authority.recheck_calls, 0);
  assert!(harness.publisher.locator(&harness.result_manifest.key).unwrap().is_none());
  assert!(harness.publisher.locator(&harness.settlement.key).unwrap().is_none());
  assert_eq!(selected_void_catalog_manifest_key(&harness.publisher), Some(alternate_manifest.key));
}

#[test]
fn void_claim_refuses_every_stale_or_incomplete_caller_authority_before_selection() {
  type AuthorityMutation = fn(&mut TestVoidClaimAdmissionAuthorityV1);
  let cases: [(&str, AuthorityMutation, &str); 10] = [
    ("source-manifest", |authority| authority.snapshot.selected_source_manifest_hash.fill(0x99), "void_claim_admission_source_authority"),
    ("source-sequence", |authority| authority.snapshot.selected_source_control_sequence += 1, "void_claim_admission_source_authority"),
    ("receipt", |authority| authority.snapshot.source_catalog_receipt_backed = false, "void_claim_admission_authority_incomplete"),
    ("closure", |authority| authority.snapshot.source_catalog_closure_current = false, "void_claim_admission_authority_incomplete"),
    ("allocator", |authority| authority.snapshot.allocator_admission_excluded = false, "void_claim_admission_authority_incomplete"),
    ("claim-owner", |authority| authority.snapshot.no_other_claim_admission_active = false, "void_claim_admission_authority_incomplete"),
    (
      "memory-authority",
      |authority| authority.snapshot.in_memory_void_authority_current = false,
      "void_claim_admission_authority_incomplete",
    ),
    ("receipt-conflict", |authority| authority.snapshot.conflicting_receipt_count = 1, "void_claim_admission_authority_incomplete"),
    ("repair-latch", |authority| authority.snapshot.repair_latch_clear = false, "void_claim_admission_authority_incomplete"),
    ("callback-error", |authority| authority.fail_recheck = true, "void_claim_admission_test_recheck"),
  ];

  for (name, mutate_authority, expected_code) in cases {
    let (_directory, _path, _coordinator, mut publisher) = create_environment(&format!("void-claim-authority-{name}"), None);
    publish_first_authority(&publisher);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    let source = prepare_void_catalog_publication(&publisher, &memory, 0);
    publish_prepared_void_catalog_support(&publisher, &source);
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      source.database_id,
      1,
      1,
      RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let mut source_authority = test_void_catalog_publication_authority();
    let source_receipt =
      publisher.publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut source_authority, &mut retirement_owner).unwrap();
    assert_eq!(source_receipt.manifest_key, source.manifest.key, "authority case {name}");
    let prepared = prepare_void_claim_admission(&source);
    publisher
      .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
        database_id: &source.database_id,
        artifact: &prepared.claim_directory,
        publication_timestamp_ms: 1_700_000_080_015,
      })
      .unwrap();
    let mut authority = test_void_claim_admission_authority(&source.manifest.key, 1);
    mutate_authority(&mut authority);

    let error =
      publisher.admit_void_claim(prepared.request(&cancellation, &memory, 4, 2), &mut authority, &mut retirement_owner).unwrap_err();

    assert_eq!(error.code(), expected_code, "authority case {name}");
    assert_eq!(authority.recheck_calls, 1, "authority case {name}");
    assert!(publisher.locator(&prepared.claim.key).unwrap().is_some(), "authority case {name}");
    assert!(publisher.locator(&prepared.result_manifest.key).unwrap().is_none(), "authority case {name}");
    assert!(publisher.locator(&prepared.result_control.key).unwrap().is_none(), "authority case {name}");
    assert_eq!(selected_void_catalog_manifest_key(&publisher), Some(source.manifest.key.clone()), "authority case {name}");
  }
}

#[test]
fn every_void_claim_selector_failure_restarts_as_exactly_source_or_claimed() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("void-claim-selector-{failure:?}"), None);
    publish_first_authority(&publisher);
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    let source = prepare_void_catalog_publication(&publisher, &memory, 0);
    publish_prepared_void_catalog_support(&publisher, &source);
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      source.database_id,
      1,
      1,
      RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let mut source_authority = test_void_catalog_publication_authority();
    let _source_receipt =
      publisher.publish_void_catalog(source.request(&cancellation, &memory, 2, 1), &mut source_authority, &mut retirement_owner).unwrap();
    let prepared = prepare_void_claim_admission(&source);
    publisher
      .publish_void_catalog_support_artifact(VoidCatalogSupportPublicationRequestV1 {
        database_id: &source.database_id,
        artifact: &prepared.claim_directory,
        publication_timestamp_ms: 1_700_000_080_015,
      })
      .unwrap();
    publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(NthHeaderPublicationFaultIo::new(failure, 4))),
      root_state: publisher.root_state,
    };
    let mut authority = test_void_claim_admission_authority(&source.manifest.key, 1);

    let error =
      publisher.admit_void_claim(prepared.request(&cancellation, &memory, 4, 2), &mut authority, &mut retirement_owner).unwrap_err();

    assert_eq!(authority.recheck_calls, 1, "failure {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure {failure:?}");
    let selector_may_have_committed = matches!(
      failure,
      FirstAuthorityFailurePoint::HeaderWriteAfter | FirstAuthorityFailurePoint::FullBarrier | FirstAuthorityFailurePoint::Verify
    );
    if selector_may_have_committed {
      let permit = error
        .committed_permit()
        .unwrap_or_else(|| panic!("uncertain claimed Void authority requires an exact permit for {failure:?}: {error:?}"));
      assert_eq!(permit.claim_key(), prepared.claim.key, "failure {failure:?}");
      assert_eq!(permit.result_manifest_key(), prepared.result_manifest.key, "failure {failure:?}");
    } else {
      assert!(error.committed_permit().is_none(), "failure {failure:?}");
    }
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    let expected_selected = if selector_may_have_committed { &prepared.result_manifest.key } else { &source.manifest.key };
    assert_eq!(selected_void_catalog_manifest_key(&reopened), Some(expected_selected.clone()), "failure {failure:?}");
    let restart_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    let mut restart_runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
    let restart_state = reopened
      .reconstruct_void_reusable_state(
        VoidReusableStateReconstructionRequestV1 { cancellation: &cancellation, memory: &restart_memory, limits: void_runtime_limits() },
        &mut restart_runtime_authority,
      )
      .unwrap()
      .unwrap();
    if selector_may_have_committed {
      assert_eq!(restart_state.free_count(), 0, "failure {failure:?}");
      assert_eq!(restart_state.outstanding_claim_count(), 1, "failure {failure:?}");
      assert!(restart_state.candidate_extents().is_empty(), "failure {failure:?}");
      assert_eq!(restart_runtime_authority.recheck_calls, 0, "failure {failure:?}");
    } else {
      assert_eq!(restart_state.free_count(), 1, "failure {failure:?}");
      assert_eq!(restart_state.outstanding_claim_count(), 0, "failure {failure:?}");
      assert_eq!(restart_state.candidate_extents().len(), 1, "failure {failure:?}");
      assert_eq!(restart_runtime_authority.recheck_calls, 1, "failure {failure:?}");
    }
    drop(restart_state);
    let retry_cancellation = CancellationToken::new();
    let retry_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      source.database_id,
      1,
      1,
      RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
      &retry_cancellation,
      &retry_memory,
    )
    .unwrap();
    let mut retry_authority = test_void_claim_admission_authority(&source.manifest.key, 1);
    retry_authority.fail_recheck = selector_may_have_committed;
    let retry = reopened
      .admit_void_claim(prepared.request(&retry_cancellation, &retry_memory, 4, 3), &mut retry_authority, &mut retry_owner)
      .unwrap();
    assert_eq!(retry_authority.recheck_calls, if selector_may_have_committed { 0 } else { 1 }, "failure {failure:?}");
    assert_eq!(retry.idempotent(), selector_may_have_committed, "failure {failure:?}");
    assert_eq!(retry.claim_key(), prepared.claim.key, "failure {failure:?}");
    assert_eq!(selected_void_catalog_manifest_key(&reopened), Some(prepared.result_manifest.key.clone()), "failure {failure:?}");
    let mut retry_runtime_authority = TestVoidReclaimReceiptAuthorityV1::default();
    let retry_state = reopened
      .reconstruct_void_reusable_state(
        VoidReusableStateReconstructionRequestV1 {
          cancellation: &retry_cancellation,
          memory: &retry_memory,
          limits: void_runtime_limits(),
        },
        &mut retry_runtime_authority,
      )
      .unwrap()
      .unwrap();
    assert_eq!(retry_state.free_count(), 0, "failure {failure:?}");
    assert_eq!(retry_state.outstanding_claim_count(), 1, "failure {failure:?}");
    assert!(retry_state.candidate_extents().is_empty(), "failure {failure:?}");
    assert_eq!(retry_runtime_authority.recheck_calls, 0, "failure {failure:?}");
  }
}

#[test]
fn corrupt_void_support_refuses_before_manifest_or_selector_publication() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("void-catalog-corrupt-support", None);
  publish_first_authority(&publisher);
  let completion_memory = MemoryCoordinator::new(MemoryPolicy::new(4 << 20, 8 << 20, 1, 1 << 20).unwrap());
  let prepared = prepare_void_catalog_publication(&publisher, &completion_memory, 0);
  publish_prepared_void_catalog_support(&publisher, &prepared);
  corrupt_last_entity_byte(&publisher, &prepared.directory.key);

  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    prepared.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut authority = test_void_catalog_publication_authority();

  let error =
    publisher.publish_void_catalog(prepared.request(&cancellation, &memory, 2, 1), &mut authority, &mut retirement_owner).unwrap_err();

  assert!(matches!(error, VoidCatalogPublicationErrorV1::Format(_) | VoidCatalogPublicationErrorV1::Authority(_)));
  assert_eq!(authority.recheck_calls, 0);
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.control.key).unwrap().is_none());
  assert_eq!(selected_void_catalog_manifest_key(&publisher), None);
}

#[test]
fn void_extent_commit_sequence_must_match_the_exact_locator_removal_completion() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("void-catalog-commit-sequence", None);
  publish_first_authority(&publisher);
  let completion_memory = MemoryCoordinator::new(MemoryPolicy::new(4 << 20, 8 << 20, 1, 1 << 20).unwrap());
  let prepared = prepare_void_catalog_publication(&publisher, &completion_memory, 1);
  publish_prepared_void_catalog_support(&publisher, &prepared);

  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    prepared.database_id,
    1,
    1,
    RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut authority = test_void_catalog_publication_authority();

  let error =
    publisher.publish_void_catalog(prepared.request(&cancellation, &memory, 2, 1), &mut authority, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "void_closure_sweep_extents");
  assert_eq!(authority.recheck_calls, 0);
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.control.key).unwrap().is_none());
  assert_eq!(selected_void_catalog_manifest_key(&publisher), None);
}

#[test]
fn every_void_selector_failure_restarts_as_exactly_prior_or_selected_and_reuse_blocked() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("void-catalog-selector-{failure:?}"), None);
    publish_first_authority(&publisher);
    let completion_memory = MemoryCoordinator::new(MemoryPolicy::new(4 << 20, 8 << 20, 1, 1 << 20).unwrap());
    let prepared = prepare_void_catalog_publication(&publisher, &completion_memory, 0);
    publish_prepared_void_catalog_support(&publisher, &prepared);
    publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(NthHeaderPublicationFaultIo::new(failure, 3))),
      root_state: publisher.root_state,
    };
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      prepared.database_id,
      1,
      1,
      RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let mut authority = test_void_catalog_publication_authority();

    let error =
      publisher.publish_void_catalog(prepared.request(&cancellation, &memory, 2, 1), &mut authority, &mut retirement_owner).unwrap_err();

    assert_eq!(authority.recheck_calls, 1, "failure {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure {failure:?}");
    let selector_may_have_committed = matches!(
      failure,
      FirstAuthorityFailurePoint::HeaderWriteAfter | FirstAuthorityFailurePoint::FullBarrier | FirstAuthorityFailurePoint::Verify
    );
    if selector_may_have_committed {
      let receipt = error
        .committed_receipt()
        .unwrap_or_else(|| panic!("uncertain selected Void authority requires an exact receipt for {failure:?}: {error:?}"));
      assert_eq!(receipt.manifest_key, prepared.manifest.key, "failure {failure:?}");
      assert!(receipt.receipt_reconciliation_required, "failure {failure:?}");
      assert!(receipt.reuse_blocked, "failure {failure:?}");
    } else {
      assert!(error.committed_receipt().is_none(), "failure {failure:?}");
    }
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    let expected_selected = selector_may_have_committed.then(|| prepared.manifest.key.clone());
    assert_eq!(selected_void_catalog_manifest_key(&reopened), expected_selected, "failure {failure:?}");
    let retry_cancellation = CancellationToken::new();
    let retry_memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      prepared.database_id,
      1,
      1,
      RetirementJournalBufferOptionsV1::new(256, 1 << 20, 30_000),
      &retry_cancellation,
      &retry_memory,
    )
    .unwrap();
    let mut retry_authority = test_void_catalog_publication_authority();
    retry_authority.fail_recheck = selector_may_have_committed;

    let retry = reopened
      .publish_void_catalog(prepared.request(&retry_cancellation, &retry_memory, 2, 2), &mut retry_authority, &mut retry_owner)
      .unwrap();

    assert_eq!(retry_authority.recheck_calls, if selector_may_have_committed { 0 } else { 1 }, "failure {failure:?}");
    assert_eq!(retry.idempotent, selector_may_have_committed, "failure {failure:?}");
    assert!(retry.receipt_reconciliation_required, "failure {failure:?}");
    assert!(retry.reuse_blocked, "failure {failure:?}");
    assert_eq!(selected_void_catalog_manifest_key(&reopened), Some(prepared.manifest.key.clone()), "failure {failure:?}");
  }
}

#[test]
fn sweep_receipt_commit_time_cannot_predate_the_durable_proposal() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x32; 16];
  let batch_id = [0x92; 16];
  let logical_key = digest_parts(algorithm, &[b"sweep temporal logical key"]);
  let integrity = digest_parts(algorithm, &[b"sweep temporal integrity"]);
  let quarantine_manifest_hash = digest_parts(algorithm, &[b"sweep temporal quarantine"]);
  let candidates = [PhysicalIncarnationV1 {
    logical_key: &logical_key,
    integrity_or_legacy_digest: &integrity,
    wal_offset: 12_288,
    write_sequence: 78,
    entity_length: 768,
    entry_type: 1,
    entity_version: 1,
  }];
  let proposal_artifact = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: 104,
    created_at_ms: 1_700_000_090_000,
    quarantine_manifest_hash: &quarantine_manifest_hash,
    candidates: &candidates,
  })
  .unwrap();
  let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&proposal_artifact.value, algorithm).unwrap() else {
    panic!("encoded sweep proposal must decode as a proposal")
  };
  let cancellation = CancellationToken::new();
  let request = SweepReceiptVoidAuthorityRequestV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: proposal.generation,
    proposal_hash: &proposal_artifact.key,
    proposal_write_sequence: 418,
    proposal: &proposal,
    recovery: true,
    cancellation: &cancellation,
  };
  let reclaimed = SweepLocatorRemovalOutcomeV1 {
    ordinal: 0,
    outcome: SweepOutcomeClassV1::Reclaimed,
    stable_reason_detail: 0,
    resulting_void_offset: candidates[0].wal_offset,
    resulting_void_length: candidates[0].entity_length,
  };
  let void_catalog_hash = digest_parts(algorithm, &[b"sweep temporal Void catalog"]);
  let mut authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  authority.snapshot.reclaim_committed_at_ms = proposal.created_at_ms - 1;
  let memory = MemoryCoordinator::new(MemoryPolicy::new(1 << 20, 2 << 20, 1, 1 << 16).unwrap());
  let baseline_reserved = memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes;
  let reservation = reserve_sweep_receipt_reconciliation_v1(algorithm, proposal.candidate_count, &memory).unwrap();

  let error = prepare_sweep_receipt_reconciliation_v1(request, &authority.snapshot, &[reclaimed], reservation).unwrap_err();
  assert_eq!(error.code(), "sweep_receipt_time");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, baseline_reserved);
}

#[test]
fn partial_sweep_receipt_preserves_every_outcome_and_restarts_idempotently() {
  let (_directory, path, _coordinator, publisher) = create_environment("partial-sweep-receipt", None);
  publish_first_authority(&publisher);
  let observation = publisher.observe().unwrap();
  let algorithm = observation.selected.header.hash_algorithm;
  let database_id = observation.selected.header.database_id;
  let batch_id = [0x93; 16];
  let logical_keys = [vec![0x11; algorithm.hash_length()], vec![0x22; algorithm.hash_length()], vec![0x33; algorithm.hash_length()]];
  let integrity_digests = [vec![0x44; algorithm.hash_length()], vec![0x55; algorithm.hash_length()], vec![0x66; algorithm.hash_length()]];
  let candidates = [
    PhysicalIncarnationV1 {
      logical_key: &logical_keys[0],
      integrity_or_legacy_digest: &integrity_digests[0],
      wal_offset: 16_384,
      write_sequence: 81,
      entity_length: 512,
      entry_type: 1,
      entity_version: 1,
    },
    PhysicalIncarnationV1 {
      logical_key: &logical_keys[1],
      integrity_or_legacy_digest: &integrity_digests[1],
      wal_offset: 20_480,
      write_sequence: 82,
      entity_length: 768,
      entry_type: 1,
      entity_version: 1,
    },
    PhysicalIncarnationV1 {
      logical_key: &logical_keys[2],
      integrity_or_legacy_digest: &integrity_digests[2],
      wal_offset: 24_576,
      write_sequence: 83,
      entity_length: 1_024,
      entry_type: 1,
      entity_version: 1,
    },
  ];
  let quarantine_manifest_hash = digest_parts(algorithm, &[b"partial sweep quarantine"]);
  let proposal_artifact = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: 105,
    created_at_ms: 1_700_000_089_500,
    quarantine_manifest_hash: &quarantine_manifest_hash,
    candidates: &candidates,
  })
  .unwrap();
  let proposal_write_sequence = publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::SweepProposal,
        database_id: &database_id,
        artifact_key: &proposal_artifact.key,
        value: &proposal_artifact.value,
        minimum_timestamp_ms: 1_700_000_089_500,
        committed_postcondition_code: "test_partial_sweep_proposal",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&proposal_artifact.value, algorithm).unwrap() else {
    panic!("encoded sweep proposal must decode as a proposal")
  };
  let outcomes = [
    SweepLocatorRemovalOutcomeV1 {
      ordinal: 0,
      outcome: SweepOutcomeClassV1::Reclaimed,
      stable_reason_detail: 0,
      resulting_void_offset: candidates[0].wal_offset,
      resulting_void_length: candidates[0].entity_length,
    },
    SweepLocatorRemovalOutcomeV1 {
      ordinal: 1,
      outcome: SweepOutcomeClassV1::SkippedPinned,
      stable_reason_detail: 41,
      resulting_void_offset: 0,
      resulting_void_length: 0,
    },
    SweepLocatorRemovalOutcomeV1 {
      ordinal: 2,
      outcome: SweepOutcomeClassV1::FailedIo,
      stable_reason_detail: 42,
      resulting_void_offset: 0,
      resulting_void_length: 0,
    },
  ];
  let cancellation = CancellationToken::new();
  let removal_request = SweepLocatorRemovalAuthorityRequestV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: proposal.generation,
    proposal_hash: &proposal_artifact.key,
    proposal_write_sequence,
    quarantine_manifest_hash: &quarantine_manifest_hash,
    proposal: &proposal,
    cancellation: &cancellation,
  };
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let removal_memory = reserve_sweep_locator_removal_results_v1(&memory, proposal.candidate_count).unwrap();
  let completion = complete_sweep_locator_removal_v1(
    removal_request,
    SweepLocatorRemovalBatchOutcomeV1 { reclaim_commit_sequence: proposal_write_sequence + 1, outcomes: outcomes.to_vec() },
    removal_memory,
  )
  .unwrap();
  let void_catalog_hash = digest_parts(algorithm, &[b"partial sweep selected Void catalog"]);
  let mut authority = test_sweep_receipt_void_authority(&void_catalog_hash, outcomes.to_vec());
  let request = SweepReceiptReconciliationRequestV1 {
    source: SweepReceiptReconciliationSourceV1::Completion(&completion),
    cancellation: &cancellation,
    memory: &memory,
  };
  let receipt = publisher.reconcile_sweep_receipt(request, &mut authority).unwrap();
  let expected_outcomes = [
    SweepReceiptOutcomeWriteV1 {
      incarnation: candidates[0],
      outcome: outcomes[0].outcome,
      stable_reason_detail: outcomes[0].stable_reason_detail,
      resulting_void_offset: outcomes[0].resulting_void_offset,
      resulting_void_length: outcomes[0].resulting_void_length,
    },
    SweepReceiptOutcomeWriteV1 {
      incarnation: candidates[1],
      outcome: outcomes[1].outcome,
      stable_reason_detail: outcomes[1].stable_reason_detail,
      resulting_void_offset: outcomes[1].resulting_void_offset,
      resulting_void_length: outcomes[1].resulting_void_length,
    },
    SweepReceiptOutcomeWriteV1 {
      incarnation: candidates[2],
      outcome: outcomes[2].outcome,
      stable_reason_detail: outcomes[2].stable_reason_detail,
      resulting_void_offset: outcomes[2].resulting_void_offset,
      resulting_void_length: outcomes[2].resulting_void_length,
    },
  ];
  let expected_receipt = encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
    hash_algorithm: algorithm,
    recovered: false,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: proposal.generation,
    reclaim_committed_at_ms: authority.snapshot.reclaim_committed_at_ms,
    proposal_hash: &proposal_artifact.key,
    void_catalog_hash: &void_catalog_hash,
    outcomes: &expected_outcomes,
  })
  .unwrap();
  assert_eq!(receipt.receipt_key, expected_receipt.key);
  assert!(!receipt.recovered);
  assert_eq!(authority.recheck_calls, 1);
  assert_eq!(authority.recovery_calls, 0);
  assert!(publisher.locator(&receipt.receipt_key).unwrap().is_some());

  drop(publisher);
  let (_restart_coordinator, reopened) = reopen(&path);
  let restart_cancellation = CancellationToken::new();
  let restart_request = SweepReceiptReconciliationRequestV1 {
    source: SweepReceiptReconciliationSourceV1::Recovery(SweepReceiptRecoveryIdentityV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      proposal_hash: &proposal_artifact.key,
      proposal_write_sequence,
    }),
    cancellation: &restart_cancellation,
    memory: &memory,
  };
  let mut restart_authority = test_sweep_receipt_void_authority(&void_catalog_hash, outcomes.to_vec());
  restart_authority.snapshot.allocator_admission_blocked = false;
  restart_authority.snapshot.existing_receipt = Some(ExistingSweepReceiptAuthorityV1 {
    receipt_hash: receipt.receipt_key.clone(),
    receipt_write_sequence: receipt.hard_publication_sequence,
  });
  let restarted_receipt = reopened.reconcile_sweep_receipt(restart_request, &mut restart_authority).unwrap();
  assert_eq!(restarted_receipt, receipt);
  assert_eq!(restart_authority.recheck_calls, 1);
  assert_eq!(restart_authority.recovery_calls, 0);

  let mut conflicting_outcomes = expected_outcomes;
  conflicting_outcomes[1].outcome = SweepOutcomeClassV1::SkippedPolicy;
  conflicting_outcomes[1].stable_reason_detail = 77;
  let conflicting_receipt = encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
    hash_algorithm: algorithm,
    recovered: false,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: proposal.generation,
    reclaim_committed_at_ms: authority.snapshot.reclaim_committed_at_ms,
    proposal_hash: &proposal_artifact.key,
    void_catalog_hash: &void_catalog_hash,
    outcomes: &conflicting_outcomes,
  })
  .unwrap();
  let conflicting_sequence = reopened
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::SweepCommitReceipt,
        database_id: &database_id,
        artifact_key: &conflicting_receipt.key,
        value: &conflicting_receipt.value,
        minimum_timestamp_ms: u64::try_from(authority.snapshot.reclaim_committed_at_ms).unwrap(),
        committed_postcondition_code: "test_conflicting_partial_sweep_receipt",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let before_dishonest_reconcile = reopened.observe().unwrap();
  let mut dishonest_authority = test_sweep_receipt_void_authority(&void_catalog_hash, outcomes.to_vec());
  dishonest_authority.snapshot.allocator_admission_blocked = false;
  dishonest_authority.snapshot.existing_receipt =
    Some(ExistingSweepReceiptAuthorityV1 { receipt_hash: conflicting_receipt.key, receipt_write_sequence: conflicting_sequence });
  let dishonest_request = SweepReceiptReconciliationRequestV1 {
    source: SweepReceiptReconciliationSourceV1::Completion(&completion),
    cancellation: &restart_cancellation,
    memory: &memory,
  };
  let dishonest_error = reopened.reconcile_sweep_receipt(dishonest_request, &mut dishonest_authority).unwrap_err();
  assert_eq!(dishonest_error.code(), "sweep_receipt_existing_conflict");
  assert_eq!(dishonest_authority.recovery_calls, 0);
  assert_eq!(reopened.observe().unwrap(), before_dishonest_reconcile);
}

#[test]
fn sweep_receipt_recovery_fails_closed_and_hard_publishes_exactly_once() {
  let (_directory, _path, _coordinator, publisher) = create_environment("sweep-receipt-recovery", None);
  publish_first_authority(&publisher);
  let observation = publisher.observe().unwrap();
  let algorithm = observation.selected.header.hash_algorithm;
  let database_id = observation.selected.header.database_id;
  let batch_id = [0xA7; 16];
  let logical_key = digest_parts(algorithm, &[b"recovered sweep logical key"]);
  let integrity = digest_parts(algorithm, &[b"recovered sweep integrity"]);
  let quarantine_manifest_hash = digest_parts(algorithm, &[b"recovered sweep quarantine"]);
  let candidates = [PhysicalIncarnationV1 {
    logical_key: &logical_key,
    integrity_or_legacy_digest: &integrity,
    wal_offset: 24_576,
    write_sequence: 801,
    entity_length: 1_024,
    entry_type: 1,
    entity_version: 1,
  }];
  let proposal = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    batch_id: &batch_id,
    generation: 211,
    created_at_ms: 1_700_000_089_000,
    quarantine_manifest_hash: &quarantine_manifest_hash,
    candidates: &candidates,
  })
  .unwrap();
  let proposal_write_sequence = publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::SweepProposal,
        database_id: &database_id,
        artifact_key: &proposal.key,
        value: &proposal.value,
        minimum_timestamp_ms: 1_700_000_089_000,
        committed_postcondition_code: "test_recovered_sweep_proposal",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let reclaimed = SweepLocatorRemovalOutcomeV1 {
    ordinal: 0,
    outcome: SweepOutcomeClassV1::Reclaimed,
    stable_reason_detail: 0,
    resulting_void_offset: candidates[0].wal_offset,
    resulting_void_length: candidates[0].entity_length,
  };
  let void_catalog_hash = digest_parts(algorithm, &[b"selected recovered Void catalog"]);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let request = SweepReceiptReconciliationRequestV1 {
    source: SweepReceiptReconciliationSourceV1::Recovery(SweepReceiptRecoveryIdentityV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      proposal_hash: &proposal.key,
      proposal_write_sequence,
    }),
    cancellation: &cancellation,
    memory: &memory,
  };
  let baseline = publisher.observe().unwrap();

  let canceled = CancellationToken::new();
  canceled.cancel();
  let canceled_request = SweepReceiptReconciliationRequestV1 { cancellation: &canceled, ..request };
  let mut canceled_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  assert_eq!(publisher.reconcile_sweep_receipt(canceled_request, &mut canceled_authority).unwrap_err().code(), "sweep_receipt_canceled");
  assert_eq!(canceled_authority.recheck_calls, 0);

  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
  let constrained_request = SweepReceiptReconciliationRequestV1 { memory: &constrained_memory, ..request };
  let mut constrained_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  assert_eq!(
    publisher.reconcile_sweep_receipt(constrained_request, &mut constrained_authority).unwrap_err().code(),
    "sweep_receipt_memory"
  );
  assert_eq!(constrained_authority.recheck_calls, 0);

  let wrong_sequence_identity = SweepReceiptRecoveryIdentityV1 {
    proposal_write_sequence: proposal_write_sequence + 1,
    ..match request.source {
      SweepReceiptReconciliationSourceV1::Recovery(identity) => identity,
      SweepReceiptReconciliationSourceV1::Completion(_) => unreachable!(),
    }
  };
  let wrong_sequence_request =
    SweepReceiptReconciliationRequestV1 { source: SweepReceiptReconciliationSourceV1::Recovery(wrong_sequence_identity), ..request };
  let mut wrong_sequence_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  assert_eq!(
    publisher.reconcile_sweep_receipt(wrong_sequence_request, &mut wrong_sequence_authority).unwrap_err().code(),
    "sweep_receipt_proposal_changed"
  );
  assert_eq!(wrong_sequence_authority.recheck_calls, 0);

  let original_proposal_locator = publisher.locator(&proposal.key).unwrap().unwrap();
  let mut corrupt_proposal_locator = original_proposal_locator.clone();
  corrupt_proposal_locator.type_flags = KV_TYPE_CHUNK;
  publisher.kv.lock().unwrap().insert(corrupt_proposal_locator).unwrap();
  let mut corrupt_proposal_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  assert_eq!(
    publisher.reconcile_sweep_receipt(request, &mut corrupt_proposal_authority).unwrap_err().code(),
    "sweep_receipt_proposal_collision"
  );
  assert_eq!(corrupt_proposal_authority.recheck_calls, 0);
  publisher.kv.lock().unwrap().insert(original_proposal_locator).unwrap();

  for case in [
    "hash",
    "generation",
    "time",
    "selected",
    "closure",
    "reclaimed",
    "nonreclaimed",
    "locator",
    "lineage",
    "memory",
    "allocator",
    "search",
    "conflict",
    "existing-hash",
    "existing-sequence",
    "repair",
  ] {
    let mut authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
    match case {
      "hash" => authority.snapshot.selected_void_catalog_hash.clear(),
      "generation" => authority.snapshot.selected_void_catalog_generation = 0,
      "time" => authority.snapshot.reclaim_committed_at_ms = 0,
      "selected" => authority.snapshot.selected_void_catalog_current = false,
      "closure" => authority.snapshot.proposal_catalog_closure_complete = false,
      "reclaimed" => authority.snapshot.reclaimed_extents_exact = false,
      "nonreclaimed" => authority.snapshot.nonreclaimed_extents_absent = false,
      "locator" => authority.snapshot.locator_removals_durable = false,
      "lineage" => authority.snapshot.replacement_lineage_complete = false,
      "memory" => authority.snapshot.memory_coordinator_current = false,
      "allocator" => authority.snapshot.allocator_admission_blocked = false,
      "search" => authority.snapshot.receipt_search_complete = false,
      "conflict" => authority.snapshot.conflicting_receipt_count = 1,
      "existing-hash" => {
        authority.snapshot.existing_receipt = Some(ExistingSweepReceiptAuthorityV1 { receipt_hash: Vec::new(), receipt_write_sequence: 1 });
      }
      "existing-sequence" => {
        authority.snapshot.existing_receipt =
          Some(ExistingSweepReceiptAuthorityV1 { receipt_hash: void_catalog_hash.clone(), receipt_write_sequence: 0 });
      }
      "repair" => authority.snapshot.repair_latch_clear = false,
      _ => unreachable!(),
    }
    let error = publisher.reconcile_sweep_receipt(request, &mut authority).unwrap_err();
    assert!(
      matches!(
        error.code(),
        "sweep_receipt_void_identity"
          | "sweep_receipt_void_authority_changed"
          | "sweep_receipt_allocator_unblocked"
          | "sweep_receipt_existing_identity"
      ),
      "case {case}: {}",
      error.code()
    );
    assert_eq!(authority.recovery_calls, 0, "case {case}");
    assert_eq!(publisher.observe().unwrap(), baseline, "case {case}");
  }

  let mut refused_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  refused_authority.fail_recheck = true;
  assert_eq!(publisher.reconcile_sweep_receipt(request, &mut refused_authority).unwrap_err().code(), "sweep_receipt_test_recheck");
  assert_eq!(refused_authority.recovery_calls, 0);

  let mut failed_recovery = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  failed_recovery.fail_recovery = true;
  assert_eq!(publisher.reconcile_sweep_receipt(request, &mut failed_recovery).unwrap_err().code(), "sweep_receipt_test_recovery");

  let mut malformed_recovery = test_sweep_receipt_void_authority(
    &void_catalog_hash,
    vec![SweepLocatorRemovalOutcomeV1 { resulting_void_offset: reclaimed.resulting_void_offset + 1, ..reclaimed }],
  );
  assert_eq!(publisher.reconcile_sweep_receipt(request, &mut malformed_recovery).unwrap_err().code(), "sweep_removal_outcome_shape");
  let mut missing_recovery = test_sweep_receipt_void_authority(&void_catalog_hash, Vec::new());
  assert_eq!(publisher.reconcile_sweep_receipt(request, &mut missing_recovery).unwrap_err().code(), "sweep_removal_outcome_count");

  let recovery_cancellation = CancellationToken::new();
  let canceling_request = SweepReceiptReconciliationRequestV1 { cancellation: &recovery_cancellation, ..request };
  let mut canceling_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  canceling_authority.cancel_during_recovery = true;
  assert_eq!(publisher.reconcile_sweep_receipt(canceling_request, &mut canceling_authority).unwrap_err().code(), "sweep_receipt_canceled");

  let recheck_cancellation = CancellationToken::new();
  let recheck_canceling_request = SweepReceiptReconciliationRequestV1 { cancellation: &recheck_cancellation, ..request };
  let mut recheck_canceling_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  recheck_canceling_authority.cancel_during_recheck = true;
  assert_eq!(
    publisher.reconcile_sweep_receipt(recheck_canceling_request, &mut recheck_canceling_authority).unwrap_err().code(),
    "sweep_receipt_canceled"
  );
  assert_eq!(recheck_canceling_authority.recovery_calls, 0);

  let mut exact_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  let receipt = publisher.reconcile_sweep_receipt(request, &mut exact_authority).unwrap();
  assert!(receipt.recovered);
  assert_eq!(receipt.void_catalog_hash, void_catalog_hash);
  assert_eq!(receipt.reclaim_committed_at_ms, exact_authority.snapshot.reclaim_committed_at_ms);
  assert!(publisher.locator(&receipt.receipt_key).unwrap().is_some());
  assert_eq!(exact_authority.recheck_calls, 1);
  assert_eq!(exact_authority.recovery_calls, 1);

  let mut exact_retry_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  exact_retry_authority.snapshot.allocator_admission_blocked = false;
  exact_retry_authority.snapshot.existing_receipt = Some(ExistingSweepReceiptAuthorityV1 {
    receipt_hash: receipt.receipt_key.clone(),
    receipt_write_sequence: receipt.hard_publication_sequence,
  });
  let retry = publisher.reconcile_sweep_receipt(request, &mut exact_retry_authority).unwrap();
  assert_eq!(retry, receipt);

  let original_receipt_locator = publisher.locator(&receipt.receipt_key).unwrap().unwrap();
  let mut corrupt_receipt_locator = original_receipt_locator.clone();
  corrupt_receipt_locator.type_flags = KV_TYPE_CHUNK;
  publisher.kv.lock().unwrap().insert(corrupt_receipt_locator).unwrap();
  let mut corrupt_existing = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  corrupt_existing.snapshot.allocator_admission_blocked = false;
  corrupt_existing.snapshot.existing_receipt = exact_retry_authority.snapshot.existing_receipt.clone();
  assert_eq!(publisher.reconcile_sweep_receipt(request, &mut corrupt_existing).unwrap_err().code(), "sweep_receipt_existing_collision");
  publisher.kv.lock().unwrap().insert(original_receipt_locator).unwrap();

  let mut wrong_existing_sequence = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  wrong_existing_sequence.snapshot.allocator_admission_blocked = false;
  wrong_existing_sequence.snapshot.existing_receipt = Some(ExistingSweepReceiptAuthorityV1 {
    receipt_hash: receipt.receipt_key.clone(),
    receipt_write_sequence: receipt.hard_publication_sequence + 1,
  });
  assert_eq!(
    publisher.reconcile_sweep_receipt(request, &mut wrong_existing_sequence).unwrap_err().code(),
    "sweep_receipt_existing_changed"
  );

  let mut conflicting_existing = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  conflicting_existing.snapshot.allocator_admission_blocked = false;
  conflicting_existing.snapshot.existing_receipt =
    Some(ExistingSweepReceiptAuthorityV1 { receipt_hash: proposal.key.clone(), receipt_write_sequence: proposal_write_sequence });
  assert_eq!(publisher.reconcile_sweep_receipt(request, &mut conflicting_existing).unwrap_err().code(), "sweep_receipt_existing_kind");
}

#[derive(Clone, Copy, Debug)]
enum FirstAuthorityFailurePoint {
  DataBarrier,
  HeaderWriteBefore,
  HeaderWriteAfter,
  FullBarrier,
  Verify,
}

#[derive(Debug)]
struct FaultingNativeHeaderPublicationIo {
  failure: FirstAuthorityFailurePoint,
}

impl FaultingNativeHeaderPublicationIo {
  fn injected(operation: NativeDurabilityOperation) -> NativeDurabilityError {
    NativeDurabilityError::operation_io(operation, std::io::Error::other("injected first-authority publication failure"))
  }
}

#[derive(Debug)]
struct NthHeaderPublicationFaultIo {
  failure: FirstAuthorityFailurePoint,
  target_publication: usize,
  current_publication: AtomicUsize,
}

impl NthHeaderPublicationFaultIo {
  fn new(failure: FirstAuthorityFailurePoint, target_publication: usize) -> Self {
    Self { failure, target_publication, current_publication: AtomicUsize::new(0) }
  }

  fn is_target(&self) -> bool {
    self.current_publication.load(AtomicOrdering::SeqCst) == self.target_publication
  }

  fn injected(operation: NativeDurabilityOperation) -> NativeDurabilityError {
    NativeDurabilityError::operation_io(operation, std::io::Error::other("injected final-selector publication failure"))
  }
}

impl HeaderPublicationIo for NthHeaderPublicationFaultIo {
  fn read_observation(&self, file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
    observe_database_header_v4(file)
  }

  fn data_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    let publication = self.current_publication.fetch_add(1, AtomicOrdering::SeqCst) + 1;
    if publication == self.target_publication && matches!(self.failure, FirstAuthorityFailurePoint::DataBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::DataBarrier));
    }
    sync_file_data_native(file)
  }

  fn write_slot(&self, file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError> {
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteBefore) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    write_file_at_native(file, (slot * DATABASE_HEADER_V4_SLOT_LENGTH) as u64, bytes)?;
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteAfter) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    Ok(())
  }

  fn full_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::FullBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::FileBarrier));
    }
    sync_file_all_native(file)
  }

  fn verify_region(&self, file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError> {
    if self.is_target() && matches!(self.failure, FirstAuthorityFailurePoint::Verify) {
      return Err(Self::injected(NativeDurabilityOperation::ReadBack));
    }
    verify_file_bytes_native(file, 0, expected)
  }
}

impl HeaderPublicationIo for FaultingNativeHeaderPublicationIo {
  fn read_observation(&self, file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
    observe_database_header_v4(file)
  }

  fn data_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::DataBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::DataBarrier));
    }
    sync_file_data_native(file)
  }

  fn write_slot(&self, file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteBefore) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    write_file_at_native(file, (slot * DATABASE_HEADER_V4_SLOT_LENGTH) as u64, bytes)?;
    if matches!(self.failure, FirstAuthorityFailurePoint::HeaderWriteAfter) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    Ok(())
  }

  fn full_barrier(&self, file: &File) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::FullBarrier) {
      return Err(Self::injected(NativeDurabilityOperation::FileBarrier));
    }
    sync_file_all_native(file)
  }

  fn verify_region(&self, file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError> {
    if matches!(self.failure, FirstAuthorityFailurePoint::Verify) {
      return Err(Self::injected(NativeDurabilityOperation::ReadBack));
    }
    verify_file_bytes_native(file, 0, expected)
  }
}

struct VisibilityObserver {
  called: bool,
}

impl FirstAuthorityDependencyObserverV1 for VisibilityObserver {
  fn staged(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    self.called = true;
    let snapshot = kv.snapshot_handle().load();
    for entity in entities {
      assert!(snapshot.get(&entity.key).unwrap().is_none());
      assert!(kv.get_buffered(&entity.key).is_some());
    }
    assert_eq!(kv.hot_buffer_len(), FIRST_AUTHORITY_ENTITY_COUNT);
    Ok(())
  }
}

struct FailingVisibilityObserver;

impl FirstAuthorityDependencyObserverV1 for FailingVisibilityObserver {
  fn staged(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    let snapshot = kv.snapshot_handle().load();
    assert!(entities.iter().all(|entity| snapshot.get(&entity.key).unwrap().is_none()));
    Err(NativeDurabilityError::invalid(NativeDurabilityOperation::ReadBack, "injected failure after hidden dependency staging"))
  }
}

struct FailingPostCommitObserver;

impl FirstAuthorityDependencyObserverV1 for FailingPostCommitObserver {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn authority_committed(
    &mut self,
    _kv: &DiskKVStore,
    _entities: &[PreparedWholeEntityV1],
  ) -> Result<(), FirstAuthorityPublicationErrorV1> {
    Err(FirstAuthorityPublicationErrorV1::invalid("injected_post_commit_failure", "injected failure after authority linearization"))
  }
}

struct CancelRetirementAfterCommitObserver {
  cancellation: CancellationToken,
}

impl FirstAuthorityDependencyObserverV1 for CancelRetirementAfterCommitObserver {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn authority_committed(
    &mut self,
    _kv: &DiskKVStore,
    _entities: &[PreparedWholeEntityV1],
  ) -> Result<(), FirstAuthorityPublicationErrorV1> {
    self.cancellation.cancel();
    Ok(())
  }
}

struct BlockingControlPublicationObserverV1 {
  staged: Arc<Barrier>,
  release: Arc<Barrier>,
}

impl FirstAuthorityDependencyObserverV1 for BlockingControlPublicationObserverV1 {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    self.staged.wait();
    self.release.wait();
    Ok(())
  }
}

#[derive(Clone, Copy, Debug)]
enum DependencyFailurePhase {
  BeforeEntity,
  EntityWritten,
  EntityStaged,
}

struct FailingDependencyObserver {
  phase: DependencyFailurePhase,
  entity_index: usize,
}

impl FirstAuthorityDependencyObserverV1 for FailingDependencyObserver {
  fn before_entity(&mut self, index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    self.fail_at(DependencyFailurePhase::BeforeEntity, index)
  }

  fn entity_written(&mut self, index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    self.fail_at(DependencyFailurePhase::EntityWritten, index)
  }

  fn entity_staged(&mut self, index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    self.fail_at(DependencyFailurePhase::EntityStaged, index)
  }

  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }
}

impl FailingDependencyObserver {
  fn fail_at(&self, phase: DependencyFailurePhase, index: usize) -> Result<(), NativeDurabilityError> {
    if std::mem::discriminant(&self.phase) == std::mem::discriminant(&phase) && self.entity_index == index {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        format!("injected {phase:?} failure for first-authority entity {index}"),
      ));
    }
    Ok(())
  }
}

#[derive(Debug)]
struct CapturedRetirementSegmentV1 {
  segment_ordinal: u64,
  generation: u64,
  first_replacement_sequence: u64,
  last_replacement_sequence: u64,
  record_count: u32,
  artifact_key: Vec<u8>,
  value: Vec<u8>,
}

impl CapturedRetirementSegmentV1 {
  fn prepared(&self) -> PreparedRetirementJournalSegmentV1<'_> {
    PreparedRetirementJournalSegmentV1 {
      segment_ordinal: self.segment_ordinal,
      generation: self.generation,
      first_replacement_sequence: self.first_replacement_sequence,
      last_replacement_sequence: self.last_replacement_sequence,
      record_count: self.record_count,
      artifact_key: &self.artifact_key,
      value: &self.value,
    }
  }
}

#[derive(Default)]
struct CapturingRetirementSinkV1 {
  captured: Option<CapturedRetirementSegmentV1>,
}

impl RetirementJournalDurableSinkV1 for CapturingRetirementSinkV1 {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    self.captured = Some(CapturedRetirementSegmentV1 {
      segment_ordinal: segment.segment_ordinal,
      generation: segment.generation,
      first_replacement_sequence: segment.first_replacement_sequence,
      last_replacement_sequence: segment.last_replacement_sequence,
      record_count: segment.record_count,
      artifact_key: segment.artifact_key.to_vec(),
      value: segment.value.to_vec(),
    });
    Ok(RetirementJournalDurabilityReceiptV1 {
      artifact_key: segment.artifact_key.to_vec(),
      stored_value_length: segment.value.len() as u32,
      hard_publication_sequence: 1,
    })
  }
}

fn create_environment(
  name: &str,
  failure: Option<FirstAuthorityFailurePoint>,
) -> (tempfile::TempDir, PathBuf, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  create_environment_for_database(name, failure, [0x31; 16])
}

fn create_environment_for_database(
  name: &str,
  failure: Option<FirstAuthorityFailurePoint>,
  database_id: [u8; 16],
) -> (tempfile::TempDir, PathBuf, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join(format!("{name}.aeordb"));
  let mut file = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let kv_block_length = initial_block_size();
  let header = DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id,
    write_sequence_high_water: 1,
    required_reader_capabilities: [0; 32],
    kv_block_offset: DATABASE_HEADER_V4_DATA_OFFSET,
    kv_block_length,
    kv_block_version: DiskKVStore::CURRENT_KV_BLOCK_VERSION,
    kv_block_stage: 0,
    resize_in_progress: false,
    resize_target_stage: 0,
    nvt_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    nvt_length: 0,
    nvt_version: 1,
    backup_type: 0,
    hot_tail_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    buffer_kvs_offset: 0,
    buffer_nvt_offset: 0,
    entry_count: 0,
    head_hash: vec![0; algorithm.hash_length()],
    base_hash: vec![0; algorithm.hash_length()],
    target_hash: vec![0; algorithm.hash_length()],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; algorithm.hash_length()],
    writer_fence_epoch: 1,
    physical_instance_id: [0x51; 16],
  };
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    coordinator.clone(),
  )
  .unwrap();
  file.sync_all().unwrap();
  let publisher = if let Some(failure) = failure {
    let publisher_file = kv.clone_database_file().unwrap();
    let observation = observe_database_header_v4(&publisher_file).unwrap();
    validate_kv_header_alignment(&kv, &observation.selected.header).unwrap();
    V4FirstAuthorityPublisher {
      file: publisher_file,
      kv: Mutex::new(kv),
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(FaultingNativeHeaderPublicationIo { failure })),
      root_state: Mutex::new(()),
    }
  } else {
    V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap()
  };
  (directory, path, coordinator, publisher)
}

fn environment(name: &str) -> (tempfile::TempDir, Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let (directory, _path, coordinator, publisher) = create_environment(name, None);
  (directory, coordinator, publisher)
}

fn reopen(path: &Path) -> (Arc<DurabilityCoordinator>, V4FirstAuthorityPublisher) {
  let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
  let observation = observe_database_header_v4(&file).unwrap();
  let header = &observation.selected.header;
  let hot_tail = if header.head_hash.iter().any(|byte| *byte != 0) {
    read_hot_tail_checked(&mut file, header.hot_tail_offset, header.hash_algorithm.hash_length()).unwrap()
  } else {
    HotTailPayload::default()
  };
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::open_with_layout_and_coordinator(
    file.try_clone().unwrap(),
    header.hash_algorithm,
    header.kv_block_offset,
    header.kv_block_length,
    header.hot_tail_offset,
    header.kv_block_stage as usize,
    hot_tail.writes,
    hot_tail.voids,
    header.kv_block_version,
    coordinator.clone(),
  )
  .unwrap();
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator.clone()).unwrap();
  (coordinator, publisher)
}

fn seed_namespace_tree_collision(publisher: &V4FirstAuthorityPublisher, request: &FirstAuthorityPublicationRequestV1) {
  let mut observation = publisher.observe().unwrap();
  let sequence = observation.selected.header.write_sequence_high_water + 1;
  let entity = encode_entity(
    EntryTypeV4::DirectoryIndex,
    0,
    observation.selected.header.hash_algorithm,
    request.created_at_ms,
    sequence,
    &request.namespace_tree.root_hash,
    &request.namespace_tree.stored_value,
  )
  .unwrap();
  let offset = observation.selected.header.hot_tail_offset;
  write_file_at_native(&publisher.file, offset, &entity).unwrap();

  let mut kv = publisher.kv.lock().unwrap();
  let hot_tail_offset = offset + entity.len() as u64;
  kv.set_hot_tail_offset(hot_tail_offset);
  kv.insert(KVEntry {
    type_flags: KV_TYPE_DIRECTORY,
    hash: request.namespace_tree.root_hash.clone(),
    offset,
    total_length: entity.len() as u32,
  })
  .unwrap();
  kv.force_flush_hot_buffer().unwrap();
  drop(kv);

  observation.selected.header.updated_at_ms += 1;
  observation.selected.header.write_sequence_high_water = sequence;
  observation.selected.header.hot_tail_offset = hot_tail_offset;
  observation.selected.header.entry_count = 1;
  let slot = encode_database_header_slot(&observation.selected.header).unwrap();
  write_file_at_native(&publisher.file, 0, &slot).unwrap();
  write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &slot).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
}

fn request() -> FirstAuthorityPublicationRequestV1 {
  request_for_database([0x31; 16])
}

fn request_for_database(database_id: [u8; 16]) -> FirstAuthorityPublicationRequestV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  FirstAuthorityPublicationRequestV1 {
    database_id,
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state: encode_semantic_state_object(
      &SemanticStateWriteV1 {
        required_capabilities: [0; 32],
        availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
      },
      algorithm,
    )
    .unwrap(),
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(algorithm, &[b"typed test closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn captured_retirement_segment(database_id: [u8; 16]) -> CapturedRetirementSegmentV1 {
  captured_retirement_segment_with_timestamp(database_id, None)
}

fn captured_retirement_segment_with_timestamp(database_id: [u8; 16], retired_at_ms: Option<u64>) -> CapturedRetirementSegmentV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let fixture_path =
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-retirement-journal-segment-valid.bin");
  let fixture = std::fs::read(fixture_path).unwrap();
  let decoded = decode_retirement_journal_segment_v1(&fixture, algorithm).unwrap();
  let record = retirement_journal_records_v1(&decoded, algorithm).unwrap().next().unwrap().unwrap();
  let physical_length = 24 + 2 * algorithm.hash_length();
  let old_start = 24;
  let replacement_start = old_start + physical_length;
  let replacement_end = replacement_start + physical_length;
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 64 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap());
  let mut owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let mut sink = CapturingRetirementSinkV1::default();
  owner
    .append(
      RetirementJournalRecordWriteV1 {
        reason: record.reason,
        replacement_publication_sequence: record.replacement_publication_sequence,
        retired_at_ms: retired_at_ms.unwrap_or(record.retired_at_ms),
        old_incarnation: &record.encoded[old_start..replacement_start],
        replacement_incarnation: &record.encoded[replacement_start..replacement_end],
      },
      1,
      &mut sink,
    )
    .unwrap();
  sink.captured.unwrap()
}

fn publish_first_authority(publisher: &V4FirstAuthorityPublisher) {
  publisher.publish(&request()).unwrap();
}

struct PreparedGuardedRootRetirementV1 {
  target_root_hash: Vec<u8>,
  prior_lifecycle_manifest_key: Vec<u8>,
  intent: RootRetirementIntentV1,
  support_closure: RootLifecycleSupportClosureV1,
  retirement_commit: EncodedImmutableGcArtifactV1,
  expiry_record: Vec<u8>,
  expiry_manifest: EncodedImmutableGcArtifactV1,
  lifecycle_manifest: EncodedImmutableGcArtifactV1,
  lifecycle_control: EncodedGcActiveControlV1,
  pin_coordinator: RootReadPinCoordinatorV1,
}

impl PreparedGuardedRootRetirementV1 {
  fn request<'a>(&'a self, cancellation: &'a CancellationToken) -> RootRetirementPublicationRequestV1<'a> {
    RootRetirementPublicationRequestV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      intent: &self.intent,
      support_closure: &self.support_closure,
      retirement_commit: &self.retirement_commit,
      expiry_manifest: &self.expiry_manifest,
      lifecycle_manifest: &self.lifecycle_manifest,
      lifecycle_control: &self.lifecycle_control,
      publication_timestamp_ms: 1_700_000_100_001,
      monotonic_now_ms: 1_700_000_100_001,
      cancellation,
      pin_coordinator: &self.pin_coordinator,
    }
  }
}

struct ExactRootRetirementAuthorityVerifierV1 {
  called: bool,
  expected_root_hash: Vec<u8>,
  expected_authority_root_set_digest: Vec<u8>,
  returned_authority_root_set_digest: Option<Vec<u8>>,
  target_is_authoritative: bool,
}

struct BlockingRootRetirementAuthorityVerifierV1 {
  entered: Arc<Barrier>,
  release: Arc<Barrier>,
  expected_root_hash: Vec<u8>,
  expected_authority_root_set_digest: Vec<u8>,
}

struct CleanupFailingRootRetirementAuthorityVerifierV1 {
  pin_coordinator: RootReadPinCoordinatorV1,
  expected_authority_root_set_digest: Vec<u8>,
}

impl RootRetirementAuthorityVerifierV1 for CleanupFailingRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    assert_eq!(request.expected_authority_root_set_digest, self.expected_authority_root_set_digest);
    self.pin_coordinator.fail_next_cleanup_for_test();
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: false,
      authority_root_set_digest: self.expected_authority_root_set_digest.clone(),
    })
  }
}

impl RootRetirementAuthorityVerifierV1 for BlockingRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    assert_eq!(request.namespace_root_hash, self.expected_root_hash);
    assert_eq!(request.expected_authority_root_set_digest, self.expected_authority_root_set_digest);
    self.entered.wait();
    self.release.wait();
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: false,
      authority_root_set_digest: self.expected_authority_root_set_digest.clone(),
    })
  }
}

impl RootRetirementAuthorityVerifierV1 for ExactRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    self.called = true;
    assert_eq!(request.hash_algorithm, HashAlgorithm::Blake3_256);
    assert_eq!(request.database_id, [0x31; 16]);
    assert_eq!(request.namespace_root_hash, self.expected_root_hash);
    assert_eq!(request.expected_authority_root_set_digest, self.expected_authority_root_set_digest);
    assert_eq!(request.final_mark_generation, 5);
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: self.target_is_authoritative,
      authority_root_set_digest: self
        .returned_authority_root_set_digest
        .clone()
        .unwrap_or_else(|| self.expected_authority_root_set_digest.clone()),
    })
  }
}

struct FailingRootRetirementAuthorityVerifierV1 {
  called: bool,
}

impl RootRetirementAuthorityVerifierV1 for FailingRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    _request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    self.called = true;
    Err(RootRetirementAuthorityRecheckErrorV1::new("root_authority_source_unavailable", "injected caller-owned authority source failure"))
  }
}

fn publish_empty_lifecycle_authority(
  publisher: &V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  slot: u8,
  sequence: u64,
  generation: u64,
  timestamp_ms: u64,
) -> EncodedImmutableGcArtifactV1 {
  publish_empty_lifecycle_authority_for_database(publisher, retirement_owner, slot, sequence, generation, timestamp_ms, [0x31; 16])
}

fn publish_empty_lifecycle_authority_for_database(
  publisher: &V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  slot: u8,
  sequence: u64,
  generation: u64,
  timestamp_ms: u64,
  database_id: [u8; 16],
) -> EncodedImmutableGcArtifactV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let authority_root_set_digest = digest_parts(algorithm, &[b"prior complete authority roots", &generation.to_le_bytes()]);
  let manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation,
    published_at_ms: i64::try_from(timestamp_ms).unwrap(),
    source_complete_mark_generation: generation,
    authority_root_set_digest: &authority_root_set_digest,
    candidate_directory_hash: None,
    root_expiry_manifest_hash: None,
    next_page_id: 1,
    candidate_count: 0,
    pending_count: 0,
    retired_evidence_count: 0,
    candidate_bytes: 0,
    expiry_bytes: 0,
  })
  .unwrap();
  publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootLifecycleManifest,
        database_id: &database_id,
        artifact_key: &manifest.key,
        value: &manifest.value,
        minimum_timestamp_ms: timestamp_ms,
        committed_postcondition_code: "root_lifecycle_manifest_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::RootLifecycleActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot,
    sequence,
    generation,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let outcome = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
        encoded_control: &control,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let GcControlPublicationOutcomeV1::Complete(publication) = outcome else {
    panic!("prior lifecycle control unexpectedly reported a committed failure");
  };
  assert_eq!(publication.control_slot, slot);
  assert!(!publication.idempotent);
  manifest
}

fn prepare_guarded_root_retirement(
  publisher: &mut V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  cancellation: &CancellationToken,
  memory: &Arc<MemoryCoordinator>,
  publish_support: bool,
) -> PreparedGuardedRootRetirementV1 {
  prepare_guarded_root_retirement_for_database(publisher, retirement_owner, cancellation, memory, publish_support, [0x31; 16])
}

fn prepare_guarded_root_retirement_for_database(
  publisher: &mut V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  cancellation: &CancellationToken,
  memory: &Arc<MemoryCoordinator>,
  publish_support: bool,
  database_id: [u8; 16],
) -> PreparedGuardedRootRetirementV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let first_authority = publisher.publish(&request_for_database(database_id)).unwrap();
  let target_root_hash = first_authority.namespace_root.root_hash;
  let admission_commit_payload_hash = digest_parts(algorithm, &[&first_authority.admission_control]);
  publish_empty_lifecycle_authority_for_database(publisher, retirement_owner, 0, 1, 3, 1_700_000_050_000, database_id);
  let prior_lifecycle_manifest =
    publish_empty_lifecycle_authority_for_database(publisher, retirement_owner, 1, 2, 4, 1_700_000_060_000, database_id);
  assert_eq!(retirement_owner.status().pending_records, 0);

  let authority_root_set_digest = digest_parts(algorithm, &[b"complete authority roots after final mark"]);
  let committed_at_ms = 1_700_000_100_000i64;
  let retirement_commit = encode_root_retirement_commit_v1(&RootRetirementCommitWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    namespace_root_hash: &target_root_hash,
    retirement_id: &[0x81; 16],
    committed_at_ms,
    pending_since_ms: committed_at_ms - 86_400_000,
    grace_at_pending_ms: 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    prior_lifecycle_manifest_hash: &prior_lifecycle_manifest.key,
    authority_root_set_digest: &authority_root_set_digest,
    admission_commit_payload_hash: &admission_commit_payload_hash,
  })
  .unwrap();
  let expiry_record = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: &target_root_hash,
    retired_at_ms: committed_at_ms,
    last_pending_since_ms: committed_at_ms - 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    state: RootExpiryStateV1::LogicallyRetired,
    retirement_commit_hash: &retirement_commit.key,
    root_object_reclaim_proof_hash: None,
    evidence_expires_at_ms: None,
  })
  .unwrap();
  let expiry_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &database_id,
    catalog_id: &[0x71; 16],
    generation: 6,
    page_id: 1,
    records: &[&expiry_record],
  })
  .unwrap();
  let GcStateArtifactV1::Page(decoded_page) = decode_gc_state_artifact(&expiry_page.value, algorithm).unwrap() else {
    unreachable!();
  };
  let directory_entries = [GcStateDirectoryEntryWriteV1 {
    lower_fence: decoded_page.lower_fence,
    upper_fence: decoded_page.upper_fence,
    child_hash: &expiry_page.key,
    child_generation: decoded_page.generation,
    live_count: u64::from(decoded_page.record_count),
    tombstone_count: 0,
    page_count: 1,
    logical_bytes: decoded_page.logical_bytes,
    minimum_page_id: decoded_page.page_id,
    maximum_page_id: decoded_page.page_id,
    physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }];
  let expiry_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &database_id,
    catalog_id: decoded_page.catalog_id,
    generation: 6,
    level: 0,
    entries: &directory_entries,
  })
  .unwrap();
  if publish_support {
    for artifact in [&expiry_page, &expiry_directory] {
      publisher
        .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
          database_id: &database_id,
          artifact,
          publication_timestamp_ms: 1_700_000_100_001,
        })
        .unwrap();
    }
  }

  let logical_bytes = u64::try_from(expiry_record.len()).unwrap();
  let expiry_manifest = encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: 6,
    retention_ms: 30 * 24 * 60 * 60 * 1_000,
    optional_byte_budget: 256 * 1024 * 1024,
    directory_root_hash: Some(&expiry_directory.key),
    next_page_id: 2,
    record_count: 1,
    logical_bytes,
    mandatory_count: 1,
    mandatory_bytes: logical_bytes,
    optional_count: 0,
    optional_bytes: 0,
    oldest_retired_at_ms: Some(committed_at_ms),
    newest_retired_at_ms: Some(committed_at_ms),
  })
  .unwrap();
  let lifecycle_manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: 6,
    published_at_ms: committed_at_ms + 1,
    source_complete_mark_generation: 5,
    authority_root_set_digest: &authority_root_set_digest,
    candidate_directory_hash: None,
    root_expiry_manifest_hash: Some(&expiry_manifest.key),
    next_page_id: 1,
    candidate_count: 0,
    pending_count: 0,
    retired_evidence_count: 1,
    candidate_bytes: 0,
    expiry_bytes: logical_bytes,
  })
  .unwrap();
  let retirement = decode_root_retirement_commit_v1(&retirement_commit.value, algorithm).unwrap();
  let expiry = decode_root_expiry_manifest_v1(&expiry_manifest.value, algorithm).unwrap();
  let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_manifest.value, algorithm).unwrap();
  let mut closure_builder = RootLifecycleSupportClosureBuilderV1::new_for_retirement(
    &lifecycle,
    &expiry,
    &retirement,
    algorithm,
    cancellation,
    RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
    memory,
  )
  .unwrap();
  closure_builder.observe_encoded(&expiry_page.value).unwrap();
  closure_builder.observe_encoded(&expiry_directory.value).unwrap();
  let support_closure = closure_builder.finish().unwrap();
  let lifecycle_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::RootLifecycleActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 0,
    sequence: 3,
    generation: 6,
    target_manifest_hash: &lifecycle_manifest.key,
  })
  .unwrap();
  let intent = RootRetirementIntentV1 {
    namespace_root_hash: target_root_hash.clone(),
    committed_at_ms,
    pending_since_ms: committed_at_ms - 86_400_000,
    grace_at_pending_ms: 86_400_000,
    final_mark_generation: 5,
    reason: 1,
    prior_lifecycle_manifest_hash: prior_lifecycle_manifest.key.clone(),
    authority_root_set_digest,
    admission_commit_payload_hash,
  };

  let observation = publisher.observe().unwrap();
  let mut successor = observation.selected.header.clone();
  successor.updated_at_ms += 1;
  successor.head_hash = digest_parts(algorithm, &[b"new current namespace root"]);
  publisher.header_publisher.publish_inactive_slot(&publisher.file, &observation, successor).unwrap();
  let pin_coordinator = RootReadPinCoordinatorV1::new(memory.clone(), algorithm, 16, 16).unwrap();

  PreparedGuardedRootRetirementV1 {
    target_root_hash,
    prior_lifecycle_manifest_key: prior_lifecycle_manifest.key,
    intent,
    support_closure,
    retirement_commit,
    expiry_record,
    expiry_manifest,
    lifecycle_manifest,
    lifecycle_control,
    pin_coordinator,
  }
}

struct ExactRootReclaimEvidenceVerifierV1 {
  expected_database_id: [u8; 16],
  expected_root_hash: Vec<u8>,
  called: bool,
}

impl RootObjectReclaimEvidenceVerifierV1 for ExactRootReclaimEvidenceVerifierV1 {
  fn verify_root_object_reclaim(
    &mut self,
    request: RootObjectReclaimEvidenceVerificationRequestV1<'_>,
  ) -> Result<(), RootObjectReclaimEvidenceVerificationErrorV1> {
    self.called = true;
    assert_eq!(request.database_id, self.expected_database_id);
    assert_eq!(request.namespace_root_hash, self.expected_root_hash);
    assert!(request.final_physical_inventory_generation > request.latest_sweep_receipt_generation);
    assert_eq!(request.root_object_incarnation_count, 2);
    assert_eq!(request.sweep_receipt_count, 3);
    Ok(())
  }
}

struct DatabaseRootRetirementAuthorityVerifierV1 {
  expected_database_id: [u8; 16],
  expected_root_hash: Vec<u8>,
  expected_authority_root_set_digest: Vec<u8>,
}

impl RootRetirementAuthorityVerifierV1 for DatabaseRootRetirementAuthorityVerifierV1 {
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1> {
    assert_eq!(request.database_id, self.expected_database_id);
    assert_eq!(request.namespace_root_hash, self.expected_root_hash);
    assert_eq!(request.expected_authority_root_set_digest, self.expected_authority_root_set_digest);
    Ok(RootRetirementAuthoritySnapshotV1 {
      target_is_authoritative: false,
      authority_root_set_digest: self.expected_authority_root_set_digest.clone(),
    })
  }
}

struct PreparedGuardedRootReclaimV1 {
  retention_permit: RootExpiryRetentionPermitV1,
  support_closure: RootLifecycleSupportClosureV1,
  root_object_reclaim_proof: EncodedImmutableGcArtifactV1,
  expiry_page: EncodedImmutableGcArtifactV1,
  expiry_directory: EncodedImmutableGcArtifactV1,
  expiry_manifest: EncodedImmutableGcArtifactV1,
  lifecycle_manifest: EncodedImmutableGcArtifactV1,
  lifecycle_control: EncodedGcActiveControlV1,
  publication_timestamp_ms: u64,
}

impl PreparedGuardedRootReclaimV1 {
  fn request<'a>(
    &'a self,
    cancellation: &'a CancellationToken,
    pin_coordinator: &'a RootReadPinCoordinatorV1,
  ) -> RootReclaimPublicationRequestV1<'a> {
    RootReclaimPublicationRequestV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      retention_permit: &self.retention_permit,
      support_closure: &self.support_closure,
      root_object_reclaim_proof: &self.root_object_reclaim_proof,
      expiry_manifest: &self.expiry_manifest,
      lifecycle_manifest: &self.lifecycle_manifest,
      lifecycle_control: &self.lifecycle_control,
      publication_timestamp_ms: self.publication_timestamp_ms,
      monotonic_now_ms: self.publication_timestamp_ms,
      cancellation,
      pin_coordinator,
    }
  }
}

fn prepare_guarded_root_reclaim(
  publisher: &V4FirstAuthorityPublisher,
  retirement: &PreparedGuardedRootRetirementV1,
  database_id: [u8; 16],
  physical_inventory_bytes: &[u8],
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> PreparedGuardedRootReclaimV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let physical_inventory = decode_physical_inventory_manifest_v1(physical_inventory_bytes, algorithm).unwrap();
  assert_eq!(physical_inventory.database_id, database_id);
  let retirement_commit = decode_root_retirement_commit_v1(&retirement.retirement_commit.value, algorithm).unwrap();
  let prior_expiry_record = decode_root_expiry_record_v1(&retirement.expiry_record, algorithm).unwrap();
  let prior_expiry_manifest = decode_root_expiry_manifest_v1(&retirement.expiry_manifest.value, algorithm).unwrap();
  let prior_lifecycle_manifest = decode_root_lifecycle_manifest_v1(&retirement.lifecycle_manifest.value, algorithm).unwrap();
  let inventory_completed_at_ms = i64::try_from(physical_inventory.completed_at_ms).unwrap();
  let reclaimed_at_ms = inventory_completed_at_ms.max(retirement_commit.committed_at_ms) + 1_000;
  let retention_ms = prior_expiry_manifest.retention_ms;
  let root_object_incarnation_digest = digest_parts(algorithm, &[b"qualified root-object incarnations"]);
  let sweep_receipt_merkle_root = digest_parts(algorithm, &[b"qualified sweep receipts"]);
  let absence_digest = digest_parts(algorithm, &[b"qualified final root-object absence"]);
  let mut evidence_verifier = ExactRootReclaimEvidenceVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    called: false,
  };
  let qualified = qualify_root_object_reclaim_v1(
    RootObjectReclaimQualificationRequestV1 {
      hash_algorithm: algorithm,
      prior_expiry: &prior_expiry_record,
      retirement: &retirement_commit,
      final_physical_inventory: &physical_inventory,
      proof_id: &[0xa1; 16],
      reclaimed_at_ms,
      latest_sweep_receipt_generation: physical_inventory.generation - 1,
      root_object_incarnation_digest: &root_object_incarnation_digest,
      root_object_incarnation_count: 2,
      sweep_receipt_merkle_root: &sweep_receipt_merkle_root,
      sweep_receipt_count: 3,
      absence_digest: &absence_digest,
      retention_ms,
    },
    cancellation,
    &mut evidence_verifier,
  )
  .unwrap();
  assert!(evidence_verifier.called);
  let completed_at_ms = reclaimed_at_ms + 1;
  let mut retention_model = RootExpiryRetentionModelV1::new(
    RootExpiryRetentionContextV1 {
      hash_algorithm: algorithm,
      prior_lifecycle: &prior_lifecycle_manifest,
      prior_expiry: &prior_expiry_manifest,
      lifecycle_generation: prior_lifecycle_manifest.generation + 1,
      completed_at_ms,
      retention_ms,
      optional_byte_budget: prior_expiry_manifest.optional_byte_budget,
      maximum_records: 1,
      selection: RootExpiryRetentionSelectionV1::KeepAll,
      qualified_reclaim: &qualified,
    },
    cancellation,
  )
  .unwrap();
  retention_model.observe(&prior_expiry_record).unwrap();
  let retention_permit = retention_model.finish().unwrap();
  let root_object_reclaim_proof = qualified.encoded_proof().clone();
  let expiry_record = qualified.encoded_expiry_record().to_vec();
  let catalog_id = [0x72; 16];
  let generation = retention_permit.lifecycle_generation();
  let expiry_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &database_id,
    catalog_id: &catalog_id,
    generation,
    page_id: 2,
    records: &[&expiry_record],
  })
  .unwrap();
  let GcStateArtifactV1::Page(decoded_page) = decode_gc_state_artifact(&expiry_page.value, algorithm).unwrap() else {
    unreachable!();
  };
  let directory_entries = [GcStateDirectoryEntryWriteV1 {
    lower_fence: decoded_page.lower_fence,
    upper_fence: decoded_page.upper_fence,
    child_hash: &expiry_page.key,
    child_generation: decoded_page.generation,
    live_count: u64::from(decoded_page.record_count),
    tombstone_count: 0,
    page_count: 1,
    logical_bytes: decoded_page.logical_bytes,
    minimum_page_id: decoded_page.page_id,
    maximum_page_id: decoded_page.page_id,
    physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }];
  let expiry_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &database_id,
    catalog_id: &catalog_id,
    generation,
    level: 0,
    entries: &directory_entries,
  })
  .unwrap();
  for artifact in [&expiry_page, &expiry_directory] {
    publisher
      .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
        database_id: &database_id,
        artifact,
        publication_timestamp_ms: u64::try_from(completed_at_ms).unwrap(),
      })
      .unwrap();
  }
  let summary = retention_permit.summary();
  let expiry_manifest = encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation,
    retention_ms: retention_permit.retention_ms(),
    optional_byte_budget: retention_permit.optional_byte_budget(),
    directory_root_hash: Some(&expiry_directory.key),
    next_page_id: 3,
    record_count: summary.resulting_count,
    logical_bytes: summary.resulting_bytes,
    mandatory_count: summary.resulting_mandatory_count,
    mandatory_bytes: summary.resulting_mandatory_bytes,
    optional_count: summary.resulting_optional_count,
    optional_bytes: summary.resulting_optional_bytes,
    oldest_retired_at_ms: summary.oldest_retired_at_ms,
    newest_retired_at_ms: summary.newest_retired_at_ms,
  })
  .unwrap();
  let lifecycle_manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation,
    published_at_ms: completed_at_ms,
    source_complete_mark_generation: prior_lifecycle_manifest.source_complete_mark_generation,
    authority_root_set_digest: prior_lifecycle_manifest.authority_root_set_digest,
    candidate_directory_hash: prior_lifecycle_manifest.candidate_directory_hash,
    root_expiry_manifest_hash: Some(&expiry_manifest.key),
    next_page_id: prior_lifecycle_manifest.next_page_id,
    candidate_count: prior_lifecycle_manifest.candidate_count,
    pending_count: prior_lifecycle_manifest.pending_count,
    retired_evidence_count: summary.resulting_count,
    candidate_bytes: prior_lifecycle_manifest.candidate_bytes,
    expiry_bytes: summary.resulting_bytes,
  })
  .unwrap();
  let proof = decode_root_object_reclaim_proof_v1(&root_object_reclaim_proof.value, algorithm).unwrap();
  let expiry = decode_root_expiry_manifest_v1(&expiry_manifest.value, algorithm).unwrap();
  let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_manifest.value, algorithm).unwrap();
  let mut closure_builder = RootLifecycleSupportClosureBuilderV1::new_for_reclaim(
    &lifecycle,
    &expiry,
    &proof,
    algorithm,
    cancellation,
    RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
    memory,
  )
  .unwrap();
  closure_builder.observe_encoded(&expiry_page.value).unwrap();
  closure_builder.observe_encoded(&expiry_directory.value).unwrap();
  let support_closure = closure_builder.finish().unwrap();
  let lifecycle_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::RootLifecycleActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 1,
    sequence: 4,
    generation,
    target_manifest_hash: &lifecycle_manifest.key,
  })
  .unwrap();
  PreparedGuardedRootReclaimV1 {
    retention_permit,
    support_closure,
    root_object_reclaim_proof,
    expiry_page,
    expiry_directory,
    expiry_manifest,
    lifecycle_manifest,
    lifecycle_control,
    publication_timestamp_ms: u64::try_from(completed_at_ms).unwrap(),
  }
}

fn substitute_guarded_root_reclaim_expiry_row(
  publisher: &V4FirstAuthorityPublisher,
  reclaim: &mut PreparedGuardedRootReclaimV1,
  database_id: [u8; 16],
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) {
  let algorithm = HashAlgorithm::Blake3_256;
  let GcStateArtifactV1::Page(prior_page) = decode_gc_state_artifact(&reclaim.expiry_page.value, algorithm).unwrap() else {
    unreachable!();
  };
  let prior_record = decode_root_expiry_record_v1(prior_page.records, algorithm).unwrap();
  let substituted_record = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: prior_record.namespace_root_hash,
    retired_at_ms: prior_record.retired_at_ms,
    last_pending_since_ms: prior_record.last_pending_since_ms,
    final_mark_generation: prior_record.final_mark_generation,
    reason: prior_record.reason + 1,
    state: prior_record.state,
    retirement_commit_hash: prior_record.retirement_commit_hash,
    root_object_reclaim_proof_hash: prior_record.root_object_reclaim_proof_hash,
    evidence_expires_at_ms: prior_record.evidence_expires_at_ms,
  })
  .unwrap();
  let expiry_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: prior_page.role,
    database_id: &database_id,
    catalog_id: prior_page.catalog_id,
    generation: prior_page.generation,
    page_id: prior_page.page_id,
    records: &[&substituted_record],
  })
  .unwrap();
  let GcStateArtifactV1::Page(decoded_page) = decode_gc_state_artifact(&expiry_page.value, algorithm).unwrap() else {
    unreachable!();
  };
  let directory_entries = [GcStateDirectoryEntryWriteV1 {
    lower_fence: decoded_page.lower_fence,
    upper_fence: decoded_page.upper_fence,
    child_hash: &expiry_page.key,
    child_generation: decoded_page.generation,
    live_count: u64::from(decoded_page.record_count),
    tombstone_count: 0,
    page_count: 1,
    logical_bytes: decoded_page.logical_bytes,
    minimum_page_id: decoded_page.page_id,
    maximum_page_id: decoded_page.page_id,
    physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }];
  let expiry_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id: &database_id,
    catalog_id: prior_page.catalog_id,
    generation: prior_page.generation,
    level: 0,
    entries: &directory_entries,
  })
  .unwrap();
  for artifact in [&expiry_page, &expiry_directory] {
    publisher
      .publish_root_lifecycle_support_artifact(RootLifecycleSupportPublicationRequestV1 {
        database_id: &database_id,
        artifact,
        publication_timestamp_ms: reclaim.publication_timestamp_ms,
      })
      .unwrap();
  }

  let prior_expiry = decode_root_expiry_manifest_v1(&reclaim.expiry_manifest.value, algorithm).unwrap();
  let expiry_manifest = encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: prior_expiry.generation,
    retention_ms: prior_expiry.retention_ms,
    optional_byte_budget: prior_expiry.optional_byte_budget,
    directory_root_hash: Some(&expiry_directory.key),
    next_page_id: prior_expiry.next_page_id,
    record_count: prior_expiry.record_count,
    logical_bytes: prior_expiry.logical_bytes,
    mandatory_count: prior_expiry.mandatory_count,
    mandatory_bytes: prior_expiry.mandatory_bytes,
    optional_count: prior_expiry.optional_count,
    optional_bytes: prior_expiry.optional_bytes,
    oldest_retired_at_ms: prior_expiry.oldest_retired_at_ms,
    newest_retired_at_ms: prior_expiry.newest_retired_at_ms,
  })
  .unwrap();
  let prior_lifecycle = decode_root_lifecycle_manifest_v1(&reclaim.lifecycle_manifest.value, algorithm).unwrap();
  let lifecycle_manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    generation: prior_lifecycle.generation,
    published_at_ms: prior_lifecycle.published_at_ms,
    source_complete_mark_generation: prior_lifecycle.source_complete_mark_generation,
    authority_root_set_digest: prior_lifecycle.authority_root_set_digest,
    candidate_directory_hash: prior_lifecycle.candidate_directory_hash,
    root_expiry_manifest_hash: Some(&expiry_manifest.key),
    next_page_id: prior_lifecycle.next_page_id,
    candidate_count: prior_lifecycle.candidate_count,
    pending_count: prior_lifecycle.pending_count,
    retired_evidence_count: prior_lifecycle.retired_evidence_count,
    candidate_bytes: prior_lifecycle.candidate_bytes,
    expiry_bytes: prior_lifecycle.expiry_bytes,
  })
  .unwrap();
  let proof = decode_root_object_reclaim_proof_v1(&reclaim.root_object_reclaim_proof.value, algorithm).unwrap();
  let expiry = decode_root_expiry_manifest_v1(&expiry_manifest.value, algorithm).unwrap();
  let lifecycle = decode_root_lifecycle_manifest_v1(&lifecycle_manifest.value, algorithm).unwrap();
  let mut closure_builder = RootLifecycleSupportClosureBuilderV1::new_for_reclaim(
    &lifecycle,
    &expiry,
    &proof,
    algorithm,
    cancellation,
    RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
    memory,
  )
  .unwrap();
  closure_builder.observe_encoded(&expiry_page.value).unwrap();
  closure_builder.observe_encoded(&expiry_directory.value).unwrap();
  let support_closure = closure_builder.finish().unwrap();
  let prior_control = decode_gc_active_control(&reclaim.lifecycle_control.value, algorithm).unwrap();
  let lifecycle_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: prior_control.kind,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: prior_control.slot,
    sequence: prior_control.sequence,
    generation: prior_control.generation,
    target_manifest_hash: &lifecycle_manifest.key,
  })
  .unwrap();

  reclaim.support_closure = support_closure;
  reclaim.expiry_page = expiry_page;
  reclaim.expiry_directory = expiry_directory;
  reclaim.expiry_manifest = expiry_manifest;
  reclaim.lifecycle_manifest = lifecycle_manifest;
  reclaim.lifecycle_control = lifecycle_control;
}

fn selected_root_lifecycle_manifest_key(publisher: &V4FirstAuthorityPublisher) -> Vec<u8> {
  let observation = publisher.observe().unwrap();
  let kv = publisher.kv.lock().unwrap();
  select_root_lifecycle_control(&publisher.file, &kv, &observation.selected.header).unwrap().target_manifest_hash
}

fn selected_physical_quarantine_manifest_key(publisher: &V4FirstAuthorityPublisher) -> Vec<u8> {
  let observation = publisher.observe().unwrap();
  let kv = publisher.kv.lock().unwrap();
  select_physical_quarantine_control(&publisher.file, &kv, &observation.selected.header).unwrap().target_manifest_hash
}

fn corrupt_last_entity_byte(publisher: &V4FirstAuthorityPublisher, key: &[u8]) {
  let locator = publisher.locator(key).unwrap().expect("corruption target must be durably published");
  let offset = locator.offset + u64::from(locator.total_length) - 1;
  let mut file = publisher.file.try_clone().unwrap();
  file.seek(SeekFrom::Start(offset)).unwrap();
  let mut byte = [0u8; 1];
  file.read_exact(&mut byte).unwrap();
  byte[0] ^= 0x80;
  file.seek(SeekFrom::Start(offset)).unwrap();
  file.write_all(&byte).unwrap();
  file.sync_all().unwrap();
}

struct PreparedMarkCheckpointV1 {
  closure: DurableMarkWorkspaceClosureV1,
  checkpoint: EncodedImmutableGcArtifactV1,
  control: EncodedGcActiveControlV1,
}

fn prepare_mark_checkpoint(
  database_path: &Path,
  scratch_root: &Path,
  memory: &MemoryCoordinator,
  run_byte: u8,
  generation: u64,
  checkpoint_sequence: u64,
) -> PreparedMarkCheckpointV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let run_id = [run_byte; 16];
  let identity = MarkWorkspaceIdentityV1::new(database_id, run_id, generation, checkpoint_sequence, algorithm).unwrap();
  let basis = MarkWorkspaceBasisV1::new(
    1,
    1_700_000_100_000 + checkpoint_sequence,
    1_700_000_100_500 + checkpoint_sequence,
    vec![0x51; algorithm.hash_length()],
    vec![0x11; algorithm.hash_length()],
    [0x71; 32],
  )
  .unwrap();
  let mut workspace = DurableMarkWorkspaceV1::create(
    database_path,
    identity,
    basis,
    MarkWorkspaceOptionsV1::new(Some(scratch_root.to_path_buf()), 64 * 1024 * 1024, 0).unwrap(),
    CancellationToken::new(),
    memory,
  )
  .unwrap();
  let closure = workspace.complete().unwrap();
  let mut capabilities = [0u8; 32];
  for bit in [12usize, 13, 14, 15, 17] {
    capabilities[bit / 8] |= 1 << (bit % 8);
  }
  let checkpoint = encode_mark_run_checkpoint(&MarkRunCheckpointWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation,
    checkpoint_sequence,
    state: 1,
    phase: 1,
    resumable: true,
    canceled: false,
    capabilities,
    started_at_ms: 1_700_000_100_000 + checkpoint_sequence,
    updated_at_ms: 1_700_000_100_500 + checkpoint_sequence,
    authority_root_set_digest: &[0x11; 32],
    semantic_state_digest: &[0x31; 32],
    kv_layout_fingerprint: &[0x51; 32],
    effective_policy_fingerprint: [0x71; 32],
    system_family_registry_fingerprint: [0x91; 32],
    captured_header_sequence: 17,
    captured_write_high_water: 900,
    reconciled_through_sequence: 801,
    active_bitmap_bit_count: 512,
    kv_bucket_count: 8,
    kv_slots_per_bucket: 64,
    workspace_path: &closure.checkpoint_workspace_path().unwrap(),
    workspace_id: [run_byte.wrapping_add(0x20); 16],
    workspace_manifest_digest: closure.manifest_digest(),
    mutation_journal_head: &[0xB1; 32],
    checkpoint_logical_work: checkpoint_sequence * 1024,
    total_logical_work_hint: 64 * 1024 * 1024,
  })
  .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::MarkRunActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: u8::try_from((checkpoint_sequence - 1) % 2).unwrap(),
    sequence: checkpoint_sequence,
    generation,
    target_manifest_hash: &checkpoint.key,
  })
  .unwrap();
  PreparedMarkCheckpointV1 { closure, checkpoint, control }
}

fn publish_mark_checkpoint(
  publisher: &mut V4FirstAuthorityPublisher,
  owner: &mut RetirementJournalOwnerV1<'_>,
  prepared: &PreparedMarkCheckpointV1,
  timestamp_ms: u64,
) -> MarkRunCheckpointPublicationReceiptV1 {
  publisher
    .publish_mark_run_checkpoint(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &prepared.checkpoint,
        control: &prepared.control,
        workspace: &prepared.closure,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      owner,
    )
    .unwrap()
}

fn write_redundant_header(publisher: &V4FirstAuthorityPublisher, header: &DatabaseHeaderV4) {
  let encoded = encode_database_header_slot(header).unwrap();
  write_file_at_native(&publisher.file, 0, &encoded).unwrap();
  write_file_at_native(&publisher.file, DATABASE_HEADER_V4_SLOT_LENGTH as u64, &encoded).unwrap();
  sync_file_all_native(&publisher.file).unwrap();
}

#[test]
fn staged_first_authority_entities_are_absent_from_every_published_snapshot() {
  let (_directory, _coordinator, publisher) = environment("hidden");
  let mut observer = VisibilityObserver { called: false };

  let receipt = publisher.publish_with_observer(&request(), &mut observer).unwrap();

  assert!(observer.called);
  assert!(publisher.locator(&receipt.namespace_root.root_hash).unwrap().is_some());
}

#[test]
fn failure_after_dependency_staging_restores_the_old_view_and_hot_tail_frontier() {
  let (_directory, coordinator, publisher) = environment("abort");
  let request = request();
  let before = publisher.observe().unwrap();
  let old_hot_tail_offset = publisher.kv.lock().unwrap().hot_tail_offset();
  let root =
    prepare_namespace_root(&request, before.selected.header.hash_algorithm, before.selected.header.write_sequence_high_water).unwrap();
  let mut observer = FailingVisibilityObserver;

  let error = publisher.publish_with_observer(&request, &mut observer).unwrap_err();

  assert_eq!(error.code(), "durability_failure");
  assert_eq!(publisher.observe().unwrap(), before);
  assert!(publisher.locator(&root.root_hash).unwrap().is_none());
  let kv = publisher.kv.lock().unwrap();
  assert_eq!(kv.hot_tail_offset(), old_hot_tail_offset);
  assert_eq!(kv.write_buffer_len(), 0);
  assert_eq!(kv.hot_buffer_len(), 0);
  assert!(coordinator.hard_failure().unwrap().is_some());
}

#[test]
fn post_commit_failure_returns_the_exact_committed_receipt_and_retry_is_idempotent() {
  let (_directory, coordinator, publisher) = environment("post-commit-failure");
  let request = request();
  let mut observer = FailingPostCommitObserver;

  let error = publisher.publish_with_observer(&request, &mut observer).unwrap_err();

  assert_eq!(error.code(), "first_authority_committed_postcondition_failure");
  let committed = error.committed_receipt().expect("post-commit failure must retain the exact receipt");
  let committed_sequence = committed.publication_sequence;
  let committed_root = committed.namespace_root.root_hash.clone();
  assert!(!committed.idempotent);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, committed_sequence);

  let retry = publisher.publish(&request).unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry.publication_sequence, committed_sequence);
  assert_eq!(retry.namespace_root.root_hash, committed_root);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, committed_sequence + 1);
}

#[test]
fn root_lifecycle_and_mark_controls_share_one_kind_scoped_replacement_path() {
  let (_directory, _path, _coordinator, publisher) = create_environment("shared-gc-control", None);
  publish_first_authority(&publisher);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let database_id = [0x31; 16];
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(8, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();

  let mut last_control = None;
  for sequence in 1..=3u64 {
    let generation = 500 + sequence;
    let timestamp_ms = 1_700_000_500_000 + sequence;
    let manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
      hash_algorithm: HashAlgorithm::Blake3_256,
      database_id: &database_id,
      generation,
      published_at_ms: i64::try_from(timestamp_ms).unwrap(),
      source_complete_mark_generation: generation,
      authority_root_set_digest: &[0x41; 32],
      candidate_directory_hash: None,
      root_expiry_manifest_hash: None,
      next_page_id: 1,
      candidate_count: 0,
      pending_count: 0,
      retired_evidence_count: 0,
      candidate_bytes: 0,
      expiry_bytes: 0,
    })
    .unwrap();
    publisher
      .publish_immutable_gc_artifact(
        ImmutableGcArtifactPublicationV1 {
          kind: GcArtifactKindV1::RootLifecycleManifest,
          database_id: &database_id,
          artifact_key: &manifest.key,
          value: &manifest.value,
          minimum_timestamp_ms: timestamp_ms,
          committed_postcondition_code: "root_lifecycle_manifest_committed_postcondition",
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap();
    let control = encode_gc_active_control(&GcActiveControlWriteV1 {
      kind: GcArtifactKindV1::RootLifecycleActiveControl,
      hash_algorithm: HashAlgorithm::Blake3_256,
      database_id: &database_id,
      slot: u8::try_from((sequence - 1) % 2).unwrap(),
      sequence,
      generation,
      target_manifest_hash: &manifest.key,
    })
    .unwrap();
    let outcome = publisher
      .publish_gc_active_control(
        GcControlPublicationRequestV1 {
          expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
          encoded_control: &control,
          publication_timestamp_ms: timestamp_ms,
          monotonic_now_ms: timestamp_ms,
        },
        &mut owner,
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap();
    let GcControlPublicationOutcomeV1::Complete(publication) = outcome else {
      panic!("lifecycle control publication unexpectedly reported a committed failure");
    };
    assert_eq!(publication.control_slot, u8::try_from((sequence - 1) % 2).unwrap());
    assert_eq!(publication.replaced_control, sequence == 3);
    assert!(!publication.idempotent);
    last_control = Some((control, timestamp_ms));
  }

  assert_eq!(owner.status().pending_records, 1);
  let mark_control_key = gc_active_control_key(HashAlgorithm::Blake3_256, GcArtifactKindV1::MarkRunActiveControl, &database_id, 0).unwrap();
  assert!(publisher.locator(&mark_control_key).unwrap().is_none());
  let (last_control, timestamp_ms) = last_control.unwrap();
  let retry = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
        encoded_control: &last_control,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      &mut owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let GcControlPublicationOutcomeV1::Complete(retry) = retry else {
    panic!("exact lifecycle control retry unexpectedly reported a committed failure");
  };
  assert!(retry.idempotent);
  assert_eq!(owner.status().pending_records, 1);

  let before_wrong_kind = publisher.observe().unwrap();
  let wrong_kind = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::MarkRunActiveControl,
        encoded_control: &last_control,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      &mut owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap_err();
  assert_eq!(wrong_kind.code(), "gc_control_kind");
  assert_eq!(publisher.observe().unwrap(), before_wrong_kind);
}

#[test]
fn guarded_root_retirement_selects_control_last_and_exact_retry_does_not_recheck_stale_authority() {
  let (_directory, path, coordinator, mut publisher) = create_environment("guarded-root-retirement", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let target_locator = publisher.locator(&prepared.target_root_hash).unwrap().unwrap();
  let target_admission_locator = publisher.admission_locator(&prepared.target_root_hash).unwrap().unwrap();
  let file_length_before = std::fs::metadata(&path).unwrap().len();
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let receipt = publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();

  assert!(authority_verifier.called);
  assert!(!receipt.idempotent);
  assert_eq!(receipt.lifecycle_control_slot, 0);
  assert!(receipt.retirement_commit_write_sequence < receipt.expiry_manifest_write_sequence);
  assert!(receipt.expiry_manifest_write_sequence < receipt.lifecycle_manifest_write_sequence);
  assert!(receipt.lifecycle_manifest_write_sequence < receipt.lifecycle_control_write_sequence);
  assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }));
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  assert_eq!(publisher.locator(&prepared.target_root_hash).unwrap().unwrap(), target_locator);
  assert_eq!(publisher.admission_locator(&prepared.target_root_hash).unwrap().unwrap(), target_admission_locator);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_some());
  assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_some());
  assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_some());
  assert!(publisher.locator(&prepared.lifecycle_control.key).unwrap().is_some());
  assert!(std::fs::metadata(&path).unwrap().len() >= file_length_before);
  assert_eq!(retirement_owner.status().pending_records, 0);
  assert_eq!(retirement_owner.status().durable_records, 1);

  let before_retry = publisher.observe().unwrap();
  let before_retry_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let before_retry_locators = [
    publisher.locator(&prepared.retirement_commit.key).unwrap().unwrap(),
    publisher.locator(&prepared.expiry_manifest.key).unwrap().unwrap(),
    publisher.locator(&prepared.lifecycle_manifest.key).unwrap().unwrap(),
    publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap(),
  ];
  authority_verifier.called = false;
  authority_verifier.target_is_authoritative = true;
  let retry = publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();

  assert!(retry.idempotent);
  assert!(!authority_verifier.called, "exact selected retry must not depend on stale caller authority");
  assert!(matches!(retry.lineage_state, RootRetirementLineageStateV1::NotRequired));
  assert_eq!(publisher.observe().unwrap(), before_retry);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_retry_frontier);
  assert_eq!(
    [
      publisher.locator(&prepared.retirement_commit.key).unwrap().unwrap(),
      publisher.locator(&prepared.expiry_manifest.key).unwrap().unwrap(),
      publisher.locator(&prepared.lifecycle_manifest.key).unwrap().unwrap(),
      publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap(),
    ],
    before_retry_locators,
  );
}

#[test]
fn every_sweep_receipt_publication_failure_restarts_to_absent_or_exact_receipt() {
  for failure in [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ] {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("sweep-receipt-crash-{failure:?}"), None);
    publish_first_authority(&publisher);
    let observation = publisher.observe().unwrap();
    let algorithm = observation.selected.header.hash_algorithm;
    let database_id = observation.selected.header.database_id;
    let batch_id = [0xB4; 16];
    let logical_key = digest_parts(algorithm, &[b"crash sweep logical key"]);
    let integrity = digest_parts(algorithm, &[b"crash sweep integrity"]);
    let quarantine_manifest_hash = digest_parts(algorithm, &[b"crash sweep quarantine"]);
    let candidates = [PhysicalIncarnationV1 {
      logical_key: &logical_key,
      integrity_or_legacy_digest: &integrity,
      wal_offset: 32_768,
      write_sequence: 901,
      entity_length: 2_048,
      entry_type: 1,
      entity_version: 1,
    }];
    let proposal = encode_sweep_proposal_v1(&SweepProposalWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: 311,
      created_at_ms: 1_700_000_099_000,
      quarantine_manifest_hash: &quarantine_manifest_hash,
      candidates: &candidates,
    })
    .unwrap();
    let proposal_write_sequence = publisher
      .publish_immutable_gc_artifact(
        ImmutableGcArtifactPublicationV1 {
          kind: GcArtifactKindV1::SweepProposal,
          database_id: &database_id,
          artifact_key: &proposal.key,
          value: &proposal.value,
          minimum_timestamp_ms: 1_700_000_099_000,
          committed_postcondition_code: "test_crash_sweep_proposal",
        },
        &mut NoopFirstAuthorityDependencyObserverV1,
      )
      .unwrap();
    let reclaimed = SweepLocatorRemovalOutcomeV1 {
      ordinal: 0,
      outcome: SweepOutcomeClassV1::Reclaimed,
      stable_reason_detail: 0,
      resulting_void_offset: candidates[0].wal_offset,
      resulting_void_length: candidates[0].entity_length,
    };
    let void_catalog_hash = digest_parts(algorithm, &[b"crash-selected Void catalog"]);
    let committed_at_ms = 1_700_000_100_000;
    let expected_receipt = encode_sweep_receipt_v1(&SweepReceiptWriteV1 {
      hash_algorithm: algorithm,
      recovered: true,
      database_id: &database_id,
      batch_id: &batch_id,
      generation: 311,
      reclaim_committed_at_ms: committed_at_ms,
      proposal_hash: &proposal.key,
      void_catalog_hash: &void_catalog_hash,
      outcomes: &[SweepReceiptOutcomeWriteV1 {
        incarnation: candidates[0],
        outcome: reclaimed.outcome,
        stable_reason_detail: reclaimed.stable_reason_detail,
        resulting_void_offset: reclaimed.resulting_void_offset,
        resulting_void_length: reclaimed.resulting_void_length,
      }],
    })
    .unwrap();
    publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(NthHeaderPublicationFaultIo::new(failure, 1))),
      root_state: publisher.root_state,
    };
    let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
    let cancellation = CancellationToken::new();
    let request = SweepReceiptReconciliationRequestV1 {
      source: SweepReceiptReconciliationSourceV1::Recovery(SweepReceiptRecoveryIdentityV1 {
        hash_algorithm: algorithm,
        database_id: &database_id,
        proposal_hash: &proposal.key,
        proposal_write_sequence,
      }),
      cancellation: &cancellation,
      memory: &memory,
    };
    let mut authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
    authority.snapshot.reclaim_committed_at_ms = committed_at_ms;
    let error = publisher.reconcile_sweep_receipt(request, &mut authority).unwrap_err();
    assert_eq!(error.code(), "durability_failure", "failure {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure {failure:?}");
    drop(publisher);

    let late_failure = matches!(
      failure,
      FirstAuthorityFailurePoint::HeaderWriteAfter | FirstAuthorityFailurePoint::FullBarrier | FirstAuthorityFailurePoint::Verify
    );
    let (_restart_coordinator, reopened) = reopen(&path);
    let reopened_locator = reopened.locator(&expected_receipt.key).unwrap();
    assert_eq!(reopened_locator.is_some(), late_failure, "failure {failure:?}");

    let retry_cancellation = CancellationToken::new();
    let retry_request = SweepReceiptReconciliationRequestV1 { cancellation: &retry_cancellation, ..request };
    let mut retry_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
    retry_authority.snapshot.reclaim_committed_at_ms = committed_at_ms;
    if late_failure {
      retry_authority.snapshot.allocator_admission_blocked = false;
      retry_authority.snapshot.existing_receipt = Some(ExistingSweepReceiptAuthorityV1 {
        receipt_hash: expected_receipt.key.clone(),
        receipt_write_sequence: proposal_write_sequence + 1,
      });
    }
    let receipt = reopened.reconcile_sweep_receipt(retry_request, &mut retry_authority).unwrap();
    assert_eq!(receipt.receipt_key, expected_receipt.key, "failure {failure:?}");
    assert_eq!(receipt.hard_publication_sequence, proposal_write_sequence + 1, "failure {failure:?}");
    assert_eq!(retry_authority.recovery_calls, usize::from(!late_failure), "failure {failure:?}");
    assert!(reopened.locator(&receipt.receipt_key).unwrap().is_some(), "failure {failure:?}");
  }
}

#[test]
fn guarded_root_reclaim_selects_control_last_without_removing_any_physical_entity() {
  let algorithm = HashAlgorithm::Blake3_256;
  let physical_inventory_bytes = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
  let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
  let (_directory, _path, coordinator, mut publisher) = create_environment_for_database("guarded-root-reclaim", None, database_id);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement =
    prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
  let target_locator = publisher.locator(&retirement.target_root_hash).unwrap().unwrap();
  let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let _retirement_receipt =
    publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
  let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
  assert!(publisher.locator(&reclaim.expiry_page.key).unwrap().is_some());
  assert!(publisher.locator(&reclaim.expiry_directory.key).unwrap().is_some());
  let before = publisher.observe().unwrap();

  let receipt = publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap();

  assert!(!receipt.idempotent);
  assert_eq!(receipt.namespace_root_hash, retirement.target_root_hash);
  assert_eq!(receipt.lifecycle_control_slot, 1);
  assert!(receipt.root_object_reclaim_proof_write_sequence < receipt.expiry_manifest_write_sequence);
  assert!(receipt.expiry_manifest_write_sequence < receipt.lifecycle_manifest_write_sequence);
  assert!(receipt.lifecycle_manifest_write_sequence < receipt.lifecycle_control_write_sequence);
  assert!(matches!(receipt.lineage_state, RootReclaimLineageStateV1::HardPublished { .. }));
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), reclaim.lifecycle_manifest.key);
  assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator);
  assert!(publisher.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_some());
  assert!(publisher.locator(&reclaim.expiry_manifest.key).unwrap().is_some());
  assert!(publisher.locator(&reclaim.lifecycle_manifest.key).unwrap().is_some());
  assert!(publisher.locator(&reclaim.lifecycle_control.key).unwrap().is_some());
  assert!(publisher.observe().unwrap().selected.header.slot_sequence > before.selected.header.slot_sequence);

  let before_retry = publisher.observe().unwrap();
  let before_retry_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let retry = publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap();
  assert!(retry.idempotent);
  assert!(matches!(retry.lineage_state, RootReclaimLineageStateV1::NotRequired));
  assert_eq!(publisher.observe().unwrap(), before_retry);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_retry_frontier);
  assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator);
}

#[test]
fn guarded_root_reclaim_rejects_an_aggregate_identical_substituted_expiry_catalog() {
  let algorithm = HashAlgorithm::Blake3_256;
  let physical_inventory_bytes = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
  let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
  let (_directory, _path, _coordinator, mut publisher) = create_environment_for_database("substituted-root-reclaim", None, database_id);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement =
    prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
  let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let _retirement_receipt =
    publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
  let mut reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
  substitute_guarded_root_reclaim_expiry_row(&publisher, &mut reclaim, database_id, &cancellation, &memory);

  let error =
    publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_reclaim_support_closure");
  assert!(error.committed_receipt().is_none());
  assert!(publisher.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_none());
  assert!(publisher.locator(&reclaim.expiry_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&reclaim.lifecycle_manifest.key).unwrap().is_none());
}

#[test]
fn guarded_root_reclaim_cancellation_and_active_pins_refuse_before_authority_publication_and_allow_exact_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let physical_inventory_bytes = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
  let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
  let (_directory, _path, _coordinator, mut publisher) = create_environment_for_database("guarded-root-reclaim-pins", None, database_id);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement =
    prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
  let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let _retirement_receipt =
    publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
  let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
  let retired_manifest_key = selected_root_lifecycle_manifest_key(&publisher);
  let target_locator = publisher.locator(&retirement.target_root_hash).unwrap().unwrap();

  let canceled = CancellationToken::new();
  canceled.cancel();
  let canceled_error =
    publisher.publish_root_reclaim(reclaim.request(&canceled, &retirement.pin_coordinator), &mut retirement_owner).unwrap_err();
  assert_eq!(canceled_error.code(), "root_reclaim_canceled");
  assert!(canceled_error.committed_receipt().is_none());
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), retired_manifest_key);
  assert!(publisher.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_none());

  let active_read = retirement
    .pin_coordinator
    .admit_read(&retirement.target_root_hash, &cancellation, || Ok(RootLifecycleObservationV1::Retained))
    .unwrap();
  let pinned_error =
    publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap_err();
  assert_eq!(pinned_error.code(), "root_pinned");
  assert!(pinned_error.committed_receipt().is_none());
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), retired_manifest_key);
  assert!(publisher.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_none());
  assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator);

  drop(active_read);
  let receipt = publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap();
  assert!(!receipt.idempotent);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), reclaim.lifecycle_manifest.key);
  assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator);
}

#[test]
fn guarded_root_reclaim_rejects_stale_lifecycle_before_publishing_the_proof() {
  let algorithm = HashAlgorithm::Blake3_256;
  let physical_inventory_bytes = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
  let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
  let (_directory, _path, _coordinator, mut publisher) = create_environment_for_database("stale-root-reclaim", None, database_id);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement =
    prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
  let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let _retirement_receipt =
    publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
  let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
  let stale_manifest = publish_empty_lifecycle_authority_for_database(
    &publisher,
    &mut retirement_owner,
    1,
    4,
    8,
    reclaim.publication_timestamp_ms,
    database_id,
  );

  let error =
    publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_reclaim_prior_lifecycle_changed");
  assert!(error.committed_receipt().is_none());
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), stale_manifest.key);
  assert!(publisher.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_none());
  assert!(publisher.locator(&reclaim.expiry_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&reclaim.lifecycle_manifest.key).unwrap().is_none());
}

#[test]
fn every_root_reclaim_selector_header_failure_restarts_as_exactly_retired_or_reclaimed_without_removing_the_target() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let algorithm = HashAlgorithm::Blake3_256;
    let physical_inventory_bytes = fs::read(
      Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
    )
    .unwrap();
    let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
    let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
    let (_directory, path, coordinator, mut publisher) =
      create_environment_for_database(&format!("root-reclaim-selector-{failure:?}"), None, database_id);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id,
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let retirement =
      prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
    let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
      expected_database_id: database_id,
      expected_root_hash: retirement.target_root_hash.clone(),
      expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
    };
    let _retirement_receipt =
      publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
    let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
    let retired_manifest_key = selected_root_lifecycle_manifest_key(&publisher);
    let target_locator = publisher.locator(&retirement.target_root_hash).unwrap().unwrap();
    publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(NthHeaderPublicationFaultIo::new(failure, 5))),
      root_state: publisher.root_state,
    };

    let error =
      publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap_err();

    assert!(coordinator.hard_failure().unwrap().is_some(), "failure {failure:?}");
    let selector_may_have_committed = matches!(
      failure,
      FirstAuthorityFailurePoint::HeaderWriteAfter | FirstAuthorityFailurePoint::FullBarrier | FirstAuthorityFailurePoint::Verify
    );
    if selector_may_have_committed {
      let receipt = error.committed_receipt().expect("a selected uncertain reclaim needs an exact committed receipt");
      assert_eq!(receipt.lifecycle_manifest_key, reclaim.lifecycle_manifest.key, "failure {failure:?}");
      assert!(matches!(receipt.lineage_state, RootReclaimLineageStateV1::BufferedAfterFlushFailure { .. }), "failure {failure:?}");
      assert_eq!(retirement_owner.status().pending_records, 1, "failure {failure:?}");
    } else {
      assert!(error.committed_receipt().is_none(), "failure {failure:?}");
      assert_eq!(retirement_owner.status().pending_records, 0, "failure {failure:?}");
    }
    assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator, "failure {failure:?}");
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    let expected_manifest = if selector_may_have_committed { &reclaim.lifecycle_manifest.key } else { &retired_manifest_key };
    assert_eq!(&selected_root_lifecycle_manifest_key(&reopened), expected_manifest, "failure {failure:?}");
    assert_eq!(reopened.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator, "failure {failure:?}");
    assert!(reopened.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_some(), "failure {failure:?}");

    let retry_cancellation = CancellationToken::new();
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id,
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &retry_cancellation,
      &memory,
    )
    .unwrap();
    let retry = reopened.publish_root_reclaim(reclaim.request(&retry_cancellation, &retirement.pin_coordinator), &mut retry_owner).unwrap();
    assert_eq!(retry.idempotent, selector_may_have_committed, "failure {failure:?}");
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), reclaim.lifecycle_manifest.key, "failure {failure:?}");
    assert_eq!(reopened.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator, "failure {failure:?}");
  }
}

#[test]
fn root_reclaim_post_selector_lineage_and_pin_cleanup_failures_preserve_committed_receipts_and_target_bytes() {
  for failure in ["lineage", "pin_cleanup"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let physical_inventory_bytes = fs::read(
      Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
    )
    .unwrap();
    let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
    let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
    let (_directory, path, _coordinator, mut publisher) =
      create_environment_for_database(&format!("root-reclaim-post-selector-{failure}"), None, database_id);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id,
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let retirement =
      prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
    let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
      expected_database_id: database_id,
      expected_root_hash: retirement.target_root_hash.clone(),
      expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
    };
    let _retirement_receipt =
      publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
    let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
    let target_locator = publisher.locator(&retirement.target_root_hash).unwrap().unwrap();

    let error = if failure == "lineage" {
      let mut observer = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };
      publisher
        .publish_root_reclaim_with_control_observer(
          reclaim.request(&cancellation, &retirement.pin_coordinator),
          &mut retirement_owner,
          &mut observer,
        )
        .unwrap_err()
    } else {
      retirement.pin_coordinator.fail_next_cleanup_for_test();
      publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap_err()
    };

    let expected_code = if failure == "lineage" { "root_reclaim_committed_lineage" } else { "root_reclaim_committed_pin_cleanup" };
    assert_eq!(error.code(), expected_code, "failure {failure}");
    let receipt = error.committed_receipt().expect("post-selector reclaim failure needs an exact committed receipt");
    assert_eq!(receipt.lifecycle_manifest_key, reclaim.lifecycle_manifest.key, "failure {failure}");
    if failure == "lineage" {
      assert!(matches!(receipt.lineage_state, RootReclaimLineageStateV1::BufferedAfterFlushFailure { .. }));
      assert_eq!(retirement_owner.status().pending_records, 1);
    } else {
      assert!(matches!(receipt.lineage_state, RootReclaimLineageStateV1::HardPublished { .. }));
      assert_eq!(retirement_owner.status().pending_records, 0);
    }
    assert_eq!(selected_root_lifecycle_manifest_key(&publisher), reclaim.lifecycle_manifest.key, "failure {failure}");
    assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator, "failure {failure}");
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, reopened) = reopen(&path);
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), reclaim.lifecycle_manifest.key, "failure {failure}");
    assert_eq!(reopened.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator, "failure {failure}");
  }
}

#[test]
fn corrupt_prior_control_manifest_or_reclaim_support_cannot_advance_root_reclaim_authority() {
  for case in ["control", "manifest", "support"] {
    let algorithm = HashAlgorithm::Blake3_256;
    let physical_inventory_bytes = fs::read(
      Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
    )
    .unwrap();
    let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
    let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
    let (_directory, _path, _coordinator, mut publisher) =
      create_environment_for_database(&format!("root-reclaim-corrupt-{case}"), None, database_id);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id,
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let retirement =
      prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
    let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
      expected_database_id: database_id,
      expected_root_hash: retirement.target_root_hash.clone(),
      expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
    };
    let _retirement_receipt =
      publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
    let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
    let retired_manifest_key = selected_root_lifecycle_manifest_key(&publisher);
    let target_locator = publisher.locator(&retirement.target_root_hash).unwrap().unwrap();
    let corrupt_key = match case {
      "control" => retirement.lifecycle_control.key.as_slice(),
      "manifest" => retirement.lifecycle_manifest.key.as_slice(),
      "support" => reclaim.expiry_directory.key.as_slice(),
      _ => unreachable!(),
    };
    corrupt_last_entity_byte(&publisher, corrupt_key);

    let error =
      publisher.publish_root_reclaim(reclaim.request(&cancellation, &retirement.pin_coordinator), &mut retirement_owner).unwrap_err();

    assert!(error.committed_receipt().is_none(), "case {case}");
    assert_eq!(retirement_owner.status().pending_records, 0, "case {case}");
    assert!(publisher.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&reclaim.expiry_manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&reclaim.lifecycle_manifest.key).unwrap().is_none(), "case {case}");
    assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator, "case {case}");
    if case == "support" {
      assert_eq!(selected_root_lifecycle_manifest_key(&publisher), retired_manifest_key, "case {case}");
    }
  }
}

#[test]
fn racing_read_pin_cannot_enter_until_root_reclaim_selects_the_new_lifecycle() {
  let algorithm = HashAlgorithm::Blake3_256;
  let physical_inventory_bytes = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
  let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
  let (_directory, _path, _coordinator, mut publisher) = create_environment_for_database("root-reclaim-racing-pin", None, database_id);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement =
    prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
  let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let _retirement_receipt =
    publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
  let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
  let selector_staged = Arc::new(Barrier::new(2));
  let selector_release = Arc::new(Barrier::new(2));
  let mut observer = BlockingControlPublicationObserverV1 { staged: selector_staged.clone(), release: selector_release.clone() };
  let pin_started = Arc::new(Barrier::new(2));
  let (lifecycle_callback_sender, lifecycle_callback_receiver) = mpsc::channel();

  std::thread::scope(|scope| {
    let reclaim_publication = scope.spawn(|| {
      publisher.publish_root_reclaim_with_control_observer(
        reclaim.request(&cancellation, &retirement.pin_coordinator),
        &mut retirement_owner,
        &mut observer,
      )
    });
    selector_staged.wait();

    let pin_coordinator = retirement.pin_coordinator.clone();
    let pin_root = retirement.target_root_hash.clone();
    let pin_started_thread = pin_started.clone();
    let pin_cancellation = CancellationToken::new();
    let pin = scope.spawn(move || {
      pin_started_thread.wait();
      pin_coordinator.admit_read(&pin_root, &pin_cancellation, || {
        lifecycle_callback_sender.send(()).unwrap();
        Ok(RootLifecycleObservationV1::PhysicallyReclaimed)
      })
    });
    pin_started.wait();
    assert!(
      matches!(lifecycle_callback_receiver.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)),
      "a new read reached lifecycle admission while reclaim held the root exclusion"
    );

    selector_release.wait();
    let receipt = reclaim_publication.join().unwrap().unwrap();
    lifecycle_callback_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    let pin_error = pin.join().unwrap().unwrap_err();
    assert_eq!(pin_error.code(), "root_expired");
    assert_eq!(receipt.lifecycle_manifest_key, reclaim.lifecycle_manifest.key);
  });

  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), reclaim.lifecycle_manifest.key);
  assert_eq!(retirement.pin_coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(retirement.pin_coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn root_reclaim_memory_pressure_refuses_before_proof_or_selector_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let physical_inventory_bytes = fs::read(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1/agca-blake3-256-physical-inventory-manifest-populated.bin"),
  )
  .unwrap();
  let physical_inventory = decode_physical_inventory_manifest_v1(&physical_inventory_bytes, algorithm).unwrap();
  let database_id: [u8; 16] = physical_inventory.database_id.try_into().unwrap();
  let (_directory, _path, _coordinator, mut publisher) = create_environment_for_database("root-reclaim-memory-pressure", None, database_id);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let retirement =
    prepare_guarded_root_retirement_for_database(&mut publisher, &mut retirement_owner, &cancellation, &memory, true, database_id);
  let mut authority_verifier = DatabaseRootRetirementAuthorityVerifierV1 {
    expected_database_id: database_id,
    expected_root_hash: retirement.target_root_hash.clone(),
    expected_authority_root_set_digest: retirement.intent.authority_root_set_digest.clone(),
  };
  let _retirement_receipt =
    publisher.publish_root_retirement(retirement.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap();
  let reclaim = prepare_guarded_root_reclaim(&publisher, &retirement, database_id, &physical_inventory_bytes, &cancellation, &memory);
  let retired_manifest_key = selected_root_lifecycle_manifest_key(&publisher);
  let target_locator = publisher.locator(&retirement.target_root_hash).unwrap().unwrap();
  let constrained_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap()));
  let constrained_pins = RootReadPinCoordinatorV1::new(constrained_memory, algorithm, 1, 1).unwrap();

  let error = publisher.publish_root_reclaim(reclaim.request(&cancellation, &constrained_pins), &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_support_memory");
  assert!(error.committed_receipt().is_none());
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), retired_manifest_key);
  assert!(publisher.locator(&reclaim.root_object_reclaim_proof.key).unwrap().is_none());
  assert!(publisher.locator(&reclaim.expiry_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&reclaim.lifecycle_manifest.key).unwrap().is_none());
  assert_eq!(publisher.locator(&retirement.target_root_hash).unwrap().unwrap(), target_locator);
  assert_eq!(retirement_owner.status().pending_records, 0);
}

#[test]
fn guarded_root_retirement_requires_the_exact_support_closure_to_be_durable_before_exclusion() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-missing-support", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, false);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_support_missing");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none());
}

#[test]
fn guarded_root_retirement_refuses_active_read_pins_before_final_authority_recheck() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-active-pin", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let read = prepared
    .pin_coordinator
    .admit_read(&prepared.target_root_hash, &cancellation, || {
      Ok(RootLifecycleObservationV1::PendingDelete {
        pending_since_ms: prepared.intent.pending_since_ms,
        grace_at_pending_ms: prepared.intent.grace_at_pending_ms,
        current_configured_grace_ms: prepared.intent.grace_at_pending_ms,
      })
    })
    .unwrap();
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_pinned");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_control.key).unwrap().is_some());
  drop(read);
  assert_eq!(prepared.pin_coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(prepared.pin_coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn guarded_root_retirement_refuses_authoritative_or_changed_root_sets_before_publication() {
  for case in ["target-authoritative", "authority-digest-changed"] {
    let (_directory, _path, _coordinator, mut publisher) = create_environment(case, None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    let returned_authority_root_set_digest = if case == "authority-digest-changed" {
      digest_parts(HashAlgorithm::Blake3_256, &[b"changed caller authority roots"])
    } else {
      prepared.intent.authority_root_set_digest.clone()
    };
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: Some(returned_authority_root_set_digest),
      target_is_authoritative: case == "target-authoritative",
    };

    let error =
      publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

    assert_eq!(error.code(), "root_retirement_authority_changed", "case {case}");
    assert!(authority_verifier.called, "case {case}");
    assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key, "case {case}");
    assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none(), "case {case}");
  }
}

#[test]
fn guarded_root_retirement_propagates_authority_source_failure_without_selecting_retirement() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-authority-failure", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = FailingRootRetirementAuthorityVerifierV1 { called: false };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_authority_source_unavailable");
  assert!(authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_control.key).unwrap().is_some());
}

#[test]
fn guarded_root_retirement_cancellation_refuses_before_support_scan_or_authority_callback() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-canceled", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };
  cancellation.cancel();

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_canceled");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
}

#[test]
fn guarded_root_retirement_refuses_when_selected_prior_lifecycle_advances() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-prior-advanced", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let advanced = publish_empty_lifecycle_authority(&publisher, &mut retirement_owner, 0, 3, 5, 1_700_000_090_000);
  retirement_owner.flush(&mut publisher).unwrap();
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), advanced.key);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_prior_lifecycle_changed");
  assert!(!authority_verifier.called);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), advanced.key);
  assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none());
}

#[test]
fn root_retirement_failure_before_selector_keeps_prior_lifecycle_selected_across_restart() {
  for phase in [DependencyFailurePhase::BeforeEntity, DependencyFailurePhase::EntityWritten, DependencyFailurePhase::EntityStaged] {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("root-retirement-before-selector-{phase:?}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    let old_control_locator = publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap();
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: None,
      target_is_authoritative: false,
    };
    let mut observer = FailingDependencyObserver { phase, entity_index: 0 };

    let error = publisher
      .publish_root_retirement_with_control_observer(
        prepared.request(&cancellation),
        &mut authority_verifier,
        &mut retirement_owner,
        &mut observer,
      )
      .unwrap_err();

    assert_eq!(error.code(), "durability_failure", "phase {phase:?}");
    assert!(error.committed_receipt().is_none(), "phase {phase:?}");
    assert!(authority_verifier.called, "phase {phase:?}");
    assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.prior_lifecycle_manifest_key, "phase {phase:?}");
    assert_eq!(publisher.locator(&prepared.lifecycle_control.key).unwrap().unwrap(), old_control_locator, "phase {phase:?}");
    assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_some(), "phase {phase:?}");
    assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_some(), "phase {phase:?}");
    assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_some(), "phase {phase:?}");
    assert_eq!(retirement_owner.status().pending_records, 0, "phase {phase:?}");
    assert_eq!(retirement_owner.status().durable_records, 0, "phase {phase:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "phase {phase:?}");
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.prior_lifecycle_manifest_key, "phase {phase:?}");
    let retry_cancellation = CancellationToken::new();
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &retry_cancellation,
      &memory,
    )
    .unwrap();
    authority_verifier.called = false;
    let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
    assert!(authority_verifier.called, "phase {phase:?}");
    assert!(!retry.idempotent, "phase {phase:?}");
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key, "phase {phase:?}");
    assert!(matches!(retry.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }), "phase {phase:?}");
  }
}

#[test]
fn every_final_selector_header_failure_restarts_as_exactly_pending_or_retired_and_retains_uncertain_lineage() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("root-retirement-selector-{failure:?}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(NthHeaderPublicationFaultIo::new(failure, 5))),
      root_state: publisher.root_state,
    };
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: None,
      target_is_authoritative: false,
    };

    let error =
      publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

    assert!(authority_verifier.called, "failure {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure {failure:?}");
    let selector_may_have_committed = matches!(
      failure,
      FirstAuthorityFailurePoint::HeaderWriteAfter | FirstAuthorityFailurePoint::FullBarrier | FirstAuthorityFailurePoint::Verify
    );
    if selector_may_have_committed {
      let receipt = error.committed_receipt().expect("a selected uncertain lifecycle control needs an exact committed receipt");
      assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key, "failure {failure:?}");
      assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::BufferedAfterFlushFailure { .. }), "failure {failure:?}");
      assert_eq!(retirement_owner.status().pending_records, 1, "failure {failure:?}");
    } else {
      assert!(error.committed_receipt().is_none(), "failure {failure:?}");
      assert_eq!(retirement_owner.status().pending_records, 0, "failure {failure:?}");
    }
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    let selected_manifest = selected_root_lifecycle_manifest_key(&reopened);
    let expected_manifest =
      if selector_may_have_committed { &prepared.lifecycle_manifest.key } else { &prepared.prior_lifecycle_manifest_key };
    assert_eq!(&selected_manifest, expected_manifest, "failure {failure:?}");
    let retry_cancellation = CancellationToken::new();
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &retry_cancellation,
      &memory,
    )
    .unwrap();
    authority_verifier.called = false;
    let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
    assert_eq!(retry.idempotent, selector_may_have_committed, "failure {failure:?}");
    assert_eq!(authority_verifier.called, !selector_may_have_committed, "failure {failure:?}");
    assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key, "failure {failure:?}");
  }
}

#[test]
fn racing_read_pin_cannot_enter_until_retirement_selects_the_new_lifecycle() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("root-retirement-racing-pin", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let verifier_entered = Arc::new(Barrier::new(2));
  let verifier_release = Arc::new(Barrier::new(2));
  let mut authority_verifier = BlockingRootRetirementAuthorityVerifierV1 {
    entered: verifier_entered.clone(),
    release: verifier_release.clone(),
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
  };
  let pin_started = Arc::new(Barrier::new(2));
  let (lifecycle_callback_sender, lifecycle_callback_receiver) = mpsc::channel();

  std::thread::scope(|scope| {
    let retirement =
      scope.spawn(|| publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner));
    verifier_entered.wait();

    let pin_coordinator = prepared.pin_coordinator.clone();
    let pin_root = prepared.target_root_hash.clone();
    let pin_started_thread = pin_started.clone();
    let pin_cancellation = CancellationToken::new();
    let pin = scope.spawn(move || {
      pin_started_thread.wait();
      pin_coordinator.admit_read(&pin_root, &pin_cancellation, || {
        lifecycle_callback_sender.send(()).unwrap();
        Ok(RootLifecycleObservationV1::LogicallyRetired)
      })
    });
    pin_started.wait();
    assert!(
      matches!(lifecycle_callback_receiver.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)),
      "a new read reached lifecycle admission while retirement held the root exclusion"
    );

    verifier_release.wait();
    let retirement_receipt = retirement.join().unwrap().unwrap();
    lifecycle_callback_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    let pin_error = pin.join().unwrap().unwrap_err();
    assert_eq!(pin_error.code(), "root_expired");
    assert_eq!(retirement_receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  });

  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  assert_eq!(prepared.pin_coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(prepared.pin_coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn post_selector_lineage_failure_returns_the_exact_committed_retirement_receipt() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("root-retirement-buffered-lineage", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };
  let mut observer = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };

  let error = publisher
    .publish_root_retirement_with_control_observer(
      prepared.request(&cancellation),
      &mut authority_verifier,
      &mut retirement_owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "root_retirement_committed_lineage");
  let receipt = error.committed_receipt().expect("selected lifecycle authority requires a committed receipt");
  assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  assert!(matches!(
    receipt.lineage_state,
    RootRetirementLineageStateV1::BufferedAfterFlushFailure { code: "retirement_journal_cancelled", .. }
  ));
  assert!(authority_verifier.called);
  assert_eq!(retirement_owner.status().pending_records, 1);
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
  let retry_cancellation = CancellationToken::new();
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &retry_cancellation,
    &memory,
  )
  .unwrap();
  authority_verifier.called = false;
  let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
  assert!(retry.idempotent);
  assert!(!authority_verifier.called);
}

#[test]
fn post_selector_pin_cleanup_failure_returns_the_exact_committed_retirement_receipt() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("root-retirement-pin-cleanup", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = CleanupFailingRootRetirementAuthorityVerifierV1 {
    pin_coordinator: prepared.pin_coordinator.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "root_retirement_committed_pin_cleanup");
  let receipt = error.committed_receipt().expect("pin cleanup failure happened after lifecycle selection");
  assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }));
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
}

#[test]
fn selector_uncertainty_and_pin_cleanup_failure_preserve_the_receipt_and_both_diagnostics() {
  let (_directory, path, coordinator, mut publisher) = create_environment("root-retirement-selector-and-pin-cleanup", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  publisher = V4FirstAuthorityPublisher {
    file: publisher.file,
    kv: publisher.kv,
    header_publisher: DatabaseHeaderPublisherV4::with_io(
      coordinator,
      Arc::new(NthHeaderPublicationFaultIo::new(FirstAuthorityFailurePoint::Verify, 5)),
    ),
    root_state: publisher.root_state,
  };
  let mut authority_verifier = CleanupFailingRootRetirementAuthorityVerifierV1 {
    pin_coordinator: prepared.pin_coordinator.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
  };

  let error =
    publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "gc_control_committed_authority_uncertain");
  let receipt = error.committed_receipt().expect("combined post-selector failures must preserve the committed retirement receipt");
  assert_eq!(receipt.lifecycle_manifest_key, prepared.lifecycle_manifest.key);
  assert!(matches!(receipt.lineage_state, RootRetirementLineageStateV1::BufferedAfterFlushFailure { .. }));
  assert!(error.to_string().contains("releasing the root retirement exclusion also failed"));
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
}

#[test]
fn corrupt_prior_control_manifest_or_support_locator_cannot_advance_logical_retirement() {
  for case in ["control", "manifest", "support"] {
    let (_directory, _path, _coordinator, mut publisher) = create_environment(&format!("root-retirement-corrupt-{case}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
    let corrupt_key = match case {
      "control" => gc_active_control_key(HashAlgorithm::Blake3_256, GcArtifactKindV1::RootLifecycleActiveControl, &[0x31; 16], 1).unwrap(),
      "manifest" => prepared.prior_lifecycle_manifest_key.clone(),
      "support" => prepared.support_closure.expiry_directory_hash().unwrap().to_vec(),
      _ => unreachable!(),
    };
    corrupt_last_entity_byte(&publisher, &corrupt_key);
    let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
      called: false,
      expected_root_hash: prepared.target_root_hash.clone(),
      expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
      returned_authority_root_set_digest: None,
      target_is_authoritative: false,
    };

    let error =
      publisher.publish_root_retirement(prepared.request(&cancellation), &mut authority_verifier, &mut retirement_owner).unwrap_err();

    assert!(error.committed_receipt().is_none(), "case {case}");
    assert!(!authority_verifier.called, "case {case}");
    assert!(publisher.locator(&prepared.retirement_commit.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.expiry_manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.lifecycle_manifest.key).unwrap().is_none(), "case {case}");
    assert_eq!(retirement_owner.status().pending_records, 0, "case {case}");
  }
}

#[test]
fn root_retirement_failure_after_selector_reports_committed_and_restarts_as_retired() {
  let (_directory, path, coordinator, mut publisher) = create_environment("root-retirement-after-selector", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_root_retirement(&mut publisher, &mut retirement_owner, &cancellation, &memory, true);
  let mut authority_verifier = ExactRootRetirementAuthorityVerifierV1 {
    called: false,
    expected_root_hash: prepared.target_root_hash.clone(),
    expected_authority_root_set_digest: prepared.intent.authority_root_set_digest.clone(),
    returned_authority_root_set_digest: None,
    target_is_authoritative: false,
  };
  let mut observer = FailingPostCommitObserver;

  let error = publisher
    .publish_root_retirement_with_control_observer(
      prepared.request(&cancellation),
      &mut authority_verifier,
      &mut retirement_owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "gc_control_committed_postcondition_failure");
  let committed = error.committed_receipt().expect("selected lifecycle control must return an exact committed receipt");
  assert!(authority_verifier.called);
  assert!(!committed.idempotent);
  assert!(matches!(committed.lineage_state, RootRetirementLineageStateV1::HardPublished { .. }));
  assert_eq!(selected_root_lifecycle_manifest_key(&publisher), prepared.lifecycle_manifest.key);
  assert_eq!(retirement_owner.status().pending_records, 0);
  assert_eq!(retirement_owner.status().durable_records, 1);
  assert!(coordinator.hard_failure().unwrap().is_none());
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  assert_eq!(selected_root_lifecycle_manifest_key(&reopened), prepared.lifecycle_manifest.key);
  let retry_cancellation = CancellationToken::new();
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    2,
    402,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &retry_cancellation,
    &memory,
  )
  .unwrap();
  authority_verifier.called = false;
  authority_verifier.target_is_authoritative = true;
  let retry = reopened.publish_root_retirement(prepared.request(&retry_cancellation), &mut authority_verifier, &mut retry_owner).unwrap();
  assert!(retry.idempotent);
  assert!(!authority_verifier.called);
}

#[test]
fn mark_control_post_commit_failure_returns_exact_receipt_and_hard_lineage() {
  let (directory, path, _coordinator, mut publisher) = create_environment("mark-control-post-commit", None);
  publish_first_authority(&publisher);
  let scratch_root = directory.path().join("mark-scratch");
  std::fs::create_dir(&scratch_root).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x51, 101, 1);
  let _first_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &first, 1_700_000_200_001);
  let second = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x52, 102, 2);
  let _second_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &second, 1_700_000_200_002);
  let replacement = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x53, 103, 3);
  let mut observer = FailingPostCommitObserver;

  let error = publisher
    .publish_mark_run_checkpoint_with_control_observer(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &replacement.checkpoint,
        control: &replacement.control,
        workspace: &replacement.closure,
        publication_timestamp_ms: 1_700_000_200_003,
        monotonic_now_ms: 1_700_000_200_003,
      },
      &mut owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "mark_checkpoint_control_committed_postcondition_failure");
  let committed = error.committed_receipt().expect("selected mark control must return its exact committed receipt");
  assert_eq!(committed.control_slot, 0);
  assert!(committed.replaced_control);
  assert!(!committed.idempotent);
  assert!(matches!(committed.lineage_state, MarkRunCheckpointLineageStateV1::HardPublished { .. }));
  assert_eq!(owner.status().pending_records, 0);
  assert_eq!(owner.status().durable_records, 1);
  let committed_locator = publisher.locator(&replacement.control.key).unwrap().unwrap();
  assert_eq!(committed_locator.type_flags, kv_tag::GC_ARTIFACT);
  assert!(committed_locator.offset < committed.observation.selected.header.hot_tail_offset);

  let before_retry = publisher.observe().unwrap();
  let retry = publish_mark_checkpoint(&mut publisher, &mut owner, &replacement, 1_700_000_200_003);
  assert!(retry.idempotent);
  assert_eq!(publisher.observe().unwrap(), before_retry);
}

#[test]
fn mark_control_activation_failure_discards_soft_lineage_and_keeps_old_control_selected() {
  let (directory, path, coordinator, mut publisher) = create_environment("mark-control-pre-commit", None);
  publish_first_authority(&publisher);
  let scratch_root = directory.path().join("mark-scratch");
  std::fs::create_dir(&scratch_root).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x61, 201, 1);
  let _first_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &first, 1_700_000_300_001);
  let second = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x62, 202, 2);
  let _second_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &second, 1_700_000_300_002);
  let replacement = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x63, 203, 3);
  let old_locator = publisher.locator(&first.control.key).unwrap().unwrap();
  let mut observer = FailingDependencyObserver { phase: DependencyFailurePhase::BeforeEntity, entity_index: 0 };

  let error = publisher
    .publish_mark_run_checkpoint_with_control_observer(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &replacement.checkpoint,
        control: &replacement.control,
        workspace: &replacement.closure,
        publication_timestamp_ms: 1_700_000_300_003,
        monotonic_now_ms: 1_700_000_300_003,
      },
      &mut owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "durability_failure");
  assert!(error.committed_receipt().is_none());
  assert_eq!(publisher.locator(&replacement.control.key).unwrap().unwrap(), old_locator);
  assert_eq!(owner.status().pending_records, 0);
  assert_eq!(owner.status().durable_records, 0);
  assert!(coordinator.hard_failure().unwrap().is_some());
  let interrupted = publisher.observe().unwrap();
  let interrupted_length = std::fs::metadata(&path).unwrap().len();
  assert!(interrupted_length > interrupted.selected.header.hot_tail_offset);
  let reserved_write_sequence = interrupted.selected.header.write_sequence_high_water;
  drop(owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  assert_eq!(reopened.locator(&replacement.control.key).unwrap().unwrap(), old_locator);
  let retry_cancellation = CancellationToken::new();
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &retry_cancellation,
    &memory,
  )
  .unwrap();
  let retry = publish_mark_checkpoint(&mut reopened, &mut retry_owner, &replacement, 1_700_000_300_003);
  assert_eq!(retry.control_write_sequence, reserved_write_sequence + 1);
  assert!(reopened.locator(&replacement.control.key).unwrap().unwrap().offset >= interrupted_length);
  assert_eq!(retry_owner.status().pending_records, 0);
  assert_eq!(retry_owner.status().durable_records, 1);
}

#[test]
fn mark_control_surfaces_buffered_lineage_when_immediate_post_commit_flush_fails() {
  let (directory, path, _coordinator, mut publisher) = create_environment("mark-control-buffered-lineage", None);
  publish_first_authority(&publisher);
  let scratch_root = directory.path().join("mark-scratch");
  std::fs::create_dir(&scratch_root).unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap());
  let cancellation = CancellationToken::new();
  let mut owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x71, 301, 1);
  let _first_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &first, 1_700_000_400_001);
  let second = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x72, 302, 2);
  let _second_receipt = publish_mark_checkpoint(&mut publisher, &mut owner, &second, 1_700_000_400_002);
  let replacement = prepare_mark_checkpoint(&path, &scratch_root, &memory, 0x73, 303, 3);
  let mut observer = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };

  let receipt = publisher
    .publish_mark_run_checkpoint_with_control_observer(
      MarkRunCheckpointPublicationRequestV1 {
        hash_algorithm: HashAlgorithm::Blake3_256,
        checkpoint: &replacement.checkpoint,
        control: &replacement.control,
        workspace: &replacement.closure,
        publication_timestamp_ms: 1_700_000_400_003,
        monotonic_now_ms: 1_700_000_400_003,
      },
      &mut owner,
      &mut observer,
    )
    .unwrap();

  assert!(receipt.replaced_control);
  assert!(matches!(
    receipt.lineage_state,
    MarkRunCheckpointLineageStateV1::BufferedAfterFlushFailure { code: "retirement_journal_cancelled", .. }
  ));
  assert_eq!(owner.status().pending_records, 1);
  assert_eq!(owner.status().durable_records, 0);
  assert_eq!(publisher.locator(&replacement.control.key).unwrap().unwrap().type_flags, kv_tag::GC_ARTIFACT);
}

#[test]
fn retirement_post_commit_failure_retries_the_exact_selected_entity_without_republication() {
  let (_directory, _path, coordinator, mut publisher) = create_environment("retirement-post-commit", None);
  publish_first_authority(&publisher);
  let segment = captured_retirement_segment([0x31; 16]);
  let mut observer = FailingPostCommitObserver;

  let error = publisher.publish_retirement_journal_segment(&segment.prepared(), &mut observer).unwrap_err();

  assert_eq!(error.code(), "retirement_journal_committed_postcondition");
  let committed = publisher.observe().unwrap();
  let committed_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let retry = publisher.publish_synced(&segment.prepared()).unwrap();
  assert_eq!(retry.hard_publication_sequence, committed.selected.header.write_sequence_high_water);
  assert_eq!(publisher.observe().unwrap(), committed);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, committed_frontier);
}

#[test]
fn retirement_exact_retry_survives_later_header_timestamp_advancement() {
  let (_directory, _path, coordinator, mut publisher) = create_environment("retirement-later-header", None);
  publish_first_authority(&publisher);
  let first_segment = captured_retirement_segment([0x31; 16]);
  let first = publisher.publish_synced(&first_segment.prepared()).unwrap();
  let first_entity_timestamp = publisher.observe().unwrap().selected.header.updated_at_ms;
  let later_segment = captured_retirement_segment_with_timestamp([0x31; 16], Some(first_entity_timestamp + 10_000));
  publisher.publish_synced(&later_segment.prepared()).unwrap();
  let later_header = publisher.observe().unwrap();
  let later_frontier = coordinator.snapshot().unwrap().hard_frontier;

  let retry = publisher.publish_synced(&first_segment.prepared()).unwrap();

  assert_eq!(retry, first);
  assert_eq!(publisher.observe().unwrap(), later_header);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, later_frontier);
}

#[test]
fn every_retirement_dependency_failure_keeps_the_old_selected_hot_tail_restartable() {
  let phases = [DependencyFailurePhase::BeforeEntity, DependencyFailurePhase::EntityWritten, DependencyFailurePhase::EntityStaged];
  for phase in phases {
    let (_directory, path, coordinator, publisher) = create_environment(&format!("retirement-dependency-{phase:?}"), None);
    publish_first_authority(&publisher);
    let segment = captured_retirement_segment([0x31; 16]);
    let before = publisher.observe().unwrap();
    let mut observer = FailingDependencyObserver { phase, entity_index: 0 };

    let error = publisher.publish_retirement_journal_segment(&segment.prepared(), &mut observer).unwrap_err();

    assert_eq!(error.code(), "durability_failure", "phase {phase:?}");
    assert_eq!(publisher.observe().unwrap(), before, "phase {phase:?}");
    assert!(publisher.locator(&segment.artifact_key).unwrap().is_none(), "phase {phase:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "phase {phase:?}");
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    assert_eq!(reopened.observe().unwrap(), before, "phase {phase:?}");
    assert!(reopened.locator(&segment.artifact_key).unwrap().is_none(), "phase {phase:?}");
    reopened.publish_synced(&segment.prepared()).unwrap();
    assert!(reopened.locator(&segment.artifact_key).unwrap().is_some(), "phase {phase:?}");
  }
}

#[test]
fn every_retirement_header_failure_reopens_as_old_or_one_complete_selected_entity() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let (_directory, path, coordinator, publisher) = create_environment(&format!("retirement-header-{failure:?}"), None);
    publish_first_authority(&publisher);
    let segment = captured_retirement_segment([0x31; 16]);
    let before = publisher.observe().unwrap();
    let publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(FaultingNativeHeaderPublicationIo { failure })),
      root_state: publisher.root_state,
    };

    let error = publisher.publish_retirement_journal_segment(&segment.prepared(), &mut NoopFirstAuthorityDependencyObserverV1).unwrap_err();

    assert_eq!(error.code(), "durability_failure", "failure point {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure point {failure:?}");
    let interrupted = publisher.observe().unwrap();
    drop(publisher);

    let (restart_coordinator, mut reopened) = reopen(&path);
    assert_eq!(reopened.observe().unwrap(), interrupted, "failure point {failure:?}");
    let selected_new_entity = interrupted.selected.header.write_sequence_high_water > before.selected.header.write_sequence_high_water;
    assert_eq!(reopened.locator(&segment.artifact_key).unwrap().is_some(), selected_new_entity, "failure point {failure:?}");
    let frontier_before_retry = restart_coordinator.snapshot().unwrap().hard_frontier;
    let retry = reopened.publish_synced(&segment.prepared()).unwrap();
    assert!(reopened.locator(&segment.artifact_key).unwrap().is_some(), "failure point {failure:?}");
    if selected_new_entity {
      assert_eq!(retry.hard_publication_sequence, interrupted.selected.header.write_sequence_high_water, "failure point {failure:?}");
      assert_eq!(restart_coordinator.snapshot().unwrap().hard_frontier, frontier_before_retry, "failure point {failure:?}");
    } else {
      assert!(restart_coordinator.snapshot().unwrap().hard_frontier > frontier_before_retry, "failure point {failure:?}");
    }
  }
}

#[test]
fn retirement_authority_preconditions_refuse_before_append_or_ticket_reservation() {
  let segment = captured_retirement_segment([0x31; 16]);

  let (_missing_directory, missing_path, missing_coordinator, mut missing) = create_environment("retirement-missing-authority", None);
  let missing_before = missing.observe().unwrap();
  let missing_length = std::fs::metadata(&missing_path).unwrap().len();
  let missing_sequence = missing_coordinator.snapshot().unwrap().next_sequence;
  let error = missing.publish_synced(&segment.prepared()).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_missing_authority");
  assert_eq!(missing.observe().unwrap(), missing_before);
  assert_eq!(std::fs::metadata(&missing_path).unwrap().len(), missing_length);
  assert_eq!(missing_coordinator.snapshot().unwrap().next_sequence, missing_sequence);

  let (_mismatch_directory, mismatch_path, mismatch_coordinator, mut mismatch) = create_environment("retirement-database-mismatch", None);
  publish_first_authority(&mismatch);
  let other_database_segment = captured_retirement_segment([0x32; 16]);
  let mismatch_before = mismatch.observe().unwrap();
  let mismatch_length = std::fs::metadata(&mismatch_path).unwrap().len();
  let mismatch_sequence = mismatch_coordinator.snapshot().unwrap().next_sequence;
  let error = mismatch.publish_synced(&other_database_segment.prepared()).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_database_mismatch");
  assert_eq!(mismatch.observe().unwrap(), mismatch_before);
  assert_eq!(std::fs::metadata(&mismatch_path).unwrap().len(), mismatch_length);
  assert_eq!(mismatch_coordinator.snapshot().unwrap().next_sequence, mismatch_sequence);
}

#[test]
fn degraded_or_exhausted_retirement_authority_refuses_without_flushing_baseline_state() {
  let segment = captured_retirement_segment([0x31; 16]);

  let (_degraded_directory, degraded_path, degraded_coordinator, mut degraded) = create_environment("retirement-degraded", None);
  publish_first_authority(&degraded);
  let selected = degraded.observe().unwrap().selected;
  let invalid_slot_offset = ((1 - selected.selected_slot) * DATABASE_HEADER_V4_SLOT_LENGTH) as u64;
  write_file_at_native(&degraded.file, invalid_slot_offset, &[0; DATABASE_HEADER_V4_SLOT_LENGTH]).unwrap();
  sync_file_all_native(&degraded.file).unwrap();
  let degraded_before = degraded.observe().unwrap();
  assert!(degraded_before.selected.redundancy_degraded);
  let degraded_length = std::fs::metadata(&degraded_path).unwrap().len();
  let degraded_sequence = degraded_coordinator.snapshot().unwrap().next_sequence;
  let error = degraded.publish_synced(&segment.prepared()).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_degraded_header");
  assert_eq!(degraded.observe().unwrap(), degraded_before);
  assert_eq!(std::fs::metadata(&degraded_path).unwrap().len(), degraded_length);
  assert_eq!(degraded_coordinator.snapshot().unwrap().next_sequence, degraded_sequence);

  let (_directory, path, coordinator, mut publisher) = create_environment("retirement-exhausted-write-sequence", None);
  publish_first_authority(&publisher);
  let mut header = publisher.observe().unwrap().selected.header;
  header.write_sequence_high_water = u64::MAX;
  write_redundant_header(&publisher, &header);
  let before = publisher.observe().unwrap();
  let before_length = std::fs::metadata(&path).unwrap().len();
  let before_sequence = coordinator.snapshot().unwrap().next_sequence;

  let error = publisher.publish_synced(&segment.prepared()).unwrap_err();

  assert_eq!(error.code(), "retirement_journal_write_sequence_exhausted");
  assert_eq!(publisher.observe().unwrap(), before);
  assert_eq!(std::fs::metadata(&path).unwrap().len(), before_length);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence);
}

#[test]
fn retirement_identity_collisions_refuse_before_flushing_or_header_mutation() {
  for type_flags in [KV_TYPE_DIRECTORY, kv_tag::GC_ARTIFACT] {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("retirement-collision-{type_flags}"), None);
    publish_first_authority(&publisher);
    let segment = captured_retirement_segment([0x31; 16]);
    {
      let mut kv = publisher.kv.lock().unwrap();
      kv.insert(KVEntry { type_flags, hash: segment.artifact_key.clone(), offset: 0, total_length: 1 }).unwrap();
    }
    let mut aligned_header = publisher.observe().unwrap().selected.header;
    aligned_header.entry_count += 1;
    write_redundant_header(&publisher, &aligned_header);
    let before = publisher.observe().unwrap();
    let before_length = std::fs::metadata(&path).unwrap().len();
    let before_frontier = coordinator.snapshot().unwrap().hard_frontier;

    let error = publisher.publish_synced(&segment.prepared()).unwrap_err();

    if type_flags == KV_TYPE_DIRECTORY {
      assert_eq!(error.code(), "retirement_journal_identity_collision");
    } else {
      assert_eq!(error.code(), "truncated_entity_prefix");
    }
    assert_eq!(publisher.observe().unwrap(), before);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before_length);
    assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
  }
}

#[test]
fn every_dependency_record_failure_prefix_remains_unadmitted_after_restart() {
  let phases = [DependencyFailurePhase::BeforeEntity, DependencyFailurePhase::EntityWritten, DependencyFailurePhase::EntityStaged];
  for phase in phases {
    for entity_index in 0..FIRST_AUTHORITY_ENTITY_COUNT {
      let (_directory, path, coordinator, publisher) = create_environment(&format!("dependency-{phase:?}-{entity_index}"), None);
      let request = request();
      let before = publisher.observe().unwrap();
      let expected_root =
        prepare_namespace_root(&request, before.selected.header.hash_algorithm, before.selected.header.write_sequence_high_water).unwrap();
      let mut observer = FailingDependencyObserver { phase, entity_index };

      let error = publisher.publish_with_observer(&request, &mut observer).unwrap_err();

      assert_eq!(error.code(), "durability_failure", "phase {phase:?}, entity {entity_index}");
      assert_eq!(publisher.observe().unwrap(), before, "phase {phase:?}, entity {entity_index}");
      assert!(publisher.locator(&expected_root.root_hash).unwrap().is_none(), "phase {phase:?}, entity {entity_index}");
      assert!(coordinator.hard_failure().unwrap().is_some(), "phase {phase:?}, entity {entity_index}");
      drop(publisher);

      let (_restart_coordinator, reopened) = reopen(&path);
      assert_eq!(reopened.observe().unwrap(), before, "phase {phase:?}, entity {entity_index}");
      assert!(reopened.locator(&expected_root.root_hash).unwrap().is_none(), "phase {phase:?}, entity {entity_index}");
      assert!(!reopened.publish(&request).unwrap().idempotent, "phase {phase:?}, entity {entity_index}");
    }
  }
}

#[test]
fn every_header_failure_prefix_reopens_as_old_or_one_complete_selected_authority() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];

  for failure in failures {
    let (_directory, path, coordinator, publisher) = create_environment(&format!("prefix-{failure:?}"), Some(failure));
    let request = request();
    let initial = publisher.observe().unwrap();
    let expected_root =
      prepare_namespace_root(&request, initial.selected.header.hash_algorithm, initial.selected.header.write_sequence_high_water).unwrap();

    let error = publisher.publish(&request).unwrap_err();
    assert_eq!(error.code(), "durability_failure", "failure point {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure point {failure:?}");
    let interrupted = publisher.observe().unwrap();
    drop(publisher);

    let (restart_coordinator, reopened) = reopen(&path);
    let selected_after_restart = reopened.observe().unwrap();
    assert_eq!(selected_after_restart, interrupted, "failure point {failure:?}");
    let selected_new_authority = selected_after_restart.selected.header.head_hash == expected_root.root_hash;
    if selected_new_authority {
      assert!(reopened.locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
      assert!(reopened.admission_locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
    } else {
      assert!(selected_after_restart.selected.header.head_hash.iter().all(|byte| *byte == 0), "failure point {failure:?}");
      assert!(reopened.locator(&expected_root.root_hash).unwrap().is_none(), "failure point {failure:?}");
      assert!(reopened.admission_locator(&expected_root.root_hash).unwrap().is_none(), "failure point {failure:?}");
    }

    let frontier_before_retry = restart_coordinator.snapshot().unwrap().hard_frontier;
    let retry = reopened.publish(&request).unwrap();
    assert_eq!(retry.idempotent, selected_new_authority, "failure point {failure:?}");
    assert_eq!(retry.namespace_root.root_hash, expected_root.root_hash, "failure point {failure:?}");
    assert!(reopened.locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
    assert!(reopened.admission_locator(&expected_root.root_hash).unwrap().is_some(), "failure point {failure:?}");
    let frontier_after_retry = restart_coordinator.snapshot().unwrap().hard_frontier;
    if selected_new_authority {
      assert_eq!(frontier_after_retry, frontier_before_retry, "failure point {failure:?}");
    } else {
      assert!(frontier_after_retry > frontier_before_retry, "failure point {failure:?}");
    }
  }
}

#[test]
fn malformed_requests_and_oversized_roots_refuse_before_ticket_or_file_mutation() {
  let (_directory, coordinator, publisher) = environment("malformed");
  let before_header = publisher.observe().unwrap();
  let before_file_length = publisher.file.metadata().unwrap().len();
  let before_sequence = coordinator.snapshot().unwrap().next_sequence;
  let valid = request();
  let mut cases = Vec::new();

  let mut database_mismatch = valid.clone();
  database_mismatch.database_id[0] ^= 0xFF;
  cases.push(("database mismatch", database_mismatch));
  let mut zero_transaction = valid.clone();
  zero_transaction.transaction_id = [0; 16];
  cases.push(("zero transaction", zero_transaction));
  let mut timestamp_overflow = valid.clone();
  timestamp_overflow.created_at_ms = i64::MAX as u64 + 1;
  cases.push(("timestamp overflow", timestamp_overflow));
  let mut semantic_identity_mismatch = valid.clone();
  semantic_identity_mismatch.semantic_state.object_id[0] ^= 0xFF;
  cases.push(("semantic identity mismatch", semantic_identity_mismatch));
  let mut closure_width_mismatch = valid.clone();
  closure_width_mismatch.typed_closure_digest.pop();
  cases.push(("closure width mismatch", closure_width_mismatch));
  let mut empty_authority = valid.clone();
  empty_authority.authority_identity.clear();
  cases.push(("empty authority", empty_authority));
  let mut tree_identity_mismatch = valid.clone();
  tree_identity_mismatch.namespace_tree.root_hash[0] ^= 0xFF;
  cases.push(("tree identity mismatch", tree_identity_mismatch));

  for (case, invalid) in cases {
    assert!(publisher.publish(&invalid).is_err(), "case {case}");
    assert_eq!(publisher.observe().unwrap(), before_header, "case {case}");
    assert_eq!(publisher.file.metadata().unwrap().len(), before_file_length, "case {case}");
    assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence, "case {case}");
    assert!(coordinator.hard_failure().unwrap().is_none(), "case {case}");
  }

  let mut oversized_tree = valid.clone();
  oversized_tree.namespace_tree.stored_value = vec![0x61; FIRST_AUTHORITY_NAMESPACE_TREE_CAP + 1];
  let error = publisher.publish(&oversized_tree).unwrap_err();
  assert_eq!(error.code(), "first_authority_namespace_tree_exceeds_cap");
  assert_eq!(publisher.observe().unwrap(), before_header);
  assert_eq!(publisher.file.metadata().unwrap().len(), before_file_length);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence);
  assert!(coordinator.hard_failure().unwrap().is_none());

  assert!(!publisher.publish(&valid).unwrap().idempotent, "invalid requests must leave authority reusable");
}

#[test]
fn existing_package_identity_refuses_before_hard_admission_or_new_bytes() {
  let (_directory, coordinator, publisher) = environment("collision");
  let request = request();
  seed_namespace_tree_collision(&publisher, &request);
  let before_header = publisher.observe().unwrap();
  let before_file_length = publisher.file.metadata().unwrap().len();
  let before_sequence = coordinator.snapshot().unwrap().next_sequence;

  let error = publisher.publish(&request).unwrap_err();

  assert_eq!(error.code(), "first_authority_identity_collision");
  assert_eq!(publisher.observe().unwrap(), before_header);
  assert_eq!(publisher.file.metadata().unwrap().len(), before_file_length);
  assert_eq!(coordinator.snapshot().unwrap().next_sequence, before_sequence);
  assert!(coordinator.hard_failure().unwrap().is_none());
}

#[test]
fn concurrent_exact_attempts_publish_once_and_every_retry_observes_the_same_witness() {
  let (_directory, coordinator, publisher) = environment("concurrent");
  let publisher = Arc::new(publisher);
  let request = Arc::new(request());
  let start = Arc::new(std::sync::Barrier::new(16));
  let mut workers = Vec::new();
  for _ in 0..16 {
    let publisher = publisher.clone();
    let request = request.clone();
    let start = start.clone();
    workers.push(std::thread::spawn(move || {
      start.wait();
      publisher.publish(&request).unwrap()
    }));
  }
  let receipts: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();

  assert_eq!(receipts.iter().filter(|receipt| !receipt.idempotent).count(), 1);
  let first = &receipts[0];
  for receipt in &receipts[1..] {
    assert_eq!(receipt.namespace_root, first.namespace_root);
    assert_eq!(receipt.admission_control, first.admission_control);
    assert_eq!(receipt.publication_sequence, first.publication_sequence);
    assert_eq!(receipt.observation, first.observation);
  }
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, first.publication_sequence);
}

#[test]
fn clean_restart_loads_the_exact_witness_without_another_hard_publication() {
  let (_directory, path, _coordinator, publisher) = create_environment("restart", None);
  let request = request();
  let first = publisher.publish(&request).unwrap();
  drop(publisher);
  let (restart_coordinator, reopened) = reopen(&path);
  let before = restart_coordinator.snapshot().unwrap();

  let retry = reopened.publish(&request).unwrap();

  assert!(retry.idempotent);
  assert_eq!(retry.namespace_root, first.namespace_root);
  assert_eq!(retry.admission_control, first.admission_control);
  assert_eq!(retry.publication_sequence, first.publication_sequence);
  assert_eq!(restart_coordinator.snapshot().unwrap(), before);
}

#[test]
fn retry_rejects_oversized_locator_metadata_before_allocation() {
  let (_directory, coordinator, publisher) = environment("locator-cap");
  let request = request();
  let receipt = publisher.publish(&request).unwrap();
  let path =
    system_control_path(SystemControlKindV1::RootAdmissionCommit, &receipt.namespace_root.root_hash, SystemControlSlotV1::Immutable)
      .unwrap();
  let key = first_authority_file_path_hash(&path, HashAlgorithm::Blake3_256);
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;
  let mut kv = publisher.kv.lock().unwrap();
  let mut locator = kv.get(&key).unwrap().unwrap();
  locator.total_length = u32::MAX;
  kv.insert(locator).unwrap();
  drop(kv);

  let error = publisher.publish(&request).unwrap_err();

  assert_eq!(error.code(), "first_authority_locator_exceeds_cap");
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, before_frontier);
}

#[test]
fn selected_root_rejects_a_different_transaction_without_republication() {
  let (_directory, coordinator, publisher) = environment("retry-mismatch");
  let request = request();
  publisher.publish(&request).unwrap();
  let before = coordinator.snapshot().unwrap();
  let mut different = request.clone();
  different.transaction_id[0] ^= 0xFF;

  let error = publisher.publish(&different).unwrap_err();

  assert_eq!(error.code(), "first_authority_witness_mismatch");
  assert_eq!(coordinator.snapshot().unwrap(), before);
}

struct ExactPhysicalQuarantineAuthorityVerifierV1 {
  called: bool,
  fail: bool,
  expected_prior_manifest_hash: Vec<u8>,
  expected_next_manifest_hash: Vec<u8>,
  expected_request: PhysicalQuarantineAuthoritySnapshotV1,
  snapshot: PhysicalQuarantineAuthoritySnapshotV1,
}

impl PhysicalQuarantineAuthorityVerifierV1 for ExactPhysicalQuarantineAuthorityVerifierV1 {
  fn recheck_physical_quarantine_authority(
    &mut self,
    request: PhysicalQuarantineAuthorityRecheckRequestV1<'_>,
  ) -> Result<PhysicalQuarantineAuthoritySnapshotV1, PhysicalQuarantineAuthorityRecheckErrorV1> {
    self.called = true;
    if self.fail {
      return Err(PhysicalQuarantineAuthorityRecheckErrorV1::new(
        "quarantine_authority_source_unavailable",
        "injected quarantine authority source failure",
      ));
    }
    assert_eq!(request.hash_algorithm, HashAlgorithm::Blake3_256);
    assert_eq!(request.database_id, [0x31; 16]);
    assert_eq!(request.prior_manifest_hash, self.expected_prior_manifest_hash);
    assert_eq!(request.next_manifest_hash, self.expected_next_manifest_hash);
    assert_eq!(request.mark_generation, self.expected_request.selected_complete_mark_generation);
    assert_eq!(request.expected_authority_root_set_digest, self.expected_request.authority_root_set_digest);
    assert_eq!(request.expected_semantic_state_digest, self.expected_request.semantic_state_digest);
    assert_eq!(request.expected_kv_layout_fingerprint, self.expected_request.kv_layout_fingerprint);
    assert_eq!(request.expected_mark_result_digest, self.expected_request.mark_result_digest);
    assert_eq!(request.expected_root_lifecycle_manifest, self.expected_request.selected_root_lifecycle_manifest);
    Ok(self.snapshot.clone())
  }
}

struct BlockingPhysicalQuarantineAuthorityVerifierV1 {
  entered: Arc<Barrier>,
  release: Arc<Barrier>,
  snapshot: PhysicalQuarantineAuthoritySnapshotV1,
}

impl PhysicalQuarantineAuthorityVerifierV1 for BlockingPhysicalQuarantineAuthorityVerifierV1 {
  fn recheck_physical_quarantine_authority(
    &mut self,
    _request: PhysicalQuarantineAuthorityRecheckRequestV1<'_>,
  ) -> Result<PhysicalQuarantineAuthoritySnapshotV1, PhysicalQuarantineAuthorityRecheckErrorV1> {
    self.entered.wait();
    self.release.wait();
    Ok(self.snapshot.clone())
  }
}

struct PreparedGuardedPhysicalQuarantineV1 {
  permit: PhysicalQuarantinePublicationPermitV1,
  manifest: EncodedImmutableGcArtifactV1,
  control: EncodedGcActiveControlV1,
  lifecycle_manifest: EncodedImmutableGcArtifactV1,
  pin_coordinator: RootReadPinCoordinatorV1,
  authority_snapshot: PhysicalQuarantineAuthoritySnapshotV1,
  prior_manifest_key: Vec<u8>,
  publication_timestamp_ms: u64,
}

impl PreparedGuardedPhysicalQuarantineV1 {
  fn request<'a>(&'a self, cancellation: &'a CancellationToken) -> PhysicalQuarantinePublicationRequestV1<'a> {
    self.request_with_pins(cancellation, &self.pin_coordinator)
  }

  fn request_with_pins<'a>(
    &'a self,
    cancellation: &'a CancellationToken,
    pin_coordinator: &'a RootReadPinCoordinatorV1,
  ) -> PhysicalQuarantinePublicationRequestV1<'a> {
    PhysicalQuarantinePublicationRequestV1 {
      permit: &self.permit,
      quarantine_manifest: &self.manifest,
      quarantine_control: &self.control,
      publication_timestamp_ms: self.publication_timestamp_ms,
      monotonic_now_ms: self.publication_timestamp_ms,
      cancellation,
      pin_coordinator,
    }
  }
}

fn prepare_guarded_physical_quarantine(
  publisher: &mut V4FirstAuthorityPublisher,
  retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  cancellation: &CancellationToken,
  memory: &Arc<MemoryCoordinator>,
) -> PreparedGuardedPhysicalQuarantineV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  publisher.publish(&request_for_database(database_id)).unwrap();
  let lifecycle_manifest = publish_empty_lifecycle_authority(publisher, retirement_owner, 0, 1, 4, 1_700_000_050_000);
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_manifest.value, algorithm).unwrap() else {
    panic!("root lifecycle support must decode as a manifest")
  };
  let mut required_capabilities = [0u8; 32];
  for capability in [12usize, 13, 15, 17] {
    required_capabilities[capability / 8] |= 1 << (capability % 8);
  }
  let prior_authority = digest_parts(algorithm, &[b"prior quarantine authority roots"]);
  let prior_semantic = digest_parts(algorithm, &[b"prior quarantine semantic state"]);
  let prior_layout = digest_parts(algorithm, &[b"prior quarantine KV layout"]);
  let prior_mark = digest_parts(algorithm, &[b"prior quarantine mark result"]);
  let prior_manifest = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    mark_generation: 100,
    completed_at_ms: 1_700_000_060_000,
    required_capabilities: &required_capabilities,
    authority_root_set_digest: &prior_authority,
    semantic_state_digest: &prior_semantic,
    kv_layout_fingerprint: &prior_layout,
    mark_result_digest: &prior_mark,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: &lifecycle_manifest.key,
    candidate_count: 0,
    candidate_bytes: 0,
    eligible_count_hint: 0,
    eligible_bytes_hint: 0,
    next_candidate_page_id: 1,
    delta_hashes: &[],
  })
  .unwrap();
  publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::QuarantineManifest,
        database_id: &database_id,
        artifact_key: &prior_manifest.key,
        value: &prior_manifest.value,
        minimum_timestamp_ms: 1_700_000_060_000,
        committed_postcondition_code: "prior_quarantine_manifest_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let prior_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::QuarantineActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 0,
    sequence: 1,
    generation: 100,
    target_manifest_hash: &prior_manifest.key,
  })
  .unwrap();
  let prior_control_outcome = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::QuarantineActiveControl,
        encoded_control: &prior_control,
        publication_timestamp_ms: 1_700_000_060_000,
        monotonic_now_ms: 1_700_000_060_000,
      },
      retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  assert!(matches!(prior_control_outcome, GcControlPublicationOutcomeV1::Complete(_)));

  let prior = decode_quarantine_manifest_v1(&prior_manifest.value, algorithm).unwrap();
  let next_authority = digest_parts(algorithm, &[b"next quarantine authority roots"]);
  let next_semantic = digest_parts(algorithm, &[b"next quarantine semantic state"]);
  let next_layout = digest_parts(algorithm, &[b"next quarantine KV layout"]);
  let next_mark = digest_parts(algorithm, &[b"next quarantine mark result"]);
  let model = PhysicalQuarantineTransitionModelV1::new(
    PhysicalQuarantineTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_manifest: &prior,
      mark_generation: 101,
      completed_at_ms: 1_700_000_070_000,
      current_configured_grace_ms: 86_400_000,
      authority_root_set_digest: &next_authority,
      semantic_state_digest: &next_semantic,
      kv_layout_fingerprint: &next_layout,
      mark_result_digest: &next_mark,
      captured_root_lifecycle_manifest: &lifecycle_manifest.key,
      maximum_incarnations: 1,
      maximum_candidates: 1,
      mark_complete: true,
      destructive_gc_enabled: true,
      mark_authority_healthy: true,
      physical_inventory_healthy: true,
      root_lifecycle_healthy: true,
    },
    cancellation,
  )
  .unwrap();
  let transition = model.finish_for_publication().unwrap();
  let manifest = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    mark_generation: 101,
    completed_at_ms: 1_700_000_070_000,
    required_capabilities: &required_capabilities,
    authority_root_set_digest: &next_authority,
    semantic_state_digest: &next_semantic,
    kv_layout_fingerprint: &next_layout,
    mark_result_digest: &next_mark,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: &lifecycle_manifest.key,
    candidate_count: 0,
    candidate_bytes: 0,
    eligible_count_hint: 0,
    eligible_bytes_hint: 0,
    next_candidate_page_id: 1,
    delta_hashes: &[],
  })
  .unwrap();
  let next = decode_quarantine_manifest_v1(&manifest.value, algorithm).unwrap();
  let support_closure = QuarantineClosureValidatorV1::new(
    &next,
    None,
    &lifecycle,
    algorithm,
    cancellation.clone(),
    QuarantineClosureLimitsV1 { maximum_support_artifacts: 1 },
    memory,
  )
  .unwrap()
  .finish()
  .unwrap();
  let permit = qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
    prior_manifest: &prior,
    next_manifest: &next,
    support_closure: &support_closure,
    transition: &transition,
    appended_delta: None,
    cancellation,
  })
  .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::QuarantineActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 1,
    sequence: 2,
    generation: 101,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let pin_coordinator = RootReadPinCoordinatorV1::new(Arc::clone(memory), algorithm, 16, 16).unwrap();
  let authority_snapshot = PhysicalQuarantineAuthoritySnapshotV1 {
    selected_complete_mark_generation: 101,
    authority_root_set_digest: next_authority,
    semantic_state_digest: next_semantic,
    kv_layout_fingerprint: next_layout,
    mark_result_digest: next_mark,
    selected_root_lifecycle_manifest: lifecycle_manifest.key.clone(),
    physical_inventory_and_lineage_complete: true,
    all_candidate_incarnations_exact_and_unreachable: true,
    task_and_audit_pins_absent: true,
  };
  PreparedGuardedPhysicalQuarantineV1 {
    permit,
    manifest,
    control,
    lifecycle_manifest,
    pin_coordinator,
    authority_snapshot,
    prior_manifest_key: prior_manifest.key,
    publication_timestamp_ms: 1_700_000_070_001,
  }
}

fn prepare_successor_physical_quarantine(
  prior: &PreparedGuardedPhysicalQuarantineV1,
  cancellation: &CancellationToken,
  memory: &Arc<MemoryCoordinator>,
  mark_generation: u64,
  control_slot: u8,
  control_sequence: u64,
) -> PreparedGuardedPhysicalQuarantineV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let prior_manifest = decode_quarantine_manifest_v1(&prior.manifest.value, algorithm).unwrap();
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&prior.lifecycle_manifest.value, algorithm).unwrap() else {
    panic!("root lifecycle support must decode as a manifest")
  };
  let generation_bytes = mark_generation.to_le_bytes();
  let authority = digest_parts(algorithm, &[b"successor quarantine authority roots", &generation_bytes]);
  let semantic = digest_parts(algorithm, &[b"successor quarantine semantic state", &generation_bytes]);
  let layout = digest_parts(algorithm, &[b"successor quarantine KV layout", &generation_bytes]);
  let mark = digest_parts(algorithm, &[b"successor quarantine mark result", &generation_bytes]);
  let completed_at_ms = 1_700_000_070_000 + mark_generation;
  let model = PhysicalQuarantineTransitionModelV1::new(
    PhysicalQuarantineTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_manifest: &prior_manifest,
      mark_generation,
      completed_at_ms,
      current_configured_grace_ms: 86_400_000,
      authority_root_set_digest: &authority,
      semantic_state_digest: &semantic,
      kv_layout_fingerprint: &layout,
      mark_result_digest: &mark,
      captured_root_lifecycle_manifest: &prior.lifecycle_manifest.key,
      maximum_incarnations: 1,
      maximum_candidates: 1,
      mark_complete: true,
      destructive_gc_enabled: true,
      mark_authority_healthy: true,
      physical_inventory_healthy: true,
      root_lifecycle_healthy: true,
    },
    cancellation,
  )
  .unwrap();
  let transition = model.finish_for_publication().unwrap();
  let manifest = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    mark_generation,
    completed_at_ms,
    required_capabilities: prior_manifest.required_capabilities,
    authority_root_set_digest: &authority,
    semantic_state_digest: &semantic,
    kv_layout_fingerprint: &layout,
    mark_result_digest: &mark,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: &prior.lifecycle_manifest.key,
    candidate_count: 0,
    candidate_bytes: 0,
    eligible_count_hint: 0,
    eligible_bytes_hint: 0,
    next_candidate_page_id: prior_manifest.next_candidate_page_id,
    delta_hashes: &[],
  })
  .unwrap();
  let next_manifest = decode_quarantine_manifest_v1(&manifest.value, algorithm).unwrap();
  let support_closure = QuarantineClosureValidatorV1::new(
    &next_manifest,
    None,
    &lifecycle,
    algorithm,
    cancellation.clone(),
    QuarantineClosureLimitsV1 { maximum_support_artifacts: 1 },
    memory,
  )
  .unwrap()
  .finish()
  .unwrap();
  let permit = qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
    prior_manifest: &prior_manifest,
    next_manifest: &next_manifest,
    support_closure: &support_closure,
    transition: &transition,
    appended_delta: None,
    cancellation,
  })
  .unwrap();
  let control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::QuarantineActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: control_slot,
    sequence: control_sequence,
    generation: mark_generation,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let authority_snapshot = PhysicalQuarantineAuthoritySnapshotV1 {
    selected_complete_mark_generation: mark_generation,
    authority_root_set_digest: authority,
    semantic_state_digest: semantic,
    kv_layout_fingerprint: layout,
    mark_result_digest: mark,
    selected_root_lifecycle_manifest: prior.lifecycle_manifest.key.clone(),
    physical_inventory_and_lineage_complete: true,
    all_candidate_incarnations_exact_and_unreachable: true,
    task_and_audit_pins_absent: true,
  };
  PreparedGuardedPhysicalQuarantineV1 {
    permit,
    manifest,
    control,
    lifecycle_manifest: prior.lifecycle_manifest.clone(),
    pin_coordinator: prior.pin_coordinator.clone(),
    authority_snapshot,
    prior_manifest_key: prior.manifest.key.clone(),
    publication_timestamp_ms: completed_at_ms + 1,
  }
}

#[test]
fn guarded_physical_quarantine_selects_control_last_and_exact_retry_skips_stale_external_authority() {
  let (_directory, _path, coordinator, mut publisher) = create_environment("guarded-physical-quarantine", None);
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
  let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
    expected_next_manifest_hash: prepared.manifest.key.clone(),
    expected_request: prepared.authority_snapshot.clone(),
    snapshot: prepared.authority_snapshot.clone(),
  };
  let request = prepared.request(&cancellation);
  let before_frontier = coordinator.snapshot().unwrap().hard_frontier;

  let pinned_root = digest_parts(algorithm, &[b"unrelated active request pin"]);
  let active_read = prepared.pin_coordinator.admit_read(&pinned_root, &cancellation, || Ok(RootLifecycleObservationV1::Live)).unwrap();
  let error = publisher.publish_physical_quarantine(request, &mut verifier, &mut retirement_owner).unwrap_err();
  assert_eq!(error.code(), "request_pinned");
  assert!(!verifier.called);
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  drop(active_read);

  verifier.snapshot.task_and_audit_pins_absent = false;
  let error = publisher.publish_physical_quarantine(request, &mut verifier, &mut retirement_owner).unwrap_err();
  assert_eq!(error.code(), "quarantine_publication_authority_changed");
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert!(publisher.locator(&prepared.control.key).unwrap().is_none());
  assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.prior_manifest_key);
  verifier.called = false;
  verifier.snapshot.task_and_audit_pins_absent = true;

  let receipt = publisher.publish_physical_quarantine(request, &mut verifier, &mut retirement_owner).unwrap();

  assert!(verifier.called);
  assert!(!receipt.idempotent);
  assert_eq!(receipt.quarantine_control_slot, 1);
  assert!(receipt.quarantine_manifest_write_sequence < receipt.quarantine_control_write_sequence);
  assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.manifest.key);
  assert!(coordinator.snapshot().unwrap().hard_frontier > before_frontier);

  let before_retry = publisher.observe().unwrap();
  verifier.called = false;
  verifier.fail = true;
  let retry = publisher.publish_physical_quarantine(request, &mut verifier, &mut retirement_owner).unwrap();
  assert!(retry.idempotent);
  assert!(!verifier.called);
  assert_eq!(publisher.observe().unwrap(), before_retry);
}

#[test]
fn sweep_proposal_hard_publication_requires_the_exact_selected_quarantine_and_is_retry_safe() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("sweep-proposal-publication", None);
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    501,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();

  let empty = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
  let mut empty_verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: empty.prior_manifest_key.clone(),
    expected_next_manifest_hash: empty.manifest.key.clone(),
    expected_request: empty.authority_snapshot.clone(),
    snapshot: empty.authority_snapshot.clone(),
  };
  let _empty_receipt =
    publisher.publish_physical_quarantine(empty.request(&cancellation), &mut empty_verifier, &mut retirement_owner).unwrap();

  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&empty.lifecycle_manifest.value, algorithm).unwrap() else {
    panic!("root lifecycle support must decode as a manifest")
  };
  let empty_manifest = decode_quarantine_manifest_v1(&empty.manifest.value, algorithm).unwrap();
  let candidate_authority = digest_parts(algorithm, &[b"candidate quarantine authority roots"]);
  let candidate_semantic = digest_parts(algorithm, &[b"candidate quarantine semantic state"]);
  let candidate_layout = digest_parts(algorithm, &[b"candidate quarantine KV layout"]);
  let candidate_mark = digest_parts(algorithm, &[b"candidate quarantine mark result"]);
  let candidate_completed_at_ms = 1_700_000_080_000;
  let mut candidate_model = PhysicalQuarantineTransitionModelV1::new(
    PhysicalQuarantineTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_manifest: &empty_manifest,
      mark_generation: 102,
      completed_at_ms: candidate_completed_at_ms,
      current_configured_grace_ms: 0,
      authority_root_set_digest: &candidate_authority,
      semantic_state_digest: &candidate_semantic,
      kv_layout_fingerprint: &candidate_layout,
      mark_result_digest: &candidate_mark,
      captured_root_lifecycle_manifest: &empty.lifecycle_manifest.key,
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
  let logical_key = digest_parts(algorithm, &[b"eligible candidate logical key"]);
  let integrity = digest_parts(algorithm, &[b"eligible candidate integrity"]);
  let incarnation = PhysicalIncarnationV1 {
    logical_key: &logical_key,
    integrity_or_legacy_digest: &integrity,
    wal_offset: 8_192,
    write_sequence: 77,
    entity_length: 512,
    entry_type: 1,
    entity_version: 1,
  };
  let PhysicalQuarantineTransitionV1::CandidateStarted(candidate) = candidate_model
    .observe(PhysicalQuarantineObservationV1 {
      incarnation,
      prior_candidate: None,
      reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
        class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
      },
    })
    .unwrap()
  else {
    panic!("the first complete unreachable mark must start a candidate")
  };
  let candidate_records = [candidate.as_delta_write_request()];
  let candidate_delta = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    mark_generation: 102,
    delta_ordinal: 1,
    previous_delta_hash: None,
    records: &candidate_records,
  })
  .unwrap();
  publisher
    .publish_physical_quarantine_support_artifact(PhysicalQuarantineSupportPublicationRequestV1 {
      database_id: &database_id,
      artifact: &candidate_delta,
      publication_timestamp_ms: candidate_completed_at_ms,
    })
    .unwrap();
  let candidate_transition = candidate_model.finish_for_publication().unwrap();
  let candidate_record_bytes = u64::try_from(52 + 2 * algorithm.hash_length()).unwrap();
  let candidate_manifest_artifact = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    mark_generation: 102,
    completed_at_ms: candidate_completed_at_ms,
    required_capabilities: empty_manifest.required_capabilities,
    authority_root_set_digest: &candidate_authority,
    semantic_state_digest: &candidate_semantic,
    kv_layout_fingerprint: &candidate_layout,
    mark_result_digest: &candidate_mark,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: &empty.lifecycle_manifest.key,
    candidate_count: 1,
    candidate_bytes: candidate_record_bytes,
    eligible_count_hint: 0,
    eligible_bytes_hint: 0,
    next_candidate_page_id: 1,
    delta_hashes: &candidate_delta.key,
  })
  .unwrap();
  let candidate_manifest = decode_quarantine_manifest_v1(&candidate_manifest_artifact.value, algorithm).unwrap();
  let mut candidate_closure = QuarantineClosureValidatorV1::new(
    &candidate_manifest,
    None,
    &lifecycle,
    algorithm,
    cancellation.clone(),
    QuarantineClosureLimitsV1 { maximum_support_artifacts: 2 },
    &memory,
  )
  .unwrap();
  candidate_closure.observe_delta(&candidate_delta.value).unwrap();
  let candidate_closure = candidate_closure.finish().unwrap();
  let candidate_permit = qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
    prior_manifest: &empty_manifest,
    next_manifest: &candidate_manifest,
    support_closure: &candidate_closure,
    transition: &candidate_transition,
    appended_delta: Some(&candidate_delta.value),
    cancellation: &cancellation,
  })
  .unwrap();
  let candidate_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::QuarantineActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 0,
    sequence: 3,
    generation: 102,
    target_manifest_hash: &candidate_manifest_artifact.key,
  })
  .unwrap();
  let candidate_snapshot = PhysicalQuarantineAuthoritySnapshotV1 {
    selected_complete_mark_generation: 102,
    authority_root_set_digest: candidate_authority,
    semantic_state_digest: candidate_semantic,
    kv_layout_fingerprint: candidate_layout,
    mark_result_digest: candidate_mark,
    selected_root_lifecycle_manifest: empty.lifecycle_manifest.key.clone(),
    physical_inventory_and_lineage_complete: true,
    all_candidate_incarnations_exact_and_unreachable: true,
    task_and_audit_pins_absent: true,
  };
  let candidate_prepared = PreparedGuardedPhysicalQuarantineV1 {
    permit: candidate_permit,
    manifest: candidate_manifest_artifact.clone(),
    control: candidate_control,
    lifecycle_manifest: empty.lifecycle_manifest.clone(),
    pin_coordinator: empty.pin_coordinator.clone(),
    authority_snapshot: candidate_snapshot.clone(),
    prior_manifest_key: empty.manifest.key.clone(),
    publication_timestamp_ms: candidate_completed_at_ms + 1,
  };
  let mut candidate_verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: candidate_prepared.prior_manifest_key.clone(),
    expected_next_manifest_hash: candidate_prepared.manifest.key.clone(),
    expected_request: candidate_snapshot.clone(),
    snapshot: candidate_snapshot,
  };
  let _candidate_receipt = publisher
    .publish_physical_quarantine(candidate_prepared.request(&cancellation), &mut candidate_verifier, &mut retirement_owner)
    .unwrap();

  let prior_candidate_bytes = encode_physical_quarantine_candidate_v1(&candidate.as_write_request()).unwrap();
  let prior_candidate = decode_physical_quarantine_candidate_v1(&prior_candidate_bytes, algorithm, false).unwrap();
  let eligible_authority = digest_parts(algorithm, &[b"eligible quarantine authority roots"]);
  let eligible_semantic = digest_parts(algorithm, &[b"eligible quarantine semantic state"]);
  let eligible_layout = digest_parts(algorithm, &[b"eligible quarantine KV layout"]);
  let eligible_mark = digest_parts(algorithm, &[b"eligible quarantine mark result"]);
  let eligible_completed_at_ms = candidate_completed_at_ms + 1;
  let mut eligible_model = PhysicalQuarantineTransitionModelV1::new(
    PhysicalQuarantineTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_manifest: &candidate_manifest,
      mark_generation: 103,
      completed_at_ms: eligible_completed_at_ms,
      current_configured_grace_ms: 0,
      authority_root_set_digest: &eligible_authority,
      semantic_state_digest: &eligible_semantic,
      kv_layout_fingerprint: &eligible_layout,
      mark_result_digest: &eligible_mark,
      captured_root_lifecycle_manifest: &empty.lifecycle_manifest.key,
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
  let PhysicalQuarantineTransitionV1::SweepEligible(intent) = eligible_model
    .observe(PhysicalQuarantineObservationV1 {
      incarnation: prior_candidate.incarnation,
      prior_candidate: Some(&prior_candidate),
      reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable { class: prior_candidate.class },
    })
    .unwrap()
  else {
    panic!("the second complete unreachable mark must emit one exact sweep intent")
  };
  let eligible_transition = eligible_model.finish_for_publication().unwrap();
  let eligible_manifest_artifact = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    mark_generation: 103,
    completed_at_ms: eligible_completed_at_ms,
    required_capabilities: candidate_manifest.required_capabilities,
    authority_root_set_digest: &eligible_authority,
    semantic_state_digest: &eligible_semantic,
    kv_layout_fingerprint: &eligible_layout,
    mark_result_digest: &eligible_mark,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: &empty.lifecycle_manifest.key,
    candidate_count: 1,
    candidate_bytes: candidate_record_bytes,
    eligible_count_hint: 1,
    eligible_bytes_hint: candidate_record_bytes,
    next_candidate_page_id: 1,
    delta_hashes: &candidate_delta.key,
  })
  .unwrap();
  let eligible_manifest = decode_quarantine_manifest_v1(&eligible_manifest_artifact.value, algorithm).unwrap();
  let mut eligible_closure = QuarantineClosureValidatorV1::new(
    &eligible_manifest,
    None,
    &lifecycle,
    algorithm,
    cancellation.clone(),
    QuarantineClosureLimitsV1 { maximum_support_artifacts: 2 },
    &memory,
  )
  .unwrap();
  eligible_closure.observe_delta(&candidate_delta.value).unwrap();
  let eligible_closure = eligible_closure.finish().unwrap();
  let eligible_permit = qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
    prior_manifest: &candidate_manifest,
    next_manifest: &eligible_manifest,
    support_closure: &eligible_closure,
    transition: &eligible_transition,
    appended_delta: None,
    cancellation: &cancellation,
  })
  .unwrap();
  let batch_id = [0x91; 16];
  let sweep_permit = qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
    quarantine_publication: &eligible_permit,
    quarantine_manifest: &eligible_manifest,
    batch_id: &batch_id,
    created_at_ms: i64::try_from(eligible_completed_at_ms + 1).unwrap(),
    intents: std::slice::from_ref(&intent),
    cancellation: &cancellation,
  })
  .unwrap();
  let publication_request = SweepProposalHardPublicationRequestV1 {
    permit: &sweep_permit,
    publication_timestamp_ms: eligible_completed_at_ms + 1,
    cancellation: &cancellation,
  };
  let error = publisher.publish_sweep_proposal(publication_request).unwrap_err();
  assert_eq!(error.code(), "sweep_proposal_publication_quarantine_changed");
  assert!(publisher.locator(&sweep_permit.proposal().key).unwrap().is_none());

  let eligible_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::QuarantineActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 1,
    sequence: 4,
    generation: 103,
    target_manifest_hash: &eligible_manifest_artifact.key,
  })
  .unwrap();
  let eligible_snapshot = PhysicalQuarantineAuthoritySnapshotV1 {
    selected_complete_mark_generation: 103,
    authority_root_set_digest: eligible_authority,
    semantic_state_digest: eligible_semantic,
    kv_layout_fingerprint: eligible_layout,
    mark_result_digest: eligible_mark,
    selected_root_lifecycle_manifest: empty.lifecycle_manifest.key.clone(),
    physical_inventory_and_lineage_complete: true,
    all_candidate_incarnations_exact_and_unreachable: true,
    task_and_audit_pins_absent: true,
  };
  let eligible_prepared = PreparedGuardedPhysicalQuarantineV1 {
    permit: eligible_permit,
    manifest: eligible_manifest_artifact,
    control: eligible_control,
    lifecycle_manifest: empty.lifecycle_manifest,
    pin_coordinator: empty.pin_coordinator,
    authority_snapshot: eligible_snapshot.clone(),
    prior_manifest_key: candidate_prepared.manifest.key,
    publication_timestamp_ms: eligible_completed_at_ms + 1,
  };
  let mut eligible_verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: eligible_prepared.prior_manifest_key.clone(),
    expected_next_manifest_hash: eligible_prepared.manifest.key.clone(),
    expected_request: eligible_snapshot.clone(),
    snapshot: eligible_snapshot,
  };
  let _eligible_receipt =
    publisher.publish_physical_quarantine(eligible_prepared.request(&cancellation), &mut eligible_verifier, &mut retirement_owner).unwrap();

  let selected_before = selected_physical_quarantine_manifest_key(&publisher);
  let original_manifest_locator = publisher.locator(&eligible_prepared.manifest.key).unwrap().unwrap();
  let mut corrupted_manifest_locator = original_manifest_locator.clone();
  corrupted_manifest_locator.type_flags = KV_TYPE_CHUNK;
  publisher.kv.lock().unwrap().insert(corrupted_manifest_locator).unwrap();
  let error = publisher.publish_sweep_proposal(publication_request).unwrap_err();
  assert_eq!(error.code(), "quarantine_publication_manifest_collision");
  assert!(publisher.locator(&sweep_permit.proposal().key).unwrap().is_none());
  publisher.kv.lock().unwrap().insert(original_manifest_locator).unwrap();

  let first = publisher.publish_sweep_proposal(publication_request).unwrap();
  let retry = publisher.publish_sweep_proposal(publication_request).unwrap();
  assert_eq!(first, retry);
  assert_eq!(first.proposal_key, sweep_permit.proposal().key);
  assert!(publisher.locator(&first.proposal_key).unwrap().is_some());
  assert_eq!(selected_physical_quarantine_manifest_key(&publisher), selected_before);

  let removal_request = SweepLocatorRemovalRequestV1 {
    permit: &sweep_permit,
    hard_publication: &first,
    cancellation: &cancellation,
    pin_coordinator: &eligible_prepared.pin_coordinator,
  };
  let reclaimed = SweepLocatorRemovalOutcomeV1 {
    ordinal: 0,
    outcome: SweepOutcomeClassV1::Reclaimed,
    stable_reason_detail: 0,
    resulting_void_offset: 8_192,
    resulting_void_length: 512,
  };

  let active_root = digest_parts(algorithm, &[b"active read while sweep removal waits"]);
  let active_read =
    eligible_prepared.pin_coordinator.admit_read(&active_root, &cancellation, || Ok(RootLifecycleObservationV1::Live)).unwrap();
  let mut pinned_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  let error = publisher.execute_sweep_locator_removals(removal_request, &mut pinned_authority).unwrap_err();
  assert_eq!(error.code(), "request_pinned");
  assert_eq!(pinned_authority.recheck_calls, 0);
  assert_eq!(pinned_authority.remove_calls, 0);
  drop(active_read);

  let recheck_entered = Arc::new(Barrier::new(2));
  let recheck_release = Arc::new(Barrier::new(2));
  let mut racing_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  racing_authority.recheck_barriers = Some((recheck_entered.clone(), recheck_release.clone()));
  let read_started = Arc::new(Barrier::new(2));
  let (lifecycle_callback_sender, lifecycle_callback_receiver) = mpsc::channel();
  std::thread::scope(|scope| {
    let removal = scope.spawn(|| publisher.execute_sweep_locator_removals(removal_request, &mut racing_authority));
    recheck_entered.wait();

    let pin_coordinator = eligible_prepared.pin_coordinator.clone();
    let read_started_thread = read_started.clone();
    let read_cancellation = CancellationToken::new();
    let read = scope.spawn(move || {
      read_started_thread.wait();
      pin_coordinator.admit_read(&digest_parts(algorithm, &[b"racing sweep removal read"]), &read_cancellation, || {
        lifecycle_callback_sender.send(()).unwrap();
        Ok(RootLifecycleObservationV1::Live)
      })
    });
    read_started.wait();
    assert!(
      matches!(lifecycle_callback_receiver.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)),
      "a new read reached lifecycle admission while sweep removal held global exclusion"
    );

    recheck_release.wait();
    drop(removal.join().unwrap().unwrap());
    lifecycle_callback_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    drop(read.join().unwrap().unwrap());
  });
  assert_eq!(racing_authority.recheck_calls, 1);
  assert_eq!(racing_authority.remove_calls, 1);
  assert_eq!(eligible_prepared.pin_coordinator.active_pin_count().unwrap(), 0);

  let constrained_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap()));
  let constrained_pins = RootReadPinCoordinatorV1::new(constrained_memory, algorithm, 1, 1).unwrap();
  let constrained_request = SweepLocatorRemovalRequestV1 { pin_coordinator: &constrained_pins, ..removal_request };
  let mut constrained_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  let error = publisher.execute_sweep_locator_removals(constrained_request, &mut constrained_authority).unwrap_err();
  assert_eq!(error.code(), "sweep_removal_memory");
  assert_eq!(constrained_authority.recheck_calls, 0);

  for case in [
    "manifest",
    "generation",
    "lifecycle",
    "grace",
    "incarnation",
    "locator",
    "lineage",
    "range",
    "request-pins",
    "pins",
    "policy",
    "repair",
  ] {
    let mut stale_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
    match case {
      "manifest" => stale_authority.snapshot.selected_quarantine_manifest_hash[0] ^= 0xFF,
      "generation" => stale_authority.snapshot.selected_mark_generation += 1,
      "lifecycle" => stale_authority.snapshot.lifecycle_current = false,
      "grace" => stale_authority.snapshot.all_candidates_still_grace_eligible = false,
      "incarnation" => stale_authority.snapshot.all_candidate_incarnations_exact_and_unreachable = false,
      "locator" => stale_authority.snapshot.all_locator_and_replacement_states_match = false,
      "lineage" => stale_authority.snapshot.replacement_lineage_complete = false,
      "range" => stale_authority.snapshot.all_physical_ranges_valid = false,
      "request-pins" => stale_authority.snapshot.request_pin_coordinator_current = false,
      "pins" => stale_authority.snapshot.task_and_audit_pins_absent = false,
      "policy" => stale_authority.snapshot.protected_family_policy_allows = false,
      "repair" => stale_authority.snapshot.repair_latch_clear = false,
      _ => unreachable!(),
    }
    let error = publisher.execute_sweep_locator_removals(removal_request, &mut stale_authority).unwrap_err();
    assert_eq!(error.code(), "sweep_removal_authority_changed", "case {case}");
    assert_eq!(stale_authority.recheck_calls, 1, "case {case}");
    assert_eq!(stale_authority.remove_calls, 0, "case {case}");
  }

  let mut refused_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  refused_authority.fail_recheck = true;
  let error = publisher.execute_sweep_locator_removals(removal_request, &mut refused_authority).unwrap_err();
  assert_eq!(error.code(), "sweep_removal_test_recheck");
  assert_eq!(refused_authority.remove_calls, 0);

  let cancellation_after_recheck = CancellationToken::new();
  let cancellation_request = SweepLocatorRemovalRequestV1 { cancellation: &cancellation_after_recheck, ..removal_request };
  let mut canceling_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  canceling_authority.cancel_during_recheck = true;
  let error = publisher.execute_sweep_locator_removals(cancellation_request, &mut canceling_authority).unwrap_err();
  assert_eq!(error.code(), "sweep_removal_canceled");
  assert_eq!(canceling_authority.recheck_calls, 1);
  assert_eq!(canceling_authority.remove_calls, 0);

  let pre_canceled = CancellationToken::new();
  pre_canceled.cancel();
  let pre_canceled_request = SweepLocatorRemovalRequestV1 { cancellation: &pre_canceled, ..removal_request };
  let mut pre_canceled_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  let error = publisher.execute_sweep_locator_removals(pre_canceled_request, &mut pre_canceled_authority).unwrap_err();
  assert_eq!(error.code(), "sweep_removal_canceled");
  assert_eq!(pre_canceled_authority.recheck_calls, 0);
  assert_eq!(pre_canceled_authority.remove_calls, 0);

  for (case, outcomes, expected_code) in [
    ("missing", vec![], "sweep_removal_outcome_count"),
    ("extra", vec![reclaimed, reclaimed], "sweep_removal_outcome_count"),
    ("order", vec![SweepLocatorRemovalOutcomeV1 { ordinal: 1, ..reclaimed }], "sweep_removal_outcome_order"),
    ("reclaimed-reason", vec![SweepLocatorRemovalOutcomeV1 { stable_reason_detail: 1, ..reclaimed }], "sweep_removal_outcome_shape"),
    ("reclaimed-offset", vec![SweepLocatorRemovalOutcomeV1 { resulting_void_offset: 8_193, ..reclaimed }], "sweep_removal_outcome_shape"),
    ("reclaimed-length", vec![SweepLocatorRemovalOutcomeV1 { resulting_void_length: 511, ..reclaimed }], "sweep_removal_outcome_shape"),
    (
      "skipped-reason",
      vec![SweepLocatorRemovalOutcomeV1 {
        outcome: SweepOutcomeClassV1::SkippedChanged,
        resulting_void_offset: 0,
        resulting_void_length: 0,
        ..reclaimed
      }],
      "sweep_removal_outcome_shape",
    ),
    (
      "skipped-extent",
      vec![SweepLocatorRemovalOutcomeV1 { outcome: SweepOutcomeClassV1::SkippedChanged, stable_reason_detail: 1, ..reclaimed }],
      "sweep_removal_outcome_shape",
    ),
  ] {
    let mut malformed_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, outcomes);
    let error = publisher.execute_sweep_locator_removals(removal_request, &mut malformed_authority).unwrap_err();
    assert_eq!(error.code(), expected_code, "case {case}");
    assert_eq!(malformed_authority.remove_calls, 1, "case {case}");
  }

  let wrong_publication = SweepProposalHardPublicationReceiptV1 {
    proposal_key: first.proposal_key.clone(),
    hard_publication_sequence: first.hard_publication_sequence + 1,
  };
  let wrong_publication_request = SweepLocatorRemovalRequestV1 { hard_publication: &wrong_publication, ..removal_request };
  let mut wrong_publication_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  let error = publisher.execute_sweep_locator_removals(wrong_publication_request, &mut wrong_publication_authority).unwrap_err();
  assert_eq!(error.code(), "sweep_removal_proposal_changed");
  assert_eq!(wrong_publication_authority.recheck_calls, 0);

  let original_proposal_locator = publisher.locator(&first.proposal_key).unwrap().unwrap();
  let mut corrupt_proposal_locator = original_proposal_locator.clone();
  corrupt_proposal_locator.type_flags = KV_TYPE_CHUNK;
  publisher.kv.lock().unwrap().insert(corrupt_proposal_locator).unwrap();
  let mut corrupt_proposal_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  let error = publisher.execute_sweep_locator_removals(removal_request, &mut corrupt_proposal_authority).unwrap_err();
  assert_eq!(error.code(), "sweep_removal_proposal_collision");
  assert_eq!(corrupt_proposal_authority.recheck_calls, 0);
  publisher.kv.lock().unwrap().insert(original_proposal_locator).unwrap();

  let cancellation_during_remove = CancellationToken::new();
  let during_remove_request = SweepLocatorRemovalRequestV1 { cancellation: &cancellation_during_remove, ..removal_request };
  let mut during_remove_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  during_remove_authority.cancel_during_remove = true;
  let canceled_completion = publisher.execute_sweep_locator_removals(during_remove_request, &mut during_remove_authority).unwrap();
  assert!(cancellation_during_remove.is_cancelled());
  assert_eq!(canceled_completion.outcomes(), &[reclaimed]);
  drop(canceled_completion);

  let observation_before_removal = publisher.observe().unwrap();
  let reserved_before_removal = memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes;
  let mut exact_authority = test_sweep_locator_removal_authority(&eligible_prepared.manifest.key, 103, vec![reclaimed]);
  let completion = publisher.execute_sweep_locator_removals(removal_request, &mut exact_authority).unwrap();
  assert_eq!(exact_authority.recheck_calls, 1);
  assert_eq!(exact_authority.remove_calls, 1);
  assert_eq!(exact_authority.observed_proposal_hash, first.proposal_key);
  assert_eq!(exact_authority.observed_proposal_write_sequence, first.hard_publication_sequence);
  assert_eq!(exact_authority.observed_candidate_count, 1);
  assert_eq!(completion.hash_algorithm(), algorithm);
  assert_eq!(completion.database_id(), database_id);
  assert_eq!(completion.batch_id(), batch_id);
  assert_eq!(completion.generation(), 103);
  assert_eq!(completion.proposal_hash(), sweep_permit.proposal().key);
  assert_eq!(completion.proposal_write_sequence(), first.hard_publication_sequence);
  assert_eq!(completion.quarantine_manifest_hash(), eligible_prepared.manifest.key);
  assert_eq!(completion.outcomes(), &[reclaimed]);
  assert_eq!(publisher.observe().unwrap(), observation_before_removal);
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes > reserved_before_removal);

  let void_catalog_hash = digest_parts(algorithm, &[b"selected Void catalog for completed sweep"]);
  let mut receipt_authority = test_sweep_receipt_void_authority(&void_catalog_hash, vec![reclaimed]);
  let receipt_request = SweepReceiptReconciliationRequestV1 {
    source: SweepReceiptReconciliationSourceV1::Completion(&completion),
    cancellation: &cancellation,
    memory: &memory,
  };
  let receipt = publisher.reconcile_sweep_receipt(receipt_request, &mut receipt_authority).unwrap();
  assert!(!receipt.recovered);
  assert_eq!(receipt.void_catalog_hash, void_catalog_hash);
  assert_eq!(receipt.reclaim_committed_at_ms, receipt_authority.snapshot.reclaim_committed_at_ms);
  assert!(publisher.locator(&receipt.receipt_key).unwrap().is_some());
  assert_eq!(receipt_authority.recheck_calls, 1);
  assert_eq!(receipt_authority.recovery_calls, 0);

  receipt_authority.snapshot.existing_receipt = Some(ExistingSweepReceiptAuthorityV1 {
    receipt_hash: receipt.receipt_key.clone(),
    receipt_write_sequence: receipt.hard_publication_sequence,
  });
  receipt_authority.snapshot.allocator_admission_blocked = false;
  let retry = publisher.reconcile_sweep_receipt(receipt_request, &mut receipt_authority).unwrap();
  assert_eq!(retry, receipt);

  let recovery_request = SweepReceiptReconciliationRequestV1 {
    source: SweepReceiptReconciliationSourceV1::Recovery(SweepReceiptRecoveryIdentityV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      proposal_hash: &first.proposal_key,
      proposal_write_sequence: first.hard_publication_sequence,
    }),
    cancellation: &cancellation,
    memory: &memory,
  };
  let recovered_retry = publisher.reconcile_sweep_receipt(recovery_request, &mut receipt_authority).unwrap();
  assert_eq!(recovered_retry, receipt, "recovery must reuse an existing semantically exact commit receipt");
  assert_eq!(receipt_authority.recovery_calls, 0, "an exact receipt must bypass mutable-locator reconstruction");

  drop(completion);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, reserved_before_removal,);

  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = publisher
    .publish_sweep_proposal(SweepProposalHardPublicationRequestV1 {
      permit: &sweep_permit,
      publication_timestamp_ms: eligible_completed_at_ms + 1,
      cancellation: &canceled,
    })
    .unwrap_err();
  assert_eq!(error.code(), "sweep_proposal_publication_canceled");
}

#[test]
fn physical_quarantine_rejects_every_final_authority_drift_before_selector_publication() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("physical-quarantine-authority-drift", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
  let expected = prepared.authority_snapshot.clone();
  let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
    expected_next_manifest_hash: prepared.manifest.key.clone(),
    expected_request: expected.clone(),
    snapshot: expected.clone(),
  };

  for case in ["source", "generation", "authority", "semantic", "layout", "mark", "lifecycle", "inventory", "incarnation", "task-audit"] {
    verifier.called = false;
    verifier.fail = case == "source";
    verifier.snapshot = expected.clone();
    match case {
      "source" => {}
      "generation" => verifier.snapshot.selected_complete_mark_generation += 1,
      "authority" => verifier.snapshot.authority_root_set_digest[0] ^= 0xFF,
      "semantic" => verifier.snapshot.semantic_state_digest[0] ^= 0xFF,
      "layout" => verifier.snapshot.kv_layout_fingerprint[0] ^= 0xFF,
      "mark" => verifier.snapshot.mark_result_digest[0] ^= 0xFF,
      "lifecycle" => verifier.snapshot.selected_root_lifecycle_manifest[0] ^= 0xFF,
      "inventory" => verifier.snapshot.physical_inventory_and_lineage_complete = false,
      "incarnation" => verifier.snapshot.all_candidate_incarnations_exact_and_unreachable = false,
      "task-audit" => verifier.snapshot.task_and_audit_pins_absent = false,
      _ => unreachable!(),
    }

    let error = publisher.publish_physical_quarantine(prepared.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap_err();

    let expected_code =
      if case == "source" { "quarantine_authority_source_unavailable" } else { "quarantine_publication_authority_changed" };
    assert_eq!(error.code(), expected_code, "case {case}");
    assert!(verifier.called, "case {case}");
    assert!(error.committed_receipt().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.control.key).unwrap().is_none(), "case {case}");
    assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.prior_manifest_key, "case {case}");
  }
}

#[test]
fn physical_quarantine_cancellation_corrupt_support_and_memory_pressure_never_select_new_authority() {
  for case in ["canceled", "corrupt-support", "memory-pressure"] {
    let (_directory, _path, _coordinator, mut publisher) = create_environment(&format!("physical-quarantine-{case}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let preparation_cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &preparation_cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &preparation_cancellation, &memory);
    let request_cancellation = CancellationToken::new();
    let constrained_pins = if case == "memory-pressure" {
      let constrained_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap()));
      Some(RootReadPinCoordinatorV1::new(constrained_memory, HashAlgorithm::Blake3_256, 1, 1).unwrap())
    } else {
      None
    };
    if case == "canceled" {
      request_cancellation.cancel();
    }
    if case == "corrupt-support" {
      corrupt_last_entity_byte(&publisher, &prepared.lifecycle_manifest.key);
    }
    let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
      called: false,
      fail: false,
      expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
      expected_next_manifest_hash: prepared.manifest.key.clone(),
      expected_request: prepared.authority_snapshot.clone(),
      snapshot: prepared.authority_snapshot.clone(),
    };
    let pin_coordinator = constrained_pins.as_ref().unwrap_or(&prepared.pin_coordinator);

    let error = publisher
      .publish_physical_quarantine(prepared.request_with_pins(&request_cancellation, pin_coordinator), &mut verifier, &mut retirement_owner)
      .unwrap_err();

    let expected_code = match case {
      "canceled" => "quarantine_publication_canceled",
      "corrupt-support" => "integrity_hash_mismatch",
      "memory-pressure" => "quarantine_support_memory",
      _ => unreachable!(),
    };
    assert_eq!(error.code(), expected_code, "case {case}");
    assert!(!verifier.called, "case {case}");
    assert!(error.committed_receipt().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.control.key).unwrap().is_none(), "case {case}");
    assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.prior_manifest_key, "case {case}");
  }
}

#[test]
fn corrupt_prior_quarantine_control_manifest_or_support_cannot_advance_authority() {
  for case in ["control", "manifest", "support"] {
    let (_directory, _path, _coordinator, mut publisher) = create_environment(&format!("physical-quarantine-corrupt-{case}"), None);
    let algorithm = HashAlgorithm::Blake3_256;
    let database_id = [0x31; 16];
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id,
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
    let corrupt_key = match case {
      "control" => gc_active_control_key(algorithm, GcArtifactKindV1::QuarantineActiveControl, &database_id, 0).unwrap(),
      "manifest" => prepared.prior_manifest_key.clone(),
      "support" => prepared.lifecycle_manifest.key.clone(),
      _ => unreachable!(),
    };
    corrupt_last_entity_byte(&publisher, &corrupt_key);
    let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
      called: false,
      fail: false,
      expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
      expected_next_manifest_hash: prepared.manifest.key.clone(),
      expected_request: prepared.authority_snapshot.clone(),
      snapshot: prepared.authority_snapshot.clone(),
    };

    let error = publisher.publish_physical_quarantine(prepared.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap_err();

    assert!(error.committed_receipt().is_none(), "case {case}");
    assert!(!verifier.called, "case {case}");
    assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none(), "case {case}");
    assert!(publisher.locator(&prepared.control.key).unwrap().is_none(), "case {case}");
  }
}

#[test]
fn physical_quarantine_refuses_when_selected_prior_authority_advances_after_qualification() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("physical-quarantine-prior-advance", None);
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = [0x31; 16];
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id,
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
  let desired = decode_quarantine_manifest_v1(&prepared.manifest.value, algorithm).unwrap();
  let intervening_manifest = encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    mark_generation: desired.mark_generation,
    completed_at_ms: desired.completed_at_ms + 1,
    required_capabilities: desired.required_capabilities,
    authority_root_set_digest: desired.authority_root_set_digest,
    semantic_state_digest: desired.semantic_state_digest,
    kv_layout_fingerprint: desired.kv_layout_fingerprint,
    mark_result_digest: desired.mark_result_digest,
    candidate_directory_root: desired.candidate_directory_root,
    captured_root_lifecycle_manifest: desired.captured_root_lifecycle_manifest,
    candidate_count: desired.candidate_count,
    candidate_bytes: desired.candidate_bytes,
    eligible_count_hint: desired.eligible_count_hint,
    eligible_bytes_hint: desired.eligible_bytes_hint,
    next_candidate_page_id: desired.next_candidate_page_id,
    delta_hashes: desired.delta_hashes,
  })
  .unwrap();
  publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::QuarantineManifest,
        database_id: &database_id,
        artifact_key: &intervening_manifest.key,
        value: &intervening_manifest.value,
        minimum_timestamp_ms: prepared.publication_timestamp_ms,
        committed_postcondition_code: "intervening_quarantine_manifest_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  let intervening_control = encode_gc_active_control(&GcActiveControlWriteV1 {
    kind: GcArtifactKindV1::QuarantineActiveControl,
    hash_algorithm: algorithm,
    database_id: &database_id,
    slot: 1,
    sequence: 2,
    generation: desired.mark_generation,
    target_manifest_hash: &intervening_manifest.key,
  })
  .unwrap();
  let outcome = publisher
    .publish_gc_active_control(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::QuarantineActiveControl,
        encoded_control: &intervening_control,
        publication_timestamp_ms: prepared.publication_timestamp_ms,
        monotonic_now_ms: prepared.publication_timestamp_ms,
      },
      &mut retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
  assert!(matches!(outcome, GcControlPublicationOutcomeV1::Complete(_)));
  let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
    expected_next_manifest_hash: prepared.manifest.key.clone(),
    expected_request: prepared.authority_snapshot.clone(),
    snapshot: prepared.authority_snapshot.clone(),
  };

  let error = publisher.publish_physical_quarantine(prepared.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap_err();

  assert_eq!(error.code(), "quarantine_publication_prior_authority_changed");
  assert!(!verifier.called);
  assert!(error.committed_receipt().is_none());
  assert!(publisher.locator(&prepared.manifest.key).unwrap().is_none());
  assert_eq!(selected_physical_quarantine_manifest_key(&publisher), intervening_manifest.key);
}

#[test]
fn racing_read_admission_waits_until_physical_quarantine_selection_finishes() {
  let (_directory, _path, _coordinator, mut publisher) = create_environment("physical-quarantine-racing-read", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
  let verifier_entered = Arc::new(Barrier::new(2));
  let verifier_release = Arc::new(Barrier::new(2));
  let mut verifier = BlockingPhysicalQuarantineAuthorityVerifierV1 {
    entered: verifier_entered.clone(),
    release: verifier_release.clone(),
    snapshot: prepared.authority_snapshot.clone(),
  };
  let read_started = Arc::new(Barrier::new(2));
  let (lifecycle_callback_sender, lifecycle_callback_receiver) = mpsc::channel();

  std::thread::scope(|scope| {
    let publication =
      scope.spawn(|| publisher.publish_physical_quarantine(prepared.request(&cancellation), &mut verifier, &mut retirement_owner));
    verifier_entered.wait();

    let pin_coordinator = prepared.pin_coordinator.clone();
    let read_started_thread = read_started.clone();
    let read_cancellation = CancellationToken::new();
    let read = scope.spawn(move || {
      read_started_thread.wait();
      pin_coordinator.admit_read(&digest_parts(HashAlgorithm::Blake3_256, &[b"racing quarantine read"]), &read_cancellation, || {
        lifecycle_callback_sender.send(()).unwrap();
        Ok(RootLifecycleObservationV1::Live)
      })
    });
    read_started.wait();
    assert!(
      matches!(lifecycle_callback_receiver.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)),
      "a new read reached lifecycle admission while quarantine held global exclusion"
    );

    verifier_release.wait();
    let receipt = publication.join().unwrap().unwrap();
    lifecycle_callback_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    drop(read.join().unwrap().unwrap());
    assert_eq!(receipt.quarantine_manifest_key, prepared.manifest.key);
  });

  assert_eq!(selected_physical_quarantine_manifest_key(&publisher), prepared.manifest.key);
  assert_eq!(prepared.pin_coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn every_physical_quarantine_selector_failure_restarts_as_exactly_prior_or_selected() {
  let failures = [
    FirstAuthorityFailurePoint::DataBarrier,
    FirstAuthorityFailurePoint::HeaderWriteBefore,
    FirstAuthorityFailurePoint::HeaderWriteAfter,
    FirstAuthorityFailurePoint::FullBarrier,
    FirstAuthorityFailurePoint::Verify,
  ];
  for failure in failures {
    let (_directory, path, coordinator, mut publisher) = create_environment(&format!("physical-quarantine-selector-{failure:?}"), None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
    let cancellation = CancellationToken::new();
    let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap();
    let prepared = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
    publisher = V4FirstAuthorityPublisher {
      file: publisher.file,
      kv: publisher.kv,
      header_publisher: DatabaseHeaderPublisherV4::with_io(coordinator.clone(), Arc::new(NthHeaderPublicationFaultIo::new(failure, 3))),
      root_state: publisher.root_state,
    };
    let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
      called: false,
      fail: false,
      expected_prior_manifest_hash: prepared.prior_manifest_key.clone(),
      expected_next_manifest_hash: prepared.manifest.key.clone(),
      expected_request: prepared.authority_snapshot.clone(),
      snapshot: prepared.authority_snapshot.clone(),
    };

    let error = publisher.publish_physical_quarantine(prepared.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap_err();

    assert!(verifier.called, "failure {failure:?}");
    assert!(coordinator.hard_failure().unwrap().is_some(), "failure {failure:?}");
    let selector_may_have_committed = matches!(
      failure,
      FirstAuthorityFailurePoint::HeaderWriteAfter | FirstAuthorityFailurePoint::FullBarrier | FirstAuthorityFailurePoint::Verify
    );
    if selector_may_have_committed {
      let receipt = error.committed_receipt().expect("uncertain selected quarantine authority requires an exact receipt");
      assert_eq!(receipt.quarantine_manifest_key, prepared.manifest.key, "failure {failure:?}");
      assert!(matches!(receipt.lineage_state, PhysicalQuarantineLineageStateV1::NotRequired), "failure {failure:?}");
    } else {
      assert!(error.committed_receipt().is_none(), "failure {failure:?}");
    }
    drop(retirement_owner);
    drop(publisher);

    let (_restart_coordinator, mut reopened) = reopen(&path);
    let expected_manifest = if selector_may_have_committed { &prepared.manifest.key } else { &prepared.prior_manifest_key };
    assert_eq!(&selected_physical_quarantine_manifest_key(&reopened), expected_manifest, "failure {failure:?}");
    let retry_cancellation = CancellationToken::new();
    let mut retry_owner = RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      [0x31; 16],
      1,
      401,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &retry_cancellation,
      &memory,
    )
    .unwrap();
    verifier.called = false;
    let retry = reopened.publish_physical_quarantine(prepared.request(&retry_cancellation), &mut verifier, &mut retry_owner).unwrap();
    assert_eq!(retry.idempotent, selector_may_have_committed, "failure {failure:?}");
    assert_eq!(verifier.called, !selector_may_have_committed, "failure {failure:?}");
    assert_eq!(selected_physical_quarantine_manifest_key(&reopened), prepared.manifest.key, "failure {failure:?}");
  }
}

#[test]
fn post_selector_quarantine_replacement_lineage_failure_preserves_the_exact_committed_receipt() {
  let (_directory, path, _coordinator, mut publisher) = create_environment("physical-quarantine-buffered-lineage", None);
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap()));
  let cancellation = CancellationToken::new();
  let mut retirement_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = prepare_guarded_physical_quarantine(&mut publisher, &mut retirement_owner, &cancellation, &memory);
  let mut verifier = ExactPhysicalQuarantineAuthorityVerifierV1 {
    called: false,
    fail: false,
    expected_prior_manifest_hash: first.prior_manifest_key.clone(),
    expected_next_manifest_hash: first.manifest.key.clone(),
    expected_request: first.authority_snapshot.clone(),
    snapshot: first.authority_snapshot.clone(),
  };
  let first_receipt = publisher.publish_physical_quarantine(first.request(&cancellation), &mut verifier, &mut retirement_owner).unwrap();
  assert!(!first_receipt.idempotent);
  let replacement = prepare_successor_physical_quarantine(&first, &cancellation, &memory, 102, 0, 3);
  verifier.called = false;
  verifier.expected_prior_manifest_hash = replacement.prior_manifest_key.clone();
  verifier.expected_next_manifest_hash = replacement.manifest.key.clone();
  verifier.expected_request = replacement.authority_snapshot.clone();
  verifier.snapshot = replacement.authority_snapshot.clone();
  let mut observer = CancelRetirementAfterCommitObserver { cancellation: cancellation.clone() };

  let error = publisher
    .publish_physical_quarantine_with_control_observer(
      replacement.request(&cancellation),
      &mut verifier,
      &mut retirement_owner,
      &mut observer,
    )
    .unwrap_err();

  assert_eq!(error.code(), "quarantine_publication_committed_lineage");
  let receipt = error.committed_receipt().expect("selected quarantine replacement requires an exact committed receipt");
  assert_eq!(receipt.quarantine_manifest_key, replacement.manifest.key);
  assert!(matches!(
    receipt.lineage_state,
    PhysicalQuarantineLineageStateV1::BufferedAfterFlushFailure { code: "retirement_journal_cancelled", .. }
  ));
  assert!(verifier.called);
  assert_eq!(retirement_owner.status().pending_records, 1);
  assert_eq!(selected_physical_quarantine_manifest_key(&publisher), replacement.manifest.key);
  drop(retirement_owner);
  drop(publisher);

  let (_restart_coordinator, mut reopened) = reopen(&path);
  assert_eq!(selected_physical_quarantine_manifest_key(&reopened), replacement.manifest.key);
  let retry_cancellation = CancellationToken::new();
  let mut retry_owner = RetirementJournalOwnerV1::new_chain(
    HashAlgorithm::Blake3_256,
    [0x31; 16],
    1,
    401,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &retry_cancellation,
    &memory,
  )
  .unwrap();
  verifier.called = false;
  verifier.fail = true;
  let retry = reopened.publish_physical_quarantine(replacement.request(&retry_cancellation), &mut verifier, &mut retry_owner).unwrap();
  assert!(retry.idempotent);
  assert!(!verifier.called);
}
