use std::fs;
use std::path::Path;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::migration_cutover_journal::{
  CutoverJournalFailureDispositionV1, CutoverJournalFaultInjectorV1, CutoverJournalPublicationBoundaryV1, CutoverJournalWorkspaceOptionsV1,
  DurableCutoverJournalWorkspaceV1, encode_cutover_journal_pair_v1, encode_cutover_journal_slot_v1,
};
use aeordb::engine::v4::system_control::{SystemControlSlotV1, select_cutover_journal};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn fixture(folder: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{folder}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap())
}

fn workspace_options(minimum_free_bytes: u64) -> CutoverJournalWorkspaceOptionsV1 {
  CutoverJournalWorkspaceOptionsV1::new(minimum_free_bytes)
}

fn updated_control() -> Vec<u8> {
  let mut control = fixture("system-control-v1", "control-blake3-256-side-by-side-cutover-valid.bin");
  control[164..172].copy_from_slice(&1_800_000_000_000i64.to_le_bytes());
  let crc_offset = control.len() - 4;
  let crc = crc32fast::hash(&control[..crc_offset]);
  control[crc_offset..].copy_from_slice(&crc.to_le_bytes());
  control
}

fn create_workspace(root: &Path, memory: &MemoryCoordinator) -> (DurableCutoverJournalWorkspaceV1, Vec<u8>) {
  let control = fixture("system-control-v1", "control-blake3-256-side-by-side-cutover-valid.bin");
  let workspace = DurableCutoverJournalWorkspaceV1::create(
    &root.join("cutover-workspace"),
    11,
    12,
    &control,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    CancellationToken::new(),
    memory,
  )
  .unwrap();
  (workspace, control)
}

fn assert_fixture_round_trip(algorithm: HashAlgorithm, profile: &str) {
  let control = fixture("system-control-v1", &format!("control-{profile}-side-by-side-cutover-valid.bin"));
  let expected = fixture("cutover-journal-v1", &format!("cutover-{profile}-external-journal-valid.bin"));

  let encoded = encode_cutover_journal_pair_v1(11, 12, &control, algorithm).unwrap();
  assert_eq!(encoded, expected);
  assert_eq!(encode_cutover_journal_slot_v1(11, &control, algorithm).unwrap(), expected[..1_024]);
  assert_eq!(encode_cutover_journal_slot_v1(12, &control, algorithm).unwrap(), expected[1_024..]);

  let selected = select_cutover_journal(&encoded, algorithm).unwrap();
  assert_eq!(selected.selected_slot, SystemControlSlotV1::B);
  assert_eq!(selected.sequence, 12);
  assert!(!selected.redundancy_degraded);
}

#[test]
fn cutover_journal_writer_matches_both_independent_frozen_fixtures() {
  assert_fixture_round_trip(HashAlgorithm::Blake3_256, "blake3-256");
  assert_fixture_round_trip(HashAlgorithm::Sha512, "sha512");
}

#[test]
fn cutover_journal_writer_rejects_wrong_control_kind_and_malformed_bytes() {
  let wrong_kind = fixture("system-control-v1", "control-blake3-256-migration-progress-valid.bin");
  assert_eq!(encode_cutover_journal_slot_v1(1, &wrong_kind, HashAlgorithm::Blake3_256).unwrap_err().code(), "cutover_journal_control_kind");

  let mut malformed = fixture("system-control-v1", "control-blake3-256-side-by-side-cutover-valid.bin");
  malformed[80] ^= 1;
  assert!(encode_cutover_journal_slot_v1(1, &malformed, HashAlgorithm::Blake3_256).is_err());
}

#[test]
fn cutover_journal_writer_rejects_zero_sequences_before_allocating_output() {
  let control = fixture("system-control-v1", "control-blake3-256-side-by-side-cutover-valid.bin");
  assert_eq!(encode_cutover_journal_slot_v1(0, &control, HashAlgorithm::Blake3_256).unwrap_err().code(), "cutover_journal_sequence");
  assert_eq!(encode_cutover_journal_pair_v1(11, 0, &control, HashAlgorithm::Blake3_256).unwrap_err().code(), "cutover_journal_sequence");
}

