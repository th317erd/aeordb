use super::*;
use aeordb::engine::memory_coordinator::HostMemorySample;
use aeordb::engine::v4::gc_lifecycle::{RootLifecycleManifestWriteV1, encode_root_lifecycle_manifest_v1};
use aeordb::engine::v4::gc_quarantine::PhysicalQuarantineCandidateV1;
use aeordb::engine::v4::gc_state::GcStatePageV1;

fn with_nested_graph<T>(
  algorithm: HashAlgorithm,
  run: impl FnOnce(
    QuarantineEffectiveClosureRequestV1<'_>,
    &[GcStatePageV1<'_>],
    &[GcStateDirectoryV1<'_>],
    Vec<Vec<u8>>,
    &MemoryCoordinator,
  ) -> T,
) -> T {
  let bytes = fixture("agca-blake3-256-candidate-page-valid.bin");
  let GcStateArtifactV1::Page(template) = decode_gc_state_artifact(&bytes, HashAlgorithm::Blake3_256).unwrap() else {
    unreachable!()
  };
  let candidate = quarantine_candidate_records_v1(&template, HashAlgorithm::Blake3_256).unwrap().next().unwrap().unwrap();
  let hash = vec![0x35; algorithm.hash_length()];
  // Same logical key, different incarnations; numeric order crosses LE byte boundaries.
  let rows: Vec<_> = [255, 256, 65535, 65536]
    .into_iter()
    .map(|wal_offset| {
      let mut write = PhysicalQuarantineCandidateWriteV1::from(&candidate);
      write.hash_algorithm = algorithm;
      write.incarnation = PhysicalIncarnationV1 {
        logical_key: &hash,
        integrity_or_legacy_digest: &hash,
        wal_offset,
        entity_length: 1,
        ..candidate.incarnation
      };
      encode_physical_quarantine_candidate_v1(&write).unwrap()
    })
    .collect();
  let encoded_pages: Vec<_> = rows
    .iter()
    .enumerate()
    .map(|(index, row)| {
      encode_gc_state_page_v1(&GcStatePageWriteV1 {
        hash_algorithm: algorithm,
        role: template.role,
        database_id: template.database_id,
        catalog_id: template.catalog_id,
        generation: template.generation,
        page_id: index as u64 + 1,
        records: &[row],
      })
      .unwrap()
    })
    .collect();
  let pages: Vec<_> = encoded_pages
    .iter()
    .map(|artifact| match decode_gc_state_artifact(&artifact.value, algorithm).unwrap() {
      GcStateArtifactV1::Page(page) => page,
      _ => unreachable!(),
    })
    .collect();
  let hint = GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 };
  let encoded_leaves: Vec<_> = pages
    .chunks(2)
    .map(|children| {
      let entries: Vec<_> = children
        .iter()
        .map(|page| GcStateDirectoryEntryWriteV1 {
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
          physical_hint: hint,
        })
        .collect();
      encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
        hash_algorithm: algorithm,
        role: template.role,
        database_id: template.database_id,
        catalog_id: template.catalog_id,
        generation: template.generation,
        level: 0,
        entries: &entries,
      })
      .unwrap()
    })
    .collect();
  let leaves: Vec<_> = encoded_leaves
    .iter()
    .map(|artifact| match decode_gc_state_artifact(&artifact.value, algorithm).unwrap() {
      GcStateArtifactV1::Directory(directory) => directory,
      _ => unreachable!(),
    })
    .collect();
  let entries: Vec<_> = leaves
    .iter()
    .map(|directory| GcStateDirectoryEntryWriteV1 {
      lower_fence: directory.lower_fence,
      upper_fence: directory.upper_fence,
      child_hash: &directory.key,
      child_generation: directory.generation,
      live_count: directory.live_count,
      tombstone_count: directory.tombstone_count,
      page_count: directory.page_count,
      logical_bytes: directory.logical_bytes,
      minimum_page_id: directory.minimum_page_id,
      maximum_page_id: directory.maximum_page_id,
      physical_hint: hint,
    })
    .collect();
  let root_bytes = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: template.role,
    database_id: template.database_id,
    catalog_id: template.catalog_id,
    generation: template.generation,
    level: 1,
    entries: &entries,
  })
  .unwrap();
  let GcStateArtifactV1::Directory(root) = decode_gc_state_artifact(&root_bytes.value, algorithm).unwrap() else {
    unreachable!()
  };
  let lifecycle_bytes = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: template.database_id,
    generation: 1,
    published_at_ms: 1,
    source_complete_mark_generation: 1,
    authority_root_set_digest: &hash,
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
  let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(&lifecycle_bytes.value, algorithm).unwrap() else {
    unreachable!()
  };
  let original_bytes = fixture("agca-blake3-256-quarantine-manifest-populated.bin");
  let original = decode_quarantine_manifest_v1(&original_bytes, HashAlgorithm::Blake3_256).unwrap();
  let mut write = QuarantineManifestWriteV1::from_decoded(&original).unwrap();
  write.hash_algorithm = algorithm;
  write.authority_root_set_digest = &hash;
  write.semantic_state_digest = &hash;
  write.kv_layout_fingerprint = &hash;
  write.mark_result_digest = &hash;
  write.candidate_directory_root = Some(&root.key);
  write.captured_root_lifecycle_manifest = &lifecycle.key;
  write.delta_hashes = &[];
  write.candidate_count = 4;
  write.candidate_bytes = 4 * (52 + 2 * algorithm.hash_length()) as u64;
  write.eligible_count_hint = 0;
  write.eligible_bytes_hint = 0;
  write.next_candidate_page_id = 5;
  let manifest_bytes = encode_quarantine_manifest_v1(&write).unwrap();
  let manifest = decode_quarantine_manifest_v1(&manifest_bytes.value, algorithm).unwrap();
  let memory = memory_coordinator();
  run(
    QuarantineEffectiveClosureRequestV1 {
      manifest: &manifest,
      directory: Some(&root),
      lifecycle: &lifecycle,
      delta_values: &[],
      hash_algorithm: algorithm,
      limits: QuarantineEffectiveClosureLimitsV1 { maximum_support_artifacts: 7, maximum_work: 100 },
    },
    &pages,
    &leaves,
    rows,
    &memory,
  )
}

