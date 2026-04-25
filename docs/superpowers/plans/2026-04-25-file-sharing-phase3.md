# File Sharing Phase 3 — Email Notifications + Settings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Send email notifications when files are shared, with SMTP and OAuth provider support configurable via a Settings page in the portal.

**Architecture:** Four layers: (1) make email required on user creation, (2) add email config storage + sending infrastructure (SMTP via `lettre`, OAuth via `reqwest`), (3) hook share notifications into the existing `POST /files/share` flow as background tasks, (4) add a Settings portal page for email configuration.

**Tech Stack:** Rust (axum, lettre, reqwest, serde, tokio), JavaScript (web components), existing system store + event bus

**Spec:** `docs/superpowers/specs/2026-04-25-file-sharing-phase3-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `aeordb-lib/Cargo.toml` | Modify | Add `lettre` dependency |
| `aeordb-lib/src/server/admin_routes.rs` | Modify | Make email required on `CreateUserRequest` |
| `aeordb-lib/src/engine/email_config.rs` | Create | `EmailConfig` struct (SMTP/OAuth), serialize/deserialize, load/save |
| `aeordb-lib/src/engine/email_sender.rs` | Create | `send_email()` — SMTP via lettre, OAuth via reqwest |
| `aeordb-lib/src/engine/email_template.rs` | Create | `build_share_notification()` — HTML + plain text email body |
| `aeordb-lib/src/engine/mod.rs` | Modify | Register new modules |
| `aeordb-lib/src/server/settings_routes.rs` | Create | `GET/PUT /system/email-config`, `POST /system/email-test` |
| `aeordb-lib/src/server/share_routes.rs` | Modify | Add notification hook after share creation |
| `aeordb-lib/src/server/mod.rs` | Modify | Register settings routes |
| `aeordb-lib/src/portal/settings.mjs` | Create | Settings page web component |
| `aeordb-lib/src/portal/app.mjs` | Modify | Add Settings to page map + sidebar |
| `aeordb-lib/src/portal/index.html` | Modify | Import settings.mjs |
| `aeordb-lib/src/server/portal_routes.rs` | Modify | Serve settings.mjs |
| `aeordb-lib/spec/engine/email_notification_spec.rs` | Create | Tests |

---

### Task 1: Make Email Required on User Creation

**Files:**
- Modify: `aeordb-lib/src/server/admin_routes.rs`

- [ ] **Step 1: Change `CreateUserRequest.email` from `Option<String>` to `String`**

In `aeordb-lib/src/server/admin_routes.rs`, read the file first, then change:
```rust
pub struct CreateUserRequest {
  pub username: String,
  pub email: Option<String>,
  #[serde(default)]
  pub tags: Vec<String>,
}
```
to:
```rust
pub struct CreateUserRequest {
  pub username: String,
  pub email: String,
  #[serde(default)]
  pub tags: Vec<String>,
}
```

- [ ] **Step 2: Update `create_user` handler to pass required email**

Change:
```rust
let mut user = User::new(&payload.username, payload.email.as_deref());
```
to:
```rust
let mut user = User::new(&payload.username, Some(&payload.email));
```

Note: `User::new` still accepts `Option<&str>` — we're just always passing `Some` now. The `User.email` field stays `Option<String>` for backward compat with existing stored users.

- [ ] **Step 3: Verify compilation**

Run: `cd /home/wyatt/Projects/aeordb-workspace/aeordb && cargo build -p aeordb 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/server/admin_routes.rs
git commit -m "Make email required on user creation API"
```

---

### Task 2: Email Config Model + Storage Endpoints

**Files:**
- Create: `aeordb-lib/src/engine/email_config.rs`
- Create: `aeordb-lib/src/server/settings_routes.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Modify: `aeordb-lib/src/server/mod.rs`

- [ ] **Step 1: Create email_config.rs**

