use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::read_view::{ReadableRootStateV1, RootLifecycleObservationV1, RootPinCoordinatorErrorV1, RootReadPinCoordinatorV1};
use aeordb::engine::HashAlgorithm;
use tokio_util::sync::CancellationToken;

fn build_coordinator(maximum_tracked_roots: usize, maximum_active_pins: u64) -> RootReadPinCoordinatorV1 {
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(8 * 1_024 * 1_024, 16 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()));
  RootReadPinCoordinatorV1::new(memory, HashAlgorithm::Blake3_256, maximum_tracked_roots, maximum_active_pins).unwrap()
}

fn coordinator_with(
  memory: Arc<MemoryCoordinator>,
  hash_algorithm: HashAlgorithm,
  maximum_tracked_roots: usize,
  maximum_active_pins: u64,
) -> RootReadPinCoordinatorV1 {
  RootReadPinCoordinatorV1::new(memory, hash_algorithm, maximum_tracked_roots, maximum_active_pins).unwrap()
}

fn root(byte: u8) -> Vec<u8> {
  vec![byte; 32]
}

#[test]
fn coordinator_requires_nonzero_root_and_pin_caps() {
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(8 * 1_024 * 1_024, 16 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()));
  let root_error = RootReadPinCoordinatorV1::new(Arc::clone(&memory), HashAlgorithm::Blake3_256, 0, 1).err().unwrap();
  assert_eq!(root_error.code(), "read_pin_invalid_configuration");
  let pin_error = RootReadPinCoordinatorV1::new(memory, HashAlgorithm::Blake3_256, 1, 0).err().unwrap();
  assert_eq!(pin_error.code(), "read_pin_invalid_configuration");
}

#[test]
fn readable_states_take_owned_pins_and_pending_expiry_uses_the_larger_grace() {
  let coordinator = build_coordinator(4, 4);
  let cancellation = CancellationToken::new();
  for (observation, expected) in [
    (RootLifecycleObservationV1::Live, ReadableRootStateV1::Live),
    (RootLifecycleObservationV1::Retained, ReadableRootStateV1::Retained),
    (
      RootLifecycleObservationV1::PendingDelete { pending_since_ms: 1_000, grace_at_pending_ms: 500, current_configured_grace_ms: 900 },
      ReadableRootStateV1::PendingDelete { pending_since_ms: 1_000, expires_at_ms: 1_900 },
    ),
  ] {
    let admission = coordinator.admit_read(&root(7), &cancellation, || Ok(observation)).unwrap();
    assert_eq!(admission.state, expected);
    assert_eq!(coordinator.active_pin_count().unwrap(), 1);
    drop(admission);
    assert_eq!(coordinator.active_pin_count().unwrap(), 0);
    assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
  }
}

