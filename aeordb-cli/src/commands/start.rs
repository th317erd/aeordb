use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use aeordb::auth::auth_uri::{AuthMode, resolve_auth_mode};
use aeordb::auth::bootstrap_root_key;
use aeordb::engine::{
  spawn_heartbeat, spawn_metrics_pulse, spawn_rate_sampler, spawn_webhook_dispatcher, spawn_cron_scheduler, spawn_task_worker,
};
use aeordb::engine::rate_tracker::RateTrackerSet;
use aeordb::plugins::PluginManager;
use aeordb::logging::{LogConfig, LogFormat, try_initialize_logging};
use aeordb::server::try_create_app_with_auth_mode_cancel_progress_and_configuration_overrides;

#[cfg(test)]
#[path = "../../spec/start_internal_spec.rs"]
mod start_internal_spec;

/// All settings the `start` command needs. Built in `main.rs` by merging the
/// clap-parsed CLI flags with the optional config file, then passed to
/// `run` as a single arg. Replaces the previous 15-arg signature.
pub struct StartConfig<'a> {
  pub port: u16,
  pub host: &'a str,
  pub database: &'a str,
  pub log_format: &'a str,
  pub auth_flag: Option<&'a str>,
  pub hot_dir_arg: Option<&'a str>,
  pub cors_flag: Option<&'a str>,
  pub tls_cert: Option<&'a str>,
  pub tls_key: Option<&'a str>,
  pub jwt_expiry: i64,
  pub chunk_size: usize,
  pub peers: Vec<String>,
  pub join_url: Option<&'a str>,
  pub join_token: Option<&'a str>,
  pub advertise_url: Option<&'a str>,
  pub command_line_overrides: aeordb::engine::config_resolver::CommandLineConfigOverrides,
}

#[derive(Clone)]
struct StartupGateState {
  inner: Arc<std::sync::RwLock<StartupGateInner>>,
  started_at: String,
  started_at_instant: std::time::Instant,
}

#[derive(Clone)]
enum StartupGateInner {
  Starting { phase: String, message: String, updated_at: String, progress: f64, eta_seconds: Option<u64> },
  Ready { application: Router },
  Failed { error: String, updated_at: String },
}

struct ServerRuntime {
  engine: Arc<aeordb::engine::StorageEngine>,
  startup_instant: std::time::Instant,
  handles: Vec<tokio::task::JoinHandle<()>>,
}

struct InitConfig {
  database: String,
  auth_mode: AuthMode,
  hot_dir: PathBuf,
  cors_flag: Option<String>,
  peers: Vec<String>,
  join_url: Option<String>,
  join_token: Option<String>,
  advertise_url: Option<String>,
  command_line_overrides: aeordb::engine::config_resolver::CommandLineConfigOverrides,
}

impl StartupGateState {
  fn new() -> Self {
    let now = chrono::Utc::now().to_rfc3339();
    Self {
      inner: Arc::new(std::sync::RwLock::new(StartupGateInner::Starting {
        phase: "binding_http".to_string(),
        message: "AeorDB is binding the HTTP listener".to_string(),
        updated_at: now.clone(),
        progress: 0.0,
        eta_seconds: None,
      })),
      started_at: now,
      started_at_instant: std::time::Instant::now(),
    }
  }

  fn set_phase(&self, phase: impl Into<String>, message: impl Into<String>, progress: f64, eta_seconds: Option<u64>) {
    let mut inner = self.write_inner();
    *inner = StartupGateInner::Starting {
      phase: phase.into(),
      message: message.into(),
      updated_at: chrono::Utc::now().to_rfc3339(),
      progress: progress.clamp(0.0, 1.0),
      eta_seconds,
    };
  }

  fn set_ready(&self, application: Router) {
    *self.write_inner() = StartupGateInner::Ready { application };
  }

  fn set_failed(&self, error: impl Into<String>) {
    *self.write_inner() = StartupGateInner::Failed { error: error.into(), updated_at: chrono::Utc::now().to_rfc3339() };
  }

  fn ready_application(&self) -> Option<Router> {
    match &*self.read_inner() {
      StartupGateInner::Ready { application } => Some(application.clone()),
      _ => None,
    }
  }

