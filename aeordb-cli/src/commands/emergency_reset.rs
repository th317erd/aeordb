use aeordb::auth::{ApiKeyRecord, generate_api_key, hash_api_key};
use aeordb::engine::{RequestContext, SystemTables, ROOT_USER_ID};
use aeordb::server::create_engine_for_storage;

pub fn run(database: &str, force: bool) {
  if !force {
    println!("WARNING: This will invalidate the current root API key.");
    println!("A new root API key will be generated.");
    print!("Proceed? [y/N]: ");

    // Flush stdout so the prompt appears before reading stdin.
    use std::io::Write;
    std::io::stdout().flush().unwrap();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
    if !input.trim().eq_ignore_ascii_case("y") {
      println!("Aborted.");
      return;
    }
  }

  let engine = create_engine_for_storage(database);
  let ctx = RequestContext::system();
  let system_tables = SystemTables::new(&engine);

  // Find and revoke all API keys linked to the nil UUID (root).
  let all_keys = match system_tables.list_system_api_keys() {
    Ok(keys) => keys,
    Err(error) => {
      eprintln!("Failed to list API keys: {}", error);
      std::process::exit(1);
    }
  };

  let mut revoked_count = 0u64;
  for key in &all_keys {
    if key.user_id == ROOT_USER_ID && !key.is_revoked {
      if let Err(error) = system_tables.revoke_api_key(&ctx, key.key_id) {
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
    user_id: ROOT_USER_ID,
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };

  // SECURITY: Use bootstrap path to allow nil UUID.
  if let Err(error) = system_tables.store_api_key_for_bootstrap(&ctx, &record) {
    eprintln!("Failed to store new root API key: {}", error);
    std::process::exit(1);
  }

  println!();
  println!("==========================================================");
  println!("  NEW ROOT API KEY (shown once, save it now!):");
  println!("  {}", plaintext_key);
  println!("==========================================================");
}
