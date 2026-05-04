'use strict';

import { escapeHtml } from '/shared/utils.js';

class AeorGroups extends HTMLElement {
  constructor() {
    super();
    this._groups = [];
    this._error = null;
    this._forbidden = false;
  }

  connectedCallback() {
    this.render();
    this.fetchGroups();
  }

  /** Called by navigate() when this page becomes visible. */
  onPageShow() {
    this.fetchGroups();
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Groups</h1>
        <button class="button button-primary" id="create-group-button">Create Group</button>
      </div>
      <div id="groups-error"></div>
      <div id="groups-content"></div>
      <div id="groups-modal"></div>
    `;

    this.querySelector('#create-group-button').addEventListener('click', () => this.showCreateModal());
  }

  async fetchGroups() {
    try {
      const response = await window.api('/system/groups');

      if (response.status === 403) {
        this._forbidden = true;
        this.renderContent();
        return;
      }

      if (!response.ok)
        throw new Error(`Failed to fetch groups (${response.status})`);

      const data = await response.json();
      this._groups = data.items || data;
      this._error = null;
      this._forbidden = false;

      // Resolve user:UUID group names to usernames
      await this._resolveUserGroupNames();

      this.renderContent();
    } catch (error) {
      this._error = error.message;
      this.renderContent();
    }
  }

  async _resolveUserGroupNames() {
    const userGroups = this._groups.filter((g) => g.name && g.name.startsWith('user:'));
    if (userGroups.length === 0) return;

    try {
      const resp = await window.api('/auth/keys/users');
      if (!resp.ok) return;
      const data = await resp.json();
      const users = data.items || [];
      const userMap = {};
      for (const u of users) userMap[String(u.user_id)] = u.username;

      for (const g of this._groups) {
        if (g.name && g.name.startsWith('user:')) {
          const uid = g.name.slice(5);
          if (userMap[uid]) g._displayName = userMap[uid] + ' (auto)';
        }
      }
    } catch (_) { /* best-effort */ }
  }

  renderContent() {
    const contentContainer = this.querySelector('#groups-content');
    const errorContainer = this.querySelector('#groups-error');

    if (!contentContainer || !errorContainer)
      return;

    if (this._error) {
      errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(this._error)}</div>`;
    } else {
      errorContainer.innerHTML = '';
    }

    if (this._forbidden) {
      contentContainer.innerHTML = `
        <div class="card empty-state">
          <div class="empty-state-message-lg">You don't have permission to manage groups.</div>
        </div>
      `;
      return;
    }

    if (this._groups.length === 0 && !this._error) {
      contentContainer.innerHTML = `
        <div class="card empty-state">
          <div class="empty-state-message">No groups found. Create one to get started.</div>
        </div>
      `;
      return;
    }

    contentContainer.innerHTML = `
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Query Field</th>
              <th>Query Operator</th>
              <th>Query Value</th>
              <th>Created</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            ${this._groups.map((group) => `
              <tr>
                <td><strong>${escapeHtml(group._displayName || group.name || '')}</strong></td>
                <td class="cell-mono">${escapeHtml(group.query_field || '')}</td>
                <td class="cell-mono">${escapeHtml(group.query_operator || '')}</td>
                <td class="cell-mono">${escapeHtml(group.query_value || '')}</td>
                <td class="cell-mono">
                  ${(group.created_at) ? new Date(group.created_at).toLocaleDateString() : '\u2014'}
                </td>
                <td>
                  <button class="button button-small button-danger delete-group-button" data-group-name="${escapeHtml(group.name || '')}">Delete</button>
                </td>
              </tr>
            `).join('')}
          </tbody>
        </table>
      </div>
    `;

