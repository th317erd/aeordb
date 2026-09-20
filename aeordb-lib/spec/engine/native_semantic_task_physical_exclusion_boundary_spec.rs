#[path = "native_semantic_task_physical_exclusion_quarantine_spec.rs"]
mod quarantine;
use super::*;
use crate::engine::v4::gc::PhysicalIncarnationV1;
use crate::engine::v4::gc_void::{SweepProposalWriteV1, encode_sweep_proposal_v1};
use crate::engine::memory_coordinator::HostMemorySample;

fn physical_bounds() -> NativeSemanticTaskPhysicalExclusionBoundsV1 {
  NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_support_artifacts: 1024, maximum_work: 100_000, maximum_read_bytes: 64 << 20 }
}

struct RecordedPhysicalTarget {
  locator: KVEntry,
  bytes: Vec<u8>,
  algorithm: HashAlgorithm,
}

impl RecordedPhysicalTarget {
  fn read(publisher: &V4FirstAuthorityPublisher, key: &[u8]) -> Self {
    let locator = publisher.locator(key).unwrap().unwrap();
    let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
    let mut bytes = vec![0; locator.total_length as usize];
    read_file_at_native(&publisher.file, locator.offset, &mut bytes).unwrap();
    // Full integrity is an independent fixture setup obligation. The native
    // task predicate itself must only read the checked prefix and physical key.
    assert_eq!(decode_whole_entity(&bytes, algorithm, u64::MAX).unwrap().key, key);
    Self { locator, bytes, algorithm }
  }

  fn incarnation(&self) -> PhysicalIncarnationV1<'_> {
    // Literal frozen framing offsets, not a second production projection.
    PhysicalIncarnationV1 {
      logical_key: &self.locator.hash,
      integrity_or_legacy_digest: &self.bytes[41..41 + self.algorithm.hash_length()],
      wal_offset: self.locator.offset,
      write_sequence: u64::from_le_bytes(self.bytes[33..41].try_into().unwrap()),
      entity_length: self.locator.total_length,
      entry_type: self.bytes[5],
      entity_version: self.bytes[4],
    }
  }
}

fn proposal(algorithm: HashAlgorithm, candidates: &[PhysicalIncarnationV1<'_>]) -> EncodedImmutableGcArtifactV1 {
  encode_sweep_proposal_v1(&SweepProposalWriteV1 {
    hash_algorithm: algorithm,
    database_id: &[1; 16],
    batch_id: &[7; 16],
    generation: 3,
    created_at_ms: 100,
    quarantine_manifest_hash: &vec![9; algorithm.hash_length()],
    candidates,
  })
  .unwrap()
}

fn validate_physical_proof(
  publisher: &V4FirstAuthorityPublisher,
  proof: &NativeSemanticTaskPhysicalExclusionV1<'_>,
  kind: GcArtifactKindV1,
  key: &[u8],
) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
  let guard = publisher.root_state.lock().unwrap();
  let observation = publisher.observe().unwrap();
  publisher.validate_semantic_task_physical_exclusion_locked(&guard, Some(proof), &observation, kind, key)
}

fn sweep_fixture_proof<'publisher>(
  publisher: &'publisher V4FirstAuthorityPublisher,
  artifact: &EncodedImmutableGcArtifactV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> NativeSemanticTaskPhysicalExclusionV1<'publisher> {
  let protection = publisher.acquire_staging_protection(memory, cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), memory, cancellation).unwrap();
  let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  publisher.qualify_semantic_task_sweep_exclusion(&mark, artifact, physical_bounds()).unwrap()
}

#[test]
fn native_semantic_task_physical_exclusion_sweep_final_gate_accepts_exact_proof_and_refuses_stale() {
  for masks in [(true, false), (false, true), (true, true)] {
    guarded_sweep_fixture_for_task_proof(Some(masks), Some(sweep_fixture_proof));
  }
}