Read the spec for the full config structure. Create `aeordb-lib/src/engine/email_config.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::EngineResult;
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

const EMAIL_CONFIG_PATH: &str = "/.system/email-config.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider")]
pub enum EmailConfig {
    #[serde(rename = "smtp")]
    Smtp(SmtpConfig),
    #[serde(rename = "oauth")]
    OAuth(OAuthConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from_address: String,
    #[serde(default = "default_from_name")]
    pub from_name: String,
    #[serde(default = "default_tls")]
    pub tls: String, // "starttls", "tls", or "none"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    pub oauth_provider: String, // "gmail", "outlook", "custom"
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub from_address: String,
    #[serde(default = "default_from_name")]
    pub from_name: String,
    pub token_url: Option<String>,
    pub send_url: Option<String>,
}

fn default_from_name() -> String { "AeorDB".to_string() }
fn default_tls() -> String { "starttls".to_string() }

impl EmailConfig {
    pub fn from_address(&self) -> &str {
        match self {
            EmailConfig::Smtp(c) => &c.from_address,
            EmailConfig::OAuth(c) => &c.from_address,
        }
    }

    pub fn from_name(&self) -> &str {
        match self {
            EmailConfig::Smtp(c) => &c.from_name,
            EmailConfig::OAuth(c) => &c.from_name,
        }
    }

    /// Mask sensitive fields for API responses.
    pub fn masked(&self) -> serde_json::Value {
        let mut val = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = val.as_object_mut() {
            for key in ["password", "client_secret", "refresh_token"] {
                if obj.contains_key(key) {
                    obj.insert(key.to_string(), serde_json::json!("••••••••"));
                }
            }
            obj.insert("configured".to_string(), serde_json::json!(true));
        }
        val
    }
}

/// Load email config from the database.
pub fn load_email_config(engine: &StorageEngine) -> EngineResult<Option<EmailConfig>> {
    let ops = DirectoryOps::new(engine);
    match ops.read_file(EMAIL_CONFIG_PATH) {
        Ok(data) => {
            let config: EmailConfig = serde_json::from_slice(&data)
                .map_err(|e| crate::engine::errors::EngineError::JsonParseError(
                    format!("Invalid email config: {}", e)
                ))?;
            Ok(Some(config))
        }
        Err(crate::engine::errors::EngineError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Save email config to the database.
pub fn save_email_config(engine: &StorageEngine, config: &EmailConfig) -> EngineResult<()> {
    let ops = DirectoryOps::new(engine);
    let ctx = RequestContext::system();
    let data = serde_json::to_vec_pretty(config)
        .map_err(|e| crate::engine::errors::EngineError::JsonParseError(e.to_string()))?;
    ops.store_file(&ctx, EMAIL_CONFIG_PATH, &data, Some("application/json"))
}
```

- [ ] **Step 2: Register module in engine/mod.rs**

Add `pub mod email_config;` to `aeordb-lib/src/engine/mod.rs`.

- [ ] **Step 3: Create settings_routes.rs**

Read `aeordb-lib/src/server/share_routes.rs` for patterns. Create `aeordb-lib/src/server/settings_routes.rs`:

```rust
use axum::{Extension, extract::State, http::StatusCode, response::{IntoResponse, Response}, Json};
use uuid::Uuid;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::email_config::{self, EmailConfig};
use crate::engine::user::is_root;

/// GET /system/email-config — return current email config (root only, secrets masked).
pub async fn get_email_config(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => return ErrorResponse::new("Invalid identity").with_status(StatusCode::FORBIDDEN).into_response(),
    };
    if !is_root(&caller_id) {
        return ErrorResponse::new("Root only").with_status(StatusCode::FORBIDDEN).into_response();
    }

    match email_config::load_email_config(&state.engine) {
        Ok(Some(config)) => Json(config.masked()).into_response(),
        Ok(None) => Json(serde_json::json!({"configured": false})).into_response(),
        Err(e) => ErrorResponse::new(format!("Failed to load config: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

/// PUT /system/email-config — save email config (root only).
pub async fn put_email_config(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(config): Json<EmailConfig>,
) -> Response {
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => return ErrorResponse::new("Invalid identity").with_status(StatusCode::FORBIDDEN).into_response(),
    };
    if !is_root(&caller_id) {
        return ErrorResponse::new("Root only").with_status(StatusCode::FORBIDDEN).into_response();
    }

    match email_config::save_email_config(&state.engine, &config) {
        Ok(()) => Json(config.masked()).into_response(),
        Err(e) => ErrorResponse::new(format!("Failed to save config: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    }
}

/// POST /system/email-test — send a test email (root only).
pub async fn send_test_email(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let caller_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => return ErrorResponse::new("Invalid identity").with_status(StatusCode::FORBIDDEN).into_response(),
    };
    if !is_root(&caller_id) {
        return ErrorResponse::new("Root only").with_status(StatusCode::FORBIDDEN).into_response();
    }

    let to = match body.get("to").and_then(|v| v.as_str()) {
        Some(to) => to.to_string(),
        None => return ErrorResponse::new("Missing 'to' field").with_status(StatusCode::BAD_REQUEST).into_response(),
    };

    let config = match email_config::load_email_config(&state.engine) {
        Ok(Some(c)) => c,
        Ok(None) => return ErrorResponse::new("Email not configured").with_status(StatusCode::BAD_REQUEST).into_response(),
        Err(e) => return ErrorResponse::new(format!("Failed to load config: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response(),
    };

    match crate::engine::email_sender::send_email(
        &config,
        &to,
        "AeorDB Test Email",
        "<h2>Test Email</h2><p>If you received this, your email configuration is working correctly.</p>",
        "Test Email\n\nIf you received this, your email configuration is working correctly.",
    ).await {
        Ok(()) => Json(serde_json::json!({"sent": true, "message": format!("Test email sent to {}", to)})).into_response(),
        Err(e) => Json(serde_json::json!({"sent": false, "error": e})).into_response(),
    }
}
```

