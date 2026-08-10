use super::*;

fn sample_stats() -> PeerClockStats {
  PeerClockStats { clock_offset_ms: 1.0, wire_time_ms: 2.0, jitter_ms: 0.5, samples: 3, last_updated_ms: 4 }
}

fn poison_tracker(tracker: &PeerClockTracker) {
  std::thread::scope(|scope| {
    let unwind = scope
      .spawn(|| {
        let _peers = tracker.peers.write().unwrap();
        panic!("inject peer clock tracker poison");
      })
      .join();
    assert!(unwind.is_err());
  });
}

#[test]
fn every_peer_clock_operation_reports_poison_instead_of_a_domain_value() {
  let tracker = PeerClockTracker::new(30_000);
  poison_tracker(&tracker);

  assert!(matches!(tracker.record_heartbeat(7, 1_000, 1_001, 1_002), Err(PeerClockTrackerError::Poisoned)));
  assert!(matches!(tracker.get_peer_stats(7), Err(PeerClockTrackerError::Poisoned)));
  assert!(matches!(tracker.is_settled(7, 3, 1.0), Err(PeerClockTrackerError::Poisoned)));
  assert!(matches!(tracker.all_peer_stats(), Err(PeerClockTrackerError::Poisoned)));
  assert_eq!(tracker.seed_peer(7, sample_stats()), Err(PeerClockTrackerError::Poisoned));
}
