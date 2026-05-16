//! Lightweight RSS / VmHWM sampler for diagnosing memory peaks.
//!
//! Wraps a phase of work with a background thread that polls process
//! resident-set-size at a configurable cadence. Reports baseline RSS,
//! peak RSS observed during the phase, end RSS, and the HWM delta.
//!
//! Cross-platform: Linux reads `/proc/self/status` (VmRSS, VmHWM, etc.).
//! macOS calls Mach `task_info(MACH_TASK_BASIC_INFO)` for resident_size and
//! resident_size_max. All values are reported in kB to match the Linux
//! `/proc` semantics.
//!
//! Gated on `AEORDB_GC_MEM_PROFILE` so production builds pay nothing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Current resident set size in kB. 0 if unavailable.
pub fn read_rss_kb() -> u64 { read_process_memory().resident_kb }

/// Peak resident set size observed by the kernel for this process, in kB.
/// 0 if unavailable. Monotonic-non-decreasing for the life of the process.
pub fn read_hwm_kb() -> u64 { read_process_memory().peak_resident_kb }

/// Aggregate process memory stats. Values in kB to match Linux `/proc` units.
/// Fields the host platform doesn't expose are 0.
#[derive(Default, Debug, Clone, Copy)]
pub struct ProcessMemory {
  pub resident_kb: u64,      // current RSS  (Linux VmRSS / macOS resident_size)
  pub peak_resident_kb: u64, // peak  RSS  (Linux VmHWM / macOS resident_size_max)
  pub virtual_kb: u64,       // virtual size (Linux VmSize / macOS virtual_size)
  pub data_kb: u64,          // heap+data segment (Linux VmData; 0 on macOS)
}

pub fn read_process_memory() -> ProcessMemory {
  #[cfg(target_os = "linux")]
  { read_linux_proc_status() }
  #[cfg(target_os = "macos")]
  { read_macos_task_info().unwrap_or_default() }
  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  { ProcessMemory::default() }
}

#[cfg(target_os = "linux")]
fn read_linux_proc_status() -> ProcessMemory {
  let Ok(s) = std::fs::read_to_string("/proc/self/status") else {
    return ProcessMemory::default();
  };
  let mut out = ProcessMemory::default();
  let parse = |line: &str, prefix: &str| -> Option<u64> {
    line
      .strip_prefix(prefix)?
      .trim()
      .trim_end_matches(" kB")
      .split_ascii_whitespace()
      .next()?
      .parse()
      .ok()
  };
  for line in s.lines() {
    if let Some(v) = parse(line, "VmRSS:")  { out.resident_kb = v; }
    if let Some(v) = parse(line, "VmHWM:")  { out.peak_resident_kb = v; }
    if let Some(v) = parse(line, "VmSize:") { out.virtual_kb = v; }
    if let Some(v) = parse(line, "VmData:") { out.data_kb = v; }
  }
  out
}

#[cfg(target_os = "macos")]
fn read_macos_task_info() -> Option<ProcessMemory> {
  // mach_task_basic_info from <mach/task_info.h>. We declare the struct
  // and the syscalls ourselves to avoid pulling in a Mach FFI crate just
  // for this one call. Values come back in bytes; we divide to match the
  // Linux /proc kB convention.
  use std::mem::size_of;

  #[repr(C)]
  struct TimeValue { seconds: i32, microseconds: i32 }
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
    fn task_info(
      task: u32,
      flavor: u32,
      info_out: *mut i32,
      count: *mut u32,
    ) -> i32;
  }

  let mut info: MachTaskBasicInfo = unsafe { std::mem::zeroed() };
  let mut count: u32 = (size_of::<MachTaskBasicInfo>() / size_of::<i32>()) as u32;
  let result = unsafe {
    task_info(
      mach_task_self(),
      MACH_TASK_BASIC_INFO,
      &mut info as *mut MachTaskBasicInfo as *mut i32,
      &mut count,
    )
  };
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
  })
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
    let handle = thread::Builder::new()
      .name(format!("rss-sampler-{}", label))
      .spawn(move || {
        while !stop_for_thread.load(Ordering::Relaxed) {
          let rss = read_rss_kb();
          // Race-free max update.
          let mut cur = peak_for_thread.load(Ordering::Relaxed);
          while rss > cur {
            match peak_for_thread.compare_exchange_weak(
              cur, rss, Ordering::Relaxed, Ordering::Relaxed,
            ) {
              Ok(_) => break,
              Err(observed) => cur = observed,
            }
          }
          thread::sleep(interval);
        }
      })
      .ok();
    Self { label, baseline_kb, baseline_hwm_kb, peak_kb, stop, handle, start: std::time::Instant::now() }
  }

  /// Stop the sampler and emit a one-line summary to stderr.
  pub fn finish(mut self) {
    self.finish_inner();
  }

  fn finish_inner(&mut self) {
    if !enabled() { return; }
    self.stop.store(true, Ordering::Relaxed);
    if let Some(h) = self.handle.take() { let _ = h.join(); }
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
    if cfg!(any(target_os = "linux", target_os = "macos")) {
      let rss = read_rss_kb();
      assert!(rss > 0, "expected nonzero RSS, got {rss}");
    }
  }

  #[test]
  fn read_hwm_returns_nonzero_on_supported_platforms() {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
      let hwm = read_hwm_kb();
      assert!(hwm > 0, "expected nonzero peak RSS, got {hwm}");
    }
  }

  #[test]
  fn read_process_memory_is_internally_consistent() {
    if !cfg!(any(target_os = "linux", target_os = "macos")) { return; }
    let m = read_process_memory();
    // HWM is monotonic upper bound on RSS, so HWM >= RSS always.
    assert!(m.peak_resident_kb >= m.resident_kb,
      "peak RSS {} should be >= current RSS {}", m.peak_resident_kb, m.resident_kb);
    // Virtual size is always >= resident size (you can have unmapped pages
    // in your address space but you can't have resident bytes outside it).
    if m.virtual_kb > 0 {
      assert!(m.virtual_kb >= m.resident_kb,
        "virtual {} should be >= resident {}", m.virtual_kb, m.resident_kb);
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
