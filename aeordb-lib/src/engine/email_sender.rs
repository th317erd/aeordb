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
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(text_body.to_string()),
                )
                .singlepart(
                    SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(html_body.to_string()),
                ),
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

    transport
        .send(email)
        .await
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
    let (token_url, _send_url) = match config.oauth_provider.as_str() {
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
    let token_resp = client
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &config.client_id),
            ("client_secret", &config.client_secret),
            ("refresh_token", &config.refresh_token),
        ])
        .send()
        .await
        .map_err(|e| format!("Token refresh request failed: {}", e))?;

    if !token_resp.status().is_success() {
        let status = token_resp.status();
        let body = token_resp.text().await.unwrap_or_default();
        return Err(format!("Token refresh returned {}: {}", status, body));
    }

    let token_data: serde_json::Value = token_resp
        .json()
        .await
        .map_err(|e| format!("Token response parse error: {}", e))?;
    let access_token = token_data["access_token"]
        .as_str()
        .ok_or("No access_token in token response")?;

    // Send via provider API
    match config.oauth_provider.as_str() {
        "gmail" => send_gmail(access_token, &config.from_address, to, subject, html_body).await,
        "outlook" => send_outlook(access_token, to, subject, html_body).await,
        // For custom providers, fall back to Gmail-style raw send
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
    let resp = client
        .post("https://gmail.googleapis.com/gmail/v1/users/me/messages/send")
        .bearer_auth(access_token)
        .json(&serde_json::json!({"raw": encoded}))
        .send()
        .await
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
    let resp = client
        .post("https://graph.microsoft.com/v1.0/me/sendMail")
        .bearer_auth(access_token)
        .json(&serde_json::json!({
            "message": {
                "subject": subject,
                "body": { "contentType": "HTML", "content": html_body },
                "toRecipients": [{ "emailAddress": { "address": to } }],
            }
        }))
        .send()
        .await
        .map_err(|e| format!("Outlook send failed: {}", e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Outlook API error: {}", body))
    }
}
