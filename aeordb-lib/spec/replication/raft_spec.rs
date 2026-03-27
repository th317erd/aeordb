use std::io;
use std::sync::Arc;

use aeordb::replication::log_store::InMemoryLogStore;
use aeordb::replication::state_machine::ChunkStateMachine;
use aeordb::replication::types::{RaftNode, RaftRequest, RaftResponse};
use aeordb::replication::RaftNodeManager;
use aeordb::storage::chunk::hash_data;
use aeordb::storage::chunk_storage::ChunkStorage;
use aeordb::storage::in_memory_chunk_storage::InMemoryChunkStorage;

use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, RaftLogReader, RaftLogStorage, RaftStateMachine};
use openraft::vote::RaftLeaderId;
use openraft::RaftTypeConfig;

type TypeConfig = aeordb::replication::TypeConfig;
type Entry = <TypeConfig as RaftTypeConfig>::Entry;

/// Helper: create a committed leader ID for term 1, node 1.
fn committed_leader() -> <openraft::impls::leader_id_adv::LeaderId<u64, u64> as RaftLeaderId>::Committed {
  openraft::impls::leader_id_adv::LeaderId::<u64, u64>::new(1, 1).to_committed()
}

// -----------------------------------------------------------------------
// Helper: create a RaftNodeManager backed by in-memory storage
// -----------------------------------------------------------------------
async fn create_single_node_manager() -> RaftNodeManager {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  RaftNodeManager::new(1, chunk_storage)
    .await
    .expect("failed to create raft node manager")
}

async fn create_bootstrapped_node() -> (RaftNodeManager, Arc<dyn ChunkStorage>) {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let manager = RaftNodeManager::new(1, chunk_storage.clone())
    .await
    .expect("failed to create raft node manager");
  manager
    .initialize_single_node()
    .await
    .expect("failed to bootstrap single-node cluster");

  // Give the node a moment to elect itself leader.
  tokio::time::sleep(std::time::Duration::from_millis(500)).await;

  (manager, chunk_storage)
}

// ===================================================================
// Single-node bootstrap and lifecycle tests
// ===================================================================

#[tokio::test]
async fn test_single_node_bootstrap() {
  let manager = create_single_node_manager().await;
  let result = manager.initialize_single_node().await;
  assert!(result.is_ok(), "single-node bootstrap should succeed");
}

#[tokio::test]
async fn test_single_node_double_bootstrap_fails() {
  let manager = create_single_node_manager().await;
  manager
    .initialize_single_node()
    .await
    .expect("first bootstrap should succeed");

  let second_result = manager.initialize_single_node().await;
  assert!(
    second_result.is_err(),
    "double-initializing the same node should fail"
  );
}

#[tokio::test]
async fn test_raft_node_is_leader_after_bootstrap() {
  let (manager, _storage) = create_bootstrapped_node().await;
  assert!(
    manager.is_leader().await,
    "single-node should be leader after bootstrap"
  );
}

#[tokio::test]
async fn test_raft_node_id() {
  let manager = create_single_node_manager().await;
  assert_eq!(manager.node_id(), 1);
}

// ===================================================================
// Single-node write tests
// ===================================================================

#[tokio::test]
async fn test_single_node_write_and_read() {
  let (manager, storage) = create_bootstrapped_node().await;

  let data = b"hello world".to_vec();
  let hash = hash_data(&data).to_vec();

  let response = manager
    .client_write(RaftRequest::StoreChunk {
      hash: hash.clone(),
      data: data.clone(),
    })
    .await
    .expect("client_write should succeed");

  assert!(response.success, "store chunk response should be successful");

  // Verify the chunk is actually in storage.
  let mut chunk_hash = [0u8; 32];
  chunk_hash.copy_from_slice(&hash);
  let chunk = storage
    .get_chunk(&chunk_hash)
    .expect("get_chunk should not error")
    .expect("chunk should exist in storage");
  assert_eq!(chunk.data, data);
}

