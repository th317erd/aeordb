use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Barrier};

use aeordb::engine::durability_coordinator::{DurabilityCoordinator, DurabilityOperation};
use aeordb::engine::v4::database_header::{
  DATABASE_HEADER_V4_SLOT_LENGTH, DatabaseHeaderV4, SelectedDatabaseHeaderV4, decode_header_region, encode_database_header_slot,
};
use aeordb::engine::v4::header_publication::{DatabaseHeaderPublisherV4, observe_database_header_v4};

fn fixture_bytes(name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/database-header-v4/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn fixture_file(name: &str) -> (tempfile::TempDir, std::path::PathBuf, File) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("database.aeordb");
  std::fs::write(&path, fixture_bytes(name)).unwrap();
  let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
  (directory, path, file)
}

fn publisher() -> (Arc<DurabilityCoordinator>, DatabaseHeaderPublisherV4) {
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let publisher = DatabaseHeaderPublisherV4::new(coordinator.clone());
  (coordinator, publisher)
}

fn selected_slot(region: &[u8], slot: usize) -> SelectedDatabaseHeaderV4 {
  let start = slot * DATABASE_HEADER_V4_SLOT_LENGTH;
  let bytes = &region[start..start + DATABASE_HEADER_V4_SLOT_LENGTH];
  let mut duplicated = Vec::with_capacity(DATABASE_HEADER_V4_SLOT_LENGTH * 2);
  duplicated.extend_from_slice(bytes);
  duplicated.extend_from_slice(bytes);
  decode_header_region(&duplicated).unwrap()
}

fn region_with_identical_header(mut header: DatabaseHeaderV4) -> Vec<u8> {
  header.updated_at_ms = header.updated_at_ms.max(header.created_at_ms);
  let slot = encode_database_header_slot(&header).unwrap();
  [slot.as_slice(), slot.as_slice()].concat()
}

#[test]
fn ordinary_publication_writes_only_the_inactive_slot_and_returns_exact_observation() {
  let (_directory, path, file) = fixture_file("header-blake3-256-valid-ab.bin");
  let source_bytes = std::fs::read(&path).unwrap();
  let source = observe_database_header_v4(&file).unwrap();
  let source_slot = source.selected.selected_slot;
  let mut candidate = source.selected.header.clone();
  candidate.entry_count += 1;
  candidate.updated_at_ms += 1;
  let (coordinator, publisher) = publisher();

  let receipt = publisher.publish_inactive_slot(&file, &source, candidate.clone()).unwrap();

  assert_eq!(receipt.observation.selected.selected_slot, 1 - source_slot);
  assert_eq!(receipt.observation.selected.header.slot_sequence, source.selected.header.slot_sequence + 1);
  assert_eq!(receipt.observation.selected.header.entry_count, candidate.entry_count);
  assert_eq!(receipt.observation.selected.header.physical_instance_id, source.selected.header.physical_instance_id);
  assert_eq!(receipt.observation.selected.header.writer_fence_epoch, source.selected.header.writer_fence_epoch);
  assert!(!receipt.observation.selected.redundancy_degraded);
  assert_eq!(receipt.observation.region.as_slice(), std::fs::read(&path).unwrap());

  let target_start = (1 - source_slot) * DATABASE_HEADER_V4_SLOT_LENGTH;
  let source_start = source_slot * DATABASE_HEADER_V4_SLOT_LENGTH;
  assert_eq!(
    &receipt.observation.region[source_start..source_start + DATABASE_HEADER_V4_SLOT_LENGTH],
    &source_bytes[source_start..source_start + DATABASE_HEADER_V4_SLOT_LENGTH]
  );
  assert_ne!(
    &receipt.observation.region[target_start..target_start + DATABASE_HEADER_V4_SLOT_LENGTH],
    &source_bytes[target_start..target_start + DATABASE_HEADER_V4_SLOT_LENGTH]
  );
  assert_eq!(receipt.durability.sequence, 1);
  let snapshot = coordinator.snapshot().unwrap();
  assert_eq!(snapshot.hard_frontier, 1);
  assert_eq!(
    snapshot.ledger.iter().map(|entry| entry.operation).collect::<Vec<_>>(),
    vec![
      DurabilityOperation::DependencyAppend,
      DurabilityOperation::DataBarrier,
      DurabilityOperation::AuthorityWrite,
      DurabilityOperation::HeaderAb,
      DurabilityOperation::AuthorityBarrier,
      DurabilityOperation::AuthorityReadback,
    ]
  );
}

