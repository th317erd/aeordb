use std::sync::Arc;

use super::*;
use crate::engine::{DirectoryOps, RequestContext};

#[test]
fn startup_counter_initialization_rejects_poisoned_void_authority() {
  let directory = tempfile::tempdir().unwrap();
  let engine = Arc::new(StorageEngine::create(directory.path().join("counter-poison.aeordb").to_str().unwrap()).unwrap());
  DirectoryOps::new(&engine).ensure_root_directory(&RequestContext::system()).unwrap();

  let poison_engine = Arc::clone(&engine);
  let unwind = std::thread::spawn(move || {
    let _void_manager = poison_engine.void_manager.write().unwrap();
    panic!("inject Void manager poison");
  })
  .join();
  assert!(unwind.is_err());

  let error = match EngineCounters::initialize_from_kv(&engine) {
    Ok(_) => panic!("startup counters accepted poisoned Void authority"),
    Err(error) => error,
  };
  assert!(error.to_string().contains("void manager lock poisoned"), "unexpected error: {error}");
}
