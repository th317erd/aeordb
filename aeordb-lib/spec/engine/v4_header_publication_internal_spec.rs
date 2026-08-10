use std::io;
use std::sync::Mutex;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoEvent {
  Read,
  DataBarrier,
  Write(usize),
  FullBarrier,
  Verify,
}

#[derive(Clone, Copy, Debug)]
enum FailurePoint {
  DataBarrier,
  WriteBefore(usize),
  WriteAfter(usize),
  FullBarrier,
  Verify,
}

#[derive(Debug)]
struct MemoryIoState {
  region: [u8; DATABASE_HEADER_V4_REGION_LENGTH],
  events: Vec<IoEvent>,
  write_calls: usize,
}

#[derive(Debug)]
struct MemoryHeaderPublicationIo {
  state: Mutex<MemoryIoState>,
  failure: Option<FailurePoint>,
}

impl MemoryHeaderPublicationIo {
  fn new(region: [u8; DATABASE_HEADER_V4_REGION_LENGTH], failure: Option<FailurePoint>) -> Self {
    Self { state: Mutex::new(MemoryIoState { region, events: Vec::new(), write_calls: 0 }), failure }
  }

  fn region(&self) -> [u8; DATABASE_HEADER_V4_REGION_LENGTH] {
    self.state.lock().unwrap().region
  }

  fn events(&self) -> Vec<IoEvent> {
    self.state.lock().unwrap().events.clone()
  }

  fn injected(operation: NativeDurabilityOperation) -> NativeDurabilityError {
    NativeDurabilityError::operation_io(operation, io::Error::other("injected v4 header publication failure"))
  }
}

impl HeaderPublicationIo for MemoryHeaderPublicationIo {
  fn read_observation(&self, _file: &File) -> Result<DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4> {
    let mut state = self.state.lock().unwrap();
    state.events.push(IoEvent::Read);
    let region = state.region;
    drop(state);
    let selected = decode_header_region(&region)?;
    Ok(DatabaseHeaderObservationV4 { region, selected })
  }

  fn data_barrier(&self, _file: &File) -> Result<(), NativeDurabilityError> {
    self.state.lock().unwrap().events.push(IoEvent::DataBarrier);
    if matches!(self.failure, Some(FailurePoint::DataBarrier)) {
      return Err(Self::injected(NativeDurabilityOperation::DataBarrier));
    }
    Ok(())
  }

  fn write_slot(&self, _file: &File, slot: usize, bytes: &[u8; DATABASE_HEADER_V4_SLOT_LENGTH]) -> Result<(), NativeDurabilityError> {
    let mut state = self.state.lock().unwrap();
    let call = state.write_calls + 1;
    state.events.push(IoEvent::Write(slot));
    if matches!(self.failure, Some(FailurePoint::WriteBefore(expected)) if expected == call) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    replace_slot(&mut state.region, slot, bytes);
    state.write_calls = call;
    if matches!(self.failure, Some(FailurePoint::WriteAfter(expected)) if expected == call) {
      return Err(Self::injected(NativeDurabilityOperation::WriteAt));
    }
    Ok(())
  }

  fn full_barrier(&self, _file: &File) -> Result<(), NativeDurabilityError> {
    self.state.lock().unwrap().events.push(IoEvent::FullBarrier);
    if matches!(self.failure, Some(FailurePoint::FullBarrier)) {
      return Err(Self::injected(NativeDurabilityOperation::FileBarrier));
    }
    Ok(())
  }

