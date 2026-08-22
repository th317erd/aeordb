use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::durability_coordinator::{CommitClass, DurabilityCommitReceipt};
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerRewritePlanV1, ActivePointerSlotObservationV1, ActivePointerWriteV1, EncodedActivePointerV1,
  EncodedImmutableIndexArtifactV1, decode_index_manifest, encode_active_pointer, plan_active_pointer_rewrite,
};
use aeordb::engine::v4::index_generation_publication::{
  INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1, IndexGenerationBarrierStageV1,
  IndexGenerationPublicationActionV1, IndexGenerationPublicationFailureBoundaryV1, IndexGenerationPublicationLimitsV1,
  IndexGenerationPublicationMachineV1, IndexGenerationPublicationModeV1, IndexGenerationPublicationReceiptV1,
  IndexGenerationPublicationRequestV1, IndexGenerationPublicationStepReceiptV1,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use aeordb::engine::HashAlgorithm;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

#[test]
fn soft_publication_orders_dependencies_manifest_pointer_and_live_closure_without_barriers() {
  let (dependencies, manifest, pointer) = publication_inputs();
  let dependency_refs = dependencies.iter().collect::<Vec<_>>();
  let mut machine = machine(IndexGenerationPublicationModeV1::Soft, &dependency_refs, &manifest, &pointer);

  assert_eq!(machine.failure_boundary(), IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained);
  assert_dependency(machine.next_action().unwrap(), 0, &dependencies[0]);
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::ImmutablePublished {
        artifact_key: &manifest.key,
        stored_length: dependencies[0].value.len(),
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_dependency(machine.next_action().unwrap(), 0, &dependencies[0]);
  acknowledge_immutable(&mut machine, &dependencies[0]);
  assert_dependency(machine.next_action().unwrap(), 1, &dependencies[1]);
  acknowledge_immutable(&mut machine, &dependencies[1]);

  match machine.next_action().unwrap() {
    IndexGenerationPublicationActionV1::PublishManifest { artifact } => assert_eq!(artifact, &manifest),
    action => panic!("expected manifest, got {action:?}"),
  }
  acknowledge_immutable(&mut machine, &manifest);
  assert_eq!(machine.failure_boundary(), IndexGenerationPublicationFailureBoundaryV1::PointerCommitUnknown);
  match machine.next_action().unwrap() {
    IndexGenerationPublicationActionV1::PublishPointer { pointer: observed } => assert_eq!(observed, &pointer),
    action => panic!("expected pointer, got {action:?}"),
  }
  acknowledge_pointer(&mut machine, &pointer, 1);
  assert_eq!(machine.failure_boundary(), IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible);
  assert_closure(machine.next_action().unwrap(), &manifest, &pointer, 1);
  let receipt = machine
    .acknowledge(IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated {
      pointer_key: &pointer.key,
      manifest_key: &manifest.key,
      generation: 4_097,
      pointer_sequence: 1,
    })
    .unwrap()
    .unwrap();
  assert!(machine.next_action().is_none());
  match receipt {
    IndexGenerationPublicationReceiptV1::Soft { dependency_count, manifest_key, pointer_key, pointer_sequence, total_bytes } => {
      assert_eq!(dependency_count, 2);
      assert_eq!(manifest_key, manifest.key);
      assert_eq!(pointer_key, pointer.key);
      assert_eq!(pointer_sequence, 1);
      assert_eq!(total_bytes, total_bytes_for(&dependencies, &manifest, &pointer));
    }
    receipt => panic!("expected soft receipt, got {receipt:?}"),
  }
}

#[test]
fn hard_publication_requires_two_ordered_hard_barriers_and_final_reread() {
  let (dependencies, manifest, pointer) = publication_inputs();
  let dependency_refs = dependencies.iter().collect::<Vec<_>>();
  let mut machine = machine(IndexGenerationPublicationModeV1::Hard, &dependency_refs, &manifest, &pointer);
  for dependency in &dependencies {
    acknowledge_immutable(&mut machine, dependency);
  }
  acknowledge_immutable(&mut machine, &manifest);

  assert_barrier(machine.next_action().unwrap(), IndexGenerationBarrierStageV1::ImmutableClosure);
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted {
        stage: IndexGenerationBarrierStageV1::ImmutableClosure,
        receipt: durability(40, CommitClass::RecoverableSoftState),
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted {
        stage: IndexGenerationBarrierStageV1::Pointer,
        receipt: durability(40, CommitClass::HardAuthority),
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted {
        stage: IndexGenerationBarrierStageV1::ImmutableClosure,
        receipt: DurabilityCommitReceipt { sequence: 40, class: CommitClass::HardAuthority, hard_frontier: 39 },
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_barrier(machine.next_action().unwrap(), IndexGenerationBarrierStageV1::ImmutableClosure);
  acknowledge_barrier(&mut machine, IndexGenerationBarrierStageV1::ImmutableClosure, 41);
  acknowledge_pointer(&mut machine, &pointer, 1);

  assert_barrier(machine.next_action().unwrap(), IndexGenerationBarrierStageV1::Pointer);
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted {
        stage: IndexGenerationBarrierStageV1::Pointer,
        receipt: durability(41, CommitClass::HardAuthority),
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
  assert_barrier(machine.next_action().unwrap(), IndexGenerationBarrierStageV1::Pointer);
  acknowledge_barrier(&mut machine, IndexGenerationBarrierStageV1::Pointer, 42);
  assert_closure(machine.next_action().unwrap(), &manifest, &pointer, 1);
  let receipt = machine
    .acknowledge(IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated {
      pointer_key: &pointer.key,
      manifest_key: &manifest.key,
      generation: 4_097,
      pointer_sequence: 1,
    })
    .unwrap()
    .unwrap();
  match receipt {
    IndexGenerationPublicationReceiptV1::Hard { immutable_barrier_sequence, pointer_barrier_sequence, pointer_sequence, .. } => {
      assert_eq!(immutable_barrier_sequence, 41);
      assert_eq!(pointer_barrier_sequence, 42);
      assert_eq!(pointer_sequence, 1);
    }
    receipt => panic!("expected hard receipt, got {receipt:?}"),
  }
}

#[test]
fn publication_request_rejects_wrong_closure_prepared_bytes_duplicates_and_limits() {
  let (dependencies, manifest, pointer) = publication_inputs();
  let dependency_refs = dependencies.iter().collect::<Vec<_>>();
  let limits = limits();

  let wrong_target = [0xAA; 32];
  let decoded_manifest = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  let mismatched_pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: decoded_manifest.generation,
    owner_id: decoded_manifest.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &wrong_target,
  })
  .unwrap();
  assert_request_error(&dependency_refs, &manifest, &mismatched_pointer, limits, MalformedInputClass::CrossRecordClosureMismatch);

  let wrong_kind = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldNvt,
    hash_algorithm: ALGORITHM,
    generation: decoded_manifest.generation,
    owner_id: decoded_manifest.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  assert_request_error(&dependency_refs, &manifest, &wrong_kind, limits, MalformedInputClass::CrossRecordClosureMismatch);

  let duplicate_refs = vec![&dependencies[0], &dependencies[0]];
  assert_request_error(&duplicate_refs, &manifest, &pointer, limits, MalformedInputClass::NoncanonicalOrderOrDuplicate);

  let mut bad_dependency = dependencies[0].clone();
  bad_dependency.key[0] ^= 1;
  let bad_refs = vec![&bad_dependency];
  assert_request_error(&bad_refs, &manifest, &pointer, limits, MalformedInputClass::IdentityKeyOrGenerationMismatch);

  assert_request_error(
    &dependency_refs,
    &manifest,
    &pointer,
    IndexGenerationPublicationLimitsV1::new(1, INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1).unwrap(),
    MalformedInputClass::AllocationAmplification,
  );
  assert_request_error(
    &dependency_refs,
    &manifest,
    &pointer,
    IndexGenerationPublicationLimitsV1::new(INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, 1).unwrap(),
    MalformedInputClass::AllocationAmplification,
  );
  assert!(IndexGenerationPublicationLimitsV1::new(INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1 + 1, 1).is_err());
  assert!(IndexGenerationPublicationLimitsV1::new(0, INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1 + 1).is_err());
  assert!(IndexGenerationPublicationLimitsV1::new(0, 0).is_err());

  let mut bad_manifest = manifest.clone();
  bad_manifest.key[0] ^= 1;
  assert_request_error(&dependency_refs, &bad_manifest, &pointer, limits, MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let mut bad_pointer = pointer.clone();
  bad_pointer.key[0] ^= 1;
  assert_request_error(&dependency_refs, &manifest, &bad_pointer, limits, MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let exact_bytes = total_bytes_for(&dependencies, &manifest, &pointer);
  assert!(IndexGenerationPublicationMachineV1::new(request(
    IndexGenerationPublicationModeV1::Soft,
    &dependency_refs,
    &manifest,
    &pointer,
    IndexGenerationPublicationLimitsV1::new(dependency_refs.len(), exact_bytes).unwrap(),
  ))
  .is_ok());
  assert_request_error(
    &dependency_refs,
    &manifest,
    &pointer,
    IndexGenerationPublicationLimitsV1::new(dependency_refs.len(), exact_bytes - 1).unwrap(),
    MalformedInputClass::AllocationAmplification,
  );
}

#[test]
fn publication_requires_the_planned_slot_and_sequence_and_supports_empty_dependency_sets() {
  let (_dependencies, manifest, pointer) = publication_inputs();
  let empty = Vec::new();
  let empty_machine = machine(IndexGenerationPublicationModeV1::Soft, &empty, &manifest, &pointer);
  match empty_machine.next_action().unwrap() {
    IndexGenerationPublicationActionV1::PublishManifest { artifact } => assert_eq!(artifact, &manifest),
    action => panic!("expected manifest for an empty dependency set, got {action:?}"),
  }

  let decoded_manifest = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  let wrong_slot = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: decoded_manifest.generation,
    owner_id: decoded_manifest.owner_id,
    slot: 1,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  assert_request_error(&empty, &manifest, &wrong_slot, limits(), MalformedInputClass::CrossRecordClosureMismatch);

  let wrong_sequence = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: decoded_manifest.generation,
    owner_id: decoded_manifest.owner_id,
    slot: 0,
    sequence: 2,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  assert_request_error(&empty, &manifest, &wrong_sequence, limits(), MalformedInputClass::CrossRecordClosureMismatch);

  let decoded_manifest = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  let foreign_kind_plan = plan_active_pointer_rewrite(
    ActivePointerKindV1::FieldNvt,
    decoded_manifest.owner_id,
    ActivePointerSlotObservationV1::Missing,
    ActivePointerSlotObservationV1::Missing,
  )
  .unwrap();
  assert_eq!(
    IndexGenerationPublicationMachineV1::new(IndexGenerationPublicationRequestV1 {
      rewrite_plan: foreign_kind_plan,
      ..request(IndexGenerationPublicationModeV1::Soft, &empty, &manifest, &pointer, limits())
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let foreign_owner = [0xAB; 32];
  let foreign_owner_plan = plan_active_pointer_rewrite(
    ActivePointerKindV1::FieldIndex,
    &foreign_owner,
    ActivePointerSlotObservationV1::Missing,
    ActivePointerSlotObservationV1::Missing,
  )
  .unwrap();
  assert_eq!(
    IndexGenerationPublicationMachineV1::new(IndexGenerationPublicationRequestV1 {
      rewrite_plan: foreign_owner_plan,
      ..request(IndexGenerationPublicationModeV1::Soft, &empty, &manifest, &pointer, limits())
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn every_pointer_family_closes_over_its_exact_manifest_kind_at_both_hash_widths() {
  let cases = [
    (HashAlgorithm::Blake3_256, "aidx-blake3-256-field-index-manifest-empty.bin", ActivePointerKindV1::FieldIndex),
    (HashAlgorithm::Blake3_256, "aidx-blake3-256-field-nvt-manifest-empty.bin", ActivePointerKindV1::FieldNvt),
    (HashAlgorithm::Blake3_256, "aidx-blake3-256-scope-catalog-manifest-empty.bin", ActivePointerKindV1::ScopeCatalog),
    (HashAlgorithm::Sha512, "aidx-sha512-field-index-manifest-empty.bin", ActivePointerKindV1::FieldIndex),
    (HashAlgorithm::Sha512, "aidx-sha512-field-nvt-manifest-empty.bin", ActivePointerKindV1::FieldNvt),
    (HashAlgorithm::Sha512, "aidx-sha512-scope-catalog-manifest-empty.bin", ActivePointerKindV1::ScopeCatalog),
  ];
  let dependencies = Vec::new();
  for (algorithm, fixture, kind) in cases {
    let manifest = immutable_fixture(fixture);
    let decoded_manifest = decode_index_manifest(&manifest.value, algorithm).unwrap();
    let pointer = encode_active_pointer(&ActivePointerWriteV1 {
      kind,
      hash_algorithm: algorithm,
      generation: decoded_manifest.generation,
      owner_id: decoded_manifest.owner_id,
      slot: 0,
      sequence: 1,
      target_manifest_hash: &manifest.key,
    })
    .unwrap();
    let rewrite_plan = plan_active_pointer_rewrite(
      kind,
      decoded_manifest.owner_id,
      ActivePointerSlotObservationV1::Missing,
      ActivePointerSlotObservationV1::Missing,
    )
    .unwrap();
    assert!(IndexGenerationPublicationMachineV1::new(IndexGenerationPublicationRequestV1 {
      mode: IndexGenerationPublicationModeV1::Soft,
      hash_algorithm: algorithm,
      dependencies: &dependencies,
      manifest: &manifest,
      pointer: &pointer,
      rewrite_plan,
      limits: limits(),
    })
    .is_ok());
  }

  let manifest = immutable_fixture("aidx-blake3-256-value-store-manifest-empty.bin");
  let decoded_manifest = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: decoded_manifest.generation,
    owner_id: decoded_manifest.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  assert_eq!(
    IndexGenerationPublicationMachineV1::new(IndexGenerationPublicationRequestV1 {
      mode: IndexGenerationPublicationModeV1::Soft,
      hash_algorithm: ALGORITHM,
      dependencies: &dependencies,
      manifest: &manifest,
      pointer: &pointer,
      rewrite_plan: plan_active_pointer_rewrite(
        ActivePointerKindV1::FieldIndex,
        decoded_manifest.owner_id,
        ActivePointerSlotObservationV1::Missing,
        ActivePointerSlotObservationV1::Missing,
      )
      .unwrap(),
      limits: limits(),
    })
    .unwrap_err()
    .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn step_receipts_are_exact_and_failure_boundaries_never_claim_unproven_authority() {
  let (dependencies, manifest, pointer) = publication_inputs();
  let dependency_refs = dependencies.iter().collect::<Vec<_>>();
  let mut machine = machine(IndexGenerationPublicationModeV1::Soft, &dependency_refs, &manifest, &pointer);
  for dependency in &dependencies {
    acknowledge_immutable(&mut machine, dependency);
  }
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::ImmutablePublished {
        artifact_key: &manifest.key,
        stored_length: manifest.value.len() + 1,
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  acknowledge_immutable(&mut machine, &manifest);
  assert_eq!(machine.failure_boundary(), IndexGenerationPublicationFailureBoundaryV1::PointerCommitUnknown);
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::ActivePointerPublished {
        pointer_key: &pointer.key,
        stored_length: pointer.value.len(),
        pointer_sequence: 2,
        generation: 4_097,
        target_manifest_hash: &manifest.key,
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::ActivePointerPublished {
        pointer_key: &pointer.key,
        stored_length: pointer.value.len(),
        pointer_sequence: 1,
        generation: 4_098,
        target_manifest_hash: &manifest.key,
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  let wrong_target = [0xCD; 32];
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::ActivePointerPublished {
        pointer_key: &pointer.key,
        stored_length: pointer.value.len(),
        pointer_sequence: 1,
        generation: 4_097,
        target_manifest_hash: &wrong_target,
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  acknowledge_pointer(&mut machine, &pointer, 1);
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated {
        pointer_key: &pointer.key,
        manifest_key: &manifest.key,
        generation: 4_098,
        pointer_sequence: 1,
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert_eq!(machine.failure_boundary(), IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible);
  assert_closure(machine.next_action().unwrap(), &manifest, &pointer, 1);
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated {
        pointer_key: &pointer.key,
        manifest_key: &manifest.key,
        generation: 4_097,
        pointer_sequence: 2,
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
  assert!(machine
    .acknowledge(IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated {
      pointer_key: &pointer.key,
      manifest_key: &manifest.key,
      generation: 4_097,
      pointer_sequence: 1,
    })
    .unwrap()
    .is_some());
  assert_eq!(
    machine
      .acknowledge(IndexGenerationPublicationStepReceiptV1::SelectedClosureValidated {
        pointer_key: &pointer.key,
        manifest_key: &manifest.key,
        generation: 4_097,
        pointer_sequence: 1,
      })
      .unwrap_err()
      .class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );
}

fn machine<'a>(
  mode: IndexGenerationPublicationModeV1,
  dependencies: &'a [&'a EncodedImmutableIndexArtifactV1],
  manifest: &'a EncodedImmutableIndexArtifactV1,
  pointer: &'a EncodedActivePointerV1,
) -> IndexGenerationPublicationMachineV1<'a> {
  IndexGenerationPublicationMachineV1::new(request(mode, dependencies, manifest, pointer, limits())).unwrap()
}

fn request<'a>(
  mode: IndexGenerationPublicationModeV1,
  dependencies: &'a [&'a EncodedImmutableIndexArtifactV1],
  manifest: &'a EncodedImmutableIndexArtifactV1,
  pointer: &'a EncodedActivePointerV1,
  limits: IndexGenerationPublicationLimitsV1,
) -> IndexGenerationPublicationRequestV1<'a> {
  IndexGenerationPublicationRequestV1 {
    mode,
    hash_algorithm: ALGORITHM,
    dependencies,
    manifest,
    pointer,
    rewrite_plan: rewrite_plan(manifest),
    limits,
  }
}

fn rewrite_plan(manifest: &EncodedImmutableIndexArtifactV1) -> ActivePointerRewritePlanV1<'_> {
  let manifest = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  plan_active_pointer_rewrite(
    ActivePointerKindV1::FieldIndex,
    manifest.owner_id,
    ActivePointerSlotObservationV1::Missing,
    ActivePointerSlotObservationV1::Missing,
  )
  .unwrap()
}

fn limits() -> IndexGenerationPublicationLimitsV1 {
  IndexGenerationPublicationLimitsV1::new(8, 1024 * 1024).unwrap()
}

fn acknowledge_immutable(machine: &mut IndexGenerationPublicationMachineV1<'_>, artifact: &EncodedImmutableIndexArtifactV1) {
  assert!(machine
    .acknowledge(IndexGenerationPublicationStepReceiptV1::ImmutablePublished {
      artifact_key: &artifact.key,
      stored_length: artifact.value.len(),
    })
    .unwrap()
    .is_none());
}

fn acknowledge_pointer(machine: &mut IndexGenerationPublicationMachineV1<'_>, pointer: &EncodedActivePointerV1, sequence: u64) {
  let decoded = aeordb::engine::v4::index_artifact::decode_active_pointer(&pointer.value, ALGORITHM).unwrap();
  assert!(machine
    .acknowledge(IndexGenerationPublicationStepReceiptV1::ActivePointerPublished {
      pointer_key: &pointer.key,
      stored_length: pointer.value.len(),
      pointer_sequence: sequence,
      generation: decoded.generation,
      target_manifest_hash: decoded.target_manifest_hash,
    })
    .unwrap()
    .is_none());
}

fn acknowledge_barrier(machine: &mut IndexGenerationPublicationMachineV1<'_>, stage: IndexGenerationBarrierStageV1, sequence: u64) {
  assert!(machine
    .acknowledge(IndexGenerationPublicationStepReceiptV1::DurabilityBarrierCompleted {
      stage,
      receipt: durability(sequence, CommitClass::HardAuthority),
    })
    .unwrap()
    .is_none());
}

fn assert_dependency(action: IndexGenerationPublicationActionV1<'_>, ordinal: usize, expected: &EncodedImmutableIndexArtifactV1) {
  match action {
    IndexGenerationPublicationActionV1::PublishDependency { ordinal: observed, artifact } => {
      assert_eq!(observed, ordinal);
      assert_eq!(artifact, expected);
    }
    action => panic!("expected dependency, got {action:?}"),
  }
}

fn assert_barrier(action: IndexGenerationPublicationActionV1<'_>, expected: IndexGenerationBarrierStageV1) {
  match action {
    IndexGenerationPublicationActionV1::DurabilityBarrier { stage } => assert_eq!(stage, expected),
    action => panic!("expected durability barrier, got {action:?}"),
  }
}

fn assert_closure(
  action: IndexGenerationPublicationActionV1<'_>,
  manifest: &EncodedImmutableIndexArtifactV1,
  pointer: &EncodedActivePointerV1,
  pointer_sequence: u64,
) {
  match action {
    IndexGenerationPublicationActionV1::ValidateSelectedClosure {
      manifest: observed_manifest,
      pointer: observed_pointer,
      pointer_sequence: observed_sequence,
    } => {
      assert_eq!(observed_manifest, manifest);
      assert_eq!(observed_pointer, pointer);
      assert_eq!(observed_sequence, pointer_sequence);
    }
    action => panic!("expected selected closure validation, got {action:?}"),
  }
}

fn assert_request_error(
  dependencies: &[&EncodedImmutableIndexArtifactV1],
  manifest: &EncodedImmutableIndexArtifactV1,
  pointer: &EncodedActivePointerV1,
  limits: IndexGenerationPublicationLimitsV1,
  class: MalformedInputClass,
) {
  let error = IndexGenerationPublicationMachineV1::new(IndexGenerationPublicationRequestV1 {
    ..request(IndexGenerationPublicationModeV1::Soft, dependencies, manifest, pointer, limits)
  })
  .unwrap_err();
  assert_eq!(error.class(), class);
}

fn durability(sequence: u64, class: CommitClass) -> DurabilityCommitReceipt {
  DurabilityCommitReceipt { sequence, class, hard_frontier: sequence }
}

fn publication_inputs() -> (Vec<EncodedImmutableIndexArtifactV1>, EncodedImmutableIndexArtifactV1, EncodedActivePointerV1) {
  let dependencies = vec![
    immutable_fixture("aidx-blake3-256-posting-page-valid.bin"),
    immutable_fixture("aidx-blake3-256-posting-directory-leaf-valid.bin"),
  ];
  let manifest = immutable_fixture("aidx-blake3-256-field-index-manifest-empty.bin");
  let decoded_manifest = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: decoded_manifest.generation,
    owner_id: decoded_manifest.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  (dependencies, manifest, pointer)
}

fn immutable_fixture(name: &str) -> EncodedImmutableIndexArtifactV1 {
  let value = fs::read(fixture_root().join(name)).unwrap();
  let fixture_manifest: serde_json::Value =
    serde_json::from_slice(&fs::read(fixture_root().parent().unwrap().join("format-fixture-manifest.json")).unwrap()).unwrap();
  let id = name.strip_suffix(".bin").unwrap();
  let key = fixture_manifest["fixtures"]
    .as_array()
    .unwrap()
    .iter()
    .find(|fixture| fixture["id"].as_str() == Some(id))
    .and_then(|fixture| fixture["canonical_key"].as_str())
    .map(hex::decode)
    .unwrap()
    .unwrap();
  EncodedImmutableIndexArtifactV1 { key, value }
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn total_bytes_for(
  dependencies: &[EncodedImmutableIndexArtifactV1],
  manifest: &EncodedImmutableIndexArtifactV1,
  pointer: &EncodedActivePointerV1,
) -> usize {
  dependencies.iter().map(|artifact| artifact.value.len()).sum::<usize>() + manifest.value.len() + pointer.value.len()
}
