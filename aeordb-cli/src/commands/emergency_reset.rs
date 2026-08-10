use std::io::{self, BufRead, Write};

use aeordb::auth::{ApiKeyRecord, generate_api_key, hash_api_key};
use aeordb::engine::{RequestContext, ROOT_USER_ID};
use aeordb::engine::system_store;
use aeordb::server::create_engine_for_storage;

fn confirm_emergency_reset_with<R: BufRead, W: Write>(input: &mut R, output: &mut W) -> io::Result<bool> {
  write!(output, "Proceed? [y/N]: ")?;
  output.flush()?;

  let mut answer = String::new();
  input.read_line(&mut answer)?;
  Ok(answer.trim().eq_ignore_ascii_case("y"))
}

fn confirm_emergency_reset() -> io::Result<bool> {
  let stdin = io::stdin();
  let stdout = io::stdout();
  confirm_emergency_reset_with(&mut stdin.lock(), &mut stdout.lock())
}

pub fn run(database: &str, force: bool) {
  if !force {
    println!("WARNING: This will invalidate the current root API key.");
    println!("A new root API key will be generated.");
    match confirm_emergency_reset() {
      Ok(true) => {}
      Ok(false) => {
        println!("Aborted.");
        return;
      }
      Err(error) => {
        eprintln!("Failed to read emergency reset confirmation: {error}");
        std::process::exit(1);
      }
    }
  }

  let engine = create_engine_for_storage(database);
  let ctx = RequestContext::system();

  // Find and revoke all API keys linked to the nil UUID (root).
  let all_keys = match system_store::list_api_keys(&engine) {
    Ok(keys) => keys,
    Err(error) => {
      eprintln!("Failed to list API keys: {}", error);
      std::process::exit(1);
    }
  };

  let mut revoked_count = 0u64;
  for key in &all_keys {
    if key.user_id == Some(ROOT_USER_ID) && !key.is_revoked {
      if let Err(error) = system_store::revoke_api_key(&engine, &ctx, key.key_id) {
        eprintln!("Failed to revoke root key {}: {}", key.key_id, error);
        std::process::exit(1);
      }
      revoked_count += 1;
    }
  }

  println!("Revoked {} existing root API key(s).", revoked_count);

  // Generate a new root API key.
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = match hash_api_key(&plaintext_key) {
    Ok(hash) => hash,
    Err(error) => {
      eprintln!("Failed to hash new root API key: {}", error);
      std::process::exit(1);
    }
  };

  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(ROOT_USER_ID),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: chrono::Utc::now().timestamp_millis() + (aeordb::auth::DEFAULT_EXPIRY_DAYS * 24 * 60 * 60 * 1000),
    label: Some("emergency-reset".to_string()),
    rules: vec![],
  };

  // SECURITY: Use bootstrap path to allow nil UUID.
  if let Err(error) = system_store::store_api_key_for_bootstrap(&engine, &ctx, &record) {
    eprintln!("Failed to store new root API key: {}", error);
    std::process::exit(1);
  }

  println!();
  println!("==========================================================");
  println!("  NEW ROOT API KEY (shown once, save it now!):");
  println!("  {}", plaintext_key);
  println!("==========================================================");
}

#[cfg(test)]
#[path = "../../spec/commands/emergency_reset_prompt_internal_spec.rs"]
mod emergency_reset_prompt_internal_spec;
