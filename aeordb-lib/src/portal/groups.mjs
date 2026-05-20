'use strict';

import { AeorAdminPage } from '/shared/components/aeor-admin-page.js';
import { elements } from '/aeor/elements.js';
import '/shared/components/aeor-crudlify.js';

const { div, label, input, select, option } = elements;
const aeorCrudlify = elements['aeor-crudlify'];
const aeorConfirmButton = elements['aeor-confirm-button'];

const QUERY_FIELDS = ['tags', 'user_id', 'is_active', 'created_at', 'updated_at'];
const QUERY_OPERATORS = [
  'has', 'has_any', 'has_all', 'eq', 'neq',
  'contains', 'starts_with', 'in', 'lt', 'gt',
];

/** Build a DocumentFragment from a list of ElementDefinition nodes. */
function fragment(doc, ...defs) {
  const frag = doc.createDocumentFragment();
  for (const d of defs) {
    if (d == null) continue;
    frag.appendChild(d.build(doc));
  }
  return frag;
}

/** Build a <select> populated with the given options. */
function selectField(id, values, selected) {
  const opts = values.map((value) => {
    const opt = option.value(value)(value);
    return value === selected ? opt.selected(true) : opt;
  });
  return select.class('form-input').id(id)(...opts);
}

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
    const allowFlags = item.default_allow || '--------';
    const denyFlags = item.default_deny || '--------';
    const qf = item.query_field || '';
    const qo = item.query_operator || '';
    const qv = item.query_value || '';

    return fragment(document,
      div.class('admin-card-header')(
        div.class('admin-card-title')(item._displayName || item.name || ''),
      ),
      div.class('admin-card-meta')(
        `Allow: ${allowFlags} · Deny: ${denyFlags}`,
      ),
      div.class('admin-card-meta')(
        `Query: ${qf} ${qo} ${qv}`,
      ),
    );
  }

  // ── Action bar ──────────────────────────────────────────────────────

  getActionButtons(selectedItems) {
    const count = selectedItems.length;
    const labelText = count === 1 ? 'Delete' : `Delete ${count} Groups`;
    return fragment(document,
      aeorConfirmButton
        .class('confirm-button-danger admin-delete-btn')
        .label(labelText)
        .confirmedText('Deleted!')
        .duration('1000')(),
    );
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
    return fragment(document,
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-name')('Group Name'),
        input.class('form-input').id('create-name').type('text').required(true)(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label')('Default Allow'),
        aeorCrudlify.id('create-allow').value('-r------')(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label')('Default Deny'),
        aeorCrudlify.id('create-deny').value('--------')(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-query-field')('Query Field'),
        selectField('create-query-field', QUERY_FIELDS, 'tags'),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-query-operator')('Query Operator'),
        selectField('create-query-operator', QUERY_OPERATORS, 'has'),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-query-value')('Query Value'),
        input.class('form-input').id('create-query-value').type('text').placeholder('e.g. engineering')(),
      ),
    );
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
      return fragment(document,
        div.class('modal-field-group')(
          label.class('modal-field-label').for('edit-name')('Group Name'),
          input.class('form-input').id('edit-name').type('text').value(group.name || '').required(true)(),
        ),
        div.class('modal-field-group')(
          label.class('modal-field-label')('Default Allow'),
          aeorCrudlify.id('edit-allow').value(group.default_allow || '--------')(),
        ),
        div.class('modal-field-group')(
          label.class('modal-field-label')('Default Deny'),
          aeorCrudlify.id('edit-deny').value(group.default_deny || '--------')(),
        ),
        div.class('modal-field-group')(
          label.class('modal-field-label').for('edit-query-field')('Query Field'),
          selectField('edit-query-field', QUERY_FIELDS, group.query_field),
        ),
        div.class('modal-field-group')(
          label.class('modal-field-label').for('edit-query-operator')('Query Operator'),
          selectField('edit-query-operator', QUERY_OPERATORS, group.query_operator),
        ),
        div.class('modal-field-group')(
          label.class('modal-field-label').for('edit-query-value')('Query Value'),
          input.class('form-input').id('edit-query-value').type('text').value(group.query_value || '')(),
        ),
      );
    }

    // Multi-edit: only allow/deny are editable, query fields hidden
    return fragment(document,
      div.class('modal-field-group')(
        label.class('modal-field-label').for('edit-name')('Group Name'),
        input.class('form-input').id('edit-name').type('text').value('(multiple)').disabled(true)(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label')('Default Allow'),
        aeorCrudlify.id('edit-allow').value('--------')(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label')('Default Deny'),
        aeorCrudlify.id('edit-deny').value('--------')(),
      ),
    );
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
