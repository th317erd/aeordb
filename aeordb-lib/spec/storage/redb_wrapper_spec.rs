use aeordb::storage::document::MetadataUpdates;
use aeordb::storage::redb_backend::{RedbStorage, StorageError};
use chrono::Utc;
use uuid::Uuid;

const TABLE: &str = "test_documents";

fn create_storage() -> RedbStorage {
  RedbStorage::new_in_memory().expect("failed to create in-memory storage")
}

// ---------------------------------------------------------------------------
// CREATE
// ---------------------------------------------------------------------------

#[test]
fn create_document_generates_uuid() {
  let storage = create_storage();
  let document = storage
    .create_document(TABLE, b"hello".to_vec(), Some("text/plain".into()))
    .unwrap();

  // UUID v4 has version nibble == 4
  assert_eq!(document.document_id.get_version_num(), 4);
}

#[test]
fn create_document_sets_timestamps() {
  let before = Utc::now();
  let storage = create_storage();
  let document = storage
    .create_document(TABLE, b"data".to_vec(), None)
    .unwrap();
  let after = Utc::now();

  assert!(document.created_at >= before && document.created_at <= after);
  assert!(document.updated_at >= before && document.updated_at <= after);
  assert_eq!(document.created_at, document.updated_at);
}

#[test]
fn create_document_sets_is_deleted_to_false() {
  let storage = create_storage();
  let document = storage
    .create_document(TABLE, b"data".to_vec(), None)
    .unwrap();

  assert!(!document.is_deleted);
}

#[test]
fn create_document_with_user_provided_id_preserves_it() {
  let storage = create_storage();
  let custom_id = Uuid::new_v4();
  let document = storage
    .create_document_with_id(TABLE, custom_id, b"payload".to_vec(), None)
    .unwrap();

  assert_eq!(document.document_id, custom_id);
}

#[test]
fn create_document_stores_content_type() {
  let storage = create_storage();
  let document = storage
    .create_document(TABLE, b"{}".to_vec(), Some("application/json".into()))
    .unwrap();

  assert_eq!(document.content_type, Some("application/json".to_string()));
}

#[test]
fn create_document_with_none_content_type() {
  let storage = create_storage();
  let document = storage
    .create_document(TABLE, b"raw".to_vec(), None)
    .unwrap();

  assert_eq!(document.content_type, None);
}

#[test]
fn create_document_with_empty_data_gets_mandatory_fields() {
  let storage = create_storage();
  let document = storage
    .create_document(TABLE, vec![], None)
    .unwrap();

  assert!(!document.document_id.is_nil());
  assert!(!document.is_deleted);
  assert_eq!(document.data, Vec::<u8>::new());
}

// ---------------------------------------------------------------------------
// GET
// ---------------------------------------------------------------------------

#[test]
fn get_document_returns_none_for_missing() {
  let storage = create_storage();
  // Create the table first by inserting something, then query a different id
  storage
    .create_document(TABLE, b"seed".to_vec(), None)
    .unwrap();

  let result = storage
    .get_document(TABLE, Uuid::new_v4())
    .unwrap();

  assert!(result.is_none());
}

#[test]
fn get_document_returns_none_for_nonexistent_table() {
  let storage = create_storage();
  let result = storage
    .get_document("does_not_exist", Uuid::new_v4())
    .unwrap();

  assert!(result.is_none());
}

#[test]
fn get_document_returns_document() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"hello world".to_vec(), Some("text/plain".into()))
    .unwrap();

  let fetched = storage
    .get_document(TABLE, created.document_id)
    .unwrap()
    .expect("document should exist");

  assert_eq!(fetched.document_id, created.document_id);
  assert_eq!(fetched.data, b"hello world");
  assert_eq!(fetched.content_type, Some("text/plain".to_string()));
  assert!(!fetched.is_deleted);
}

