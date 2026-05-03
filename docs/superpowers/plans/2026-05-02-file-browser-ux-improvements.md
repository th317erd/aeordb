# File Browser UX Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Six UX improvements: folder-first sorting, non-destructive loading, delete text fade, navigation selection clearing, context menu overhaul with cut/copy/paste, and a server-side copy endpoint.

**Architecture:** Client-side changes in aeor-file-browser-base.js (sorting, loading, selection, keyboard shortcuts, clipboard), portal subclass (action bar buttons, API calls), long-press button (text fade). Server-side: new `copy_file` engine method + `POST /files/copy` endpoint. Clipboard state in sessionStorage.

**Tech Stack:** JavaScript (web components), Rust (axum, AeorDB engine)

---

### Task 1: Folder-First Sorting

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js` (lines 396-403)

- [ ] **Step 1: Update `_getVisibleEntries` to partition dirs before files**

In `aeor-file-browser-base.js`, replace the `_getVisibleEntries` method (lines 396-403):

```javascript
_getVisibleEntries(tab) {
  const live = this._showHidden
    ? tab.entries
    : tab.entries.filter((e) => !e.name.startsWith('.'));
  const deleted = tab._deletedEntries || [];
  const all = [...live, ...deleted];

  // Directories always sort before files
  const dirs = all.filter((e) => e.entry_type === ENTRY_TYPE_DIR);
  const files = all.filter((e) => e.entry_type !== ENTRY_TYPE_DIR);
  return [...dirs, ...files];
}
```

- [ ] **Step 2: Verify — select a file, hard-refresh, confirm folders appear above files regardless of sort column**

- [ ] **Step 3: Commit**

```bash
git add aeordb-web-components/components/aeor-file-browser-base.js
git commit -m "File browser: folders always sort before files"
```

---

### Task 2: Non-Destructive Loading State

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js` (lines 462-466, 1517-1527)

- [ ] **Step 1: Change `_fetchListing` to apply visual loading state instead of replacing content**

In `_fetchListing` (around line 1517), replace the first `_updateTabContent` call with a visual loading state:

```javascript
async _fetchListing() {
  if (this._refreshSuppressed) return;
  const tab = this._activeTab();
  if (!tab) return;

  tab.entries = [];
  tab.total = null;
  tab.loading_more = false;
  tab.loading = true;

  // Non-destructive loading: dim existing content instead of replacing with "Loading..."
  const container = this.querySelector(`#tab-content-${tab.id}`);
  const listingArea = container && container.querySelector('.tab-listing-area');
  const listing = listingArea && listingArea.querySelector('.tab-listing');
  if (listing) {
    listing.style.opacity = '0.5';
    listing.style.pointerEvents = 'none';
    listing.style.cursor = 'wait';
  }

  try {
```

Then after the data is loaded (around line 1550 where `tab.loading = false`), restore the visual state:

```javascript
  tab.loading = false;
  this._updateTabContent(tab.id);
  this._attachScrollListener();
```

No change needed here — `_updateTabContent` will replace the listing content, which naturally removes the opacity/cursor styles since the `.tab-listing` innerHTML is replaced.

- [ ] **Step 2: Remove the "Loading..." text from `_renderListingContent`**

In `_renderListingContent` (line 465-466), change the loading branch to return empty string so that if `_updateTabContent` IS called during loading (e.g., first render), it doesn't show "Loading...":

```javascript
if (tab.loading) {
  return '';
}
```

- [ ] **Step 3: Verify — navigate between folders, confirm no "Loading..." flash and existing content dims briefly**

- [ ] **Step 4: Commit**

```bash
git commit -am "File browser: non-destructive loading state (dim + wait cursor)"
```

---

### Task 3: Delete Button Text Color Fade

**Files:**
- Modify: `aeordb-web-components/components/aeor-long-press-button.js` (lines 146-155)

- [ ] **Step 1: Add text color interpolation in `_tick()`**

In `_tick()` (line 146), after the background opacity line, add text color interpolation. Find:

```javascript
this._btn.style.backgroundColor = `color-mix(in srgb, ${this._originalBg} ${Math.round(bgOpacity * 100)}%, transparent)`;
```

Add after it:

```javascript
// Fade text color from original to white as progress increases
const label = this.querySelector('.lpb-label');
if (label) {
  label.style.color = `color-mix(in srgb, white ${Math.round(pct * 100)}%, var(--lpb-text, var(--text, #e6edf3)))`;
}
```

- [ ] **Step 2: Reset text color in `_reset()`**

In `_reset()` (line 190), add after the button border reset:

```javascript
const label = this.querySelector('.lpb-label');
if (label) label.style.color = '';
```

- [ ] **Step 3: Verify — press and hold a delete button, confirm text fades from red to white**

- [ ] **Step 4: Commit**

```bash
git commit -am "Long-press button: fade text color to white during press"
```

---

### Task 4: Navigation Clears Selection Bar

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js` (lines 1406-1419)

- [ ] **Step 1: Add `_updateSelectionVisual` call in `_navigateTo`**

In `_navigateTo` (line 1406), after `tab.selectedEntries.clear()` and before `this._saveState()`, add:

```javascript
this._updateSelectionVisual(tab);
```

The full method becomes:

```javascript
_navigateTo(path) {
  const tab = this._activeTab();
  if (!tab) return;
  (tab._gridBlobUrls || []).forEach(u => URL.revokeObjectURL(u));
  tab._gridBlobUrls = [];
  tab.path = path;
  tab.preview_entry = null;
  tab.selectedEntries.clear();
  tab.lastSelectedAnchor = null;
  this._updateSelectionVisual(tab);
  this._saveState();
  this._updateTabBarLabel(tab);
  this._fetchListing();
}
```

- [ ] **Step 2: Verify — select a file, double-click a folder to navigate in, confirm action bar clears immediately**

- [ ] **Step 3: Commit**

```bash
git commit -am "File browser: clear selection bar on navigation"
```

---

### Task 5: Context Menu Overhaul

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js` (lines 2815-2863)

- [ ] **Step 1: Rewrite `_showContextMenu` with new layout**

Replace the `_showContextMenu` method (lines 2815-2863):

```javascript
_showContextMenu(x, y, entry) {
  const existing = this.querySelector('.context-menu');
  if (existing) existing.remove();

  const isMac = navigator.platform.includes('Mac');
  const mod = isMac ? 'Cmd' : 'Ctrl';
  const clipboard = this._getClipboard();
  const tab = this._activeTab();

  const menu = document.createElement('div');
  menu.className = 'context-menu';
  menu.style.left = x + 'px';
  menu.style.top = y + 'px';
  menu.innerHTML = `
    <div class="context-menu-item" data-context="preview">Preview</div>
    ${this._hasPermission('y') ? `<div class="context-menu-item" data-context="share">Share</div>` : ''}
    ${this._hasPermission('u') ? `<div class="context-menu-item" data-context="cut">Cut <span class="context-menu-hotkey">${mod}+X</span></div>` : ''}
    <div class="context-menu-item" data-context="copy">Copy <span class="context-menu-hotkey">${mod}+C</span></div>
    ${clipboard ? `<div class="context-menu-item" data-context="paste">Paste <span class="context-menu-hotkey">${mod}+V</span></div>` : ''}
    ${this._hasPermission('d') ? '<hr style="border:none;border-top:1px solid var(--border,#30363d);margin:4px 0;">' : ''}
    ${this._hasPermission('d') ? `<div class="context-menu-item context-menu-danger" data-context="delete-instant">Delete <span class="context-menu-hotkey">Del</span></div>` : ''}
  `;

  this.appendChild(menu);

  // Clamp to viewport
  const rect = menu.getBoundingClientRect();
  if (rect.right > window.innerWidth) menu.style.left = (x - rect.width) + 'px';
  if (rect.bottom > window.innerHeight) menu.style.top = (y - rect.height) + 'px';

  menu.querySelectorAll('.context-menu-item').forEach((item) => {
    item.addEventListener('click', () => {
      menu.remove();
      const action = item.dataset.context;
      if (action === 'preview') {
        if (tab) {
          tab.preview_entry = entry;
          tab.preview_component = null;
        }
        this._loadPreview();
      } else if (action === 'share') {
        if (tab) {
          let filePath = tab.path.replace(/\/$/, '') + '/' + entry.name;
          if (entry.entry_type === ENTRY_TYPE_DIR) filePath += '/';
          this._showShareModal([filePath]);
        }
      } else if (action === 'cut') {
        this._cutSelected(entry);
      } else if (action === 'copy') {
        this._copySelected(entry);
      } else if (action === 'paste') {
        this._pasteClipboard();
      } else if (action === 'delete-instant') {
        this._deleteInstant(entry);
      }
    });
  });

  const closeMenu = (event) => {
    if (!menu.contains(event.target)) {
      menu.remove();
      document.removeEventListener('click', closeMenu);
    }
  };
  setTimeout(() => document.addEventListener('click', closeMenu), 0);
}
```

- [ ] **Step 2: Add CSS for hotkey labels**

Add to `aeordb-web-components/styles/components.css` after the existing `.context-menu-danger` rule:

```css
.context-menu-hotkey {
  float: right;
  margin-left: 24px;
  color: var(--text-muted);
  font-size: 11px;
}
```

- [ ] **Step 3: Add `_deleteInstant` method for context menu delete (no confirmation)**

Add to `aeor-file-browser-base.js` near the other delete methods:

```javascript
async _deleteInstant(entry) {
  const tab = this._activeTab();
  if (!tab) return;
  const filePath = tab.path.replace(/\/$/, '') + '/' + entry.name;
  try {
    await this.deletePath(filePath);
    this._fetchListing();
  } catch (error) {
    if (window.aeorToast) window.aeorToast('Delete failed: ' + error.message, 'error');
  }
}
```

- [ ] **Step 4: Verify — right-click a file, confirm new menu layout with hotkeys, Delete is at bottom with separator, delete works instantly**

- [ ] **Step 5: Commit**

```bash
git commit -am "Context menu: cut/copy/paste entries, hotkey hints, instant delete with separator"
```

---

### Task 6: Server-Side Copy Endpoint

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`
- Modify: `aeordb-lib/src/server/engine_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`

- [ ] **Step 1: Add `copy_file` method to `DirectoryOps`**

In `directory_ops.rs`, add after `rename_file` (around line 2052):

```rust
/// Copy a file to a new path. Reuses existing chunk hashes (no data duplication).
pub fn copy_file(
  &self,
  ctx: &RequestContext,
  from_path: &str,
  to_path: &str,
) -> EngineResult<FileRecord> {
  let from_normalized = normalize_path(from_path);
  let to_normalized = normalize_path(to_path);

  if from_normalized == "/" || to_normalized == "/" {
    return Err(EngineError::InvalidInput("Cannot copy root path".to_string()));
  }
  if from_normalized == to_normalized {
    return Err(EngineError::InvalidInput("Source and destination are the same".to_string()));
  }
  if is_system_path(&from_normalized) || is_system_path(&to_normalized) {
    return Err(EngineError::InvalidInput("Cannot copy system paths".to_string()));
  }

  let algo = self.engine.hash_algo();
  let hash_length = algo.hash_length();

  // Read the source FileRecord
  let from_key = file_path_hash(&from_normalized, &algo)?;
  let source_record = match self.engine.get_entry(&from_key)? {
    Some((header, _key, value)) => FileRecord::deserialize(&value, hash_length, header.entry_version)?,
    None => return Err(EngineError::NotFound(from_normalized)),
  };

  // Use restore_file_from_record which already handles all 3 keys + parent dirs
  self.restore_file_from_record(ctx, &to_normalized, &source_record)?;

  // Read back the new record to return it
  let to_key = file_path_hash(&to_normalized, &algo)?;
  match self.engine.get_entry(&to_key)? {
    Some((header, _key, value)) => Ok(FileRecord::deserialize(&value, hash_length, header.entry_version)?),
    None => Err(EngineError::NotFound(to_normalized)),
  }
}

/// Recursively copy a path (file or directory) to a new location.
pub fn copy_path(
  &self,
  ctx: &RequestContext,
  from_path: &str,
  to_path: &str,
) -> EngineResult<Vec<String>> {
  let from_normalized = normalize_path(from_path);
  let to_normalized = normalize_path(to_path);
  let mut copied = Vec::new();

  // Check if source is a directory
  let algo = self.engine.hash_algo();
  let dir_key = crate::engine::directory_ops::directory_path_hash(&from_normalized, &algo)?;
  if self.engine.has_entry(&dir_key)? {
    // It's a directory — create destination dir and recurse
    let _ = self.create_directory(ctx, &to_normalized);
    let children = self.list_directory(&from_normalized)?;
    for child in &children {
      let child_from = format!("{}/{}", from_normalized.trim_end_matches('/'), child.name);
      let child_to = format!("{}/{}", to_normalized.trim_end_matches('/'), child.name);
      let sub_copied = self.copy_path(ctx, &child_from, &child_to)?;
      copied.extend(sub_copied);
    }
    return Ok(copied);
  }

  // It's a file
  self.copy_file(ctx, &from_normalized, &to_normalized)?;
  copied.push(to_normalized);
  Ok(copied)
}
```

- [ ] **Step 2: Add `POST /files/copy` endpoint**

In `engine_routes.rs`, add the handler and request struct:

```rust
#[derive(Debug, Deserialize)]
pub struct CopyRequest {
  pub paths: Vec<String>,
  pub destination: String,
}

pub async fn copy_files(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CopyRequest>,
) -> Response {
  let dest_normalized = crate::engine::path_utils::normalize_path(&payload.destination);

  if is_system_path(&dest_normalized) {
    return ErrorResponse::new("Not found")
      .with_status(StatusCode::NOT_FOUND)
      .into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let ops = DirectoryOps::new(&state.engine);
  let mut copied = Vec::new();
  let mut errors = Vec::new();

  for path in &payload.paths {
    let from_normalized = crate::engine::path_utils::normalize_path(path);
    let name = crate::engine::path_utils::file_name(&from_normalized)
      .unwrap_or("").to_string();
    let to_path = format!("{}/{}", dest_normalized.trim_end_matches('/'), name);

    match ops.copy_path(&ctx, &from_normalized, &to_path) {
      Ok(paths) => copied.extend(paths),
      Err(error) => errors.push(format!("{}: {}", from_normalized, error)),
    }
  }

  let mut response = serde_json::json!({ "copied": copied });
  if !errors.is_empty() {
    response["errors"] = serde_json::json!(errors);
  }

  (StatusCode::OK, Json(response)).into_response()
}
```

- [ ] **Step 3: Register the route in `mod.rs`**

In `server/mod.rs`, add before the `/files/{*path}` wildcard (near the other `/files/` routes):

```rust
.route("/files/copy", post(engine_routes::copy_files))
```

- [ ] **Step 4: Build and verify**

```bash
cargo build --release 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs aeordb-lib/src/server/engine_routes.rs aeordb-lib/src/server/mod.rs
git commit -m "Add POST /files/copy endpoint for content-addressed file copy"
```

---

### Task 7: Client Clipboard + Keyboard Shortcuts + Action Bar

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js`
- Modify: `aeordb-web-components/components/aeor-file-browser-portal.js`

- [ ] **Step 1: Add clipboard helper methods to base class**

In `aeor-file-browser-base.js`, add near the selection methods:

```javascript
_getClipboard() {
  try {
    const raw = sessionStorage.getItem('aeordb-clipboard');
    return raw ? JSON.parse(raw) : null;
  } catch (_) { return null; }
}

_setClipboard(mode, paths) {
  sessionStorage.setItem('aeordb-clipboard', JSON.stringify({ mode, paths }));
}

_clearClipboard() {
  sessionStorage.removeItem('aeordb-clipboard');
}

_cutSelected(contextEntry) {
  const tab = this._activeTab();
  if (!tab) return;
  const paths = tab.selectedEntries.size > 0
    ? [...tab.selectedEntries]
    : [tab.path.replace(/\/$/, '') + '/' + contextEntry.name];
  this._setClipboard('cut', paths);
  this._updateTabContent(tab.id); // re-render to show cut visual
  if (window.aeorToast) window.aeorToast('Files cut!', 'success');
}

_copySelected(contextEntry) {
  const tab = this._activeTab();
  if (!tab) return;
  const paths = tab.selectedEntries.size > 0
    ? [...tab.selectedEntries]
    : [tab.path.replace(/\/$/, '') + '/' + contextEntry.name];
  this._setClipboard('copy', paths);
  if (window.aeorToast) window.aeorToast('Files copied!', 'success');
}

async _pasteClipboard() {
  const clipboard = this._getClipboard();
  if (!clipboard || !clipboard.paths.length) return;
  const tab = this._activeTab();
  if (!tab) return;

  try {
    if (clipboard.mode === 'copy') {
      await this._pasteAsCopy(clipboard.paths, tab.path);
    } else {
      await this._pasteAsMove(clipboard.paths, tab.path);
    }
    this._clearClipboard();
    this._fetchListing();
    if (window.aeorToast) window.aeorToast('Files pasted!', 'success');
  } catch (error) {
    if (window.aeorToast) window.aeorToast('Paste failed: ' + error.message, 'error');
  }
}

async _pasteAsSymlinks() {
  const clipboard = this._getClipboard();
  if (!clipboard || !clipboard.paths.length) return;
  const tab = this._activeTab();
  if (!tab) return;

  let errors = 0;
  for (const srcPath of clipboard.paths) {
    const name = srcPath.split('/').pop();
    const linkPath = tab.path.replace(/\/$/, '') + '/' + name;
    try {
      await this._createSymlink(linkPath, srcPath);
    } catch (_) { errors++; }
  }
  this._clearClipboard();
  this._fetchListing();
  if (errors > 0) {
    if (window.aeorToast) window.aeorToast(`${errors} symlink(s) failed`, 'error');
  } else {
    if (window.aeorToast) window.aeorToast('Symlinks created!', 'success');
  }
}

// Abstract methods for subclass implementation
async _pasteAsCopy(paths, destination) {
  throw new Error('_pasteAsCopy must be implemented by subclass');
}
async _pasteAsMove(paths, destination) {
  throw new Error('_pasteAsMove must be implemented by subclass');
}
async _createSymlink(path, target) {
  throw new Error('_createSymlink must be implemented by subclass');
}
```

- [ ] **Step 2: Add keyboard shortcuts (Ctrl+C, Ctrl+X, Ctrl+V, Ctrl+Shift+V)**

In `_bindKeyboardAndControls` (line 1209), inside the `keydownHandler` function, add after the Escape handler:

```javascript
} else if ((event.ctrlKey || event.metaKey) && event.key === 'c' && !event.shiftKey) {
  if (tab.selectedEntries.size > 0) {
    event.preventDefault();
    this._setClipboard('copy', [...tab.selectedEntries]);
    if (window.aeorToast) window.aeorToast('Files copied!', 'success');
  }
} else if ((event.ctrlKey || event.metaKey) && event.key === 'x') {
  if (tab.selectedEntries.size > 0) {
    event.preventDefault();
    this._setClipboard('cut', [...tab.selectedEntries]);
    this._updateTabContent(tab.id);
    if (window.aeorToast) window.aeorToast('Files cut!', 'success');
  }
} else if ((event.ctrlKey || event.metaKey) && event.key === 'v' && event.shiftKey) {
  event.preventDefault();
  this._pasteAsSymlinks();
} else if ((event.ctrlKey || event.metaKey) && event.key === 'v') {
  event.preventDefault();
  this._pasteClipboard();
} else if (event.key === 'Delete') {
  if (tab.selectedEntries.size > 0) {
    this._deleteSelected();
  }
}
```

- [ ] **Step 3: Add cut visual feedback in `_renderListRow`**

In `_renderListRow` (line 516), after the `isDeleted` variable, add:

```javascript
const clipboard = this._getClipboard();
const isCut = clipboard && clipboard.mode === 'cut' &&
  clipboard.paths.some((p) => p.endsWith('/' + entry.name));
