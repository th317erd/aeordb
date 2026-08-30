use super::*;

#[test]
fn interrupted_expansion_reorders_shifted_and_retained_voids_by_offset() {
  let payload =
    HotTailPayload { writes: Vec::new(), voids: vec![VoidRecord { offset: 100, size: 20 }, VoidRecord { offset: 300, size: 20 }] };

  let relocated = relocate_hot_tail_payload(payload, 0, 200, 1_000, 300, 2_000).unwrap();

  assert_eq!(
    relocated.voids,
    vec![VoidRecord { offset: 300, size: 20 }, VoidRecord { offset: 1_100, size: 20 }],
    "a shifted low range must be ordered after a retained range that now precedes it"
  );
}
