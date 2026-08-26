#![cfg(unix)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use aeordb::engine::HashAlgorithm;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

fn unused_local_port() -> u16 {
  TcpListener::bind(("127.0.0.1", 0)).expect("reserve local port").local_addr().expect("read local address").port()
}

fn send_sigterm(child: &Child) -> bool {
  unsafe extern "C" {
    fn kill(process_id: i32, signal: i32) -> i32;
  }
  const SIGTERM: i32 = 15;
  unsafe { kill(child.id() as i32, SIGTERM) == 0 }
}

fn terminate_gracefully(child: &Child) {
  assert!(send_sigterm(child), "send SIGTERM to live AeorDB child");
}

fn collect_output(mut child: Child, timeout: Duration) -> (Output, bool) {
  let deadline = Instant::now() + timeout;
  loop {
    if child.try_wait().expect("poll live AeorDB child").is_some() {
      return (child.wait_with_output().expect("collect live AeorDB output"), false);
    }
    if Instant::now() >= deadline {
      child.kill().expect("kill hung live AeorDB child");
      return (child.wait_with_output().expect("collect timed-out live AeorDB output"), true);
    }
    std::thread::sleep(Duration::from_millis(25));
  }
}

struct LiveServer {
  child: Option<Child>,
  base_url: String,
  database: PathBuf,
  _temporary_directory: tempfile::TempDir,
}

impl LiveServer {
  async fn start(auth: &str, cors_origins: Option<&str>) -> Self {
    let temporary_directory = tempfile::tempdir().expect("create live route temporary directory");
    let database = temporary_directory.path().join("live.aeordb");
    let hot_directory = temporary_directory.path().join("hot");
    let runtime_data = temporary_directory.path().join("runtime-data");
    let runtime_temp = temporary_directory.path().join("runtime-temp");
    let spill_directory = runtime_data.join("emergency-spill");
    for directory in [&hot_directory, &runtime_data, &runtime_temp, &spill_directory] {
      std::fs::create_dir_all(directory).expect("create live route runtime directory");
    }

    let port = unused_local_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_aeordb"));
    command.args([
      "start",
      "--database",
      database.to_str().expect("database path is UTF-8"),
      "--hot-dir",
      hot_directory.to_str().expect("hot path is UTF-8"),
      "--host",
      "127.0.0.1",
      "--port",
      &port.to_string(),
      "--auth",
      auth,
      "--log-format",
      "compact",
    ]);
    if let Some(origins) = cors_origins {
      command.args(["--cors-origins", origins]);
    }
    let mut child = command
      .env("XDG_DATA_HOME", &runtime_data)
      .env("TMPDIR", &runtime_temp)
      .env("AEORDB_RECOVERY_EMERGENCY_SPILL_DIR", &spill_directory)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .expect("start live AeorDB child");
    let base_url = format!("http://127.0.0.1:{port}");
    let client = http_client();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      if let Some(status) = child.try_wait().expect("poll live AeorDB startup") {
        let output = child.wait_with_output().expect("collect failed live AeorDB startup");
        panic!(
          "live AeorDB exited before readiness with {status}\nstdout:\n{}\nstderr:\n{}",
          String::from_utf8_lossy(&output.stdout),
          String::from_utf8_lossy(&output.stderr)
        );
      }
      if let Ok(response) = client.get(format!("{base_url}/system/health")).send().await {
        if response.status() == StatusCode::OK {
          let body = response.json::<Value>().await.expect("decode live health response");
          if body["status"] == "healthy" {
            break;
          }
        }
      }
      if Instant::now() >= deadline {
        terminate_gracefully(&child);
        let (output, _) = collect_output(child, Duration::from_secs(10));
        panic!(
          "live AeorDB did not become healthy\nstdout:\n{}\nstderr:\n{}",
          String::from_utf8_lossy(&output.stdout),
          String::from_utf8_lossy(&output.stderr)
        );
      }
      tokio::time::sleep(Duration::from_millis(25)).await;
    }