  fn read_inner(&self) -> std::sync::RwLockReadGuard<'_, StartupGateInner> {
    self.inner.read().unwrap_or_else(|poisoned| {
      tracing::error!("Startup status lock was poisoned; recovering the last structurally valid state");
      self.inner.clear_poison();
      poisoned.into_inner()
    })
  }

  fn write_inner(&self) -> std::sync::RwLockWriteGuard<'_, StartupGateInner> {
    self.inner.write().unwrap_or_else(|poisoned| {
      tracing::error!("Startup status lock was poisoned; recovering and replacing its state");
      self.inner.clear_poison();
      poisoned.into_inner()
    })
  }

  fn status_payload(&self) -> serde_json::Value {
    let elapsed_ms = self.started_at_instant.elapsed().as_millis() as u64;
    let inner = self.read_inner();
    match &*inner {
      StartupGateInner::Starting { phase, message, updated_at, progress, eta_seconds } => serde_json::json!({
        "status": "starting",
        "phase": phase,
        "message": message,
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": self.started_at,
        "updated_at": updated_at,
        "progress": progress,
        "eta": eta_payload(*eta_seconds),
        "elapsed_ms": elapsed_ms,
      }),
      StartupGateInner::Ready { .. } => serde_json::json!({
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": self.started_at,
        "progress": 1.0,
        "eta": null,
        "elapsed_ms": elapsed_ms,
      }),
      StartupGateInner::Failed { error, updated_at } => serde_json::json!({
        "status": "failed",
        "phase": "startup_failed",
        "message": error,
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": self.started_at,
        "updated_at": updated_at,
        "progress": null,
        "eta": null,
        "elapsed_ms": elapsed_ms,
      }),
    }
  }
}

fn eta_payload(eta_seconds: Option<u64>) -> serde_json::Value {
  match eta_seconds {
    Some(seconds) => {
      let at =
        chrono::Utc::now().checked_add_signed(chrono::Duration::seconds(seconds.min(i64::MAX as u64) as i64)).map(|time| time.to_rfc3339());
      serde_json::json!({
        "seconds": seconds,
        "at": at,
      })
    }
    None => serde_json::Value::Null,
  }
}

fn apply_engine_startup_progress(gate: &StartupGateState, progress: aeordb::engine::EngineStartupProgress) {
  let phase_progress = progress.progress.unwrap_or(0.0).clamp(0.0, 1.0);
  // Storage open/recovery is the only startup phase that can take a long time.
  // Reserve 15%-90% of the overall bar for it, with worker startup finishing
  // the remaining tail.
  let overall_progress = 0.15 + (phase_progress * 0.75);
  gate.set_phase(progress.phase, progress.message, overall_progress, progress.eta_seconds);
}

async fn startup_gate_handler(State(gate): State<StartupGateState>, request: Request<Body>) -> Response {
  if let Some(application) = gate.ready_application() {
    return application.oneshot(request).await.unwrap_or_else(|error| {
      json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        serde_json::json!({
          "status": "error",
          "message": format!("request dispatch failed: {}", error),
        }),
      )
    });
  }

  let path = request.uri().path().to_string();
  let payload = gate.status_payload();
  if path == "/system/health" {
    let code =
      if payload.get("status").and_then(|v| v.as_str()) == Some("failed") { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::OK };
    return json_response(code, payload);
  }

  let code = if payload.get("status").and_then(|v| v.as_str()) == Some("failed") {
    StatusCode::INTERNAL_SERVER_ERROR
  } else {
    StatusCode::SERVICE_UNAVAILABLE
  };
  json_response(code, payload)
}

fn json_response(status: StatusCode, payload: serde_json::Value) -> Response {
  let body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{\"status\":\"error\"}".to_vec());
  Response::builder()
    .status(status)
    .header("content-type", "application/json")
    .body(Body::from(body))
    .unwrap_or_else(|_| Response::new(Body::from("{\"status\":\"error\"}")))
}

async fn supervise_server_and_initialization<InitializationOutput: Send + 'static>(
  mut initialization_task: tokio::task::JoinHandle<Result<InitializationOutput, String>>,
  mut server_task: tokio::task::JoinHandle<Result<(), String>>,
  cancellation: &CancellationToken,
) -> (Result<Result<InitializationOutput, String>, tokio::task::JoinError>, Result<Result<(), String>, tokio::task::JoinError>) {
  tokio::select! {
    initialization_result = &mut initialization_task => {
      if !matches!(&initialization_result, Ok(Ok(_))) {
        cancellation.cancel();
      }
      let server_result = server_task.await;
      (initialization_result, server_result)
    }
    server_result = &mut server_task => {
      cancellation.cancel();
      let initialization_result = initialization_task.await;
      (initialization_result, server_result)
    }
  }
}

