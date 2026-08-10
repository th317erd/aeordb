//! Lightweight RSS / VmHWM sampler for diagnosing memory peaks.
//!
//! Wraps a phase of work with a background thread that polls process
//! resident-set-size at a configurable cadence. Reports baseline RSS,
//! peak RSS observed during the phase, end RSS, and the HWM delta.
//!
//! Cross-platform: Linux reads `/proc/self/status` (VmRSS, VmHWM, etc.).
//! macOS calls Mach `task_info(MACH_TASK_BASIC_INFO)` for resident_size and
//! resident_size_max. Windows calls `K32GetProcessMemoryInfo`. All values are
//! reported in kB to match the Linux `/proc` semantics.
//!
//! Gated on `AEORDB_GC_MEM_PROFILE` so production builds pay nothing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{io, str::FromStr};

/// Current resident set size in kB. 0 if unavailable.
pub fn read_rss_kb() -> u64 {
  read_process_memory().resident_kb
}

/// Peak resident set size observed by the kernel for this process, in kB.
/// 0 if unavailable. Monotonic-non-decreasing for the life of the process.
pub fn read_hwm_kb() -> u64 {
  read_process_memory().peak_resident_kb
}

/// Aggregate process memory stats. Values in kB to match Linux `/proc` units.
/// Fields the host platform doesn't expose are 0.
#[derive(Default, Debug, Clone, Copy)]
pub struct ProcessMemory {
  pub resident_kb: u64,      // current RSS  (Linux VmRSS / macOS resident_size)
  pub peak_resident_kb: u64, // peak  RSS  (Linux VmHWM / macOS resident_size_max)
  pub virtual_kb: u64,       // virtual size (Linux VmSize / macOS virtual_size)
  pub data_kb: u64,          // heap+data segment (Linux VmData; 0 on macOS)
  pub swap_kb: u64,          // Linux VmSwap; 0 when unavailable
  pub thread_count: u64,     // Linux Threads; 0 when unavailable
  pub fd_count: u64,         // open file descriptors; 0 when unavailable
  pub private_kb: Option<u64>,
  pub shared_kb: Option<u64>,
  pub mapped_kb: Option<u64>,
  pub allocator_kb: Option<u64>,
}

pub fn read_process_memory() -> ProcessMemory {
  try_read_process_memory().unwrap_or_default()
}

/// Read the current process memory without converting platform observation
/// failures into a plausible all-zero sample.
pub fn try_read_process_memory() -> io::Result<ProcessMemory> {
  #[cfg(target_os = "linux")]
  {
    read_linux_proc_status()
  }
  #[cfg(target_os = "macos")]
  {
    read_macos_task_info().ok_or_else(|| io::Error::other("Mach task_info failed while reading process memory"))
  }
  #[cfg(target_os = "windows")]
  {
    read_windows_process_memory().ok_or_else(|| io::Error::last_os_error())
  }
  #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
  {
    Err(io::Error::new(io::ErrorKind::Unsupported, "process memory observation is unsupported on this platform"))
  }
}

/// Process memory with the platform's more expensive ownership breakdown.
/// Fast RSS/HWM samplers intentionally use [`read_process_memory`] instead.
pub fn read_process_memory_detailed() -> ProcessMemory {
  try_read_process_memory_detailed().unwrap_or_default()
}

