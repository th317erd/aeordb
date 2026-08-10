use aeordb::engine::rss_sampler::{try_read_process_memory, try_read_process_memory_detailed};

#[test]
fn supported_platform_process_memory_is_available_and_nonzero() {
  if !cfg!(any(target_os = "linux", target_os = "macos", target_os = "windows")) {
    return;
  }

  let memory = try_read_process_memory().expect("read process memory");
  assert!(memory.resident_kb > 0, "resident memory must not be fabricated as zero");
  assert!(memory.peak_resident_kb >= memory.resident_kb);

  let detailed = try_read_process_memory_detailed().expect("read detailed process memory");
  assert!(detailed.resident_kb > 0);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_status_parser_requires_valid_core_memory_evidence() {
  use aeordb::engine::rss_sampler::parse_linux_proc_status;

  let valid = "Name:\taeordb\nVmSize:\t2048 kB\nVmHWM:\t1024 kB\nVmRSS:\t768 kB\nVmData:\t512 kB\nVmSwap:\t0 kB\nThreads:\t4\n";
  let memory = parse_linux_proc_status(valid).expect("parse valid proc status");
  assert_eq!(memory.resident_kb, 768);
  assert_eq!(memory.peak_resident_kb, 1024);
  assert_eq!(memory.virtual_kb, 2048);
  assert_eq!(memory.data_kb, 512);
  assert_eq!(memory.thread_count, 4);

  let missing_rss = "VmSize:\t2048 kB\nVmHWM:\t1024 kB\n";
  let error = parse_linux_proc_status(missing_rss).unwrap_err();
  assert!(error.to_string().contains("VmRSS"), "{error}");

  let malformed_rss = "VmSize:\t2048 kB\nVmHWM:\t1024 kB\nVmRSS:\tnot-a-number kB\n";
  let error = parse_linux_proc_status(malformed_rss).unwrap_err();
  assert!(error.to_string().contains("VmRSS"), "{error}");
}
