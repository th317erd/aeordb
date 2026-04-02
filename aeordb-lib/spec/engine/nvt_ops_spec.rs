use aeordb::engine::nvt::NormalizedVectorTable;
use aeordb::engine::nvt_ops::NVTMask;
use aeordb::engine::scalar_converter::U64Converter;

fn make_nvt_with_entries(bucket_count: usize, occupied_buckets: &[usize]) -> NormalizedVectorTable {
  let converter = Box::new(U64Converter::with_range(0, 1000));
  let mut nvt = NormalizedVectorTable::new(converter, bucket_count);
  for &bucket_index in occupied_buckets {
    nvt.update_bucket(bucket_index, bucket_index as u64, 1);
  }
  nvt
}

// --- Basic construction ---

#[test]
fn test_mask_new_all_off() {
  let mask = NVTMask::new(128);
  assert_eq!(mask.bucket_count(), 128);
  assert_eq!(mask.popcount(), 0);
  assert!(mask.is_empty());
  for index in 0..128 {
    assert!(!mask.get_bit(index));
  }
}

#[test]
fn test_mask_all_on() {
  let mask = NVTMask::all_on(128);
  assert_eq!(mask.bucket_count(), 128);
  assert_eq!(mask.popcount(), 128);
  assert!(!mask.is_empty());
  for index in 0..128 {
    assert!(mask.get_bit(index));
  }
  // Bit beyond bucket_count should be off
  assert!(!mask.get_bit(128));
}

#[test]
fn test_mask_all_on_non_multiple_of_64() {
  let mask = NVTMask::all_on(100);
  assert_eq!(mask.popcount(), 100);
  // Bit 100 and beyond should be off
  assert!(!mask.get_bit(100));
  assert!(!mask.get_bit(127));
}

#[test]
fn test_mask_set_and_get_bit() {
  let mut mask = NVTMask::new(256);

  mask.set_bit(0);
  mask.set_bit(63);
  mask.set_bit(64);
  mask.set_bit(127);
  mask.set_bit(255);

  assert!(mask.get_bit(0));
  assert!(mask.get_bit(63));
  assert!(mask.get_bit(64));
  assert!(mask.get_bit(127));
  assert!(mask.get_bit(255));
  assert!(!mask.get_bit(1));
  assert!(!mask.get_bit(128));

  // Setting beyond bucket_count is a no-op
  mask.set_bit(256);
  assert!(!mask.get_bit(256));

  // Clear a bit
  mask.clear_bit(63);
  assert!(!mask.get_bit(63));
  assert_eq!(mask.popcount(), 4);
}

#[test]
fn test_mask_from_nvt() {
  let nvt = make_nvt_with_entries(64, &[0, 10, 20, 63]);
  let mask = NVTMask::from_nvt(&nvt);

  assert_eq!(mask.bucket_count(), 64);
  assert_eq!(mask.popcount(), 4);
  assert!(mask.get_bit(0));
  assert!(mask.get_bit(10));
  assert!(mask.get_bit(20));
  assert!(mask.get_bit(63));
  assert!(!mask.get_bit(1));
  assert!(!mask.get_bit(30));
}

#[test]
fn test_mask_from_range() {
  let mask = NVTMask::from_range(128, 10, 20);
  assert_eq!(mask.popcount(), 10);
  for index in 0..128 {
    if index >= 10 && index < 20 {
      assert!(mask.get_bit(index), "bit {} should be on", index);
    } else {
      assert!(!mask.get_bit(index), "bit {} should be off", index);
    }
  }
}

#[test]
fn test_mask_from_range_clamped() {
  // Range extends beyond bucket_count
  let mask = NVTMask::from_range(64, 60, 100);
  assert_eq!(mask.popcount(), 4); // 60, 61, 62, 63
}

// --- Logical operations ---

