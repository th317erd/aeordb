'use strict';

function formatBytes(bytes) {
  if (bytes === 0)
    return '0 B';

  const kilobyte = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const index = Math.floor(Math.log(bytes) / Math.log(kilobyte));
  return parseFloat((bytes / Math.pow(kilobyte, index)).toFixed(1)) + ' ' + sizes[index];
}

function formatNumber(number) {
  return number.toLocaleString();
}

function formatRate(value) {
  if (value == null)
    return '\u2014';

  return (value < 10) ? value.toFixed(2) : formatNumber(Math.round(value));
}

function formatPercent(value) {
  if (value == null)
    return '\u2014';

  return value.toFixed(1) + '%';
}

function formatUptime(seconds) {
  if (seconds == null)
    return '\u2014';

  const days    = Math.floor(seconds / 86400);
  const hours   = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const secs    = Math.floor(seconds % 60);

  if (days > 0)
    return `${days}d ${hours}h ${minutes}m`;

  if (hours > 0)
    return `${hours}h ${minutes}m ${secs}s`;

  if (minutes > 0)
    return `${minutes}m ${secs}s`;

  return `${secs}s`;
}

const COUNT_DEFINITIONS = [
  { key: 'files',       label: 'Files',       format: formatNumber },
  { key: 'directories', label: 'Directories', format: formatNumber },
  { key: 'symlinks',    label: 'Symlinks',    format: formatNumber },
  { key: 'chunks',      label: 'Chunks',      format: formatNumber },
  { key: 'snapshots',   label: 'Snapshots',   format: formatNumber },
  { key: 'forks',       label: 'Forks',       format: formatNumber },
];

const SIZE_DEFINITIONS = [
  { key: 'disk_total',    label: 'Disk Total',    format: formatBytes },
  { key: 'logical_data',  label: 'Logical Data',  format: formatBytes },
  { key: 'chunk_data',    label: 'Chunk Data',    format: formatBytes },
  { key: 'dedup_savings', label: 'Dedup Savings', format: formatBytes },
  { key: 'void_space',    label: 'Void Space',    format: formatBytes },
];

const DARK_THEME = {
  title:   { color: '#e6edf3' },
  grid:    { color: '#30363d' },
  legend:  { color: '#8b949e' },
  preview: { maskColor: 'rgba(22, 27, 34, 0.8)' },
  xAxis:   { textColor: '#8b949e' },
  yAxis:   { textColor: '#8b949e' },
};

const CHART_COLORS = ['#f0883e', '#3fb950', '#d2a8ff', '#58a6ff'];

class AeorDashboard extends HTMLElement {
  constructor() {
    super();
    this._interval        = null;
    this._eventSource     = null;
    this._activityHistory = [];
    this._storageChart    = null;
    this._activityChart   = null;
    this._stats           = null;
  }

  connectedCallback() {
    this.render();
    this.fetchStats(); // initial load
    this.connectSSE();
  }

  disconnectedCallback() {
    if (this._eventSource) {
      this._eventSource.close();
      this._eventSource = null;
    }
    if (this._interval) {
      clearInterval(this._interval);
      this._interval = null;
    }
  }

