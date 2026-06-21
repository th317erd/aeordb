'use strict';

const POLL_INTERVAL_MS = 15000;
const MAX_ACTIVITY_POINTS = 60;

function normalizeStatsEnvelope(raw) {
  if (!raw)
    return null;
  return raw.payload || raw;
}

function activityPointFromStats(data) {
  return {
    timestamp: Date.now(),
    writesPerSecond: data.throughput?.writes_per_sec?.['1m'] || 0,
    readsPerSecond: data.throughput?.reads_per_sec?.['1m'] || 0,
    bytesWrittenPerSecond: data.throughput?.bytes_written_per_sec?.['1m'] || 0,
    bytesReadPerSecond: data.throughput?.bytes_read_per_sec?.['1m'] || 0,
  };
}

class DashboardMetricsStore {
  constructor() {
    this._started = false;
    this._sessionKey = null;
    this._eventSource = null;
    this._pollInterval = null;
    this._fetchPromise = null;
    this._subscribers = new Set();
    this._stats = null;
    this._activityHistory = [];
    this._error = null;
    this._loading = false;
  }

  start(options = {}) {
    const auth = window.AUTH || null;
    const token = auth?.token || '';
    const allowNoAuth = options.allowNoAuth === true || window.aeordbAuthDisabled === true;

    if (auth?._isShareSession)
      return;
    if (!token && !allowNoAuth)
      return;

    const sessionKey = token || 'auth-disabled';
    if (this._started && this._sessionKey === sessionKey)
      return;

    this.stop({ clear: this._sessionKey !== sessionKey });
    this._started = true;
    this._sessionKey = sessionKey;
    this._loading = !this._stats;
    this._notify();

    this.fetchStats();
    this._connectSSE();
  }

  stop(options = {}) {
    if (this._eventSource) {
      this._eventSource.close();
      this._eventSource = null;
    }

    if (this._pollInterval) {
      clearInterval(this._pollInterval);
      this._pollInterval = null;
    }

    this._started = false;
    this._sessionKey = null;
    this._fetchPromise = null;
    this._loading = false;

    if (options.clear !== false) {
      this._stats = null;
      this._activityHistory = [];
      this._error = null;
    }

    this._notify();
  }

  subscribe(listener) {
    this._subscribers.add(listener);
    listener(this.snapshot());
    return () => this._subscribers.delete(listener);
  }

  snapshot() {
    return {
      stats: this._stats,
      history: this._activityHistory.slice(),
      error: this._error,
      loading: this._loading,
      started: this._started,
    };
  }

  async fetchStats() {
    if (this._fetchPromise)
      return this._fetchPromise;

    this._loading = !this._stats;
    this._notify();

    this._fetchPromise = this._fetchStatsOnce()
      .catch((error) => {
        this._error = error;
        this._loading = false;
        this._notify();
      })
      .finally(() => {
        this._fetchPromise = null;
      });

    return this._fetchPromise;
  }

  async _fetchStatsOnce() {
    if (!window.api)
      throw new Error('Stats API is not initialized');

    const response = await window.api('/system/stats');
    if (!response.ok)
      throw new Error(`Stats request failed (${response.status})`);

    this._applyStats(await response.json());
  }

  _connectSSE() {
    let url = '/system/events?events=metrics';
    if (window.AUTH && window.AUTH.token)
      url += '&token=' + encodeURIComponent(window.AUTH.token);

    try {
      this._eventSource = new EventSource(url);

      this._eventSource.addEventListener('metrics', (event) => {
        try {
          this._applyStats(normalizeStatsEnvelope(JSON.parse(event.data)));
        } catch (_) {
          // Ignore malformed metrics events; the next pulse or poll will fix state.
        }
      });

      this._eventSource.onerror = () => {
        if (this._eventSource) {
          this._eventSource.close();
          this._eventSource = null;
        }
        this._startPollingFallback();
      };
    } catch (_) {
      this._startPollingFallback();
    }
  }

  _startPollingFallback() {
    if (this._pollInterval)
      return;
    this._pollInterval = setInterval(() => this.fetchStats(), POLL_INTERVAL_MS);
  }

  _applyStats(data) {
    if (!data)
      return;

    // Metrics SSE omits static identity. Preserve the last full identity from
    // /system/stats so the Dashboard header remains populated.
    this._stats = { ...(this._stats || {}), ...data };
    this._activityHistory.push(activityPointFromStats(this._stats));
    if (this._activityHistory.length > MAX_ACTIVITY_POINTS)
      this._activityHistory.shift();

    this._error = null;
    this._loading = false;
    this._notify();
  }

  _notify() {
    const snapshot = this.snapshot();
    for (const subscriber of this._subscribers) {
      try {
        subscriber(snapshot);
      } catch (_) {
        // Subscriber rendering failures should not break metrics collection.
      }
    }
  }
}

export const dashboardMetrics = new DashboardMetricsStore();
window.dashboardMetrics = dashboardMetrics;
