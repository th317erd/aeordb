'use strict';

import { escapeHtml } from '/system/portal/shared/utils.js';

class AeorSettings extends HTMLElement {
  constructor() {
    super();
    this._config = null;
    this._error = null;
    this._forbidden = false;
    this._provider = 'smtp';
    this._feedback = null;
    this._activeTab = 'email';
    this._gcSchedule = null;
  }

  connectedCallback() {
    this.render();
    this._fetchAll();
  }

  onPageShow() {
    this._fetchAll();
  }

  async _fetchAll() {
    await Promise.all([this.fetchConfig(), this.fetchGcSchedule()]);
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Settings</h1>
      </div>
      <div id="settings-error"></div>
      <div id="settings-feedback"></div>
      <div class="tab-bar" style="margin-bottom:20px;">
        <div class="tab active settings-tab" data-tab="email">Email</div>
        <div class="tab settings-tab" data-tab="gc">Garbage Collector</div>
      </div>
      <div id="settings-tab-email"></div>
      <div id="settings-tab-gc" style="display:none;"></div>
    `;

    this.querySelectorAll('.settings-tab').forEach((btn) => {
      btn.addEventListener('click', () => {
        this.querySelectorAll('.settings-tab').forEach((b) => b.classList.remove('active'));
        btn.classList.add('active');
        this._activeTab = btn.dataset.tab;
        this.querySelector('#settings-tab-email').style.display = this._activeTab === 'email' ? '' : 'none';
        this.querySelector('#settings-tab-gc').style.display = this._activeTab === 'gc' ? '' : 'none';
      });
    });
  }

  // ---------------------------------------------------------------------------
  // Email tab
  // ---------------------------------------------------------------------------

  async fetchConfig() {
    try {
      const response = await window.api('/system/email-config');

      if (response.status === 403) {
        this._forbidden = true;
        this._config = null;
        this._error = null;
        this.renderEmailTab();
        return;
      }

      if (response.status === 404) {
        this._config = null;
        this._error = null;
        this._forbidden = false;
        this.renderEmailTab();
        return;
      }

      if (!response.ok)
        throw new Error(`Failed to fetch email config (${response.status})`);

      const data = await response.json();
      this._config = data;
      this._error = null;
      this._forbidden = false;

      if (data.oauth_service || data.client_id) {
        this._provider = 'oauth';
      } else {
        this._provider = 'smtp';
      }

      this.renderEmailTab();
    } catch (error) {
      this._error = error.message;
      this.renderEmailTab();
    }
  }

  renderEmailTab() {
    const container = this.querySelector('#settings-tab-email');
    const errorContainer = this.querySelector('#settings-error');
    if (!container) return;

    if (this._error && errorContainer) {
      errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(this._error)}</div>`;
    } else if (errorContainer) {
      errorContainer.innerHTML = '';
    }

    if (this._forbidden) {
      container.innerHTML = `
        <div class="card" style="text-align:center;padding:40px;">
          <div style="color:#8b949e;font-size:1rem;">You don't have permission to manage settings.</div>
        </div>
      `;
      return;
    }

    const cfg = this._config || {};
    const provider = this._provider;

