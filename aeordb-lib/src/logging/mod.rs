pub mod config;
pub mod request_id;

pub use config::{LogConfig, LogFormat};
pub use request_id::request_id_middleware;

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize the global tracing subscriber based on the provided config.
///
/// The subscriber is composed of layers so future destinations (file output,
/// database output, remote services) can be added without restructuring.
///
/// The `AEORDB_LOG` environment variable, when set, takes precedence over the
/// configured level string.
pub fn resolve_log_filter(config: &LogConfig) -> Result<EnvFilter, String> {
  match std::env::var("AEORDB_LOG") {
    Ok(value) => EnvFilter::try_new(&value).map_err(|error| format!("invalid AEORDB_LOG value {value:?}: {error}")),
    Err(std::env::VarError::NotPresent) => {
      EnvFilter::try_new(&config.level).map_err(|error| format!("invalid configured log level {:?}: {error}", config.level))
    }
    Err(std::env::VarError::NotUnicode(_)) => Err("AEORDB_LOG must contain valid Unicode".to_string()),
  }
}

/// Fallibly initialize the process-wide tracing subscriber.
pub fn try_initialize_logging(config: &LogConfig) -> Result<(), String> {
  let env_filter = resolve_log_filter(config)?;

  match config.format {
    LogFormat::Json => {
      let fmt_layer = fmt::layer()
        .json()
        .with_target(config.show_target)
        .with_thread_names(config.show_thread)
        .with_thread_ids(config.show_thread)
        .with_file(config.show_file_line)
        .with_line_number(config.show_file_line)
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .with_current_span(true);

      tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
        .map_err(|error| format!("failed to initialize logging: {error}"))?;
    }
    LogFormat::Pretty => {
      let fmt_layer = fmt::layer()
        .pretty()
        .with_target(config.show_target)
        .with_thread_names(config.show_thread)
        .with_thread_ids(config.show_thread)
        .with_file(config.show_file_line)
        .with_line_number(config.show_file_line)
        .with_timer(fmt::time::UtcTime::rfc_3339());

      tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
        .map_err(|error| format!("failed to initialize logging: {error}"))?;
    }
  }
  Ok(())
}

/// Initialize logging for callers that retain the historical infallible API.
/// CLI startup and diagnostics use [`try_initialize_logging`] so configuration
/// failures are returned to the operator instead of panicking.
pub fn initialize_logging(config: &LogConfig) {
  try_initialize_logging(config).expect("logging initialization failed");
}
