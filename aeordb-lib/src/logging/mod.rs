pub mod config;
pub mod request_id;

pub use config::{LogConfig, LogFormat};
pub use request_id::request_id_middleware;

use tracing_subscriber::{
  EnvFilter,
  fmt,
  layer::SubscriberExt,
  util::SubscriberInitExt,
};

/// Initialize the global tracing subscriber based on the provided config.
///
/// The subscriber is composed of layers so future destinations (file output,
/// database output, remote services) can be added without restructuring.
///
/// The `AEORDB_LOG` environment variable, when set, takes precedence over the
/// configured level string.
pub fn initialize_logging(config: &LogConfig) {
  let env_filter = EnvFilter::try_from_env("AEORDB_LOG")
    .unwrap_or_else(|_| EnvFilter::new(&config.level));

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
        .init();
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
        .init();
    }
  }
}
