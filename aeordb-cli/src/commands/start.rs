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
  spawn_heartbeat, spawn_metrics_pulse, spawn_rate_sampler, spawn_webhook_dispatcher, spawn_cron_scheduler, spawn_task_worker, TaskStatus,
};
use aeordb::engine::rate_tracker::RateTrackerSet;
use aeordb::plugins::PluginManager;
use aeordb::logging::{LogConfig, LogFormat, initialize_logging};
use aeordb::server::create_app_with_auth_mode_cancel_progress;

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
    if let Ok(mut inner) = self.inner.write() {
      *inner = StartupGateInner::Starting {
        phase: phase.into(),
        message: message.into(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        progress: progress.clamp(0.0, 1.0),
        eta_seconds,
      };
    }
  }

  fn set_ready(&self, application: Router) {
    if let Ok(mut inner) = self.inner.write() {
      *inner = StartupGateInner::Ready { application };
    }
  }

  fn set_failed(&self, error: impl Into<String>) {
    if let Ok(mut inner) = self.inner.write() {
      *inner = StartupGateInner::Failed { error: error.into(), updated_at: chrono::Utc::now().to_rfc3339() };
    }
  }

  fn ready_application(&self) -> Option<Router> {
    match &*self.inner.read().ok()? {
      StartupGateInner::Ready { application } => Some(application.clone()),
      _ => None,
    }
  }

  fn status_payload(&self) -> serde_json::Value {
    let elapsed_ms = self.started_at_instant.elapsed().as_millis() as u64;
    let Ok(inner) = self.inner.read() else {
      return serde_json::json!({
        "status": "failed",
        "phase": "startup_lock_poisoned",
        "message": "startup status lock is unavailable",
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": self.started_at,
        "progress": null,
        "eta": null,
        "elapsed_ms": elapsed_ms,
      });
    };
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

pub async fn run(config: StartConfig<'_>) {
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
  } = config;
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
  let bind_address: std::net::IpAddr = host.parse().unwrap_or_else(|_| {
    eprintln!("Error: invalid host address '{host}'");
    std::process::exit(1);
  });
  let address = SocketAddr::from((bind_address, port));

  // Create a CancellationToken shared by all background tasks (including
  // the sync loop spawned inside create_app_with_auth_mode_and_cancel) and
  // the HTTP server below.
  let cancel = CancellationToken::new();
  let startup_gate = StartupGateState::new();
  let startup_application = Router::new().fallback(startup_gate_handler).with_state(startup_gate.clone());

  let init_config = InitConfig {
    database: database.to_string(),
    auth_mode,
    hot_dir,
    cors_flag: cors_flag.map(str::to_string),
    peers,
    join_url: join_url.map(str::to_string),
    join_token: join_token.map(str::to_string),
    advertise_url: advertise_url.map(str::to_string),
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
      .serve(startup_application.into_make_service())
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

    let serve_fut = axum::serve(listener, startup_application).with_graceful_shutdown(shutdown_fut);

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

  cancel.cancel();

  let runtime = match init_task.await {
    Ok(Ok(runtime)) => Some(runtime),
    Ok(Err(error)) => {
      eprintln!("Startup error: {error}");
      None
    }
    Err(error) => {
      eprintln!("Startup task failed: {error}");
      None
    }
  };

  let mut clean_shutdown = true;
  if let Some(runtime) = runtime {
    runtime.engine.begin_shutdown();

    // Wait for background tasks to finish (with a timeout).
    tracing::info!("Waiting for background tasks to finish...");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), futures_join_all(runtime.handles)).await;

    // Flush engine buffers and sync to disk.
    if let Err(error) = runtime.engine.shutdown() {
      clean_shutdown = false;
      tracing::error!("Storage engine shutdown did not complete cleanly: {}", error);
      eprintln!("Storage engine shutdown did not complete cleanly: {error}");
    }
    let uptime = runtime.startup_instant.elapsed().as_secs();
    tracing::info!(uptime_seconds = uptime, "AeorDB shutting down");
  }

  if let Err(error) = server_result {
    eprintln!("Server error: {error}");
    std::process::exit(1);
  }

  if clean_shutdown {
    println!("Server shut down gracefully.");
  } else {
    eprintln!("Server stopped, but storage shutdown did not complete cleanly.");
  }
}

