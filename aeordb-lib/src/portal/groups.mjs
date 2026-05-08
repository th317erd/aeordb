'use strict';

import { AeorAdminPage } from '/shared/components/aeor-admin-page.js';
import { escapeHtml } from '/shared/utils.js';
import '/shared/components/aeor-crudlify.js';

class AeorGroupsPage extends AeorAdminPage {

  // ── Subclass contract ───────────────────────────────────────────────

  get title() { return 'Groups'; }
  get showCreateButton() { return true; }

  async fetchItems() {
    const response = await window.api('/system/groups');
    if (!response.ok)
      throw new Error(`Failed to fetch groups (${response.status})`);
    const data = await response.json();
    return data.items || [];
  }

  getItemId(item) {
    return item.name;
  }

  matchesSearch(item, query) {
    const name = (item._displayName || item.name || '').toLowerCase();
    const queryValue = (item.query_value || '').toLowerCase();
    return name.includes(query) || queryValue.includes(query);
  }

  shouldShowEditButton(selectedItems) {
    return selectedItems.length >= 1;
  }

  // ── Post-fetch hook ─────────────────────────────────────────────────

  async onItemsLoaded(items) {
    const userGroups = items.filter((g) => g.name && g.name.startsWith('user:'));
    if (userGroups.length === 0) return;

    try {
      const resp = await window.api('/auth/keys/users');
      if (!resp.ok) return;
      const data = await resp.json();
      const users = data.items || [];
      const userMap = {};
      for (const u of users) userMap[String(u.user_id)] = u.username;

      for (const g of items) {
        if (g.name && g.name.startsWith('user:')) {
          const uid = g.name.slice(5);
          if (userMap[uid]) g._displayName = userMap[uid] + ' (auto)';
        }
      }
    } catch (_) { /* best-effort */ }
  }

  // ── Card rendering ──────────────────────────────────────────────────

  renderCard(item) {
    return `
      <div class="admin-card-header">
        <div class="admin-card-title">${escapeHtml(item._displayName || item.name)}</div>
      </div>
      <div class="admin-card-meta">
        Allow: ${escapeHtml(item.default_allow || '--------')} &middot;
        Deny: ${escapeHtml(item.default_deny || '--------')}
      </div>
      <div class="admin-card-meta">
        Query: ${escapeHtml(item.query_field || '')} ${escapeHtml(item.query_operator || '')} ${escapeHtml(item.query_value || '')}
      </div>
    `;
  }

  // ── Action bar ──────────────────────────────────────────────────────

  getActionButtons(selectedItems) {
    const count = selectedItems.length;
    const label = count === 1 ? 'Delete' : `Delete ${count} Groups`;
    return `<aeor-confirm-button class="confirm-button-danger admin-delete-btn" label="${label}" confirmed-text="Deleted!" duration="1000"></aeor-confirm-button>`;
  }

  _bindActionBarEvents(bar, selectedItems) {
    const deleteBtn = bar.querySelector('.admin-delete-btn');
    if (!deleteBtn) return;

    deleteBtn.addEventListener('confirm', async () => {
      for (const item of selectedItems) {
        const name = this.getItemId(item);
        try {
          const response = await window.api(`/system/groups/${encodeURIComponent(name)}`, { method: 'DELETE' });
          if (!response.ok) {
            const text = await response.text();
            throw new Error(text || `Delete failed (${response.status})`);
          }
        } catch (error) {
          if (window.aeorToast)
            window.aeorToast(`Delete failed: ${error.message}`, 'error');
        }
      }

      this._clearSelection();
      await this._loadItems();
      if (window.aeorToast)
        window.aeorToast('Group(s) deleted', 'success');
    });
  }

  // ── Create modal ────────────────────────────────────────────────────

  renderCreateForm() {
    return `
      <div class="modal-field-group">
        <label class="modal-field-label" for="create-name">Group Name</label>
        <input class="form-input" id="create-name" type="text" required>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label">Default Allow</label>
        <aeor-crudlify id="create-allow" value="-r------"></aeor-crudlify>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label">Default Deny</label>
        <aeor-crudlify id="create-deny" value="--------"></aeor-crudlify>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label" for="create-query-field">Query Field</label>
        <select class="form-input" id="create-query-field">
          <option value="tags" selected>tags</option>
          <option value="user_id">user_id</option>
          <option value="is_active">is_active</option>
          <option value="created_at">created_at</option>
          <option value="updated_at">updated_at</option>
        </select>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label" for="create-query-operator">Query Operator</label>
        <select class="form-input" id="create-query-operator">
          <option value="has" selected>has</option>
          <option value="has_any">has_any</option>
          <option value="has_all">has_all</option>
          <option value="eq">eq</option>
          <option value="neq">neq</option>
          <option value="contains">contains</option>
          <option value="starts_with">starts_with</option>
          <option value="in">in</option>
          <option value="lt">lt</option>
          <option value="gt">gt</option>
        </select>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label" for="create-query-value">Query Value</label>
        <input class="form-input" id="create-query-value" type="text" placeholder="e.g. engineering">
      </div>
    `;
  }