- [ ] **Step 4: Register routes in mod.rs**

In `aeordb-lib/src/server/mod.rs`:
- Add `pub mod settings_routes;`
- Register:
```rust
    .route("/system/email-config", get(settings_routes::get_email_config).put(settings_routes::put_email_config))
    .route("/system/email-test", post(settings_routes::send_test_email))
```

- [ ] **Step 5: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -10`
(This will fail until email_sender.rs exists — create a stub first if needed)

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/engine/email_config.rs aeordb-lib/src/engine/mod.rs aeordb-lib/src/server/settings_routes.rs aeordb-lib/src/server/mod.rs
git commit -m "Add email config model, settings endpoints (GET/PUT/test)"
```

---

### Task 3: Email Sending Infrastructure

**Files:**
- Modify: `aeordb-lib/Cargo.toml`
- Create: `aeordb-lib/src/engine/email_sender.rs`
- Create: `aeordb-lib/src/engine/email_template.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`

- [ ] **Step 1: Add `lettre` dependency**

In `aeordb-lib/Cargo.toml`, add to `[dependencies]`:
```toml
lettre = { version = "0.11", features = ["tokio1-native-tls", "builder", "smtp-transport"] }
```

- [ ] **Step 2: Create email_sender.rs**

Create `aeordb-lib/src/engine/email_sender.rs`:

```rust
use crate::engine::email_config::{EmailConfig, SmtpConfig, OAuthConfig};

/// Send an email using the configured provider.
/// Returns Ok(()) on success, Err(error_message) on failure.
pub async fn send_email(
    config: &EmailConfig,
    to: &str,
    subject: &str,
    html_body: &str,
    text_body: &str,
) -> Result<(), String> {
    match config {
        EmailConfig::Smtp(smtp) => send_smtp(smtp, to, subject, html_body, text_body).await,
        EmailConfig::OAuth(oauth) => send_oauth(oauth, to, subject, html_body, text_body).await,
    }
}

async fn send_smtp(
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    html_body: &str,
    text_body: &str,
) -> Result<(), String> {
    use lettre::{
        AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
        message::{header::ContentType, Mailbox, MultiPart, SinglePart},
        transport::smtp::authentication::Credentials,
        Message,
    };

    let from: Mailbox = format!("{} <{}>", config.from_name, config.from_address)
        .parse()
        .map_err(|e| format!("Invalid from address: {}", e))?;

    let to_mailbox: Mailbox = to.parse()
        .map_err(|e| format!("Invalid to address: {}", e))?;

    let email = Message::builder()
        .from(from)
        .to(to_mailbox)
        .subject(subject)
        .multipart(
            MultiPart::alternative()
                .singlepart(SinglePart::builder().header(ContentType::TEXT_PLAIN).body(text_body.to_string()))
                .singlepart(SinglePart::builder().header(ContentType::TEXT_HTML).body(html_body.to_string()))
        )
        .map_err(|e| format!("Failed to build email: {}", e))?;

    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let transport = match config.tls.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
            .map_err(|e| format!("SMTP relay error: {}", e))?
            .port(config.port)
            .credentials(creds)
            .build(),
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host)
            .port(config.port)
            .credentials(creds)
            .build(),
        _ /* starttls */ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
            .map_err(|e| format!("SMTP STARTTLS error: {}", e))?
            .port(config.port)
            .credentials(creds)
            .build(),
    };

    transport.send(email).await
        .map(|_| ())
        .map_err(|e| format!("SMTP send failed: {}", e))
}

async fn send_oauth(
    config: &OAuthConfig,
    to: &str,
    subject: &str,
    html_body: &str,
    _text_body: &str,
) -> Result<(), String> {
    // Determine token and send URLs based on provider
    let (token_url, send_url) = match config.oauth_provider.as_str() {
        "gmail" => (
            "https://oauth2.googleapis.com/token".to_string(),
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/send".to_string(),
        ),
        "outlook" => (
            "https://login.microsoftonline.com/common/oauth2/v2.0/token".to_string(),
            "https://graph.microsoft.com/v1.0/me/sendMail".to_string(),
        ),
        "custom" => (
            config.token_url.clone().ok_or("Custom provider requires token_url")?,
            config.send_url.clone().ok_or("Custom provider requires send_url")?,
        ),
        other => return Err(format!("Unknown OAuth provider: {}", other)),
    };

    // Refresh the access token
    let client = reqwest::Client::new();
    let token_resp = client.post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("refresh_token", &config.refresh_token),
        ])
        .send().await
        .map_err(|e| format!("Token refresh failed: {}", e))?;

    if !token_resp.status().is_success() {
        let body = token_resp.text().await.unwrap_or_default();
        return Err(format!("Token refresh returned {}: {}", "error", body));
    }

    let token_data: serde_json::Value = token_resp.json().await
        .map_err(|e| format!("Token response parse error: {}", e))?;
    let access_token = token_data["access_token"].as_str()
        .ok_or("No access_token in response")?;

    // Send via provider API
    match config.oauth_provider.as_str() {
        "gmail" => send_gmail(access_token, &config.from_address, to, subject, html_body).await,
        "outlook" => send_outlook(access_token, to, subject, html_body).await,
        _ => send_gmail(access_token, &config.from_address, to, subject, html_body).await,
    }
}

async fn send_gmail(
    access_token: &str,
    from: &str,
    to: &str,
    subject: &str,
    html_body: &str,
) -> Result<(), String> {
    use base64::Engine;

    let raw_email = format!(
        "From: {}\r\nTo: {}\r\nSubject: {}\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{}",
        from, to, subject, html_body
    );
    let encoded = base64::engine::general_purpose::URL_SAFE.encode(raw_email.as_bytes());

    let client = reqwest::Client::new();
    let resp = client.post("https://gmail.googleapis.com/gmail/v1/users/me/messages/send")
        .bearer_auth(access_token)
        .json(&serde_json::json!({"raw": encoded}))
        .send().await
        .map_err(|e| format!("Gmail send failed: {}", e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Gmail API error: {}", body))
    }
}

async fn send_outlook(
    access_token: &str,
    to: &str,
    subject: &str,
    html_body: &str,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let resp = client.post("https://graph.microsoft.com/v1.0/me/sendMail")
        .bearer_auth(access_token)
        .json(&serde_json::json!({
            "message": {
                "subject": subject,
                "body": {"contentType": "HTML", "content": html_body},
                "toRecipients": [{"emailAddress": {"address": to}}],
            }
        }))
        .send().await
        .map_err(|e| format!("Outlook send failed: {}", e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Outlook API error: {}", body))
    }
}
```

- [ ] **Step 3: Create email_template.rs**

Create `aeordb-lib/src/engine/email_template.rs`:

```rust
/// Build the share notification email.
/// Returns (subject, html_body, text_body).
pub fn build_share_notification(
    sharer_name: &str,
    paths: &[String],
    permissions: &str,
    portal_url: &str,
) -> (String, String, String) {
    let subject = format!("{} shared files with you", sharer_name);

    let perm_label = match permissions {
        "cr..l..." | "-r--l---" => "View only",
        "crudl..." => "Can edit",
        "crudlify" => "Full access",
        _ => permissions,
    };

    let file_list: String = paths.iter().map(|p| {
        let icon = if p.ends_with('/') { "📁" } else { "📄" };
        format!("      <li style=\"padding:4px 0;\">{} {}</li>", icon, html_escape(p))
    }).collect::<Vec<_>>().join("\n");

    let text_files: String = paths.iter().map(|p| format!("  - {}", p)).collect::<Vec<_>>().join("\n");

    let html_body = format!(r#"<!DOCTYPE html>
<html>
<body style="font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;margin:0;padding:0;background:#f6f8fa;">
  <div style="max-width:560px;margin:40px auto;background:#ffffff;border-radius:8px;border:1px solid #d0d7de;overflow:hidden;">
    <div style="padding:32px;">
      <h2 style="margin:0 0 16px;color:#24292f;font-size:20px;">{sharer} shared files with you</h2>
      <div style="margin-bottom:20px;">
        <div style="font-size:14px;color:#57606a;margin-bottom:8px;font-weight:600;">Files:</div>
        <ul style="list-style:none;padding:0;margin:0;font-size:14px;color:#24292f;">
{file_list}
        </ul>
      </div>
      <div style="margin-bottom:24px;font-size:14px;color:#57606a;">
        Permission: <strong style="color:#24292f;">{perm_label}</strong>
      </div>
      <a href="{url}" style="display:inline-block;padding:10px 24px;background:#e87400;color:#ffffff;text-decoration:none;border-radius:6px;font-weight:600;font-size:14px;">View Files</a>
    </div>
    <div style="padding:16px 32px;background:#f6f8fa;border-top:1px solid #d0d7de;font-size:12px;color:#57606a;">
      Sent from AeorDB
    </div>
  </div>
</body>
</html>"#,
        sharer = html_escape(sharer_name),
        file_list = file_list,
        perm_label = perm_label,
        url = html_escape(portal_url),
    );

    let text_body = format!(
        "{sharer} shared files with you\n\nFiles:\n{files}\n\nPermission: {perm}\n\nView Files: {url}\n\n--\nSent from AeorDB",
        sharer = sharer_name,
        files = text_files,
        perm = perm_label,
        url = portal_url,
    );

    (subject, html_body, text_body)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
```

