const SELECTED_FILE_BODY_FIXED_BYTES_V1: u64 = 4 * 1_024;

pub(crate) fn selected_file_body_reservation_bytes_v1(total_size: u64) -> Option<u64> {
  total_size.checked_mul(2)?.checked_add(SELECTED_FILE_BODY_FIXED_BYTES_V1)
}
