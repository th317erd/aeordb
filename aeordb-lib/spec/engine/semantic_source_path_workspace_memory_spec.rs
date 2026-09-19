use super::*;
#[test]
fn semantic_source_path_workspace_memory_admission_never_exceeds_the_sort_ceiling() {
  for maximum_path_bytes in [64, 1024] {
    let parent = tempfile::tempdir().unwrap();
    let memory = path_workspace_memory();
    let cancellation = CancellationToken::new();
    let bounds = SemanticSourcePathWorkspaceBoundsV1 {
      maximum_path_bytes,
      maximum_sort_bytes: 64 << 10,
      maximum_paths_per_run: 10_000,
      ..path_workspace_bounds()
    };
    let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
    let path = format!("/{}", "x".repeat(maximum_path_bytes - 1));
    for _ in 0..builder.window - 1 {
      builder.append_path(&path).unwrap();
      assert!(
        memory.snapshot().unwrap().reserved_bytes <= bounds.maximum_sort_bytes,
        "admitted {} bytes beyond sort ceiling {}",
        memory.snapshot().unwrap().reserved_bytes,
        bounds.maximum_sort_bytes
      );
    }
    drop(builder);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
  }
}

#[test]
fn semantic_source_path_workspace_peak_admission_includes_sorting_merging_and_finished_readers() {
  for fan_in in [2, 3, 7] {
    for maximum_path_bytes in [64, 1024] {
      let parent = tempfile::tempdir().unwrap();
      let memory = path_workspace_memory();
      let cancellation = CancellationToken::new();
      let bounds = SemanticSourcePathWorkspaceBoundsV1 {
        maximum_path_bytes,
        maximum_sort_bytes: if fan_in == 7 && maximum_path_bytes == 1024 { 128 << 10 } else { 64 << 10 },
        maximum_paths_per_run: 10_000,
        merge_fan_in: fan_in,
        ..path_workspace_bounds()
      };
      if fan_in == 7 && maximum_path_bytes == 1024 {
        let insufficient = SemanticSourcePathWorkspaceBoundsV1 { maximum_sort_bytes: 64 << 10, ..bounds };
        assert!(matches!(
          SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), insufficient, &memory, &cancellation),
          Err(SemanticCompilationErrorV1::InvalidSource { .. })
        ));
        assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
      }
      let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
      let count = builder.window * (fan_in + 1) + 1;
      let mut expected = std::collections::BTreeSet::new();
      for index in 0..count {
        let prefix = format!("/{:05}", index % 103);
        let path = format!("{prefix}{}", "x".repeat(maximum_path_bytes - prefix.len()));
        builder.append_path(&path).unwrap();
        expected.insert(path);
      }
      let workspace = builder.finish().unwrap();
      assert_eq!(collect_paths(&mut workspace.open_cursor().unwrap()), expected.into_iter().collect::<Vec<_>>());
      let snapshot = memory.snapshot().unwrap();
      let peak = snapshot.owner(MemoryOwner::Task).unwrap().peak_reserved_bytes;
      assert!(peak <= bounds.maximum_sort_bytes, "fan={fan_in} width={maximum_path_bytes} peak={peak}");
      drop(workspace);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }
  }
}
