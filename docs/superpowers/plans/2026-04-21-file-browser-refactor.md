# File Browser Component Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the monolithic file browser + fetch shim with a clean base class / subclass hierarchy — eliminating all monkey-patching and enabling both the client app and DB portal to extend the same core.

**Architecture:** Extract shared logic from the current 900-line `aeor-file-browser.js` into `AeorFileBrowserBase`. The base class handles tabs, navigation, listing, preview, upload, pagination — but never calls `fetch()` directly. Subclasses implement abstract data-access methods. Client subclass adds sync relationships and drag-out. Portal subclass adds direct `/files/` API access, auto-open tab, and ZIP download. A new `POST /files/download` Rust endpoint streams ZIP archives.

**Tech Stack:** JavaScript (web components, ES modules), Rust (axum, zip crate), HTML/CSS

**Spec:** `docs/superpowers/specs/2026-04-21-file-browser-refactor-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `aeordb-web-components/components/aeor-file-browser-base.js` | Create | Base class: tabs, navigation, listing, preview, upload, pagination, rendering |
| `aeordb-web-components/components/aeor-file-browser.js` | Rewrite | Client subclass: sync relationships, drag-out, client API calls |
| `aeordb-web-components/components/aeor-file-browser-portal.js` | Create | Portal subclass: `/files/` API, auto-open tab, last-tab guard, download button |
| `aeordb-lib/src/portal/files.mjs` | Rewrite | Simplified — just mounts `<aeor-file-browser-portal>`, no shim |
| `aeordb-lib/src/server/download_routes.rs` | Create | `POST /files/download` ZIP streaming endpoint |
| `aeordb-lib/src/server/mod.rs` | Modify | Register download route + new portal include_str! assets |
| `aeordb-lib/src/server/portal_routes.rs` | Modify | Add include_str! for new portal/base component files |
| `aeordb-lib/spec/http/download_spec.rs` | Create | Tests for ZIP download endpoint |

---

### Task 1: Create the Base Class — `AeorFileBrowserBase`

**Files:**
- Create: `aeordb-web-components/components/aeor-file-browser-base.js`

This is the largest task. Extract all shared logic from the current `aeor-file-browser.js` into a base class. The base class handles everything except data fetching and the "new tab" interstitial.

- [ ] **Step 1: Read the current monolith for reference**

Read `aeordb-web-components/components/aeor-file-browser.js` (the shared library version) and `aeordb-web-components/components/aeor-file-view-shared.js` to understand all functions being extracted.

- [ ] **Step 2: Create the base class**

Create `aeordb-web-components/components/aeor-file-browser-base.js`. This file should contain:

**Imports:**
```javascript
import {
  formatSize, formatDate, fileIcon,
  escapeHtml, escapeAttr, isImageFile, isVideoFile, isAudioFile, isTextFile,
  ENTRY_TYPE_DIR, directionArrow,
} from './aeor-file-view-shared.js';
```

**The `loadPreviewComponent` function** — copy verbatim from the monolith (lines ~9-40). This uses dynamic `import('./previews/...')` which resolves relative to this file — both base and subclasses live in the same `components/` directory, so the path works.

**The `AeorFileBrowserBase` class** — extends `HTMLElement`, exported. Include these methods (copied from the monolith, with data-access calls replaced by abstract method calls):

Constructor (`constructor()`):
- Initialize: `this._tabs = []`, `this._active_tab_id = null`, `this._tab_counter = 0`, `this._relationships = []` (kept for backward compat but unused by base)

State management:
- `_saveState()` — serialize tabs to localStorage (copy from monolith)
- `_loadState()` — restore tabs from localStorage (copy from monolith)

Lifecycle:
- `connectedCallback()` — calls `this._loadState()`, `this.render()`, then if there's an active tab with entries, calls `this._fetchListing()`

Core rendering:
- `render()` — builds the full DOM: page header, tab bar (if tabs exist), tab content containers. If no active tab, calls `this.renderNoTabContent()` (a hook subclasses override). Copy the render logic from the monolith but replace the `_renderRelationshipSelector()` call with `this.renderNoTabContent()`.
- `_renderTabBar()` — copy from monolith
- `_renderDirectoryViewFor(tab)` — copy from monolith (renders list/grid view, handles loading state, empty state). Replace the inline fetch URLs for file content (grid thumbnails) with `this.fileUrl(path)`.
- `_renderBreadcrumbs(tab)` — copy from monolith, but use a `this.rootLabel()` method for the root breadcrumb name (subclasses override; default returns `'Root'`)
- `_updateTabContent(tabId)` — copy from monolith
- `_showPreview(tab)` — copy from monolith, but replace the hardcoded `/api/v1/files/{rel_id}/...` URL with `this.fileUrl(path)`

Event binding:
- `_bindShellEvents()` — copy from monolith, but replace the "new tab" click handler `() => { this._active_tab_id = null; this.render(); }` with `() => { this.openNewTab(); }`
- `_bindTabContentEvents(tabId)` — copy from monolith, but replace inline fetch calls:
  - File entry click (directory) → `this._navigateTo(path)` (no change)
  - File entry click (file) → sets preview_entry, calls `this._loadPreview()` (no change)
  - Upload button click → calls `this._handleUpload(event)` (no change internally, but `_handleUpload` uses abstract `upload()`)
  - Delete action → calls `this._handlePreviewAction('delete')` which uses abstract `deletePath()`
  - Rename → calls `this._renamePreviewFile(newName)` which uses abstract `renamePath()`

Tab management:
- `_openTab(id, name)` — copy from monolith. Creates tab object, sets active, saves state, renders, calls `_fetchListing()`
- `_switchTab(tabId)` — copy from monolith
- `_closeTab(tabId)` — copy from monolith (subclasses can override to guard)
- `_navigateTo(path)` — copy from monolith. Updates tab path, calls `_fetchListing()`

Data fetching (uses abstract methods):
- `async _fetchListing()` — replaces the monolith's version. Gets the active tab, calls `const data = await this.browse(tab.path, tab.page_size, 0)`, sets `tab.entries = data.entries`, `tab.total = data.total`. On error, sets entries to empty.
- `async _fetchMore()` — infinite scroll handler. Calls `this.browse(tab.path, tab.page_size, tab.entries.length)`, appends results.
- `async _loadPreview()` — copy from monolith (calls `loadPreviewComponent` then `_showPreview`)
- `_hydratePreview()` — copy from monolith
- `async _renamePreviewFile(newName)` — replaces monolith version: calls `await this.renamePath(fromPath, toPath)` instead of inline fetch
- `async _handlePreviewAction(action)` — replaces monolith version: `delete` calls `await this.deletePath(path)` instead of inline fetch, `close-preview` unchanged. Remove `open-local` — that's a client subclass concern.
- `async _handleUpload(event)` — replaces monolith version: iterates files, calls `await this.upload(path, body, contentType)` for each

Utility:
- `_activeTab()` — copy from monolith
- `_truncate(str, max)` — copy from monolith

**Abstract methods** — throw errors if not overridden:
```javascript
async browse(path, limit, offset) {
  throw new Error('AeorFileBrowserBase.browse() must be implemented by subclass');
}

