//! Bounded logical progress for synchronous maintenance passes.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::engine::errors::EngineResult;
use crate::engine::kv_page_provider::{KvPageActivity, KvPageActivityMonitor, KvPageProvider};

const REPORT_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) struct MaintenanceProgress {
  phase: &'static str,
  unit: &'static str,
  started: Instant,
  last_report: Cell<Instant>,
  units: Cell<u64>,
  completed: Cell<bool>,
  cache: Option<KvPageActivityMonitor>,
}

impl MaintenanceProgress {
  pub(crate) fn run<T>(
    phase: &'static str,
    unit: &'static str,
    cache: Option<KvPageProvider>,
    action: impl FnOnce(&Self) -> EngineResult<T>,
  ) -> EngineResult<T> {
    let progress = Self::new(phase, unit, cache);
    let result = action(&progress)?;
    progress.complete();
    Ok(result)
  }

  pub(crate) fn new(phase: &'static str, unit: &'static str, cache: Option<KvPageProvider>) -> Self {
    let started = Instant::now();
    let progress = Self {
      phase,
      unit,
      started,
      last_report: Cell::new(started),
      units: Cell::new(0),
      completed: Cell::new(false),
      cache: cache.as_ref().map(KvPageProvider::activity_monitor),
    };
    progress.report("started", started);
    progress
  }

  pub(crate) fn complete(&self) {
    self.completed.set(true);
    self.report("completed", Instant::now());
  }

  pub(crate) fn advance(&self, units: u64) {
    self.advance_at(units, Instant::now());
  }

  fn advance_at(&self, units: u64, now: Instant) {
    self.units.set(self.units.get().saturating_add(units));
    if now.saturating_duration_since(self.last_report.get()) >= REPORT_INTERVAL {
      self.report("running", now);
      self.last_report.set(now);
    }
  }

  fn report(&self, status: &'static str, now: Instant) {
    let elapsed = now.saturating_duration_since(self.started);
    let cache = match &self.cache {
      Some(monitor) => monitor.activity(),
      None => KvPageActivity { state: "absent", ..Default::default() },
    };
    let KvPageActivity {
      state,
      resident_pages,
      resident_bytes,
      hits,
      misses,
      disk_reads,
      evictions,
      eviction_candidates,
      read_failures,
      cache_deferrals,
    } = cache;
    tracing::info!(
      target: "aeordb::maintenance_progress",
      phase = self.phase, unit = self.unit, status, units = self.units.get(),
      elapsed_ms = elapsed.as_millis() as u64,
      average_units_per_second = self.units.get() as f64 / elapsed.as_secs_f64().max(0.001),
      cache_state = state, cache_resident_pages = resident_pages, cache_resident_bytes = resident_bytes,
      cache_hits = hits, cache_misses = misses, cache_disk_reads = disk_reads, cache_evictions = evictions,
      cache_eviction_candidates = eviction_candidates, cache_read_failures = read_failures, cache_deferrals,
      "AeorDB maintenance progress"
    );
  }
}

impl Drop for MaintenanceProgress {
  fn drop(&mut self) {
    if !self.completed.get() {
      self.report("interrupted", Instant::now());
    }
  }
}

#[cfg(test)]
#[path = "../../spec/engine/maintenance_progress_internal_spec.rs"]
mod maintenance_progress_internal_spec;
