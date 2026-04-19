'use strict';

import '/system/portal/dashboard.mjs';
import '/system/portal/users.mjs';
import '/system/portal/groups.mjs';

// ---------------------------------------------------------------------------
// <aeor-crudlify> — Toggle component for crudlify permission flags
// Usage: <aeor-crudlify value="cr--l---"></aeor-crudlify>
// Read value: element.value (returns string like "cr--l---")
// ---------------------------------------------------------------------------

const CRUDLIFY_FLAGS = [
  { char: 'c', label: 'Create' },
  { char: 'r', label: 'Read' },
  { char: 'u', label: 'Update' },
  { char: 'd', label: 'Delete' },
  { char: 'l', label: 'List' },
  { char: 'i', label: 'Index' },
  { char: 'f', label: 'Fork' },
  { char: 'y', label: 'Sync' },
];

class AeorCrudlify extends HTMLElement {
  constructor() {
    super();
    this._flags = [false, false, false, false, false, false, false, false];
  }

  connectedCallback() {
    // Parse initial value attribute
    const initial = this.getAttribute('value') || '--------';
    for (let i = 0; i < 8; i++) {
      this._flags[i] = (initial[i] && initial[i] !== '-');
    }
    this.render();
  }

  get value() {
    return this._flags.map((on, i) => on ? CRUDLIFY_FLAGS[i].char : '-').join('');
  }

  set value(v) {
    for (let i = 0; i < 8; i++) {
      this._flags[i] = (v[i] && v[i] !== '-');
    }
    this.render();
  }

  render() {
    this.innerHTML = `<div class="crudlify-row">${
      CRUDLIFY_FLAGS.map((flag, i) => {
        const active = this._flags[i] ? 'active' : '';
        return `<button type="button" class="crudlify-flag ${active}" data-idx="${i}" title="${flag.label}">${flag.char.toUpperCase()}</button>`;
      }).join('')
    }</div>`;

    this.querySelectorAll('.crudlify-flag').forEach((btn) => {
      btn.addEventListener('click', (e) => {
        e.preventDefault();
        if (e.shiftKey) {
          // Shift+click: invert ALL flags
          for (let i = 0; i < 8; i++) this._flags[i] = !this._flags[i];
          this.querySelectorAll('.crudlify-flag').forEach((b, i) => {
            b.classList.toggle('active', this._flags[i]);
          });
        } else {
          const idx = parseInt(btn.dataset.idx);
          this._flags[idx] = !this._flags[idx];
          btn.classList.toggle('active', this._flags[idx]);
        }
      });
    });
  }
}

customElements.define('aeor-crudlify', AeorCrudlify);

// Auth state management
const AUTH = {
  token: localStorage.getItem('aeordb_token'),
  setToken(token) {
    this.token = token;
    localStorage.setItem('aeordb_token', token);
  },
  clear() {
    this.token = null;
    localStorage.removeItem('aeordb_token');
  },
  headers() {
    return (this.token) ? { 'Authorization': `Bearer ${this.token}` } : {};
  },
};

// Simple fetch wrapper with auth
async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      ...AUTH.headers(),
      ...options.headers,
    },
  });

  if (response.status === 401) {
    AUTH.clear();
    navigate();
    throw new Error('Unauthorized');
  }

  return response;
}

// Expose globals for components
window.AUTH = AUTH;
window.api = api;

// Login component
class AeorLogin extends HTMLElement {
  connectedCallback() {
    this.render();
  }

  render() {
    this.innerHTML = `
      <div class="login-wrap">
        <div class="login-card">
          <div class="login-title">Sign in to AeorDB</div>
          <div id="login-error"></div>
          <form id="login-form">
            <div class="form-group">
              <label class="form-label" for="api-key-input">API Key</label>
              <input class="form-input" id="api-key-input" type="password" placeholder="Enter your API key" autocomplete="off" required>
            </div>
            <button class="button button-primary" type="submit" style="width:100%">Login</button>
          </form>
        </div>
      </div>
    `;

    this.querySelector('#login-form').addEventListener('submit', (event) => this.handleSubmit(event));
  }

  async handleSubmit(event) {
    event.preventDefault();
    const errorContainer = this.querySelector('#login-error');
    const apiKeyInput = this.querySelector('#api-key-input');
    const submitButton = this.querySelector('button[type="submit"]');

    errorContainer.innerHTML = '';
    submitButton.disabled = true;
    submitButton.textContent = 'Signing in...';

    try {
      const response = await fetch('/auth/token', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ api_key: apiKeyInput.value }),
      });

      if (!response.ok) {
        const text = await response.text();
        throw new Error(text || `Authentication failed (${response.status})`);
      }

      const data = await response.json();
      AUTH.setToken(data.token);
      navigate();
    } catch (error) {
      errorContainer.innerHTML = `<div class="alert alert-error">${escapeHtml(error.message)}</div>`;
      submitButton.disabled = false;
      submitButton.textContent = 'Login';
    }
  }
}

customElements.define('aeor-login', AeorLogin);

// HTML escaping helper
function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

// Track whether auth is disabled (--auth=false mode)
let authDisabled = false;

// Cached page instances — survives navigation so chart history isn't lost
const pageCache = {};

function getOrCreatePage(tag) {
  if (!pageCache[tag]) {
    pageCache[tag] = document.createElement(tag);
  }
  return pageCache[tag];
}

// Router
function navigate() {
  const page = location.hash.slice(1) || 'dashboard';
  const main = document.getElementById('main-content');

  // Remove current child without destroying cached elements
  while (main.firstChild) main.removeChild(main.firstChild);

  if (!AUTH.token && !authDisabled) {
    main.appendChild(document.createElement('aeor-login'));
    updateNavLinks('');
    return;
  }

  updateNavLinks(page);

  switch (page) {
    case 'users':
      main.appendChild(getOrCreatePage('aeor-users'));
      break;
    case 'groups':
      main.appendChild(getOrCreatePage('aeor-groups'));
      break;
    default:
      main.appendChild(getOrCreatePage('aeor-dashboard'));
  }
}

function updateNavLinks(activePage) {
  document.querySelectorAll('.nav-link').forEach((element) => {
    element.classList.toggle('active', element.dataset.page === activePage);
  });
}

// Wire up nav click handlers
document.querySelectorAll('.nav-link').forEach((element) => {
  element.addEventListener('click', (event) => {
    event.preventDefault();
    location.hash = element.dataset.page;
  });
});

// Logout button
document.getElementById('logout-button').addEventListener('click', () => {
  AUTH.clear();
  authDisabled = false;
  location.hash = '';
  navigate();
});

// Listen for hash changes
window.addEventListener('hashchange', navigate);

// Detect no-auth mode: probe /system/stats without a token.
// If it succeeds, auth is disabled and we skip the login screen.
// We use /system/health (public) first, then try /system/stats.
async function init() {
  if (!AUTH.token) {
    try {
      const res = await fetch('/system/stats');
      if (res.ok) {
        authDisabled = true;
      }
    } catch (_) {
      // Auth required — show login
    }
  }
  navigate();
}

init();