#[tokio::test]
async fn test_single_node_store_chunk_via_raft() {
  let (manager, storage) = create_bootstrapped_node().await;

  let data = b"aeordb raft integration test data".to_vec();
  let hash = hash_data(&data).to_vec();

  let response = manager
    .client_write(RaftRequest::StoreChunk {
      hash: hash.clone(),
      data: data.clone(),
    })
    .await
    .expect("client_write should succeed");
  assert!(response.success);

  let mut chunk_hash = [0u8; 32];
  chunk_hash.copy_from_slice(&hash);
  assert!(
    storage.has_chunk(&chunk_hash).unwrap(),
    "chunk should be present after raft write"
  );
}

#[tokio::test]
async fn test_single_node_multiple_writes() {
  let (manager, storage) = create_bootstrapped_node().await;

  for index in 0..10u32 {
    let data = format!("chunk-{}", index).into_bytes();
    let hash = hash_data(&data).to_vec();

    let response = manager
      .client_write(RaftRequest::StoreChunk {
        hash: hash.clone(),
        data: data.clone(),
      })
      .await
      .expect("client_write should succeed");
    assert!(response.success, "write {} should succeed", index);
  }

  let count = storage.chunk_count().expect("chunk_count should not error");
  assert_eq!(count, 10, "should have 10 chunks after 10 writes");
}

#[tokio::test]
async fn test_single_node_delete_chunk_via_raft() {
  let (manager, storage) = create_bootstrapped_node().await;

  let data = b"to be deleted".to_vec();
  let hash = hash_data(&data).to_vec();

  // Store first.
  manager
    .client_write(RaftRequest::StoreChunk {
      hash: hash.clone(),
      data: data.clone(),
    })
    .await
    .expect("store should succeed");

  // Delete.
  let delete_response = manager
    .client_write(RaftRequest::DeleteChunk { hash: hash.clone() })
    .await
    .expect("delete should succeed");
  assert!(delete_response.success);

  let mut chunk_hash = [0u8; 32];
  chunk_hash.copy_from_slice(&hash);
  assert!(
    !storage.has_chunk(&chunk_hash).unwrap(),
    "chunk should be gone after delete"
  );
}

#[tokio::test]
async fn test_single_node_store_hash_map_acknowledged() {
  let (manager, _storage) = create_bootstrapped_node().await;

  let response = manager
    .client_write(RaftRequest::StoreHashMap {
      key: "test-key".to_string(),
      hash_map_data: vec![1, 2, 3],
    })
    .await
    .expect("store hash map should succeed");
  assert!(response.success);
}

#[tokio::test]
async fn test_single_node_store_chunk_hash_mismatch() {
  let (manager, _storage) = create_bootstrapped_node().await;

  let data = b"some data".to_vec();
  // Intentionally wrong 32-byte hash.
  let wrong_hash = vec![0u8; 32];

  let response = manager
    .client_write(RaftRequest::StoreChunk {
      hash: wrong_hash,
      data,
    })
    .await
    .expect("write should not error at raft level");
  assert!(
    !response.success,
    "should fail when hash does not match data"
  );
  assert!(
    response.message.unwrap().contains("hash mismatch"),
    "error message should mention hash mismatch"
  );
}

#[tokio::test]
async fn test_single_node_delete_chunk_invalid_hash_length() {
  let (manager, _storage) = create_bootstrapped_node().await;

  let response = manager
    .client_write(RaftRequest::DeleteChunk {
      hash: vec![1, 2, 3],
    })
    .await
    .expect("write should not error at raft level");
  assert!(!response.success, "should fail with invalid hash length");
  assert!(response.message.unwrap().contains("invalid chunk hash length"));
}

#[tokio::test]
async fn test_single_node_delete_nonexistent_chunk() {
  let (manager, _storage) = create_bootstrapped_node().await;

  let response = manager
    .client_write(RaftRequest::DeleteChunk {
      hash: vec![0u8; 32],
    })
    .await
    .expect("write should not error at raft level");
  assert!(response.success, "deleting nonexistent chunk should succeed");
  assert!(
    response.message.as_deref() == Some("chunk not found"),
    "message should indicate chunk not found"
  );
}

// ===================================================================
// Serialization round-trip tests
// ===================================================================