#[test]
fn cutover_journal_writer_keeps_the_control_body_byte_identical() {
  let control = fixture("system-control-v1", "control-blake3-256-side-by-side-cutover-valid.bin");
  let expected = fixture("cutover-journal-v1", "cutover-blake3-256-external-journal-valid.bin");
  let encoded = encode_cutover_journal_pair_v1(11, 12, &control, HashAlgorithm::Blake3_256).unwrap();
  let selected = select_cutover_journal(&encoded, HashAlgorithm::Blake3_256).unwrap();
  let expected_selected = select_cutover_journal(&expected, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(selected.body, expected_selected.body);
}

#[test]
fn durable_workspace_creates_private_exact_journal_and_reopens_with_bounded_migration_memory() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let baseline = memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().reserved_bytes;
  let (workspace, control) = create_workspace(directory.path(), &memory);
  let expected = fixture("cutover-journal-v1", "cutover-blake3-256-external-journal-valid.bin");

  assert_eq!(workspace.journal_path(), directory.path().join("cutover-workspace/cutover.acut"));
  assert_eq!(workspace.selected_slot(), SystemControlSlotV1::B);
  assert_eq!(workspace.sequence(), 12);
  assert!(!workspace.redundancy_degraded());
  let exact_accounted_bytes = 2 * 2_048
    + 1_024
    + 16
    + u64::try_from(workspace.workspace_path().as_os_str().len()).unwrap()
    + u64::try_from(workspace.journal_path().as_os_str().len()).unwrap()
    + u64::try_from(std::mem::size_of_val(&workspace)).unwrap();
  assert_eq!(workspace.reserved_memory_bytes(), exact_accounted_bytes);
  assert_eq!(
    memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().reserved_bytes,
    baseline + workspace.reserved_memory_bytes()
  );
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(workspace.workspace_path()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(workspace.journal_path()).unwrap().permissions().mode() & 0o777, 0o600);
  }

  let workspace_path = workspace.workspace_path().to_path_buf();
  let journal_path = workspace.journal_path().to_path_buf();
  drop(workspace);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().reserved_bytes, baseline);
  assert_eq!(fs::read(&journal_path).unwrap(), expected);
  let reopened = DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &control,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(reopened.sequence(), 12);
  assert_eq!(reopened.selected_slot(), SystemControlSlotV1::B);
  assert!(!reopened.redundancy_degraded());
}

#[test]
fn durable_workspace_persists_and_reopens_the_sha512_fixture() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let control = fixture("system-control-v1", "control-sha512-side-by-side-cutover-valid.bin");
  let expected = fixture("cutover-journal-v1", "cutover-sha512-external-journal-valid.bin");
  let workspace_path = directory.path().join("cutover-workspace-sha512");
  let workspace = DurableCutoverJournalWorkspaceV1::create(
    &workspace_path,
    11,
    12,
    &control,
    HashAlgorithm::Sha512,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let journal_path = workspace.journal_path().to_path_buf();
  drop(workspace);
  assert_eq!(fs::read(&journal_path).unwrap(), expected);
  let reopened = DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &control,
    HashAlgorithm::Sha512,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(reopened.sequence(), 12);
  assert_eq!(reopened.selected_slot(), SystemControlSlotV1::B);
}

#[test]
fn publication_updates_only_the_inactive_slot_and_exact_retry_is_idempotent() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (mut workspace, control) = create_workspace(directory.path(), &memory);
  let before = fixture("cutover-journal-v1", "cutover-blake3-256-external-journal-valid.bin");
  let workspace_path = workspace.workspace_path().to_path_buf();
  let journal_path = workspace.journal_path().to_path_buf();
  let updated = updated_control();

  let receipt = workspace.publish(&updated).unwrap();
  assert!(receipt.changed());
  assert_eq!(receipt.selected_slot(), SystemControlSlotV1::A);
  assert_eq!(receipt.sequence(), 13);
  drop(workspace);
  let after = fs::read(&journal_path).unwrap();
  assert_ne!(&after[..1_024], &before[..1_024]);
  assert_eq!(&after[1_024..], &before[1_024..]);

  let mut workspace = DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &updated,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let retry = workspace.publish(&updated).unwrap();
  assert!(!retry.changed());
  assert_eq!(retry.sequence(), 13);
  drop(workspace);
  assert_eq!(fs::read(&journal_path).unwrap(), after);

  let mut workspace = DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &updated,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(workspace.publish(&control).unwrap().sequence(), 14);
}

#[derive(Debug)]
struct FailAtBoundary(CutoverJournalPublicationBoundaryV1);

