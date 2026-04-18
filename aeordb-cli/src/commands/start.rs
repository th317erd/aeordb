use std::net::SocketAddr;
use std::path::Path;

use tokio_util::sync::CancellationToken;

use aeordb::auth::auth_uri::{AuthMode, resolve_auth_mode};
use aeordb::auth::bootstrap_root_key;
use aeordb::engine::{spawn_heartbeat, spawn_webhook_dispatcher, spawn_cron_scheduler, spawn_task_worker, TaskStatus};
use aeordb::plugins::PluginManager;
use aeordb::logging::{LogConfig, LogFormat, initialize_logging};
use aeordb::server::{create_app_with_auth_mode, create_engine_with_hot_dir};

pub async fn run(port: u16, database: &str, log_format: &str, auth_flag: Option<&str>, hot_dir_arg: Option<&str>, cors_flag: Option<&str>) {
  let log_config = LogConfig {
    format: match log_format {
      "json" => LogFormat::Json,
      _ => LogFormat::Pretty,
    },
    ..LogConfig::default()
  };

  initialize_logging(&log_config);

  let auth_mode = resolve_auth_mode(auth_flag);

  println!("AeorDB v{}", env!("CARGO_PKG_VERSION"));
  println!("Database: {database}");
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
  println!();

  // For SelfContained mode, bootstrap the root key using the engine before
  // building the app (preserves existing behavior).
  if auth_mode == AuthMode::SelfContained {
    let engine = create_engine_with_hot_dir(database, Some(hot_dir_ref));
    if let Some(root_key) = bootstrap_root_key(&engine) {
      println!("==========================================================");
      println!("  ROOT API KEY (shown once, save it now!):");
      println!("  {root_key}");
      println!("==========================================================");
      println!();
    }
    drop(engine);
  }

  let (application, file_bootstrap_key, engine, event_bus, task_queue) = create_app_with_auth_mode(database, &auth_mode, Some(hot_dir_ref), cors_flag);

  if let Some(root_key) = file_bootstrap_key {
    println!("==========================================================");
    println!("  ROOT API KEY (shown once, save it now!):");
    println!("  {root_key}");
    println!("==========================================================");
    println!();
  }

  // Create a CancellationToken shared by all background tasks and the server.
  let cancel = CancellationToken::new();

  // Start the heartbeat task (emits DatabaseStats every 15 seconds).
  // TODO: replace hard-coded node_id=1 with a configured value once
  // multi-node support is wired up.
  let heartbeat_handle = spawn_heartbeat(event_bus.clone(), engine.clone(), 1, cancel.clone());

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

  let address = SocketAddr::from(([0, 0, 0, 0], port));
  println!("Listening on http://{address}");

  let listener = match tokio::net::TcpListener::bind(address).await {
    Ok(listener) => listener,
    Err(error) => {
      eprintln!("Failed to bind to {address}: {error}");
      std::process::exit(1);
    }
  };

  // Wire axum's graceful shutdown to the cancellation token.
  let server_cancel = cancel.clone();
  let shutdown_fut = async move {
    shutdown_signal().await;
    server_cancel.cancel();
  };

  if let Err(error) = axum::serve(listener, application)
    .with_graceful_shutdown(shutdown_fut)
    .await
  {
    eprintln!("Server error: {error}");
    cancel.cancel();
    // Give background tasks a moment to notice cancellation
    let _ = tokio::time::timeout(
      std::time::Duration::from_secs(5),
      futures_join_all(vec![heartbeat_handle, webhook_handle, cron_handle, worker_handle]),
    ).await;
    engine.shutdown().ok();
    std::process::exit(1);
  }

  // Wait for background tasks to finish (with a timeout).
  tracing::info!("Waiting for background tasks to finish...");
  let _ = tokio::time::timeout(
    std::time::Duration::from_secs(10),
    futures_join_all(vec![heartbeat_handle, webhook_handle, cron_handle, worker_handle]),
  ).await;

  // Flush engine buffers and sync to disk.
  engine.shutdown().ok();
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
