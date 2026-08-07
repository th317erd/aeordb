#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn wait_for_output(mut child: Child, timeout: Duration) -> (Output, bool) {
  let deadline = Instant::now() + timeout;
  loop {
    if child.try_wait().expect("poll aeordb child").is_some() {
      return (child.wait_with_output().expect("collect aeordb output"), false);
    }
    if Instant::now() >= deadline {
      child.kill().expect("terminate hung aeordb child");
      return (child.wait_with_output().expect("collect timed-out aeordb output"), true);
    }
    std::thread::sleep(Duration::from_millis(20));
  }
}

#[cfg(unix)]
fn unused_local_port() -> u16 {
  TcpListener::bind(("127.0.0.1", 0)).expect("reserve local port").local_addr().expect("read local address").port()
}

#[cfg(unix)]
fn wait_until_ready(child: &mut Child, port: u16, timeout: Duration) -> Result<(), String> {
  let deadline = Instant::now() + timeout;
  while Instant::now() < deadline {
    if let Some(status) = child.try_wait().map_err(|error| format!("poll AeorDB child: {error}"))? {
      return Err(format!("AeorDB exited before becoming ready with status {status}"));
    }
    if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
      stream.set_read_timeout(Some(Duration::from_millis(500))).expect("set health read timeout");
      stream.write_all(b"GET /system/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").expect("write health request");
      let mut response = String::new();
      if stream.read_to_string(&mut response).is_ok() && response.starts_with("HTTP/1.1 200") && response.contains("\"status\":\"healthy\"")
      {
        return Ok(());
      }
    }
    std::thread::sleep(Duration::from_millis(25));
  }
  Err(format!("AeorDB did not become ready within {timeout:?}"))
}

#[cfg(unix)]
fn terminate_gracefully(child: &Child) {
  unsafe extern "C" {
    fn kill(process_id: i32, signal: i32) -> i32;
  }
  const SIGTERM: i32 = 15;
  let result = unsafe { kill(child.id() as i32, SIGTERM) };
  assert_eq!(result, 0, "send SIGTERM to AeorDB child");
}

#[test]
fn fatal_initialization_error_terminates_listener_and_exits_nonzero() {
  let temporary_directory = tempfile::tempdir().expect("create temporary directory");
  let database = temporary_directory.path().join("invalid.aeordb");
  std::fs::write(&database, b"not an AeorDB database").expect("write invalid database");
  let runtime_data = temporary_directory.path().join("runtime-data");
  let runtime_temp = temporary_directory.path().join("runtime-temp");
  let hot_directory = temporary_directory.path().join("hot");
  std::fs::create_dir_all(&runtime_data).expect("create runtime data directory");
  std::fs::create_dir_all(&runtime_temp).expect("create runtime temporary directory");
  std::fs::create_dir_all(&hot_directory).expect("create hot directory");

  let child = Command::new(env!("CARGO_BIN_EXE_aeordb"))
    .args([
      "start",
      "--database",
      database.to_str().expect("database path is UTF-8"),
      "--hot-dir",
      hot_directory.to_str().expect("hot path is UTF-8"),
      "--host",
      "127.0.0.1",
      "--port",
      "0",
      "--auth",
      "disabled",
    ])
    .env("XDG_DATA_HOME", &runtime_data)
    .env("TMPDIR", &runtime_temp)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("start aeordb child");

  let (output, timed_out) = wait_for_output(child, Duration::from_secs(5));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(!timed_out, "fatal initialization left the listener running:\n{stderr}");
  assert!(!output.status.success(), "fatal initialization must return a nonzero status");
  assert!(stderr.contains("Startup error:"), "expected a surfaced startup error, got:\n{stderr}");
  assert!(!stderr.contains("panicked at"), "startup failure must not unwind:\n{stderr}");
}