    container.innerHTML = `
      <div class="card">
        <h2 style="font-size:1.1rem;font-weight:700;margin-bottom:18px;">Email Configuration</h2>
        <form id="email-config-form">
          <div class="form-group">
            <label class="form-label" for="cfg-provider">Provider</label>
            <select class="form-input" id="cfg-provider">
              <option value="smtp" ${provider === 'smtp' ? 'selected' : ''}>SMTP</option>
              <option value="oauth" ${provider === 'oauth' ? 'selected' : ''}>OAuth</option>
            </select>
          </div>

          <div id="smtp-fields" style="display:${provider === 'smtp' ? 'block' : 'none'};">
            <div class="form-group">
              <label class="form-label" for="cfg-host">Host</label>
              <input class="form-input" id="cfg-host" type="text" value="${escapeHtml(cfg.host || '')}" placeholder="smtp.example.com">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-port">Port</label>
              <input class="form-input" id="cfg-port" type="number" value="${escapeHtml(String(cfg.port || ''))}" placeholder="587">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-username">Username</label>
              <input class="form-input" id="cfg-username" type="text" value="${escapeHtml(cfg.username || '')}" placeholder="user@example.com">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-password">Password</label>
              <input class="form-input" id="cfg-password" type="password" value="${escapeHtml(cfg.password || '')}" placeholder="Enter password">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-smtp-from-address">From Address</label>
              <input class="form-input" id="cfg-smtp-from-address" type="email" value="${escapeHtml(cfg.from_address || '')}" placeholder="noreply@example.com">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-smtp-from-name">From Name</label>
              <input class="form-input" id="cfg-smtp-from-name" type="text" value="${escapeHtml(cfg.from_name || '')}" placeholder="AeorDB">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-tls-mode">TLS Mode</label>
              <select class="form-input" id="cfg-tls-mode">
                <option value="starttls" ${cfg.tls_mode === 'starttls' ? 'selected' : ''}>STARTTLS</option>
                <option value="tls" ${cfg.tls_mode === 'tls' ? 'selected' : ''}>TLS</option>
                <option value="none" ${cfg.tls_mode === 'none' ? 'selected' : ''}>None</option>
              </select>
            </div>
          </div>

          <div id="oauth-fields" style="display:${provider === 'oauth' ? 'block' : 'none'};">
            <div class="form-group">
              <label class="form-label" for="cfg-oauth-service">OAuth Service</label>
              <select class="form-input" id="cfg-oauth-service">
                <option value="gmail" ${cfg.oauth_service === 'gmail' ? 'selected' : ''}>Gmail</option>
                <option value="outlook" ${cfg.oauth_service === 'outlook' ? 'selected' : ''}>Outlook</option>
                <option value="custom" ${cfg.oauth_service === 'custom' ? 'selected' : ''}>Custom</option>
              </select>
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-client-id">Client ID</label>
              <input class="form-input" id="cfg-client-id" type="text" value="${escapeHtml(cfg.client_id || '')}" placeholder="OAuth Client ID">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-client-secret">Client Secret</label>
              <input class="form-input" id="cfg-client-secret" type="password" value="${escapeHtml(cfg.client_secret || '')}" placeholder="OAuth Client Secret">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-refresh-token">Refresh Token</label>
              <input class="form-input" id="cfg-refresh-token" type="password" value="${escapeHtml(cfg.refresh_token || '')}" placeholder="OAuth Refresh Token">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-oauth-from-address">From Address</label>
              <input class="form-input" id="cfg-oauth-from-address" type="email" value="${escapeHtml(cfg.from_address || '')}" placeholder="noreply@example.com">
            </div>
            <div class="form-group">
              <label class="form-label" for="cfg-oauth-from-name">From Name</label>
              <input class="form-input" id="cfg-oauth-from-name" type="text" value="${escapeHtml(cfg.from_name || '')}" placeholder="AeorDB">
            </div>
          </div>

          <div style="display:flex;gap:10px;margin-top:18px;">
            <button class="button button-primary" type="submit">Save</button>
            <button class="button" type="button" id="test-email-button">Send Test Email</button>
          </div>
        </form>
      </div>
    `;

    this.querySelector('#cfg-provider').addEventListener('change', (event) => {
      this._provider = event.target.value;
      const smtpFields = this.querySelector('#smtp-fields');
      const oauthFields = this.querySelector('#oauth-fields');
      if (smtpFields) smtpFields.style.display = (this._provider === 'smtp') ? 'block' : 'none';
      if (oauthFields) oauthFields.style.display = (this._provider === 'oauth') ? 'block' : 'none';
    });