fileUrl(path) {
  throw new Error('AeorFileBrowserBase.fileUrl() must be implemented by subclass');
}

async upload(path, body, contentType) {
  throw new Error('AeorFileBrowserBase.upload() must be implemented by subclass');
}

async deletePath(path) {
  throw new Error('AeorFileBrowserBase.deletePath() must be implemented by subclass');
}

async renamePath(fromPath, toPath) {
  throw new Error('AeorFileBrowserBase.renamePath() must be implemented by subclass');
}

openNewTab() {
  throw new Error('AeorFileBrowserBase.openNewTab() must be implemented by subclass');
}
```

**Hook methods** — subclasses CAN override:
```javascript
renderNoTabContent() {
  return '<div class="empty-state">No tabs open.</div>';
}

rootLabel() {
  return 'Root';
}
```

**Do NOT call `customElements.define()`** — the base class is abstract. Only subclasses register themselves.

**Do NOT include:**
- `_fetchRelationships()` — client subclass only
- `_renderRelationshipSelector()` — client subclass only
- Sync badge rendering — client subclass only
- Drag-out to OS logic — client subclass only
- Any hardcoded `/api/v1/...` URLs — all data access through abstract methods

- [ ] **Step 3: Verify the file exists and has reasonable structure**

Run: `wc -l aeordb-web-components/components/aeor-file-browser-base.js`
Expected: roughly 500-650 lines (the monolith is 900, we're removing ~250 lines of client-specific code)

- [ ] **Step 4: Commit**

```bash
cd /home/wyatt/Projects/aeordb-workspace/aeordb-web-components
git add components/aeor-file-browser-base.js
git commit -m "Add AeorFileBrowserBase: shared file browser logic with abstract data access"
```

---

### Task 2: Rewrite Client Subclass — `AeorFileBrowser`

**Files:**
- Rewrite: `aeordb-web-components/components/aeor-file-browser.js`

Replace the 900-line monolith with a thin subclass that extends `AeorFileBrowserBase` and adds sync/relationship logic.

- [ ] **Step 1: Rewrite the client subclass**

Replace the contents of `aeordb-web-components/components/aeor-file-browser.js` with:

```javascript
'use strict';

