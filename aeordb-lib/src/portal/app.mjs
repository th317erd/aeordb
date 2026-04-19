'use strict';

import '/portal/dashboard.mjs';
import '/portal/users.mjs';

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

// Router
function navigate() {
  const page = location.hash.slice(1) || 'dashboard';
  const main = document.getElementById('main-content');
  main.innerHTML = '';

  if (!AUTH.token && !authDisabled) {
    main.appendChild(document.createElement('aeor-login'));
    updateNavLinks('');
    return;
  }

  updateNavLinks(page);

  switch (page) {
    case 'users':
      main.appendChild(document.createElement('aeor-users'));
      break;
    default:
      main.appendChild(document.createElement('aeor-dashboard'));
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
async function init() {
  if (!AUTH.token) {
    try {
      const res = await fetch('/system/stats');
      if (res.ok) {
        authDisabled = true;
      }
    } catch (_) {
      // Server unreachable or auth required — show login
    }
  }
  navigate();
}

init();