```

Then update `rowStyle` to include cut opacity:

```javascript
const rowStyle = isDeleted
  ? 'style="opacity:0.6;background:rgba(248,81,73,0.06);"'
  : isCut
    ? 'style="opacity:0.4;"'
    : '';
```

- [ ] **Step 4: Add action bar buttons in portal `selectionActions`**

In `aeor-file-browser-portal.js`, update `selectionActions`:

```javascript
selectionActions(tab) {
  const clipboard = this._getClipboard();
  return `
    <button class="secondary small selection-cut">Cut</button>
    <button class="secondary small selection-copy">Copy</button>
    ${clipboard ? '<button class="secondary small selection-paste">Paste</button>' : ''}
    <button class="primary small selection-download-zip">Download ZIP</button>
  `;
}
```

- [ ] **Step 5: Implement portal API methods and bind action bar buttons**

In `aeor-file-browser-portal.js`, add the API methods:

```javascript
async _pasteAsCopy(paths, destination) {
  const response = await window.api('/files/copy', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ paths, destination }),
  });
  if (!response.ok) {
    const err = await response.json().catch(() => ({ error: 'Copy failed' }));
    throw new Error(err.error || `HTTP ${response.status}`);
  }
}

async _pasteAsMove(paths, destination) {
  for (const srcPath of paths) {
    const name = srcPath.split('/').pop();
    const toPath = destination.replace(/\/$/, '') + '/' + name;
    await this.renamePath(srcPath, toPath);
  }
}

