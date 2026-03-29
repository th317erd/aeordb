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
    #[arg(short = 'D', long, default_value = "data.aeor")]
    database: String,
    #[arg(long, default_value = "pretty")]
    log_format: String,
  },
  /// Run stress tests against a running instance
  Stress(StressArgs),
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();

  match cli.command {
    Commands::Start { port, database, log_format } => {
      commands::start::run(port, &database, &log_format).await;
    }
    Commands::Stress(arguments) => {
      if let Err(error) = commands::stress::run(arguments).await {
        eprintln!("Stress test failed: {error}");
        std::process::exit(1);
      }
    }
  }
}
