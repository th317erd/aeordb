//! Exclusive captured configuration-file successors, including deleted bounds.
use super::*;

#[test]
fn namespace_cursor_seek_preserves_full_configuration_file_order_at_existing_and_absent_bounds() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-cursor-seek-order", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (deep, _) = namespace_configuration_tree(&publisher, "/a/deep", b"deep", vec![]);
    let (a, _) = namespace_configuration_tree(&publisher, "/a", b"a", vec![namespace_directory_child("deep", deep)]);
    let mut children = vec![namespace_directory_child("a", a)];
    for owner in ["a!", "a0", "z"] {
      let (root, _) = namespace_configuration_tree(&publisher, &format!("/{owner}"), owner.as_bytes(), vec![]);
      children.push(namespace_directory_child(owner, root));
    }
    let root = publish_namespace_directory(&publisher, children);
    let mut expected: Vec<_> = ["a", "a!", "a/deep", "a0", "z"].map(|owner| format!("/{owner}/.aeordb-config/indexes.json")).into();
    expected.sort();
    assert!(expected[0].starts_with("/a!/"));
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    let mut bounds = expected.clone();
    for owner in ["0", "a/missing", "missing", "zz"] {
      bounds.push(format!("/{owner}/.aeordb-config/indexes.json"));
    }
    for after in bounds {
      let mut cursor = capture
        .open_namespace_configuration_cursor_after(namespace_source_request(&root), &after)
        .expect("exclusive configuration-file seek must not require the bound to exist");
      let mut actual = Vec::new();
      while let Some(source) = cursor.next_source().unwrap() {
        actual.push(source.record().path.clone());
      }
      assert_eq!(actual, expected.iter().filter(|path| *path > &after).cloned().collect::<Vec<_>>(), "after {after}");
      assert!(cursor.next_source().unwrap().is_none());
      drop(cursor);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert_eq!(fs::read(&path).unwrap(), before);
    }
  }
}

#[test]
fn namespace_cursor_seek_uses_the_same_bound_for_base_and_requested_deletions() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (_directory, path, _coordinator, publisher) =
      create_environment_for_algorithm_at_kv_stage("namespace-cursor-seek-deletion", None, [1; 16], algorithm, 0);
    publisher.publish(&request_for_database_and_algorithm([1; 16], algorithm)).unwrap();
    let (a, _) = namespace_configuration_tree(&publisher, "/a", b"added", vec![]);
    let (b, _) = namespace_configuration_tree(&publisher, "/b", b"removed", vec![]);
    let (c, _) = namespace_configuration_tree(&publisher, "/c", b"retained", vec![]);
    let base = publish_namespace_directory(&publisher, vec![namespace_directory_child("b", b), namespace_directory_child("c", c.clone())]);
    let requested = publish_namespace_directory(&publisher, vec![namespace_directory_child("a", a), namespace_directory_child("c", c)]);
    let memory = observation_memory();
    let cancellation = CancellationToken::new();
    let protection = publisher.acquire_staging_protection(&memory, &cancellation).unwrap();
    let capture = protection.capture_semantic_mutation_inventory(capture_bounds(), &memory, &cancellation).unwrap();
    let before = fs::read(&path).unwrap();
    let retained = memory.snapshot().unwrap().reserved_bytes;
    for root in [&base, &requested] {
      let mut cursor = capture
        .open_namespace_configuration_cursor_after(namespace_source_request(root), "/b/.aeordb-config/indexes.json")
        .expect("a deleted bound still has a well-defined successor");
      let source = cursor.next_source().unwrap().unwrap();
      assert_eq!(source.record().path, "/c/.aeordb-config/indexes.json");
      assert_eq!(source.body(), b"retained");
      drop(source);
      assert!(cursor.next_source().unwrap().is_none());
      drop(cursor);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
    }
    assert_eq!(fs::read(&path).unwrap(), before);
  }
}
