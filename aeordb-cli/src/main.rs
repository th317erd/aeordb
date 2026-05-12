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
use aeordb_cli::config::{AeorConfig, load_config};
use clap::{Parser, Subcommand};
use commands::stress::StressArgs;

#[derive(Parser)]
#[command(
  name = "aeordb",
  about = "AeorDB — Content-addressed database with built-in versioning",
  version,
  subcommand_required = true,
  arg_required_else_help = true,
  subcommand_help_heading = "Commands",
  after_help = "Use 'aeordb help <command>' or 'aeordb <command> --help' for more information on a specific command."
)]
struct Cli {
  #[command(subcommand)]
  command: Commands,
}

#[derive(Subcommand)]
enum Commands {
  /// Start the database server
  Start {
    /// Path to a TOML configuration file
    #[arg(short, long)]
    config: Option<String>,
    /// HTTP server port (default: 6830)
    #[arg(short, long)]
    port: Option<u16>,
    /// Bind address (default: "0.0.0.0")
    #[arg(long)]
    host: Option<String>,
    /// Path to the database file (default: "data.aeordb")
    #[arg(short = 'D', long)]
    database: Option<String>,
    /// Log output format: "pretty", "json", or "compact" (default: "pretty")
    #[arg(long)]
    log_format: Option<String>,
    /// Auth provider URI: disabled/false/null/no/0, self, file:///path/to/identity
    #[arg(long)]
    auth: Option<String>,
    /// Directory for write-ahead hot files (defaults to database file's parent directory)
    #[arg(long)]
    hot_dir: Option<String>,
    /// CORS allowed origins: "*" for all, or comma-separated origins (e.g. "https://a.com,https://b.com")
    #[arg(long)]
    cors_origins: Option<String>,
    /// Path to TLS certificate PEM file (requires --tls-key)
    #[arg(long)]
    tls_cert: Option<String>,
    /// Path to TLS private key PEM file (requires --tls-cert)
    #[arg(long)]
    tls_key: Option<String>,
    /// JWT token lifetime in seconds (default: 3600)
    #[arg(long)]
    jwt_expiry: Option<i64>,
    /// Write chunk size in bytes (default: 262144 = 256 KiB)
    #[arg(long)]
    chunk_size: Option<usize>,
    /// Comma-separated list of peer URLs to register at startup
    /// (e.g. "http://node2:6830,http://node3:6830").
    /// Peers are persisted; this flag is idempotent.
    #[arg(long)]
    peers: Option<String>,
    /// Join an existing cluster: URL of any existing cluster member.
    /// Combined with --join-token, the new node fetches the cluster's
    /// JWT signing key (so JWTs validate cluster-wide) and registers
    /// the joined node as a peer.
    #[arg(long)]
    join: Option<String>,
    /// Root API key (or bearer token) of an existing cluster member,
    /// used to authorize the join request. Required with --join.
    #[arg(long)]
    join_token: Option<String>,
    /// URL that other nodes should use to reach this node (e.g.
    /// "https://node-b.internal:6841"). If omitted, --join falls back to
    /// http://localhost:PORT, which is unreachable from any other host.
    /// Required for multi-host clusters.
    #[arg(long)]
    advertise_url: Option<String>,
  },
  /// Run stress tests against a running instance
  Stress(StressArgs),
  /// Emergency reset: revoke the current root API key and generate a new one
  EmergencyReset {
    /// Path to the database file
    #[arg(short = 'D', long)]
    database: String,
    /// Skip confirmation prompt
    #[arg(long)]
    force: bool,
  },
  /// Export a version as a self-contained .aeordb file
  Export {
    /// Path to the database file (default: "data.aeordb")
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Output file path for the exported .aeordb
    #[arg(short, long)]
    output: String,
    /// Snapshot name to export (default: current HEAD)
    #[arg(short, long)]
    snapshot: Option<String>,
    /// Version hash to export (alternative to snapshot name)
    #[arg(long)]
    hash: Option<String>,
    /// Root API key for full backup mode (includes system data and all snapshots).
    /// Can also be set via AEORDB_ROOT_KEY env var.
    #[arg(long)]
    root_key: Option<String>,
  },
  /// Create a patch .aeordb containing only the changeset between two versions
  Diff {
    /// Path to the database file (default: "data.aeordb")
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Output file path for the patch .aeordb
    #[arg(short, long)]
    output: String,
    /// Source version (snapshot name or hash)
    #[arg(long)]
    from: String,
    /// Target version (default: current HEAD)
    #[arg(long)]
    to: Option<String>,
  },
  /// Import an export or patch .aeordb file into a target database
  Import {
    /// Path to the target database file (default: "data.aeordb")
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Path to the .aeordb file to import
    #[arg(short, long)]
    file: String,
    /// Force import, overwriting conflicts
    #[arg(long)]
    force: bool,
    /// Promote imported version to HEAD after import
    #[arg(long)]
    promote: bool,
    /// Root API key for the target database. Required when importing system
    /// data (users, groups, keys). Can also be set via AEORDB_ROOT_KEY env var.
    #[arg(long)]
    root_key: Option<String>,
  },
  /// Promote a version hash to HEAD
  Promote {
    /// Path to the database file (default: "data.aeordb")
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Version hash to promote to HEAD
    #[arg(long)]
    hash: String,
  },
  /// Verify database integrity and optionally repair issues
  Verify {
    /// Path to the database file (default: "data.aeordb")
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Auto-repair recoverable issues (rebuild KV, quarantine corrupt entries).
    /// By default, repairs are written to a copy (<database>.repaired).
    /// Use --force-fix-in-place to modify the original file directly.
    #[arg(long)]
    repair: bool,
    /// Apply repairs directly to the original database file instead of
    /// creating a repaired copy. Use when disk space is limited or the
    /// database is too large to copy.
    #[arg(long)]
    force_fix_in_place: bool,
  },
  /// Run garbage collection to reclaim unreachable entries
  Gc {
    /// Path to the database file (default: "data.aeordb")
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Report what would be collected without actually deleting
    #[arg(long)]
    dry_run: bool,
  },
  /// Probe a database file: list /.aeordb-system/ contents (diagnostic).
  #[command(hide = true)]
  Probe {
    #[arg(short = 'D', long)]
    database: String,
  },
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();