#[test]
fn get_document_excludes_soft_deleted() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"will be deleted".to_vec(), None)
    .unwrap();

  storage.delete_document(TABLE, created.document_id).unwrap();

  let result = storage
    .get_document(TABLE, created.document_id)
    .unwrap();

  assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// UPDATE
// ---------------------------------------------------------------------------

#[test]
fn update_document_changes_updated_at() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"v1".to_vec(), None)
    .unwrap();

  // Small sleep to ensure timestamp changes (millisecond precision)
  std::thread::sleep(std::time::Duration::from_millis(2));

  let updated = storage
    .update_document(TABLE, created.document_id, b"v2".to_vec())
    .unwrap();

  assert!(updated.updated_at > created.updated_at);
  assert_eq!(updated.data, b"v2");
}

#[test]
fn update_document_preserves_created_at() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"v1".to_vec(), None)
    .unwrap();

  std::thread::sleep(std::time::Duration::from_millis(2));

  let updated = storage
    .update_document(TABLE, created.document_id, b"v2".to_vec())
    .unwrap();

  assert_eq!(updated.created_at, created.created_at);
}

#[test]
fn update_document_preserves_content_type() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"v1".to_vec(), Some("application/xml".into()))
    .unwrap();

  let updated = storage
    .update_document(TABLE, created.document_id, b"v2".to_vec())
    .unwrap();

  assert_eq!(updated.content_type, Some("application/xml".to_string()));
}

#[test]
fn update_document_returns_error_for_missing_document() {
  let storage = create_storage();
  // Create table so it exists
  storage
    .create_document(TABLE, b"seed".to_vec(), None)
    .unwrap();

  let missing_id = Uuid::new_v4();
  let result = storage.update_document(TABLE, missing_id, b"nope".to_vec());

  assert!(result.is_err());
  match result.unwrap_err() {
    StorageError::DocumentNotFound(id) => assert_eq!(id, missing_id),
    other => panic!("expected DocumentNotFound, got: {other}"),
  }
}

#[test]
fn update_document_returns_error_for_nonexistent_table() {
  let storage = create_storage();
  let result = storage.update_document("no_table", Uuid::new_v4(), b"nope".to_vec());

  assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// DELETE (soft)
// ---------------------------------------------------------------------------

#[test]
fn delete_document_sets_is_deleted_to_true() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"doomed".to_vec(), None)
    .unwrap();

  storage.delete_document(TABLE, created.document_id).unwrap();

  // Use list with include_deleted to verify the flag
  let all = storage.list_documents(TABLE, true).unwrap();
  let document = all
    .iter()
    .find(|d| d.document_id == created.document_id)
    .expect("document should still exist in storage");

  assert!(document.is_deleted);
}

#[test]
fn delete_document_preserves_data() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"important data".to_vec(), Some("text/plain".into()))
    .unwrap();

  storage.delete_document(TABLE, created.document_id).unwrap();

  let all = storage.list_documents(TABLE, true).unwrap();
  let document = all
    .iter()
    .find(|d| d.document_id == created.document_id)
    .unwrap();

  assert_eq!(document.data, b"important data");
  assert_eq!(document.content_type, Some("text/plain".to_string()));
}

#[test]
fn delete_document_returns_error_for_missing() {
  let storage = create_storage();
  storage
    .create_document(TABLE, b"seed".to_vec(), None)
    .unwrap();

  let missing_id = Uuid::new_v4();
  let result = storage.delete_document(TABLE, missing_id);

  assert!(result.is_err());
  match result.unwrap_err() {
    StorageError::DocumentNotFound(id) => assert_eq!(id, missing_id),
    other => panic!("expected DocumentNotFound, got: {other}"),
  }
}