/// Read process memory plus optional ownership details. Missing optional
/// platform breakdowns remain `None`, while failure of the core process sample
/// is returned to the caller.
pub fn try_read_process_memory_detailed() -> io::Result<ProcessMemory> {
  #[cfg(target_os = "linux")]
  {
    let mut process = try_read_process_memory()?;
    if let Ok(rollup) = std::fs::read_to_string("/proc/self/smaps_rollup") {
      if let Some(rollup) = parse_linux_smaps_rollup(&rollup) {
        process.private_kb = Some(rollup.private_kb);
        process.shared_kb = Some(rollup.shared_kb);
        process.mapped_kb = Some(rollup.mapped_kb);
      }
    }
    Ok(process)
  }
  #[cfg(not(target_os = "linux"))]
  {
    try_read_process_memory()
  }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinuxSmapsRollup {
  pub private_kb: u64,
  pub shared_kb: u64,
  pub mapped_kb: u64,
}

/// Parse the ownership fields used from Linux `/proc/<pid>/smaps_rollup`.
/// Unknown and malformed lines are ignored; `None` means no supported field
/// supplied trustworthy numeric evidence.
#[cfg(target_os = "linux")]
pub fn parse_linux_smaps_rollup(input: &str) -> Option<LinuxSmapsRollup> {
  const PRIVATE_CLEAN: u8 = 1 << 0;
  const PRIVATE_DIRTY: u8 = 1 << 1;
  const SHARED_CLEAN: u8 = 1 << 2;
  const SHARED_DIRTY: u8 = 1 << 3;
  const PSS_FILE: u8 = 1 << 4;
  const PSS_SHMEM: u8 = 1 << 5;
  const REQUIRED_FIELDS: u8 = PRIVATE_CLEAN | PRIVATE_DIRTY | SHARED_CLEAN | SHARED_DIRTY | PSS_FILE | PSS_SHMEM;

  let mut private_kb = 0u64;
  let mut shared_kb = 0u64;
  let mut mapped_kb = 0u64;
  let mut seen = 0u8;

  for line in input.lines() {
    let Some((name, value)) = line.split_once(':') else {
      continue;
    };
    let Some(value) = value.trim().split_ascii_whitespace().next().and_then(|value| value.parse::<u64>().ok()) else {
      continue;
    };
    match name {
      "Private_Clean" => {
        private_kb = private_kb.saturating_add(value);
        seen |= PRIVATE_CLEAN;
      }
      "Private_Dirty" => {
        private_kb = private_kb.saturating_add(value);
        seen |= PRIVATE_DIRTY;
      }
      "Private_Hugetlb" => private_kb = private_kb.saturating_add(value),
      "Shared_Clean" => {
        shared_kb = shared_kb.saturating_add(value);
        seen |= SHARED_CLEAN;
      }
      "Shared_Dirty" => {
        shared_kb = shared_kb.saturating_add(value);
        seen |= SHARED_DIRTY;
      }
      "Shared_Hugetlb" => shared_kb = shared_kb.saturating_add(value),
      "Pss_File" => {
        mapped_kb = mapped_kb.saturating_add(value);
        seen |= PSS_FILE;
      }
      "Pss_Shmem" => {
        mapped_kb = mapped_kb.saturating_add(value);
        seen |= PSS_SHMEM;
      }
      _ => {}
    }
  }

  (seen & REQUIRED_FIELDS == REQUIRED_FIELDS).then_some(LinuxSmapsRollup { private_kb, shared_kb, mapped_kb })
}

/// Host memory currently available without swapping, in bytes. `None` means
/// the platform probe failed or the platform has no supported native probe.
pub fn read_host_available_bytes() -> Option<u64> {
  #[cfg(target_os = "linux")]
  {
    let memory = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kilobytes = memory.lines().find_map(|line| {
      line.strip_prefix("MemAvailable:")?.trim().trim_end_matches(" kB").split_ascii_whitespace().next()?.parse::<u64>().ok()
    })?;
    return kilobytes.checked_mul(1024);
  }
  #[cfg(target_os = "windows")]
  {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
      return Some(status.ullAvailPhys);
    }
    return None;
  }
  #[cfg(target_os = "macos")]
  {
    return read_macos_available_bytes();
  }
  #[allow(unreachable_code)]
  None
}