    Self { child: Some(child), base_url, database, _temporary_directory: temporary_directory }
  }

  fn url(&self, path: &str) -> String {
    format!("{}{path}", self.base_url)
  }

  fn stop(mut self) {
    let child = self.child.take().expect("live AeorDB child is present");
    terminate_gracefully(&child);
    let (output, timed_out) = collect_output(child, Duration::from_secs(20));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!timed_out, "live AeorDB graceful shutdown timed out\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(output.status.success(), "live AeorDB graceful shutdown failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("Server shut down gracefully."), "live AeorDB did not report graceful shutdown\nstdout:\n{stdout}");

    let verify = Command::new(env!("CARGO_BIN_EXE_aeordb"))
      .args(["verify", "--database", self.database.to_str().expect("database path is UTF-8")])
      .env("TMPDIR", self._temporary_directory.path().join("runtime-temp"))
      .output()
      .expect("verify live route database");
    assert!(
      verify.status.success() && String::from_utf8_lossy(&verify.stdout).contains("Status: OK"),
      "live route database verification failed\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&verify.stdout),
      String::from_utf8_lossy(&verify.stderr)
    );
  }
}

impl Drop for LiveServer {
  fn drop(&mut self) {
    let Some(mut child) = self.child.take() else {
      return;
    };
    let _ = send_sigterm(&child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
      if child.try_wait().ok().flatten().is_some() {
        return;
      }
      std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
  }
}

fn http_client() -> Client {
  Client::builder().connect_timeout(Duration::from_secs(2)).timeout(Duration::from_secs(20)).build().expect("build live HTTP client")
}

fn chunk_hash(data: &[u8]) -> String {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"chunk:");
  input.extend_from_slice(data);
  hex::encode(HashAlgorithm::Blake3_256.compute_hash(&input).expect("compute chunk hash"))
}