#[test]
fn unreadable_and_corrupt_states_remain_distinct_and_never_create_pins() {
  let coordinator = build_coordinator(4, 4);
  let cancellation = CancellationToken::new();
  let cases = [
    (RootLifecycleObservationV1::LogicallyRetired, "root_expired"),
    (RootLifecycleObservationV1::PhysicallyReclaimed, "root_expired"),
    (RootLifecycleObservationV1::UnknownOrUnadmitted, "invalid_namespace_root"),
    (RootLifecycleObservationV1::Corrupt, "root_lifecycle_corrupt"),
    (RootLifecycleObservationV1::Unavailable, "root_lifecycle_unavailable"),
  ];
  for (state, expected_code) in cases {
    let error = coordinator.admit_read(&root(8), &cancellation, || Ok(state)).unwrap_err();
    assert_eq!(error.code(), expected_code);
    assert_eq!(coordinator.active_pin_count().unwrap(), 0);
    assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
  }

  let error = coordinator
    .admit_read(&root(8), &cancellation, || {
      Ok(RootLifecycleObservationV1::PendingDelete { pending_since_ms: i64::MAX, grace_at_pending_ms: 1, current_configured_grace_ms: 1 })
    })
    .unwrap_err();
  assert_eq!(error.code(), "root_lifecycle_corrupt");

  for (pending_since_ms, grace_at_pending_ms) in [(0, 1), (-1, 1), (1, u64::MAX)] {
    let error = coordinator
      .admit_read(&root(8), &cancellation, || {
        Ok(RootLifecycleObservationV1::PendingDelete { pending_since_ms, grace_at_pending_ms, current_configured_grace_ms: 0 })
      })
      .unwrap_err();
    assert_eq!(error.code(), "root_lifecycle_corrupt");
  }
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn cancellation_is_checked_before_lifecycle_lookup_or_pin_allocation() {
  let coordinator = build_coordinator(4, 4);
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let lookups = AtomicUsize::new(0);
  let error = coordinator
    .admit_read(&root(9), &cancellation, || {
      lookups.fetch_add(1, Ordering::SeqCst);
      Ok(RootLifecycleObservationV1::Live)
    })
    .unwrap_err();
  assert_eq!(error.code(), "read_view_canceled");
  assert_eq!(lookups.load(Ordering::SeqCst), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn pin_and_retirement_share_one_root_guard_so_exactly_one_side_wins() {
  let coordinator = Arc::new(build_coordinator(4, 4));
  let lifecycle_retired = Arc::new(AtomicBool::new(false));
  let (read_entered_sender, read_entered_receiver) = mpsc::channel();
  let (release_read_sender, release_read_receiver) = mpsc::channel();
  let read_coordinator = Arc::clone(&coordinator);
  let read_retired = Arc::clone(&lifecycle_retired);
  let read = thread::spawn(move || {
    read_coordinator.admit_read(&root(10), &CancellationToken::new(), || {
      read_entered_sender.send(()).unwrap();
      release_read_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
      Ok(if read_retired.load(Ordering::SeqCst) {
        RootLifecycleObservationV1::LogicallyRetired
      } else {
        RootLifecycleObservationV1::PendingDelete { pending_since_ms: 1_000, grace_at_pending_ms: 0, current_configured_grace_ms: 0 }
      })
    })
  });
  read_entered_receiver.recv_timeout(Duration::from_secs(2)).unwrap();

  let (retirement_started_sender, retirement_started_receiver) = mpsc::channel();
  let retirement_coordinator = Arc::clone(&coordinator);
  let retirement_retired = Arc::clone(&lifecycle_retired);
  let retirement = thread::spawn(move || {
    retirement_started_sender.send(()).unwrap();
    retirement_coordinator.with_retirement_exclusion(&root(10), &CancellationToken::new(), || {
      retirement_retired.store(true, Ordering::SeqCst);
      Ok(())
    })
  });
  retirement_started_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
  release_read_sender.send(()).unwrap();

  let admission = read.join().unwrap().expect("the request acquired the root guard first");
  assert!(matches!(retirement.join().unwrap(), Err(RootPinCoordinatorErrorV1::RootPinned)));
  assert!(!lifecycle_retired.load(Ordering::SeqCst));
  drop(admission);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);

  let coordinator = Arc::new(build_coordinator(4, 4));
  let lifecycle_retired = Arc::new(AtomicBool::new(false));
  let (retirement_entered_sender, retirement_entered_receiver) = mpsc::channel();
  let (release_retirement_sender, release_retirement_receiver) = mpsc::channel();
  let retirement_coordinator = Arc::clone(&coordinator);
  let retirement_retired = Arc::clone(&lifecycle_retired);
  let retirement = thread::spawn(move || {
    retirement_coordinator.with_retirement_exclusion(&root(10), &CancellationToken::new(), || {
      retirement_entered_sender.send(()).unwrap();
      release_retirement_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
      retirement_retired.store(true, Ordering::SeqCst);
      Ok(())
    })
  });
  retirement_entered_receiver.recv_timeout(Duration::from_secs(2)).unwrap();

  let (read_started_sender, read_started_receiver) = mpsc::channel();
  let read_coordinator = Arc::clone(&coordinator);
  let read_retired = Arc::clone(&lifecycle_retired);
  let read = thread::spawn(move || {
    read_started_sender.send(()).unwrap();
    read_coordinator.admit_read(&root(10), &CancellationToken::new(), || {
      Ok(if read_retired.load(Ordering::SeqCst) { RootLifecycleObservationV1::LogicallyRetired } else { RootLifecycleObservationV1::Live })
    })
  });
  read_started_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
  release_retirement_sender.send(()).unwrap();

  retirement.join().unwrap().expect("retirement acquired the root guard first");
  assert_eq!(read.join().unwrap().unwrap_err().code(), "root_expired");
  assert!(lifecycle_retired.load(Ordering::SeqCst));
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn explicit_caps_bound_distinct_roots_and_total_pins_without_leaking_reservations() {
  let coordinator = build_coordinator(1, 1);
  let cancellation = CancellationToken::new();
  let first = coordinator.admit_read(&root(11), &cancellation, || Ok(RootLifecycleObservationV1::Live)).unwrap();
  assert_eq!(coordinator.active_pin_count().unwrap(), 1);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 1);

  let pin_error = coordinator.admit_read(&root(11), &cancellation, || Ok(RootLifecycleObservationV1::Live)).unwrap_err();
  assert_eq!(pin_error.code(), "read_pin_limit");
  let root_error = coordinator.admit_read(&root(12), &cancellation, || Ok(RootLifecycleObservationV1::Live)).unwrap_err();
  assert_eq!(root_error.code(), "read_pin_root_limit");

  drop(first);
  let snapshot = coordinator.memory_coordinator().snapshot().unwrap();
  let server_caches = snapshot.owner(MemoryOwner::ServerCaches).unwrap();
  assert_eq!(server_caches.reserved_bytes, 0);
  assert_eq!(server_caches.active_reservations, 0);
}

#[test]
fn hash_validation_covers_both_widths_and_precedes_lifecycle_lookup() {
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(8 * 1_024 * 1_024, 16 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()));
  let blake3 = coordinator_with(Arc::clone(&memory), HashAlgorithm::Blake3_256, 4, 4);
  let sha512 = coordinator_with(memory, HashAlgorithm::Sha512, 4, 4);
  let cancellation = CancellationToken::new();
  let lookups = AtomicUsize::new(0);

  for invalid in [vec![1; 31], vec![0; 32], vec![1; 64]] {
    let error = blake3
      .admit_read(&invalid, &cancellation, || {
        lookups.fetch_add(1, Ordering::SeqCst);
        Ok(RootLifecycleObservationV1::Live)
      })
      .unwrap_err();
    assert_eq!(error.code(), "invalid_root_hash");
  }
  assert_eq!(lookups.load(Ordering::SeqCst), 0);

  let admission = sha512.admit_read(&[2; 64], &cancellation, || Ok(RootLifecycleObservationV1::Live)).unwrap();
  assert_eq!(admission.state, ReadableRootStateV1::Live);
  drop(admission);
  assert_eq!(sha512.tracked_root_count().unwrap(), 0);
}

#[test]
fn cancellation_and_lifecycle_failures_after_gate_allocation_release_all_state() {
  let coordinator = build_coordinator(4, 4);
  let cancellation = CancellationToken::new();
  let canceled_inside = cancellation.clone();
  let error = coordinator
    .admit_read(&root(13), &cancellation, || {
      canceled_inside.cancel();
      Ok(RootLifecycleObservationV1::Live)
    })
    .unwrap_err();
  assert_eq!(error.code(), "read_view_canceled");
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);

  let error =
    coordinator.admit_read(&root(13), &CancellationToken::new(), || Err(RootPinCoordinatorErrorV1::LifecycleUnavailable)).unwrap_err();
  assert_eq!(error.code(), "root_lifecycle_unavailable");
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn memory_pressure_refuses_gate_before_lifecycle_lookup_and_leaks_nothing() {
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(150, 300, 1, 100).unwrap()));
  let coordinator = coordinator_with(Arc::clone(&memory), HashAlgorithm::Blake3_256, 4, 4);
  let lookups = AtomicUsize::new(0);
  let error = coordinator
    .admit_read(&root(14), &CancellationToken::new(), || {
      lookups.fetch_add(1, Ordering::SeqCst);
      Ok(RootLifecycleObservationV1::Live)
    })
    .unwrap_err();
  assert_eq!(error.code(), "read_pin_memory_admission");
  assert_eq!(lookups.load(Ordering::SeqCst), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
  let owner = memory.snapshot().unwrap().owner(MemoryOwner::ServerCaches).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn same_root_pins_share_one_accounted_gate_until_the_last_pin_drops() {
  let coordinator = build_coordinator(2, 3);
  let first = coordinator.admit_read(&root(15), &CancellationToken::new(), || Ok(RootLifecycleObservationV1::Live)).unwrap();
  let second = coordinator.admit_read(&root(15), &CancellationToken::new(), || Ok(RootLifecycleObservationV1::Retained)).unwrap();
  assert_eq!(coordinator.active_pin_count().unwrap(), 2);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 1);
  assert_eq!(coordinator.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::ServerCaches).unwrap().active_reservations, 1);

  drop(first);
  assert_eq!(coordinator.active_pin_count().unwrap(), 1);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 1);
  drop(second);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn retirement_cancellation_and_action_failure_release_the_gate() {
  let coordinator = build_coordinator(2, 2);
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let actions = AtomicUsize::new(0);
  let error = coordinator
    .with_retirement_exclusion(&root(16), &cancellation, || {
      actions.fetch_add(1, Ordering::SeqCst);
      Ok(())
    })
    .unwrap_err();
  assert_eq!(error.code(), "read_view_canceled");
  assert_eq!(actions.load(Ordering::SeqCst), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);

  let error = coordinator
    .with_retirement_exclusion(&root(16), &CancellationToken::new(), || Err::<(), _>(RootPinCoordinatorErrorV1::LifecycleUnavailable))
    .unwrap_err();
  assert_eq!(error.code(), "root_lifecycle_unavailable");
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn lifecycle_lookup_for_one_root_does_not_block_an_unrelated_root() {
  let coordinator = Arc::new(build_coordinator(4, 4));
  let (first_entered_sender, first_entered_receiver) = mpsc::channel();
  let (release_first_sender, release_first_receiver) = mpsc::channel();
  let first_coordinator = Arc::clone(&coordinator);
  let first = thread::spawn(move || {
    first_coordinator.admit_read(&root(17), &CancellationToken::new(), || {
      first_entered_sender.send(()).unwrap();
      release_first_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
      Ok(RootLifecycleObservationV1::Live)
    })
  });
  first_entered_receiver.recv_timeout(Duration::from_secs(2)).unwrap();

  let (second_result_sender, second_result_receiver) = mpsc::channel();
  let second_coordinator = Arc::clone(&coordinator);
  let second = thread::spawn(move || {
    let result = second_coordinator.admit_read(&root(18), &CancellationToken::new(), || Ok(RootLifecycleObservationV1::Live));
    second_result_sender.send(result).unwrap();
  });
  let second_admission = second_result_receiver
    .recv_timeout(Duration::from_secs(2))
    .expect("an unrelated root must not wait for the first root lifecycle lookup")
    .unwrap();
  release_first_sender.send(()).unwrap();
  let first_admission = first.join().unwrap().unwrap();
  second.join().unwrap();
  drop(first_admission);
  drop(second_admission);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
}

#[test]
fn global_exclusion_refuses_any_active_request_pin_without_running_the_action() {
  let coordinator = build_coordinator(4, 4);
  let active = coordinator.admit_read(&root(19), &CancellationToken::new(), || Ok(RootLifecycleObservationV1::Live)).unwrap();
  let actions = AtomicUsize::new(0);

  let error = coordinator
    .with_global_exclusion(&CancellationToken::new(), || {
      actions.fetch_add(1, Ordering::SeqCst);
      Ok(())
    })
    .unwrap_err();

  assert_eq!(error.code(), "request_pinned");
  assert_eq!(actions.load(Ordering::SeqCst), 0);
  drop(active);
  coordinator
    .with_global_exclusion(&CancellationToken::new(), || {
      actions.fetch_add(1, Ordering::SeqCst);
      Ok(())
    })
    .unwrap();
  assert_eq!(actions.load(Ordering::SeqCst), 1);
}

#[test]
fn global_exclusion_blocks_new_read_admission_until_its_final_recheck_finishes() {
  let coordinator = Arc::new(build_coordinator(4, 4));
  let authority_retired = Arc::new(AtomicBool::new(false));
  let (exclusion_entered_sender, exclusion_entered_receiver) = mpsc::channel();
  let (release_exclusion_sender, release_exclusion_receiver) = mpsc::channel();
  let exclusion_coordinator = Arc::clone(&coordinator);
  let exclusion_retired = Arc::clone(&authority_retired);
  let exclusion = thread::spawn(move || {
    exclusion_coordinator.with_global_exclusion(&CancellationToken::new(), || {
      exclusion_entered_sender.send(()).unwrap();
      release_exclusion_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
      exclusion_retired.store(true, Ordering::SeqCst);
      Ok(())
    })
  });
  exclusion_entered_receiver.recv_timeout(Duration::from_secs(2)).unwrap();

  let (read_started_sender, read_started_receiver) = mpsc::channel();
  let (read_result_sender, read_result_receiver) = mpsc::channel();
  let read_coordinator = Arc::clone(&coordinator);
  let read_retired = Arc::clone(&authority_retired);
  let read = thread::spawn(move || {
    read_started_sender.send(()).unwrap();
    let result = read_coordinator.admit_read(&root(20), &CancellationToken::new(), || {
      Ok(if read_retired.load(Ordering::SeqCst) { RootLifecycleObservationV1::LogicallyRetired } else { RootLifecycleObservationV1::Live })
    });
    read_result_sender.send(result.map(|admission| admission.state).map_err(|error| error.code())).unwrap();
  });
  read_started_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
  assert!(read_result_receiver.recv_timeout(Duration::from_millis(100)).is_err());

  release_exclusion_sender.send(()).unwrap();
  exclusion.join().unwrap().unwrap();
  assert_eq!(read_result_receiver.recv_timeout(Duration::from_secs(2)).unwrap(), Err("root_expired"));
  read.join().unwrap();
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}
