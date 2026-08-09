use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Barrier};

use super::*;

#[test]
fn manual_snapshot_rate_limit_allows_exactly_one_concurrent_claimant() {
  let timestamp = Arc::new(AtomicI64::new(0));
  let barrier = Arc::new(Barrier::new(17));
  let mut workers = Vec::new();

  for _ in 0..16 {
    let timestamp = Arc::clone(&timestamp);
    let barrier = Arc::clone(&barrier);
    workers.push(std::thread::spawn(move || {
      barrier.wait();
      claim_manual_snapshot_rate_limit(&timestamp, 1_000_000).is_ok()
    }));
  }

  barrier.wait();
  let admitted = workers.into_iter().map(|worker| worker.join().unwrap()).filter(|admitted| *admitted).count();
  assert_eq!(admitted, 1);
}

#[test]
fn manual_snapshot_rate_limit_reports_a_ceiling_retry_delay() {
  let timestamp = AtomicI64::new(1_000_000);

  assert_eq!(claim_manual_snapshot_rate_limit(&timestamp, 1_059_001), Err(1));
  assert_eq!(claim_manual_snapshot_rate_limit(&timestamp, 1_030_000), Err(30));
}

#[test]
fn manual_snapshot_rate_limit_accepts_the_exact_window_boundary() {
  let timestamp = AtomicI64::new(1_000_000);

  assert_eq!(claim_manual_snapshot_rate_limit(&timestamp, 1_060_000), Ok(()));
}

#[test]
fn manual_snapshot_rate_limit_rejects_during_wall_clock_rollback() {
  let timestamp = AtomicI64::new(1_000_000);

  assert_eq!(claim_manual_snapshot_rate_limit(&timestamp, 900_000), Err(160));
}
