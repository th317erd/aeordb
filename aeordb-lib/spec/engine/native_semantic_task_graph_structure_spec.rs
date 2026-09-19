//! B-tree range composition and ordinary streamed media, not semantic source bodies.
use super::*;

fn btree_leaf(publisher: &V4FirstAuthorityPublisher, name: &str, directory: &[u8]) -> Vec<u8> {
  let algorithm = publisher.observe().unwrap().selected.header.hash_algorithm;
  let row = crate::engine::directory_entry::serialize_child_entries(
    &[crate::engine::directory_entry::ChildEntry {
      entry_type: EntryTypeV4::DirectoryIndex.to_u8(),
      hash: directory.to_vec(),
      total_size: 0,
      created_at: 11,
      updated_at: 13,
      name: name.to_string(),
      content_type: None,
      virtual_time: 0,
      node_id: 0,
    }],
    algorithm.hash_length(),
  )
  .unwrap();
  let mut bytes = vec![0, 1, 0];
  bytes.extend_from_slice(&row);
  publish_namespace_value(publisher, EntryTypeV4::DirectoryIndex, 0, b"btree:", &bytes)
}

fn btree_parent(publisher: &V4FirstAuthorityPublisher, left: &[u8], right: &[u8], separator: &str) -> Vec<u8> {
  let mut bytes = vec![1, 1, 0];
  bytes.extend_from_slice(&(separator.len() as u16).to_le_bytes());
  bytes.extend_from_slice(separator.as_bytes());
  bytes.extend_from_slice(left);
  bytes.extend_from_slice(right);
  publish_namespace_value(publisher, EntryTypeV4::DirectoryIndex, 0, b"btree:", &bytes)
}

#[test]
fn native_semantic_task_graph_enforces_inherited_btree_ranges_and_child_shapes() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for case in ["valid", "range", "flat-child", "repeated-child"] {
      let (_directory, path, _coordinator, publisher) =
        create_environment_for_algorithm_at_kv_stage("task-graph-btree", None, [1; 16], algorithm, 0);
      let (mut expected, _, _) = seed_captured_graph(&publisher);
      let empty = publish_namespace_directory(&publisher, vec![]);
      let left = btree_leaf(&publisher, "a", &empty);
      let right = btree_leaf(&publisher, "z", &empty);
      let flat = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", empty.clone())]);
      let tree = btree_parent(
        &publisher,
        if case == "flat-child" { &flat } else { &left },
        if case == "repeated-child" { &left } else { &right },
        if case == "range" { "0" } else { "m" },
      );
      use_staged_tree(&publisher, &mut expected, &tree);
      let memory = observation_memory();
      let cancellation = CancellationToken::new();
      let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
      let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let before = fs::read(&path).unwrap();
      let mut visits = BTreeMap::<Vec<u8>, u64>::new();
      let result = capture.visit_captured_semantic_task_physical_entries(&[2; 16], graph_bounds(), |entry| {
        *visits.entry(entry.hash.clone()).or_default() += 1;
        Ok(())
      });
      if case == "valid" {
        assert_eq!(result.unwrap().namespace_directories, 4);
        for hash in [&left, &right, &tree] {
          assert_eq!(visits.get(hash), Some(&1));
        }
      } else {
        assert!(matches!(result.unwrap_err(), SemanticTaskGraphErrorV1::Namespace(_)), "{case}");
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn native_semantic_task_graph_streams_compressed_and_raw_v0_v1_files_beyond_source_body_cap() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for version in [0, 1] {
      for compression in [CompressionAlgorithm::None, CompressionAlgorithm::Zstd] {
        let (_directory, path, _coordinator, publisher) =
          create_environment_for_algorithm_at_kv_stage("task-graph-streamed-file", None, [1; 16], algorithm, 0);
        let (mut expected, _, _) = seed_captured_graph(&publisher);
        let raw = seed_raw_source(&publisher, "/media.bin", version, 0, 0, false, compression);
        let file = publish_namespace_value(&publisher, EntryTypeV4::FileRecord, version, b"filec:", &raw);
        let tree = publish_namespace_directory(
          &publisher,
          vec![NamespaceFixtureChild {
            name: "media.bin".into(),
            kind: EntryTypeV4::FileRecord,
            key: file,
            size: 11,
            content_type: Some("application/custom"),
          }],
        );
        use_staged_tree(&publisher, &mut expected, &tree);
        let memory = observation_memory();
        let cancellation = CancellationToken::new();
        let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
        let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
        let baseline = memory.snapshot().unwrap().reserved_bytes;
        let before = fs::read(&path).unwrap();
        let mut bounds = graph_bounds();
        bounds.maximum_decoded_chunk_bytes = 6;
        bounds.sources.maximum_source_bytes = 1;
        let (result, allocations) = measure(0, || capture.visit_captured_semantic_task_physical_entries(&[2; 16], bounds, |_| Ok(())));
        let result = result.unwrap();
        assert_eq!((result.namespace_files, result.namespace_chunks), (1, 2));
        assert!(!allocations.injected_failure);
        // Ordinary retention does not decode chunks, even when they exceed the
        // admitted decoded buffer. Deep inspection above keeps its full check.
        bounds.maximum_decoded_chunk_bytes = 1;
        let metadata = capture.visit_captured_semantic_task_metadata_entries(&[2; 16], bounds, |_| Ok(())).unwrap();
        assert_eq!((metadata.namespace_files, metadata.namespace_chunks), (1, 2));
        assert_eq!(metadata.physical_reads + 2, result.physical_reads);
        let chunks: u64 = [b"first".as_slice(), b"second".as_slice()]
          .iter()
          .map(|body| u64::from(publisher.locator(&digest_parts(algorithm, &[b"chunk:", body])).unwrap().unwrap().total_length))
          .sum();
        assert_eq!(metadata.read_bytes + chunks, result.read_bytes);
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
        assert_eq!(fs::read(&path).unwrap(), before);
      }
    }
  }
}
