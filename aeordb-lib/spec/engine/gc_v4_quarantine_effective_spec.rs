use super::*;
use aeordb::engine::v4::gc_quarantine::{
  QuarantineEffectiveClosureErrorV1, QuarantineEffectiveClosureLimitsV1, QuarantineEffectiveClosureRequestV1, QuarantineEffectiveClosureV1,
};
use std::collections::BTreeMap;
#[path = "gc_v4_quarantine_effective_allocation_spec.rs"]
mod allocation;
#[path = "gc_v4_quarantine_effective_boundary_spec.rs"]
mod boundary;
#[path = "gc_v4_quarantine_effective_graph_spec.rs"]
mod graph;
#[path = "gc_v4_physical_incarnation_framing_spec.rs"]
mod physical_framing;

// Independent tuple ordering/map reduction: do not call the production merge
// or physical comparator to compute its expected survivors or their order.
type Identity = (Vec<u8>, Vec<u8>, u64, u64, u32, u8, u8);
fn identity(row: PhysicalIncarnationV1<'_>) -> Identity {
  (
    row.logical_key.to_vec(),
    row.integrity_or_legacy_digest.to_vec(),
    row.wal_offset,
    row.write_sequence,
    row.entity_length,
    row.entry_type,
    row.entity_version,
  )
}

fn with_effective_fixture<T>(
  algorithm: HashAlgorithm,
  delta_values: &[Vec<u8>],
  include_base: bool,
  run: impl FnOnce(
    QuarantineEffectiveClosureRequestV1<'_>,
    Option<&aeordb::engine::v4::gc_state::GcStatePageV1<'_>>,
    BTreeMap<Identity, Vec<u8>>,
    u64,
    &MemoryCoordinator,
  ) -> T,
) -> T {
  let name = algorithm_name(algorithm);
  let page_bytes = fixture(&format!("agca-{name}-candidate-page-valid.bin"));
  let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
    unreachable!()
  };
  let directory_bytes = fixture(&format!("agca-{name}-candidates-directory-valid.bin"));
  let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
    unreachable!()
  };
  let lifecycle_bytes = fixture(&format!("agca-{name}-root-lifecycle-manifest-populated.bin"));
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes, algorithm).unwrap() else {
    unreachable!()
  };
  let original_bytes = fixture(&format!("agca-{name}-quarantine-manifest-populated.bin"));
  let original = decode_quarantine_manifest_v1(&original_bytes, algorithm).unwrap();
  let mut expected = BTreeMap::new();
  let mut input_rows = 0;
  if include_base {
    for row in quarantine_candidate_records_v1(&page, algorithm).unwrap() {
      let row = row.unwrap();
      expected.insert(identity(row.incarnation), row.encoded.to_vec());
      input_rows += 1;
    }
  }
  let mut delta_hashes = Vec::new();
  for value in delta_values {
    let delta = decode_candidate_delta_v1(value, algorithm).unwrap();
    delta_hashes.extend_from_slice(&delta.key);
    for record in delta.records().unwrap() {
      let record = record.unwrap();
      let key = identity(record.candidate.incarnation);
      match record.operation {
        CandidateDeltaOperationV1::Set => {
          expected.insert(key, record.candidate.encoded.to_vec());
        }
        CandidateDeltaOperationV1::Clear => {
          expected.remove(&key);
        }
      }
      input_rows += 1;
    }
  }
  let count = expected.len() as u64;
  let bytes = count * (52 + 2 * algorithm.hash_length()) as u64;
  let mut request = QuarantineManifestWriteV1::from_decoded(&original).unwrap();
  request.candidate_count = count;
  request.candidate_bytes = bytes;
  request.eligible_count_hint = 0;
  request.eligible_bytes_hint = 0;
  request.delta_hashes = &delta_hashes;
  request.candidate_directory_root = include_base.then_some(directory.key.as_slice());
  let encoded = encode_quarantine_manifest_v1(&request).unwrap();
  let manifest = decode_quarantine_manifest_v1(&encoded.value, algorithm).unwrap();
  let values: Vec<_> = delta_values.iter().map(Vec::as_slice).collect();
  let memory = memory_coordinator();
  run(
    QuarantineEffectiveClosureRequestV1 {
      manifest: &manifest,
      directory: include_base.then_some(&directory),
      lifecycle: &lifecycle,
      delta_values: &values,
      hash_algorithm: algorithm,
      limits: QuarantineEffectiveClosureLimitsV1 { maximum_support_artifacts: 1024, maximum_work: 10_000 },
    },
    include_base.then_some(&page),
    expected,
    input_rows,
    &memory,
  )
}

