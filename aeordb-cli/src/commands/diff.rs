use std::process;

pub fn run(database: &str, output: &str, from: &str, to: Option<&str>) {
  println!("AeorDB Diff");
  println!("Source: {}", database);
  println!("Output: {}", output);
  println!("From: {}", from);
  println!("To: {}", to.unwrap_or("HEAD"));

  if std::path::Path::new(output).exists() {
    eprintln!("Error: output file '{}' already exists.", output);
    process::exit(1);
  }

  let source = match aeordb::engine::StorageEngine::open(database) {
    Ok(engine) => engine,
    Err(e) => {
      eprintln!("Error opening database: {}", e);
      process::exit(1);
    }
  };

  let result = aeordb::engine::backup::create_patch_from_references(&source, from, to, output);

  match result {
    Ok(result) => println!("\n{}", result),
    Err(e) => {
      eprintln!("Diff failed: {}", e);
      let _ = std::fs::remove_file(output);
      process::exit(1);
    }
  }
}
