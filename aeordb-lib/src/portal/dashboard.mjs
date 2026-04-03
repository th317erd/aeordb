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

const STAT_DEFINITIONS = [
  { key: 'entry_count',       label: 'Total Entries',   format: formatNumber },
  { key: 'file_count',        label: 'Files',           format: formatNumber },
  { key: 'chunk_count',       label: 'Chunks',          format: formatNumber },
  { key: 'directory_count',   label: 'Directories',     format: formatNumber },
  { key: 'snapshot_count',    label: 'Snapshots',       format: formatNumber },
  { key: 'fork_count',        label: 'Forks',           format: formatNumber },
  { key: 'kv_size_bytes',     label: 'KV Store Size',   format: formatBytes },
  { key: 'nvt_buckets',       label: 'NVT Buckets',     format: formatNumber },
  { key: 'db_file_size_bytes', label: 'DB Size',        format: formatBytes },
  { key: 'void_space_bytes',  label: 'Void Space',      format: formatBytes },
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
    this._interval = null;
    this._activityHistory = [];
    this._storageChart = null;
    this._activityChart = null;
    this._stats = null;
  }

  connectedCallback() {
    this.render();
    this.fetchStats();
    this._interval = setInterval(() => this.fetchStats(), 5000);
  }

  disconnectedCallback() {
    if (this._interval) {
      clearInterval(this._interval);
      this._interval = null;
    }
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Dashboard</h1>
      </div>
      <div id="dashboard-error"></div>
      <div class="stats-grid" id="stats-grid">
        ${STAT_DEFINITIONS.map((definition) => `
          <div class="stat-card">
            <div class="stat-label">${definition.label}</div>
            <div class="stat-value" id="stat-${definition.key}">&mdash;</div>
          </div>
        `).join('')}
      </div>
      <div class="charts-row">
        <div class="chart-card">
          <div class="chart-title">Storage Overview</div>
          <div class="chart-container" id="chart-storage"></div>
        </div>
        <div class="chart-card">
          <div class="chart-title">Activity</div>
          <div class="chart-container" id="chart-activity"></div>
        </div>
      </div>
    `;
  }

  async fetchStats() {
    try {
      const response = await window.api('/api/stats');

      if (!response.ok)
        throw new Error(`Stats request failed (${response.status})`);

      const stats = await response.json();
      this._stats = stats;

      this.updateStatCards(stats);
      this.updateStorageChart(stats);
      this.recordActivityPoint(stats);
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

  updateStatCards(stats) {
    for (const definition of STAT_DEFINITIONS) {
      const element = this.querySelector(`#stat-${definition.key}`);
      if (!element)
        continue;

      const value = stats[definition.key];
      element.textContent = (value != null) ? definition.format(value) : '\u2014';
    }
  }

  updateStorageChart(stats) {
    const container = this.querySelector('#chart-storage');
    if (!container || typeof window.Charty === 'undefined')
      return;

    const categories = ['chunk_count', 'file_count', 'directory_count', 'snapshot_count'];
    const labels = ['Chunks', 'Files', 'Directories', 'Snapshots'];
    const values = categories.map((key) => stats[key] || 0);

    container.innerHTML = '';

    this.renderBarChart(container, labels, values);
  }

  renderBarChart(container, labels, values) {
    const maxValue = Math.max(...values, 1);
    const barHeight = 32;
    const gap = 10;
    const totalHeight = labels.length * (barHeight + gap);

    let html = `<div style="padding:8px 0;">`;

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

  recordActivityPoint(stats) {
    this._activityHistory.push({
      timestamp: Date.now(),
      kvEntries: stats.kv_entries || stats.entry_count || 0,
    });

    // Keep rolling window of 60 data points (5 minutes at 5s intervals)
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

    const values = history.map((point) => point.kvEntries);
    const minValue = Math.min(...values);
    const maxValue = Math.max(...values);
    const range = maxValue - minValue || 1;

    const points = history.map((point, index) => {
      const x = paddingLeft + (index / (history.length - 1)) * chartWidth;
      const y = paddingTop + chartHeight - ((point.kvEntries - minValue) / range) * chartHeight;
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
      yLabels += `<text x="${paddingLeft - 8}" y="${y + 4}" text-anchor="end" fill="#8b949e" font-size="11" font-family="var(--font-mono)">${formatNumber(Math.round(value))}</text>`;
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