#[cfg(target_os = "macos")]
fn read_macos_available_bytes() -> Option<u64> {
  #[repr(C)]
  #[derive(Default)]
  struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
  }

  const HOST_VM_INFO64: i32 = 4;
  const KERN_SUCCESS: i32 = 0;
  unsafe extern "C" {
    fn mach_host_self() -> u32;
    fn host_page_size(host: u32, page_size: *mut u32) -> i32;
    fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
  }

  let host = unsafe { mach_host_self() };
  let mut page_size = 0u32;
  if unsafe { host_page_size(host, &mut page_size) } != KERN_SUCCESS || page_size == 0 {
    return None;
  }
  let mut statistics = VmStatistics64::default();
  let mut count = (std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<u32>()) as u32;
  if unsafe { host_statistics64(host, HOST_VM_INFO64, &mut statistics as *mut VmStatistics64 as *mut i32, &mut count) } != KERN_SUCCESS {
    return None;
  }
  u64::from(statistics.free_count)
    .saturating_add(u64::from(statistics.inactive_count))
    .saturating_add(u64::from(statistics.speculative_count))
    .saturating_add(u64::from(statistics.purgeable_count))
    .checked_mul(u64::from(page_size))
}

#[cfg(target_os = "linux")]
fn read_linux_proc_status() -> io::Result<ProcessMemory> {
  let status = std::fs::read_to_string("/proc/self/status")?;
  let mut process = parse_linux_proc_status(&status)?;
  process.fd_count = read_fd_count();
  Ok(process)
}

#[cfg(target_os = "linux")]
pub fn parse_linux_proc_status(status: &str) -> io::Result<ProcessMemory> {
  let mut out = ProcessMemory::default();
  let mut resident_seen = false;
  let mut peak_seen = false;
  let mut virtual_seen = false;
  let parse = |line: &str, prefix: &str| -> io::Result<Option<u64>> {
    let Some(value) = line.strip_prefix(prefix) else {
      return Ok(None);
    };
    let value = value
      .trim()
      .split_ascii_whitespace()
      .next()
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{prefix} has no numeric value")))?;
    u64::from_str(value)
      .map(Some)
      .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{prefix} has invalid numeric value {value:?}")))
  };
  for line in status.lines() {
    if let Some(v) = parse(line, "VmRSS:")? {
      out.resident_kb = v;
      resident_seen = true;
    }
    if let Some(v) = parse(line, "VmHWM:")? {
      out.peak_resident_kb = v;
      peak_seen = true;
    }
    if let Some(v) = parse(line, "VmSize:")? {
      out.virtual_kb = v;
      virtual_seen = true;
    }
    if let Some(v) = parse(line, "VmData:")? {
      out.data_kb = v;
    }
    if let Some(v) = parse(line, "VmSwap:")? {
      out.swap_kb = v;
    }
    if let Some(v) = parse(line, "Threads:")? {
      out.thread_count = v;
    }
  }
  if !resident_seen {
    return Err(io::Error::new(io::ErrorKind::InvalidData, "/proc status is missing VmRSS"));
  }
  if !peak_seen {
    return Err(io::Error::new(io::ErrorKind::InvalidData, "/proc status is missing VmHWM"));
  }
  if !virtual_seen {
    return Err(io::Error::new(io::ErrorKind::InvalidData, "/proc status is missing VmSize"));
  }
  Ok(out)
}

