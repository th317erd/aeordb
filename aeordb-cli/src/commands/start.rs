use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use aeordb::auth::auth_uri::{AuthMode, resolve_auth_mode};
use aeordb::auth::bootstrap_root_key;
use aeordb::engine::{spawn_heartbeat, spawn_metrics_pulse, spawn_rate_sampler, spawn_webhook_dispatcher, spawn_cron_scheduler, spawn_task_worker, TaskStatus};
use aeordb::engine::rate_tracker::RateTrackerSet;
use aeordb::plugins::PluginManager;
use aeordb::logging::{LogConfig, LogFormat, initialize_logging};
use aeordb::server::create_app_with_auth_mode_and_cancel;

pub async fn run(
  port: u16,
  host: &str,
  database: &str,
  log_format: &str,
  auth_flag: Option<&str>,
  hot_dir_arg: Option<&str>,
  cors_flag: Option<&str>,
  tls_cert: Option<&str>,
  tls_key: Option<&str>,
  _jwt_expiry: i64,
  _chunk_size: usize,
  peers: Vec<String>,
  join_url: Option<&str>,
  join_token: Option<&str>,
  advertise_url: Option<&str>,
) {
  let log_config = LogConfig {
    format: match log_format {
      "json" => LogFormat::Json,
      _ => LogFormat::Pretty,
    },
    ..LogConfig::default()
  };

  initialize_logging(&log_config);

  // Validate TLS flags: must supply both or neither.
  let tls_config = match (tls_cert, tls_key) {
    (Some(cert), Some(key)) => Some((cert.to_string(), key.to_string())),
    (None, None) => None,
    (Some(_), None) => {
      eprintln!("Error: --tls-cert requires --tls-key");
      std::process::exit(1);
    }
    (None, Some(_)) => {
      eprintln!("Error: --tls-key requires --tls-cert");
      std::process::exit(1);
    }
  };

  let auth_mode = resolve_auth_mode(auth_flag);

  let auth_mode_str = match &auth_mode {
    AuthMode::Disabled => "disabled (dev mode)".to_string(),
    AuthMode::SelfContained => "self-contained".to_string(),
    AuthMode::File(path) => format!("file://{}", path),
  };

  tracing::info!(
    port = %port,
    host = %host,
    auth_mode = %auth_mode_str,
    db_path = %database,
    tls = %tls_config.is_some(),
    version = env!("CARGO_PKG_VERSION"),
    "AeorDB starting",
  );

  println!("AeorDB v{}", env!("CARGO_PKG_VERSION"));
  println!("Database: {database}");
  println!("Host: {host}");
  println!("Port: {port}");
  match &auth_mode {
    AuthMode::Disabled => println!("Auth: disabled (dev mode)"),
    AuthMode::SelfContained => println!("Auth: self-contained"),
    AuthMode::File(path) => println!("Auth: file://{path}"),
  }

  // Safety check: if auth is disabled AND the bind address is non-loopback,
  // refuse to start unless the operator opts in via env var. A typo or
  // misconfiguration that puts AEORDB_AUTH=false on a public server
  // exposes the entire database — the audit found this is the worst
  // mis-deploy footgun. Require explicit acknowledgement.
  if matches!(auth_mode, AuthMode::Disabled) {
    let is_loopback = host == "127.0.0.1"
      || host == "::1"
      || host == "localhost";
    if !is_loopback && std::env::var("AEORDB_ALLOW_UNAUTHENTICATED_PUBLIC_BIND").is_err() {
      eprintln!();
      eprintln!("==========================================================");
      eprintln!("REFUSING TO START: auth disabled with non-loopback bind");
      eprintln!("==========================================================");
      eprintln!("Host: {host}");
      eprintln!();
      eprintln!("Running with auth disabled on a non-loopback address exposes");
      eprintln!("the entire database to anyone who can reach this port.");
      eprintln!();
      eprintln!("To proceed (you are sure this is dev / inside a private network /");
      eprintln!("behind another auth layer), set the environment variable:");
      eprintln!();
      eprintln!("    AEORDB_ALLOW_UNAUTHENTICATED_PUBLIC_BIND=1");
      eprintln!();
      eprintln!("Otherwise bind to 127.0.0.1 or enable --auth self.");
      std::process::exit(1);
    } else if !is_loopback {
      eprintln!();
      eprintln!("WARNING: auth is disabled and bind address ({host}) is not loopback.");
      eprintln!("         AEORDB_ALLOW_UNAUTHENTICATED_PUBLIC_BIND is set — proceeding.");
      eprintln!();
    }
  }
  // Resolve hot directory: use --hot-dir if specified, otherwise default to
  // the database file's parent directory.
  let default_hot_dir = Path::new(database)
    .parent()
    .unwrap_or(Path::new("."))
    .to_path_buf();
  let hot_dir = hot_dir_arg
    .map(std::path::PathBuf::from)
    .unwrap_or(default_hot_dir);
  let hot_dir_ref = hot_dir.as_path();

  println!("Hot dir: {}", hot_dir_ref.display());
  match cors_flag {
    Some("*") => println!("CORS: allow all origins"),
    Some(origins) => println!("CORS: {origins}"),
    None => println!("CORS: disabled"),
  }
  if tls_config.is_some() {
    println!("TLS: enabled");
  }
  println!();

  // If --join was supplied, perform the cluster join BEFORE the app opens
  // the engine for serving. The join writes the cluster's JWT signing key
  // into the local system_store; the JwtManager then loads it during
  // create_app_with_auth_mode and JWTs validate cluster-wide.
  if let Some(join_url) = join_url {
    let token = join_token.expect("--join requires --join-token");
    if let Err(e) = perform_cluster_join(database, hot_dir_ref, join_url, token, port, advertise_url).await {
      eprintln!("Error: --join failed: {}", e);
      std::process::exit(1);
    }
    println!("Cluster join complete. Adopting shared signing key.");
  }

  // Register any --peers URLs into the system store before serving.
  if !peers.is_empty() {
    if let Err(e) = register_initial_peers(database, hot_dir_ref, &peers) {
      eprintln!("Warning: failed to register some --peers: {}", e);
    } else {
      println!("Registered {} peer(s) from --peers", peers.len());
    }
  }

  // Create a CancellationToken shared by all background tasks (including
  // the sync loop spawned inside create_app_with_auth_mode_and_cancel) and
  // the HTTP server below.
  let cancel = CancellationToken::new();

  // Build the app (single engine open — no separate bootstrap engine).
  // We use the *_and_cancel variant so the sync loop's shutdown is wired
  // to this token; without it, the loop runs until the process is killed.
  let (application, file_bootstrap_key, engine, event_bus, task_queue) =
    create_app_with_auth_mode_and_cancel(
      database,
      &auth_mode,
      Some(hot_dir_ref),
      cors_flag,
      Some(cancel.clone()),
    );

  // For SelfContained mode, bootstrap the root key using the already-open engine.
  if auth_mode == AuthMode::SelfContained {
    if let Some(root_key) = bootstrap_root_key(&engine).unwrap_or(None) {
      println!("==========================================================");
      println!("  ROOT API KEY (shown once, save it now!):");
      println!("  {root_key}");
      println!("==========================================================");
      println!();
    }
  }

  if let Some(root_key) = file_bootstrap_key {
    println!("==========================================================");
    println!("  ROOT API KEY (shown once, save it now!):");
    println!("  {root_key}");
    println!("==========================================================");
    println!();
  }

  // Start the heartbeat task (clock-sync only, every 15 seconds).
  // TODO: replace hard-coded node_id=1 with a configured value once
  // multi-node support is wired up.
  let heartbeat_handle = spawn_heartbeat(event_bus.clone(), 1, cancel.clone());

  // Start the KV hot buffer flush timer (100ms).
  // Flushes buffered KV entries to the hot tail for crash recovery.
  {
    let engine_for_timer = engine.clone();
    let cancel_for_timer = cancel.clone();
    tokio::spawn(async move {
      let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
      loop {
        interval.tick().await;
        if cancel_for_timer.is_cancelled() { break; }
        engine_for_timer.try_flush_hot_buffer();
      }
    });
  }

  // Start the rate sampler (1 Hz) and metrics pulse (15s) for detailed stats.
  let counters = engine.counters().clone();
  let rate_trackers = Arc::new(RateTrackerSet::new());
  let sampler_handle = spawn_rate_sampler(counters.clone(), rate_trackers.clone(), cancel.clone());
  let metrics_handle = spawn_metrics_pulse(
    event_bus.clone(),
    counters,
    rate_trackers.clone(),
    database.to_string(),
    cancel.clone(),
  );

  // Make rate_trackers and db_path available to the stats endpoint via Extension.
  let application = application
    .layer(axum::Extension(rate_trackers))
    .layer(axum::Extension(database.to_string()));

  // Reset any tasks left in Running state from a previous crash.
  if let Ok(tasks) = task_queue.list_tasks() {
    for task in &tasks {
      if task.status == TaskStatus::Running {
        let _ = task_queue.update_status(&task.id, TaskStatus::Pending, None);
      }
    }
  }

  // Write default global index config if it doesn't exist.
  {
    let ops = aeordb::engine::DirectoryOps::new(&engine);
    let ctx = aeordb::engine::RequestContext::system();
    let config_path = "/.config/indexes.json";

    match ops.read_file(config_path) {
      Ok(_) => {
        // Config exists — don't overwrite.
      }
      Err(_) => {
        let default_config = serde_json::json!({
          "glob": "**/*",
          "indexes": [
            {"name": "@filename", "type": ["string", "trigram", "phonetic", "dmetaphone"]},
            {"name": "@hash", "type": "trigram"},
            {"name": "@created_at", "type": "timestamp"},
            {"name": "@updated_at", "type": "timestamp"},
            {"name": "@size", "type": "u64"},
            {"name": "@content_type", "type": "string"}
          ]
        });
        let config_bytes = serde_json::to_vec_pretty(&default_config).unwrap();
        if let Err(e) = ops.store_file(&ctx, config_path, &config_bytes, Some("application/json")) {
          tracing::warn!("Failed to write default index config: {}", e);
        } else {
          tracing::info!("Created default global index config");
          // Enqueue initial reindex.
          let _ = task_queue.enqueue("reindex", serde_json::json!({"path": "/"}));
          tracing::info!("Enqueued initial global reindex");
        }
      }
    }
  }

  // Seed default cron schedules on first start (hourly cleanup, daily GC).
  // No-op if a cron config already exists.
  if let Err(error) = aeordb::engine::seed_default_cron_if_missing(&engine) {
    tracing::warn!("Failed to seed default cron schedules: {}", error);
  }

  // Start the cron scheduler (enqueues tasks based on cron config every 60s).
  let cron_handle = spawn_cron_scheduler(task_queue.clone(), engine.clone(), event_bus.clone(), cancel.clone());

  // Start the task worker (dequeues and executes background tasks).
  let plugin_manager = std::sync::Arc::new(PluginManager::new(engine.clone()));
  let worker_handle = spawn_task_worker(
    task_queue,
    engine.clone(),
    plugin_manager,
    event_bus.clone(),
    cancel.clone(),
  );

  // Start the webhook dispatcher (delivers matching events to registered URLs).
  let webhook_handle = spawn_webhook_dispatcher(event_bus, engine.clone(), cancel.clone());

  let startup_instant = std::time::Instant::now();

  // Parse the host address into an IP.
  let bind_address: std::net::IpAddr = host.parse().unwrap_or_else(|_| {
    eprintln!("Error: invalid host address '{host}'");
    std::process::exit(1);
  });
  let address = SocketAddr::from((bind_address, port));

  let server_result = if let Some((cert_path, key_path)) = tls_config {
    // TLS path: use axum_server with rustls + Handle for graceful shutdown
    println!("Listening on https://{address}");

    let rustls_config = match axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path).await {
      Ok(config) => config,
      Err(error) => {
        eprintln!("Failed to load TLS certificate/key: {error}");
        eprintln!("  cert: {cert_path}");
        eprintln!("  key:  {key_path}");
        std::process::exit(1);
      }
    };

    let handle = axum_server::Handle::new();
    let shutdown_handle = handle.clone();
    let server_cancel = cancel.clone();
    tokio::spawn(async move {
      shutdown_signal().await;
      server_cancel.cancel();
      shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });

    axum_server::bind_rustls(address, rustls_config)
      .handle(handle)
      .serve(application.into_make_service())
      .await
      .map_err(|error| format!("{}", error))
  } else {
    // Non-TLS path: standard axum::serve
    println!("Listening on http://{address}");

    let listener = match tokio::net::TcpListener::bind(address).await {
      Ok(listener) => listener,
      Err(error) => {
        eprintln!("Failed to bind to {address}: {error}");
        std::process::exit(1);
      }
    };

    let server_cancel = cancel.clone();
    let shutdown_fut = async move {
      shutdown_signal().await;
      server_cancel.cancel();
    };

    let serve_fut = axum::serve(listener, application)
      .with_graceful_shutdown(shutdown_fut);

    // axum's graceful shutdown waits for ALL active connections to close,
    // but long-lived connections (SSE) never close. Once the shutdown
    // signal fires and the cancellation token is set, drop the serve
    // future immediately and proceed to cleanup (background tasks +
    // engine flush). Connections are dropped when the process exits.
    tokio::select! {
      result = serve_fut => result.map_err(|error| format!("{}", error)),
      _ = cancel.cancelled() => Ok(()),
    }
  };

  if let Err(error) = server_result {
    eprintln!("Server error: {error}");
    cancel.cancel();
    // Give background tasks a moment to notice cancellation
    let _ = tokio::time::timeout(
      std::time::Duration::from_secs(5),
      futures_join_all(vec![heartbeat_handle, sampler_handle, metrics_handle, webhook_handle, cron_handle, worker_handle]),
    ).await;
    engine.shutdown().ok();
    std::process::exit(1);
  }

  // Wait for background tasks to finish (with a timeout).
  tracing::info!("Waiting for background tasks to finish...");
  let _ = tokio::time::timeout(
    std::time::Duration::from_secs(10),
    futures_join_all(vec![heartbeat_handle, sampler_handle, metrics_handle, webhook_handle, cron_handle, worker_handle]),
  ).await;

  // Flush engine buffers and sync to disk.
  engine.shutdown().ok();
  let uptime = startup_instant.elapsed().as_secs();
  tracing::info!(uptime_seconds = uptime, "AeorDB shutting down");
  println!("Server shut down gracefully.");
}