import { AeorFileBrowserBase } from './aeor-file-browser-base.js';
import { escapeHtml, escapeAttr, directionArrow } from './aeor-file-view-shared.js';

const ENTRY_TYPE_DIR = 3;

export class AeorFileBrowser extends AeorFileBrowserBase {
  constructor() {
    super();
    this._relationships = [];
  }

  connectedCallback() {
    super.connectedCallback();
    this._fetchRelationships();
  }

  // ---------------------------------------------------------------------------
  // Abstract method implementations
  // ---------------------------------------------------------------------------

  async browse(path, limit, offset) {
    const tab = this._activeTab();
    if (!tab) throw new Error('No active tab');
    const encodedPath = (path === '/') ? '' : encodeURIComponent(path);
    const baseUrl = encodedPath
      ? `/api/v1/browse/${tab.relationship_id}/${encodedPath}`
      : `/api/v1/browse/${tab.relationship_id}`;
    const url = `${baseUrl}?limit=${limit}&offset=${offset}`;
    const response = await fetch(url);
    if (!response.ok) throw new Error(`Request failed: ${response.status}`);
    return response.json();
  }

  fileUrl(path) {
    const tab = this._activeTab();
    if (!tab) return '#';
    return `/api/v1/files/${tab.relationship_id}/${encodeURIComponent(path)}`;
  }

  async upload(path, body, contentType) {
    const response = await fetch(this.fileUrl(path), {
      method: 'PUT',
      headers: { 'Content-Type': contentType },
      body,
    });
    if (!response.ok) throw new Error(`Upload failed: ${response.status}`);
  }

  async deletePath(path) {
    const response = await fetch(this.fileUrl(path), { method: 'DELETE' });
    if (!response.ok) throw new Error(`Delete failed: ${response.status}`);
  }