fn check_effective_fixture(algorithm: HashAlgorithm, delta_values: &[Vec<u8>]) {
  with_effective_fixture(algorithm, delta_values, true, |request, page, expected, input_rows, memory| {
    let count = expected.len() as u64;
    let bytes = count * (52 + 2 * algorithm.hash_length()) as u64;
    let manifest_key = request.manifest.key.clone();
    let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory)
      .expect("effective candidate closure must open for a valid independent graph");
    let mut actual = Vec::new();
    let mut visitor = |row: aeordb::engine::v4::gc_quarantine::PhysicalQuarantineCandidateV1<'_>| {
      actual.push((identity(row.incarnation), row.encoded.to_vec()));
      Ok::<(), QuarantineEffectiveClosureErrorV1>(())
    };
    closure.observe_base_page(page.unwrap(), &mut visitor).unwrap();
    let summary = closure.finish(&mut visitor).unwrap();
    assert_eq!(actual, expected.into_iter().collect::<Vec<_>>());
    assert_eq!((summary.candidate_count, summary.candidate_bytes), (count, bytes));
    assert_eq!(summary.input_rows, input_rows);
    assert_eq!(summary.work, summary.input_rows + summary.comparisons);
    assert_eq!(summary.closure.manifest_key(), manifest_key);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  });
}

#[test]
fn effective_quarantine_candidates_merge_independent_base_and_delta() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let delta = fixture(&format!("agca-{}-candidate-delta-valid.bin", algorithm_name(algorithm)));
    check_effective_fixture(algorithm, &[delta]);
  }
}

#[test]
fn effective_quarantine_candidates_apply_later_clear_restore_and_delta_only_rows() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let name = algorithm_name(algorithm);
    let page_bytes = fixture(&format!("agca-{name}-candidate-page-valid.bin"));
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
      unreachable!()
    };
    let base: Vec<_> = quarantine_candidate_records_v1(&page, algorithm).unwrap().map(Result::unwrap).collect();
    let manifest_bytes = fixture(&format!("agca-{name}-quarantine-manifest-populated.bin"));
    let manifest = decode_quarantine_manifest_v1(&manifest_bytes, algorithm).unwrap();
    let clear = |index: usize| {
      let mut candidate = PhysicalQuarantineCandidateWriteV1::from(&base[index]);
      candidate.pending_since_ms = 0;
      candidate.first_unreachable_generation = 0;
      candidate.grace_at_pending_ms = 0;
      CandidateDeltaRecordWriteV1 { operation: CandidateDeltaOperationV1::Clear, candidate }
    };
    let mut restored = PhysicalQuarantineCandidateWriteV1::from(&base[0]);
    restored.grace_at_pending_ms += 17;
    let new_key = vec![0xfe; algorithm.hash_length()];
    let mut additional = restored;
    additional.incarnation.logical_key = &new_key;
    assert!(base.iter().all(|row| row.incarnation.logical_key != new_key));
    let rows = [
      clear(0),
      CandidateDeltaRecordWriteV1 { operation: CandidateDeltaOperationV1::Set, candidate: restored },
      clear(1),
      CandidateDeltaRecordWriteV1 { operation: CandidateDeltaOperationV1::Set, candidate: additional },
    ];
    let mut artifacts: Vec<aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
      let encoded = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
        hash_algorithm: algorithm,
        database_id: manifest.database_id.try_into().unwrap(),
        mark_generation: manifest.mark_generation,
        delta_ordinal: index as u32 + 1,
        previous_delta_hash: artifacts.last().map(|artifact| artifact.key.as_slice()),
        records: std::slice::from_ref(row),
      })
      .unwrap();
      artifacts.push(encoded);
    }
    check_effective_fixture(algorithm, &artifacts.into_iter().map(|artifact| artifact.value).collect::<Vec<_>>());
  }
}
