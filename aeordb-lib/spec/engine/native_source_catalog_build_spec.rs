//! Actual native readback of constructed nodes; controls are test-seeded only.
use super::*;
use crate::engine::v4::semantic_source_capture::{
  build_semantic_source_catalog_pair_v1, SemanticSourceCatalogBuildRequestV1, SemanticSourceCatalogPairRowV1,
};

#[test]
fn constructed_catalog_pairs_read_exact_staged_sources_after_native_reopen() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("source-catalog-assembled-native", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    seed_raw_source(&publisher, INDEX_SOURCE, 0, 1, 0, false, CompressionAlgorithm::Zstd);
    {
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      source.stage_retained_copy(source_bounds(), publisher.observe().unwrap().selected.header.updated_at_ms + 1).unwrap();
      let old = source.revision().to_vec();
      drop(source);
      drop(captured);
      seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", b"requested captured input")]);
      let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let source = captured.read_protected_source(INDEX_SOURCE, source_bounds()).unwrap().unwrap();
      source.stage_retained_copy(source_bounds(), publisher.observe().unwrap().selected.header.updated_at_ms + 1).unwrap();
      let new = source.revision().to_vec();
      drop(source);
      drop(captured);
      let first = [
        SemanticSourceCatalogPairRowV1 { path: INDEX_SOURCE.into(), base_file_record_id: Some(old), requested_file_record_id: Some(new) },
        SemanticSourceCatalogPairRowV1 { path: PARSER_SOURCE.into(), base_file_record_id: None, requested_file_record_id: None },
      ];
      let remaining = (0..511).map(|index| SemanticSourceCatalogPairRowV1 {
        path: format!("/.aeordb-system/plugin-aliases/{index:05}"),
        base_file_record_id: None,
        requested_file_record_id: None,
      });
      let roots = build_semantic_source_catalog_pair_v1(
        SemanticSourceCatalogBuildRequestV1 {
          database_id: [1; 16],
          hash_algorithm: algorithm,
          expected_path_count: 513,
          maximum_path_bytes: 128,
          maximum_workspace_bytes: 4 << 20,
          maximum_node_pairs: 5,
          maximum_output_bytes: 1 << 20,
        },
        first.into_iter().chain(remaining).map(Ok),
        &mut |base, requested| {
          // Production semantic-task publication remains refused. Install exact
          // emitted controls through the existing fixture owner to qualify the
          // constructor/native-reader edge, NOT task publication or retention.
          seed_catalog_node(&publisher, base);
          if base != requested {
            seed_catalog_node(&publisher, requested);
          }
          Ok(())
        },
        &memory,
        &|| false,
      )
      .unwrap();
      assert_eq!(roots.node_count(), 5);
      let checkpoint = frozen(algorithm, "checkpoint");
      let decoded = decode_semantic_mutation_checkpoint(&checkpoint, algorithm).unwrap();
      let checkpoint_hash = digest_parts(algorithm, &[&checkpoint]);
      let capture = encode_semantic_source_capture_v1(
        &SemanticSourceCaptureV1 {
          database_id: decoded.database_id,
          task_id: decoded.task_id,
          checkpoint_sequence: decoded.checkpoint_sequence,
          physical_instance_id: decoded.physical_instance_id,
          writer_fence_epoch: decoded.writer_fence_epoch,
          semantic_generation: decoded.semantic_generation,
          header_sequence: decoded.header_sequence,
          captured_at_ms: decoded.captured_at_ms,
          protected_path_count: roots.path_count(),
          base_catalog_node_count: roots.node_count(),
          requested_catalog_node_count: roots.node_count(),
          base_namespace_root: decoded.base_namespace_root,
          staged_directory_root: decoded.staged_directory_root,
          base_source_catalog: roots.base_root(),
          requested_source_catalog: roots.requested_root(),
          source_identity_fingerprint: decoded.source_identity_fingerprint,
          checkpoint_payload_hash: &checkpoint_hash,
        },
        algorithm,
      )
      .unwrap();
      let identity = checkpoint_identity();
      seed(
        &publisher,
        &[
          (SystemControlKindV1::SemanticMutationCheckpoint, &identity, SystemControlSlotV1::Immutable, &checkpoint),
          (SystemControlKindV1::SemanticSourceCapture, &identity, SystemControlSlotV1::Immutable, &capture),
        ],
      );
      let mut header = publisher.observe().unwrap().selected.header;
      header.required_reader_capabilities[3] |= 8;
      header.required_writer_capabilities[3] |= 8;
      write_redundant_header(&publisher, &header);
    }
    seed_files(&publisher, &[(INDEX_SOURCE.to_owned(), "application/json", b"later current input")]);
    drop(publisher);
    let (_coordinator, reopened) = reopen(&path);
    let before = fs::read(&path).unwrap();
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = reopened.acquire_staging_protection(&memory, &cancellation).unwrap();
    let captured = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let baseline = memory.snapshot().unwrap().reserved_bytes;
    let mut count = 0;
    let summary = captured
      .visit_captured_protected_source_pairs(&[2; 16], 1, catalog_bounds(), |path, base, requested| {
        if count == 0 {
          assert_eq!(path, INDEX_SOURCE);
          assert_eq!(base.unwrap().body(), b"firstsecond");
          assert_eq!(requested.unwrap().body(), b"requested captured input");
        } else {
          assert!(base.is_none() && requested.is_none());
          if count == 1 {
            assert_eq!(path, PARSER_SOURCE);
          } else {
            assert_eq!(path, format!("/.aeordb-system/plugin-aliases/{:05}", count - 2));
          }
        }
        count += 1;
        Ok(true)
      })
      .unwrap();
    assert_eq!(count, 513);
    assert_eq!(summary, SemanticSourceCatalogSummaryV1 { paths: 513, base_nodes: 5, requested_nodes: 5, complete: true });
    assert_eq!(
      captured
        .read_captured_protected_source(&[2; 16], 1, SemanticSourceCatalogSideV1::Requested, INDEX_SOURCE, catalog_bounds())
        .unwrap()
        .source()
        .unwrap()
        .body(),
      b"requested captured input"
    );
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    assert!(fs::read(&path).unwrap() == before);
  }
}
