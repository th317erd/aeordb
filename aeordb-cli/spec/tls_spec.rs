use std::process::Command;

/// Helper to run the aeordb binary with the given arguments and capture the result.
fn run_aeordb(arguments: &[&str]) -> std::process::Output {
  let binary = env!("CARGO_BIN_EXE_aeordb");
  Command::new(binary)
    .args(arguments)
    .output()
    .expect("failed to execute aeordb binary")
}

// ---------------------------------------------------------------------------
// CLI flag parsing: --tls-cert and --tls-key are accepted
// ---------------------------------------------------------------------------

#[test]
fn start_accepts_both_tls_flags() {
  // Providing both --tls-cert and --tls-key with nonexistent files should
  // fail at cert loading, not at argument parsing. This proves both flags
  // are recognized by clap.
  let output = run_aeordb(&[
    "start",
    "--tls-cert", "/nonexistent/cert.pem",
    "--tls-key", "/nonexistent/key.pem",
  ]);

  let stderr = String::from_utf8_lossy(&output.stderr);
  // Should fail loading the cert, not from unrecognized argument
  assert!(
    !stderr.contains("unexpected argument"),
    "clap should accept --tls-cert and --tls-key, but got: {stderr}"
  );
  assert!(
    stderr.contains("Failed to load TLS certificate/key"),
    "expected TLS file loading error, got: {stderr}"
  );
}

// ---------------------------------------------------------------------------
// Mutual requirement: --tls-cert without --tls-key
// ---------------------------------------------------------------------------

#[test]
fn start_errors_when_only_tls_cert_provided() {
  let output = run_aeordb(&[
    "start",
    "--tls-cert", "/some/cert.pem",
  ]);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    !output.status.success(),
    "should exit with non-zero when only --tls-cert is provided"
  );
  assert!(
    stderr.contains("--tls-cert requires --tls-key"),
    "expected mutual requirement error message, got: {stderr}"
  );
}

// ---------------------------------------------------------------------------
// Mutual requirement: --tls-key without --tls-cert
// ---------------------------------------------------------------------------

#[test]
fn start_errors_when_only_tls_key_provided() {
  let output = run_aeordb(&[
    "start",
    "--tls-key", "/some/key.pem",
  ]);

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    !output.status.success(),
    "should exit with non-zero when only --tls-key is provided"
  );
  assert!(
    stderr.contains("--tls-key requires --tls-cert"),
    "expected mutual requirement error message, got: {stderr}"
  );
}

// ---------------------------------------------------------------------------
// No TLS flags: prints "TLS: disabled" (backwards-compatible startup)
// ---------------------------------------------------------------------------

#[test]
fn start_without_tls_flags_prints_tls_disabled() {
  // Start with a nonexistent database to trigger an early (but benign) failure
  // after printing the startup banner. We just want to verify the TLS line.
  let output = run_aeordb(&[
    "start",
    "--database", "/nonexistent/data.aeordb",
  ]);

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("TLS: disabled"),
    "expected 'TLS: disabled' in startup output, got: {stdout}"
  );
  assert!(
    stdout.contains("Listening on http://"),
    "expected http:// URL when TLS is disabled, got: {stdout}"
  );
}

// ---------------------------------------------------------------------------
// TLS enabled: prints "TLS: enabled" and https:// URL
// ---------------------------------------------------------------------------

#[test]
fn start_with_tls_flags_prints_tls_enabled() {
  // Use nonexistent cert files -- the banner should print before cert loading
  // fails, so we can verify the TLS status line.
  let output = run_aeordb(&[
    "start",
    "--tls-cert", "/nonexistent/cert.pem",
    "--tls-key", "/nonexistent/key.pem",
  ]);

  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(
    stdout.contains("TLS: enabled"),
    "expected 'TLS: enabled' in startup output, got: {stdout}"
  );
  // The URL should show https when TLS is configured
  assert!(
    stdout.contains("Listening on https://"),
    "expected https:// URL when TLS is enabled, got: {stdout}"
  );
}