#[test]
fn test_raft_request_serialization_roundtrip() {
  let requests = vec![
    RaftRequest::StoreChunk {
      hash: vec![1, 2, 3],
      data: vec![4, 5, 6],
    },
    RaftRequest::StoreHashMap {
      key: "key".to_string(),
      hash_map_data: vec![7, 8],
    },
    RaftRequest::DeleteChunk { hash: vec![9, 10] },
  ];

  for request in requests {
    let serialized = serde_json::to_string(&request).expect("serialize should succeed");
    let deserialized: RaftRequest =
      serde_json::from_str(&serialized).expect("deserialize should succeed");
    // Check variant matches via Debug representation.
    assert_eq!(format!("{:?}", request), format!("{:?}", deserialized));
  }
}

#[test]
fn test_raft_response_serialization_roundtrip() {
  let responses = vec![
    RaftResponse {
      success: true,
      message: None,
    },
    RaftResponse {
      success: false,
      message: Some("error details".to_string()),
    },
  ];

  for response in responses {
    let serialized = serde_json::to_string(&response).expect("serialize should succeed");
    let deserialized: RaftResponse =
      serde_json::from_str(&serialized).expect("deserialize should succeed");
    assert_eq!(response.success, deserialized.success);
    assert_eq!(response.message, deserialized.message);
  }
}

#[test]
fn test_raft_node_serialization_roundtrip() {
  let node = RaftNode {
    address: "192.168.1.1:8080".to_string(),
  };
  let serialized = serde_json::to_string(&node).expect("serialize should succeed");
  let deserialized: RaftNode =
    serde_json::from_str(&serialized).expect("deserialize should succeed");
  assert_eq!(node, deserialized);
}

// ===================================================================
// Log store tests
// ===================================================================

#[tokio::test]
async fn test_log_store_append_and_read() {
  let mut store = InMemoryLogStore::new();

  // Initially empty.
  let state = store.get_log_state().await.expect("get_log_state should work");
  assert!(state.last_log_id.is_none());
  assert!(state.last_purged_log_id.is_none());

  // Create and append an entry.
  let entry = Entry::new_blank(openraft::LogId::new(committed_leader(), 1));

  store
    .append(vec![entry.clone()], IOFlushed::noop())
    .await
    .expect("append should succeed");

  // Should be readable.
  let mut reader = store.get_log_reader().await;
  let entries = reader
    .try_get_log_entries(0..=1)
    .await
    .expect("reading entries should work");
  assert_eq!(entries.len(), 1);

  // Log state should reflect the new entry.
  let state = store.get_log_state().await.expect("get_log_state should work");
  assert!(state.last_log_id.is_some());
}

#[tokio::test]
async fn test_log_store_append_multiple_entries() {
  let mut store = InMemoryLogStore::new();

  let leader = committed_leader();
  let entries: Vec<Entry> = (1..=5)
    .map(|index| Entry::new_blank(openraft::LogId::new(leader.clone(), index)))
    .collect();

  store
    .append(entries, IOFlushed::noop())
    .await
    .expect("append should succeed");

  let mut reader = store.get_log_reader().await;
  let read_entries = reader
    .try_get_log_entries(1..=5)
    .await
    .expect("reading entries should work");
  assert_eq!(read_entries.len(), 5);
}

#[tokio::test]
async fn test_log_store_truncate() {
  let mut store = InMemoryLogStore::new();

  let leader = committed_leader();
  let entries: Vec<Entry> = (1..=5)
    .map(|index| Entry::new_blank(openraft::LogId::new(leader.clone(), index)))
    .collect();

  store
    .append(entries, IOFlushed::noop())
    .await
    .expect("append should succeed");

  // Truncate after index 3 -- entries 4, 5 should be removed.
  let truncate_at = openraft::LogId::new(leader.clone(), 3);
  store
    .truncate_after(Some(truncate_at))
    .await
    .expect("truncate should succeed");

  let mut reader = store.get_log_reader().await;
  let remaining = reader
    .try_get_log_entries(1..=10)
    .await
    .expect("reading entries should work");
  assert_eq!(remaining.len(), 3, "should have 3 entries after truncation");
}

#[tokio::test]
async fn test_log_store_truncate_all() {
  let mut store = InMemoryLogStore::new();

  let leader = committed_leader();
  let entry = Entry::new_blank(openraft::LogId::new(leader.clone(), 1));

  store
    .append(vec![entry], IOFlushed::noop())
    .await
    .expect("append should succeed");

  store
    .truncate_after(None)
    .await
    .expect("truncate all should succeed");

  let mut reader = store.get_log_reader().await;
  let remaining = reader
    .try_get_log_entries(0..=100)
    .await
    .expect("reading should work");
  assert!(remaining.is_empty(), "all entries should be removed");
}

