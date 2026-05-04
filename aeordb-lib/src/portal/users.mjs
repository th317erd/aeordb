'use strict';

import { escapeHtml } from '/shared/utils.js';

class AeorUsers extends HTMLElement {
  constructor() {
    super();
    this._users = [];
    this._error = null;
    this._forbidden = false;
  }

  connectedCallback() {
    this.render();
    this.fetchUsers();
  }

  onPageShow() {
    this.fetchUsers();
  }

  render() {
    this.innerHTML = `
      <div class="page-header">
        <h1 class="page-title">Users</h1>
        <button class="button button-primary" id="create-user-button">Create User</button>
      </div>
      <div id="users-error"></div>
      <div id="users-content"></div>
      <div id="users-modal"></div>
    `;

    this.querySelector('#create-user-button').addEventListener('click', () => this.showCreateModal());
  }

  async fetchUsers() {
    try {
      const response = await window.api('/system/users');

      if (response.status === 403) {
        this._forbidden = true;
        this.renderContent();
        return;
      }

      if (!response.ok)
        throw new Error(`Failed to fetch users (${response.status})`);

      const data = await response.json();
      this._users = data.items || data;
      this._error = null;
      this._forbidden = false;
      this.renderContent();
    } catch (error) {
      this._error = error.message;
      this.renderContent();
    }
  }

  renderContent() {
    const contentContainer = this.querySelector('#users-content');
    const errorContainer = this.querySelector('#users-error');

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
          <div class="empty-state-message-lg">You don't have permission to manage users.</div>
        </div>
      `;
      return;
    }

    if (this._users.length === 0 && !this._error) {
      contentContainer.innerHTML = `
        <div class="card empty-state">
          <div class="empty-state-message">No users found.</div>
        </div>
      `;
      return;
    }

    contentContainer.innerHTML = `
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Username</th>
              <th>Email</th>
              <th>Active</th>
              <th>Created</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>
            ${this._users.map((user) => `
              <tr data-user-id="${escapeHtml(String(user.id || user.user_id || ''))}">
                <td>${escapeHtml(user.username || '')}</td>
                <td>${escapeHtml(user.email || '')}</td>
                <td>
                  <span class="badge ${(user.is_active) ? 'badge-active' : 'badge-inactive'}">
                    ${(user.is_active) ? 'Active' : 'Inactive'}
                  </span>
                </td>
                <td class="cell-mono">
                  ${(user.created_at) ? new Date(user.created_at).toLocaleDateString() : '\u2014'}
                </td>
                <td>
                  <div class="flex-gap">
                    <button class="button button-small edit-user-button" data-user-id="${escapeHtml(String(user.id || user.user_id || ''))}">Edit</button>
                    <button class="button button-small button-danger deactivate-user-button" data-user-id="${escapeHtml(String(user.id || user.user_id || ''))}">Deactivate</button>
                  </div>
                </td>
              </tr>
            `).join('')}
          </tbody>
        </table>
      </div>
    `;

    contentContainer.querySelectorAll('.edit-user-button').forEach((button) => {
      button.addEventListener('click', () => {
        const userId = button.dataset.userId;
        const user = this._users.find((u) => String(u.id || u.user_id) === userId);
        if (user)
          this.showEditModal(user);
      });
    });

    contentContainer.querySelectorAll('.deactivate-user-button').forEach((button) => {
      button.addEventListener('click', () => {
        const userId = button.dataset.userId;
        this.deactivateUser(userId);
      });
    });
  }

  showCreateModal() {
    const modalContainer = this.querySelector('#users-modal');
    if (!modalContainer)
      return;

    modalContainer.innerHTML = `
      <div class="modal-overlay" id="modal-overlay">
        <div class="modal-content">
          <div class="modal-title">Create User</div>
          <div id="modal-error"></div>
          <form id="create-user-form">
            <div class="form-group">
              <label class="form-label" for="create-username">Username</label>
              <input class="form-input" id="create-username" type="text" required>
            </div>
            <div class="form-group">
              <label class="form-label" for="create-email">Email</label>
              <input class="form-input" id="create-email" type="email" required>
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

    modalContainer.querySelector('#create-user-form').addEventListener('submit', (event) => this.handleCreate(event));
  }

  async handleCreate(event) {
    event.preventDefault();
    const usernameInput = this.querySelector('#create-username');
    const emailInput = this.querySelector('#create-email');
    const modalError = this.querySelector('#modal-error');

    try {
      const response = await window.api('/system/users', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          username: usernameInput.value,
          email:    emailInput.value,
        }),
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Create failed (${response.status})`);
      }

      this.closeModal();
      this.fetchUsers();
    } catch (error) {
      if (modalError)
        modalError.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
    }
  }

  showEditModal(user) {
    const modalContainer = this.querySelector('#users-modal');
    if (!modalContainer)
      return;

    const userId = user.id || user.user_id;
    const isActive = (user.is_active) ? 'checked' : '';

    modalContainer.innerHTML = `
      <div class="modal-overlay" id="modal-overlay">
        <div class="modal-content">
          <div class="modal-title">Edit User</div>
          <div id="modal-error"></div>
          <form id="edit-user-form">
            <div class="form-group">
              <label class="form-label" for="edit-username">Username</label>
              <input class="form-input" id="edit-username" type="text" value="${escapeHtml(user.username || '')}" required>
            </div>
            <div class="form-group">
              <label class="form-label" for="edit-email">Email</label>
              <input class="form-input" id="edit-email" type="email" value="${escapeHtml(user.email || '')}" required>
            </div>
            <div class="form-group">
              <div class="toggle-wrap">
                <label class="toggle">
                  <input type="checkbox" id="edit-active" ${isActive}>
                  <span class="toggle-track"></span>
                </label>
                <span class="form-label toggle-label">Active</span>
              </div>
            </div>
            <div class="modal-actions">
              <button class="button" type="button" id="cancel-edit-button">Cancel</button>
              <button class="button button-primary" type="submit">Save</button>
            </div>
          </form>
        </div>
      </div>
    `;

    modalContainer.querySelector('#cancel-edit-button').addEventListener('click', () => this.closeModal());
    modalContainer.querySelector('#modal-overlay').addEventListener('click', (event) => {
      if (event.target.id === 'modal-overlay')
        this.closeModal();
    });

    modalContainer.querySelector('#edit-user-form').addEventListener('submit', (event) => this.handleEdit(event, userId));
  }

  async handleEdit(event, userId) {
    event.preventDefault();
    const usernameInput = this.querySelector('#edit-username');
    const emailInput = this.querySelector('#edit-email');
    const activeCheckbox = this.querySelector('#edit-active');
    const modalError = this.querySelector('#modal-error');

    try {
      const response = await window.api(`/system/users/${userId}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          username:  usernameInput.value,
          email:     emailInput.value,
          is_active: activeCheckbox.checked,
        }),
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Update failed (${response.status})`);
      }

      this.closeModal();
      this.fetchUsers();
    } catch (error) {
      if (modalError)
        modalError.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
    }
  }

  async deactivateUser(userId) {
    if (!confirm('Are you sure you want to deactivate this user?'))
      return;

    try {
      const response = await window.api(`/system/users/${userId}`, {
        method: 'DELETE',
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Deactivate failed (${response.status})`);
      }

      this.fetchUsers();
    } catch (error) {
      const errorContainer = this.querySelector('#users-error');
      if (errorContainer)
        errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
    }
  }

  closeModal() {
    const modalContainer = this.querySelector('#users-modal');
    if (modalContainer)
      modalContainer.innerHTML = '';
  }
}

customElements.define('aeor-users', AeorUsers);