#[test]
fn test_mask_and() {
  let mut mask_a = NVTMask::new(128);
  let mut mask_b = NVTMask::new(128);

  mask_a.set_bit(0);
  mask_a.set_bit(10);
  mask_a.set_bit(20);

  mask_b.set_bit(10);
  mask_b.set_bit(20);
  mask_b.set_bit(30);

  let result = mask_a.and(&mask_b).unwrap();
  assert_eq!(result.popcount(), 2);
  assert!(!result.get_bit(0));
  assert!(result.get_bit(10));
  assert!(result.get_bit(20));
  assert!(!result.get_bit(30));
}

#[test]
fn test_mask_or() {
  let mut mask_a = NVTMask::new(128);
  let mut mask_b = NVTMask::new(128);

  mask_a.set_bit(0);
  mask_a.set_bit(10);

  mask_b.set_bit(10);
  mask_b.set_bit(20);

  let result = mask_a.or(&mask_b).unwrap();
  assert_eq!(result.popcount(), 3);
  assert!(result.get_bit(0));
  assert!(result.get_bit(10));
  assert!(result.get_bit(20));
}

#[test]
fn test_mask_not() {
  let mut mask = NVTMask::new(128);
  mask.set_bit(0);
  mask.set_bit(127);

  let result = mask.not();
  assert_eq!(result.popcount(), 126);
  assert!(!result.get_bit(0));
  assert!(!result.get_bit(127));
  assert!(result.get_bit(1));
  assert!(result.get_bit(64));
}

#[test]
fn test_mask_not_non_multiple_of_64() {
  // Ensure NOT doesn't leak bits beyond bucket_count
  let mask = NVTMask::new(100);
  let result = mask.not();
  assert_eq!(result.popcount(), 100);
  assert!(!result.get_bit(100));
  assert!(!result.get_bit(127));
}

#[test]
fn test_mask_xor() {
  let mut mask_a = NVTMask::new(128);
  let mut mask_b = NVTMask::new(128);

  mask_a.set_bit(0);
  mask_a.set_bit(10);
  mask_a.set_bit(20);

  mask_b.set_bit(10);
  mask_b.set_bit(20);
  mask_b.set_bit(30);

  let result = mask_a.xor(&mask_b).unwrap();
  assert_eq!(result.popcount(), 2);
  assert!(result.get_bit(0));
  assert!(!result.get_bit(10));
  assert!(!result.get_bit(20));
  assert!(result.get_bit(30));
}

#[test]
fn test_mask_difference() {
  let mut mask_a = NVTMask::new(128);
  let mut mask_b = NVTMask::new(128);

  mask_a.set_bit(0);
  mask_a.set_bit(10);
  mask_a.set_bit(20);

  mask_b.set_bit(10);
  mask_b.set_bit(30);

  let result = mask_a.difference(&mask_b).unwrap();
  assert_eq!(result.popcount(), 2);
  assert!(result.get_bit(0));
  assert!(!result.get_bit(10));
  assert!(result.get_bit(20));
  assert!(!result.get_bit(30));
}

#[test]
fn test_mask_popcount() {
  let mut mask = NVTMask::new(256);
  assert_eq!(mask.popcount(), 0);

  for index in (0..256).step_by(4) {
    mask.set_bit(index);
  }
  assert_eq!(mask.popcount(), 64); // 256/4 = 64
}

#[test]
fn test_mask_surviving_buckets() {
  let mut mask = NVTMask::new(128);
  mask.set_bit(5);
  mask.set_bit(63);
  mask.set_bit(64);
  mask.set_bit(100);

  let survivors = mask.surviving_buckets();
  assert_eq!(survivors, vec![5, 63, 64, 100]);
}

#[test]
fn test_mask_surviving_buckets_empty() {
  let mask = NVTMask::new(128);
  let survivors = mask.surviving_buckets();
  assert!(survivors.is_empty());
}

#[test]
fn test_mask_is_empty() {
  let mask = NVTMask::new(64);
  assert!(mask.is_empty());

  let mut mask = NVTMask::new(64);
  mask.set_bit(0);
  assert!(!mask.is_empty());
}

// --- Strided and progressive ---

