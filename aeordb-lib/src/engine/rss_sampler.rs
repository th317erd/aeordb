//! Lightweight RSS / VmHWM sampler for diagnosing GC memory peaks.
//!
//! Wraps a phase of work with a background thread that polls
//! `/proc/self/status` at a configurable cadence. Reports baseline RSS,
//! peak RSS observed during the phase, end RSS, and the VmHWM delta.
//!
//! Gated on `AEORDB_GC_MEM_PROFILE` so production builds pay nothing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Read `VmRSS:` (resident set size, in kB) from `/proc/self/status`.
/// Returns 0 if the file is missing or unparseable (e.g. on non-Linux).
pub fn read_rss_kb() -> u64 { read_proc_status_field("VmRSS:") }

/// Read `VmHWM:` (peak RSS ever observed, in kB).
pub fn read_hwm_kb() -> u64 { read_proc_status_field("VmHWM:") }

fn read_proc_status_field(name: &str) -> u64 {
  let Ok(s) = std::fs::read_to_string("/proc/self/status") else { return 0; };
  for line in s.lines() {
    if let Some(rest) = line.strip_prefix(name) {
      // Format is `VmRSS:    12345 kB`; we want the integer.
      return rest
        .trim()
        .trim_end_matches(" kB")
        .split_ascii_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    }
  }
  0
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
  fn read_rss_returns_nonzero_on_linux() {
    if cfg!(target_os = "linux") {
      let rss = read_rss_kb();
      assert!(rss > 0, "expected nonzero VmRSS on Linux, got {rss}");
    }
  }

  #[test]
  fn read_hwm_returns_nonzero_on_linux() {
    if cfg!(target_os = "linux") {
      let hwm = read_hwm_kb();
      assert!(hwm > 0, "expected nonzero VmHWM on Linux, got {hwm}");
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