- [ ] **Step 4: Register modules in engine/mod.rs**

Add:
```rust
pub mod email_sender;
pub mod email_template;
```

- [ ] **Step 5: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -10`

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/Cargo.toml aeordb-lib/src/engine/email_sender.rs aeordb-lib/src/engine/email_template.rs aeordb-lib/src/engine/mod.rs
git commit -m "Add email sending infrastructure: SMTP (lettre) + OAuth (reqwest)"
```

---

### Task 4: Share Notification Hook

**Files:**
- Modify: `aeordb-lib/src/server/share_routes.rs`

- [ ] **Step 1: Add notification after share creation**

In `aeordb-lib/src/server/share_routes.rs`, read the `share` handler. After the successful response is built (before returning), spawn a background task to send notifications:

```rust
    // Spawn background email notification (best-effort, never blocks the response)
    let engine = state.engine.clone();
    let shared_paths = body.paths.clone();
    let permissions = body.permissions.clone();
    let users_to_notify: Vec<String> = body.users.clone().unwrap_or_default();
    tokio::spawn(async move {
        if let Err(e) = send_share_notifications(&engine, &users_to_notify, &shared_paths, &permissions).await {
            tracing::warn!("Share notification failed: {}", e);
        }
    });
```

Add the notification function in the same file:

```rust
async fn send_share_notifications(
    engine: &crate::engine::StorageEngine,
    user_ids: &[String],
    paths: &[String],
    permissions: &str,
) -> Result<(), String> {
    // Load email config — if not configured, silently skip
    let config = match crate::engine::email_config::load_email_config(engine) {
        Ok(Some(c)) => c,
        _ => return Ok(()),
    };

    // Resolve sharer name (for now, "Someone" — we don't have the caller's name in this context easily)
    // TODO: pass caller username from the handler
    let sharer_name = "Someone";

    for uid_str in user_ids {
        let uid = match uuid::Uuid::parse_str(uid_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let user = match crate::engine::system_store::get_user(engine, &uid) {
            Ok(Some(u)) => u,
            _ => continue,
        };
        let email = match user.email {
            Some(ref e) if !e.is_empty() => e.clone(),
            _ => continue,
        };

        let portal_url = format!("/system/portal/?page=files&path={}", paths.first().map(|p| p.as_str()).unwrap_or("/"));
        let (subject, html, text) = crate::engine::email_template::build_share_notification(
            sharer_name, paths, permissions, &portal_url,
        );

        if let Err(e) = crate::engine::email_sender::send_email(&config, &email, &subject, &html, &text).await {
            tracing::warn!("Failed to notify {}: {}", email, e);
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Pass sharer username to the notification**

Update the handler to resolve the caller's username before spawning the notification. At the top of the `share` handler, after parsing `caller_id`, resolve their username:

```rust
    let sharer_name = if is_root(&caller_id) {
        "Root".to_string()
    } else {
        crate::engine::system_store::get_user(&state.engine, &caller_id)
            .ok().flatten()
            .map(|u| u.username)
            .unwrap_or_else(|| "Someone".to_string())
    };
```

Then pass `sharer_name` into the spawned task and the `send_share_notifications` function.

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/server/share_routes.rs
git commit -m "Hook share notifications: background email on POST /files/share"
```

