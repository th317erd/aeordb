use super::*;

struct PartialWriter {
  remaining: usize,
  written: Vec<u8>,
}

impl std::io::Write for PartialWriter {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    if self.remaining == 0 {
      return Err(std::io::Error::other("injected partial scratch write"));
    }
    let count = self.remaining.min(bytes.len());
    self.written.extend_from_slice(&bytes[..count]);
    self.remaining -= count;
    Ok(count)
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

#[test]
fn semantic_source_path_workspace_every_partial_record_write_is_an_operational_error() {
  let memory = path_workspace_memory();
  let cancellation = CancellationToken::new();
  let context = Context { bounds: path_workspace_bounds(), memory: memory.clone(), cancellation, io: AtomicU64::new(0) };
  let path = "/bounded/path";
  for allowed in 0..8 + path.len() {
    let mut writer = PartialWriter { remaining: allowed, written: Vec::new() };
    assert!(matches!(write_path(&context, &mut writer, path), Err(SemanticCompilationErrorV1::Operational { .. })));
    assert_eq!(writer.written.len(), allowed);
  }
  let mut writer = PartialWriter { remaining: 8 + path.len(), written: Vec::new() };
  write_path(&context, &mut writer, path).unwrap();
  assert_eq!(&writer.written[8..], path.as_bytes());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn semantic_source_path_workspace_run_collision_preserves_bytes_and_poisons_build() {
  let parent = tempfile::tempdir().unwrap();
  let memory = path_workspace_memory();
  let cancellation = CancellationToken::new();
  let bounds = SemanticSourcePathWorkspaceBoundsV1 { maximum_paths_per_run: 1, ..path_workspace_bounds() };
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
  let collision = run_path(builder.directory.path(), 0);
  std::fs::write(&collision, b"existing owned scratch bytes").unwrap();
  assert!(matches!(builder.append_path("/a"), Err(SemanticCompilationErrorV1::Operational { .. })));
  assert_eq!(std::fs::read(collision).unwrap(), b"existing owned scratch bytes");
  assert!(builder.append_path("/b").is_err());
  assert!(builder.finish().is_err());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn semantic_source_path_workspace_varied_partitioning_matches_an_independent_set() {
  for fan_in in [2, 3, 7] {
    for window in [1, 2, 7, 97] {
      let parent = tempfile::tempdir().unwrap();
      let memory = path_workspace_memory();
      let cancellation = CancellationToken::new();
      let bounds = SemanticSourcePathWorkspaceBoundsV1 { maximum_paths_per_run: window, merge_fan_in: fan_in, ..path_workspace_bounds() };
      let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
      let mut expected = std::collections::BTreeSet::new();
      let mut state = 0x1234abcd_u64;
      for _ in 0..97 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let path = format!("/sources/{:03}", (state >> 32) % 41);
        builder.append_path(&path).unwrap();
        expected.insert(path);
      }
      let workspace = builder.finish().unwrap();
      assert_eq!(workspace.path_count(), expected.len() as u64);
      assert_eq!(collect_paths(&mut workspace.open_cursor().unwrap()), expected.into_iter().collect::<Vec<_>>());
      assert!(workspace.statistics().peak_open_inputs <= fan_in);
      drop(workspace);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }
  }
}