async fn initialize_server_runtime(
  config: InitConfig,
  cancel: CancellationToken,
  startup_gate: StartupGateState,
  port: u16,
) -> Result<ServerRuntime, String> {
  let InitConfig { database, auth_mode, hot_dir, cors_flag, peers, join_url, join_token, advertise_url } = config;
  let hot_dir_ref = hot_dir.as_path();

  fail_if_unresolved_emergency_spills(&database)?;

  // If --join was supplied, perform the cluster join BEFORE the app opens
  // the engine for serving. The join writes the cluster's JWT signing key
  // into the local system_store; the JwtManager then loads it during
  // create_app_with_auth_mode and JWTs validate cluster-wide.
  if let Some(join_url) = join_url.as_deref() {
    startup_gate.set_phase("cluster_join", "Joining cluster before opening the serving engine", 0.05, None);
    let token = join_token.as_deref().ok_or_else(|| "--join requires --join-token".to_string())?;
    perform_cluster_join(&database, hot_dir_ref, join_url, token, port, advertise_url.as_deref()).await?;
    println!("Cluster join complete. Adopting shared signing key.");
  }

  // Register any --peers URLs into the system store before serving.
  if !peers.is_empty() {
    startup_gate.set_phase("registering_peers", "Registering configured peers before opening the serving engine", 0.10, None);
    if let Err(e) = register_initial_peers(&database, hot_dir_ref, &peers) {
      tracing::warn!("Failed to register some --peers: {}", e);
      eprintln!("Warning: failed to register some --peers: {}", e);
    } else {
      println!("Registered {} peer(s) from --peers", peers.len());
    }
  }

  // Build the app (single engine open — no separate bootstrap engine).
  // We use the *_and_cancel variant so the sync loop's shutdown is wired
  // to this token; without it, the loop runs until the process is killed.
  startup_gate.set_phase("opening_engine", "Opening storage engine; dirty startups may rebuild the KV index from WAL", 0.15, None);
  let engine_progress_gate = startup_gate.clone();
  let engine_progress: aeordb::engine::EngineStartupProgressCallback = Arc::new(move |progress| {
    apply_engine_startup_progress(&engine_progress_gate, progress);
  });
  let (application, file_bootstrap_key, engine, event_bus, task_queue) = create_app_with_auth_mode_cancel_progress(
    &database,
    &auth_mode,
    Some(hot_dir_ref),
    cors_flag.as_deref(),
    Some(cancel.clone()),
    Some(engine_progress),
  );

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
    let config_path = "/.aeordb-config/indexes.json";

    match ops.read_file_buffered(config_path) {
      Ok(_) => {
        // Config exists — don't overwrite.
      }
      Err(_) => {
        // Default index config: covers every file (`glob: **/*`) with both
        // virtual metadata fields (`@filename`, `@hash`, ...) and the fields
        // every native parser emits (`text`, `title`, `metadata.format`,
        // `metadata.duration`). This gives out-of-the-box full-text + metadata
        // search across text, JSON, HTML, PDF, MS Office, ODF, image, audio,
        // and video files without any config tweaking. Operators can override
        // by replacing /.aeordb-config/indexes.json before first start, or
        // overriding via per-directory `.aeordb-config/indexes.json` files
        // anywhere in the tree.
        let default_config = serde_json::json!({
          "glob": "**/*",
          "indexes": [
            // Virtual metadata (always present)
            {"name": "@path", "type": ["string", "trigram"]},
            {"name": "@filename", "type": ["string", "trigram", "soundex", "dmetaphone", "dmetaphone_alt"]},
            {"name": "@extension", "type": "string"},
            {"name": "@hash", "type": ["string", "trigram"]},
            {"name": "@created_at", "type": "timestamp"},
            {"name": "@updated_at", "type": "timestamp"},
            {"name": "@size", "type": "u64"},
            {"name": "@content_type", "type": ["string", "trigram"]},

            // Extracted content from native parsers (text, html, pdf, msoffice,
            // odf, image, audio, video). Parsers that have no body text emit
            // an empty string for `text` and put their useful info in `metadata`.
            {"name": "text", "type": "trigram"},
            {"name": "title", "type": ["string", "trigram"]},
            {"name": "metadata.format", "type": "string"},
            {"name": "metadata.duration", "type": "f64", "min": 0, "max": 86400}
          ]
        });
        let config_bytes = serde_json::to_vec_pretty(&default_config).unwrap();
        if let Err(e) = ops.store_file_buffered(&ctx, config_path, &config_bytes, Some("application/json")) {
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

fn fail_if_unresolved_emergency_spills(database: &str) -> Result<(), String> {
  let artifacts = aeordb::engine::emergency_spill::scan_unapplied_for_database(database)
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

/// Wait for all join handles to complete.
async fn futures_join_all(handles: Vec<tokio::task::JoinHandle<()>>) {
  for handle in handles {
    let _ = handle.await;
  }
}

/// Listen for shutdown signals (SIGINT and SIGTERM on Unix, Ctrl+C everywhere).
async fn shutdown_signal() {
  let ctrl_c = async {
    tokio::signal::ctrl_c().await.expect("failed to install CTRL+C handler");
  };

  #[cfg(unix)]
  let terminate = async {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("failed to install SIGTERM handler").recv().await;
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
fn register_initial_peers(database: &str, hot_dir: &std::path::Path, peers: &[String]) -> Result<(), String> {
  let engine = aeordb::engine::StorageEngine::open_with_hot_dir(database, Some(hot_dir))
    .or_else(|_| aeordb::engine::StorageEngine::create_with_hot_dir(database, Some(hot_dir)))
    .map_err(|e| format!("failed to open engine: {}", e))?;

  let ctx = aeordb::engine::RequestContext::system();
  let mut peer_configs = aeordb::engine::system_store::get_peer_configs(&engine).unwrap_or_default();

  for url in peers {
    if peer_configs.iter().any(|p| &p.address == url) {
      continue;
    }
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