#[test]
fn fatal_initialization_error_terminates_tls_listener_and_exits_nonzero() {
  let temporary_directory = tempfile::tempdir().expect("create temporary directory");
  let database = temporary_directory.path().join("invalid.aeordb");
  std::fs::write(&database, b"not an AeorDB database").expect("write invalid database");
  let runtime_data = temporary_directory.path().join("runtime-data");
  let runtime_temp = temporary_directory.path().join("runtime-temp");
  let hot_directory = temporary_directory.path().join("hot");
  let certificate = temporary_directory.path().join("localhost-cert.pem");
  let private_key = temporary_directory.path().join("localhost-key.pem");
  std::fs::create_dir_all(&runtime_data).expect("create runtime data directory");
  std::fs::create_dir_all(&runtime_temp).expect("create runtime temporary directory");
  std::fs::create_dir_all(&hot_directory).expect("create hot directory");
  std::fs::write(&certificate, include_bytes!("fixtures/localhost-cert.pem")).expect("write TLS certificate");
  std::fs::write(&private_key, include_bytes!("fixtures/localhost-key.pem")).expect("write TLS private key");

  let child = Command::new(env!("CARGO_BIN_EXE_aeordb"))
    .args([
      "start",
      "--database",
      database.to_str().expect("database path is UTF-8"),
      "--hot-dir",
      hot_directory.to_str().expect("hot path is UTF-8"),
      "--host",
      "127.0.0.1",
      "--port",
      "0",
      "--auth",
      "disabled",
      "--tls-cert",
      certificate.to_str().expect("certificate path is UTF-8"),
      "--tls-key",
      private_key.to_str().expect("private-key path is UTF-8"),
    ])
    .env("XDG_DATA_HOME", &runtime_data)
    .env("TMPDIR", &runtime_temp)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("start aeordb TLS child");

  let (output, timed_out) = wait_for_output(child, Duration::from_secs(5));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(!timed_out, "fatal initialization left the TLS listener running:\n{stderr}");
  assert!(!output.status.success(), "fatal TLS initialization must return a nonzero status");
  assert!(stderr.contains("Startup error:"), "expected a surfaced startup error, got:\n{stderr}");
  assert!(!stderr.contains("panicked at"), "TLS startup failure must not unwind:\n{stderr}");
}

#[cfg(unix)]
#[test]
fn successful_startup_remains_serving_until_signal_and_exits_cleanly() {
  let temporary_directory = tempfile::tempdir().expect("create temporary directory");
  let database = temporary_directory.path().join("healthy.aeordb");
  let runtime_data = temporary_directory.path().join("runtime-data");
  let runtime_temp = temporary_directory.path().join("runtime-temp");
  let hot_directory = temporary_directory.path().join("hot");
  std::fs::create_dir_all(&runtime_data).expect("create runtime data directory");
  std::fs::create_dir_all(&runtime_temp).expect("create runtime temporary directory");
  std::fs::create_dir_all(&hot_directory).expect("create hot directory");
  let port = unused_local_port();
  let port_text = port.to_string();

  let mut child = Command::new(env!("CARGO_BIN_EXE_aeordb"))
    .args([
      "start",
      "--database",
      database.to_str().expect("database path is UTF-8"),
      "--hot-dir",
      hot_directory.to_str().expect("hot path is UTF-8"),
      "--host",
      "127.0.0.1",
      "--port",
      &port_text,
      "--auth",
      "disabled",
    ])
    .env("XDG_DATA_HOME", &runtime_data)
    .env("TMPDIR", &runtime_temp)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("start healthy AeorDB child");

  if let Err(error) = wait_until_ready(&mut child, port, Duration::from_secs(10)) {
    terminate_gracefully(&child);
    let (output, _) = wait_for_output(child, Duration::from_secs(10));
    panic!("{error}\nstdout:\n{}\nstderr:\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
  }
  terminate_gracefully(&child);
  let (output, timed_out) = wait_for_output(child, Duration::from_secs(10));
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(!timed_out, "graceful shutdown hung:\n{stderr}");
  assert!(output.status.success(), "graceful shutdown failed:\n{stderr}");
  assert!(String::from_utf8_lossy(&output.stdout).contains("Server shut down gracefully."));
}
