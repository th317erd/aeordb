# File Browser Component Refactor — Design Spec

**Date:** 2026-04-21
**Status:** Approved

## Problem

The shared file browser component (`aeor-file-browser.js`) is a 900-line monolith built around the client app's sync/relationship model. Integrating it into the DB portal required a fetch shim (~90 lines intercepting `window.fetch`) and prototype monkey-patches (~50 lines overriding methods at runtime). This is fragile, hard to maintain, and will break as the component evolves.

## Approach

Split the component into a base class with abstract data-access methods, and two subclasses — one for the client app (sync relationships, native drag-out) and one for the portal (direct `/files/` API, ZIP download). No fetch shims, no monkey-patching.

## Design

### 1. Base Class — `AeorFileBrowserBase`

**File:** `aeordb-web-components/components/aeor-file-browser-base.js`

A plain web component (`extends HTMLElement`) providing all shared file browsing functionality.

**What it owns:**
- Tab management: `_openTab(id, name)`, `_switchTab(tabId)`, `_closeTab(tabId)`, `_saveState()`, `_loadState()`
- Directory listing + pagination: renders list/grid views, infinite scroll, calls `browse()` with increasing offsets
- Navigation: breadcrumbs, click-to-enter-folder
- Preview panel: loads preview components dynamically, resize handle, metadata display
- Upload: drop zone detection, iterates dropped files and calls `upload()` for each
- View toggle: list/grid mode switching
- Rendering: `render()`, `_renderTabBar()`, `_renderDirectoryViewFor()`, `_renderBreadcrumbs()`, all event binding
- State persistence: localStorage save/restore of tabs, active tab, view modes

**Abstract methods subclasses MUST implement:**
```javascript
async browse(path, limit, offset)    // → { entries: [...], total: N }
fileUrl(path)                         // → string URL to fetch/display a file
async upload(path, body, contentType)
async deletePath(path)
async renamePath(fromPath, toPath)
openNewTab()                          // → what happens when "+" is clicked
```

**What it does NOT own:**
- Any `fetch()` calls — all data access goes through abstract methods
- Relationship/sync concepts — not in the vocabulary
- Drag-out to OS — subclass concern
- Download/ZIP — subclass concern

### 2. Client Subclass — `AeorFileBrowserClient`

**File:** `aeordb-web-components/components/aeor-file-browser.js` (replaces current monolith)

Extends `AeorFileBrowserBase`.

**What it adds:**
- `connectedCallback()` — calls super, then `_fetchRelationships()` to load sync configs
- `openNewTab()` — sets `_active_tab_id = null`, renders the relationship selector interstitial
- `_fetchRelationships()` — calls `GET /api/v1/sync`, stores results, re-renders if on selector screen
- `_renderRelationshipSelector()` — card grid for picking a sync relationship
- Relationship card click → calls `this._openTab(rel.id, rel.name)`
- `browse(path, limit, offset)` — calls `/api/v1/browse/{relationship_id}/{path}?limit=N&offset=M`
- `fileUrl(path)` — returns `/api/v1/files/{relationship_id}/{path}`
- `upload(path, body, contentType)` — `PUT /api/v1/files/{relationship_id}/{path}`
- `deletePath(path)` — `DELETE /api/v1/files/{relationship_id}/{path}`
- `renamePath(from, to)` — `POST /api/v1/files/{relationship_id}/rename`
- Sync badges on entries (synced/pending/error dots)
- Drag-out to OS — `DownloadURL` + `file-drag-start` event
- "Open Locally" button in preview panel

The active tab carries `relationship_id` — set when opened from the selector. The base class tab structure has `id`, `name`, `path`, `entries`, etc. The client subclass extends tab objects with `relationship_id` and `relationship_name`.

### 3. Portal Subclass — `AeorFileBrowserPortal`

**File:** `aeordb-web-components/components/aeor-file-browser-portal.js`

Extends `AeorFileBrowserBase`.