  connectSSE() {
    // Build SSE URL — subscribe to metrics events
    let url = '/events/stream?events=metrics';

    // EventSource doesn't support Authorization headers natively.
    // For --auth=false mode, no token is needed. For auth mode,
    // we'd need a polyfill or query-param token. For now, direct connect.
    try {
      this._eventSource = new EventSource(url);

      this._eventSource.addEventListener('metrics', (event) => {
        try {
          const data = JSON.parse(event.data);
          this._stats = data;
          this.updateIdentityBar(data.identity);
          this.updateStatCards(data);
          this.updateThroughput(data.throughput);
          this.updateHealthIndicators(data.health);
          this.updateStorageChart(data);
          this.recordActivityPoint(data);
          this.updateActivityChart();

          const errorContainer = this.querySelector('#dashboard-error');
          if (errorContainer)
            errorContainer.innerHTML = '';
        } catch (_) {
          // malformed event, skip
        }
      });

      this._eventSource.onerror = () => {
        // SSE failed — fall back to polling
        if (this._eventSource) {
          this._eventSource.close();
          this._eventSource = null;
        }
        if (!this._interval) {
          this._interval = setInterval(() => this.fetchStats(), 15000);
        }
      };
    } catch (_) {
      // EventSource not supported — fall back to polling
      this._interval = setInterval(() => this.fetchStats(), 15000);
    }
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Dashboard</h1>
      </div>
      <div id="identity-bar" style="
        background: var(--card);
        border: 1px solid var(--border);
        border-radius: 8px;
        padding: 12px 18px;
        margin-bottom: 18px;
        display: flex;
        flex-wrap: wrap;
        gap: 24px;
        font-size: 0.85rem;
      ">
        <div><span style="color:var(--text-muted);">Version</span> <span id="identity-version" style="color:var(--text);font-family:var(--font-mono);margin-left:6px;">&mdash;</span></div>
        <div><span style="color:var(--text-muted);">Database</span> <span id="identity-database-path" style="color:var(--text);font-family:var(--font-mono);margin-left:6px;">&mdash;</span></div>
        <div><span style="color:var(--text-muted);">Uptime</span> <span id="identity-uptime" style="color:var(--text);font-family:var(--font-mono);margin-left:6px;">&mdash;</span></div>
        <div><span style="color:var(--text-muted);">Hash</span> <span id="identity-hash-algorithm" style="color:var(--text);font-family:var(--font-mono);margin-left:6px;">&mdash;</span></div>
      </div>
      <div id="dashboard-error"></div>
      <div style="font-size:0.75rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px;">Counts</div>
      <div class="stats-grid" id="stats-counts">
        ${COUNT_DEFINITIONS.map((definition) => `
          <div class="stat-card">
            <div class="stat-label">${definition.label}</div>
            <div class="stat-value" id="stat-count-${definition.key}">&mdash;</div>
          </div>
        `).join('')}
      </div>
      <div style="font-size:0.75rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px;">Sizes</div>
      <div class="stats-grid" id="stats-sizes">
        ${SIZE_DEFINITIONS.map((definition) => `
          <div class="stat-card">
            <div class="stat-label">${definition.label}</div>
            <div class="stat-value" id="stat-size-${definition.key}">&mdash;</div>
          </div>
        `).join('')}
      </div>
      <div style="font-size:0.75rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px;">Throughput</div>
      <div class="stats-grid" id="stats-throughput">
        <div class="stat-card">
          <div class="stat-label">Writes / sec (1m)</div>
          <div class="stat-value" id="stat-writes-per-sec">&mdash;</div>
        </div>
        <div class="stat-card">
          <div class="stat-label">Reads / sec (1m)</div>
          <div class="stat-value" id="stat-reads-per-sec">&mdash;</div>
        </div>
      </div>
      <div style="font-size:0.75rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.5px;margin-bottom:8px;">Health</div>
      <div class="stats-grid" id="stats-health">
        <div class="stat-card">
          <div class="stat-label">Disk Usage</div>
          <div id="health-disk-usage" style="margin-top:8px;">
            <div style="display:flex;justify-content:space-between;font-size:0.8rem;margin-bottom:4px;">
              <span style="color:var(--text-muted);">Usage</span>
              <span style="color:var(--text);font-family:var(--font-mono);" id="health-disk-usage-value">&mdash;</span>
            </div>
            <div style="background:#161b22;border-radius:4px;height:20px;overflow:hidden;">
              <div id="health-disk-usage-bar" style="background:var(--success);height:100%;width:0%;border-radius:4px;transition:width 0.4s ease,background 0.4s ease;"></div>
            </div>
          </div>
        </div>
        <div class="stat-card">
          <div class="stat-label">Dedup Hit Rate</div>
          <div class="stat-value" id="health-dedup-hit-rate">&mdash;</div>
        </div>
        <div class="stat-card">
          <div class="stat-label">Write Buffer Depth</div>
          <div class="stat-value" id="health-write-buffer-depth">&mdash;</div>
        </div>
      </div>
      <div class="charts-row">
        <div class="chart-card">
          <div class="chart-title">Storage Overview</div>
          <div class="chart-container" id="chart-storage"></div>
        </div>
        <div class="chart-card">
          <div class="chart-title">Activity (writes/sec)</div>
          <div class="chart-container" id="chart-activity"></div>
        </div>
      </div>
    `;
  }

  async fetchStats() {
    try {
      const response = await window.api('/system/stats');

      if (!response.ok)
        throw new Error(`Stats request failed (${response.status})`);

      const data = await response.json();
      this._stats = data;

      this.updateIdentityBar(data.identity);
      this.updateStatCards(data);
      this.updateThroughput(data.throughput);
      this.updateHealthIndicators(data.health);
      this.updateStorageChart(data);
      this.recordActivityPoint(data);
      this.updateActivityChart();

      const errorContainer = this.querySelector('#dashboard-error');
      if (errorContainer)
        errorContainer.innerHTML = '';
    } catch (error) {
      const errorContainer = this.querySelector('#dashboard-error');
      if (errorContainer)
        errorContainer.innerHTML = `<div class="alert alert-error">Failed to load stats: ${escapeHtml(error.message)}</div>`;
    }
  }

  updateIdentityBar(identity) {
    if (!identity)
      return;

    const versionElement      = this.querySelector('#identity-version');
    const databasePathElement = this.querySelector('#identity-database-path');
    const uptimeElement       = this.querySelector('#identity-uptime');
    const hashAlgorithmElement = this.querySelector('#identity-hash-algorithm');

    if (versionElement)
      versionElement.textContent = identity.version || '\u2014';

    if (databasePathElement)
      databasePathElement.textContent = identity.database_path || '\u2014';

    if (uptimeElement)
      uptimeElement.textContent = formatUptime(identity.uptime_seconds);

    if (hashAlgorithmElement)
      hashAlgorithmElement.textContent = identity.hash_algorithm || '\u2014';
  }

  updateStatCards(data) {
    const counts = data.counts || {};
    const sizes  = data.sizes || {};

    for (const definition of COUNT_DEFINITIONS) {
      const element = this.querySelector(`#stat-count-${definition.key}`);
      if (!element)
        continue;

      const value = counts[definition.key];
      element.textContent = (value != null) ? definition.format(value) : '\u2014';
    }

