use super::*;
use aeordb::engine::v4::namespace::SemanticUnavailableReasonV1;

fn seek_child(name: &str, hash: Vec<u8>, entry_type: EntryTypeV4) -> ChildEntry {
  ChildEntry {
    entry_type: entry_type.to_u8(),
    hash,
    total_size: 0,
    created_at: 1_700_000_000_400,
    updated_at: 1_700_000_000_400,
    name: name.to_string(),
    content_type: None,
    virtual_time: 1,
    node_id: 1,
  }
}

fn publish_prefix_tree(publisher: &V4FirstAuthorityPublisher, algorithm: HashAlgorithm, expected_head_hash: Vec<u8>) {
  let mut entities = Vec::new();
  let mut files = BTreeMap::new();
  for path in ["/docs/a/inside", "/docs/a-", "/docs/a.", "/docs/z"] {
    let record = FileRecord {
      path: path.to_string(),
      content_type: None,
      total_size: 0,
      created_at: 1_700_000_000_400,
      updated_at: 1_700_000_000_400,
      metadata: Vec::new(),
      content_hash: digest_parts(algorithm, &[b""]),
      chunk_hashes: Vec::new(),
    };
    let bytes = record.serialize_for_version(algorithm.hash_length(), 1).unwrap();
    let identity = digest_parts(algorithm, &[b"filec:", &bytes]);
    files.insert(path, identity.clone());
    entities.push((1, EntryTypeV4::FileRecord, identity, bytes));
  }
  let nested =
    serialize_child_entries(&[seek_child("inside", files["/docs/a/inside"].clone(), EntryTypeV4::FileRecord)], algorithm.hash_length())
      .unwrap();
  let nested_id = digest_parts(algorithm, &[b"dirc:", &nested]);
  let docs = serialize_child_entries(
    &[
      seek_child("a", nested_id.clone(), EntryTypeV4::DirectoryIndex),
      seek_child("a-", files["/docs/a-"].clone(), EntryTypeV4::FileRecord),
      seek_child("a.", files["/docs/a."].clone(), EntryTypeV4::FileRecord),
      seek_child("z", files["/docs/z"].clone(), EntryTypeV4::FileRecord),
    ],
    algorithm.hash_length(),
  )
  .unwrap();
  let docs_id = digest_parts(algorithm, &[b"dirc:", &docs]);
  let root = serialize_child_entries(&[seek_child("docs", docs_id.clone(), EntryTypeV4::DirectoryIndex)], algorithm.hash_length()).unwrap();
  let root_id = digest_parts(algorithm, &[b"dirc:", &root]);
  entities.push((0, EntryTypeV4::DirectoryIndex, nested_id, nested));
  entities.push((0, EntryTypeV4::DirectoryIndex, docs_id, docs));
  entities.push((0, EntryTypeV4::DirectoryIndex, root_id.clone(), root.clone()));
  let writes: Vec<_> = entities
    .iter()
    .map(|(entity_version, entry_type, key, stored_value)| ImmutableEntityWriteV1 {
      entity_version: *entity_version,
      entry_type: *entry_type,
      flags: 0,
      key,
      stored_value,
    })
    .collect();
  publisher
    .publish_immutable_entity_batch(ImmutableEntityBatchPublicationRequestV1 {
      database_id: &[0x31; 16],
      entities: &writes,
      publication_timestamp_ms: 1_700_000_000_400,
    })
    .unwrap();
  publisher
    .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
      database_id: [0x31; 16],
      transaction_id: [0x78; 16],
      created_at_ms: 1_700_000_000_401,
      expected_head_hash,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: root_id, stored_value: root },
      semantic_state: semantic_state(algorithm, SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured),
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"namespace seek prefix fixture"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap();
}

