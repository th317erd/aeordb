use super::*;

use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[test]
fn workspace_entry_capacity_accepts_the_exact_bound_and_rejects_excess_or_overflow() {
  let maximum_derived_entries = MAXIMUM_WORKSPACE_ENTRIES - SEALED_WORKSPACE_ENTRY_COUNT;
  ensure_workspace_entry_capacity(maximum_derived_entries).unwrap();

  let error = ensure_workspace_entry_capacity(maximum_derived_entries + 1).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");

  let error = ensure_workspace_entry_capacity(u64::MAX).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");
}

#[test]
fn initial_run_entry_peak_accounts_for_empty_single_and_merge_pending_runs() {
  assert_eq!(initial_run_peak_derived_entries(0, 8).unwrap(), 1);
  assert_eq!(initial_run_peak_derived_entries(8, 8).unwrap(), 1);
  assert_eq!(initial_run_peak_derived_entries(9, 8).unwrap(), 3);
  assert_eq!(initial_run_peak_derived_entries(17, 8).unwrap(), 4);

  let error = initial_run_peak_derived_entries(1, 0).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");

  let error = initial_run_peak_derived_entries(u64::MAX, 1).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");
}

#[test]
fn page_entry_peak_retains_the_final_run_and_rejects_overflow() {
  assert_eq!(page_peak_derived_entries(0).unwrap(), 1);
  assert_eq!(page_peak_derived_entries(7).unwrap(), 8);

  let error = page_peak_derived_entries(u64::MAX).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");
}

#[test]
fn pending_cleanup_carries_one_entry_budget_across_directories() {
  let temporary = tempdir().unwrap();
  let first = temporary.path().join("first");
  let second = temporary.path().join("second");
  create_private_directory_synced(&first, temporary.path()).unwrap();
  create_private_directory_synced(&second, temporary.path()).unwrap();
  let first_pending = first.join(".root-map-00000000000000000000000000000001.pending");
  let second_pending = second.join(".root-map-00000000000000000000000000000002.pending");
  for path in [&first_pending, &second_pending] {
    let mut file = create_new_regular_file_no_follow(path).unwrap();
    file.write_all(b"pending").unwrap();
    drop(file);
  }

  let cancellation = CancellationToken::new();
  let mut entries = MAXIMUM_WORKSPACE_ENTRIES - 1;
  remove_stale_pending_files(&first, "first pending directory", &cancellation, &mut entries).unwrap();
  assert_eq!(entries, MAXIMUM_WORKSPACE_ENTRIES);
  assert!(!first_pending.exists());

  let error = remove_stale_pending_files(&second, "second pending directory", &cancellation, &mut entries).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");
  assert!(second_pending.exists());
}