**What it adds:**
- `connectedCallback()` — calls super, auto-opens a tab if none restored from localStorage
- `openNewTab()` — directly calls `this._openTab('portal', 'Database')` (no interstitial)
- `_closeTab(tabId)` — guards against closing the last tab, calls super otherwise
- Render override — hides close button when only one tab remains
- `browse(path, limit, offset)` — calls `/files/{path}?limit=N&offset=M` via `window.api()`, transforms response (`items` → `entries`)
- `fileUrl(path)` — returns `/files/{path}`
- `upload(path, body, contentType)` — `PUT /files/{path}` via `window.api()`
- `deletePath(path)` — `DELETE /files/{path}` via `window.api()`
- `renamePath(from, to)` — `POST /files/rename` via `window.api()`
- Download button in preview panel (single file — direct download via `fileUrl()`)
- Multi-select download — calls `POST /files/download` with selected paths, receives streamed ZIP

No fetch shim. No monkey-patching. The portal subclass talks directly to AeorDB's `/files/` API.

**Portal `files.mjs` becomes trivial:**
```javascript
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

### 4. Server-Side ZIP Endpoint

**`POST /files/download`** — accepts a JSON body with paths, streams a ZIP archive back.

**Request:**
```json
{
  "paths": ["/docs/readme.md", "/docs/notes.txt", "/images/logo.svg"]
}
```

**Response:** `200 OK` with `Content-Type: application/zip`, `Content-Disposition: attachment; filename="aeordb-download.zip"`, body is streamed ZIP data.

**Behavior:**
- Resolves each path through the normal file access pipeline (auth, permissions, .system/ blocking)
- Skips paths that don't exist or that the user lacks permission for (doesn't fail the whole request)
- Preserves directory structure in the ZIP (e.g. `/docs/readme.md` becomes `docs/readme.md` in the archive)
- Folders in the paths list include their contents recursively
- Response header `X-AeorDB-Skipped` lists any paths that were skipped (for debugging)
- Size limit: refuse if total uncompressed size exceeds a configurable threshold (default 1GB) — returns 413

**Implementation:** Uses the `zip` crate (already a dependency for MS Office parsing). Iterates paths, reads file data from the engine, writes entries to a `ZipWriter` wrapping the response body.

**Why POST not GET:** The paths list can be arbitrarily long and contain special characters. POST body is cleaner than query parameters.

### 5. Drag and Drop

**Base class handles:**
- Drop-to-upload (files from OS → browser) — both environments support this, base class calls `this.upload(path, body, contentType)` which the subclass implements

**Client subclass adds:**
- Drag-out to OS — sets `DownloadURL` on drag, emits `file-drag-start` event. Works because the client has local file system access.

**Portal subclass adds:**
- No drag-out (no local filesystem access)
- Download button in preview panel for single files
- Multi-select "Download" button that triggers `POST /files/download` for ZIP archive

## Testing Strategy

**Portal subclass integration (Rust tests):**
- `POST /files/download` with valid paths — returns ZIP with correct entries
- `POST /files/download` with mixed valid/invalid paths — returns ZIP with valid files, skips invalid
- `POST /files/download` with empty paths array — returns 400
- `POST /files/download` with folder path — includes folder contents recursively
- `POST /files/download` with .system/ paths — paths are silently skipped
- `POST /files/download` exceeding size limit — returns 413
- `POST /files/download` without auth — returns 401

**Component refactor (manual verification via Puppeteer):**
- Portal: auto-opens tab, "+" opens new tab directly, last tab can't close, browse/navigate/preview work
- Client: relationship selector appears on "+", sync badges render, drag-out works

**Regression:**
- Full `cargo test` suite passes
- All existing portal pages still work (Dashboard, Users, Groups)

## Out of Scope

- Client-side ZIP (not feasible — portal is a remote web UI)
- Adapter/`setAdapter()` pattern (replaced by subclass approach)
- Streaming upload progress bars
- File search/filter within the browser
