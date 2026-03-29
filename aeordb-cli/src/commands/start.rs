use std::sync::Arc;
use std::net::SocketAddr;

use aeordb::auth::bootstrap_root_key;
use aeordb::logging::{LogConfig, LogFormat, initialize_logging};
use aeordb::server::create_app;
use aeordb::storage::RedbStorage;

pub async fn run(port: u16, database: &str, log_format: &str) {
  let log_config = LogConfig {
    format: match log_format {
      "json" => LogFormat::Json,
      _ => LogFormat::Pretty,
    },
    ..LogConfig::default()
  };

  initialize_logging(&log_config);

  println!("AeorDB v{}", env!("CARGO_PKG_VERSION"));
  println!("Database: {database}");
  println!("Port: {port}");
  println!();

  let storage = match RedbStorage::new(database) {
    Ok(storage) => Arc::new(storage),
    Err(error) => {
      eprintln!("Failed to open database at '{database}': {error}");
      std::process::exit(1);
    }
  };

  if let Some(root_key) = bootstrap_root_key(&storage) {
    println!("==========================================================");
    println!("  ROOT API KEY (shown once, save it now!):");
    println!("  {root_key}");
    println!("==========================================================");
    println!();
  }

  let application = create_app(storage);

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
    std::process::exit(1);
  }

  println!("Server shut down gracefully.");
}

async fn shutdown_signal() {
  tokio::signal::ctrl_c()
    .await
    .expect("failed to install CTRL+C handler");
  println!("\nShutting down...");
}