impl CutoverJournalFaultInjectorV1 for FailAtBoundary {
  fn inject(&mut self, boundary: CutoverJournalPublicationBoundaryV1) -> bool {
    boundary == self.0
  }
}

#[test]
fn every_injected_publication_boundary_has_an_explicit_recovery_disposition() {
  let cases = [
    (CutoverJournalPublicationBoundaryV1::BeforeSlotWrite, CutoverJournalFailureDispositionV1::PriorAuthorityRetained, 12),
    (CutoverJournalPublicationBoundaryV1::AfterSlotWrite, CutoverJournalFailureDispositionV1::SelectionMustBeReopened, 13),
    (CutoverJournalPublicationBoundaryV1::AfterFileSync, CutoverJournalFailureDispositionV1::SyncedSelectionMustBeReopened, 13),
    (CutoverJournalPublicationBoundaryV1::AfterReadBack, CutoverJournalFailureDispositionV1::SuccessorDurable, 13),
  ];
  for (boundary, expected_disposition, expected_sequence) in cases {
    let directory = tempdir().unwrap();
    let memory = memory_coordinator();
    let (mut workspace, control) = create_workspace(directory.path(), &memory);
    let workspace_path = workspace.workspace_path().to_path_buf();
    let updated = updated_control();
    let error = workspace.publish_with_fault_injector(&updated, &mut FailAtBoundary(boundary)).unwrap_err();
    assert_eq!(error.code(), "cutover_journal_workspace_injected_fault");
    assert_eq!(error.publication_boundary(), Some(boundary));
    assert_eq!(error.failure_disposition(), expected_disposition);
    if boundary == CutoverJournalPublicationBoundaryV1::BeforeSlotWrite {
      let unchanged = workspace.publish(&control).unwrap();
      assert!(!unchanged.changed());
      assert_eq!(unchanged.sequence(), 12);
    } else {
      assert_eq!(workspace.publish(&updated).unwrap_err().code(), "cutover_journal_workspace_state");
    }
    drop(workspace);

    let expected = if expected_sequence == 12 { &control } else { &updated };
    let reopened = DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      expected,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap();
    assert_eq!(reopened.sequence(), expected_sequence);
  }
}

#[derive(Debug)]
struct CancelAfterWrite {
  cancellation: CancellationToken,
}

impl CutoverJournalFaultInjectorV1 for CancelAfterWrite {
  fn inject(&mut self, boundary: CutoverJournalPublicationBoundaryV1) -> bool {
    if boundary == CutoverJournalPublicationBoundaryV1::AfterSlotWrite {
      self.cancellation.cancel();
    }
    false
  }
}

#[derive(Debug)]
struct TruncateAfterSync {
  journal_path: std::path::PathBuf,
}

impl CutoverJournalFaultInjectorV1 for TruncateAfterSync {
  fn inject(&mut self, boundary: CutoverJournalPublicationBoundaryV1) -> bool {
    if boundary == CutoverJournalPublicationBoundaryV1::AfterFileSync {
      fs::OpenOptions::new().write(true).open(&self.journal_path).unwrap().set_len(1_024).unwrap();
    }
    false
  }
}

#[test]
fn malformed_post_sync_read_back_requires_reopening_durable_selection() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (mut workspace, _control) = create_workspace(directory.path(), &memory);
  let journal_path = workspace.journal_path().to_path_buf();

  let error = workspace.publish_with_fault_injector(&updated_control(), &mut TruncateAfterSync { journal_path }).unwrap_err();
  assert_eq!(error.publication_boundary(), Some(CutoverJournalPublicationBoundaryV1::AfterFileSync));
  assert_eq!(error.failure_disposition(), CutoverJournalFailureDispositionV1::SyncedSelectionMustBeReopened);
  assert_eq!(workspace.publish(&updated_control()).unwrap_err().code(), "cutover_journal_workspace_state");
}