#[test]
fn selected_namespace_seek_orders_prefix_directories_by_complete_path_bytes_across_pages() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    publish_prefix_tree(&publisher, algorithm, first.namespace_root.root_hash);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
    let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
    let reader = source
      .selected_namespace_reader(&view, NativeSelectedNamespaceLimitsV1::new(1, 1024 * 1024, 256, 32, 1024, 10_000).unwrap())
      .unwrap();
    let before = fs::read(&path).unwrap();
    let mut expected = ["/docs/a/inside", "/docs/a-", "/docs/a.", "/docs/z"];
    expected.sort();
    let mut actual = Vec::new();
    let mut resume = None;
    // Bounded even if a broken implementation repeats its resume path.
    for _ in 0..=expected.len() {
      let page = reader.scan_files("/docs", resume.as_deref()).unwrap();
      actual.extend(page.rows().iter().map(|row| row.path().to_string()));
      if page.complete() {
        assert!(page.next_resume_after().is_none());
        break;
      }
      let next = page.next_resume_after().unwrap();
      assert_ne!(resume.as_deref(), Some(next));
      resume = Some(next.to_string());
    }
    assert_eq!(actual, expected);
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(reader);
    drop(view);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn selected_namespace_seek_reaches_a_late_btree_page_without_rescanning_the_prefix() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    // The existing fixture makes exactly two canonical 40-entry leaves.
    let names: Vec<String> = (0..80).map(|index| format!("{index:02}")).collect();
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let (selected_root, _) =
      publish_file_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, true, &borrowed, FileTreeCorruption::None);
    let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 * 1024 * 1024, 512 * 1024 * 1024, 1, 1024 * 1024).unwrap()));
    let source = Arc::new(NativeReadViewSourceV1::new(Arc::new(publisher), Arc::clone(&memory), 86_400_000));
    let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
    let current = CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    );
    let authorizer =
      ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
    let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();
    let limits = NativeSelectedNamespaceLimitsV1::new(2, 1024 * 1024, 256, 32, 64, 10_000).unwrap();
    let reader = source.selected_namespace_reader(&view, limits).unwrap();
    let before = fs::read(&path).unwrap();
    let page = reader.scan_files("/docs", Some("/docs/75")).unwrap();
    assert_eq!(page.selected_root(), selected_root);
    assert_eq!(page.rows().iter().map(|row| row.path()).collect::<Vec<_>>(), ["/docs/76", "/docs/77"]);
    assert_eq!(page.next_resume_after(), Some("/docs/77"));
    assert!(!page.complete());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(page);
    let final_page = reader.scan_files("/docs", Some("/docs/77")).unwrap();
    assert_eq!(final_page.rows().iter().map(|row| row.path()).collect::<Vec<_>>(), ["/docs/78", "/docs/79"]);
    assert!(final_page.complete());
    assert!(final_page.next_resume_after().is_none());
    drop(final_page);
    drop(reader);
    drop(view);
    assert_eq!(pins.active_pin_count().unwrap(), 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

fn with_seek_view(
  algorithm: HashAlgorithm,
  publisher: Arc<V4FirstAuthorityPublisher>,
  action: impl FnOnce(&NativeReadViewSourceV1, &ResolvedReadViewV1<ResolvedPathAuthorizationV1>, &MemoryCoordinator, &CancellationToken),
) {
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(256 << 20, 512 << 20, 1, 1 << 20).unwrap()));
  let source = Arc::new(NativeReadViewSourceV1::new(publisher, Arc::clone(&memory), 86_400_000));
  let pins = RootReadPinCoordinatorV1::new(Arc::clone(&memory), algorithm, 8, 16).unwrap();
  let current = CurrentReadAuthorizationV1::new(
    CurrentPathAuthorizationV1::for_root("/docs/", CrudlifyOp::List),
    ReadViewCredentialKindV1::Ordinary,
    ReadViewConcealmentV1::Conceal,
  );
  let authorizer = ReadViewPermissionAuthorizerV1::new(CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)), source.as_ref().clone());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pins.clone(), all_capabilities_profile());
  let cancellation = CancellationToken::new();
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &cancellation).unwrap();
  action(source.as_ref(), &view, memory.as_ref(), &cancellation);
  drop(view);
  assert_eq!(pins.active_pin_count().unwrap(), 0);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn selected_namespace_seek_rejects_non_file_resumes_and_releases_pressure_and_cancellation() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    publish_prefix_tree(&publisher, algorithm, first.namespace_root.root_hash);
    // Keep the fixture writer alive across the comparison: its eventual Drop
    // flushes fixture publication buffers, independently of read-view lifetime.
    let publisher = Arc::new(publisher);
    let before = fs::read(&path).unwrap();
    with_seek_view(algorithm, Arc::clone(&publisher), |source, view, memory, cancellation| {
      let reader =
        source.selected_namespace_reader(view, NativeSelectedNamespaceLimitsV1::new(1, 1 << 20, 256, 32, 1024, 10_000).unwrap()).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      for resume in ["/docs/a", "/docs/missing"] {
        let error = match reader.scan_files("/docs", Some(resume)) {
          Ok(_) => panic!("non-file resume accepted"),
          Err(error) => error,
        };
        assert_eq!(error.code(), "selected_namespace_resume_missing");
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      }
      // Workload admission uses the ordinary hard limit, not the soft cache
      // limit. Leave room for the page reservation but not seek workspace.
      let ordinary_limit = memory.snapshot().unwrap().policy.unwrap().ordinary_limit_bytes();
      let pressure = memory
        .reserve(
          aeordb::engine::memory_coordinator::MemoryOwner::Query,
          ordinary_limit - baseline - (1 << 20) - 1,
          aeordb::engine::memory_coordinator::AdmissionClass::Workload,
        )
        .unwrap();
      let error = match reader.scan_files("/docs", None) {
        Ok(_) => panic!("workspace pressure accepted"),
        Err(error) => error,
      };
      assert_eq!(error.code(), "selected_namespace_workspace_memory");
      drop(pressure);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      drop(reader.scan_files("/docs", None).unwrap());
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
      cancellation.cancel();
      let error = match reader.scan_files("/docs", None) {
        Ok(_) => panic!("cancelled seek accepted"),
        Err(error) => error,
      };
      assert_eq!(error.code(), "selected_namespace_cancelled");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    });
    assert!(fs::read(&path).unwrap() == before, "read-view operations changed fixture bytes");
  }
}

