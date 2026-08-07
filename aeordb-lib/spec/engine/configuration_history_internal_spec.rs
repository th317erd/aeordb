use crate::engine::config_resolver::ConfigurationFamily;
use crate::engine::configuration_history::load_configuration_history_with_limits;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::{RequestContext, StorageEngine};

fn create_engine(directory: &tempfile::TempDir) -> StorageEngine {
  let path = directory.path().join("configuration-history.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();
  engine
}

#[test]
fn scan_byte_bound_does_not_expand_to_find_an_older_revision() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  engine
    .replace_configuration_document(ConfigurationFamily::Runtime, br#"{"schema_version":1,"index":{"flush_after_seconds":17}}"#)
    .unwrap();

  let loaded = load_configuration_history_with_limits(&engine, ConfigurationFamily::Runtime, 1, 32);

  assert!(loaded.candidates.is_empty());
  assert!(loaded.issues.iter().any(|issue| issue.contains("inspected only the newest 1 bytes")), "{:?}", loaded.issues);
}

#[test]
fn candidate_bound_is_visible_and_keeps_the_newest_physical_revision() {
  let directory = tempfile::tempdir().unwrap();
  let engine = create_engine(&directory);
  for seconds in [11, 12, 13] {
    let document = format!(r#"{{"schema_version":1,"index":{{"flush_after_seconds":{seconds}}}}}"#);
    engine.replace_configuration_document(ConfigurationFamily::Runtime, document.as_bytes()).unwrap();
  }

  let loaded = load_configuration_history_with_limits(&engine, ConfigurationFamily::Runtime, u64::MAX, 1);

  assert_eq!(loaded.candidates.len(), 1);
  assert!(loaded.issues.iter().any(|issue| issue.contains("1-candidate bound")), "{:?}", loaded.issues);
  assert!(String::from_utf8_lossy(&loaded.candidates[0].bytes).contains("13"));
}