    for (const definition of SIZE_DEFINITIONS) {
      const element = this.querySelector(`#stat-size-${definition.key}`);
      if (!element)
        continue;

      const value = sizes[definition.key];
      element.textContent = (value != null) ? definition.format(value) : '\u2014';
    }
  }

  updateThroughput(throughput) {
    if (!throughput)
      return;

    const writesElement = this.querySelector('#stat-writes-per-sec');
    const readsElement  = this.querySelector('#stat-reads-per-sec');

    if (writesElement) {
      const rate = throughput.writes_per_sec?.['1m'];
      writesElement.textContent = formatRate(rate);
    }

    if (readsElement) {
      const rate = throughput.reads_per_sec?.['1m'];
      readsElement.textContent = formatRate(rate);
    }
  }

  updateHealthIndicators(health) {
    if (!health)
      return;

    // Disk usage percentage bar
    const diskUsageValue = this.querySelector('#health-disk-usage-value');
    const diskUsageBar   = this.querySelector('#health-disk-usage-bar');

    if (diskUsageValue && diskUsageBar) {
      const percent = health.disk_usage_percent;
      diskUsageValue.textContent = formatPercent(percent);

      if (percent != null) {
        diskUsageBar.style.width = Math.min(percent, 100) + '%';

        // Color based on usage level
        if (percent >= 90) {
          diskUsageBar.style.background = 'var(--danger)';
        } else if (percent >= 75) {
          diskUsageBar.style.background = 'var(--accent)';
        } else {
          diskUsageBar.style.background = 'var(--success)';
        }
      }
    }

    // Dedup hit rate
    const dedupElement = this.querySelector('#health-dedup-hit-rate');
    if (dedupElement)
      dedupElement.textContent = formatPercent(health.dedup_hit_rate);

    // Write buffer depth
    const bufferElement = this.querySelector('#health-write-buffer-depth');
    if (bufferElement)
      bufferElement.textContent = (health.write_buffer_depth != null) ? formatNumber(health.write_buffer_depth) : '\u2014';
  }

  updateStorageChart(data) {
    const container = this.querySelector('#chart-storage');
    if (!container)
      return;

    const counts = data.counts || {};
    const labels = ['Chunks', 'Files', 'Directories', 'Snapshots'];
    const values = [
      counts.chunks || 0,
      counts.files || 0,
      counts.directories || 0,
      counts.snapshots || 0,
    ];

    container.innerHTML = '';
    this.renderBarChart(container, labels, values);
  }

  renderBarChart(container, labels, values) {
    const maxValue = Math.max(...values, 1);
    const barHeight = 32;
    const gap = 10;

    let html = '<div style="padding:8px 0;">';

    for (let index = 0; index < labels.length; index++) {
      const percentage = (values[index] / maxValue) * 100;
      const color = CHART_COLORS[index % CHART_COLORS.length];

      html += `
        <div style="margin-bottom:${gap}px;">
          <div style="display:flex;justify-content:space-between;font-size:0.8rem;margin-bottom:4px;">
            <span style="color:#8b949e;">${labels[index]}</span>
            <span style="color:#e6edf3;font-family:var(--font-mono);">${formatNumber(values[index])}</span>
          </div>
          <div style="background:#161b22;border-radius:4px;height:${barHeight}px;overflow:hidden;">
            <div style="background:${color};height:100%;width:${Math.max(percentage, 1)}%;border-radius:4px;transition:width 0.4s ease;"></div>
          </div>
        </div>
      `;
    }

    html += '</div>';
    container.innerHTML = html;
  }

  recordActivityPoint(data) {
    const writesPerSecond = data.throughput?.writes_per_sec?.['1m'] || 0;

    this._activityHistory.push({
      timestamp:      Date.now(),
      writesPerSecond: writesPerSecond,
    });

    // Keep rolling window of 60 data points (15 minutes at 15s metrics intervals)
    if (this._activityHistory.length > 60)
      this._activityHistory.shift();
  }

  updateActivityChart() {
    const container = this.querySelector('#chart-activity');
    if (!container)
      return;

    const history = this._activityHistory;

    if (history.length < 2) {
      container.innerHTML = '<div style="color:#8b949e;font-size:0.85rem;padding:20px;text-align:center;">Collecting data...</div>';
      return;
    }

    this.renderLineChart(container, history);
  }

  renderLineChart(container, history) {
    const width = container.clientWidth || 400;
    const height = 220;
    const paddingLeft = 60;
    const paddingRight = 16;
    const paddingTop = 16;
    const paddingBottom = 30;

    const chartWidth = width - paddingLeft - paddingRight;
    const chartHeight = height - paddingTop - paddingBottom;

    const values = history.map((point) => point.writesPerSecond);
    const minValue = Math.min(...values);
    const maxValue = Math.max(...values);
    const range = maxValue - minValue || 1;

    const points = history.map((point, index) => {
      const x = paddingLeft + (index / (history.length - 1)) * chartWidth;
      const y = paddingTop + chartHeight - ((point.writesPerSecond - minValue) / range) * chartHeight;
      return `${x},${y}`;
    });

    const polyline = points.join(' ');

    // Area fill path
    const firstX = paddingLeft;
    const lastX = paddingLeft + chartWidth;
    const bottomY = paddingTop + chartHeight;
    const areaPath = `M${firstX},${bottomY} L${points.map((point) => `L${point}`).join(' ')} L${lastX},${bottomY} Z`;

    // Y-axis labels
    const yLabelCount = 4;
    let yLabels = '';
    for (let index = 0; index <= yLabelCount; index++) {
      const value = minValue + (range * index / yLabelCount);
      const y = paddingTop + chartHeight - (index / yLabelCount) * chartHeight;
      yLabels += `<text x="${paddingLeft - 8}" y="${y + 4}" text-anchor="end" fill="#8b949e" font-size="11" font-family="var(--font-mono)">${formatRate(value)}</text>`;
      yLabels += `<line x1="${paddingLeft}" y1="${y}" x2="${width - paddingRight}" y2="${y}" stroke="#30363d" stroke-width="1"/>`;
    }

    // X-axis time labels
    const timeLabels = [];
    const labelCount = Math.min(5, history.length);
    for (let index = 0; index < labelCount; index++) {
      const dataIndex = Math.floor(index * (history.length - 1) / (labelCount - 1));
      const x = paddingLeft + (dataIndex / (history.length - 1)) * chartWidth;
      const time = new Date(history[dataIndex].timestamp);
      const label = `${time.getHours().toString().padStart(2, '0')}:${time.getMinutes().toString().padStart(2, '0')}:${time.getSeconds().toString().padStart(2, '0')}`;
      timeLabels.push(`<text x="${x}" y="${height - 4}" text-anchor="middle" fill="#8b949e" font-size="10" font-family="var(--font-mono)">${label}</text>`);
    }

    container.innerHTML = `
      <svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">
        ${yLabels}
        ${timeLabels.join('')}
        <path d="${areaPath}" fill="rgba(240, 136, 62, 0.1)"/>
        <polyline points="${polyline}" fill="none" stroke="#f0883e" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>
      </svg>
    `;
  }
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

customElements.define('aeor-dashboard', AeorDashboard);