#[test]
fn selected_namespace_seek_rejects_corrupt_inherited_ranges_before_emitting_a_page() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, publisher) = publisher(algorithm);
    let first = publisher.publish(&first_request(algorithm)).unwrap();
    // Each leaf is locally sorted, but the left leaf is above the root's 'm'
    // separator. Physical wrappers and content hashes remain valid.
    publish_file_tree(&publisher, algorithm, first.namespace_root.root_hash, 1, true, &["z", "zz", "m", "n"], FileTreeCorruption::None);
    let publisher = Arc::new(publisher);
    let before = fs::read(&path).unwrap();
    with_seek_view(algorithm, Arc::clone(&publisher), |source, view, memory, _| {
      let reader =
        source.selected_namespace_reader(view, NativeSelectedNamespaceLimitsV1::new(1, 1 << 20, 256, 32, 1024, 10_000).unwrap()).unwrap();
      let baseline = memory.snapshot().unwrap().reserved_bytes;
      let error = match reader.scan_files("/docs", None) {
        Ok(_) => panic!("corrupt range produced a page"),
        Err(error) => error,
      };
      assert_eq!(error.code(), "selected_namespace_btree_range");
      assert_eq!(error.class(), NativeSelectedNamespaceReadErrorClassV1::Corrupt);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, baseline);
    });
    assert!(fs::read(&path).unwrap() == before, "failed range validation changed fixture bytes");
  }
}