#[test]
fn positional_observation_does_not_change_the_shared_file_cursor() {
  let (_directory, _path, mut file) = fixture_file("header-blake3-256-valid-ab.bin");
  file.seek(SeekFrom::Start(137)).unwrap();

  let observed = observe_database_header_v4(&file).unwrap();

  assert_eq!(observed.selected.header.slot_sequence, 42);
  assert_eq!(file.stream_position().unwrap(), 137);
}

#[test]
fn startup_fence_and_clone_adoption_publish_both_slots_before_writable_evidence() {
  let (_directory, _path, file) = fixture_file("header-blake3-256-valid-ab.bin");
  let source = observe_database_header_v4(&file).unwrap();
  let source_identity = source.selected.header.physical_instance_id;
  let source_fence = source.selected.header.writer_fence_epoch;
  let (_coordinator, publisher) = publisher();

  let fenced = publisher.advance_writer_fence(&file, &source, source.selected.header.updated_at_ms + 1).unwrap();
  assert_eq!(fenced.observation.selected.header.physical_instance_id, source_identity);
  assert_eq!(fenced.observation.selected.header.writer_fence_epoch, source_fence + 1);
  assert_eq!(fenced.observation.selected.selected_slot, source.selected.selected_slot);
  assert!(!fenced.observation.selected.redundancy_degraded);
  for slot in 0..2 {
    let slot = selected_slot(&fenced.observation.region, slot);
    assert_eq!(slot.header.physical_instance_id, source_identity);
    assert_eq!(slot.header.writer_fence_epoch, source_fence + 1);
  }

  let adopted_identity = [0xD5; 16];
  let adopted = publisher
    .adopt_physical_instance(&file, &fenced.observation, adopted_identity, fenced.observation.selected.header.updated_at_ms + 1)
    .unwrap();
  assert_eq!(adopted.observation.selected.header.physical_instance_id, adopted_identity);
  assert_eq!(adopted.observation.selected.header.writer_fence_epoch, source_fence + 2);
  assert_eq!(adopted.observation.selected.selected_slot, source.selected.selected_slot);
  assert!(!adopted.observation.selected.redundancy_degraded);
  for slot in 0..2 {
    let slot = selected_slot(&adopted.observation.region, slot);
    assert_eq!(slot.header.physical_instance_id, adopted_identity);
    assert_eq!(slot.header.writer_fence_epoch, source_fence + 2);
  }
}

#[test]
fn clone_adoption_byte_matches_the_independent_fail_closed_fixture() {
  let (_directory, _path, file) = fixture_file("header-blake3-256-valid-ab.bin");
  let source = observe_database_header_v4(&file).unwrap();
  let expected_region = fixture_bytes("header-blake3-256-adopted-physical-id.bin");
  let expected = decode_header_region(&expected_region).unwrap();
  let (_coordinator, publisher) = publisher();

  let adopted = publisher
    .adopt_physical_instance(&file, &source, expected.header.physical_instance_id, source.selected.header.updated_at_ms + 1)
    .unwrap();

  assert_eq!(adopted.observation.region.as_slice(), expected_region);
}

#[test]
fn stale_and_concurrent_callers_fail_before_mutation_or_durability_latch() {
  let (_directory, path, file) = fixture_file("header-blake3-256-valid-ab.bin");
  let source = observe_database_header_v4(&file).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let publisher = Arc::new(DatabaseHeaderPublisherV4::new(coordinator.clone()));
  let barrier = Arc::new(Barrier::new(2));
  let mut handles = Vec::new();
  for delta in [1, 2] {
    let file = file.try_clone().unwrap();
    let source = source.clone();
    let publisher = publisher.clone();
    let barrier = barrier.clone();
    handles.push(std::thread::spawn(move || {
      let mut candidate = source.selected.header.clone();
      candidate.entry_count += delta;
      candidate.updated_at_ms += delta;
      barrier.wait();
      publisher.publish_inactive_slot(&file, &source, candidate)
    }));
  }
  let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
  assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
  let stale = results.iter().find_map(|result| result.as_ref().err()).unwrap();
  assert_eq!(stale.code(), "stale_header_observation");
  let bytes_after_race = std::fs::read(&path).unwrap();

  let mut stale_candidate = source.selected.header.clone();
  stale_candidate.updated_at_ms += 3;
  let error = publisher.publish_inactive_slot(&file, &source, stale_candidate).unwrap_err();
  assert_eq!(error.code(), "stale_header_observation");
  assert_eq!(std::fs::read(&path).unwrap(), bytes_after_race);
  assert!(coordinator.hard_failure().unwrap().is_none());
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 1);
}

