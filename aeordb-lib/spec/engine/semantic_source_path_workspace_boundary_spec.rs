//! Scratch failures must remain errors, never an apparently complete source set.
use super::*;
use crate::engine::memory_coordinator::HostMemorySample;
use std::io::Write;

#[test]
fn semantic_source_path_workspace_io_refusal_retains_counter_and_diagnostic() {
  let context = Context {
    bounds: SemanticSourcePathWorkspaceBoundsV1 { maximum_io_bytes: 100, ..path_workspace_bounds() },
    memory: path_workspace_memory(),
    cancellation: CancellationToken::new(),
    io: AtomicU64::new(70),
  };
  let error = context.charge_io(31).unwrap_err();
  assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }));
  assert!(error.to_string().contains("used=70, requested=31, limit=100"), "{error}");
  assert_eq!(context.io.load(Ordering::Relaxed), 70);
  context.charge_io(30).unwrap();
  assert_eq!(context.io.load(Ordering::Relaxed), 100);
  let error = context.charge_io(u64::MAX).unwrap_err();
  assert!(error.to_string().contains("used=100, requested=18446744073709551615, limit=100"), "{error}");
  assert_eq!(context.io.load(Ordering::Relaxed), 100);
}

fn finished_paths(paths: &[&str]) -> (tempfile::TempDir, MemoryCoordinator, CancellationToken, SemanticSourcePathWorkspaceV1) {
  let parent = tempfile::tempdir().unwrap();
  let memory = path_workspace_memory();
  let cancellation = CancellationToken::new();
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), path_workspace_bounds(), &memory, &cancellation).unwrap();
  for path in paths {
    builder.append_path(path).unwrap();
  }
  let workspace = builder.finish().unwrap();
  (parent, memory, cancellation, workspace)
}

fn expect_bad_cursor(workspace: &SemanticSourcePathWorkspaceV1) {
  if let Ok(mut cursor) = workspace.open_cursor() {
    for _ in 0..10 {
      match cursor.next_path() {
        Err(_) => {
          assert!(cursor.next_path().is_err(), "failed cursor must remain failed");
          return;
        }
        Ok(Some(_)) => {}
        Ok(None) => panic!("damaged scratch state became a successful end"),
      }
    }
    panic!("damaged scratch produced an unbounded stream");
  }
}

