use std::net::SocketAddr;
use std::path::Path;

use aeordb::auth::auth_uri::{AuthMode, resolve_auth_mode};
use aeordb::auth::bootstrap_root_key;
use aeordb::engine::{spawn_heartbeat, spawn_webhook_dispatcher};
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

  let (application, file_bootstrap_key, engine, event_bus) = create_app_with_auth_mode(database, &auth_mode, Some(hot_dir_ref), cors_flag);

  if let Some(root_key) = file_bootstrap_key {
    println!("==========================================================");
    println!("  ROOT API KEY (shown once, save it now!):");
    println!("  {root_key}");
    println!("==========================================================");
    println!();
  }

  // Start the heartbeat task (emits DatabaseStats every 15 seconds).
  let heartbeat_handle = spawn_heartbeat(event_bus.clone(), engine.clone());

  // Start the webhook dispatcher (delivers matching events to registered URLs).
  let webhook_handle = spawn_webhook_dispatcher(event_bus, engine);

  let address = SocketAddr::from(([0, 0, 0, 0], port));
  println!("Listening on http://{address}");

  let listener = match tokio::net::TcpListener::bind(address).await {
    Ok(listener) => listener,
    Err(error) => {
      eprintln!("Failed to bind to {address}: {error}");
      std::process::exit(1);
    }
  };

  if let Err(error) = axum::serve(listener, application)
    .with_graceful_shutdown(shutdown_signal())
    .await
  {
    eprintln!("Server error: {error}");
    heartbeat_handle.abort();
    webhook_handle.abort();
    std::process::exit(1);
  }

  heartbeat_handle.abort();
  webhook_handle.abort();
  println!("Server shut down gracefully.");
}

async fn shutdown_signal() {
  tokio::signal::ctrl_c()
    .await
    .expect("failed to install CTRL+C handler");
  println!("\nShutting down...");
}