#[tokio::test(flavor = "current_thread")]
async fn actual_cli_preserves_six_class_v3_routes_streaming_and_http_fallbacks() {
  let server = LiveServer::start("disabled", Some("*")).await;
  let client = http_client();

  let health = client.get(server.url("/system/health")).send().await.expect("request live health");
  assert_eq!(health.status(), StatusCode::OK);
  assert_eq!(health.json::<Value>().await.expect("decode live health")["status"], "healthy");

  let unknown = client.get(server.url("/__root_route_matrix_missing__")).send().await.expect("request unknown route");
  assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
  let wrong_method = client.post(server.url("/system/health")).send().await.expect("request undeclared method");
  assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);

  let preflight = client
    .request(reqwest::Method::OPTIONS, server.url("/files"))
    .header("origin", "https://matrix.example")
    .header("access-control-request-method", "GET")
    .send()
    .await
    .expect("request CORS preflight");
  assert!(preflight.status().is_success(), "CORS preflight failed with {}", preflight.status());
  assert_eq!(preflight.headers().get("access-control-allow-origin").expect("CORS allow-origin header"), "*");

  let streamed_bytes = (0..(3 * 1_024 * 1_024 + 17)).map(|index| ((index * 31 + 7) % 251) as u8).collect::<Vec<_>>();
  let store = client
    .put(server.url("/files/qualification/stream.bin"))
    .header("content-type", "application/octet-stream")
    .body(streamed_bytes.clone())
    .send()
    .await
    .expect("stream file into live server");
  assert_eq!(store.status(), StatusCode::CREATED);
  assert!(store.headers().get("x-aeordb-root-hash").is_none(), "inactive v4 mutation emitted root metadata");
  let stored = store.json::<Value>().await.expect("decode store response");
  let file_hash = stored["hash"].as_str().expect("store response hash").to_string();

  let read = client.get(server.url("/files/qualification/stream.bin")).send().await.expect("read live streamed file");
  assert_eq!(read.status(), StatusCode::OK);
  let read_root_hash = read.headers().get("x-aeordb-root-hash").expect("file read root hash").to_str().unwrap().to_string();
  assert_eq!(read_root_hash.len(), 64);
  assert_eq!(read.headers().get("x-aeordb-root-state").expect("file read root state"), "live");
  assert_eq!(read.headers().get("x-aeordb-root-expires-at").expect("file read root expiry"), "");
  assert_eq!(read.bytes().await.expect("read streamed response bytes").as_ref(), streamed_bytes);

  let head = client.head(server.url("/files/qualification/stream.bin")).send().await.expect("HEAD live streamed file");
  assert_eq!(head.status(), StatusCode::OK);
  assert_eq!(head.headers().get("x-aeordb-root-hash").expect("HEAD root hash"), &read_root_hash);
  assert_eq!(head.headers().get("x-aeordb-root-state").expect("HEAD root state"), "live");
  assert_eq!(head.headers().get("x-aeordb-root-expires-at").expect("HEAD root expiry"), "");
  assert!(head.bytes().await.expect("read HEAD response").is_empty());

  let listing = client.get(server.url("/files/qualification")).send().await.expect("list live directory");
  assert_eq!(listing.status(), StatusCode::OK);
  let listing = listing.json::<Value>().await.expect("decode live listing");
  assert_eq!(listing["root"]["hash"], read_root_hash);
  assert_eq!(listing["root"]["state"], "live");
  assert!(listing["root"]["expires_at"].is_null());
  assert!(listing.to_string().contains("stream.bin"), "live listing omitted streamed file: {listing}");

  let by_hash = client.get(server.url(&format!("/blobs/{file_hash}"))).send().await.expect("fetch live file by hash");
  assert_eq!(by_hash.status(), StatusCode::OK);
  assert_eq!(by_hash.headers().get("x-aeordb-root-hash").expect("hash read root hash"), &read_root_hash);
  assert_eq!(by_hash.headers().get("x-aeordb-root-state").expect("hash read root state"), "live");
  assert_eq!(by_hash.headers().get("x-aeordb-root-expires-at").expect("hash read root expiry"), "");
  assert_eq!(by_hash.bytes().await.expect("read hash response").as_ref(), streamed_bytes);

  let config = client.get(server.url("/blobs/config")).send().await.expect("request blob config");
  assert_eq!(config.status(), StatusCode::OK);
  assert!(config.json::<Value>().await.expect("decode blob config")["chunk_size"].as_u64().is_some());

  let blob_bytes = b"live content-staging round trip";
  let blob_hash = chunk_hash(blob_bytes);
  let check = client.post(server.url("/blobs/check")).json(&json!({ "hashes": [&blob_hash] })).send().await.expect("check staged chunk");
  assert_eq!(check.status(), StatusCode::OK);
  assert_eq!(check.json::<Value>().await.expect("decode chunk check")["needed"].as_array().expect("needed array").len(), 1);

  let upload = client
    .put(server.url(&format!("/blobs/chunks/{blob_hash}")))
    .header("content-type", "application/octet-stream")
    .body(blob_bytes.to_vec())
    .send()
    .await
    .expect("upload staged chunk");
  assert!(upload.status().is_success(), "staged chunk upload failed with {}", upload.status());

  let commit = client
    .post(server.url("/blobs/commit"))
    .json(&json!({
      "files": [{
        "path": "/qualification/staged.txt",
        "chunks": [&blob_hash],
        "content_type": "text/plain"
      }]
    }))
    .send()
    .await
    .expect("commit staged file");
  assert_eq!(commit.status(), StatusCode::OK);
  assert_eq!(commit.json::<Value>().await.expect("decode blob commit")["committed"], 1);

  let staged = client.get(server.url("/files/qualification/staged.txt")).send().await.expect("read committed staged file");
  assert_eq!(staged.status(), StatusCode::OK);
  assert_eq!(staged.bytes().await.expect("read staged file").as_ref(), blob_bytes);

  let multi_root = client.post(server.url("/versions/diff")).send().await.expect("request malformed multi-root operation");
  assert_eq!(multi_root.status(), StatusCode::BAD_REQUEST);

  server.stop();
}

#[tokio::test(flavor = "current_thread")]
async fn actual_cli_keeps_public_health_ahead_of_protected_self_contained_auth() {
  let server = LiveServer::start("self", None).await;
  let client = http_client();

  let health = client.get(server.url("/system/health")).send().await.expect("request authenticated-server health");
  assert_eq!(health.status(), StatusCode::OK);
  let protected = client.get(server.url("/files")).send().await.expect("request protected route without credentials");
  assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);

  server.stop();
}