/// Wait for all join handles to complete.
async fn futures_join_all(handles: Vec<tokio::task::JoinHandle<()>>) {
  for handle in handles {
    let _ = handle.await;
  }
}

/// Listen for shutdown signals (SIGINT and SIGTERM on Unix, Ctrl+C everywhere).
async fn shutdown_signal() {
  let ctrl_c = async {
    tokio::signal::ctrl_c()
      .await
      .expect("failed to install CTRL+C handler");
  };

  #[cfg(unix)]
  let terminate = async {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
      .expect("failed to install SIGTERM handler")
      .recv()
      .await;
  };

  #[cfg(not(unix))]
  let terminate = std::future::pending::<()>();

  tokio::select! {
    _ = ctrl_c => {},
    _ = terminate => {},
  }

  println!("\nReceived shutdown signal");
}

// ---------------------------------------------------------------------------
// Cluster bootstrap helpers
// ---------------------------------------------------------------------------

/// POST /sync/join against an existing cluster member. Writes the returned
/// signing key and peer record into the local engine so that
/// create_app_with_auth_mode loads the cluster's shared key.
async fn perform_cluster_join(
  database: &str,
  hot_dir: &std::path::Path,
  join_url: &str,
  join_token: &str,
  local_port: u16,
  advertise_url: Option<&str>,
) -> Result<(), String> {
  use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

  // Determine the URL the responding node will use to reach us back. If the
  // operator supplied --advertise-url, use that verbatim. Otherwise fall
  // back to http://localhost:PORT and print a loud warning, since localhost
  // is unreachable from any other host.
  let our_url = match advertise_url {
    Some(url) => url.trim_end_matches('/').to_string(),
    None => {
      eprintln!(
        "Warning: --advertise-url not set; advertising http://localhost:{} \
         to the join target. The peer will be unable to reach this node from \
         a different host. Pass --advertise-url https://your-host:{} on a \
         multi-host cluster.",
        local_port, local_port
      );
      format!("http://localhost:{}", local_port)
    }
  };

  // The /sync/join endpoint expects an Authorization header. The
  // join_token may be either a raw API key or a JWT. If it looks like
  // an API key (aeor_k_... prefix), exchange it for a JWT first.
  let bearer = if join_token.starts_with("aeor_k_") {
    let token_resp = reqwest::Client::new()
      .post(format!("{}/auth/token", join_url.trim_end_matches('/')))
      .json(&serde_json::json!({ "api_key": join_token }))
      .send()
      .await
      .map_err(|e| format!("token exchange request failed: {}", e))?;
    let token_json: serde_json::Value = token_resp.json().await
      .map_err(|e| format!("token exchange response parse failed: {}", e))?;
    token_json.get("token").and_then(|v| v.as_str()).map(String::from)
      .ok_or_else(|| format!("token exchange did not return a token: {}", token_json))?
  } else {
    join_token.to_string()
  };

  let resp = reqwest::Client::new()
    .post(format!("{}/sync/join", join_url.trim_end_matches('/')))
    .bearer_auth(&bearer)
    .json(&serde_json::json!({ "node_url": our_url }))
    .send()
    .await
    .map_err(|e| format!("HTTP request failed: {}", e))?;

  if !resp.status().is_success() {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    return Err(format!("status {}: {}", status, body));
  }

  let body: serde_json::Value = resp.json().await
    .map_err(|e| format!("response parse failed: {}", e))?;

  let signing_key_b64 = body.get("signing_key").and_then(|v| v.as_str())
    .ok_or_else(|| "response missing 'signing_key'".to_string())?;
  let signing_key = B64.decode(signing_key_b64)
    .map_err(|e| format!("invalid base64 signing key: {}", e))?;

  let responding_node_id = body.get("responding_node_id").and_then(|v| v.as_u64())
    .ok_or_else(|| "response missing 'responding_node_id'".to_string())?;

  // Open the engine, write the signing key + peer, then drop it so the
  // server can re-open it normally.
  let engine = aeordb::engine::StorageEngine::open_with_hot_dir(database, Some(hot_dir))
    .or_else(|_| aeordb::engine::StorageEngine::create_with_hot_dir(database, Some(hot_dir)))
    .map_err(|e| format!("failed to open engine for join: {}", e))?;

  let ctx = aeordb::engine::RequestContext::system();
  aeordb::engine::system_store::store_config(&engine, &ctx, "jwt_signing_key", &signing_key)
    .map_err(|e| format!("failed to store signing key: {}", e))?;

  // Register the responding node as a peer.
  let peer_config = aeordb::engine::PeerConfig {
    node_id: responding_node_id,
    address: join_url.to_string(),
    label: Some("Join target".to_string()),
    sync_paths: None,
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  };
  let mut peer_configs = aeordb::engine::system_store::get_peer_configs(&engine).unwrap_or_default();
  peer_configs.retain(|p| p.address != peer_config.address);
  peer_configs.push(peer_config);
  aeordb::engine::system_store::store_peer_configs(&engine, &ctx, &peer_configs)
    .map_err(|e| format!("failed to store peer config: {}", e))?;

  drop(engine);
  Ok(())
}

/// Write peer configs for --peers URLs into the engine's system store.
fn register_initial_peers(
  database: &str,
  hot_dir: &std::path::Path,
  peers: &[String],
) -> Result<(), String> {
  let engine = aeordb::engine::StorageEngine::open_with_hot_dir(database, Some(hot_dir))
    .or_else(|_| aeordb::engine::StorageEngine::create_with_hot_dir(database, Some(hot_dir)))
    .map_err(|e| format!("failed to open engine: {}", e))?;

  let ctx = aeordb::engine::RequestContext::system();
  let mut peer_configs = aeordb::engine::system_store::get_peer_configs(&engine).unwrap_or_default();

  for url in peers {
    if peer_configs.iter().any(|p| &p.address == url) { continue; }
    peer_configs.push(aeordb::engine::PeerConfig {
      node_id: rand::random(),
      address: url.clone(),
      label: None,
      sync_paths: None,
      last_clock_offset_ms: None,
      last_wire_time_ms: None,
      last_jitter_ms: None,
      clock_state_at: None,
    });
  }

  aeordb::engine::system_store::store_peer_configs(&engine, &ctx, &peer_configs)
    .map_err(|e| format!("failed to store peer configs: {}", e))?;

  drop(engine);
  Ok(())
}