  async renamePath(fromPath, toPath) {
    const tab = this._activeTab();
    if (!tab) throw new Error('No active tab');
    const response = await fetch(`/api/v1/files/${tab.relationship_id}/rename`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ from: fromPath, to: toPath }),
    });
    if (!response.ok) throw new Error(`Rename failed: ${response.status}`);
  }

  openNewTab() {
    // Show the relationship selector
    this._active_tab_id = null;
    this.render();
  }

  // ---------------------------------------------------------------------------
  // Hook overrides
  // ---------------------------------------------------------------------------

  renderNoTabContent() {
    return this._renderRelationshipSelector();
  }

  rootLabel() {
    const tab = this._activeTab();
    return (tab && tab.relationship_name) ? tab.relationship_name : 'Database';
  }

  // ---------------------------------------------------------------------------
  // Client-specific: relationship selector
  // ---------------------------------------------------------------------------

  async _fetchRelationships() {
    try {
      const response = await fetch('/api/v1/sync');
      if (!response.ok) throw new Error(`Request failed: ${response.status}`);
      this._relationships = await response.json();
      if (!this._active_tab_id) this.render();
    } catch (error) {
      console.error('Failed to fetch relationships:', error);
    }
  }

  _renderRelationshipSelector() {
    if (this._relationships.length === 0) {
      return '<div class="empty-state">No sync relationships configured. Set up a sync first.</div>';
    }

    const cards = this._relationships.map((rel) => {
      const remoteName = rel.remote_path.replace(/\/$/, '').split('/').pop() || rel.remote_path;
      const localName = rel.local_path.split('/').pop() || rel.local_path;
      const arrow = directionArrow(rel.direction);
      const displayName = rel.name || `${remoteName} ${arrow} ${localName}`;

      return `
        <div class="relationship-card" data-id="${rel.id}" data-name="${escapeAttr(displayName)}">
          <div class="relationship-card-name">${escapeHtml(displayName)}</div>
          <div class="relationship-card-paths">${escapeHtml(rel.remote_path)} ${arrow} ${escapeHtml(rel.local_path)}</div>
        </div>
      `;
    }).join('');

    return `<div class="relationship-grid">${cards}</div>`;
  }

  // Override _bindShellEvents to add relationship card click handlers
  _bindShellEvents() {
    super._bindShellEvents();

    this.querySelectorAll('.relationship-card').forEach((card) => {
      card.addEventListener('click', () => {
        this._openTab(card.dataset.id, card.dataset.name);
      });
    });
  }

  // Override _openTab to attach relationship metadata to tabs
  _openTab(relationshipId, relationshipName) {
    super._openTab(relationshipId, relationshipName);
    // Attach relationship info to the newly created tab
    const tab = this._activeTab();
    if (tab) {
      tab.relationship_id = relationshipId;
      tab.relationship_name = relationshipName;
    }
    this._saveState();
  }

  // ---------------------------------------------------------------------------
  // Client-specific: drag-out to OS
  // ---------------------------------------------------------------------------

  _bindTabContentEvents(tabId) {
    super._bindTabContentEvents(tabId);

    const container = this.querySelector(`#tab-content-${tabId}`);
    if (!container) return;
    const tab = this._tabs.find((t) => t.id === tabId);
    if (!tab) return;

    // Make file entries draggable
    container.querySelectorAll('.file-entry').forEach((el) => {
      const entryType = parseInt(el.dataset.type, 10);
      if (entryType === ENTRY_TYPE_DIR) return;

      el.setAttribute('draggable', 'true');
      el.addEventListener('dragstart', (event) => {
        const entry = tab.entries.find((e) => e.name === el.dataset.name);
        if (!entry) return;

        const filePath = tab.path.replace(/\/$/, '') + '/' + entry.name;
        const fullUrl = `${window.location.origin}${this.fileUrl(filePath)}`;
        const mimeType = entry.content_type || 'application/octet-stream';

        event.dataTransfer.setData('DownloadURL', `${mimeType}:${entry.name}:${fullUrl}`);
        event.dataTransfer.setData('text/uri-list', fullUrl);
        event.dataTransfer.effectAllowed = 'copy';

        this.dispatchEvent(new CustomEvent('file-drag-start', {
          bubbles: true,
          detail: {
            entry,
            path: filePath,
            url: fullUrl,
            isDirectory: false,
          },
        }));
      });
    });
  }

  // Client-specific: "Open Locally" in preview actions
  _handlePreviewAction(action) {
    if (action === 'open-local') {
      const tab = this._activeTab();
      if (!tab || !tab.preview_entry) return;
      const filePath = tab.path.replace(/\/$/, '') + '/' + tab.preview_entry.name;
      fetch(`/api/v1/files/${tab.relationship_id}/open`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: filePath.replace(/^\//, '') }),
      });
      return;
    }
    super._handlePreviewAction(action);
  }
}

customElements.define('aeor-file-browser', AeorFileBrowser);
```

- [ ] **Step 2: Verify the client subclass is reasonable size**

Run: `wc -l aeordb-web-components/components/aeor-file-browser.js`
Expected: roughly 180-220 lines

- [ ] **Step 3: Commit**

```bash
cd /home/wyatt/Projects/aeordb-workspace/aeordb-web-components
git add components/aeor-file-browser.js
git commit -m "Rewrite client file browser as subclass of AeorFileBrowserBase"
```

---

### Task 3: Create Portal Subclass — `AeorFileBrowserPortal`

**Files:**
- Create: `aeordb-web-components/components/aeor-file-browser-portal.js`

- [ ] **Step 1: Create the portal subclass**

Create `aeordb-web-components/components/aeor-file-browser-portal.js`:

```javascript
'use strict';