#[test]
fn delete_document_returns_error_for_nonexistent_table() {
  let storage = create_storage();
  let result = storage.delete_document("no_table", Uuid::new_v4());

  assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// UNDELETE (via update_document_metadata)
// ---------------------------------------------------------------------------

#[test]
fn undelete_document_via_metadata_update() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"revivable".to_vec(), None)
    .unwrap();

  storage.delete_document(TABLE, created.document_id).unwrap();

  // Verify it's gone from normal get
  assert!(storage.get_document(TABLE, created.document_id).unwrap().is_none());

  // Undelete
  let restored = storage
    .update_document_metadata(
      TABLE,
      created.document_id,
      MetadataUpdates {
        is_deleted: Some(false),
        ..Default::default()
      },
    )
    .unwrap();

  assert!(!restored.is_deleted);
  assert_eq!(restored.data, b"revivable");

  // Now get should return it
  let fetched = storage
    .get_document(TABLE, created.document_id)
    .unwrap()
    .expect("document should be visible after undelete");
  assert!(!fetched.is_deleted);
}

#[test]
fn update_document_metadata_returns_error_for_missing() {
  let storage = create_storage();
  storage
    .create_document(TABLE, b"seed".to_vec(), None)
    .unwrap();

  let missing_id = Uuid::new_v4();
  let result = storage.update_document_metadata(
    TABLE,
    missing_id,
    MetadataUpdates {
      is_deleted: Some(false),
      ..Default::default()
    },
  );

  assert!(result.is_err());
  match result.unwrap_err() {
    StorageError::DocumentNotFound(id) => assert_eq!(id, missing_id),
    other => panic!("expected DocumentNotFound, got: {other}"),
  }
}

#[test]
fn update_document_metadata_can_change_content_type() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"data".to_vec(), Some("text/plain".into()))
    .unwrap();

  let updated = storage
    .update_document_metadata(
      TABLE,
      created.document_id,
      MetadataUpdates {
        content_type: Some(Some("application/json".into())),
        ..Default::default()
      },
    )
    .unwrap();

  assert_eq!(updated.content_type, Some("application/json".to_string()));
}

// ---------------------------------------------------------------------------
// LIST
// ---------------------------------------------------------------------------

#[test]
fn list_documents_excludes_soft_deleted_by_default() {
  let storage = create_storage();
  let alive = storage
    .create_document(TABLE, b"alive".to_vec(), None)
    .unwrap();
  let doomed = storage
    .create_document(TABLE, b"doomed".to_vec(), None)
    .unwrap();

  storage.delete_document(TABLE, doomed.document_id).unwrap();

  let documents = storage.list_documents(TABLE, false).unwrap();
  assert_eq!(documents.len(), 1);
  assert_eq!(documents[0].document_id, alive.document_id);
}

#[test]
fn list_documents_includes_soft_deleted_when_requested() {
  let storage = create_storage();
  storage
    .create_document(TABLE, b"alive".to_vec(), None)
    .unwrap();
  let doomed = storage
    .create_document(TABLE, b"doomed".to_vec(), None)
    .unwrap();

  storage.delete_document(TABLE, doomed.document_id).unwrap();

  let documents = storage.list_documents(TABLE, true).unwrap();
  assert_eq!(documents.len(), 2);
}

#[test]
fn list_documents_on_empty_table_returns_empty_vec() {
  let storage = create_storage();
  // Table doesn't even exist yet
  let documents = storage.list_documents("empty_table", false).unwrap();
  assert!(documents.is_empty());
}

#[test]
fn list_documents_on_existing_but_empty_table() {
  let storage = create_storage();
  // Create and immediately delete, so table exists but no visible docs
  let document = storage
    .create_document(TABLE, b"temp".to_vec(), None)
    .unwrap();
  storage.delete_document(TABLE, document.document_id).unwrap();

  let documents = storage.list_documents(TABLE, false).unwrap();
  assert!(documents.is_empty());
}