#[tokio::test]
async fn test_log_store_purge() {
  let mut store = InMemoryLogStore::new();

  let leader = committed_leader();
  let entries: Vec<Entry> = (1..=5)
    .map(|index| Entry::new_blank(openraft::LogId::new(leader.clone(), index)))
    .collect();

  store
    .append(entries, IOFlushed::noop())
    .await
    .expect("append should succeed");

  let purge_at = openraft::LogId::new(leader.clone(), 3);
  store.purge(purge_at).await.expect("purge should succeed");

  let mut reader = store.get_log_reader().await;
  let remaining = reader
    .try_get_log_entries(1..=10)
    .await
    .expect("reading should work");
  assert_eq!(remaining.len(), 2, "entries 4, 5 should remain after purging through 3");

  let state = store.get_log_state().await.unwrap();
  assert!(state.last_purged_log_id.is_some());
}

#[tokio::test]
async fn test_log_store_vote_persistence() {
  let mut store = InMemoryLogStore::new();

  // Initially no vote.
  let mut reader = store.get_log_reader().await;
  let vote = reader.read_vote().await.expect("read_vote should work");
  assert!(vote.is_none(), "no vote should exist initially");

  // Save a vote.
  let new_vote = openraft::impls::Vote::new(1u64, 1u64);
  store
    .save_vote(&new_vote)
    .await
    .expect("save_vote should succeed");

  // Read back through a fresh reader.
  let mut reader = store.get_log_reader().await;
  let saved_vote = reader
    .read_vote()
    .await
    .expect("read_vote should work")
    .expect("vote should exist after saving");
  assert_eq!(saved_vote, new_vote);
}

#[tokio::test]
async fn test_log_store_read_empty_range() {
  let mut store = InMemoryLogStore::new();
  let mut reader = store.get_log_reader().await;
  let entries = reader
    .try_get_log_entries(0..=100)
    .await
    .expect("reading empty store should work");
  assert!(entries.is_empty());
}

// ===================================================================
// State machine tests
// ===================================================================

#[tokio::test]
async fn test_state_machine_initial_state() {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let mut state_machine = ChunkStateMachine::new(chunk_storage);

  let (last_applied, _membership) = state_machine
    .applied_state()
    .await
    .expect("applied_state should work");
  assert!(
    last_applied.is_none(),
    "no log should be applied initially"
  );
}

#[tokio::test]
async fn test_state_machine_applies_store_chunk() {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let mut state_machine = ChunkStateMachine::new(chunk_storage.clone());

  let data = b"state machine test data".to_vec();
  let hash = hash_data(&data).to_vec();

  let entry = Entry::new_normal(
    openraft::LogId::new(committed_leader(), 1),
    RaftRequest::StoreChunk {
      hash: hash.clone(),
      data: data.clone(),
    },
  );

  // Create a stream of (entry, None) tuples to feed to apply().
  let items: Vec<Result<(_, Option<openraft::storage::ApplyResponder<TypeConfig>>), io::Error>> =
    vec![Ok((entry, None))];
  let stream = futures_util::stream::iter(items);

  state_machine
    .apply(stream)
    .await
    .expect("apply should succeed");

  // Verify the chunk was stored.
  let mut chunk_hash = [0u8; 32];
  chunk_hash.copy_from_slice(&hash);
  assert!(
    chunk_storage.has_chunk(&chunk_hash).unwrap(),
    "chunk should exist after applying StoreChunk"
  );
}

#[tokio::test]
async fn test_state_machine_applies_delete_chunk() {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());

  // Pre-populate a chunk.
  let data = b"to be deleted via state machine".to_vec();
  let chunk = aeordb::storage::chunk::Chunk::new(data.clone());
  let hash = chunk.hash;
  chunk_storage.store_chunk(&chunk).unwrap();
  assert!(chunk_storage.has_chunk(&hash).unwrap());

  let mut state_machine = ChunkStateMachine::new(chunk_storage.clone());

  let entry = Entry::new_normal(
    openraft::LogId::new(committed_leader(), 1),
    RaftRequest::DeleteChunk {
      hash: hash.to_vec(),
    },
  );

  let items: Vec<Result<(_, Option<openraft::storage::ApplyResponder<TypeConfig>>), io::Error>> =
    vec![Ok((entry, None))];
  let stream = futures_util::stream::iter(items);

  state_machine
    .apply(stream)
    .await
    .expect("apply should succeed");

  assert!(
    !chunk_storage.has_chunk(&hash).unwrap(),
    "chunk should be gone after applying DeleteChunk"
  );
}

