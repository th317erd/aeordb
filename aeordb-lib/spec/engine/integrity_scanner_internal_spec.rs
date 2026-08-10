use super::integrity_cycle_offset;

#[test]
fn integrity_cycle_offset_rejects_a_pre_epoch_clock() {
  let before_epoch = std::time::UNIX_EPOCH.checked_sub(std::time::Duration::from_secs(1)).unwrap();
  let error = integrity_cycle_offset(before_epoch, 0, 10).expect_err("a pre-epoch clock must not become the first sample slice");
  assert!(error.to_string().contains("precedes Unix epoch"));
}

#[test]
fn integrity_cycle_offset_applies_jitter_within_the_stride() {
  let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(23);
  assert_eq!(integrity_cycle_offset(now, 4, 10).unwrap(), 7);
}