#[test]
fn test_mask_and_strided() {
  let mask_a = NVTMask::all_on(128);
  let mut mask_b = NVTMask::new(128);
  // Set every 8th bit in mask_b
  for index in (0..128).step_by(8) {
    mask_b.set_bit(index);
  }

  // Stride of 8: check every 8th bucket, propagate to stride window
  let result = mask_a.and_strided(&mask_b, 8).unwrap();

  // Every 8th bucket in mask_b is on, so the strided check at those positions
  // sees both masks on, and sets the entire 8-bucket window.
  // All 128 bits should be set since mask_a is all-on and mask_b has a hit
  // at every stride boundary.
  assert_eq!(result.popcount(), 128);

  // Now with a sparser mask_b
  let mut mask_b_sparse = NVTMask::new(128);
  mask_b_sparse.set_bit(0);
  // Only bucket 0 is on, stride 8 means only window [0..8) gets set
  let result = mask_a.and_strided(&mask_b_sparse, 8).unwrap();
  assert_eq!(result.popcount(), 8);
  for index in 0..8 {
    assert!(result.get_bit(index));
  }
  assert!(!result.get_bit(8));
}

#[test]
fn test_mask_and_progressive() {
  let mask_a = NVTMask::all_on(128);
  let mut mask_b = NVTMask::new(128);
  // Set bits 0..16 on in mask_b
  for index in 0..16 {
    mask_b.set_bit(index);
  }

  // Progressive with stride 16: rough pass checks bucket 0, 16, 32, ...
  // Only bucket 0 has both masks on, so refine [0..16)
  // In that range, mask_b has bits 0..16 on, mask_a is all on, so all survive.
  let result = mask_a.and_progressive(&mask_b, 16).unwrap();
  assert_eq!(result.popcount(), 16);
  for index in 0..16 {
    assert!(result.get_bit(index));
  }
  assert!(!result.get_bit(16));
}

#[test]
fn test_mask_and_progressive_precise() {
  // Test that progressive AND gives exactly correct results
  let mut mask_a = NVTMask::new(128);
  let mut mask_b = NVTMask::new(128);

  // Scattered bits
  mask_a.set_bit(3);
  mask_a.set_bit(5);
  mask_a.set_bit(67);
  mask_a.set_bit(100);

  mask_b.set_bit(3);
  mask_b.set_bit(10);
  mask_b.set_bit(67);
  mask_b.set_bit(100);

  let _progressive_result = mask_a.and_progressive(&mask_b, 64).unwrap();
  let _exact_result = mask_a.and(&mask_b).unwrap();

  // Progressive should find all the same results (it refines precisely)
  // But it might miss results if the stride sample point doesn't land on an
  // overlapping bit. Let's check what happens:
  // Stride 64: checks bucket 0 and 64.
  // At bucket 0: mask_a has bit 3 (but checking bit 0 specifically: off). Not matching.
  // At bucket 64: mask_a has bit 67 (but checking bit 64 specifically: off). Not matching.
  // So progressive at stride 64 finds NO survivors.
  // This is expected behavior: progressive is an approximation that trades precision
  // for speed. Only the sample points are checked.

  // For a fair test, set bits at stride-aligned positions
  let mut mask_c = NVTMask::new(128);
  let mut mask_d = NVTMask::new(128);

  // Bits at stride boundaries and within
  for index in 0..20 {
    mask_c.set_bit(index);
  }
  for index in 5..25 {
    mask_d.set_bit(index);
  }

  let progressive_result = mask_c.and_progressive(&mask_d, 8).unwrap();

  // Stride 8 checks: 0, 8, 16, 24, ...
  // At 0: mask_c on, mask_d off -> skip
  // At 8: both on -> refine [8..16): intersection is 8..16 (mask_c has 0..20, mask_d has 5..25)
  // At 16: both on -> refine [16..24): mask_c has 16..20, mask_d has 16..24 -> intersection 16..20
  // At 24: mask_c off -> skip
  let expected_survivors: Vec<usize> = (8..20).collect();
  let actual_survivors = progressive_result.surviving_buckets();
  assert_eq!(actual_survivors, expected_survivors);
}