  fn verify_region(&self, _file: &File, expected: &[u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> Result<(), NativeDurabilityError> {
    let mut state = self.state.lock().unwrap();
    state.events.push(IoEvent::Verify);
    if matches!(self.failure, Some(FailurePoint::Verify)) {
      return Err(Self::injected(NativeDurabilityOperation::ReadBack));
    }
    if &state.region != expected {
      return Err(NativeDurabilityError::invalid(NativeDurabilityOperation::ReadBack, "in-memory read-back mismatch"));
    }
    Ok(())
  }
}

fn fixture_region(name: &str) -> [u8; DATABASE_HEADER_V4_REGION_LENGTH] {
  let bytes = std::fs::read(format!("{}/spec/fixtures/v4/database-header-v4/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
  bytes.try_into().unwrap()
}

fn observation(region: [u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> DatabaseHeaderObservationV4 {
  DatabaseHeaderObservationV4 { selected: decode_header_region(&region).unwrap(), region }
}

fn test_file() -> File {
  tempfile::tempfile().unwrap()
}

fn publisher_with_memory_io(
  region: [u8; DATABASE_HEADER_V4_REGION_LENGTH],
  failure: Option<FailurePoint>,
) -> (Arc<DurabilityCoordinator>, Arc<MemoryHeaderPublicationIo>, DatabaseHeaderPublisherV4) {
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let io = Arc::new(MemoryHeaderPublicationIo::new(region, failure));
  let publisher = DatabaseHeaderPublisherV4::with_io(coordinator.clone(), io.clone());
  (coordinator, io, publisher)
}

fn startup_fence_region(source: [u8; DATABASE_HEADER_V4_REGION_LENGTH]) -> [u8; DATABASE_HEADER_V4_REGION_LENGTH] {
  let selected = decode_header_region(&source).unwrap();
  let mut first = selected.header.clone();
  first.writer_fence_epoch += 1;
  let mut second = first.clone();
  second.slot_sequence += 1;
  second.updated_at_ms += 1;
  let mut region = source;
  replace_slot(&mut region, 1 - selected.selected_slot, &encode_database_header_slot(&first).unwrap());
  replace_slot(&mut region, selected.selected_slot, &encode_database_header_slot(&second).unwrap());
  region
}

#[test]
fn ordinary_and_dual_publication_use_data_then_full_barriers_and_exact_readback() {
  let source_region = fixture_region("header-blake3-256-valid-ab.bin");
  let source = observation(source_region);
  let (coordinator, io, publisher) = publisher_with_memory_io(source_region, None);
  let mut candidate = source.selected.header.clone();
  candidate.updated_at_ms += 1;
  candidate.entry_count += 1;

  publisher.publish_inactive_slot(&test_file(), &source, candidate).unwrap();

  assert_eq!(io.events(), vec![IoEvent::Read, IoEvent::DataBarrier, IoEvent::Write(0), IoEvent::FullBarrier, IoEvent::Verify]);
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 1);

  let source = observation(io.region());
  let adopted_identity = [0xC5; 16];
  publisher.adopt_physical_instance(&test_file(), &source, adopted_identity, source.selected.header.updated_at_ms + 1).unwrap();
  assert_eq!(
    &io.events()[5..],
    &[IoEvent::Read, IoEvent::DataBarrier, IoEvent::Write(1), IoEvent::Write(0), IoEvent::FullBarrier, IoEvent::Verify]
  );
  assert_eq!(coordinator.snapshot().unwrap().hard_frontier, 2);
}

#[test]
fn every_dual_publication_io_failure_latches_at_the_exact_boundary_and_stops() {
  let source_region = fixture_region("header-blake3-256-valid-ab.bin");
  let source = observation(source_region);
  let final_region = fixture_region("header-blake3-256-adopted-physical-id.bin");
  let adopted_identity = decode_header_region(&final_region).unwrap().header.physical_instance_id;
  let cases = [
    (FailurePoint::DataBarrier, DurabilityOperation::DataBarrier),
    (FailurePoint::WriteBefore(1), DurabilityOperation::AuthorityWrite),
    (FailurePoint::WriteAfter(1), DurabilityOperation::AuthorityWrite),
    (FailurePoint::WriteBefore(2), DurabilityOperation::AuthorityWrite),
    (FailurePoint::WriteAfter(2), DurabilityOperation::AuthorityWrite),
    (FailurePoint::FullBarrier, DurabilityOperation::AuthorityBarrier),
    (FailurePoint::Verify, DurabilityOperation::AuthorityReadback),
  ];

  for (failure_point, expected_operation) in cases {
    let (coordinator, io, publisher) = publisher_with_memory_io(source_region, Some(failure_point));
    let error =
      publisher.adopt_physical_instance(&test_file(), &source, adopted_identity, source.selected.header.updated_at_ms + 1).unwrap_err();
    assert_eq!(error.code(), "durability_failure", "failure point {failure_point:?}");
    let failure = coordinator.hard_failure().unwrap().unwrap();
    assert_eq!(failure.operation, expected_operation, "failure point {failure_point:?}");
    let snapshot = coordinator.snapshot().unwrap();
    assert_eq!(snapshot.admitted + snapshot.executing + snapshot.proven + snapshot.failed + snapshot.pending_hard, 0);
    let events = io.events();
    match failure_point {
      FailurePoint::DataBarrier => assert_eq!(events, vec![IoEvent::Read, IoEvent::DataBarrier]),
      FailurePoint::WriteBefore(1) => assert_eq!(events, vec![IoEvent::Read, IoEvent::DataBarrier, IoEvent::Write(0)]),
      FailurePoint::WriteAfter(1) | FailurePoint::WriteBefore(2) => {
        assert_eq!(decode_header_region(&io.region()).unwrap_err().code(), "ambiguous_equal_sequence")
      }
      FailurePoint::WriteAfter(2) | FailurePoint::FullBarrier | FailurePoint::Verify => assert_eq!(io.region(), final_region),
      FailurePoint::WriteBefore(_) | FailurePoint::WriteAfter(_) => panic!("unlisted write failure point {failure_point:?}"),
    }
    if expected_operation != DurabilityOperation::AuthorityReadback {
      assert!(!events.contains(&IoEvent::Verify));
    }
  }
}

#[test]
fn every_torn_fence_and_adoption_prefix_is_old_fail_closed_or_degraded_until_both_slots_complete() {
  let source = fixture_region("header-blake3-256-valid-ab.bin");
  let source_selected = decode_header_region(&source).unwrap();
  assert_eq!(source_selected.selected_slot, 1);
  let completed_transitions =
    [("writer fence", startup_fence_region(source)), ("clone adoption", fixture_region("header-blake3-256-adopted-physical-id.bin"))];

  for (transition, completed) in completed_transitions {
    let completed_selected = decode_header_region(&completed).unwrap();
    let completed_identity = completed_selected.header.physical_instance_id;
    let completed_fence = completed_selected.header.writer_fence_epoch;
    let first_bytes = &completed[..DATABASE_HEADER_V4_SLOT_LENGTH];
    let second_bytes = &completed[DATABASE_HEADER_V4_SLOT_LENGTH..];

    for prefix in 0..=DATABASE_HEADER_V4_SLOT_LENGTH {
      let mut interrupted = source;
      interrupted[..prefix].copy_from_slice(&first_bytes[..prefix]);
      match decode_header_region(&interrupted) {
        Ok(selected) => {
          assert_eq!(selected.header.writer_fence_epoch, source_selected.header.writer_fence_epoch, "{transition} first prefix {prefix}");
          assert!(prefix < DATABASE_HEADER_V4_SLOT_LENGTH, "complete {transition} first write must fail closed");
        }
        Err(error) if prefix == DATABASE_HEADER_V4_SLOT_LENGTH => assert_eq!(error.code(), "ambiguous_equal_sequence"),
        Err(_) => {}
      }
    }

    let mut first_complete = source;
    first_complete[..DATABASE_HEADER_V4_SLOT_LENGTH].copy_from_slice(first_bytes);
    for prefix in 0..=DATABASE_HEADER_V4_SLOT_LENGTH {
      let mut interrupted = first_complete;
      let start = DATABASE_HEADER_V4_SLOT_LENGTH;
      interrupted[start..start + prefix].copy_from_slice(&second_bytes[..prefix]);
      match decode_header_region(&interrupted) {
        Ok(selected) if prefix == DATABASE_HEADER_V4_SLOT_LENGTH => {
          assert_eq!(interrupted, completed, "{transition}");
          assert!(!selected.redundancy_degraded, "{transition}");
          assert_eq!(selected.selected_slot, 1, "{transition}");
          assert_eq!(selected.header.writer_fence_epoch, completed_fence, "{transition}");
          assert_eq!(selected.header.physical_instance_id, completed_identity, "{transition}");
        }
        Ok(selected) => {
          assert!(selected.redundancy_degraded, "{transition} partial second prefix {prefix} unexpectedly admitted redundant evidence");
          assert_eq!(selected.selected_slot, 0, "{transition}");
          assert_eq!(selected.header.writer_fence_epoch, completed_fence, "{transition}");
          assert_eq!(selected.header.physical_instance_id, completed_identity, "{transition}");
        }
        Err(_) => {}
      }
    }
  }
}

#[test]
fn every_torn_ordinary_prefix_preserves_old_authority_until_the_complete_inactive_slot_is_valid() {
  let source = fixture_region("header-blake3-256-valid-ab.bin");
  let source_selected = decode_header_region(&source).unwrap();
  let mut candidate = source_selected.header.clone();
  candidate.slot_sequence += 1;
  candidate.updated_at_ms += 1;
  candidate.entry_count += 1;
  let candidate = encode_database_header_slot(&candidate).unwrap();

  for prefix in 0..=DATABASE_HEADER_V4_SLOT_LENGTH {
    let mut interrupted = source;
    interrupted[..prefix].copy_from_slice(&candidate[..prefix]);
    match decode_header_region(&interrupted) {
      Ok(selected) if prefix == DATABASE_HEADER_V4_SLOT_LENGTH => {
        assert_eq!(selected.selected_slot, 0);
        assert_eq!(selected.header.slot_sequence, source_selected.header.slot_sequence + 1);
        assert!(!selected.redundancy_degraded);
      }
      Ok(selected) => {
        assert_eq!(selected.header.slot_sequence, source_selected.header.slot_sequence, "ordinary prefix {prefix}");
        assert_eq!(selected.selected_slot, source_selected.selected_slot);
      }
      Err(error) => panic!("ordinary prefix {prefix} lost the valid source slot: {error}"),
    }
  }
}
