use crate::engine::email_config::{EmailConfig, SmtpConfig, OAuthConfig};

/// Validate that a URL is safe to make requests to (SSRF protection).
/// Blocks private/internal IP ranges, metadata endpoints, and non-HTTPS URLs
/// (except localhost for development).
fn validate_url(url: &str) -> Result<(), String> {
    // Must be HTTPS (except for localhost in dev)
    if !url.starts_with("https://") && !url.starts_with("http://localhost") {
        return Err(format!("URL must use HTTPS: {}", url));
    }
    // Block private IP ranges and metadata endpoints
    let blocked = [
        "169.254.", "10.", "172.16.", "172.17.", "172.18.", "172.19.",
        "172.20.", "172.21.", "172.22.", "172.23.", "172.24.", "172.25.",
        "172.26.", "172.27.", "172.28.", "172.29.", "172.30.", "172.31.",
        "192.168.", "127.0.0.1", "[::1]", "0.0.0.0",
    ];
    for prefix in blocked {
        if url.contains(prefix) {
            return Err(format!("URL targets a blocked address: {}", url));
        }
    }
    Ok(())
}

/// Validate that an email address does not contain header-injection characters.
/// Used as defense-in-depth for paths that construct raw email headers (e.g. Gmail).
fn validate_email(email: &str) -> Result<(), String> {
    if email.contains('\r') || email.contains('\n') {
        return Err("Invalid email address: contains newline characters".to_string());
    }
    Ok(())
}

/// An inline image (or other file) to attach to the email. `content_id`
/// is the value used in HTML via `<img src="cid:...">`. Multiple
/// attachments are allowed; each needs a unique `content_id`.
#[derive(Debug, Clone)]
pub struct InlineAttachment {
    pub content_id: String,
    pub mime_type:  String,
    pub filename:   String,
    pub bytes:      Vec<u8>,
}

/// Send an email using the configured provider.
/// Returns Ok(()) on success, Err(error_message) on failure.
pub async fn send_email(
    config: &EmailConfig,
    to: &str,
    subject: &str,
    html_body: &str,
    text_body: &str,
) -> Result<(), String> {
    send_email_with_attachments(config, to, subject, html_body, text_body, &[]).await
}

/// Send an email with optional inline attachments. Currently only the
/// SMTP path honors `attachments`; OAuth providers (Gmail/Outlook) fall
/// through to a no-attachment send and log a one-line warning. Add
/// per-provider attachment support there if it ever matters.
pub async fn send_email_with_attachments(
    config: &EmailConfig,
    to: &str,
    subject: &str,
    html_body: &str,
    text_body: &str,
    attachments: &[InlineAttachment],
) -> Result<(), String> {
    match config {
        EmailConfig::Smtp(smtp) => send_smtp(smtp, to, subject, html_body, text_body, attachments).await,
        EmailConfig::OAuth(oauth) => {
            if !attachments.is_empty() {
                tracing::warn!(
                    "[email] OAuth providers don't support inline attachments yet — sending {} byte(s) of attachment(s) inline as HTML only is not implemented; the HTML body's cid: references will appear broken in the recipient's client",
                    attachments.iter().map(|a| a.bytes.len()).sum::<usize>(),
                );
            }
            send_oauth(oauth, to, subject, html_body, text_body).await
        }
    }
}