pub async fn run(config: StartConfig<'_>) -> Result<(), String> {
  let StartConfig {
    port,
    host,
    database,
    log_format,
    auth_flag,
    hot_dir_arg,
    cors_flag,
    tls_cert,
    tls_key,
    jwt_expiry: _jwt_expiry,
    chunk_size: _chunk_size,
    peers,
    join_url,
    join_token,
    advertise_url,
    command_line_overrides,
  } = config;
  let log_config = LogConfig {
    format: match log_format {
      "json" => LogFormat::Json,
      _ => LogFormat::Pretty,
    },
    ..LogConfig::default()
  };

  try_initialize_logging(&log_config).map_err(|error| format!("Error: {error}"))?;

  // Validate TLS flags: must supply both or neither.
  let tls_config = match (tls_cert, tls_key) {
    (Some(cert), Some(key)) => Some((cert.to_string(), key.to_string())),
    (None, None) => None,
    (Some(_), None) => {
      return Err("Error: --tls-cert requires --tls-key".to_string());
    }
    (None, Some(_)) => {
      return Err("Error: --tls-key requires --tls-cert".to_string());
    }
  };

  let auth_mode = resolve_auth_mode(auth_flag).map_err(|error| format!("Error: invalid authentication configuration: {error}"))?;

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
    let is_loopback = host == "127.0.0.1" || host == "::1" || host == "localhost";
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
      return Err("refusing unauthenticated startup on a non-loopback address".to_string());
    } else if !is_loopback {
      eprintln!();
      eprintln!("WARNING: auth is disabled and bind address ({host}) is not loopback.");
      eprintln!("         AEORDB_ALLOW_UNAUTHENTICATED_PUBLIC_BIND is set — proceeding.");
      eprintln!();
    }
  }
  // Resolve hot directory: use --hot-dir if specified, otherwise default to
  // the database file's parent directory.
  let default_hot_dir = Path::new(database).parent().unwrap_or(Path::new(".")).to_path_buf();
  let hot_dir = hot_dir_arg.map(std::path::PathBuf::from).unwrap_or(default_hot_dir);
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

  // Parse the host address into an IP.
  let bind_address: std::net::IpAddr = host.parse().map_err(|_| format!("Error: invalid host address '{host}'"))?;
  let address = SocketAddr::from((bind_address, port));

  // Create a CancellationToken shared by all background tasks (including
  // the sync loop spawned inside create_app_with_auth_mode_and_cancel) and
  // the HTTP server below.
  let cancel = CancellationToken::new();
  let startup_gate = StartupGateState::new();
  let startup_application = Router::new().fallback(startup_gate_handler).with_state(startup_gate.clone());

  let server_future: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> =
    if let Some((cert_path, key_path)) = tls_config {
      println!("Listening on https://{address}");
      let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .map_err(|error| format!("Failed to load TLS certificate/key: {error}\n  cert: {cert_path}\n  key:  {key_path}"))?;
      let listener = std::net::TcpListener::bind(address).map_err(|error| format!("Failed to bind to {address}: {error}"))?;
      listener.set_nonblocking(true).map_err(|error| format!("Failed to configure listener at {address}: {error}"))?;
      let server = axum_server::from_tcp_rustls(listener, rustls_config)
        .map_err(|error| format!("Failed to configure TLS listener at {address}: {error}"))?;
      let server_cancel = cancel.clone();
      Box::pin(async move {
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        let shutdown_cancel = server_cancel.clone();
        let shutdown_task = tokio::spawn(async move {
          shutdown_cancel.cancelled().await;
          shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
        });
        let result = server.handle(handle).serve(startup_application.into_make_service()).await.map_err(|error| error.to_string());
        shutdown_task.abort();
        result
      })
    } else {
      println!("Listening on http://{address}");
      let listener = tokio::net::TcpListener::bind(address).await.map_err(|error| format!("Failed to bind to {address}: {error}"))?;
      let server_cancel = cancel.clone();
      Box::pin(async move {
        let serve_future = std::future::IntoFuture::into_future(axum::serve(listener, startup_application));
        tokio::pin!(serve_future);
        tokio::select! {
          result = &mut serve_future => result.map_err(|error| error.to_string()),
          _ = server_cancel.cancelled() => Ok(()),
        }
      })
    };

  let init_config = InitConfig {
    database: database.to_string(),
    auth_mode,
    hot_dir,
    cors_flag: cors_flag.map(str::to_string),
    peers,
    join_url: join_url.map(str::to_string),
    join_token: join_token.map(str::to_string),
    advertise_url: advertise_url.map(str::to_string),
    command_line_overrides,
  };

  let init_gate = startup_gate.clone();
  let init_cancel = cancel.clone();
  let init_task = tokio::spawn(async move {
    match initialize_server_runtime(init_config, init_cancel, init_gate.clone(), port).await {
      Ok(runtime) => Ok(runtime),
      Err(error) => {
        init_gate.set_failed(error.clone());
        Err(error)
      }
    }
  });
  let server_task = tokio::spawn(server_future);
  let signal_cancel = cancel.clone();
  let signal_cancelled = cancel.clone();
  let signal_task = tokio::spawn(async move {
    tokio::select! {
      result = shutdown_signal() => {
        signal_cancel.cancel();
        result
      },
      _ = signal_cancelled.cancelled() => Ok(()),
    }
  });

  let (initialization_result, server_result) = supervise_server_and_initialization(init_task, server_task, &cancel).await;

  cancel.cancel();
  let mut shutdown_errors = Vec::new();
  if let Err(error) = wait_for_signal_task(signal_task, std::time::Duration::from_secs(1)).await {
    tracing::error!(%error, "Shutdown signal listener did not terminate cleanly");
    shutdown_errors.push(error);
  }

  let (runtime, startup_error) = match initialization_result {
    Ok(Ok(runtime)) => (Some(runtime), None),
    Ok(Err(error)) => (None, Some(format!("Startup error: {error}"))),
    Err(error) => (None, Some(format!("Startup task failed: {error}"))),
  };
  let server_result = match server_result {
    Ok(result) => result,
    Err(error) => Err(format!("server task failed: {error}")),
  };

  if let Some(runtime) = runtime {
    runtime.engine.begin_shutdown();

    // Wait for background tasks to finish (with a timeout).
    tracing::info!("Waiting for background tasks to finish...");
    if let Err(error) = wait_for_background_tasks(runtime.handles, std::time::Duration::from_secs(10)).await {
      tracing::error!(%error, "Background tasks did not terminate cleanly");
      shutdown_errors.push(error);
    }

    // Flush engine buffers and sync to disk.
    if let Err(error) = runtime.engine.shutdown() {
      tracing::error!("Storage engine shutdown did not complete cleanly: {}", error);
      eprintln!("Storage engine shutdown did not complete cleanly: {error}");
      shutdown_errors.push(format!("storage engine shutdown failed: {error}"));
    }
    let uptime = runtime.startup_instant.elapsed().as_secs();
    tracing::info!(uptime_seconds = uptime, "AeorDB shutting down");
  }

  if let Some(error) = startup_error {
    return Err(combine_primary_and_shutdown_error(error, &shutdown_errors));
  }
  if let Err(error) = server_result {
    return Err(combine_primary_and_shutdown_error(format!("Server error: {error}"), &shutdown_errors));
  }

  if shutdown_errors.is_empty() {
    println!("Server shut down gracefully.");
    Ok(())
  } else {
    Err(format!("Server stopped, but shutdown did not complete cleanly: {}", shutdown_errors.join("; ")))
  }
}