#[test]
fn native_semantic_task_physical_exclusion_matches_retained_incarnations_for_every_hash() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("physical-task-membership", None, [1; 16], algorithm, 0);
    let (expected, _, _) = seed_captured_graph(&publisher);
    let unrelated = publish_namespace_value(&publisher, EntryTypeV4::Chunk, 0, b"chunk:", b"unrelated task chunk");
    let target = RecordedPhysicalTarget::read(&publisher, &unrelated);
    let unrelated_proposal = proposal(algorithm, &[target.incarnation()]);
    flush_mark_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    assert_eq!(mark.summary().retention.tasks, 1);
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    for key in expected.keys() {
      let target = RecordedPhysicalTarget::read(&publisher, key);
      let protected = proposal(algorithm, &[target.incarnation()]);
      assert_eq!(
        publisher.qualify_semantic_task_sweep_exclusion(&mark, &protected, physical_bounds()).unwrap_err().code(),
        "semantic_task_physical_retained"
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    let proof = publisher.qualify_semantic_task_sweep_exclusion(&mark, &unrelated_proposal, physical_bounds()).unwrap();
    drop(mark);
    drop(capture);
    drop(protection);
    assert_eq!(publisher.root_state.lock().unwrap().active_staging_protections, 0);
    assert!(memory.snapshot().unwrap().reserved_bytes <= 8192);
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::SweepProposal, &unrelated_proposal.key).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(proof);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn native_semantic_task_physical_exclusion_rejects_false_incarnation_fields() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("physical-task-identity", None, [1; 16], algorithm, 0);
    let (_, base, _) = seed_captured_graph(&publisher);
    let target = RecordedPhysicalTarget::read(&publisher, &base);
    flush_mark_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let before = fs::read(&path).unwrap();
    let actual = target.incarnation();
    let other_key = vec![0xef; algorithm.hash_length()];
    for changed in [
      PhysicalIncarnationV1 { logical_key: &other_key, ..actual },
      PhysicalIncarnationV1 { integrity_or_legacy_digest: &other_key, ..actual },
      PhysicalIncarnationV1 { entity_version: 0, ..actual },
      PhysicalIncarnationV1 { write_sequence: actual.write_sequence + 1, ..actual },
      PhysicalIncarnationV1 { entry_type: EntryTypeV4::Chunk.to_u8(), ..actual },
      PhysicalIncarnationV1 { wal_offset: actual.wal_offset + 1, ..actual },
      PhysicalIncarnationV1 { wal_offset: 2048, ..actual },
      PhysicalIncarnationV1 { entity_length: actual.entity_length + 1, ..actual },
    ] {
      let artifact = proposal(algorithm, &[changed]);
      let error = publisher.qualify_semantic_task_sweep_exclusion(&mark, &artifact, physical_bounds()).unwrap_err();
      assert_ne!(error.code(), "semantic_task_physical_retained", "must validate claimed physical identity before slot membership");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_task_physical_exclusion_sweep_binds_owner_kind_target_and_frontier() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-task-binding", None, [1; 16], algorithm, 0);
  let (_other_directory, _, _other_coordinator, other) =
    create_environment_for_algorithm_at_kv_stage("physical-task-owner", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database([1; 16])).unwrap();
  let target_key = publish_namespace_value(&publisher, EntryTypeV4::Chunk, 0, b"chunk:", b"unrelated physical binding");
  let target = RecordedPhysicalTarget::read(&publisher, &target_key);
  let artifact = proposal(algorithm, &[target.incarnation()]);
  flush_mark_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let proof = {
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
    let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
    assert_eq!(
      other.qualify_semantic_task_sweep_exclusion(&mark, &artifact, physical_bounds()).unwrap_err().code(),
      "semantic_task_physical_exclusion_owner"
    );
    publisher.qualify_semantic_task_sweep_exclusion(&mark, &artifact, physical_bounds()).unwrap()
  };
  let before = fs::read(&path).unwrap();
  assert_eq!(
    validate_physical_proof(&other, &proof, GcArtifactKindV1::SweepProposal, &artifact.key).unwrap_err().code(),
    "semantic_task_physical_exclusion_owner"
  );
  assert_eq!(
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::QuarantineManifest, &artifact.key).unwrap_err().code(),
    "semantic_task_physical_exclusion_target"
  );
  assert_eq!(
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::SweepProposal, &[0xf1; 32]).unwrap_err().code(),
    "semantic_task_physical_exclusion_target"
  );
  validate_physical_proof(&publisher, &proof, GcArtifactKindV1::SweepProposal, &artifact.key).unwrap();
  assert_eq!(fs::read(&path).unwrap(), before);
  publish_namespace_value(&publisher, EntryTypeV4::Chunk, 0, b"chunk:", b"intervening publication");
  let before = fs::read(&path).unwrap();
  assert_eq!(
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::SweepProposal, &artifact.key).unwrap_err().code(),
    "semantic_task_physical_exclusion_stale"
  );
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_physical_exclusion_sweep_enforces_read_memory_and_cancellation_limits() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, _coordinator, publisher) =
    create_environment_for_algorithm_at_kv_stage("physical-task-bounds", None, [1; 16], algorithm, 0);
  publisher.publish(&request_for_database([1; 16])).unwrap();
  let key = publish_namespace_value(&publisher, EntryTypeV4::Chunk, 0, b"chunk:", b"bounded task exclusion");
  let target = RecordedPhysicalTarget::read(&publisher, &key);
  let artifact = proposal(algorithm, &[target.incarnation()]);
  flush_mark_fixture(&publisher);
  let memory = observation_memory();
  let cancellation = CancellationToken::new();
  let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
  let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
  let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
  let before = fs::read(&path).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let read_bytes = (77 + 2 * algorithm.hash_length() + page_size(algorithm.hash_length())) as u64;
  assert_eq!(
    publisher
      .qualify_semantic_task_sweep_exclusion(
        &mark,
        &artifact,
        NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_read_bytes: read_bytes - 1, ..physical_bounds() }
      )
      .unwrap_err()
      .code(),
    "semantic_task_inventory_read_bound"
  );
  let proof = publisher
    .qualify_semantic_task_sweep_exclusion(
      &mark,
      &artifact,
      NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_read_bytes: read_bytes, maximum_work: 1, ..physical_bounds() },
    )
    .unwrap();
  drop(proof);
  for bounds in [
    NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_work: 0, ..physical_bounds() },
    NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_support_artifacts: 0, ..physical_bounds() },
    NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_read_bytes: 0, ..physical_bounds() },
  ] {
    assert_eq!(
      publisher.qualify_semantic_task_sweep_exclusion(&mark, &artifact, bounds).unwrap_err().code(),
      "semantic_task_physical_exclusion_bounds"
    );
  }
  let (result, failure) = allocation_probe::measure_nth(algorithm.hash_length(), 1, || {
    publisher.qualify_semantic_task_sweep_exclusion(&mark, &artifact, physical_bounds())
  });
  assert!(failure.injected_failure);
  assert_eq!(result.unwrap_err().code(), "semantic_task_physical_exclusion_allocation");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  let proof = publisher.qualify_semantic_task_sweep_exclusion(&mark, &artifact, physical_bounds()).unwrap();
  memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..Default::default() }).unwrap();
  assert_eq!(
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::SweepProposal, &artifact.key).unwrap_err().code(),
    "semantic_task_observation_memory"
  );
  assert_eq!(
    publisher.qualify_semantic_task_sweep_exclusion(&mark, &artifact, physical_bounds()).unwrap_err().code(),
    "semantic_task_observation_memory"
  );
  memory.update_host_sample(HostMemorySample::default()).unwrap();
  cancellation.cancel();
  assert_eq!(
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::SweepProposal, &artifact.key).unwrap_err().code(),
    "semantic_task_observation_cancelled"
  );
  assert_eq!(
    publisher.qualify_semantic_task_sweep_exclusion(&mark, &artifact, physical_bounds()).unwrap_err().code(),
    "semantic_task_observation_cancelled"
  );
  drop(proof);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn native_semantic_task_physical_exclusion_distinguishes_old_and_current_task_incarnations() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("physical-task-replacement", None, [1; 16], algorithm, 0);
    seed_captured_graph(&publisher);
    let task_path = system_control_path(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A).unwrap();
    let key = first_authority_file_path_hash(&task_path, algorithm);
    let old = RecordedPhysicalTarget::read(&publisher, &key);
    let mut replacement =
      publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap().bytes;
    let sequence = u64::from_le_bytes(replacement[16..24].try_into().unwrap()) + 1;
    replacement[16..24].copy_from_slice(&sequence.to_le_bytes());
    crc(&mut replacement);
    seed(&publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &replacement)]);
    let current = RecordedPhysicalTarget::read(&publisher, &key);
    assert_ne!(old.locator.offset, current.locator.offset);
    let old_proposal = proposal(algorithm, &[old.incarnation()]);
    let current_proposal = proposal(algorithm, &[current.incarnation()]);
    flush_mark_fixture(&publisher);
    drop(publisher);
    let (_coordinator, publisher) = reopen(&path);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let before = fs::read(&path).unwrap();
    let proof = {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
      let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
      assert_eq!(mark.summary().retention.tasks, 1);
      assert!(mark.is_captured_locator_marked(&current.locator).unwrap());
      assert!(!mark.is_captured_locator_marked(&old.locator).unwrap());
      assert_eq!(
        publisher.qualify_semantic_task_sweep_exclusion(&mark, &current_proposal, physical_bounds()).unwrap_err().code(),
        "semantic_task_physical_retained"
      );
      publisher.qualify_semantic_task_sweep_exclusion(&mark, &old_proposal, physical_bounds()).unwrap()
    };
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::SweepProposal, &old_proposal.key).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(proof);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