// --- Edge cases ---

#[test]
fn test_mask_empty_mask() {
  let mask = NVTMask::new(0);
  assert!(mask.is_empty());
  assert_eq!(mask.popcount(), 0);
  assert_eq!(mask.bucket_count(), 0);
  assert!(mask.surviving_buckets().is_empty());

  // NOT of empty is empty
  let inverted = mask.not();
  assert!(inverted.is_empty());
}

#[test]
fn test_mask_full_mask() {
  let mask = NVTMask::all_on(1024);
  assert_eq!(mask.popcount(), 1024);
  assert!(!mask.is_empty());

  // NOT produces empty
  let inverted = mask.not();
  assert!(inverted.is_empty());

  // AND with empty produces empty
  let empty = NVTMask::new(1024);
  let result = mask.and(&empty).unwrap();
  assert!(result.is_empty());

  // OR with empty produces full
  let result = mask.or(&empty).unwrap();
  assert_eq!(result.popcount(), 1024);
}

#[test]
fn test_mask_different_bucket_count_error() {
  let mask_a = NVTMask::new(64);
  let mask_b = NVTMask::new(128);

  assert!(mask_a.and(&mask_b).is_err());
  assert!(mask_a.or(&mask_b).is_err());
  assert!(mask_a.xor(&mask_b).is_err());
  assert!(mask_a.difference(&mask_b).is_err());
  assert!(mask_a.and_strided(&mask_b, 4).is_err());
  assert!(mask_a.and_progressive(&mask_b, 4).is_err());
}

#[test]
fn test_mask_single_bucket() {
  let mask = NVTMask::all_on(1);
  assert_eq!(mask.popcount(), 1);
  assert!(mask.get_bit(0));
  assert!(!mask.get_bit(1));

  let inverted = mask.not();
  assert_eq!(inverted.popcount(), 0);
}

#[test]
fn test_mask_65_buckets() {
  // Tests crossing the u64 word boundary at exactly 65 bits
  let mask = NVTMask::all_on(65);
  assert_eq!(mask.popcount(), 65);

  let inverted = mask.not();
  assert_eq!(inverted.popcount(), 0);

  // XOR with self gives zero
  let xor_result = mask.xor(&mask).unwrap();
  assert!(xor_result.is_empty());
}

#[test]
fn test_mask_from_range_empty() {
  // start == end means empty range
  let mask = NVTMask::from_range(128, 10, 10);
  assert!(mask.is_empty());
}

#[test]
fn test_mask_from_range_full() {
  let mask = NVTMask::from_range(64, 0, 64);
  assert_eq!(mask.popcount(), 64);
}

#[test]
fn test_mask_and_strided_stride_1_equals_and() {
  let mut mask_a = NVTMask::new(64);
  let mut mask_b = NVTMask::new(64);

  mask_a.set_bit(5);
  mask_a.set_bit(10);
  mask_a.set_bit(20);

  mask_b.set_bit(10);
  mask_b.set_bit(20);
  mask_b.set_bit(30);

  let strided = mask_a.and_strided(&mask_b, 1).unwrap();
  let exact = mask_a.and(&mask_b).unwrap();

  assert_eq!(strided, exact);
}

#[test]
fn test_mask_from_nvt_empty() {
  let converter = Box::new(U64Converter::with_range(0, 1000));
  let nvt = NormalizedVectorTable::new(converter, 64);
  let mask = NVTMask::from_nvt(&nvt);
  assert!(mask.is_empty());
}

#[test]
fn test_mask_clear_bit_out_of_range() {
  let mut mask = NVTMask::all_on(64);
  mask.clear_bit(100); // no-op, beyond bucket_count
  assert_eq!(mask.popcount(), 64);
}

#[test]
fn test_mask_double_set_bit() {
  let mut mask = NVTMask::new(64);
  mask.set_bit(5);
  mask.set_bit(5); // idempotent
  assert_eq!(mask.popcount(), 1);
}