#[test]
fn cancellation_refuses_before_mutation_but_does_not_relabel_a_started_publication() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (workspace, control) = create_workspace(directory.path(), &memory);
  let workspace_path = workspace.workspace_path().to_path_buf();
  drop(workspace);

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(&workspace_path, &control, HashAlgorithm::Blake3_256, workspace_options(0), canceled, &memory,)
      .unwrap_err()
      .code(),
    "cutover_journal_workspace_cancelled"
  );

  let cancellation = CancellationToken::new();
  let mut workspace = DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &control,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  cancellation.cancel();
  assert_eq!(workspace.publish(&updated_control()).unwrap_err().code(), "cutover_journal_workspace_cancelled");
  assert_eq!(workspace.sequence(), 12);
  drop(workspace);

  let cancellation = CancellationToken::new();
  let mut workspace = DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &control,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let receipt = workspace.publish_with_fault_injector(&updated_control(), &mut CancelAfterWrite { cancellation }).unwrap();
  assert_eq!(receipt.sequence(), 13);
}

#[test]
fn degraded_redundancy_is_repaired_idempotently_without_advancing_sequence() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (workspace, control) = create_workspace(directory.path(), &memory);
  let workspace_path = workspace.workspace_path().to_path_buf();
  let journal_path = workspace.journal_path().to_path_buf();
  drop(workspace);
  let mut bytes = fs::read(&journal_path).unwrap();
  bytes[100] ^= 1;
  fs::write(&journal_path, bytes).unwrap();

  let mut reopened = DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &control,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert!(reopened.redundancy_degraded());
  assert_eq!(reopened.sequence(), 12);
  let receipt = reopened.publish(&control).unwrap();
  assert!(receipt.changed());
  assert_eq!(receipt.sequence(), 12);
  assert!(!receipt.redundancy_degraded());
}

#[test]
fn journal_owner_retains_an_exclusive_native_lock_until_drop() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (workspace, control) = create_workspace(directory.path(), &memory);
  let workspace_path = workspace.workspace_path().to_path_buf();

  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_locked"
  );
  drop(workspace);
  assert!(DurableCutoverJournalWorkspaceV1::open(
    &workspace_path,
    &control,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .is_ok());
}

#[cfg(unix)]
#[test]
fn journal_owner_refuses_path_replacement_before_publication() {
  use std::os::unix::fs::PermissionsExt;

  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (mut workspace, _control) = create_workspace(directory.path(), &memory);
  let journal_path = workspace.journal_path().to_path_buf();
  let original = fs::read(&journal_path).unwrap();
  let detached_path = workspace.workspace_path().join("detached.acut");
  fs::rename(&journal_path, &detached_path).unwrap();
  fs::write(&journal_path, &original).unwrap();
  fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o600)).unwrap();

  assert_eq!(workspace.publish(&updated_control()).unwrap_err().code(), "cutover_journal_workspace_identity");
  assert_eq!(fs::read(&journal_path).unwrap(), original);
}

#[test]
fn creation_refuses_invalid_path_cancellation_capacity_memory_and_existing_state_without_a_journal() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let control = fixture("system-control-v1", "control-blake3-256-side-by-side-cutover-valid.bin");

  assert_eq!(
    DurableCutoverJournalWorkspaceV1::create(
      Path::new("relative-cutover-workspace"),
      11,
      12,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_path"
  );

  let canceled_path = directory.path().join("canceled");
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::create(
      &canceled_path,
      11,
      12,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      cancellation,
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_cancelled"
  );
  assert!(!canceled_path.exists());

  let capacity_path = directory.path().join("capacity");
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::create(
      &capacity_path,
      11,
      12,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(u64::MAX),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_capacity"
  );
  assert!(!capacity_path.exists());

  let memory_path = directory.path().join("memory");
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(1, 2, 1, 1).unwrap());
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::create(
      &memory_path,
      11,
      12,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &constrained,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_memory"
  );
  assert!(!memory_path.exists());

  let existing_path = directory.path().join("existing");
  fs::create_dir(&existing_path).unwrap();
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::create(
      &existing_path,
      11,
      12,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_path"
  );
  assert!(!existing_path.join("cutover.acut").exists());
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().reserved_bytes, 0);
}

