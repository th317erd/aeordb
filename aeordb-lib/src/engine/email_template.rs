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

    let file_list: String = paths
        .iter()
        .map(|p| {
            let icon = if p.ends_with('/') { "\u{1F4C1}" } else { "\u{1F4C4}" };
            format!(
                "      <li style=\"padding:4px 0;\">{} {}</li>",
                icon,
                html_escape(p)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let text_files: String = paths
        .iter()
        .map(|p| format!("  - {}", p))
        .collect::<Vec<_>>()
        .join("\n");

    let html_body = format!(
        r#"<!DOCTYPE html>
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
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_notification() {
        let paths = vec!["/docs/report.pdf".to_string(), "/images/".to_string()];
        let (subject, html, text) = build_share_notification("Alice", &paths, "crudl...", &"/portal");

        assert_eq!(subject, "Alice shared files with you");
        assert!(html.contains("Alice shared files with you"));
        assert!(html.contains("/docs/report.pdf"));
        assert!(html.contains("/images/"));
        assert!(html.contains("#e87400")); // CTA button color
        assert!(html.contains("View Files"));
        assert!(html.contains("Sent from AeorDB"));

        assert!(text.contains("Alice shared files with you"));
        assert!(text.contains("/docs/report.pdf"));
        assert!(text.contains("Sent from AeorDB"));
    }

    #[test]
    fn test_permission_labels() {
        let paths = vec!["/test".to_string()];

        let (_, html, _) = build_share_notification("Bob", &paths, "cr..l...", "/p");
        assert!(html.contains("View only"));

        let (_, html, _) = build_share_notification("Bob", &paths, "-r--l---", "/p");
        assert!(html.contains("View only"));

        let (_, html, _) = build_share_notification("Bob", &paths, "crudl...", "/p");
        assert!(html.contains("Can edit"));

        let (_, html, _) = build_share_notification("Bob", &paths, "crudlify", "/p");
        assert!(html.contains("Full access"));

        // Unknown permissions show raw string
        let (_, html, _) = build_share_notification("Bob", &paths, "cr..l.f.", "/p");
        assert!(html.contains("cr..l.f."));
    }

    #[test]
    fn test_html_escaping() {
        let paths = vec!["<script>alert(1)</script>".to_string()];
        let (_, html, _) = build_share_notification(
            "Eve <evil>",
            &paths,
            "crudlify",
            "https://example.com?a=1&b=2",
        );

        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("Eve &lt;evil&gt;"));
        assert!(html.contains("a=1&amp;b=2"));
    }

    #[test]
    fn test_empty_paths() {
        let paths: Vec<String> = vec![];
        let (subject, html, text) = build_share_notification("Admin", &paths, "crudlify", "/portal");

        assert_eq!(subject, "Admin shared files with you");
        assert!(html.contains("Admin shared files with you"));
        assert!(text.contains("Admin shared files with you"));
    }

    #[test]
    fn test_file_vs_directory_icons() {
        let paths = vec!["/a/file.txt".to_string(), "/a/dir/".to_string()];
        let (_, html, _) = build_share_notification("X", &paths, "crudlify", "/p");

        // File icon for file.txt (no trailing slash)
        assert!(html.contains("\u{1F4C4} /a/file.txt"));
        // Folder icon for dir/ (trailing slash)
        assert!(html.contains("\u{1F4C1} /a/dir/"));
    }
}
