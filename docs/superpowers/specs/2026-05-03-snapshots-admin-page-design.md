# Snapshots Admin Page

## Goal

Admin-only page for managing database snapshots: list, search, delete (single or batch), and restore HEAD to a selected snapshot. Clones the Keys page card layout and interaction patterns.

## Layout

Card-based layout matching the Keys page. Each snapshot is a card.

### Card Contents

- **Name** — bold, top of card
- **ID** — monospace, truncated, clipboard copy icon
- **Created** — formatted date
- **Status** — "current" green badge on newest snapshot, relative age for others (e.g., "3 days ago")

### Search Bar

Client-side filter by snapshot name. Same pattern as Keys page search.

### Selection

- Checkbox per card
- Multi-select allowed for **Delete** only
- Selection bar: "N selected" + long-press **Delete Selected** + "Clear Selection"
- **Restore is per-card only** — not in the selection bar. Each card has its own long-press Restore button.

### Per-Card Actions

- Long-press **Restore** button (orange → green, confirmed text "Restored!") — restores HEAD to this snapshot. Server creates auto-snapshot before restore as safety net.
- Long-press **Delete** button (gray/red text → "Deleted!")

### Mobile

Same responsive behavior as Keys — cards stack vertically, search stays at top, selection bar wraps.

## API Endpoints

All existing, no new endpoints needed:

| Method | Endpoint | Purpose |
|--------|----------|---------|
| `GET` | `/versions/snapshots` | List all snapshots |
| `DELETE` | `/versions/snapshots/{id_or_name}` | Delete a snapshot |
| `POST` | `/versions/snapshots/{id_or_name}/restore` | Restore HEAD to snapshot |

## Files

| File | Change |
|------|--------|
| Create: `aeordb-lib/src/portal/snapshots.mjs` | Snapshot management page (clone of keys.mjs pattern) |
| Modify: `aeordb-lib/src/portal/index.html` | Add "Snapshots" nav link between Keys and Settings, add page container div |
| Modify: `aeordb-lib/src/portal/app.mjs` | Import snapshots.mjs, add page routing |
| Modify: `aeordb-lib/src/server/portal_routes.rs` | Serve snapshots.mjs as portal asset |