  async submitCreate(modal) {
    const name = modal.querySelector('#create-name').value.trim();
    if (!name) throw new Error('Group name is required');

    const response = await window.api('/system/groups', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        name,
        default_allow:  modal.querySelector('#create-allow').value || '--------',
        default_deny:   modal.querySelector('#create-deny').value || '--------',
        query_field:    modal.querySelector('#create-query-field').value,
        query_operator: modal.querySelector('#create-query-operator').value,
        query_value:    modal.querySelector('#create-query-value').value,
      }),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(text || `Create failed (${response.status})`);
    }

    return response.json();
  }

  // ── Edit modal ──────────────────────────────────────────────────────

  renderEditForm(items) {
    if (items.length === 1) {
      const group = items[0];
      return `
        <div class="modal-field-group">
          <label class="modal-field-label" for="edit-name">Group Name</label>
          <input class="form-input" id="edit-name" type="text" value="${escapeHtml(group.name || '')}" required>
        </div>
        <div class="modal-field-group">
          <label class="modal-field-label">Default Allow</label>
          <aeor-crudlify id="edit-allow" value="${escapeHtml(group.default_allow || '--------')}"></aeor-crudlify>
        </div>
        <div class="modal-field-group">
          <label class="modal-field-label">Default Deny</label>
          <aeor-crudlify id="edit-deny" value="${escapeHtml(group.default_deny || '--------')}"></aeor-crudlify>
        </div>
        <div class="modal-field-group">
          <label class="modal-field-label" for="edit-query-field">Query Field</label>
          <select class="form-input" id="edit-query-field">
            <option value="tags" ${group.query_field === 'tags' ? 'selected' : ''}>tags</option>
            <option value="user_id" ${group.query_field === 'user_id' ? 'selected' : ''}>user_id</option>
            <option value="is_active" ${group.query_field === 'is_active' ? 'selected' : ''}>is_active</option>
            <option value="created_at" ${group.query_field === 'created_at' ? 'selected' : ''}>created_at</option>
            <option value="updated_at" ${group.query_field === 'updated_at' ? 'selected' : ''}>updated_at</option>
          </select>
        </div>
        <div class="modal-field-group">
          <label class="modal-field-label" for="edit-query-operator">Query Operator</label>
          <select class="form-input" id="edit-query-operator">
            <option value="has" ${group.query_operator === 'has' ? 'selected' : ''}>has</option>
            <option value="has_any" ${group.query_operator === 'has_any' ? 'selected' : ''}>has_any</option>
            <option value="has_all" ${group.query_operator === 'has_all' ? 'selected' : ''}>has_all</option>
            <option value="eq" ${group.query_operator === 'eq' ? 'selected' : ''}>eq</option>
            <option value="neq" ${group.query_operator === 'neq' ? 'selected' : ''}>neq</option>
            <option value="contains" ${group.query_operator === 'contains' ? 'selected' : ''}>contains</option>
            <option value="starts_with" ${group.query_operator === 'starts_with' ? 'selected' : ''}>starts_with</option>
            <option value="in" ${group.query_operator === 'in' ? 'selected' : ''}>in</option>
            <option value="lt" ${group.query_operator === 'lt' ? 'selected' : ''}>lt</option>
            <option value="gt" ${group.query_operator === 'gt' ? 'selected' : ''}>gt</option>
          </select>
        </div>
        <div class="modal-field-group">
          <label class="modal-field-label" for="edit-query-value">Query Value</label>
          <input class="form-input" id="edit-query-value" type="text" value="${escapeHtml(group.query_value || '')}">
        </div>
      `;
    }

    // Multi-edit: only allow/deny are editable, query fields hidden
    return `
      <div class="modal-field-group">
        <label class="modal-field-label" for="edit-name">Group Name</label>
        <input class="form-input" id="edit-name" type="text" value="(multiple)" disabled>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label">Default Allow</label>
        <aeor-crudlify id="edit-allow" value="--------"></aeor-crudlify>
      </div>
      <div class="modal-field-group">
        <label class="modal-field-label">Default Deny</label>
        <aeor-crudlify id="edit-deny" value="--------"></aeor-crudlify>
      </div>
    `;
  }

  async submitEdit(items, modal) {
    const defaultAllow = modal.querySelector('#edit-allow').value || '--------';
    const defaultDeny = modal.querySelector('#edit-deny').value || '--------';

    if (items.length === 1) {
      const group = items[0];
      const body = {
        name:           modal.querySelector('#edit-name').value.trim(),
        default_allow:  defaultAllow,
        default_deny:   defaultDeny,
        query_field:    modal.querySelector('#edit-query-field').value,
        query_operator: modal.querySelector('#edit-query-operator').value,
        query_value:    modal.querySelector('#edit-query-value').value,
      };

      const response = await window.api(`/system/groups/${encodeURIComponent(group.name)}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Update failed (${response.status})`);
      }

      return response.json();
    }

    // Multi-edit: patch each group with only allow/deny
    for (const group of items) {
      const response = await window.api(`/system/groups/${encodeURIComponent(group.name)}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          default_allow: defaultAllow,
          default_deny:  defaultDeny,
        }),
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Update failed for ${group.name} (${response.status})`);
      }
    }
  }
}

customElements.define('aeor-groups', AeorGroupsPage);
