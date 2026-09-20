use super::*;
use crate::engine::v4::gc_quarantine::{
  CandidateDeltaOperationV1, CandidateDeltaRecordWriteV1, CandidateDeltaWriteV1, PhysicalQuarantineCandidateWriteV1,
  PhysicalQuarantineCandidateClassV1, encode_candidate_delta_v1, encode_physical_quarantine_candidate_v1,
};
use crate::engine::v4::gc_state::{
  GcStatePageWriteV1, GcStateDirectoryWriteV1, GcStateDirectoryEntryWriteV1, GcPhysicalHintV1, encode_gc_state_page_v1,
  encode_gc_state_directory_v1,
};

fn publish_support(publisher: &V4FirstAuthorityPublisher, artifact: &EncodedImmutableGcArtifactV1) {
  let kind = decode_gc_artifact_envelope(&artifact.value).unwrap().kind;
  publisher
    .publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind,
        database_id: &[1; 16],
        artifact_key: &artifact.key,
        value: &artifact.value,
        minimum_timestamp_ms: publisher.observe().unwrap().selected.header.updated_at_ms + 1,
        committed_postcondition_code: "physical_task_fixture_support",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
    .unwrap();
}

fn target_quarantine(
  publisher: &V4FirstAuthorityPublisher,
  target: &RecordedPhysicalTarget,
  include_base: bool,
  operations: &[CandidateDeltaOperationV1],
) -> EncodedImmutableGcArtifactV1 {
  let algorithm = target.algorithm;
  let digest = vec![0x35; algorithm.hash_length()];
  let lifecycle = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: &[1; 16],
    generation: 1,
    published_at_ms: 1,
    source_complete_mark_generation: 1,
    authority_root_set_digest: &digest,
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
  publish_support(publisher, &lifecycle);
  let candidate = PhysicalQuarantineCandidateWriteV1 {
    hash_algorithm: algorithm,
    incarnation: target.incarnation(),
    class: PhysicalQuarantineCandidateClassV1::UnreachableActiveLocator,
    pending_since_ms: 1,
    first_unreachable_generation: 1,
    grace_at_pending_ms: 0,
  };
  let directory = if include_base {
    let row = encode_physical_quarantine_candidate_v1(&candidate).unwrap();
    let encoded_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
      hash_algorithm: algorithm,
      role: GcDirectoryRoleV1::Candidates,
      database_id: &[1; 16],
      catalog_id: &[8; 16],
      generation: 1,
      page_id: 1,
      records: &[&row],
    })
    .unwrap();
    publish_support(publisher, &encoded_page);
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&encoded_page.value, algorithm).unwrap() else {
      unreachable!()
    };
    let encoded = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
      hash_algorithm: algorithm,
      role: GcDirectoryRoleV1::Candidates,
      database_id: &[1; 16],
      catalog_id: &[8; 16],
      generation: 1,
      level: 0,
      entries: &[GcStateDirectoryEntryWriteV1 {
        lower_fence: page.lower_fence,
        upper_fence: page.upper_fence,
        child_hash: &page.key,
        child_generation: page.generation,
        live_count: u64::from(page.record_count),
        tombstone_count: 0,
        page_count: 1,
        logical_bytes: page.logical_bytes,
        minimum_page_id: page.page_id,
        maximum_page_id: page.page_id,
        physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
      }],
    })
    .unwrap();
    publish_support(publisher, &encoded);
    Some(encoded)
  } else {
    None
  };
  let mut deltas: Vec<EncodedImmutableGcArtifactV1> = Vec::new();
  for (index, operation) in operations.iter().enumerate() {
    let mut delta_candidate = candidate;
    if *operation == CandidateDeltaOperationV1::Clear {
      delta_candidate.pending_since_ms = 0;
      delta_candidate.first_unreachable_generation = 0;
    }
    let delta = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
      hash_algorithm: algorithm,
      database_id: [1; 16],
      mark_generation: index as u64 + 2,
      delta_ordinal: 1,
      previous_delta_hash: deltas.last().map(|delta| delta.key.as_slice()),
      records: &[CandidateDeltaRecordWriteV1 { operation: *operation, candidate: delta_candidate }],
    })
    .unwrap();
    publish_support(publisher, &delta);
    deltas.push(delta);
  }
  let delta_hashes: Vec<u8> = deltas.iter().flat_map(|delta| delta.key.iter().copied()).collect();
  let count = u64::from(operations.last().map_or(include_base, |last| *last == CandidateDeltaOperationV1::Set));
  let mut capabilities = [0u8; 32];
  for bit in [12usize, 13, 15, 17] {
    capabilities[bit / 8] |= 1 << (bit % 8);
  }
  encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: [1; 16],
    mark_generation: operations.len() as u64 + 2,
    completed_at_ms: 10,
    required_capabilities: &capabilities,
    authority_root_set_digest: &digest,
    semantic_state_digest: &digest,
    kv_layout_fingerprint: &digest,
    mark_result_digest: &digest,
    candidate_directory_root: directory.as_ref().map(|directory| directory.key.as_slice()),
    captured_root_lifecycle_manifest: &lifecycle.key,
    candidate_count: count,
    candidate_bytes: count * (52 + 2 * algorithm.hash_length()) as u64,
    eligible_count_hint: 0,
    eligible_bytes_hint: 0,
    next_candidate_page_id: 2,
    delta_hashes: &delta_hashes,
  })
  .unwrap()
}