#[test]
fn semantic_source_path_workspace_all_truncated_prefixes_and_header_mutations_refuse() {
  let (parent, memory, _, workspace) = finished_paths(&["/a", "/b", "/c"]);
  let path = run_path(workspace.directory.path(), workspace.run.unwrap().id);
  let original = std::fs::read(&path).unwrap();
  for length in 0..original.len() {
    std::fs::write(&path, &original[..length]).unwrap();
    expect_bad_cursor(&workspace);
  }
  for index in 0..32 {
    let mut damaged = original.clone();
    damaged[index] ^= 1;
    std::fs::write(&path, damaged).unwrap();
    expect_bad_cursor(&workspace);
  }
  std::fs::write(&path, &original).unwrap();
  assert_eq!(collect_paths(&mut workspace.open_cursor().unwrap()), ["/a", "/b", "/c"]);
  drop(workspace);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn semantic_source_path_workspace_rejects_bad_frame_lengths_checksums_paths_and_order() {
  let (_parent, memory, _, workspace) = finished_paths(&["/a", "/b", "/c"]);
  let path = run_path(workspace.directory.path(), workspace.run.unwrap().id);
  let original = std::fs::read(&path).unwrap();
  for length in [0u32, 1, 3, 1025, u32::MAX] {
    let mut damaged = original.clone();
    damaged[32..36].copy_from_slice(&length.to_le_bytes());
    std::fs::write(&path, damaged).unwrap();
    expect_bad_cursor(&workspace);
  }
  for index in 36..original.len() {
    let mut damaged = original.clone();
    damaged[index] ^= 1;
    std::fs::write(&path, damaged).unwrap();
    expect_bad_cursor(&workspace);
  }
  for bytes in [b"zz".as_slice(), b"//", b"/\0", &[b'/', 0xff], b"/z", b"/b"] {
    let mut damaged = original.clone();
    damaged[40..42].copy_from_slice(bytes);
    damaged[36..40].copy_from_slice(&crc32fast::hash(bytes).to_le_bytes());
    std::fs::write(&path, damaged).unwrap();
    expect_bad_cursor(&workspace);
  }
  std::fs::write(&path, original).unwrap();
  let mut cursor = workspace.open_cursor().unwrap();
  std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"x").unwrap();
  for _ in 0..3 {
    assert!(cursor.next_path().unwrap().is_some());
  }
  assert!(cursor.next_path().is_err(), "trailing bytes added after open must be observed");
  assert!(cursor.next_path().is_err());
  drop(cursor);
  drop(workspace);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn semantic_source_path_workspace_rejects_invalid_bounds_before_creating_files() {
  let parent = tempfile::tempdir().unwrap();
  let memory = path_workspace_memory();
  let cancellation = CancellationToken::new();
  let valid = path_workspace_bounds();
  let cases = [
    SemanticSourcePathWorkspaceBoundsV1 { maximum_input_paths: 0, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_input_paths: u64::MAX, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_path_bytes: 0, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_path_bytes: 65_536, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_sort_bytes: 0, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_sort_bytes: 1, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_sort_bytes: u64::MAX, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_stored_bytes: 0, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_stored_bytes: u64::MAX, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_io_bytes: 0, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_io_bytes: u64::MAX, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_paths_per_run: 0, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { maximum_paths_per_run: usize::MAX, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { merge_fan_in: 1, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { merge_fan_in: 65, ..valid },
    SemanticSourcePathWorkspaceBoundsV1 { minimum_free_bytes: u64::MAX, ..valid },
  ];
  for bounds in cases {
    assert!(SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
  }
}

#[test]
fn semantic_source_path_workspace_bad_paths_and_count_limits_poison_the_builder() {
  let parent = tempfile::tempdir().unwrap();
  let memory = path_workspace_memory();
  let cancellation = CancellationToken::new();
  for path in ["", "relative", "/a/", "//a", "/a//b", "/./a", "/../a", "/a\0", "/a ", "/12345"] {
    let bounds = SemanticSourcePathWorkspaceBoundsV1 { maximum_path_bytes: 5, ..path_workspace_bounds() };
    let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
    assert!(matches!(builder.append_path(path), Err(SemanticCompilationErrorV1::InvalidSource { .. })));
    assert!(builder.append_path("/ok").is_err());
    assert!(builder.finish().is_err());
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
  }
  let bounds = SemanticSourcePathWorkspaceBoundsV1 { maximum_input_paths: 1, ..path_workspace_bounds() };
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
  builder.append_path("/a").unwrap();
  assert!(matches!(builder.append_path("/a"), Err(SemanticCompilationErrorV1::Resource { .. })));
  assert!(builder.finish().is_err());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn semantic_source_path_workspace_cumulative_io_and_simultaneous_storage_limits_release_and_retry() {
  for storage in [false, true] {
    let parent = tempfile::tempdir().unwrap();
    let memory = path_workspace_memory();
    let cancellation = CancellationToken::new();
    let mut saw_success = false;
    let mut refusals = 0;
    for limit in (32..1500).step_by(11) {
      let mut bounds = path_workspace_bounds();
      bounds.maximum_paths_per_run = 1;
      if storage {
        bounds.maximum_stored_bytes = limit;
      } else {
        bounds.maximum_io_bytes = limit;
      }
      let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
      let result = (|| {
        for path in ["/c", "/a", "/b", "/a"] {
          builder.append_path(path)?;
        }
        let workspace = builder.finish()?;
        let mut cursor = workspace.open_cursor()?;
        let mut actual = Vec::new();
        while let Some(path) = cursor.next_path()? {
          actual.push(path.as_str().to_owned());
        }
        assert_eq!(actual, ["/a", "/b", "/c"]);
        assert!(workspace.statistics().peak_stored_bytes <= bounds.maximum_stored_bytes);
        assert!(workspace.statistics().io_bytes <= bounds.maximum_io_bytes);
        Ok(())
      })();
      match result {
        Ok(()) => saw_success = true,
        Err(error) => {
          assert!(matches!(error, SemanticCompilationErrorV1::Resource { .. }), "{error:?}");
          refusals += 1;
        }
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
      if saw_success {
        break;
      }
    }
    assert!(saw_success && refusals > 0);
  }
}

#[test]
fn semantic_source_path_workspace_cancellation_and_pressure_cover_empty_end_and_poisoned_work() {
  for pressure in [false, true] {
    for paths in [&[][..], &["/a", "/b"][..]] {
      let (parent, memory, cancellation, workspace) = finished_paths(paths);
      let mut cursor = workspace.open_cursor().unwrap();
      let active = if pressure {
        memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
        true
      } else {
        cancellation.cancel();
        false
      };
      assert!(workspace.open_cursor().is_err());
      let error = cursor.next_path().err().unwrap();
      assert!(if active {
        matches!(error, SemanticCompilationErrorV1::Resource { .. })
      } else {
        matches!(error, SemanticCompilationErrorV1::Cancelled)
      });
      memory.update_host_sample(HostMemorySample::default()).unwrap();
      assert!(cursor.next_path().is_err());
      drop(cursor);
      drop(workspace);
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }
    for finish in [false, true] {
      let parent = tempfile::tempdir().unwrap();
      let memory = path_workspace_memory();
      let cancellation = CancellationToken::new();
      let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), path_workspace_bounds(), &memory, &cancellation).unwrap();
      builder.append_path("/a").unwrap();
      if pressure {
        memory.update_host_sample(HostMemorySample { rss_bytes: 96 << 20, ..HostMemorySample::default() }).unwrap();
      } else {
        cancellation.cancel();
      }
      if !finish {
        assert!(builder.append_path("/b").is_err());
        memory.update_host_sample(HostMemorySample::default()).unwrap();
        assert!(builder.append_path("/c").is_err());
      }
      assert!(builder.finish().is_err());
      assert!(
        SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), path_workspace_bounds(), &memory, &cancellation).is_err()
          || pressure && !finish
      );
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
      assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }
  }
}

#[test]
fn semantic_source_path_workspace_missing_run_is_not_empty_and_rows_retain_their_memory() {
  let (parent, memory, _, workspace) = finished_paths(&["/a", "/b"]);
  let mut cursor = workspace.open_cursor().unwrap();
  let row = cursor.next_path().unwrap().unwrap();
  drop(cursor);
  let path = run_path(workspace.directory.path(), workspace.run.unwrap().id);
  std::fs::remove_file(path).unwrap();
  assert!(workspace.open_cursor().is_err());
  drop(workspace);
  assert!(memory.snapshot().unwrap().reserved_bytes > 0);
  assert_eq!(row.as_str(), "/a");
  drop(row);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}
