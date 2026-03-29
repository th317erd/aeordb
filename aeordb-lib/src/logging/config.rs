/// Format for log output.
#[derive(Debug, Clone, PartialEq)]
pub enum LogFormat {
  /// Machine-parseable JSON, one object per line.
  Json,
  /// Human-readable, colored, multi-line spans.
  Pretty,
}

/// Configuration for the logging subsystem.
#[derive(Debug, Clone)]
pub struct LogConfig {
  /// Output format (JSON for production, Pretty for development).
  pub format: LogFormat,
  /// Default filter level, e.g. "info" or "debug,aeordb::storage=trace".
  /// Overridden by the `AEORDB_LOG` environment variable when present.
  pub level: String,
  /// Show the target module path in each log line.
  pub show_target: bool,
  /// Show thread name/id in each log line.
  pub show_thread: bool,
  /// Show source file and line number in each log line.
  pub show_file_line: bool,
}

impl Default for LogConfig {
  fn default() -> Self {
    Self {
      format: LogFormat::Pretty,
      level: "info".to_string(),
      show_target: true,
      show_thread: false,
      show_file_line: false,
    }
  }
}
