'use strict';

// ---------------------------------------------------------------------------
// Shim: intercept the client-app fetch calls that the shared file browser
// component makes (/api/v1/sync, /api/v1/browse/...) and translate them
// into AeorDB portal API calls.  This lets us use the component as-is
// without modifications.
// ---------------------------------------------------------------------------

const PORTAL_RELATIONSHIP = {
  id: 'portal',
  name: 'Database',
  remote_path: '/',
  local_path: '/',
  direction: 'pull',
};

const _realFetch = window.fetch.bind(window);

window.fetch = async function shimmedFetch(input, init) {
  const url = (typeof input === 'string') ? input : input.url;

  // GET /api/v1/sync → return a single fake "local database" relationship
  if (url === '/api/v1/sync') {
    return new Response(JSON.stringify([PORTAL_RELATIONSHIP]), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    });
  }

  // GET /api/v1/browse/{rel_id}/{path}?limit=N&offset=M → proxy to /files/{path}
  const browseMatch = url.match(/^\/api\/v1\/browse\/[^/?]+\/?(.*)$/);
  if (browseMatch) {
    const rawTail = browseMatch[1]; // "path?limit=100&offset=0" or "?limit=..."
    const [pathPart, queryString] = rawTail.split('?');

    // Build the /files/ path — root requires %2F since the route is /files/{*path}
    const decodedPath = decodeURIComponent(pathPart || '');
    const filesPath = (decodedPath && decodedPath !== '/')
      ? `/files/${decodedPath}`
      : '/files/%2F';

    const qs = (queryString) ? `?${queryString}` : '';
    const response = await window.api(`${filesPath}${qs}`);

    if (!response.ok)
      return response;

    const data = await response.json();
    const items = data.items || [];

    // Transform AeorDB listing shape → component's expected shape
    const transformed = {
      entries: items.map((item) => ({
        name: item.name,
        path: item.path,
        entry_type: item.entry_type,
        size: item.size || 0,
        content_type: item.content_type || 'application/octet-stream',
        created_at: item.created_at,
        updated_at: item.updated_at,
      })),
      total: (data.total != null) ? data.total : items.length,
    };

    return new Response(JSON.stringify(transformed), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    });
  }

  // POST /api/v1/files/{rel_id}/rename → proxy to /files/rename
  const renameMatch = url.match(/^\/api\/v1\/files\/[^/?]+\/rename$/);
  if (renameMatch) {
    return window.api('/files/rename', init);
  }

  // GET/PUT/DELETE /api/v1/files/{rel_id}/{path} → proxy to /files/{path}
  // Used by preview components (src attribute) and file operations
  const filesMatch = url.match(/^\/api\/v1\/files\/[^/?]+\/(.+)$/);
  if (filesMatch) {
    const encodedPath = filesMatch[1];
    const decodedPath = decodeURIComponent(encodedPath);
    const filesUrl = `/files/${decodedPath}`;
    return window.api(filesUrl, init);
  }

  // Everything else — pass through
  return _realFetch(input, init);
};

// ---------------------------------------------------------------------------
// Portal file browser page — just mount the shared component
// ---------------------------------------------------------------------------

import '/system/portal/shared/components/aeor-file-browser.js';

// ---------------------------------------------------------------------------
// Patch the file browser component BEFORE it instantiates — override the
// prototype so every instance gets portal behavior from the start.
// ---------------------------------------------------------------------------

const BrowserClass = customElements.get('aeor-file-browser');
if (BrowserClass) {
  const origRender = BrowserClass.prototype.render;
  const origClose = BrowserClass.prototype._closeTab;
  const origFetchRel = BrowserClass.prototype._fetchRelationships;

  // Override _fetchRelationships: after fetching, auto-open a tab instead
  // of showing the relationship selector (portal only has one "relationship")
  BrowserClass.prototype._fetchRelationships = async function() {
    await origFetchRel.call(this);

    if (!this._active_tab_id) {
      this._openTab(PORTAL_RELATIONSHIP.id, PORTAL_RELATIONSHIP.name);
    }
  };

  // Override render: after normal render, patch "+" button and hide close on last tab
  BrowserClass.prototype.render = function() {
    origRender.call(this);

    // "+" should auto-open a new tab, not show the relationship selector
    const newTabBtn = this.querySelector('.tab-new');
    if (newTabBtn) {
      const fresh = newTabBtn.cloneNode(true);
      newTabBtn.replaceWith(fresh);
      fresh.addEventListener('click', () => {
        this._openTab(PORTAL_RELATIONSHIP.id, PORTAL_RELATIONSHIP.name);
      });
    }

    // Hide close button when only one tab remains
    if (this._tabs.length <= 1) {
      this.querySelectorAll('.tab-close').forEach((btn) => {
        btn.style.display = 'none';
      });
    }
  };

  // Guard _closeTab: prevent closing the last tab
  BrowserClass.prototype._closeTab = function(tabId) {
    if (this._tabs.length <= 1)
      return;

    origClose.call(this, tabId);
  };
}

class AeorFiles extends HTMLElement {
  connectedCallback() {
    if (!this._initialized) {
      this._initialized = true;
      this.innerHTML = '<aeor-file-browser></aeor-file-browser>';
    }
  }
}

customElements.define('aeor-files', AeorFiles);