#[test]
fn native_semantic_task_physical_exclusion_checks_only_effective_quarantine_candidates() {
  use CandidateDeltaOperationV1::{Clear, Set};
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for (include_base, operations, retained) in [
      (true, vec![], true),
      (true, vec![Clear], false),
      (true, vec![Clear, Set], true),
      (false, vec![Set], true),
      (false, vec![Set, Clear], false),
    ] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("physical-task-effective", None, [1; 16], algorithm, 0);
      let (_, base, _) = seed_captured_graph(&publisher);
      let target = RecordedPhysicalTarget::read(&publisher, &base);
      let artifact = target_quarantine(&publisher, &target, include_base, &operations);
      flush_mark_fixture(&publisher);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
      let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
      assert_eq!(mark.summary().retention.tasks, 1);
      let before = fs::read(&path).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let result = publisher.qualify_semantic_task_quarantine_exclusion(&mark, &artifact, physical_bounds());
      if retained {
        assert_eq!(result.unwrap_err().code(), "semantic_task_physical_retained");
      } else {
        let proof = result.unwrap();
        validate_physical_proof(&publisher, &proof, GcArtifactKindV1::QuarantineManifest, &artifact.key).unwrap();
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_task_physical_exclusion_quarantine_accepts_unrelated_data_with_active_task() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("physical-task-quarantine-unrelated", None, [1; 16], algorithm, 0);
    seed_captured_graph(&publisher);
    let key = publish_namespace_value(&publisher, EntryTypeV4::Chunk, 0, b"chunk:", b"unretained quarantine candidate");
    let target = RecordedPhysicalTarget::read(&publisher, &key);
    let with_delta = target_quarantine(&publisher, &target, true, &[CandidateDeltaOperationV1::Set]);
    // Share one published support graph; do not republish its immutable bodies
    // with a different physical timestamp while constructing the base-only view.
    let decoded = decode_quarantine_manifest_v1(&with_delta.value, algorithm).unwrap();
    let mut base_only = QuarantineManifestWriteV1::from_decoded(&decoded).unwrap();
    base_only.delta_hashes = &[];
    let artifact = encode_quarantine_manifest_v1(&base_only).unwrap();
    let missing = vec![0xa7; algorithm.hash_length()];
    assert!(publisher.locator(&missing).unwrap().is_none());
    flush_mark_fixture(&publisher);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let before = fs::read(&path).unwrap();
    let proof = {
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(retention_capture_bounds(), &memory, &cancellation).unwrap();
      let mark = capture.mark_captured_semantic_tasks(mark_bounds()).unwrap();
      assert_eq!(mark.summary().retention.tasks, 1);
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      for (bounds, expected) in [
        (
          NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_support_artifacts: 1, ..physical_bounds() },
          "quarantine_closure_artifact_limit",
        ),
        (NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_work: 1, ..physical_bounds() }, "quarantine_effective_work"),
        (NativeSemanticTaskPhysicalExclusionBoundsV1 { maximum_read_bytes: 1, ..physical_bounds() }, "semantic_task_inventory_read_bound"),
      ] {
        assert_eq!(publisher.qualify_semantic_task_quarantine_exclusion(&mark, &with_delta, bounds).unwrap_err().code(), expected);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        assert_eq!(fs::read(&path).unwrap(), before);
      }
      let decoded = decode_quarantine_manifest_v1(&with_delta.value, algorithm).unwrap();
      for role in 0..3 {
        let mut changed = QuarantineManifestWriteV1::from_decoded(&decoded).unwrap();
        match role {
          0 => changed.captured_root_lifecycle_manifest = &missing,
          1 => changed.candidate_directory_root = Some(&missing),
          2 => changed.delta_hashes = &missing,
          _ => unreachable!(),
        }
        let missing_support = encode_quarantine_manifest_v1(&changed).unwrap();
        assert_eq!(
          publisher.qualify_semantic_task_quarantine_exclusion(&mark, &missing_support, physical_bounds()).unwrap_err().code(),
          "quarantine_support_missing"
        );
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        assert_eq!(fs::read(&path).unwrap(), before);
      }
      let retry = publisher.qualify_semantic_task_quarantine_exclusion(&mark, &with_delta, physical_bounds()).unwrap();
      validate_physical_proof(&publisher, &retry, GcArtifactKindV1::QuarantineManifest, &with_delta.key).unwrap();
      drop(retry);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      publisher.qualify_semantic_task_quarantine_exclusion(&mark, &artifact, physical_bounds()).unwrap()
    };
    assert!(memory.snapshot().unwrap().reserved_bytes <= 8192);
    validate_physical_proof(&publisher, &proof, GcArtifactKindV1::QuarantineManifest, &artifact.key).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(proof);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