import { AeorFileBrowserBase } from './aeor-file-browser-base.js';

export class AeorFileBrowserPortal extends AeorFileBrowserBase {
  connectedCallback() {
    super.connectedCallback();

    // Auto-open a tab if none were restored from localStorage
    if (!this._active_tab_id) {
      this._openTab('portal', 'Database');
    }
  }

  // ---------------------------------------------------------------------------
  // Abstract method implementations
  // ---------------------------------------------------------------------------

  async browse(path, limit, offset) {
    // AeorDB route is /files/{*path} — root requires %2F
    const filesPath = (path && path !== '/')
      ? `/files/${path}`
      : '/files/%2F';
    const response = await window.api(`${filesPath}?limit=${limit}&offset=${offset}`);
    if (!response.ok) throw new Error(`Browse failed: ${response.status}`);
    const data = await response.json();
    const items = data.items || [];
    return {
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
    if (!response.ok) throw new Error(`Upload failed: ${response.status}`);
  }

  async deletePath(path) {
    const response = await window.api(`/files${path}`, { method: 'DELETE' });
    if (!response.ok) throw new Error(`Delete failed: ${response.status}`);
  }

  async renamePath(fromPath, toPath) {
    const response = await window.api('/files/rename', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ from: fromPath, to: toPath }),
    });
    if (!response.ok) throw new Error(`Rename failed: ${response.status}`);
  }

  openNewTab() {
    // Portal has no relationship selector — just open a new tab at root
    this._openTab('portal', 'Database');
  }

  // ---------------------------------------------------------------------------
  // Hook overrides
  // ---------------------------------------------------------------------------

  rootLabel() {
    return 'Database';
  }

  // ---------------------------------------------------------------------------
  // Override: prevent closing the last tab
  // ---------------------------------------------------------------------------

  _closeTab(tabId) {
    if (this._tabs.length <= 1) return;
    super._closeTab(tabId);
  }

  render() {
    super.render();

    // Hide close button when only one tab remains
    if (this._tabs.length <= 1) {
      this.querySelectorAll('.tab-close').forEach((btn) => {
        btn.style.display = 'none';
      });
    }
  }

  // ---------------------------------------------------------------------------
  // Portal-specific: download button instead of drag-out
  // ---------------------------------------------------------------------------

  _handlePreviewAction(action) {
    if (action === 'download') {
      const tab = this._activeTab();
      if (!tab || !tab.preview_entry) return;
      const filePath = tab.path.replace(/\/$/, '') + '/' + tab.preview_entry.name;
      // Direct download via the file URL
      const link = document.createElement('a');
      link.href = this.fileUrl(filePath);
      link.download = tab.preview_entry.name;
      link.click();
      return;
    }
    if (action === 'download-zip') {
      this._downloadSelectedAsZip();
      return;
    }
    super._handlePreviewAction(action);
  }

  async _downloadSelectedAsZip() {
    const tab = this._activeTab();
    if (!tab) return;

    // Collect selected entries or all entries if none selected
    const paths = tab.entries
      .filter((entry) => entry._selected)
      .map((entry) => tab.path.replace(/\/$/, '') + '/' + entry.name);

    if (paths.length === 0) return;

    try {
      const response = await window.api('/files/download', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ paths }),
      });
      if (!response.ok) throw new Error(`Download failed: ${response.status}`);

      const blob = await response.blob();
      const link = document.createElement('a');
      link.href = URL.createObjectURL(blob);
      link.download = 'aeordb-download.zip';
      link.click();
      URL.revokeObjectURL(link.href);
    } catch (error) {
      if (window.aeorToast) {
        window.aeorToast('Download failed: ' + error.message, 'error');
      }
    }
  }
}