#[test]
fn list_documents_returns_all_non_deleted() {
  let storage = create_storage();
  let first = storage
    .create_document(TABLE, b"one".to_vec(), None)
    .unwrap();
  let second = storage
    .create_document(TABLE, b"two".to_vec(), None)
    .unwrap();
  let third = storage
    .create_document(TABLE, b"three".to_vec(), None)
    .unwrap();

  let documents = storage.list_documents(TABLE, false).unwrap();
  assert_eq!(documents.len(), 3);

  let ids: Vec<Uuid> = documents.iter().map(|d| d.document_id).collect();
  assert!(ids.contains(&first.document_id));
  assert!(ids.contains(&second.document_id));
  assert!(ids.contains(&third.document_id));
}

// ---------------------------------------------------------------------------
// CONCURRENT READS
// ---------------------------------------------------------------------------

#[test]
fn concurrent_reads_do_not_block_each_other() {
  use std::sync::Arc;
  use std::thread;

  let storage = Arc::new(create_storage());
  let created = storage
    .create_document(TABLE, b"shared".to_vec(), None)
    .unwrap();
  let document_id = created.document_id;

  let handles: Vec<_> = (0..4)
    .map(|_| {
      let storage_clone = Arc::clone(&storage);
      thread::spawn(move || {
        let document = storage_clone
          .get_document(TABLE, document_id)
          .unwrap()
          .expect("document should exist");
        assert_eq!(document.data, b"shared");
      })
    })
    .collect();

  for handle in handles {
    handle.join().expect("thread panicked");
  }
}

// ---------------------------------------------------------------------------
// DATA INTEGRITY: round-trip various payloads
// ---------------------------------------------------------------------------

#[test]
fn round_trip_binary_data() {
  let storage = create_storage();
  let binary_data: Vec<u8> = (0..=255).collect();
  let created = storage
    .create_document(TABLE, binary_data.clone(), Some("application/octet-stream".into()))
    .unwrap();

  let fetched = storage
    .get_document(TABLE, created.document_id)
    .unwrap()
    .unwrap();

  assert_eq!(fetched.data, binary_data);
}

#[test]
fn round_trip_large_payload() {
  let storage = create_storage();
  let large_data = vec![0xAB_u8; 1_000_000]; // 1 MB
  let created = storage
    .create_document(TABLE, large_data.clone(), None)
    .unwrap();

  let fetched = storage
    .get_document(TABLE, created.document_id)
    .unwrap()
    .unwrap();

  assert_eq!(fetched.data.len(), 1_000_000);
  assert_eq!(fetched.data, large_data);
}

#[test]
fn round_trip_unicode_content_type() {
  let storage = create_storage();
  let document = storage
    .create_document(TABLE, b"data".to_vec(), Some("text/plain; charset=utf-8".into()))
    .unwrap();

  let fetched = storage
    .get_document(TABLE, document.document_id)
    .unwrap()
    .unwrap();

  assert_eq!(
    fetched.content_type,
    Some("text/plain; charset=utf-8".to_string())
  );
}

// ---------------------------------------------------------------------------
// MULTIPLE TABLES (isolation)
// ---------------------------------------------------------------------------

#[test]
fn documents_are_isolated_between_tables() {
  let storage = create_storage();

  let document_a = storage
    .create_document("table_a", b"in table A".to_vec(), None)
    .unwrap();
  let document_b = storage
    .create_document("table_b", b"in table B".to_vec(), None)
    .unwrap();

  // Each document is only visible in its own table
  assert!(storage.get_document("table_a", document_b.document_id).unwrap().is_none());
  assert!(storage.get_document("table_b", document_a.document_id).unwrap().is_none());

  assert!(storage.get_document("table_a", document_a.document_id).unwrap().is_some());
  assert!(storage.get_document("table_b", document_b.document_id).unwrap().is_some());
}

// ---------------------------------------------------------------------------
// DOUBLE DELETE (FIX 4: should return error)
// ---------------------------------------------------------------------------