#[cfg(target_os = "macos")]
fn read_macos_task_info() -> Option<ProcessMemory> {
  // mach_task_basic_info from <mach/task_info.h>. We declare the struct
  // and the syscalls ourselves to avoid pulling in a Mach FFI crate just
  // for this one call. Values come back in bytes; we divide to match the
  // Linux /proc kB convention.
  use std::mem::size_of;

  #[repr(C)]
  struct TimeValue {
    seconds: i32,
    microseconds: i32,
  }
  #[repr(C)]
  struct MachTaskBasicInfo {
    virtual_size: u64,
    resident_size: u64,
    resident_size_max: u64,
    user_time: TimeValue,
    system_time: TimeValue,
    policy: i32,
    suspend_count: i32,
  }

  const MACH_TASK_BASIC_INFO: u32 = 20;
  const KERN_SUCCESS: i32 = 0;

  extern "C" {
    fn mach_task_self() -> u32;
    fn task_info(task: u32, flavor: u32, info_out: *mut i32, count: *mut u32) -> i32;
  }

  let mut info: MachTaskBasicInfo = unsafe { std::mem::zeroed() };
  let mut count: u32 = (size_of::<MachTaskBasicInfo>() / size_of::<i32>()) as u32;
  let result = unsafe { task_info(mach_task_self(), MACH_TASK_BASIC_INFO, &mut info as *mut MachTaskBasicInfo as *mut i32, &mut count) };
  if result != KERN_SUCCESS {
    return None;
  }
  Some(ProcessMemory {
    resident_kb: info.resident_size / 1024,
    peak_resident_kb: info.resident_size_max / 1024,
    virtual_kb: info.virtual_size / 1024,
    // macOS doesn't expose heap-vs-data the way Linux does via VmData.
    // Leave 0 here; the wide_rss.tsv consumer treats it as "unavailable".
    data_kb: 0,
    swap_kb: 0,
    thread_count: 0,
    fd_count: read_fd_count(),
    private_kb: None,
    shared_kb: None,
    mapped_kb: None,
    allocator_kb: None,
  })
}