  match cli.command {
    Commands::Start {
      config,
      port,
      host,
      database,
      log_format,
      auth,
      hot_dir,
      cors_origins,
      tls_cert,
      tls_key,
      jwt_expiry,
      chunk_size,
      peers,
      join,
      join_token,
      advertise_url,
    } => {
      // Load config file if specified; otherwise use all-None defaults.
      let file_config = match config {
        Some(ref path) => match load_config(path) {
          Ok(configuration) => configuration,
          Err(error) => {
            eprintln!("Error: {error}");
            std::process::exit(1);
          }
        },
        None => AeorConfig::default(),
      };

      // Merge: CLI flag > config file > built-in default.
      let merged_port = port
        .or(file_config.server.port)
        .unwrap_or(6830);

      let merged_host = host
        .or(file_config.server.host)
        .unwrap_or_else(|| "0.0.0.0".to_string());

      let merged_database = database
        .or(file_config.storage.database)
        .unwrap_or_else(|| "data.aeordb".to_string());

      let merged_log_format = log_format
        .or(file_config.server.log_format)
        .unwrap_or_else(|| "pretty".to_string());

      // Auth: CLI --auth overrides config auth.mode.
      let merged_auth: Option<String> = auth.or(file_config.auth.mode);

      // Hot dir: CLI --hot-dir overrides config storage.hot_dir.
      let merged_hot_dir: Option<String> = hot_dir.or(file_config.storage.hot_dir);

      // CORS: CLI --cors-origins overrides config server.cors.origins.
      // Config origins (Vec<String>) are joined with commas for the server layer.
      let merged_cors: Option<String> = cors_origins.or_else(|| {
        file_config.server.cors
          .and_then(|cors| cors.origins)
          .map(|origins| origins.join(","))
      });

      // TLS: CLI flags override config values.
      let merged_tls_cert: Option<String> = tls_cert.or_else(|| {
        file_config.server.tls.as_ref().and_then(|tls| tls.cert.clone())
      });
      let merged_tls_key: Option<String> = tls_key.or_else(|| {
        file_config.server.tls.as_ref().and_then(|tls| tls.key.clone())
      });

      // JWT expiry: CLI --jwt-expiry overrides config auth.jwt_expiry_seconds.
      let merged_jwt_expiry = jwt_expiry
        .or(file_config.auth.jwt_expiry_seconds)
        .unwrap_or(3600);

      // Chunk size: CLI --chunk-size overrides config storage.chunk_size.
      let merged_chunk_size = chunk_size
        .or(file_config.storage.chunk_size)
        .unwrap_or(262144);

      // Parse --peers into a Vec<String>.
      let peer_list: Vec<String> = peers
        .as_deref()
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();

      // Validate --join requires --join-token.
      if join.is_some() && join_token.is_none() {
        eprintln!("Error: --join requires --join-token (the existing cluster's root API key).");
        std::process::exit(1);
      }

      commands::start::run(commands::start::StartConfig {
        port: merged_port,
        host: &merged_host,
        database: &merged_database,
        log_format: &merged_log_format,
        auth_flag: merged_auth.as_deref(),
        hot_dir_arg: merged_hot_dir.as_deref(),
        cors_flag: merged_cors.as_deref(),
        tls_cert: merged_tls_cert.as_deref(),
        tls_key: merged_tls_key.as_deref(),
        jwt_expiry: merged_jwt_expiry,
        chunk_size: merged_chunk_size,
        peers: peer_list,
        join_url: join.as_deref(),
        join_token: join_token.as_deref(),
        advertise_url: advertise_url.as_deref(),
      }).await;
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
    Commands::Export { database, output, snapshot, hash, root_key } => {
      commands::export::run(&database, &output, snapshot.as_deref(), hash.as_deref(), root_key.as_deref());
    }
    Commands::Diff { database, output, from, to } => {
      commands::diff::run(&database, &output, &from, to.as_deref());
    }
    Commands::Import { database, file, force, promote, root_key } => {
      commands::import_cmd::run(&database, &file, force, promote, root_key.as_deref());
    }
    Commands::Promote { database, hash } => {
      commands::promote::run(&database, &hash);
    }
    Commands::Verify { database, repair, force_fix_in_place } => {
      commands::verify::run(&database, repair, force_fix_in_place);
    }
    Commands::Gc { database, dry_run } => {
      commands::gc::run(&database, dry_run);
    }
    Commands::Probe { database } => {
      commands::probe::run(&database);
    }
  }
}