    this.querySelector('#email-config-form').addEventListener('submit', (event) => this.handleSave(event));
    this.querySelector('#test-email-button').addEventListener('click', () => this.handleTestEmail());
  }

  _buildPayload() {
    if (this._provider === 'smtp') {
      return {
        provider: 'smtp',
        host: this.querySelector('#cfg-host').value,
        port: parseInt(this.querySelector('#cfg-port').value, 10) || null,
        username: this.querySelector('#cfg-username').value,
        password: this.querySelector('#cfg-password').value,
        from_address: this.querySelector('#cfg-smtp-from-address').value,
        from_name: this.querySelector('#cfg-smtp-from-name').value,
        tls_mode: this.querySelector('#cfg-tls-mode').value,
      };
    } else {
      return {
        provider: 'oauth',
        oauth_service: this.querySelector('#cfg-oauth-service').value,
        client_id: this.querySelector('#cfg-client-id').value,
        client_secret: this.querySelector('#cfg-client-secret').value,
        refresh_token: this.querySelector('#cfg-refresh-token').value,
        from_address: this.querySelector('#cfg-oauth-from-address').value,
        from_name: this.querySelector('#cfg-oauth-from-name').value,
      };
    }
  }

  _showFeedback(message, isError) {
    const feedbackContainer = this.querySelector('#settings-feedback');
    if (!feedbackContainer) return;
    const cls = isError ? 'alert-error' : 'alert-info';
    feedbackContainer.innerHTML = `<div class="alert ${cls}">${escapeHtml(message)}</div>`;
    setTimeout(() => {
      if (feedbackContainer) feedbackContainer.innerHTML = '';
    }, 5000);
  }

  async handleSave(event) {
    event.preventDefault();
    try {
      const payload = this._buildPayload();
      const response = await window.api('/system/email-config', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(payload),
      });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Save failed (${response.status})`);
      }
      this._showFeedback('Email configuration saved successfully.', false);
      this.fetchConfig();
    } catch (error) {
      this._showFeedback(error.message, true);
    }
  }

  async handleTestEmail() {
    const recipient = window.prompt('Enter recipient email address for test:');
    if (!recipient) return;
    try {
      const response = await window.api('/system/email-test', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ to: recipient }),
      });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Test email failed (${response.status})`);
      }
      this._showFeedback(`Test email sent to ${recipient}.`, false);
    } catch (error) {
      this._showFeedback(error.message, true);
    }
  }

  // ---------------------------------------------------------------------------
  // Garbage Collector tab
  // ---------------------------------------------------------------------------

  async fetchGcSchedule() {
    try {
      const response = await window.api('/system/cron');
      if (!response.ok) return;
      const data = await response.json();
      const schedules = data.items || data;
      this._gcSchedule = schedules.find((s) => s.task_type === 'gc') || null;
    } catch (e) {
      this._gcSchedule = null;
    }
    this.renderGcTab();
  }

  renderGcTab() {
    const container = this.querySelector('#settings-tab-gc');
    if (!container) return;

    if (this._forbidden) {
      container.innerHTML = `
        <div class="card" style="text-align:center;padding:40px;">
          <div style="color:#8b949e;">You don't have permission to manage settings.</div>
        </div>
      `;
      return;
    }

    const gc = this._gcSchedule;
    const isEnabled = gc ? gc.enabled : false;
    const schedule = gc ? gc.schedule : '0 3 * * *';

    container.innerHTML = `
      <div class="card" style="margin-bottom:20px;">
        <div style="color:var(--text-muted);font-size:0.9rem;line-height:1.6;margin-bottom:20px;">
          The <strong style="color:var(--text);">Garbage Collector</strong> reclaims disk space
          by removing unreachable data — orphaned file chunks from interrupted uploads,
          deleted files, and old versions that are no longer referenced. It runs safely
          in the background without affecting reads or writes. A snapshot is automatically
          created before each run as a safety net.
        </div>
      </div>

      <div class="card">
        <h2 style="font-size:1.1rem;font-weight:700;margin-bottom:18px;">Schedule</h2>

        <div class="form-group">
          <label class="form-label" style="display:flex;align-items:center;gap:8px;cursor:pointer;">
            <input type="checkbox" id="gc-enabled" ${isEnabled ? 'checked' : ''}>
            Run garbage collection on a schedule
          </label>
        </div>

        <div id="gc-schedule-fields" style="display:${isEnabled ? 'block' : 'none'};">
          <div class="form-group">
            <label class="form-label">Run every</label>
            <select class="form-input" id="gc-frequency">
              <option value="0 3 * * *" ${schedule === '0 3 * * *' ? 'selected' : ''}>Day (3:00 AM)</option>
              <option value="0 3 * * 0" ${schedule === '0 3 * * 0' ? 'selected' : ''}>Week (Sunday 3:00 AM)</option>
              <option value="0 3 1 * *" ${schedule === '0 3 1 * *' ? 'selected' : ''}>Month (1st, 3:00 AM)</option>
              <option value="custom" ${!['0 3 * * *', '0 3 * * 0', '0 3 1 * *'].includes(schedule) ? 'selected' : ''}>Custom cron expression</option>
            </select>
          </div>

          <div id="gc-custom-cron" style="display:${!['0 3 * * *', '0 3 * * 0', '0 3 1 * *'].includes(schedule) ? 'block' : 'none'};">
            <div class="form-group">
              <label class="form-label" for="gc-cron-expr">Cron Expression</label>
              <input class="form-input" id="gc-cron-expr" type="text" value="${escapeHtml(schedule)}" placeholder="0 3 * * *">
              <div style="font-size:0.75rem;color:var(--text-muted);margin-top:4px;">
                Format: minute hour day-of-month month day-of-week
              </div>
            </div>
          </div>
        </div>

        <div style="display:flex;gap:10px;margin-top:18px;">
          <button class="button button-primary" id="gc-save-button">Save</button>
          <button class="button" id="gc-run-now-button">Run Now</button>
        </div>
      </div>
    `;

    // Toggle schedule fields
    this.querySelector('#gc-enabled').addEventListener('change', (e) => {
      this.querySelector('#gc-schedule-fields').style.display = e.target.checked ? 'block' : 'none';
    });

    // Toggle custom cron input
    this.querySelector('#gc-frequency').addEventListener('change', (e) => {
      this.querySelector('#gc-custom-cron').style.display = e.target.value === 'custom' ? 'block' : 'none';
    });

    // Save
    this.querySelector('#gc-save-button').addEventListener('click', () => this.handleGcSave());

    // Run Now
    this.querySelector('#gc-run-now-button').addEventListener('click', () => this.handleGcRunNow());
  }

  async handleGcSave() {
    const enabled = this.querySelector('#gc-enabled').checked;
    const freqSelect = this.querySelector('#gc-frequency');
    const schedule = freqSelect.value === 'custom'
      ? this.querySelector('#gc-cron-expr').value
      : freqSelect.value;

    try {
      if (this._gcSchedule) {
        // Update existing
        const response = await window.api(`/system/cron/${this._gcSchedule.id}`, {
          method: 'PATCH',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ enabled, schedule }),
        });
        if (!response.ok) {
          const text = await response.text();
          throw new Error(text || `Update failed (${response.status})`);
        }
      } else if (enabled) {
        // Create new
        const response = await window.api('/system/cron', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            id: 'gc-scheduled',
            task_type: 'gc',
            schedule,
            args: {},
            enabled: true,
          }),
        });
        if (!response.ok) {
          const text = await response.text();
          throw new Error(text || `Create failed (${response.status})`);
        }
      }
      this._showFeedback('Garbage collection schedule saved.', false);
      this.fetchGcSchedule();
    } catch (error) {
      this._showFeedback(error.message, true);
    }
  }

  async handleGcRunNow() {
    try {
      const response = await window.api('/system/tasks/gc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({}),
      });
      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `GC failed (${response.status})`);
      }
      this._showFeedback('Garbage collection started. Check the Tasks page for progress.', false);
    } catch (error) {
      this._showFeedback(error.message, true);
    }
  }
}

customElements.define('aeor-settings', AeorSettings);
