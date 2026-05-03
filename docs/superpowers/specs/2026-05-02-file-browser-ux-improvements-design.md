# File Browser UX Improvements

## Goal

Six improvements to the file browser: folder-first sorting, non-destructive loading states, delete button text color animation, navigation selection clearing, context menu cleanup, and a full cut/copy/paste system with keyboard shortcuts.

## 1. Folder-First Sorting

Client-side presentation rule: directories always sort before files, regardless of sort field. Within each group, the current sort field and order apply.

**Implementation:** In `_getVisibleEntries` (or the rendering path), partition entries into directories and files, sort each group independently, then concatenate directories first. The server-side `sort` parameter is unchanged.

## 2. Non-Destructive Loading State

Replace the "Loading..." text replacement with a non-destructive visual:
- Set `cursor: wait` on the listing container
- Reduce opacity to 0.5 on the existing listing content
- When results arrive, restore cursor and opacity
- No DOM destruction, no "Loading..." text, no visible "glitch"

**Applies to:** `_fetchListing` calls in `_updateTabContent` — the first call (loading=true) should apply the visual state instead of replacing innerHTML.

## 3. Delete Button Text Color Fade

When the long-press delete button is pressed, the text color should fade from red to white as progress increases.

**Implementation:** In `aeor-long-press-button._tick()`, interpolate the label color from the initial `--lpb-text` color to white based on `pct`. Use `color-mix(in srgb, white ${pct*100}%, var(--lpb-text))` or equivalent. Reset to original on cancel.

## 4. Navigation Clears Selection Bar

After `_navigateTo()` clears `tab.selectedEntries`, also call `_updateSelectionVisual(tab)` so the selection action bar hides immediately. Currently the toolbar persists visually until the next `_updateTabContent` cycle.

## 5. Context Menu Overhaul

Remove "Rename" entry. Add Cut, Copy, Paste with keyboard shortcut hints. Move Delete to bottom with separator. Delete from context menu is instant (no confirmation).

**Layout:**
```
Preview
Share
Cut              Ctrl+X
Copy             Ctrl+C
Paste            Ctrl+V    (only when clipboard has items)
───────────────────────────
Delete           Del       (instant, no double-opt-in)
```

- Hotkeys displayed right-aligned in muted text color
- Use `Cmd` instead of `Ctrl` on macOS (detect via `navigator.platform.includes('Mac')`)
- Paste entry hidden when sessionStorage clipboard is empty

## 6. Cut/Copy/Paste System

### 6a. Server: Copy File Endpoint

**New engine method:** `DirectoryOps::copy_file(ctx, from_path, to_path)` — reads the source FileRecord, creates a new FileRecord at the destination pointing to the same chunk hashes. No chunk data is duplicated (content-addressed dedup). Also handles directories recursively.

**New endpoint:** `POST /files/copy`
```json
Request:  { "paths": ["/src/a.png", "/src/b.png"], "destination": "/dst/" }
Response: { "copied": ["/dst/a.png", "/dst/b.png"] }
```

For directories in the paths list, recursively copy all contents.

### 6b. Client: sessionStorage Clipboard

Clipboard state stored in `sessionStorage` as JSON under key `aeordb-clipboard`:
```json
{ "mode": "copy" | "cut", "paths": ["/Pictures/a.png", "/Pictures/b.png"] }
```

### 6c. Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl+C` / `Cmd+C` | Copy selected entries to clipboard. Toast: "Files copied!" |
| `Ctrl+X` / `Cmd+X` | Cut selected entries to clipboard. Toast: "Files cut!" |
| `Ctrl+V` / `Cmd+V` | Paste from clipboard. Copy mode: `POST /files/copy`. Cut mode: move each file via `PATCH /files/{path}` then clear clipboard. Toast: "Files pasted!" |
| `Ctrl+Shift+V` / `Cmd+Shift+V` | Paste as symlinks. Creates symlinks at destination pointing to source paths. Toast: "Symlinks created!" |

### 6d. Action Bar Buttons

Added to the selection action bar after "Delete Selected", before "Download ZIP":
- **Cut** (secondary button) — same as Ctrl+X
- **Copy** (secondary button) — same as Ctrl+C
- **Paste** (secondary button) — only visible when clipboard has items, same as Ctrl+V

### 6e. Cut Visual Feedback

After cutting files, entries with paths matching the clipboard's cut list get reduced opacity (0.4) in the listing. This is a CSS class applied during rendering. Resets when clipboard is cleared (paste, or new copy/cut).

### 6f. Error Handling

- If paste fails for a specific file (e.g., destination already exists), show toast with error, continue pasting remaining files.
- For cut (move) operations, each file is moved individually. If a move fails, the file stays at its original location.
- After paste completes, refresh the listing and clear the clipboard.

## Files Affected

| File | Change |
|------|--------|
| `aeor-file-browser-base.js` | Folder sorting, loading state, selection clearing, context menu, keyboard shortcuts, clipboard state, cut visual, paste logic |
| `aeor-file-browser-portal.js` | Action bar buttons (Cut/Copy/Paste), `copy()` API call, `selectionActions` update |
| `aeor-long-press-button.js` | Text color fade in `_tick()` |
| `aeordb-lib/src/engine/directory_ops.rs` | `copy_file()`, `copy_directory()` methods |
| `aeordb-lib/src/server/engine_routes.rs` | `POST /files/copy` endpoint |
