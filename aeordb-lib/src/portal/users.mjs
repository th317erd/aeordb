'use strict';

import { AeorAdminPage } from '/shared/components/aeor-admin-page.js';
import { elements } from '/aeor/elements.js';

const { div, span, label, input } = elements;

/** Build a DocumentFragment from a list of ElementDefinition nodes. */
function fragment(doc, ...defs) {
  const frag = doc.createDocumentFragment();
  for (const d of defs) {
    if (d == null) continue;
    frag.appendChild(d.build(doc));
  }
  return frag;
}

class AeorUsersPage extends AeorAdminPage {

  // ── Subclass contract ───────────────────────────────────────────────

  get title() { return 'Users'; }
  get showCreateButton() { return true; }

  async fetchItems() {
    const response = await window.api('/system/users');
    if (!response.ok)
      throw new Error(`Failed to fetch users (${response.status})`);
    const data = await response.json();
    return data.items || [];
  }

  getItemId(item) {
    return item.user_id || item.id;
  }

  renderCard(item) {
    const created = item.created_at
      ? new Date(item.created_at).toLocaleDateString()
      : '—';
    const statusClass = item.is_active ? 'badge badge-active' : 'badge badge-inactive';
    const statusLabel = item.is_active ? 'Active' : 'Inactive';

    return fragment(document,
      div.class('admin-card-header')(
        div.class('admin-card-title')(
          item.username || '',
          ' ',
          span.class(statusClass)(statusLabel),
        ),
      ),
      div.class('admin-card-meta')(item.email || ''),
      div.class('admin-card-meta')(`Created ${created}`),
    );
  }

  matchesSearch(item, query) {
    const username = (item.username || '').toLowerCase();
    const email = (item.email || '').toLowerCase();
    return username.includes(query) || email.includes(query);
  }

  shouldShowEditButton(selectedItems) {
    return selectedItems.length === 1;
  }

  getActionButtons(selectedItems) {
    const count = selectedItems.length;
    const labelText = count === 1 ? 'Deactivate' : `Deactivate ${count}`;
    const confirmBtn = elements['aeor-confirm-button'];

    return fragment(document,
      confirmBtn
        .class('confirm-button-danger admin-deactivate-btn')
        .label(labelText)
        .confirmedText('Deactivated!')
        .duration('1000')(),
    );
  }

  _bindActionBarEvents(bar, selectedItems) {
    const deactivateBtn = bar.querySelector('.admin-deactivate-btn');
    if (!deactivateBtn) return;

    deactivateBtn.addEventListener('confirm', async () => {
      for (const item of selectedItems) {
        const userId = this.getItemId(item);
        try {
          const response = await window.api(`/system/users/${userId}`, { method: 'DELETE' });
          if (!response.ok) {
            const text = await response.text();
            throw new Error(text || `Deactivate failed (${response.status})`);
          }
        } catch (error) {
          if (window.aeorToast)
            window.aeorToast(`Deactivate failed: ${error.message}`, 'error');
        }
      }

      this._clearSelection();
      await this._loadItems();
      if (window.aeorToast)
        window.aeorToast('User(s) deactivated', 'success');
    });
  }

  // ── Create modal ────────────────────────────────────────────────────

  renderCreateForm() {
    return fragment(document,
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-username')('Username'),
        input.class('form-input').id('create-username').type('text').required(true)(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-email')('Email'),
        input.class('form-input').id('create-email').type('text').required(true)(),
      ),
    );
  }

  async submitCreate(modal) {
    const username = modal.querySelector('#create-username').value.trim();
    const email = modal.querySelector('#create-email').value.trim();

    if (!username || !email)
      throw new Error('Username and email are required');

    const response = await window.api('/system/users', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, email }),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(text || `Create failed (${response.status})`);
    }

    return response.json();
  }

  // ── Edit modal ──────────────────────────────────────────────────────

  renderEditForm(items) {
    const user = items[0];
    const usernameInput = input
      .class('form-input')
      .id('edit-username')
      .type('text')
      .value(user.username || '')
      .required(true);

    const emailInput = input
      .class('form-input')
      .id('edit-email')
      .type('text')
      .value(user.email || '')
      .required(true);

    const activeInput = user.is_active
      ? input.type('checkbox').id('edit-active').checked(true)
      : input.type('checkbox').id('edit-active');

    return fragment(document,
      div.class('modal-field-group')(
        label.class('modal-field-label').for('edit-username')('Username'),
        usernameInput(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label').for('edit-email')('Email'),
        emailInput(),
      ),
      div.class('modal-field-group')(
        div.class('toggle-wrap')(
          label.class('toggle')(
            activeInput(),
            span.class('toggle-track')(),
          ),
          span.class('modal-field-label toggle-label')('Active'),
        ),
      ),
    );
  }

  async submitEdit(items, modal) {
    const user = items[0];
    const userId = this.getItemId(user);
    const username = modal.querySelector('#edit-username').value.trim();
    const email = modal.querySelector('#edit-email').value.trim();
    const is_active = modal.querySelector('#edit-active').checked;

    if (!username || !email)
      throw new Error('Username and email are required');

    const response = await window.api(`/system/users/${userId}`, {
      method: 'PATCH',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, email, is_active }),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(text || `Update failed (${response.status})`);
    }

    return response.json();
  }
}

customElements.define('aeor-users', AeorUsersPage);