async _createSymlink(path, target) {
  const response = await window.api(`/files${path}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json', 'X-Aeor-Symlink': target },
  });
  if (!response.ok) throw new Error(`Symlink failed: ${response.status}`);
}
```

Update `_bindSelectionBarExtra`:

```javascript
_bindSelectionBarExtra(selectionBar, tab) {
  const zipBtn = selectionBar.querySelector('.selection-download-zip');
  if (zipBtn) {
    zipBtn.addEventListener('click', () => this._downloadSelectedAsZip());
  }
  const shareBtn = selectionBar.querySelector('.selection-share');
  if (shareBtn) {
    shareBtn.addEventListener('click', () => {
      const paths = [...tab.selectedEntries];
      if (paths.length > 0) this._showShareModal(paths);
    });
  }
  const cutBtn = selectionBar.querySelector('.selection-cut');
  if (cutBtn) {
    cutBtn.addEventListener('click', () => {
      this._setClipboard('cut', [...tab.selectedEntries]);
      this._updateTabContent(tab.id);
      if (window.aeorToast) window.aeorToast('Files cut!', 'success');
    });
  }
  const copyBtn = selectionBar.querySelector('.selection-copy');
  if (copyBtn) {
    copyBtn.addEventListener('click', () => {
      this._setClipboard('copy', [...tab.selectedEntries]);
      if (window.aeorToast) window.aeorToast('Files copied!', 'success');
    });
  }
  const pasteBtn = selectionBar.querySelector('.selection-paste');
  if (pasteBtn) {
    pasteBtn.addEventListener('click', () => this._pasteClipboard());
  }
}
```

- [ ] **Step 6: Check symlink creation endpoint**

Check if `PUT /files/{path}` with `X-Aeor-Symlink` header already creates symlinks. If not, the `_createSymlink` method needs to use the existing `store_symlink` path. Read `engine_store_file` in `engine_routes.rs` to confirm symlink support. If it uses a different mechanism, update `_createSymlink` accordingly.

- [ ] **Step 7: Build, rebuild server, verify full flow**

```bash
cargo clean -p aeordb && cargo build --release
# Kill and restart server
# Test: select files, Ctrl+C, navigate to another folder, Ctrl+V
# Test: select files, Ctrl+X, verify dimmed, navigate, Ctrl+V
# Test: right-click, Cut/Copy/Paste from context menu
# Test: action bar Cut/Copy/Paste buttons
```

- [ ] **Step 8: Commit**

```bash
git commit -am "Cut/copy/paste: keyboard shortcuts, context menu, action bar, sessionStorage clipboard"
```

---

### Task 8: Paste Visibility in Toolbar

**Files:**
- Modify: `aeordb-web-components/components/aeor-file-browser-base.js`

- [ ] **Step 1: Show/hide paste button based on clipboard state**

The paste button in the action bar is rendered in `selectionActions` which is called during toolbar construction. Since the toolbar is only built once, the paste button's visibility needs to be toggled in `_updateSelectionVisual`. Add to `_updateSelectionVisual`, inside the `tab.selectedEntries.size > 0` branch:

```javascript
const pasteBtn = leftSlot.querySelector('.selection-paste');
if (pasteBtn) {
  pasteBtn.style.display = this._getClipboard() ? '' : 'none';
}
```

- [ ] **Step 2: Commit**

```bash
git commit -am "Action bar: toggle paste button visibility based on clipboard state"
```