#[test]
fn invalid_identity_timestamp_and_exhausted_sequences_refuse_before_mutation() {
  let (_directory, path, file) = fixture_file("header-blake3-256-valid-ab.bin");
  let source = observe_database_header_v4(&file).unwrap();
  let source_bytes = std::fs::read(&path).unwrap();
  let (coordinator, publisher) = publisher();

  for identity in [[0; 16], source.selected.header.physical_instance_id] {
    let error = publisher.adopt_physical_instance(&file, &source, identity, source.selected.header.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "invalid_physical_identity_transition");
  }
  let error = publisher.advance_writer_fence(&file, &source, source.selected.header.updated_at_ms - 1).unwrap_err();
  assert_eq!(error.code(), "header_updated_at_regressed");
  assert_eq!(std::fs::read(&path).unwrap(), source_bytes);
  assert_eq!(coordinator.snapshot().unwrap().admitted, 0);

  let mut exhausted_sequence = source.selected.header.clone();
  exhausted_sequence.slot_sequence = u64::MAX;
  std::fs::write(&path, region_with_identical_header(exhausted_sequence)).unwrap();
  let exhausted = observe_database_header_v4(&file).unwrap();
  let error = publisher.publish_inactive_slot(&file, &exhausted, exhausted.selected.header.clone()).unwrap_err();
  assert_eq!(error.code(), "header_sequence_exhausted");

  let mut exhausted_fence = source.selected.header.clone();
  exhausted_fence.writer_fence_epoch = u64::MAX;
  std::fs::write(&path, region_with_identical_header(exhausted_fence)).unwrap();
  let exhausted = observe_database_header_v4(&file).unwrap();
  let error = publisher.advance_writer_fence(&file, &exhausted, exhausted.selected.header.updated_at_ms + 1).unwrap_err();
  assert_eq!(error.code(), "writer_fence_exhausted");
  assert!(coordinator.hard_failure().unwrap().is_none());
}

#[test]
fn ordinary_publication_supports_both_hash_widths_equal_slots_and_degraded_repair() {
  for fixture in ["header-sha512-valid-ab.bin", "header-blake3-256-equal-identical.bin", "header-blake3-256-one-valid-slot.bin"] {
    let (_directory, path, file) = fixture_file(fixture);
    let source = observe_database_header_v4(&file).unwrap();
    let was_degraded = source.selected.redundancy_degraded;
    let mut candidate = source.selected.header.clone();
    candidate.updated_at_ms += 1;
    candidate.entry_count += 1;
    let (_coordinator, publisher) = publisher();

    let receipt = publisher.publish_inactive_slot(&file, &source, candidate).unwrap();

    assert!(!receipt.observation.selected.redundancy_degraded, "fixture {fixture}");
    assert_eq!(receipt.observation.selected.header.hash_algorithm, source.selected.header.hash_algorithm, "fixture {fixture}");
    assert_eq!(receipt.observation.selected.selected_slot, 1 - source.selected.selected_slot, "fixture {fixture}");
    assert_eq!(receipt.observation.region.as_slice(), std::fs::read(&path).unwrap(), "fixture {fixture}");
    if was_degraded {
      assert!(selected_slot(&receipt.observation.region, 0).header.slot_sequence > 0);
      assert!(selected_slot(&receipt.observation.region, 1).header.slot_sequence > 0);
    }
  }
}

