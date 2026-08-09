use super::*;

#[test]
fn recovered_void_extent_preserves_every_byte_beyond_u32_width() {
  let start = 4_096u64;
  let length = u64::from(u32::MAX) * 2 + 17;
  let mut recovered = Vec::new();

  append_recovered_void_extent(&mut recovered, start, start + length).unwrap();

  assert_eq!(recovered.len(), 3);
  assert_eq!(recovered[0], VoidRecord { offset: start, size: u32::MAX });
  assert_eq!(recovered[1], VoidRecord { offset: start + u64::from(u32::MAX), size: u32::MAX });
  assert_eq!(recovered[2], VoidRecord { offset: start + u64::from(u32::MAX) * 2, size: 17 });
  assert_eq!(recovered.iter().map(|record| u64::from(record.size)).sum::<u64>(), length);
}

#[test]
fn recovered_void_extent_ignores_empty_or_reversed_ranges() {
  let mut recovered = Vec::new();

  append_recovered_void_extent(&mut recovered, 9_000, 9_000).unwrap();
  append_recovered_void_extent(&mut recovered, 9_001, 9_000).unwrap();

  assert!(recovered.is_empty());
}

#[test]
fn recovered_void_records_preserve_initial_interior_and_trailing_gaps() {
  let recovered = recovered_void_records_from_sorted_ranges(100, 1_000, &[(200, 100), (350, 50), (390, 100)]).unwrap();

  assert_eq!(
    recovered,
    vec![VoidRecord { offset: 100, size: 100 }, VoidRecord { offset: 300, size: 50 }, VoidRecord { offset: 490, size: 510 },]
  );
}

#[test]
fn recovered_void_records_treat_an_empty_live_set_as_one_complete_gap() {
  let recovered = recovered_void_records_from_sorted_ranges(512, 4_096, &[]).unwrap();

  assert_eq!(recovered, vec![VoidRecord { offset: 512, size: 3_584 }]);
}
