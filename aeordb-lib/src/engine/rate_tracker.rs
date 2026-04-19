use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;

use super::engine_counters::CountersSnapshot;

/// Computes rolling throughput rates from monotonic counter samples.
///
/// A background task calls [`record()`](RateTracker::record) every second with
/// the current counter value.  Rate methods compute ops/sec from sample deltas
/// over configurable windows (1m, 5m, 15m).
pub struct RateTracker {
    samples: Mutex<VecDeque<(u64, u64)>>, // (timestamp_ms, counter_value)
    max_samples: usize,                   // 900 = 15 minutes at 1s intervals
}

/// All four rate windows captured at a single instant.
#[derive(Debug, Clone, Serialize)]
pub struct RateSnapshot {
    #[serde(rename = "1m")]
    pub rate_1m: f64,
    #[serde(rename = "5m")]
    pub rate_5m: f64,
    #[serde(rename = "15m")]
    pub rate_15m: f64,
    pub peak_1m: f64,
}

/// All four rate trackers captured at a single instant.
#[derive(Debug, Clone, Serialize)]
pub struct RateSetSnapshot {
    pub writes: RateSnapshot,
    pub reads: RateSnapshot,
    pub bytes_written: RateSnapshot,
    pub bytes_read: RateSnapshot,
}

impl RateTracker {
    /// Create an empty tracker.  Retains up to 900 samples (15 minutes at 1 Hz).
    pub fn new() -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
            max_samples: 900,
        }
    }

    /// Push a new `(timestamp_ms, counter_value)` sample.
    ///
    /// If the deque exceeds `max_samples`, the oldest entry is evicted.
    pub fn record(&self, timestamp_ms: u64, counter_value: u64) {
        let mut samples = self.samples.lock().unwrap();
        samples.push_back((timestamp_ms, counter_value));
        if samples.len() > self.max_samples {
            samples.pop_front();
        }
    }

    /// Average ops/sec over the last 60 seconds.
    pub fn rate_1m(&self) -> f64 {
        self.rate_over_window(60_000)
    }

    /// Average ops/sec over the last 300 seconds.
    pub fn rate_5m(&self) -> f64 {
        self.rate_over_window(300_000)
    }

    /// Average ops/sec over the last 900 seconds.
    pub fn rate_15m(&self) -> f64 {
        self.rate_over_window(900_000)
    }

    /// Maximum single-second rate observed in the last 60 samples.
    pub fn peak_1m(&self) -> f64 {
        let samples = self.samples.lock().unwrap();
        if samples.len() < 2 {
            return 0.0;
        }

        // Look at the last 60 samples (or all of them if fewer).
        let window_len = samples.len().min(60);
        let start = samples.len() - window_len;
        let mut peak = 0.0_f64;

        for i in (start + 1)..samples.len() {
            let (time_prev, value_prev) = samples[i - 1];
            let (time_curr, value_curr) = samples[i];
            let time_delta_ms = time_curr.saturating_sub(time_prev);
            if time_delta_ms == 0 {
                continue;
            }
            let value_delta = value_curr.saturating_sub(value_prev);
            let rate = value_delta as f64 / time_delta_ms as f64 * 1000.0;
            if rate > peak {
                peak = rate;
            }
        }

        peak
    }

    /// Capture all four rates into a single [`RateSnapshot`].
    pub fn snapshot(&self) -> RateSnapshot {
        // Take the lock once and compute everything while holding it.
        let samples = self.samples.lock().unwrap();
        RateSnapshot {
            rate_1m: Self::rate_over_window_inner(&samples, 60_000),
            rate_5m: Self::rate_over_window_inner(&samples, 300_000),
            rate_15m: Self::rate_over_window_inner(&samples, 900_000),
            peak_1m: Self::peak_1m_inner(&samples),
        }
    }

    /// Number of samples currently held (mostly useful for tests).
    pub fn sample_count(&self) -> usize {
        self.samples.lock().unwrap().len()
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn rate_over_window(&self, window_ms: u64) -> f64 {
        let samples = self.samples.lock().unwrap();
        Self::rate_over_window_inner(&samples, window_ms)
    }

    fn rate_over_window_inner(samples: &VecDeque<(u64, u64)>, window_ms: u64) -> f64 {
        if samples.len() < 2 {
            return 0.0;
        }

        let (newest_time, newest_value) = samples[samples.len() - 1];
        let cutoff = newest_time.saturating_sub(window_ms);

        // Walk backwards to find the sample closest to (but not after) the cutoff.
        // If all samples are within the window, use the oldest one.
        let mut oldest_index = 0;
        for i in 0..samples.len() {
            if samples[i].0 >= cutoff {
                oldest_index = i;
                break;
            }
        }

        let (oldest_time, oldest_value) = samples[oldest_index];
        let time_delta_ms = newest_time.saturating_sub(oldest_time);
        if time_delta_ms == 0 {
            return 0.0;
        }

        let value_delta = newest_value.saturating_sub(oldest_value);
        value_delta as f64 / time_delta_ms as f64 * 1000.0
    }

    fn peak_1m_inner(samples: &VecDeque<(u64, u64)>) -> f64 {
        if samples.len() < 2 {
            return 0.0;
        }

        let window_len = samples.len().min(60);
        let start = samples.len() - window_len;
        let mut peak = 0.0_f64;

        for i in (start + 1)..samples.len() {
            let (time_prev, value_prev) = samples[i - 1];
            let (time_curr, value_curr) = samples[i];
            let time_delta_ms = time_curr.saturating_sub(time_prev);
            if time_delta_ms == 0 {
                continue;
            }
            let value_delta = value_curr.saturating_sub(value_prev);
            let rate = value_delta as f64 / time_delta_ms as f64 * 1000.0;
            if rate > peak {
                peak = rate;
            }
        }

        peak
    }
}

/// Convenience set of four [`RateTracker`]s covering the primary I/O counters.
pub struct RateTrackerSet {
    pub writes: RateTracker,
    pub reads: RateTracker,
    pub bytes_written: RateTracker,
    pub bytes_read: RateTracker,
}

impl RateTrackerSet {
    pub fn new() -> Self {
        Self {
            writes: RateTracker::new(),
            reads: RateTracker::new(),
            bytes_written: RateTracker::new(),
            bytes_read: RateTracker::new(),
        }
    }

    /// Record all four counters from a snapshot taken at `timestamp_ms`.
    pub fn record_all(&self, timestamp_ms: u64, counters: &CountersSnapshot) {
        self.writes.record(timestamp_ms, counters.writes_total);
        self.reads.record(timestamp_ms, counters.reads_total);
        self.bytes_written.record(timestamp_ms, counters.bytes_written_total);
        self.bytes_read.record(timestamp_ms, counters.bytes_read_total);
    }

    /// Capture all four trackers into a single [`RateSetSnapshot`].
    pub fn snapshot(&self) -> RateSetSnapshot {
        RateSetSnapshot {
            writes: self.writes.snapshot(),
            reads: self.reads.snapshot(),
            bytes_written: self.bytes_written.snapshot(),
            bytes_read: self.bytes_read.snapshot(),
        }
    }
}
