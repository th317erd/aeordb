//! Use the library's existing thread-local allocator, never a second allocator.
use super::allocation_probe::measure;
use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use crate::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use crate::engine::v4::semantic_source_capture::{SemanticSourcePathWorkspaceBoundsV1, SemanticSourcePathWorkspaceBuilderV1};
use tokio_util::sync::CancellationToken;

fn environment() -> (tempfile::TempDir, MemoryCoordinator, CancellationToken, SemanticSourcePathWorkspaceBoundsV1) {
  (
    tempfile::tempdir().unwrap(),
    MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap()),
    CancellationToken::new(),
    SemanticSourcePathWorkspaceBoundsV1 {
      maximum_input_paths: 10,
      maximum_path_bytes: 1024,
      maximum_sort_bytes: 1 << 20,
      maximum_stored_bytes: 1 << 20,
      maximum_io_bytes: 1 << 20,
      maximum_paths_per_run: 3,
      merge_fan_in: 2,
      minimum_free_bytes: 0,
    },
  )
}

#[test]
fn semantic_source_path_workspace_append_allocation_refuses_without_a_finished_subset() {
  let (parent, memory, cancellation, bounds) = environment();
  let path = format!("/{}", "a".repeat(996));
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let (result, allocations) = measure(997, || builder.append_path(&path));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  assert!(builder.append_path("/next").is_err());
  assert!(builder.finish().is_err());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
  let mut retry = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
  retry.append_path(&path).unwrap();
  assert_eq!(retry.finish().unwrap().path_count(), 1);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn semantic_source_path_workspace_finish_validation_allocation_refuses_and_cleans_owned_files() {
  let (parent, memory, cancellation, bounds) = environment();
  let path = format!("/{}", "a".repeat(996));
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
  builder.append_path(&path).unwrap();
  let (result, allocations) = measure(997, || builder.finish());
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn semantic_source_path_workspace_cursor_window_and_row_allocation_refuse_and_new_cursor_retries() {
  let (parent, memory, cancellation, bounds) = environment();
  let path = format!("/{}", "a".repeat(996));
  let mut builder = SemanticSourcePathWorkspaceBuilderV1::new(parent.path(), bounds, &memory, &cancellation).unwrap();
  builder.append_path(&path).unwrap();
  let workspace = builder.finish().unwrap();
  let retained = memory.snapshot().unwrap().reserved_bytes;
  let (result, allocations) = measure(1024, || workspace.open_cursor());
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  drop(result);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
  let mut cursor = workspace.open_cursor().unwrap();
  let retained_cursor = memory.snapshot().unwrap().reserved_bytes;
  let (result, allocations) = measure(997, || cursor.next_path());
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained_cursor);
  assert!(cursor.next_path().is_err());
  drop(cursor);
  let mut retry = workspace.open_cursor().unwrap();
  assert_eq!(retry.next_path().unwrap().unwrap().as_str(), path);
  assert!(retry.next_path().unwrap().is_none());
  drop(retry);
  drop(workspace);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}