---

### Task 5: Settings Portal Page

**Files:**
- Create: `aeordb-lib/src/portal/settings.mjs`
- Modify: `aeordb-lib/src/portal/app.mjs`
- Modify: `aeordb-lib/src/portal/index.html`
- Modify: `aeordb-lib/src/server/portal_routes.rs`

- [ ] **Step 1: Create settings.mjs**

Read `aeordb-lib/src/portal/groups.mjs` for the component pattern. Create `aeordb-lib/src/portal/settings.mjs`:

The component should:
- Show an email configuration form with provider selector (SMTP / OAuth)
- SMTP fields: host, port, username, password, from_address, from_name, tls mode
- OAuth fields: oauth_provider (Gmail/Outlook/Custom), client_id, client_secret, refresh_token, from_address, from_name
- Dynamically show/hide fields based on provider selection
- "Save" button → `PUT /system/email-config`
- "Send Test Email" button → prompts for recipient, calls `POST /system/email-test`
- On load → `GET /system/email-config` to populate fields
- Handle forbidden (non-root user) gracefully

Follow the existing portal styling patterns (card, form-group, form-label, form-input, button classes).

- [ ] **Step 2: Register in app.mjs**

In `aeordb-lib/src/portal/app.mjs`:

Add import at the top:
```javascript
import '/system/portal/settings.mjs';
```

Add to the `pageMap` object:
```javascript
    'settings': 'aeor-settings',
```

- [ ] **Step 3: Add sidebar link**

In `aeordb-lib/src/portal/index.html`, find the sidebar nav links. Add after Keys:
```html
<a class="nav-link" data-page="settings" href="?page=settings">Settings</a>
```

- [ ] **Step 4: Serve settings.mjs**

In `aeordb-lib/src/server/portal_routes.rs`, add:
```rust
const PORTAL_SETTINGS_JS: &str = include_str!("../portal/settings.mjs");
```

And in the asset match:
```rust
"settings.mjs" => (PORTAL_SETTINGS_JS, "application/javascript; charset=utf-8"),
```

- [ ] **Step 5: Verify the portal loads**

Start the server and navigate to the Settings page in the browser.

- [ ] **Step 6: Commit**

```bash
git add aeordb-lib/src/portal/settings.mjs aeordb-lib/src/portal/app.mjs aeordb-lib/src/portal/index.html aeordb-lib/src/server/portal_routes.rs
git commit -m "Add Settings page with email configuration UI"
```

---

### Task 6: Tests

**Files:**
- Create: `aeordb-lib/spec/engine/email_notification_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create email_notification_spec.rs**

Tests to write:

**User model:**
1. **create_user_without_email_fails** — POST /system/users with no email field → 400 (serde deserialization fails because email is now required)
2. **create_user_with_email_succeeds** — POST /system/users with email → 201, email stored
3. **two_users_same_email_succeeds** — Create two users with the same email → both succeed (no unique constraint)

**Email config:**
4. **save_smtp_config** — PUT /system/email-config with SMTP config → 200, stored at /.system/email-config.json
5. **save_oauth_config** — PUT /system/email-config with OAuth config → 200
6. **get_config_masks_secrets** — Save SMTP config, GET → password is "••••••••"
7. **get_config_not_configured** — GET without saving → {"configured": false}
8. **config_requires_root** — Non-root user → 403 on GET and PUT
9. **test_email_requires_config** — POST /system/email-test without config → 400

**Email template:**
10. **build_share_notification_produces_valid_html** — Call `build_share_notification` directly, verify subject, verify HTML contains sharer name and paths, verify text fallback exists

- [ ] **Step 2: Register test in Cargo.toml**

```toml
[[test]]
name = "email_notification_spec"
path = "spec/engine/email_notification_spec.rs"
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test email_notification_spec 2>&1 | tail -20`
Run: `cargo test 2>&1 | grep "FAILED" || echo "ALL PASS"`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/spec/engine/email_notification_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add email notification tests: user email, config, templates"
```

---

### Task 7: Full Verification

- [ ] **Step 1: Run the complete test suite**

Run: `cargo test 2>&1 | grep "FAILED" || echo "ALL PASS"`

- [ ] **Step 2: Manual E2E test**

Start server, save an SMTP config via the API, share a file with a user who has an email, check server logs for notification attempt.

- [ ] **Step 3: Update docs**

Add email config and settings endpoints to `docs/src/api/admin.md`.

- [ ] **Step 4: Final commit**
