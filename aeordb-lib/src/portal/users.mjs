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

/**
 * Bootstrap a personal workspace for a newly-created user:
 *   1. mkdir at the target path (no-op if it already exists — admin
 *      may be reusing an existing folder).
 *   2. Grant the user full-access (`crudlify`) on that folder.
 *
 * Both calls go through the standard engine endpoints — `/files/mkdir`
 * and `/files/share` — so this is identical to an admin doing it by
 * hand from the file browser. Just collapsed into one step at
 * user-create time.
 *
 * Throws on any failure; the caller decides how to surface it.
 */
async function bootstrapWorkspace(path, userId) {
  // 1. Create the directory (idempotent — engine returns 200 even if
  //    the folder already exists per current mkdir semantics).
  const mkdirResp = await window.api('/files/mkdir', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ path }),
  });
  if (!mkdirResp.ok) {
    const raw = await mkdirResp.text();
    let msg = `mkdir failed (${mkdirResp.status})`;
    try {
      const parsed = JSON.parse(raw);
      if (parsed && parsed.error) msg = parsed.error;
    } catch (_) { if (raw) msg = raw; }
    throw new Error(msg);
  }

  // 2. Grant the user full crudlify access on the new folder.
  //    crudlify = c(reate) r(ead) u(pdate) d(elete) l(ist) i(nvoke)
  //               f(deploy) y(configure) — all eight ops.
  const shareResp = await window.api('/files/share', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      paths: [path],
      users: [userId],
      permissions: 'crudlify',
    }),
  });
  if (!shareResp.ok) {
    const raw = await shareResp.text();
    let msg = `share failed (${shareResp.status})`;
    try {
      const parsed = JSON.parse(raw);
      if (parsed && parsed.error) msg = parsed.error;
    } catch (_) { if (raw) msg = raw; }
    throw new Error(msg);
  }
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
    const frag = fragment(document,
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-username')('Username'),
        input.class('form-input').id('create-username').type('text').required(true)(),
      ),
      div.class('modal-field-group')(
        label.class('modal-field-label').for('create-email')('Email'),
        input.class('form-input').id('create-email').type('text').required(true)(),
      ),
      // Personal-workspace bootstrap. Checked by default — the 90% case
      // is an interactive user who needs *somewhere* to put files.
      // Service / backup / replication users uncheck this and start with
      // zero grants (admin will grant them specific paths later, or
      // they live entirely in API land and never use the portal).
      div.class('modal-field-group').style('margin-top:0.75rem')(
        label.style('display:flex;align-items:center;gap:0.5rem;cursor:pointer;font-weight:500')(
          input.type('checkbox').id('create-workspace').checked(true)(),
          span('Grant a personal workspace'),
        ),
        div.style('margin-left:1.5rem;margin-top:0.5rem')(
          label.class('modal-field-label').for('create-workspace-path').style('font-weight:400;color:var(--text-muted);font-size:0.85rem')(
            'Path (full access for this user)',
          ),
          input.class('form-input').id('create-workspace-path').type('text').placeholder('/workspaces/<username>')(),
          div.class('text-muted').style('font-size:0.75rem;margin-top:0.25rem')(
            'A new folder will be created at this path and shared with the user. Leave empty to use the default.',
          ),
        ),
      ),
    );

    // Live-suggest the workspace path as the username is typed. Only
    // overrides the path field if the admin hasn't typed something
    // custom (i.e. the field still matches the previous auto-suggestion).
    // setTimeout so the wiring runs after the fragment is in the DOM.
    setTimeout(() => {
      const usernameInput = document.querySelector('#create-username');
      const pathInput = document.querySelector('#create-workspace-path');
      const wsCheckbox = document.querySelector('#create-workspace');
      const pathWrap = pathInput && pathInput.parentElement;
      if (!usernameInput || !pathInput) return;
      let lastSuggested = '';
      usernameInput.addEventListener('input', () => {
        const u = usernameInput.value.trim();
        const suggestion = u ? `/workspaces/${u}` : '';
        if (pathInput.value === lastSuggested || pathInput.value === '') {
          pathInput.value = suggestion;
          lastSuggested = suggestion;
        }
      });
      // Visually muted out when the workspace is unchecked.
      if (wsCheckbox && pathWrap) {
        const applyVisibility = () => {
          pathWrap.style.opacity = wsCheckbox.checked ? '1' : '0.4';
          pathInput.disabled = !wsCheckbox.checked;
        };
        wsCheckbox.addEventListener('change', applyVisibility);
        applyVisibility();
      }
    }, 0);

    return frag;
  }

  async submitCreate(modal) {
    const username = modal.querySelector('#create-username').value.trim();
    const email = modal.querySelector('#create-email').value.trim();

    if (!username || !email)
      throw new Error('Username and email are required');

    const wsCheckbox = modal.querySelector('#create-workspace');
    const wsPathInput = modal.querySelector('#create-workspace-path');
    const grantWorkspace = wsCheckbox && wsCheckbox.checked;
    const wsPath = grantWorkspace
      ? (wsPathInput.value.trim() || `/workspaces/${username}`)
      : null;

    // 1. Create the user.
    const response = await window.api('/system/users', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, email }),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(text || `Create failed (${response.status})`);
    }

    const user = await response.json();

    // 2. Optional: bootstrap a personal workspace.
    //    Both steps are best-effort — the user account is already created.
    //    If either step fails we surface a warning toast but don't roll back;
    //    the admin can fix it from the file browser via the share modal.
    if (grantWorkspace && wsPath && user && user.user_id) {
      try {
        await bootstrapWorkspace(wsPath, user.user_id);
      } catch (e) {
        if (window.aeorToast) {
          window.aeorToast(
            `User created but workspace setup failed: ${e.message}. Create the folder and share it manually.`,
            'warning',
          );
        }
      }
    }

    return user;
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