#[test]
fn effective_quarantine_nested_graph_preserves_all_hashes_and_distinct_incarnations() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    with_nested_graph(algorithm, |request, pages, leaves, expected, memory| {
      let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory).unwrap();
      let mut actual = Vec::new();
      let mut visitor = |row: PhysicalQuarantineCandidateV1<'_>| {
        actual.push(row.encoded.to_vec());
        Ok::<(), QuarantineEffectiveClosureErrorV1>(())
      };
      for (children, directory) in pages.chunks(2).zip(leaves) {
        for page in children {
          closure.observe_base_page(page, &mut visitor).unwrap();
        }
        closure.observe_base_directory(directory).unwrap();
      }
      let summary = closure.finish(&mut visitor).unwrap();
      assert_eq!(actual, expected);
      assert_eq!((summary.candidate_count, summary.input_rows, summary.closure.support_artifact_count), (4, 4, 7));
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    });
  }
}

#[test]
fn effective_quarantine_nested_graph_rejects_missing_repeated_and_incorrect_children() {
  for scenario in 0..4 {
    with_nested_graph(HashAlgorithm::Blake3_256, |request, pages, leaves, _, memory| {
      let mut closure = QuarantineEffectiveClosureV1::new(request, CancellationToken::new(), memory).unwrap();
      let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| Ok::<(), QuarantineEffectiveClosureErrorV1>(());
      if scenario == 0 {
        assert_eq!(closure.observe_base_directory(&leaves[0]).unwrap_err().code(), "quarantine_base_directory_order");
      } else if scenario == 1 {
        closure.observe_base_page(&pages[0], &mut visitor).unwrap();
        assert_eq!(closure.observe_base_page(&pages[0], &mut visitor).unwrap_err().code(), "quarantine_base_page_order");
      } else if scenario == 2 {
        for page in &pages[..2] {
          closure.observe_base_page(page, &mut visitor).unwrap();
        }
        let mut incorrect = leaves[0].clone();
        incorrect.entries[0].child_generation += 1;
        assert_eq!(closure.observe_base_directory(&incorrect).unwrap_err().code(), "quarantine_base_directory_closure");
      } else {
        closure.observe_base_page(&pages[0], &mut visitor).unwrap();
        assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_base_directory_order");
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
        return;
      }
      assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_effective_failed");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    });
  }
}

#[test]
fn effective_quarantine_nested_graph_checks_directory_interruptions_and_support_limit() {
  for scenario in 0..3 {
    with_nested_graph(HashAlgorithm::Sha512, |mut request, pages, leaves, _, memory| {
      if scenario == 2 {
        request.limits.maximum_support_artifacts = 3;
      }
      let cancellation = CancellationToken::new();
      let mut closure = QuarantineEffectiveClosureV1::new(request, cancellation.clone(), memory).unwrap();
      let mut visitor = |_: PhysicalQuarantineCandidateV1<'_>| Ok::<(), QuarantineEffectiveClosureErrorV1>(());
      for page in &pages[..2] {
        closure.observe_base_page(page, &mut visitor).unwrap();
      }
      let expected = match scenario {
        0 => {
          cancellation.cancel();
          "quarantine_closure_canceled"
        }
        1 => {
          memory.update_host_sample(HostMemorySample { rss_bytes: 64 << 20, ..Default::default() }).unwrap();
          "quarantine_effective_memory"
        }
        _ => "quarantine_closure_artifact_limit",
      };
      assert_eq!(closure.observe_base_directory(&leaves[0]).unwrap_err().code(), expected);
      assert_eq!(closure.finish(&mut visitor).unwrap_err().code(), "quarantine_effective_failed");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    });
  }
}