async fn send_smtp(
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    html_body: &str,
    text_body: &str,
    attachments: &[InlineAttachment],
) -> Result<(), String> {
    use lettre::{
        AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
        message::{header::{ContentType, ContentDisposition, ContentId}, Mailbox, MultiPart, SinglePart},
        transport::smtp::authentication::Credentials,
        Message,
    };

    // Safety: lettre's Mailbox parser validates RFC 5321 addresses and rejects
    // embedded \r\n sequences, preventing email header injection attacks.
    let from: Mailbox = format!("{} <{}>", config.from_name, config.from_address)
        .parse()
        .map_err(|e| format!("Invalid from address: {}", e))?;

    let to_mailbox: Mailbox = to.parse()
        .map_err(|e| format!("Invalid to address: {}", e))?;

    // Build the body. Two cases:
    //   • no attachments → simple multipart/alternative (text + html).
    //   • attachments    → multipart/alternative with html branch wrapped
    //                      in multipart/related so the HTML can reference
    //                      images via `cid:...` and have them resolve to
    //                      the embedded image parts.
    let plain_part = SinglePart::builder()
        .header(ContentType::TEXT_PLAIN)
        .body(text_body.to_string());

    let html_part = SinglePart::builder()
        .header(ContentType::TEXT_HTML)
        .body(html_body.to_string());

    let multipart = if attachments.is_empty() {
        MultiPart::alternative()
            .singlepart(plain_part)
            .singlepart(html_part)
    } else {
        let mut related = MultiPart::related().singlepart(html_part);
        for att in attachments {
            let mime: ContentType = att.mime_type
                .parse()
                .map_err(|e| format!("Invalid attachment mime type {:?}: {}", att.mime_type, e))?;
            related = related.singlepart(
                SinglePart::builder()
                    .header(mime)
                    .header(ContentDisposition::inline_with_name(&att.filename))
                    .header(ContentId::from(format!("<{}>", att.content_id)))
                    .body(att.bytes.clone()),
            );
        }
        MultiPart::alternative()
            .singlepart(plain_part)
            .multipart(related)
    };

    let email = Message::builder()
        .from(from)
        .to(to_mailbox)
        .subject(subject)
        .multipart(multipart)
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
        "custom" => {
            let t_url = config.token_url.clone().ok_or("Custom provider requires token_url")?;
            let s_url = config.send_url.clone().ok_or("Custom provider requires send_url")?;
            validate_url(&t_url).map_err(|e| format!("token_url rejected: {}", e))?;
            validate_url(&s_url).map_err(|e| format!("send_url rejected: {}", e))?;
            (t_url, s_url)
        },
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

    // Defense in depth: validate email addresses before embedding in raw headers.
    // Even though the raw email is base64-encoded, malicious \r\n in from/to could
    // inject additional headers into the MIME message before encoding.
    validate_email(from)?;
    validate_email(to)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_url tests ──────────────────────────────────────────────

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_url("https://oauth2.googleapis.com/token").is_ok());
        assert!(validate_url("https://login.microsoftonline.com/common/oauth2/v2.0/token").is_ok());
        assert!(validate_url("https://custom-provider.example.com/oauth/token").is_ok());
    }

    #[test]
    fn validate_url_accepts_localhost_http() {
        assert!(validate_url("http://localhost:8080/token").is_ok());
        assert!(validate_url("http://localhost/send").is_ok());
    }

    #[test]
    fn validate_url_rejects_plain_http() {
        let result = validate_url("http://evil.com/token");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must use HTTPS"));
    }

    #[test]
    fn validate_url_rejects_metadata_endpoint() {
        let result = validate_url("https://169.254.169.254/latest/meta-data/");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blocked address"));
    }

    #[test]
    fn validate_url_rejects_private_ranges() {
        let cases = vec![
            "https://10.0.0.1/token",
            "https://172.16.0.1/token",
            "https://172.31.255.255/token",
            "https://192.168.1.1/token",
            "https://127.0.0.1/token",
            "https://[::1]/token",
            "https://0.0.0.0/token",
        ];
        for url in cases {
            let result = validate_url(url);
            assert!(result.is_err(), "Expected rejection for: {}", url);
            assert!(result.unwrap_err().contains("blocked address"));
        }
    }

    #[test]
    fn validate_url_rejects_private_ip_in_path() {
        // Attacker might try to sneak a private IP into the path or query
        let result = validate_url("https://evil.com/redirect?target=http://169.254.169.254");
        assert!(result.is_err());
    }

    #[test]
    fn validate_url_rejects_ftp_and_other_schemes() {
        assert!(validate_url("ftp://files.example.com/data").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("gopher://evil.com").is_err());
    }

    // ── validate_email tests ────────────────────────────────────────────

    #[test]
    fn validate_email_accepts_normal_addresses() {
        assert!(validate_email("user@example.com").is_ok());
        assert!(validate_email("admin+tag@sub.domain.org").is_ok());
    }

    #[test]
    fn validate_email_rejects_cr() {
        let result = validate_email("attacker@evil.com\rBcc: payroll@evil.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("newline"));
    }

    #[test]
    fn validate_email_rejects_lf() {
        let result = validate_email("attacker@evil.com\nBcc: payroll@evil.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("newline"));
    }

    #[test]
    fn validate_email_rejects_crlf() {
        let result = validate_email("attacker@evil.com\r\nBcc: payroll@evil.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("newline"));
    }

    #[test]
    fn validate_email_rejects_embedded_newline() {
        // Newline in the middle of an otherwise-valid looking address
        let result = validate_email("user\n@example.com");
        assert!(result.is_err());
    }

    // ── lettre Mailbox parser injection test ────────────────────────────

    #[test]
    fn lettre_mailbox_rejects_header_injection() {
        use lettre::message::Mailbox;
        // Verify that lettre itself rejects addresses with \r\n (our safety comment is accurate)
        let injected = "attacker@evil.com\r\nBcc: payroll@evil.com";
        let result: Result<Mailbox, _> = injected.parse();
        assert!(result.is_err(), "lettre must reject addresses containing CRLF");
    }
}
