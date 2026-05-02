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
use aeordb::server::{create_app_with_auth_mode, create_engine_with_hot_dir};

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
  // Resolve hot directory: use --hot-dir if specified, otherwise default to
  // the database file's parent directory.
  let default_hot_dir = Path::new(database)
    .parent()
    .unwrap_or(Path::new("."))
    .to_path_buf();
  let hot_dir = hot_dir_arg
    .map(|s| std::path::PathBuf::from(s))
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

  // Build the app (single engine open — no separate bootstrap engine).
  let (application, file_bootstrap_key, engine, event_bus, task_queue) = create_app_with_auth_mode(database, &auth_mode, Some(hot_dir_ref), cors_flag);

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

  // Create a CancellationToken shared by all background tasks and the server.
  let cancel = CancellationToken::new();

  // Start the heartbeat task (clock-sync only, every 15 seconds).
  // TODO: replace hard-coded node_id=1 with a configured value once
  // multi-node support is wired up.
  let heartbeat_handle = spawn_heartbeat(event_bus.clone(), 1, cancel.clone());

  // Start the KV hot buffer flush timer (250ms).
  // Flushes buffered KV entries to the hot tail for crash recovery.
  {
    let engine_for_timer = engine.clone();
    let cancel_for_timer = cancel.clone();
    tokio::spawn(async move {
      let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
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

    // axum's graceful shutdown waits for ALL active connections to close.
    // Long-lived connections (SSE, slow downloads) can hang forever.
    // Give connections 5 seconds to drain after the signal, then force exit.
    tokio::select! {
      result = serve_fut => result.map_err(|error| format!("{}", error)),
      _ = async {
        // Wait for the cancellation token (set by shutdown_signal), then timeout
        cancel.cancelled().await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tracing::warn!("Graceful shutdown timed out after 5s — forcing exit");
      } => Ok(()),
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
