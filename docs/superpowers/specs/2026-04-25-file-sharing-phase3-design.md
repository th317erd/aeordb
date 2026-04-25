# File Sharing — Phase 3 Design Spec

**Date:** 2026-04-25
**Status:** Approved
**Phase:** 3 of 3 (Email notifications + Settings page)

## Overview

Send email notifications when files are shared. Add a Settings page to the portal for configuring email providers (SMTP and OAuth). Store email config in `/.system/email-config.json`. Make email a required field on users.

---

## User Model Change

Add `email: String` to the `User` struct. Required, not unique (allows bot users to share an admin's email).

### Affected Code

- `User` struct in `user.rs` — add `pub email: String`
- `User::new()` — add `email` parameter
- `POST /system/users` (admin_routes) — require `email` field in request body
- `store_user` / `update_user` in `system_store.rs` — no change needed (serializes full struct)
- All test helpers that create `User::new(username, password)` — add email param
- Portal Users page — add email field to Create User form, show email in user list

---

## Email Configuration

Stored at `/.system/email-config.json`, readable/writable only by root.

### SMTP Config

```json
{
  "provider": "smtp",
  "host": "smtp.example.com",
  "port": 587,
  "username": "noreply@example.com",
  "password": "secret",
  "from_address": "noreply@example.com",
  "from_name": "AeorDB",
  "tls": "starttls"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `provider` | string | Yes | `"smtp"` |
| `host` | string | Yes | SMTP server hostname |
| `port` | integer | Yes | SMTP port (587 for STARTTLS, 465 for TLS) |
| `username` | string | Yes | SMTP auth username |
| `password` | string | Yes | SMTP auth password |
| `from_address` | string | Yes | Sender email address |
| `from_name` | string | No | Sender display name (default: "AeorDB") |
| `tls` | string | No | `"starttls"`, `"tls"`, or `"none"` (default: `"starttls"`) |

### OAuth Config

```json
{
  "provider": "oauth",
  "oauth_provider": "gmail",
  "client_id": "...",
  "client_secret": "...",
  "refresh_token": "...",
  "from_address": "noreply@example.com",
  "from_name": "AeorDB"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `provider` | string | Yes | `"oauth"` |
| `oauth_provider` | string | Yes | `"gmail"`, `"outlook"`, or `"custom"` |
| `client_id` | string | Yes | OAuth client ID |
| `client_secret` | string | Yes | OAuth client secret |
| `refresh_token` | string | Yes | OAuth refresh token (long-lived) |
| `from_address` | string | Yes | Sender email address |
| `from_name` | string | No | Sender display name (default: "AeorDB") |
| `token_url` | string | No | Custom token endpoint (required for `"custom"` provider) |
| `send_url` | string | No | Custom send endpoint (required for `"custom"` provider) |

### Known OAuth Providers

**Gmail:**
- Token URL: `https://oauth2.googleapis.com/token`
- Send via Gmail API: `POST https://gmail.googleapis.com/gmail/v1/users/me/messages/send`
- Requires `gmail.send` scope

**Outlook / Microsoft 365:**
- Token URL: `https://login.microsoftonline.com/common/oauth2/v2.0/token`
- Send via Microsoft Graph: `POST https://graph.microsoft.com/v1.0/me/sendMail`
- Requires `Mail.Send` scope

---

## Email Endpoints

### `GET /system/email-config`

Return the current email configuration (root only). Passwords/secrets are masked in the response.

**Response:**
```json
{
  "provider": "smtp",
  "host": "smtp.example.com",
  "port": 587,
  "username": "noreply@example.com",
  "password": "••••••••",
  "from_address": "noreply@example.com",
  "from_name": "AeorDB",
  "tls": "starttls",
  "configured": true
}
```

### `PUT /system/email-config`

Save email configuration (root only). Validates the config structure.

**Request:** The full config JSON (SMTP or OAuth format above).

**Response:** `200 OK` with the saved config (secrets masked).

### `POST /system/email-test`

Send a test email to verify the configuration works (root only).

**Request:**
```json
{
  "to": "admin@example.com"
}
```

**Response:**
```json
{
  "sent": true,
  "message": "Test email sent to admin@example.com"
}
```

Or on failure:
```json
{
  "sent": false,
  "error": "Connection refused: smtp.example.com:587"
}
```

---

## Notification Flow

When `POST /files/share` is called (existing endpoint):

1. Share is created (existing behavior, returns immediately)
2. After the response is sent, spawn a background task:
   a. Read `/.system/email-config.json` — if absent or `provider` is missing, skip
   b. For each user in the `users` array, look up their `User` record to get their email
   c. Build the notification email (see template below)
   d. Send via the configured provider
   e. Log success/failure (best-effort, never fail the share)

**Group shares:** When sharing with a group, resolve group members to users, collect their emails. Skip duplicates.

---

## Email Template

**Subject:** `"{sharer_name} shared files with you"`

**HTML Body:**

```
┌──────────────────────────────────────────┐
│                                          │
│  {sharer_name} shared files with you     │
│                                          │
│  Files:                                  │
│    📁 /photos/                           │
│    📄 sunset.jpg                         │
│                                          │
│  Permission: View only                   │
│                                          │
│  ┌──────────────────────┐                │
│  │     View Files       │                │
│  └──────────────────────┘                │
│                                          │
│  ─────────────────────────               │
│  Sent from AeorDB                        │
└──────────────────────────────────────────┘
```

- "View Files" button links to the portal at the shared path: `{base_url}/system/portal/?page=files&path={first_path}`
- The link uses the recipient's own auth (they need to log in), not a share link token
- Plain text fallback with the URL inline
- Clean, minimal styling — not a marketing email

### Permission Level Display

Map crudlify strings to human-readable labels:
- `cr..l...` → "View only"
- `crudl...` → "Can edit"
- `crudlify` → "Full access"
- Anything else → show the raw flags

---

## Settings Page (Portal)

New sidebar entry: **"Settings"** (below "Keys", root only).

### Layout

```
Settings
────────────────────────────────

Email Configuration
  ┌─────────────────────────────────────┐
  │ Provider: [SMTP ▾]                  │
  │                                     │
  │ Host:     [smtp.example.com      ]  │
  │ Port:     [587                   ]  │
  │ Username: [noreply@example.com   ]  │
  │ Password: [••••••••              ]  │
  │ From:     [noreply@example.com   ]  │
  │ TLS:      [STARTTLS ▾]             │
  │                                     │
  │ [Send Test Email]  [Save]           │
  └─────────────────────────────────────┘
```

When provider is switched to "OAuth":
```
  │ Provider:      [OAuth ▾]            │
  │ OAuth Service: [Gmail ▾]            │
  │ Client ID:     [                 ]  │
  │ Client Secret: [                 ]  │
  │ Refresh Token: [                 ]  │
  │ From:          [noreply@gmail.com]  │
```

**Behavior:**
- On page load, `GET /system/email-config` to populate form (or show empty state)
- "Save" → `PUT /system/email-config`
- "Send Test Email" → prompts for recipient email, calls `POST /system/email-test`
- Non-root users don't see Settings in the sidebar

---

## Rust Dependencies

- `lettre` — SMTP email sending (well-maintained, async support)
- `reqwest` — already in deps, used for OAuth token refresh + API calls
- No new dependencies for HTML templating — build email body with format strings

---

## Testing Strategy

**User model:**
- Create user without email → 400
- Create user with email → succeeds, email stored
- Two users with same email → both succeed (no unique constraint)
- Update user email → succeeds

**Email config:**
- Save SMTP config → stored at `/.system/email-config.json`
- Save OAuth config → stored correctly
- Get config → secrets masked
- Non-root → 403

**Email sending (unit tests with mock):**
- Share with user who has email + email configured → email sent
- Share with user who has email + no email config → no error, skipped
- Share with group → resolves members, sends to each
- SMTP connection failure → logged, share still succeeds
- OAuth token refresh → new access token obtained, email sent

**Settings page (manual/Puppeteer):**
- Settings link visible for root, hidden for non-root
- SMTP form saves and loads correctly
- OAuth form saves and loads correctly
- Test email sends and shows success/failure feedback
