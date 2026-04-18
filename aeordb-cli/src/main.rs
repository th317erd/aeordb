//! # AeorDB CLI
//!
//! Command-line interface for the AeorDB database.
//!
//! ## Commands
//!
//! - `aeordb start` — start the database server
//! - `aeordb gc` — run garbage collection
//! - `aeordb export` — export a version
//! - `aeordb diff` — create a patch between versions
//! - `aeordb import` — import a backup or patch
//! - `aeordb promote` — promote a version hash to HEAD
//! - `aeordb emergency-reset` — reset the root API key

use aeordb_cli::commands;
use clap::{Parser, Subcommand};
use commands::stress::StressArgs;

#[derive(Parser)]
#[command(name = "aeordb", about = "AeorDB command-line interface")]
struct Cli {
  #[command(subcommand)]
  command: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the database server
  Start {
    #[arg(short, long, default_value = "3000")]
    port: u16,
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    #[arg(long, default_value = "pretty")]
    log_format: String,
    /// Auth provider URI: false/null/no/0, self, file:///path/to/identity
    #[arg(long)]
    auth: Option<String>,
    /// Directory for write-ahead hot files (defaults to database file's parent directory)
    #[arg(long)]
    hot_dir: Option<String>,
    /// CORS allowed origins: "*" for all, or comma-separated origins (e.g. "https://a.com,https://b.com")
    #[arg(long)]
    cors: Option<String>,
    /// Path to TLS certificate PEM file (requires --tls-key)
    #[arg(long)]
    tls_cert: Option<String>,
    /// Path to TLS private key PEM file (requires --tls-cert)
    #[arg(long)]
    tls_key: Option<String>,
  },
  /// Run stress tests against a running instance
  Stress(StressArgs),
  /// Emergency reset: revoke the current root API key and generate a new one
  EmergencyReset {
    #[arg(short = 'D', long)]
    database: String,
    #[arg(long)]
    force: bool,
  },
  /// Export a version as a self-contained .aeordb file
  Export {
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    #[arg(short, long)]
    output: String,
    #[arg(short, long)]
    snapshot: Option<String>,
    #[arg(long)]
    hash: Option<String>,
  },
  /// Create a patch .aeordb containing only the changeset between two versions
  Diff {
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    #[arg(short, long)]
    output: String,
    #[arg(long)]
    from: String,
    #[arg(long)]
    to: Option<String>,
  },
  /// Import an export or patch .aeordb file into a target database
  Import {
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    #[arg(short, long)]
    file: String,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    promote: bool,
  },
  /// Promote a version hash to HEAD
  Promote {
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    #[arg(long)]
    hash: String,
  },
  /// Run garbage collection to reclaim unreachable entries
  Gc {
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Report what would be collected without actually deleting
    #[arg(long)]
    dry_run: bool,
  },
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();

  match cli.command {
    Commands::Start { port, database, log_format, auth, hot_dir, cors, tls_cert, tls_key } => {
      commands::start::run(port, &database, &log_format, auth.as_deref(), hot_dir.as_deref(), cors.as_deref(), tls_cert.as_deref(), tls_key.as_deref()).await;
    }
    Commands::Stress(arguments) => {
      if let Err(error) = commands::stress::run(arguments).await {
        eprintln!("Stress test failed: {error}");
        std::process::exit(1);
      }
    }
    Commands::EmergencyReset { database, force } => {
      commands::emergency_reset::run(&database, force);
    }
    Commands::Export { database, output, snapshot, hash } => {
      commands::export::run(&database, &output, snapshot.as_deref(), hash.as_deref());
    }
    Commands::Diff { database, output, from, to } => {
      commands::diff::run(&database, &output, &from, to.as_deref());
    }
    Commands::Import { database, file, force, promote } => {
      commands::import_cmd::run(&database, &file, force, promote);
    }
    Commands::Promote { database, hash } => {
      commands::promote::run(&database, &hash);
    }
    Commands::Gc { database, dry_run } => {
      commands::gc::run(&database, dry_run);
    }
  }
}
