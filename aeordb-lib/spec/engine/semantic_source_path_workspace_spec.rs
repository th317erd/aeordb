//! Independent path ordering, bounded run ownership and native scratch lifetime.
#[path = "semantic_source_path_workspace_boundary_spec.rs"]
mod boundary;
#[path = "semantic_source_path_workspace_memory_spec.rs"]
mod memory_bounds;
#[path = "semantic_source_path_workspace_write_spec.rs"]
mod writes;
use super::*;
use crate::engine::memory_coordinator::MemoryPolicy;

fn path_workspace_memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap())
}

fn path_workspace_bounds() -> SemanticSourcePathWorkspaceBoundsV1 {
  SemanticSourcePathWorkspaceBoundsV1 {
    maximum_input_paths: 10_000,
    maximum_path_bytes: 1024,
    maximum_sort_bytes: 1 << 20,
    maximum_stored_bytes: 8 << 20,
    maximum_io_bytes: 64 << 20,
    maximum_paths_per_run: 3,
    merge_fan_in: 2,
    minimum_free_bytes: 0,
  }
}

fn collect_paths(cursor: &mut SemanticSourcePathCursorV1<'_>) -> Vec<String> {
  let mut paths = Vec::new();
  while let Some(path) = cursor.next_path().unwrap() {
    paths.push(path.as_str().to_owned());
  }
  paths
}

#[test]
fn semantic_source_path_workspace_sorts_and_deduplicates_across_many_small_runs() {
  for fan_in in [2, 3, 4] {
    let parent = tempfile::tempdir().unwrap();
    let memory = path_workspace_memory();
    let cancellation = CancellationToken::new();
    let bounds = SemanticSourcePathWorkspaceBoundsV1 { merge_fan_in: fan_in, ..path_workspace_bounds() };
    let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation)
      .expect("bounded source path workspace must admit a small sort window");
    for index in (0..137).rev() {
      let path = format!("/sources/{index:04}");
      builder.append_path(&path).unwrap();
      builder.append_path(&path).unwrap();
    }
    let workspace = builder.finish().unwrap();
    assert_eq!(workspace.path_count(), 137);
    let statistics = workspace.statistics();
    assert_eq!(statistics.input_paths, 274);
    assert!(statistics.initial_runs > 32);
    assert!(statistics.peak_retained_runs < 32);
    assert!(statistics.peak_open_inputs <= fan_in);
    assert!(statistics.peak_stored_bytes <= bounds.maximum_stored_bytes);
    assert!(statistics.io_bytes <= bounds.maximum_io_bytes);
    let expected: Vec<String> = (0..137).map(|index| format!("/sources/{index:04}")).collect();
    let mut cursor = workspace.open_cursor().unwrap();
    assert_eq!(collect_paths(&mut cursor), expected);
    assert!(cursor.next_path().unwrap().is_none());
    drop(cursor);
    drop(workspace);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
  }
}

#[test]
fn semantic_source_path_workspace_orders_complete_utf8_paths_and_keeps_cursor_positions_independent() {
  let parent = tempfile::tempdir().unwrap();
  let memory = path_workspace_memory();
  let cancellation = CancellationToken::new();
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), path_workspace_bounds(), &memory, &cancellation)
    .expect("source path ordering must support independent readers");
  for path in ["/a/child", "/z", "/é", "/a.", "/a", "/a-", "/a/child", "/猫"] {
    builder.append_path(path).unwrap();
  }
  let workspace = builder.finish().unwrap();
  assert_eq!(workspace.path_count(), 7);
  let mut left = workspace.open_cursor().unwrap();
  let mut right = workspace.open_cursor().unwrap();
  assert_eq!(left.next_path().unwrap().unwrap().as_str(), "/a");
  assert_eq!(right.next_path().unwrap().unwrap().as_str(), "/a");
  assert_eq!(right.next_path().unwrap().unwrap().as_str(), "/a-");
  assert_eq!(left.next_path().unwrap().unwrap().as_str(), "/a-");
  let remaining = ["/a.", "/a/child", "/z", "/é", "/猫"];
  assert_eq!(collect_paths(&mut left), remaining);
  assert_eq!(collect_paths(&mut right), remaining);
  drop(left);
  drop(right);
  drop(workspace);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn semantic_source_path_workspace_empty_and_exact_boundary_inputs_preserve_unrelated_files() {
  let parent = tempfile::tempdir().unwrap();
  let sibling = parent.path().join("keep-user-file.txt");
  std::fs::write(&sibling, b"unrelated bytes").unwrap();
  let memory = path_workspace_memory();
  let cancellation = CancellationToken::new();
  let empty = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), path_workspace_bounds(), &memory, &cancellation)
    .expect("empty scratch input is valid without implying complete source capture")
    .finish()
    .unwrap();
  assert_eq!(empty.path_count(), 0);
  assert!(empty.open_cursor().unwrap().next_path().unwrap().is_none());
  drop(empty);
  let bounds = SemanticSourcePathWorkspaceBoundsV1 {
    maximum_input_paths: 1,
    maximum_path_bytes: 4,
    maximum_paths_per_run: 1,
    ..path_workspace_bounds()
  };
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
  builder.append_path("/abc").unwrap();
  let workspace = builder.finish().unwrap();
  assert_eq!(workspace.path_count(), 1);
  assert_eq!(collect_paths(&mut workspace.open_cursor().unwrap()), ["/abc"]);
  drop(workspace);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(std::fs::read(&sibling).unwrap(), b"unrelated bytes");
  let remaining: Vec<_> = std::fs::read_dir(parent.path()).unwrap().map(|entry| entry.unwrap().path()).collect();
  assert_eq!(remaining, [sibling]);
}