#[cfg(target_os = "windows")]
fn read_windows_process_memory() -> Option<ProcessMemory> {
  use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
  use windows_sys::Win32::System::Threading::GetCurrentProcess;

  let mut counters = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
  let result = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
  if result == 0 {
    return None;
  }
  Some(ProcessMemory {
    resident_kb: counters.WorkingSetSize as u64 / 1024,
    peak_resident_kb: counters.PeakWorkingSetSize as u64 / 1024,
    virtual_kb: 0,
    data_kb: 0,
    swap_kb: 0,
    thread_count: 0,
    fd_count: 0,
    private_kb: None,
    shared_kb: None,
    mapped_kb: None,
    allocator_kb: None,
  })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_fd_count() -> u64 {
  let path = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
  std::fs::read_dir(path).map(|entries| entries.count() as u64).unwrap_or(0)
}

/// Returns true when `AEORDB_GC_MEM_PROFILE` is set (any non-empty value).
pub fn enabled() -> bool {
  std::env::var("AEORDB_GC_MEM_PROFILE").map(|v| !v.is_empty()).unwrap_or(false)
}

/// One sampled phase. Construct with `start`, drop or call `finish` to stop.
pub struct PhaseSampler {
  label: &'static str,
  baseline_kb: u64,
  baseline_hwm_kb: u64,
  peak_kb: Arc<AtomicU64>,
  stop: Arc<AtomicBool>,
  handle: Option<JoinHandle<()>>,
  start: std::time::Instant,
}

impl PhaseSampler {
  /// Spawn the sampler; if disabled, returns a no-op sampler that does nothing on finish.
  pub fn start(label: &'static str, interval: Duration) -> Self {
    if !enabled() {
      return Self {
        label,
        baseline_kb: 0,
        baseline_hwm_kb: 0,
        peak_kb: Arc::new(AtomicU64::new(0)),
        stop: Arc::new(AtomicBool::new(true)),
        handle: None,
        start: std::time::Instant::now(),
      };
    }
    let baseline_kb = read_rss_kb();
    let baseline_hwm_kb = read_hwm_kb();
    let peak_kb = Arc::new(AtomicU64::new(baseline_kb));
    let stop = Arc::new(AtomicBool::new(false));
    let peak_for_thread = Arc::clone(&peak_kb);
    let stop_for_thread = Arc::clone(&stop);
    let handle = match thread::Builder::new().name(format!("rss-sampler-{}", label)).spawn(move || {
      while !stop_for_thread.load(Ordering::Relaxed) {
        let rss = read_rss_kb();
        // Race-free max update.
        let mut cur = peak_for_thread.load(Ordering::Relaxed);
        while rss > cur {
          match peak_for_thread.compare_exchange_weak(cur, rss, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => cur = observed,
          }
        }
        thread::sleep(interval);
      }
    }) {
      Ok(handle) => Some(handle),
      Err(error) => {
        eprintln!("[gc-mem] {label}: failed to spawn RSS sampler thread: {error}");
        None
      }
    };
    Self { label, baseline_kb, baseline_hwm_kb, peak_kb, stop, handle, start: std::time::Instant::now() }
  }

  /// Stop the sampler and emit a one-line summary to stderr.
  pub fn finish(mut self) {
    self.finish_inner();
  }

  fn finish_inner(&mut self) {
    if !enabled() {
      return;
    }
    self.stop.store(true, Ordering::Relaxed);
    if let Some(handle) = self.handle.take() {
      if handle.join().is_err() {
        eprintln!("[gc-mem] {}: RSS sampler thread panicked", self.label);
      }
    }
    let end_kb = read_rss_kb();
    let end_hwm_kb = read_hwm_kb();
    let peak_kb = self.peak_kb.load(Ordering::Relaxed);
    // Sample the kernel HWM in case our sampler missed the actual peak.
    let effective_peak_kb = peak_kb.max(end_hwm_kb.saturating_sub(self.baseline_hwm_kb).saturating_add(self.baseline_kb));
    eprintln!(
      "[gc-mem] {}: baseline_rss={} MB peak_rss={} MB end_rss={} MB delta_hwm={} MB elapsed={:?}",
      self.label,
      self.baseline_kb / 1024,
      effective_peak_kb / 1024,
      end_kb / 1024,
      end_hwm_kb.saturating_sub(self.baseline_hwm_kb) / 1024,
      self.start.elapsed(),
    );
  }
}

impl Drop for PhaseSampler {
  fn drop(&mut self) {
    // If the user forgets to call finish, do it on drop so we still get output.
    if self.handle.is_some() {
      self.finish_inner();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn read_rss_returns_nonzero_on_supported_platforms() {
    if cfg!(any(target_os = "linux", target_os = "macos", target_os = "windows")) {
      let rss = read_rss_kb();
      assert!(rss > 0, "expected nonzero RSS, got {rss}");
    }
  }

  #[test]
  fn read_hwm_returns_nonzero_on_supported_platforms() {
    if cfg!(any(target_os = "linux", target_os = "macos", target_os = "windows")) {
      let hwm = read_hwm_kb();
      assert!(hwm > 0, "expected nonzero peak RSS, got {hwm}");
    }
  }

  #[test]
  fn read_process_memory_is_internally_consistent() {
    if !cfg!(any(target_os = "linux", target_os = "macos", target_os = "windows")) {
      return;
    }
    let m = read_process_memory();
    // HWM is monotonic upper bound on RSS, so HWM >= RSS always.
    assert!(m.peak_resident_kb >= m.resident_kb, "peak RSS {} should be >= current RSS {}", m.peak_resident_kb, m.resident_kb);
    // Virtual size is always >= resident size (you can have unmapped pages
    // in your address space but you can't have resident bytes outside it).
    if m.virtual_kb > 0 {
      assert!(m.virtual_kb >= m.resident_kb, "virtual {} should be >= resident {}", m.virtual_kb, m.resident_kb);
    }
  }

  #[test]
  fn sampler_disabled_when_env_unset() {
    std::env::remove_var("AEORDB_GC_MEM_PROFILE");
    let s = PhaseSampler::start("test", Duration::from_millis(10));
    assert!(s.handle.is_none(), "sampler thread should not start when disabled");
    s.finish();
  }

  #[test]
  fn enabled_reads_env() {
    std::env::set_var("AEORDB_GC_MEM_PROFILE", "1");
    assert!(enabled());
    std::env::remove_var("AEORDB_GC_MEM_PROFILE");
    assert!(!enabled());
  }
}
