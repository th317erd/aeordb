//! Explicit independent generator. Refuses to overwrite any artifact.
#[path = "../support/semantic_compiler_profile_cases.rs"]
mod cases;
#[allow(dead_code)]
#[path = "../support/semantic_compiler_profile_oracle.rs"]
mod oracle;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let arguments: Vec<_> = std::env::args_os().skip(1).collect();
  if arguments.len() != 3 {
    return Err("usage: semantic-compiler-profile-fixtures <frozen-v4-fixtures> <specification-directory> <empty-output-directory>".into());
  }
  let root = PathBuf::from(&arguments[0]);
  let specification = PathBuf::from(&arguments[1]);
  let output = PathBuf::from(&arguments[2]);
  std::fs::create_dir(&output)?;
  let valid = cases::valid(&root);
  let invalid = cases::invalid();
  let files = [
    std::fs::read(specification.join("SPEC.md"))?,
    oracle::packet(b"SCI1", &invalid),
    std::fs::read(specification.join("properties.json"))?,
    oracle::packet(b"SCV1", &valid),
  ];
  for (name, bytes) in oracle::FILES.iter().zip(&files) {
    OpenOptions::new().write(true).create_new(true).open(output.join(name))?.write_all(bytes)?;
  }
  let fingerprints: Vec<_> =
    (1..=5).map(|algorithm| serde_json::json!({"algorithm":algorithm,"fingerprint":oracle::fingerprint(algorithm,&files)})).collect();
  let mut fingerprints = serde_json::to_vec_pretty(&fingerprints)?;
  fingerprints.push(b'\n');
  OpenOptions::new().write(true).create_new(true).open(output.join("fingerprints.json"))?.write_all(&fingerprints)?;
  println!("Independent compiler corpus: {} valid cases x 5 algorithms, {} invalid cases", valid.len(), invalid.len());
  Ok(())
}