#[test]
fn sequence_exhaustion_refuses_before_touching_the_inactive_slot() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let control = fixture("system-control-v1", "control-blake3-256-side-by-side-cutover-valid.bin");
  let workspace_path = directory.path().join("exhausted");
  let mut workspace = DurableCutoverJournalWorkspaceV1::create(
    &workspace_path,
    u64::MAX - 1,
    u64::MAX,
    &control,
    HashAlgorithm::Blake3_256,
    workspace_options(0),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let journal_path = workspace.journal_path().to_path_buf();
  let before = encode_cutover_journal_pair_v1(u64::MAX - 1, u64::MAX, &control, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(workspace.publish(&updated_control()).unwrap_err().code(), "cutover_journal_workspace_identity");
  drop(workspace);
  assert_eq!(fs::read(&journal_path).unwrap(), before);
}

#[test]
fn reopen_rejects_malformed_foreign_insecure_and_resource_denied_workspaces() {
  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (workspace, control) = create_workspace(directory.path(), &memory);
  let workspace_path = workspace.workspace_path().to_path_buf();
  let journal_path = workspace.journal_path().to_path_buf();
  drop(workspace);

  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      &updated_control(),
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_identity"
  );

  let constrained = MemoryCoordinator::new(MemoryPolicy::new(1, 2, 1, 1).unwrap());
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &constrained,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_memory"
  );
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(u64::MAX),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_capacity"
  );

  let original = fs::read(&journal_path).unwrap();
  fs::write(&journal_path, &original[..2_047]).unwrap();
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_format"
  );
  fs::write(&journal_path, original).unwrap();

  let mut corrupt = fs::read(&journal_path).unwrap();
  corrupt[100] ^= 1;
  corrupt[1_124] ^= 1;
  fs::write(&journal_path, corrupt).unwrap();
  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_format"
  );
  let original = fixture("cutover-journal-v1", "cutover-blake3-256-external-journal-valid.bin");
  fs::write(&journal_path, &original).unwrap();

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
      DurableCutoverJournalWorkspaceV1::open(
        &workspace_path,
        &control,
        HashAlgorithm::Blake3_256,
        workspace_options(0),
        CancellationToken::new(),
        &memory,
      )
      .unwrap_err()
      .code(),
      "cutover_journal_workspace_path"
    );
  }
}

#[cfg(unix)]
#[test]
fn reopen_rejects_a_symlinked_journal_even_when_the_target_bytes_are_valid() {
  use std::os::unix::fs::symlink;

  let directory = tempdir().unwrap();
  let memory = memory_coordinator();
  let (workspace, control) = create_workspace(directory.path(), &memory);
  let workspace_path = workspace.workspace_path().to_path_buf();
  let journal_path = workspace.journal_path().to_path_buf();
  let target_path = workspace.workspace_path().join("journal-target");
  drop(workspace);
  fs::rename(&journal_path, &target_path).unwrap();
  symlink(&target_path, &journal_path).unwrap();

  assert_eq!(
    DurableCutoverJournalWorkspaceV1::open(
      &workspace_path,
      &control,
      HashAlgorithm::Blake3_256,
      workspace_options(0),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "cutover_journal_workspace_path"
  );
}

fn collect_rust_sources(path: &Path, sources: &mut Vec<std::path::PathBuf>) {
  for entry in fs::read_dir(path).unwrap() {
    let entry = entry.unwrap();
    let path = entry.path();
    if path.is_dir() {
      collect_rust_sources(&path, sources);
    } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
      sources.push(path);
    }
  }
}

#[test]
fn durable_cutover_journal_authority_remains_disconnected_from_production_callers() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let owner_path = source_root.join("engine/v4/migration_cutover_journal.rs");
  let rehearsal_owner_path = source_root.join("engine/v4/migration_cutover_rehearsal.rs");
  let module_path = source_root.join("engine/v4/mod.rs");
  let owner = fs::read_to_string(&owner_path).unwrap();
  assert!(!owner.contains("StorageEngine"));
  assert!(!owner.contains("ControlStore"));
  assert!(!owner.contains("server::"));
  let rehearsal_owner = fs::read_to_string(&rehearsal_owner_path).unwrap();
  assert!(!rehearsal_owner.contains("StorageEngine"));
  assert!(!rehearsal_owner.contains("server::"));
  assert!(!rehearsal_owner.contains("axum::"));
  assert!(!rehearsal_owner.contains("clap::"));

  let mut sources = Vec::new();
  collect_rust_sources(&source_root, &mut sources);
  for source in sources {
    if source == owner_path || source == rehearsal_owner_path {
      continue;
    }
    let contents = fs::read_to_string(&source).unwrap();
    assert!(!contents.contains("DurableCutoverJournalWorkspaceV1"), "cutover journal authority was activated by {}", source.display());
    if source != module_path {
      assert!(!contents.contains("SideBySideCutoverRehearsalOwnerV1"), "cutover rehearsal authority was activated by {}", source.display());
    }
  }
}
