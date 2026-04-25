'use strict';

import { escapeHtml } from '/system/portal/shared/utils.js';
import '/system/portal/shared/components/aeor-crudlify.js';
import '/system/portal/dashboard.mjs';
import '/system/portal/users.mjs';
import '/system/portal/groups.mjs';
import '/system/portal/files.mjs';
import '/system/portal/keys.mjs';
import '/system/portal/settings.mjs';

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
  /** Decode the JWT payload to get the current user's sub (user_id). */
  currentUserId() {
    if (!this.token) return null;
    try {
      const payload = JSON.parse(atob(this.token.split('.')[1]));
      return payload.sub || null;
    } catch (_) { return null; }
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

// Detect ?token= query param for share link access
(function detectShareToken() {
  const params = new URLSearchParams(window.location.search);
  const token = params.get('token');
  if (token) {
    AUTH.setToken(token);
    AUTH._isShareSession = true;
  }
})();

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

// Track whether auth is disabled (--auth=false mode)
let authDisabled = false;

// Cached page instances — survives navigation so chart history isn't lost
const pageCache = {};

function hideAllPages(container) {
  for (const el of Object.values(pageCache)) {
    el.style.display = 'none';
  }
}

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

  if (!AUTH.token && !authDisabled) {
    // Hide all pages, show login
    hideAllPages(main);
    let login = main.querySelector('aeor-login');
    if (!login) {
      login = document.createElement('aeor-login');
      main.appendChild(login);
    }
    login.style.display = '';
    updateNavLinks('');
    // Hide sidebar and mobile top bar on login screen
    if (sidebar) sidebar.style.display = 'none';
    const mobileTopBar = document.querySelector('.mobile-top-bar');
    if (mobileTopBar) mobileTopBar.style.display = 'none';
    main.style.marginLeft = '0';
    return;
  }

  // Show sidebar when logged in
  const mobileTopBar = document.querySelector('.mobile-top-bar');
  if (mobileTopBar) mobileTopBar.style.display = '';

  // Hide login if it exists
  const login = main.querySelector('aeor-login');
  if (login) login.style.display = 'none';

  updateNavLinks(page);

  // Ensure all pages are in the DOM (created once, never removed)
  const pageMap = {
    'dashboard': 'aeor-dashboard',
    'files': 'aeor-files',
    'users': 'aeor-users',
    'groups': 'aeor-groups',
    'keys': 'aeor-keys',
    'settings': 'aeor-settings',
  };

  // Share sessions default to files page
  const defaultPage = AUTH._isShareSession ? 'files' : 'dashboard';
  const activeTag = pageMap[page] || pageMap[defaultPage];

  for (const [, tag] of Object.entries(pageMap)) {
    let el = getOrCreatePage(tag);
    if (!el.parentNode) {
      el.style.display = 'none';
      main.appendChild(el);
    }
    const isActive = tag === activeTag;
    el.style.display = isActive ? '' : 'none';
    // Notify the page it became visible so it can refresh data
    if (isActive && typeof el.onPageShow === 'function') el.onPageShow();
  }

  // Share session: navigate file browser to shared path
  if (AUTH._isShareSession) {
    const params = new URLSearchParams(window.location.search);
    const sharedPath = params.get('path');
    // Hide non-files sidebar items in share sessions
    document.querySelectorAll('.nav-link').forEach((link) => {
      if (link.dataset.page !== 'files') {
        link.style.display = 'none';
      }
    });
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
