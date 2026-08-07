'use strict';

const POLL_INTERVAL_MS = 15000;
const MAX_ACTIVITY_POINTS = 60;
const ROOT_USER_ID = '00000000-0000-0000-0000-000000000000';

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
    const isRoot = allowNoAuth || (auth?.currentUserId && auth.currentUserId() === ROOT_USER_ID);
    if (isRoot) {
      this._connectSSE();
    } else {
      this._startPollingFallback();
    }
  }

  stop(options = {}) {
    this._closeEventSource();

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
        } catch (error) {
          this._error = new Error(`Malformed metrics SSE event: ${error.message}`);
          this._notify();
          this._startPollingFallback();
        }
      });

      this._eventSource.onerror = () => {
        this._startPollingFallback();
      };
    } catch (error) {
      this._error = new Error(`Metrics SSE connection failed: ${error.message}`);
      this._notify();
      this._startPollingFallback();
    }
  }

  _closeEventSource() {
    if (!this._eventSource)
      return;

    this._eventSource.close();
    this._eventSource = null;
  }

  _startPollingFallback() {
    this._closeEventSource();
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
      } catch (error) {
        console.error('Dashboard metrics subscriber failed', error);
      }
    }
  }
}

export const dashboardMetrics = new DashboardMetricsStore();
window.dashboardMetrics = dashboardMetrics;