customElements.define('aeor-file-browser-portal', AeorFileBrowserPortal);
```

- [ ] **Step 2: Verify size**

Run: `wc -l aeordb-web-components/components/aeor-file-browser-portal.js`
Expected: roughly 140-170 lines

- [ ] **Step 3: Commit**

```bash
cd /home/wyatt/Projects/aeordb-workspace/aeordb-web-components
git add components/aeor-file-browser-portal.js
git commit -m "Add AeorFileBrowserPortal: portal subclass with direct /files/ API access"
```

---

### Task 4: Simplify Portal `files.mjs` and Update Asset Serving

**Files:**
- Rewrite: `aeordb-lib/src/portal/files.mjs`
- Modify: `aeordb-lib/src/server/portal_routes.rs`

Remove the entire fetch shim and monkey-patch system. Replace with a clean import of the portal subclass.

- [ ] **Step 1: Rewrite `files.mjs`**

Replace the entire contents of `aeordb-lib/src/portal/files.mjs` with:

```javascript
'use strict';

import '/system/portal/shared/components/aeor-file-browser-portal.js';

class AeorFiles extends HTMLElement {
  connectedCallback() {
    if (!this._initialized) {
      this._initialized = true;
      this.innerHTML = '<aeor-file-browser-portal></aeor-file-browser-portal>';
    }
  }
}

customElements.define('aeor-files', AeorFiles);
```

- [ ] **Step 2: Add include_str! for the base class and portal subclass**

In `aeordb-lib/src/server/portal_routes.rs`, add after the existing include_str! constants:

```rust
const PORTAL_SHARED_FILE_BROWSER_BASE_JS: &str = include_str!("../portal/shared/components/aeor-file-browser-base.js");
const PORTAL_SHARED_FILE_BROWSER_PORTAL_JS: &str = include_str!("../portal/shared/components/aeor-file-browser-portal.js");
```

Add the route matches in `portal_shared_asset`:

```rust
        "components/aeor-file-browser-base.js" => (PORTAL_SHARED_FILE_BROWSER_BASE_JS, "application/javascript; charset=utf-8"),
        "components/aeor-file-browser-portal.js" => (PORTAL_SHARED_FILE_BROWSER_PORTAL_JS, "application/javascript; charset=utf-8"),
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Expected: Compiles clean (proves all include_str! paths resolve through symlinks)

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/portal/files.mjs aeordb-lib/src/server/portal_routes.rs
git commit -m "Simplify portal files.mjs: remove fetch shim, use portal subclass directly"
```

---

### Task 5: Add ZIP Download Endpoint — `POST /files/download`

**Files:**
- Create: `aeordb-lib/src/server/download_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`
- Create: `aeordb-lib/spec/http/download_spec.rs`

- [ ] **Step 1: Write the download endpoint tests**

Create `aeordb-lib/spec/http/download_spec.rs`:

```rust
use std::sync::Arc;
use std::io::Read;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<aeordb::engine::StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
    (app, jwt_manager, engine, temp_dir)
}

fn bearer_token(jwt_manager: &JwtManager) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: "test-admin".to_string(),
        iss: "aeordb".to_string(),
        iat: now,
        exp: now + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: None,
    };
    let token = jwt_manager.create_token(&claims).expect("create token");
    format!("Bearer {}", token)
}

fn store_test_files(engine: &aeordb::engine::StorageEngine) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, "/docs/readme.md", b"# Hello", Some("text/markdown")).unwrap();
    ops.store_file(&ctx, "/docs/notes.txt", b"Some notes", Some("text/plain")).unwrap();
    ops.store_file(&ctx, "/images/logo.svg", b"<svg></svg>", Some("image/svg+xml")).unwrap();
}

#[tokio::test]
async fn download_zip_with_valid_paths() {
    let (app, jwt_manager, engine, _temp) = test_app();
    store_test_files(&engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({ "paths": ["/docs/readme.md", "/docs/notes.txt"] });
    let request = Request::builder()
        .method("POST")
        .uri("/files/download")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/zip"
    );

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let reader = std::io::Cursor::new(bytes.to_vec());
    let mut archive = zip::ZipArchive::new(reader).expect("valid ZIP");
    assert_eq!(archive.len(), 2);

    let mut readme = archive.by_name("docs/readme.md").expect("readme.md in ZIP");
    let mut content = String::new();
    readme.read_to_string(&mut content).unwrap();
    assert_eq!(content, "# Hello");
}

