use axum::{
    Extension,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::responses::{ErrorResponse, require_root};
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::email_config::{EmailConfig, load_email_config, save_email_config};

// ---------------------------------------------------------------------------
// GET /system/email-config
// ---------------------------------------------------------------------------

/// Return the current email configuration (with secrets masked).
/// Returns `{ "configured": false }` when no configuration exists.
pub async fn get_email_config(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    match load_email_config(&state.engine) {
        Ok(Some(config)) => {
            Json(config.masked()).into_response()
        }
        Ok(None) => {
            Json(serde_json::json!({ "configured": false })).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to load email config: {}", e);
            ErrorResponse::new(format!("Failed to load email config: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// PUT /system/email-config
// ---------------------------------------------------------------------------

/// Save or replace the email configuration.
pub async fn put_email_config(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(config): Json<EmailConfig>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    if let Err(e) = save_email_config(&state.engine, &config) {
        tracing::error!("Failed to save email config: {}", e);
        return ErrorResponse::new(format!("Failed to save email config: {}", e))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    Json(serde_json::json!({
        "saved": true,
        "provider": match &config {
            EmailConfig::Smtp(_) => "smtp",
            EmailConfig::OAuth(_) => "oauth",
        },
        "from_address": config.from_address(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /system/email-test
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TestEmailRequest {
    pub to: String,
}

/// Send a test email using the stored configuration.
pub async fn send_test_email(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Json(body): Json<TestEmailRequest>,
) -> Response {
    let _user_id = match require_root(&claims) {
        Ok(id) => id,
        Err(response) => return response,
    };

    let config = match load_email_config(&state.engine) {
        Ok(Some(config)) => config,
        Ok(None) => {
            return ErrorResponse::new("Email is not configured. Use PUT /system/email-config first")
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
        Err(e) => {
            tracing::error!("Failed to load email config for test: {}", e);
            return ErrorResponse::new(format!("Failed to load email config: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response();
        }
    };

    let subject = "AeorDB Test Email";
    let html = "<h1>AeorDB Email Test</h1><p>If you can read this, email delivery is working.</p>";
    let text = "AeorDB Email Test\n\nIf you can read this, email delivery is working.";

    match crate::engine::email_sender::send_email(&config, &body.to, subject, html, text).await {
        Ok(()) => {
            Json(serde_json::json!({
                "sent": true,
                "to": body.to,
            }))
            .into_response()
        }
        Err(e) => {
            ErrorResponse::new(format!("Failed to send test email: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}
