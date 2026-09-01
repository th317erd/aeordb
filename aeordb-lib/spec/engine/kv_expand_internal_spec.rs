use super::*;

#[test]
fn interrupted_expansion_reorders_shifted_and_retained_voids_by_offset() {
  let payload =
    HotTailPayload { writes: Vec::new(), voids: vec![VoidRecord { offset: 100, size: 20 }, VoidRecord { offset: 300, size: 20 }] };

  let relocated = relocate_hot_tail_payload(payload, 0, 200, 1_000, 1_000, 300, 2_000).unwrap();

  assert_eq!(
    relocated.voids,
    vec![VoidRecord { offset: 300, size: 20 }, VoidRecord { offset: 1_100, size: 20 }],
    "a shifted low range must be ordered after a retained range that now precedes it"
  );
}

#[test]
fn interrupted_expansion_splits_a_coalesced_void_across_the_relocation_boundary() {
  let payload = HotTailPayload { writes: Vec::new(), voids: vec![VoidRecord { offset: 100, size: 150 }] };

  let relocated = relocate_hot_tail_payload(payload, 0, 200, 1_000, 1_000, 200, 2_000).unwrap();

  assert_eq!(
    relocated.voids,
    vec![VoidRecord { offset: 200, size: 50 }, VoidRecord { offset: 1_100, size: 100 }],
    "recovery must retain the stationary suffix and relocate the prefix of a boundary-crossing reusable extent"
  );
}

#[test]
fn interrupted_expansion_discards_a_void_past_the_old_wal_frontier() {
  // offset_delta places the copy destination (and old WAL frontier) at 1,000.
  // The malformed range below extends 50 bytes into the former hot tail.
  let payload = HotTailPayload { writes: Vec::new(), voids: vec![VoidRecord { offset: 900, size: 150 }] };

  let relocated = relocate_hot_tail_payload(payload, 0, 200, 1_000, 1_000, 200, 1_200).unwrap();

  assert!(relocated.voids.is_empty(), "recovery must not turn out-of-frontier metadata into reusable bytes");
}