    contentContainer.querySelectorAll('.delete-group-button').forEach((button) => {
      button.addEventListener('click', () => {
        this.deleteGroup(button.dataset.groupName);
      });
    });
  }

  showCreateModal() {
    const modalContainer = this.querySelector('#groups-modal');
    if (!modalContainer)
      return;

    modalContainer.innerHTML = `
      <div class="modal-overlay" id="modal-overlay">
        <div class="modal-content">
          <div class="modal-title">Create Group</div>
          <div id="modal-error"></div>
          <form id="create-group-form">
            <div class="form-group">
              <label class="form-label" for="create-name">Group Name</label>
              <input class="form-input" id="create-name" type="text" required>
            </div>
            <div class="form-group">
              <label class="form-label">Default Allow</label>
              <aeor-crudlify id="create-default-allow" value="-r------"></aeor-crudlify>
            </div>
            <div class="form-group">
              <label class="form-label">Default Deny</label>
              <aeor-crudlify id="create-default-deny" value="--------"></aeor-crudlify>
            </div>
            <div class="form-group">
              <label class="form-label" for="create-query-field">Query Field</label>
              <select class="form-input" id="create-query-field">
                <option value="tags" selected>tags</option>
                <option value="user_id">user_id</option>
                <option value="is_active">is_active</option>
                <option value="created_at">created_at</option>
                <option value="updated_at">updated_at</option>
              </select>
            </div>
            <div class="form-group">
              <label class="form-label" for="create-query-operator">Query Operator</label>
              <select class="form-input" id="create-query-operator">
                <option value="has" selected>has — user has this tag</option>
                <option value="has_any">has_any — user has at least one of these tags</option>
                <option value="has_all">has_all — user has all of these tags</option>
                <option value="eq">eq — equals</option>
                <option value="neq">neq — not equals</option>
                <option value="contains">contains</option>
                <option value="starts_with">starts_with</option>
                <option value="in">in — comma-separated list</option>
                <option value="lt">lt — less than</option>
                <option value="gt">gt — greater than</option>
              </select>
            </div>
            <div class="form-group">
              <label class="form-label" for="create-query-value">Query Value</label>
              <input class="form-input" id="create-query-value" type="text" placeholder="e.g. engineering">
            </div>
            <div class="modal-actions">
              <button class="button" type="button" id="cancel-create-button">Cancel</button>
              <button class="button button-primary" type="submit">Create</button>
            </div>
          </form>
        </div>
      </div>
    `;

    modalContainer.querySelector('#cancel-create-button').addEventListener('click', () => this.closeModal());
    modalContainer.querySelector('#modal-overlay').addEventListener('click', (event) => {
      if (event.target.id === 'modal-overlay')
        this.closeModal();
    });

    modalContainer.querySelector('#create-group-form').addEventListener('submit', (event) => this.handleCreate(event));
  }

  async handleCreate(event) {
    event.preventDefault();
    const modalError = this.querySelector('#modal-error');

    try {
      const response = await window.api('/system/groups', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name:           this.querySelector('#create-name').value,
          default_allow:  this.querySelector('#create-default-allow').value || '--------',
          default_deny:   this.querySelector('#create-default-deny').value || '--------',
          query_field:    this.querySelector('#create-query-field').value,
          query_operator: this.querySelector('#create-query-operator').value,
          query_value:    this.querySelector('#create-query-value').value,
        }),
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Create failed (${response.status})`);
      }

      this.closeModal();
      this.fetchGroups();
    } catch (error) {
      if (modalError)
        modalError.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
    }
  }

  async deleteGroup(name) {
    if (!confirm(`Are you sure you want to delete the group "${name}"?`))
      return;

    try {
      const response = await window.api(`/system/groups/${name}`, {
        method: 'DELETE',
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Delete failed (${response.status})`);
      }

      this.fetchGroups();
    } catch (error) {
      const errorContainer = this.querySelector('#groups-error');
      if (errorContainer)
        errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
    }
  }

  closeModal() {
    const modalContainer = this.querySelector('#groups-modal');
    if (modalContainer)
      modalContainer.innerHTML = '';
  }
}

customElements.define('aeor-groups', AeorGroups);
