pub mod deployment;
pub mod diff;
pub mod emergency_reset;
pub mod export;
pub mod gc;
pub mod import_cmd;
pub mod migrate_v4;
pub mod probe;
pub mod promote;
pub mod start;
pub mod status;
pub mod stress;
pub mod verify;

fn remove_failed_artifact(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
  match std::fs::remove_file(path) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error),
  }
}

#[cfg(test)]
#[path = "../../spec/commands/artifact_cleanup_spec.rs"]
mod artifact_cleanup_spec;