async fn initialize_server_runtime(
  config: InitConfig,
  cancel: CancellationToken,
  startup_gate: StartupGateState,
  port: u16,
) -> Result<ServerRuntime, String> {
  let InitConfig { database, auth_mode, hot_dir, cors_flag, peers, join_url, join_token, advertise_url, command_line_overrides } = config;
  let hot_dir_ref = hot_dir.as_path();

  fail_if_unresolved_emergency_spills(&database, &command_line_overrides)?;

  // If --join was supplied, perform the cluster join BEFORE the app opens
  // the engine for serving. The join writes the cluster's JWT signing key
  // into the local system_store; the JwtManager then loads it during
  // create_app_with_auth_mode and JWTs validate cluster-wide.
  if let Some(join_url) = join_url.as_deref() {
    startup_gate.set_phase("cluster_join", "Joining cluster before opening the serving engine", 0.05, None);
    let token = join_token.as_deref().ok_or_else(|| "--join requires --join-token".to_string())?;
    perform_cluster_join(&database, hot_dir_ref, join_url, token, port, advertise_url.as_deref(), command_line_overrides.clone()).await?;
    println!("Cluster join complete. Adopting shared signing key.");
  }

  // Register any --peers URLs into the system store before serving.
  if !peers.is_empty() {
    startup_gate.set_phase("registering_peers", "Registering configured peers before opening the serving engine", 0.10, None);
    register_initial_peers(&database, hot_dir_ref, &peers, command_line_overrides.clone())
      .map_err(|error| format!("failed to register --peers: {error}"))?;
    println!("Registered {} peer(s) from --peers", peers.len());
  }

  // Build the app (single engine open — no separate bootstrap engine).
  // We use the *_and_cancel variant so the sync loop's shutdown is wired
  // to this token; without it, the loop runs until the process is killed.
  startup_gate.set_phase("opening_engine", "Opening storage engine; dirty startups may rebuild the KV index from WAL", 0.15, None);
  let engine_progress_gate = startup_gate.clone();
  let engine_progress: aeordb::engine::EngineStartupProgressCallback = Arc::new(move |progress| {
    apply_engine_startup_progress(&engine_progress_gate, progress);
  });
  let (application, file_bootstrap_key, engine, event_bus, task_queue) =
    try_create_app_with_auth_mode_cancel_progress_and_configuration_overrides(
      &database,
      &auth_mode,
      Some(hot_dir_ref),
      cors_flag.as_deref(),
      Some(cancel.clone()),
      Some(engine_progress),
      command_line_overrides,
    )?;
  if let Some(recovery) = engine.persistent_durability_recovery().filter(|recovery| recovery.blocks_writes) {
    cancel.cancel();
    return Err(format!(
      "database has unresolved persistent durability recovery state; refusing writable startup\nreason: {}\nrun:\n  aeordb verify --repair --force-fix-in-place -D {}\nfor unattended repair after reviewing the evidence, add --yes",
      recovery.reason, database
    ));
  }

  // For SelfContained mode, bootstrap the root key using the already-open engine.
  if auth_mode == AuthMode::SelfContained {
    if let Some(root_key) = bootstrap_root_key(&engine).map_err(|error| format!("failed to bootstrap root key: {}", error))? {
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
  startup_gate.set_phase("starting_workers", "Starting background workers", 0.90, None);
  let heartbeat_handle = spawn_heartbeat(event_bus.clone(), 1, cancel.clone());

  // Start the rate sampler (1 Hz) and metrics pulse (15s) for detailed stats.
  let counters = engine.counters().clone();
  let rate_trackers = Arc::new(RateTrackerSet::new());
  let sampler_handle = spawn_rate_sampler(counters.clone(), rate_trackers.clone(), cancel.clone());
  let metrics_handle =
    spawn_metrics_pulse(event_bus.clone(), engine.clone(), counters, rate_trackers.clone(), database.clone(), cancel.clone());

  // Make rate_trackers and db_path available to the stats endpoint via Extension.
  let application = application.layer(axum::Extension(rate_trackers)).layer(axum::Extension(database.clone()));

  // Durably return crash-interrupted tasks to the retry queue. Startup must
  // not claim success while a malformed registry or failed rewrite strands
  // persisted work in Running state.
  let recovered_tasks =
    task_queue.recover_interrupted_tasks().map_err(|error| format!("failed to recover interrupted background tasks: {error}"))?;
  if recovered_tasks > 0 {
    tracing::info!(recovered_tasks, "Recovered interrupted background tasks");
  }

  // Write default global index config if it doesn't exist.
  {
    let ops = aeordb::engine::DirectoryOps::new(&engine);
    let ctx = aeordb::engine::RequestContext::system();
    let config_path = "/.aeordb-config/indexes.json";

    match ops.read_file_buffered(config_path) {
      Ok(_) => {
        // Config exists — don't overwrite.
      }
      Err(aeordb::engine::EngineError::NotFound(_)) => {
        // Default index config: covers every file (`glob: **/*`) with both
        // virtual metadata fields (`@filename`, `@hash`, ...) and the fields
        // every native parser emits (`text`, `title`, `metadata.format`,
        // `metadata.duration`). This gives out-of-the-box full-text + metadata
        // search across text, JSON, HTML, PDF, MS Office, ODF, image, audio,
        // and video files without any config tweaking. Operators can override
        // by replacing /.aeordb-config/indexes.json before first start, or
        // overriding via per-directory `.aeordb-config/indexes.json` files
        // anywhere in the tree.
        let default_config = default_global_index_config();
        let config_bytes = serde_json::to_vec_pretty(&default_config)
          .map_err(|error| format!("failed to serialize the default global index config: {error}"))?;
        ops
          .store_file_buffered(&ctx, config_path, &config_bytes, Some("application/json"))
          .map_err(|error| format!("failed to write the default global index config at {config_path}: {error}"))?;
        tracing::info!("Created default global index config");
        // Enqueue initial reindex.
        let task = task_queue
          .enqueue("reindex", serde_json::json!({"path": "/"}))
          .map_err(|error| format!("created the default index config but failed to durably enqueue its initial reindex: {error}"))?;
        tracing::info!(task_id = %task.id, "Enqueued initial global reindex");
      }
      Err(error) => return Err(format!("failed to inspect the default global index config at {config_path}: {error}")),
    }
  }

  // Seed default cron schedules on first start (hourly cleanup, daily GC).
  // No-op if a cron config already exists.
  aeordb::engine::seed_default_cron_if_missing(&engine)
    .map_err(|error| format!("failed to seed default cron schedules before starting the scheduler: {error}"))?;

  // Start the cron scheduler (enqueues tasks based on cron config every 60s).
  let cron_handle = spawn_cron_scheduler(task_queue.clone(), engine.clone(), event_bus.clone(), cancel.clone());

  // Start the task worker (dequeues and executes background tasks).
  let plugin_manager = std::sync::Arc::new(PluginManager::new(engine.clone()));
  let worker_handle = spawn_task_worker(task_queue, engine.clone(), plugin_manager, event_bus.clone(), cancel.clone());

  // Start the webhook dispatcher (delivers matching events to registered URLs).
  let webhook_handle = spawn_webhook_dispatcher(event_bus, engine.clone(), cancel);

  let startup_instant = std::time::Instant::now();
  startup_gate.set_ready(application);
  tracing::info!("AeorDB HTTP application is ready");

  Ok(ServerRuntime {
    engine,
    startup_instant,
    handles: vec![heartbeat_handle, sampler_handle, metrics_handle, webhook_handle, cron_handle, worker_handle],
  })
}

fn default_global_index_config() -> serde_json::Value {
  serde_json::json!({
    "glob": "**/*",
    "indexes": [
      // Virtual metadata (always present)
      {"name": "@path", "type": ["string", "trigram"]},
      {"name": "@filename", "type": ["string", "trigram", "soundex", "dmetaphone", "dmetaphone_alt"]},
      {"name": "@extension", "type": "string"},
      {"name": "@hash", "type": "string"},
      {"name": "@created_at", "type": "timestamp"},
      {"name": "@updated_at", "type": "timestamp"},
      {"name": "@size", "type": "u64"},
      {"name": "@content_type", "type": "string"},

      // Extracted content from native parsers (text, html, pdf, msoffice,
      // odf, image, audio, video). Parsers that have no body text emit
      // an empty string for `text` and put their useful info in `metadata`.
      {"name": "text", "type": "trigram"},
      {"name": "title", "type": ["string", "trigram"]},
      {"name": "metadata.format", "type": "string"},
      {"name": "metadata.duration", "type": "f64", "min": 0, "max": 86400}
    ]
  })
}

fn fail_if_unresolved_emergency_spills(
  database: &str,
  command_line_overrides: &aeordb::engine::config_resolver::CommandLineConfigOverrides,
) -> Result<(), String> {
  let locations = aeordb::engine::config_resolver::preopen_emergency_spill_locations(database, command_line_overrides)
    .map_err(|error| format!("failed to resolve emergency spill locations before startup: {error}"))?;
  let artifacts = aeordb::engine::emergency_spill::scan_for_database_with_locations(database, &locations)
    .map_err(|error| format!("failed to scan emergency spill locations before startup: {}", error))?;
  if artifacts.is_empty() {
    return Ok(());
  }

  let mut message = String::new();
  message.push_str("unresolved AeorDB emergency spill artifacts were found for this database; refusing to start until repair completes\n");
  message.push_str(&format!("database: {}\n", database));
  message.push_str(&format!("artifacts: {}\n", artifacts.len()));
  for (index, artifact) in artifacts.iter().enumerate() {
    message.push_str(&format!(
      "  {}. {} ({})\n",
      index + 1,
      artifact.directory.display(),
      artifact.attempted_at.as_deref().unwrap_or("unknown time")
    ));
    if artifact.wal_tail_bytes > 0 {
      message.push_str(&format!(
        "     WAL tail: {} bytes, copy_start={:?}, end={:?}, truncated={}\n",
        artifact.wal_tail_bytes, artifact.wal_tail_copy_start, artifact.wal_tail_end, artifact.wal_tail_truncated
      ));
    }
    if artifact.hot_tail_writes > 0 || artifact.hot_tail_voids > 0 {
      message.push_str(&format!("     hot-tail snapshot: {} writes, {} voids\n", artifact.hot_tail_writes, artifact.hot_tail_voids));
    }
  }
  message.push_str("\nRun repair, review the prompt, and then start again:\n");
  message.push_str(&format!("  aeordb verify --repair --force-fix-in-place -D {}\n", database));
  message.push_str("For unattended repair after reviewing the artifacts, add --yes.\n");
  Err(message)
}

/// Wait for all background workers, aborting unfinished work on timeout or a
/// peer task failure so no detached worker outlives shutdown.
#[doc(hidden)]
pub async fn wait_for_background_tasks(mut handles: Vec<tokio::task::JoinHandle<()>>, timeout: std::time::Duration) -> Result<(), String> {
  let mut completed = 0usize;
  let completion = async {
    for (index, handle) in handles.iter_mut().enumerate() {
      let result = handle.await;
      completed = index + 1;
      result.map_err(|error| describe_join_error(index, error))?;
    }
    Ok(())
  };

  let result = match tokio::time::timeout(timeout, completion).await {
    Ok(result) => result,
    Err(_) => Err(format!("{} background task(s) did not stop within {} ms", handles.len() - completed, timeout.as_millis())),
  };
  if result.is_err() {
    abort_and_join_remaining_background_tasks(&mut handles, completed).await;
  }
  result
}

fn describe_join_error(index: usize, error: tokio::task::JoinError) -> String {
  let outcome = if error.is_panic() {
    "panicked"
  } else if error.is_cancelled() {
    "was cancelled before shutdown requested it"
  } else {
    "failed"
  };
  format!("background task {} {outcome}: {error}", index + 1)
}

async fn abort_and_join_remaining_background_tasks(handles: &mut [tokio::task::JoinHandle<()>], completed: usize) {
  for handle in handles.iter().skip(completed) {
    handle.abort();
  }
  for (index, handle) in handles.iter_mut().enumerate().skip(completed) {
    match handle.await {
      Ok(()) => tracing::warn!(task_number = index + 1, "Background task completed while shutdown was aborting it"),
      Err(error) if error.is_cancelled() => {}
      Err(error) => tracing::error!(task_number = index + 1, %error, "Background task failed while shutdown was aborting it"),
    }
  }
}

async fn wait_for_signal_task(mut handle: tokio::task::JoinHandle<Result<(), String>>, timeout: std::time::Duration) -> Result<(), String> {
  match tokio::time::timeout(timeout, &mut handle).await {
    Ok(Ok(result)) => result,
    Ok(Err(error)) => Err(format!("shutdown signal task join failed: {error}")),
    Err(_) => {
      handle.abort();
      match handle.await {
        Err(error) if error.is_cancelled() => {}
        Ok(result) => tracing::warn!(?result, "Shutdown signal task completed while it was being aborted"),
        Err(error) => tracing::error!(%error, "Shutdown signal task failed while it was being aborted"),
      }
      Err(format!("shutdown signal task did not stop within {} ms", timeout.as_millis()))
    }
  }
}

fn combine_primary_and_shutdown_error(primary: String, shutdown_errors: &[String]) -> String {
  if shutdown_errors.is_empty() {
    return primary;
  }
  format!("{primary}\nShutdown also failed: {}", shutdown_errors.join("; "))
}

/// Listen for shutdown signals (SIGINT and SIGTERM on Unix, Ctrl+C everywhere).
async fn shutdown_signal() -> Result<(), String> {
  let ctrl_c = async { tokio::signal::ctrl_c().await.map_err(|error| format!("failed to install or receive Ctrl+C: {error}")) };

  #[cfg(unix)]
  let terminate = async {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
      .map_err(|error| format!("failed to install SIGTERM handler: {error}"))?;
    signal.recv().await.ok_or_else(|| "SIGTERM listener closed before receiving a signal".to_string())
  };

  #[cfg(not(unix))]
  let terminate = std::future::pending::<Result<(), String>>();

  let result = tokio::select! {
    result = ctrl_c => result,
    result = terminate => result,
  };

  if result.is_ok() {
    println!("\nReceived shutdown signal");
  }
  result
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
  command_line_overrides: aeordb::engine::config_resolver::CommandLineConfigOverrides,
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
    let token_json: serde_json::Value = token_resp.json().await.map_err(|e| format!("token exchange response parse failed: {}", e))?;
    token_json
      .get("token")
      .and_then(|v| v.as_str())
      .map(String::from)
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

  let body: serde_json::Value = resp.json().await.map_err(|e| format!("response parse failed: {}", e))?;

  let signing_key_b64 = body.get("signing_key").and_then(|v| v.as_str()).ok_or_else(|| "response missing 'signing_key'".to_string())?;
  let signing_key = B64.decode(signing_key_b64).map_err(|e| format!("invalid base64 signing key: {}", e))?;

  let responding_node_id =
    body.get("responding_node_id").and_then(|v| v.as_u64()).ok_or_else(|| "response missing 'responding_node_id'".to_string())?;

  // Open the engine, write the signing key + peer, then drop it so the
  // server can re-open it normally.
  let engine = open_or_create_bootstrap_engine(database, hot_dir, command_line_overrides, "cluster join")?;

  let ctx = aeordb::engine::RequestContext::system();
  aeordb::engine::system_store::store_jwt_signing_key(&engine, &ctx, &signing_key)
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
  aeordb::engine::system_store::PeerConfigStore::new(&engine)
    .upsert(&ctx, peer_config, aeordb::engine::system_store::PeerAddressConflictPolicy::ReplaceExisting)
    .map_err(|e| format!("failed to store peer config: {}", e))?;

  drop(engine);
  Ok(())
}

/// Write peer configs for --peers URLs into the engine's system store.
fn register_initial_peers(
  database: &str,
  hot_dir: &std::path::Path,
  peers: &[String],
  command_line_overrides: aeordb::engine::config_resolver::CommandLineConfigOverrides,
) -> Result<(), String> {
  let engine = open_or_create_bootstrap_engine(database, hot_dir, command_line_overrides, "initial peer registration")?;

  let ctx = aeordb::engine::RequestContext::system();
  aeordb::engine::system_store::PeerConfigStore::new(&engine)
    .ensure_addresses(
      &ctx,
      aeordb::engine::system_store::get_node_id(&engine).map_err(|e| format!("failed to load local node_id: {e}"))?,
      peers.to_vec(),
    )
    .map_err(|e| format!("failed to store initial peer configs: {e}"))?;

  drop(engine);
  Ok(())
}

fn open_or_create_bootstrap_engine(
  database: &str,
  hot_dir: &std::path::Path,
  command_line_overrides: aeordb::engine::config_resolver::CommandLineConfigOverrides,
  operation: &str,
) -> Result<aeordb::engine::StorageEngine, String> {
  let database_exists = std::path::Path::new(database)
    .try_exists()
    .map_err(|error| format!("failed to inspect the database path before {operation}: {error}"))?;
  if database_exists {
    return aeordb::engine::StorageEngine::open_with_hot_dir_progress_and_configuration_overrides(
      database,
      Some(hot_dir),
      None,
      command_line_overrides,
    )
    .map_err(|error| format!("failed to open existing engine for {operation}: {error}"));
  }

  aeordb::engine::StorageEngine::create_with_hot_dir_and_configuration_overrides(database, Some(hot_dir), command_line_overrides)
    .map_err(|error| format!("failed to create engine for {operation}: {error}"))
}

#[cfg(test)]
mod tests {
  use super::default_global_index_config;

  #[test]
  fn default_global_index_config_keeps_hash_and_content_type_exact_only() {
    let config = default_global_index_config();
    let indexes = config["indexes"].as_array().expect("indexes array");
    let index_type = |name: &str| {
      indexes
        .iter()
        .find(|index| index["name"] == name)
        .and_then(|index| index.get("type"))
        .cloned()
        .unwrap_or_else(|| panic!("missing index {name}"))
    };

    assert_eq!(index_type("@path"), serde_json::json!(["string", "trigram"]));
    assert_eq!(index_type("@hash"), serde_json::json!("string"));
    assert_eq!(index_type("@content_type"), serde_json::json!("string"));
  }
}