#[test]
fn double_delete_returns_error() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"data".to_vec(), None)
    .unwrap();

  storage.delete_document(TABLE, created.document_id).unwrap();
  // Second delete should return DocumentNotFound since the document is already soft-deleted
  let result = storage.delete_document(TABLE, created.document_id);
  assert!(result.is_err(), "second delete should fail");
  match result.unwrap_err() {
    StorageError::DocumentNotFound(id) => assert_eq!(id, created.document_id),
    other => panic!("expected DocumentNotFound, got: {other}"),
  }

  // Document should still be marked as deleted in storage
  let all = storage.list_documents(TABLE, true).unwrap();
  let document = all
    .iter()
    .find(|d| d.document_id == created.document_id)
    .unwrap();
  assert!(document.is_deleted);
}

// ---------------------------------------------------------------------------
// UPDATE AFTER DELETE (FIX 3: should return error)
// ---------------------------------------------------------------------------

#[test]
fn update_document_on_soft_deleted_returns_error() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"original".to_vec(), None)
    .unwrap();

  storage.delete_document(TABLE, created.document_id).unwrap();

  let result = storage.update_document(TABLE, created.document_id, b"modified".to_vec());
  assert!(result.is_err(), "updating a soft-deleted document should fail");
  match result.unwrap_err() {
    StorageError::DocumentNotFound(id) => assert_eq!(id, created.document_id),
    other => panic!("expected DocumentNotFound, got: {other}"),
  }
}

#[test]
fn update_metadata_on_soft_deleted_without_undelete_returns_error() {
  let storage = create_storage();
  let created = storage
    .create_document(TABLE, b"data".to_vec(), Some("text/plain".into()))
    .unwrap();

  storage.delete_document(TABLE, created.document_id).unwrap();

  // Try to change content_type on a deleted doc (not an undelete operation)
  let result = storage.update_document_metadata(
    TABLE,
    created.document_id,
    MetadataUpdates {
      content_type: Some(Some("application/json".into())),
      ..Default::default()
    },
  );
  assert!(result.is_err(), "metadata update on deleted doc should fail unless undeleting");
  match result.unwrap_err() {
    StorageError::DocumentNotFound(id) => assert_eq!(id, created.document_id),
    other => panic!("expected DocumentNotFound, got: {other}"),
  }
}

// ---------------------------------------------------------------------------
// SYSTEM CONFIG
// ---------------------------------------------------------------------------

#[test]
fn store_and_get_config() {
  let storage = create_storage();

  storage.store_config("test_key", b"test_value").unwrap();

  let value = storage.get_config("test_key").unwrap();
  assert_eq!(value, Some(b"test_value".to_vec()));
}

#[test]
fn get_config_returns_none_for_missing() {
  let storage = create_storage();
  let value = storage.get_config("nonexistent").unwrap();
  assert!(value.is_none());
}

#[test]
fn store_config_overwrites_existing() {
  let storage = create_storage();

  storage.store_config("key", b"v1").unwrap();
  storage.store_config("key", b"v2").unwrap();

  let value = storage.get_config("key").unwrap();
  assert_eq!(value, Some(b"v2".to_vec()));
}

// ---------------------------------------------------------------------------
// GET SYSTEM API KEY (targeted lookup)
// ---------------------------------------------------------------------------

#[test]
fn get_system_api_key_by_prefix() {
  let storage = create_storage();
  let key_id = uuid::Uuid::new_v4();
  let record = aeordb::auth::api_key::ApiKeyRecord {
    key_id,
    key_hash: "test_hash".to_string(),
    roles: vec!["admin".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };
  storage.store_api_key(&record).unwrap();

  let key_id_prefix = &key_id.simple().to_string()[..16];
  let found = storage.get_system_api_key(key_id_prefix).unwrap();
  assert!(found.is_some());
  assert_eq!(found.unwrap().key_id, key_id);
}

#[test]
fn get_system_api_key_returns_none_for_unknown_prefix() {
  let storage = create_storage();
  let found = storage.get_system_api_key("0000000000000000").unwrap();
  assert!(found.is_none());
}
