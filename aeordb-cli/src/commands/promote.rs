use std::process;

pub fn run(database: &str, hash: &str) {
  println!("AeorDB Promote");
  println!("Database: {}", database);
  println!("Hash: {}", hash);

  let hash_bytes = match hex::decode(hash) {
    Ok(bytes) => bytes,
    Err(e) => {
      eprintln!("Error: invalid hash '{}': {}", hash, e);
      process::exit(1);
    }
  };

  let engine = match aeordb::engine::StorageEngine::open(database) {
    Ok(engine) => engine,
    Err(e) => {
      eprintln!("Error opening database: {}", e);
      process::exit(1);
    }
  };

  match aeordb::engine::publish_namespace_root(&engine, &hash_bytes, aeordb::engine::NamespaceMutationKind::Promote) {
    Ok(_) => println!("HEAD promoted to {}", hash),
    Err(e) => {
      eprintln!("Promote failed: {}", e);
      process::exit(1);
    }
  }
}