#[tokio::test]
async fn download_zip_skips_missing_paths() {
    let (app, jwt_manager, engine, _temp) = test_app();
    store_test_files(&engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({ "paths": ["/docs/readme.md", "/nonexistent.txt"] });
    let request = Request::builder()
        .method("POST")
        .uri("/files/download")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let reader = std::io::Cursor::new(bytes.to_vec());
    let archive = zip::ZipArchive::new(reader).expect("valid ZIP");
    assert_eq!(archive.len(), 1, "should only contain the valid file");
}

#[tokio::test]
async fn download_zip_empty_paths_returns_400() {
    let (app, jwt_manager, _engine, _temp) = test_app();
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({ "paths": [] });
    let request = Request::builder()
        .method("POST")
        .uri("/files/download")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn download_zip_includes_folder_contents() {
    let (app, jwt_manager, engine, _temp) = test_app();
    store_test_files(&engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({ "paths": ["/docs"] });
    let request = Request::builder()
        .method("POST")
        .uri("/files/download")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let reader = std::io::Cursor::new(bytes.to_vec());
    let archive = zip::ZipArchive::new(reader).expect("valid ZIP");
    assert_eq!(archive.len(), 2, "should include both files in /docs/");
}

#[tokio::test]
async fn download_zip_skips_system_paths() {
    let (app, jwt_manager, engine, _temp) = test_app();
    store_test_files(&engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({ "paths": ["/docs/readme.md", "/.system/config"] });
    let request = Request::builder()
        .method("POST")
        .uri("/files/download")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let reader = std::io::Cursor::new(bytes.to_vec());
    let archive = zip::ZipArchive::new(reader).expect("valid ZIP");
    assert_eq!(archive.len(), 1, "should skip .system/ path");
}

#[tokio::test]
async fn download_zip_requires_auth() {
    let (app, _jwt_manager, _engine, _temp) = test_app();

    let body = serde_json::json!({ "paths": ["/docs/readme.md"] });
    let request = Request::builder()
        .method("POST")
        .uri("/files/download")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 2: Create the download endpoint**

Create `aeordb-lib/src/server/download_routes.rs`:

```rust
use axum::{
    Extension,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::io::Write;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::directory_ops::{DirectoryOps, is_system_path};
use crate::engine::path_utils::normalize_path;

#[derive(Deserialize)]
pub struct DownloadRequest {
    pub paths: Vec<String>,
}

/// POST /files/download — bundle requested paths into a ZIP archive.
pub async fn download_zip(
    State(state): State<AppState>,
    Extension(_claims): Extension<TokenClaims>,
    Json(body): Json<DownloadRequest>,
) -> Response {
    if body.paths.is_empty() {
        return ErrorResponse::new("At least one path is required in the 'paths' array")
            .with_status(StatusCode::BAD_REQUEST)
            .into_response();
    }

    let ops = DirectoryOps::new(&state.engine);
    let mut zip_buffer = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buffer));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for raw_path in &body.paths {
            let normalized = normalize_path(raw_path);

            // Skip .system/ paths
            if is_system_path(&normalized) {
                skipped.push(raw_path.clone());
                continue;
            }

            // Try as file first
            match ops.read_file(&normalized) {
                Ok(data) => {
                    let zip_entry_name = normalized.trim_start_matches('/');
                    if zip_writer.start_file(zip_entry_name, options).is_ok() {
                        let _ = zip_writer.write_all(&data);
                    }
                }
                Err(crate::engine::errors::EngineError::NotFound(_)) => {
                    // Not a file — try as directory
                    if let Err(_) = add_directory_to_zip(&ops, &normalized, &mut zip_writer, options, &mut skipped) {
                        skipped.push(raw_path.clone());
                    }
                }
                Err(_) => {
                    skipped.push(raw_path.clone());
                }
            }
        }

        if let Err(error) = zip_writer.finish() {
            tracing::error!("Failed to finalize ZIP: {}", error);
            return ErrorResponse::new("Failed to create ZIP archive")
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    }

    let mut headers = vec![
        (header::CONTENT_TYPE, "application/zip".to_string()),
        (header::CONTENT_DISPOSITION, "attachment; filename=\"aeordb-download.zip\"".to_string()),
    ];

    if !skipped.is_empty() {
        headers.push((
            header::HeaderName::from_static("x-aeordb-skipped"),
            skipped.join(", "),
        ));
    }

    let headers: Vec<(header::HeaderName, String)> = headers
        .into_iter()
        .map(|(name, value)| (name, value))
        .collect();

    (StatusCode::OK, headers, zip_buffer).into_response()
}

fn add_directory_to_zip(
    ops: &DirectoryOps,
    dir_path: &str,
    zip_writer: &mut zip::ZipWriter<std::io::Cursor<&mut Vec<u8>>>,
    options: zip::write::SimpleFileOptions,
    skipped: &mut Vec<String>,
) -> Result<(), ()> {
    let entries = ops.list_directory(dir_path).map_err(|_| ())?;

    for entry in entries {
        let child_path = if dir_path == "/" {
            format!("/{}", entry.name)
        } else {
            format!("{}/{}", dir_path, entry.name)
        };

        let normalized = normalize_path(&child_path);

        if is_system_path(&normalized) {
            skipped.push(child_path);
            continue;
        }

        // Check entry type — 3 = directory, 2 = file
        if entry.entry_type == crate::engine::entry_type::EntryType::Directory.to_u8() {
            let _ = add_directory_to_zip(ops, &normalized, zip_writer, options, skipped);
        } else if entry.entry_type == crate::engine::entry_type::EntryType::FileRecord.to_u8() {
            if let Ok(data) = ops.read_file(&normalized) {
                let zip_entry_name = normalized.trim_start_matches('/');
                if zip_writer.start_file(zip_entry_name, options).is_ok() {
                    let _ = zip_writer.write_all(&data);
                }
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Register the module and route**

In `aeordb-lib/src/server/mod.rs`, add the module declaration alongside other route modules:

```rust
pub mod download_routes;
```

Register the route BEFORE the `/files/{*path}` wildcard (next to `/files/query`):

```rust
    .route("/files/download", post(download_routes::download_zip))
```

- [ ] **Step 4: Run the download tests**

Run: `cargo test --test download_spec 2>&1 | tail -15`
Expected: All 6 tests pass

- [ ] **Step 5: Run the full test suite**

Run: `cargo test 2>&1 | grep "FAILED" | grep "test " || echo "ALL TESTS PASS"`
Expected: ALL TESTS PASS

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/server/download_routes.rs aeordb-lib/src/server/mod.rs aeordb-lib/spec/http/download_spec.rs
git commit -m "Add POST /files/download ZIP streaming endpoint with tests"
```

---

### Task 6: Full Verification

**Files:** None (verification only)

- [ ] **Step 1: Run the complete Rust test suite**

Run: `cargo test 2>&1 | grep "test result:" | awk '{sum += $4} END {print "Total:", sum, "tests"}'`
Expected: All tests pass, total count increased by the new download tests

- [ ] **Step 2: Build and start the server**

Run: `cargo build && target/debug/aeordb start -D /tmp/claude/portal-test.aeordb &`

- [ ] **Step 3: Verify portal file browser works via Puppeteer**

Navigate to `http://localhost:6830/system/portal?page=files`
- Verify: auto-opens to root listing (no relationship selector)
- Verify: click "+" opens a new tab directly (no selector)
- Verify: close button hidden on single tab, visible on multiple tabs
- Verify: folder navigation works (click docs → see files)
- Verify: file preview works (click readme.md → see content)
- Verify: breadcrumbs navigate back to root

- [ ] **Step 4: Test ZIP download endpoint**

```bash
# Upload test files if needed
TOKEN=$(curl -s -X POST http://localhost:6830/auth/token -H 'Content-Type: application/json' -d '{"api_key":"YOUR_KEY"}' | jq -r .token)
curl -s -X POST http://localhost:6830/files/download -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -d '{"paths":["/docs"]}' -o /tmp/test-download.zip
unzip -l /tmp/test-download.zip
```

Expected: ZIP contains the files from /docs/

- [ ] **Step 5: Stop server, update TODO.md**

Add under completed:
```markdown
- [x] File Browser refactor (base class + portal/client subclasses, ZIP endpoint) — N tests
```

- [ ] **Step 6: Final commit**

```bash
git add .claude/TODO.md
git commit -m "Update TODO with file browser refactor completion"
```
