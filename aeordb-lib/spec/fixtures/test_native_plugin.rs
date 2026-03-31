/// Minimal native plugin fixture for testing NativePluginRuntime.
///
/// Exports the `aeordb_handle` symbol with the expected C ABI signature.
/// Behavior: copies the request bytes into the response buffer (echo plugin).
/// If the request is empty, writes the literal bytes "empty" into the response.
/// Returns the number of bytes written, or -1 if the response buffer is too small.

#[no_mangle]
pub unsafe extern "C" fn aeordb_handle(
  request_ptr: *const u8,
  request_len: u32,
  response_ptr: *mut u8,
  response_capacity: u32,
) -> i32 {
  let request_length = request_len as usize;

  if request_length == 0 {
    let fallback = b"empty";
    if (response_capacity as usize) < fallback.len() {
      return -1;
    }
    std::ptr::copy_nonoverlapping(fallback.as_ptr(), response_ptr, fallback.len());
    return fallback.len() as i32;
  }

  if (response_capacity as usize) < request_length {
    return -1;
  }

  std::ptr::copy_nonoverlapping(request_ptr, response_ptr, request_length);
  request_length as i32
}