#[tokio::test]
async fn test_state_machine_applies_multiple_entries() {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let mut state_machine = ChunkStateMachine::new(chunk_storage.clone());

  let leader = committed_leader();

  let mut items = Vec::new();
  for index in 1..=5u64 {
    let data = format!("sm-chunk-{}", index).into_bytes();
    let hash = hash_data(&data).to_vec();
    let entry = Entry::new_normal(
      openraft::LogId::new(leader.clone(), index),
      RaftRequest::StoreChunk { hash, data },
    );
    items.push(Ok((entry, None)));
  }

  let stream = futures_util::stream::iter(items);
  state_machine
    .apply(stream)
    .await
    .expect("apply should succeed");

  assert_eq!(
    chunk_storage.chunk_count().unwrap(),
    5,
    "should have 5 chunks"
  );

  let (last_applied, _) = state_machine.applied_state().await.unwrap();
  assert_eq!(last_applied.unwrap().index, 5);
}

#[tokio::test]
async fn test_state_machine_snapshot_none_initially() {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let mut state_machine = ChunkStateMachine::new(chunk_storage);

  let snapshot = state_machine
    .get_current_snapshot()
    .await
    .expect("get_current_snapshot should work");
  assert!(snapshot.is_none(), "no snapshot initially");
}

#[tokio::test]
async fn test_state_machine_snapshot_after_apply() {
  let chunk_storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
  let mut state_machine = ChunkStateMachine::new(chunk_storage);

  let entry = Entry::new_blank(openraft::LogId::new(committed_leader(), 1));

  let items: Vec<Result<(_, Option<openraft::storage::ApplyResponder<TypeConfig>>), io::Error>> =
    vec![Ok((entry, None))];
  let stream = futures_util::stream::iter(items);
  state_machine.apply(stream).await.unwrap();

  let snapshot = state_machine
    .get_current_snapshot()
    .await
    .expect("get_current_snapshot should work");
  assert!(
    snapshot.is_some(),
    "snapshot should exist after applying entries"
  );
}

// ===================================================================
// Raft metrics test
// ===================================================================

#[tokio::test]
async fn test_raft_metrics_available() {
  let (manager, _storage) = create_bootstrapped_node().await;

  use openraft::rt::WatchReceiver;
  let metrics_receiver = manager.raft.metrics();
  let metrics = metrics_receiver.borrow_watched().clone();

  // The node should report its own ID.
  assert_eq!(metrics.id, 1);
}

// ===================================================================
// Display trait tests
// ===================================================================

#[test]
fn test_raft_request_display() {
  let request = RaftRequest::StoreChunk {
    hash: vec![0; 32],
    data: vec![1, 2, 3],
  };
  let display = format!("{}", request);
  assert!(display.contains("StoreChunk"));

  let request = RaftRequest::DeleteChunk { hash: vec![0; 32] };
  let display = format!("{}", request);
  assert!(display.contains("DeleteChunk"));

  let request = RaftRequest::StoreHashMap {
    key: "my-key".to_string(),
    hash_map_data: vec![],
  };
  let display = format!("{}", request);
  assert!(display.contains("StoreHashMap"));
  assert!(display.contains("my-key"));
}

#[test]
fn test_raft_response_display() {
  let response = RaftResponse {
    success: true,
    message: None,
  };
  assert!(format!("{}", response).contains("success=true"));

  let response = RaftResponse {
    success: false,
    message: Some("bad".to_string()),
  };
  let display = format!("{}", response);
  assert!(display.contains("success=false"));
  assert!(display.contains("bad"));
}

#[test]
fn test_raft_node_display() {
  let node = RaftNode {
    address: "10.0.0.1:3000".to_string(),
  };
  assert!(format!("{}", node).contains("10.0.0.1:3000"));
}
