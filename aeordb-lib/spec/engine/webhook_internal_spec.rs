use std::sync::Arc;

use super::{WEBHOOK_CONFIG_PATH, WebhookRegistry, reload_dispatcher_config};
use crate::engine::{DirectoryOps, RequestContext, StorageEngine};

fn create_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let directory = tempfile::tempdir().unwrap();
  let database_path = directory.path().join("webhook-reload.aeordb");
  let engine = Arc::new(StorageEngine::create(database_path.to_str().unwrap()).unwrap());
  (engine, directory)
}

#[test]
fn malformed_webhook_reload_preserves_the_last_valid_registry() {
  let (engine, _directory) = create_engine();
  let operations = DirectoryOps::new(&engine);
  let context = RequestContext::system();
  let valid = br#"{"webhooks":[{"id":"retained","url":"https://example.test/hook","events":["entries_created"],"secret":"secret"}]}"#;
  operations.store_file_buffered(&context, WEBHOOK_CONFIG_PATH, valid, Some("application/json")).unwrap();

  let mut current: Option<WebhookRegistry> = None;
  assert!(reload_dispatcher_config(&engine, &mut current, "test_initial"));
  assert_eq!(current.as_ref().unwrap().webhooks[0].id, "retained");

  operations.store_file_buffered(&context, WEBHOOK_CONFIG_PATH, b"not-json", Some("application/json")).unwrap();

  assert!(!reload_dispatcher_config(&engine, &mut current, "test_reload"));
  assert_eq!(current.as_ref().unwrap().webhooks[0].id, "retained");
}

#[test]
fn missing_webhook_config_is_a_valid_empty_registry_state() {
  let (engine, _directory) = create_engine();
  let mut current = Some(WebhookRegistry { webhooks: Vec::new() });

  assert!(reload_dispatcher_config(&engine, &mut current, "test_missing"));
  assert!(current.is_none());
}
