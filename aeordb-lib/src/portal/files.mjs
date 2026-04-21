'use strict';

import { FileBrowserAdapter } from '/system/portal/shared/components/aeor-file-browser-adapter.js';
import '/system/portal/shared/components/aeor-file-browser.js';

class PortalFileBrowserAdapter extends FileBrowserAdapter {
  constructor() {
    super();
  }

  async browse(path, limit, offset) {
    const encoded = encodeURIComponent(path === '/' ? '/' : path);
    const response = await window.api(`/files/${encoded}?limit=${limit}&offset=${offset}`);
    if (!response.ok)
      throw new Error(`${response.status}`);

    const data = await response.json();
    // Server returns { items: [...] } — map to { entries, total }
    const items = data.items || [];
    return {
      entries: items.map((item) => ({
        name: item.name,
        entry_type: item.entry_type,
        size: item.size || item.total_size || 0,
        content_type: item.content_type || 'application/octet-stream',
        created_at: item.created_at,
        updated_at: item.updated_at,
      })),
      total: data.total || items.length,
    };
  }

  fileUrl(path) {
    return `/files${path}`;
  }

  async upload(path, body, contentType) {
    const response = await window.api(`/files${path}`, {
      method: 'PUT',
      headers: { 'Content-Type': contentType },
      body,
    });
    if (!response.ok)
      throw new Error(`${response.status}`);
  }

  async delete(path) {
    const response = await window.api(`/files${path}`, { method: 'DELETE' });
    if (!response.ok)
      throw new Error(`${response.status}`);
  }

  async rename(fromPath, toPath) {
    const response = await window.api('/files/rename', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ from: fromPath, to: toPath }),
    });
    if (!response.ok)
      throw new Error(`${response.status}`);
  }

  get supportsOpenLocally() { return false; }
  get supportsTabs() { return false; }
  get supportsSync() { return false; }
}

class AeorFiles extends HTMLElement {
  connectedCallback() {
    if (!this._initialized) {
      this._initialized = true;
      this.innerHTML = '<aeor-file-browser></aeor-file-browser>';
      const browser = this.querySelector('aeor-file-browser');
      browser.setAdapter(new PortalFileBrowserAdapter());
    }
  }
}

customElements.define('aeor-files', AeorFiles);
