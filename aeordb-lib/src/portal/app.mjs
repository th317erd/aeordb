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

// URL parameter helpers
function getPageParam() {
  const params = new URLSearchParams(window.location.search);
  return params.get('page') || 'dashboard';
}

function isFrameMode() {
  const params = new URLSearchParams(window.location.search);
  return params.get('frame') === 'true';
}

function setPageParam(page) {
  const params = new URLSearchParams(window.location.search);
  params.set('page', page);
  const newUrl = `${window.location.pathname}?${params.toString()}`;
  window.history.pushState({}, '', newUrl);
  navigate();
}

// Router
function navigate() {
  const page = getPageParam();
  const main = document.getElementById('main-content');
  const sidebar = document.querySelector('.sidebar');

  // Frame mode: hide sidebar
  if (sidebar) {
    if (isFrameMode()) {
      sidebar.style.display = 'none';
      main.style.marginLeft = '0';
    } else {
      sidebar.style.display = '';
      main.style.marginLeft = '';
    }
  }

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

// Wire up nav click handlers — use URL params instead of hash
document.querySelectorAll('.nav-link').forEach((element) => {
  element.addEventListener('click', (event) => {
    event.preventDefault();
    setPageParam(element.dataset.page);
  });
});

// Logout button
document.getElementById('logout-button').addEventListener('click', () => {
  AUTH.clear();
  authDisabled = false;
  setPageParam('dashboard');
});

// Listen for browser back/forward navigation
window.addEventListener('popstate', navigate);

// Detect no-auth mode: probe /system/stats without a token.
// Detect no-auth mode by probing /system/stats (behind auth middleware).
// If it succeeds without a token, auth is disabled — skip the login screen.
// Falls back to /system/health (always public) to distinguish "auth disabled"
// from "server unreachable".
async function init() {
  if (!AUTH.token) {
    try {
      const statsRes = await fetch('/system/stats');
      if (statsRes.ok) {
        // Stats endpoint responded without auth — auth is disabled
        authDisabled = true;
      }
    } catch (_) {
      // Network error — server may be unreachable
    }

    if (!authDisabled) {
      try {
        // If stats failed, check if the server is even reachable
        const healthRes = await fetch('/system/health');
        if (healthRes.ok) {
          const health = await healthRes.json();
          // If signing_key_present is false AND mode is standalone,
          // auth is disabled (--auth=false sets no signing key)
          if (health.checks?.auth?.signing_key_present === false) {
            authDisabled = true;
          }
        }
      } catch (_) {
        // Server unreachable
      }
    }
  }
  navigate();
}

init();
