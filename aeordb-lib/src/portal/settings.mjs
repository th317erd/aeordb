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
  }

  connectedCallback() {
    this.render();
    this.fetchConfig();
  }

  /** Called by navigate() when this page becomes visible. */
  onPageShow() {
    this.fetchConfig();
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Settings</h1>
      </div>
      <div id="settings-error"></div>
      <div id="settings-feedback"></div>
      <div id="settings-content"></div>
    `;
  }

  async fetchConfig() {
    try {
      const response = await window.api('/system/email-config');

      if (response.status === 403) {
        this._forbidden = true;
        this._config = null;
        this._error = null;
        this.renderContent();
        return;
      }

      if (response.status === 404) {
        // No config yet — show empty form
        this._config = null;
        this._error = null;
        this._forbidden = false;
        this.renderContent();
        return;
      }

      if (!response.ok)
        throw new Error(`Failed to fetch email config (${response.status})`);

      const data = await response.json();
      this._config = data;
      this._error = null;
      this._forbidden = false;

      // Detect provider from existing config
      if (data.oauth_service || data.client_id) {
        this._provider = 'oauth';
      } else {
        this._provider = 'smtp';
      }

      this.renderContent();
    } catch (error) {
      this._error = error.message;
      this.renderContent();
    }
  }

  renderContent() {
    const contentContainer = this.querySelector('#settings-content');
    const errorContainer = this.querySelector('#settings-error');

    if (!contentContainer || !errorContainer)
      return;

    if (this._error) {
      errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(this._error)}</div>`;
    } else {
      errorContainer.innerHTML = '';
    }

    if (this._forbidden) {
      contentContainer.innerHTML = `
        <div class="card" style="text-align:center;padding:40px;">
          <div style="color:#8b949e;font-size:1rem;">You don't have permission to manage settings.</div>
        </div>
      `;
      return;
    }

    const cfg = this._config || {};
    const provider = this._provider;

    contentContainer.innerHTML = `
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

    // Provider toggle
    this.querySelector('#cfg-provider').addEventListener('change', (event) => {
      this._provider = event.target.value;
      const smtpFields = this.querySelector('#smtp-fields');
      const oauthFields = this.querySelector('#oauth-fields');
      if (smtpFields) smtpFields.style.display = (this._provider === 'smtp') ? 'block' : 'none';
      if (oauthFields) oauthFields.style.display = (this._provider === 'oauth') ? 'block' : 'none';
    });

    // Save handler
    this.querySelector('#email-config-form').addEventListener('submit', (event) => this.handleSave(event));

    // Test email handler
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
    // Auto-clear after 5 seconds
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
}

customElements.define('aeor-settings', AeorSettings);