#[test]
fn ordinary_publication_rejects_caller_owned_sequence_and_immutable_or_monotonic_drift() {
  let (_directory, path, file) = fixture_file("header-blake3-256-valid-ab.bin");
  let source = observe_database_header_v4(&file).unwrap();
  let source_bytes = std::fs::read(&path).unwrap();
  let (coordinator, publisher) = publisher();
  let mut cases = Vec::new();

  let mut candidate = source.selected.header.clone();
  candidate.slot_sequence += 1;
  cases.push((candidate, "candidate_sequence_not_current"));
  let mut candidate = source.selected.header.clone();
  candidate.database_id[0] ^= 1;
  cases.push((candidate, "ordinary_header_identity_changed"));
  let mut candidate = source.selected.header.clone();
  candidate.physical_instance_id[0] ^= 1;
  cases.push((candidate, "ordinary_header_identity_changed"));
  let mut candidate = source.selected.header.clone();
  candidate.writer_fence_epoch += 1;
  cases.push((candidate, "ordinary_header_identity_changed"));
  let mut candidate = source.selected.header.clone();
  candidate.system_family_registry_fingerprint[0] ^= 1;
  cases.push((candidate, "ordinary_header_identity_changed"));
  let mut candidate = source.selected.header.clone();
  candidate.required_reader_capabilities[0] ^= 1;
  cases.push((candidate, "ordinary_header_identity_changed"));
  let mut candidate = source.selected.header.clone();
  candidate.required_writer_capabilities[0] ^= 1;
  cases.push((candidate, "ordinary_header_identity_changed"));
  let mut candidate = source.selected.header.clone();
  candidate.write_sequence_high_water -= 1;
  cases.push((candidate, "write_sequence_regressed"));

  for (candidate, expected_code) in cases {
    let error = publisher.publish_inactive_slot(&file, &source, candidate).unwrap_err();
    assert_eq!(error.code(), expected_code);
    assert_eq!(std::fs::read(&path).unwrap(), source_bytes);
  }
  assert_eq!(coordinator.snapshot().unwrap().admitted, 0);
  assert!(coordinator.hard_failure().unwrap().is_none());
}

#[test]
fn dual_fencing_repairs_degraded_redundancy_and_handles_equal_identical_slots() {
  for fixture in ["header-blake3-256-one-valid-slot.bin", "header-blake3-256-equal-identical.bin", "header-sha512-valid-ab.bin"] {
    let (_directory, _path, file) = fixture_file(fixture);
    let source = observe_database_header_v4(&file).unwrap();
    let (_coordinator, publisher) = publisher();

    let receipt = publisher.advance_writer_fence(&file, &source, source.selected.header.updated_at_ms + 1).unwrap();

    assert!(!receipt.observation.selected.redundancy_degraded, "fixture {fixture}");
    assert_eq!(receipt.observation.selected.selected_slot, source.selected.selected_slot, "fixture {fixture}");
    for slot in 0..2 {
      let slot = selected_slot(&receipt.observation.region, slot);
      assert_eq!(slot.header.writer_fence_epoch, source.selected.header.writer_fence_epoch + 1, "fixture {fixture}");
      assert_eq!(slot.header.physical_instance_id, source.selected.header.physical_instance_id, "fixture {fixture}");
    }
  }
}

#[test]
fn read_only_publication_fails_hard_without_changing_header_bytes() {
  let (_directory, path, writable) = fixture_file("header-blake3-256-valid-ab.bin");
  let source = observe_database_header_v4(&writable).unwrap();
  drop(writable);
  let read_only = OpenOptions::new().read(true).open(&path).unwrap();
  let source_bytes = std::fs::read(&path).unwrap();
  let mut candidate = source.selected.header.clone();
  candidate.updated_at_ms += 1;
  candidate.entry_count += 1;
  let (coordinator, publisher) = publisher();

  let error = publisher.publish_inactive_slot(&read_only, &source, candidate).unwrap_err();
  assert!(matches!(error.code(), "durability_failure" | "native_io_failure"));
  assert_eq!(std::fs::read(&path).unwrap(), source_bytes);
  let failure = coordinator.hard_failure().unwrap().unwrap();
  assert!(matches!(failure.operation, DurabilityOperation::DataBarrier | DurabilityOperation::AuthorityWrite));
}

#[test]
fn shadow_publisher_has_no_service_or_storage_engine_caller() {
  fn collect_rust_files(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        collect_rust_files(&path, files);
      } else if path.extension().is_some_and(|extension| extension == "rs") {
        files.push(path);
      }
    }
  }

  let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let publisher_path = source_root.join("engine/v4/header_publication.rs");
  let mut source_files = Vec::new();
  collect_rust_files(&source_root, &mut source_files);
  let callers: Vec<_> = source_files
    .into_iter()
    .filter(|path| path != &publisher_path)
    .filter(|path| std::fs::read_to_string(path).unwrap().contains("DatabaseHeaderPublisherV4"))
    .collect();
  assert!(callers.is_empty(), "v4 header publication activated outside its shadow module: {callers:?}");
}
