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
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();

  match cli.command {
    Commands::Start { port, database, log_format, auth } => {
      commands::start::run(port, &database, &log_format, auth.as_deref()).await;
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
  }
}
