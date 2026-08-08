use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use aeordb::engine::conflict_store::list_conflicts;
use aeordb::engine::directory_ops::file_path_hash;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::gc::run_gc;
use aeordb::engine::peer_connection::{PeerConfig, PeerManager};
use aeordb::engine::sync_engine::{PeerSyncState, SyncConfig, SyncEngine};
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::engine::virtual_clock::PeerClockTracker;
use aeordb::engine::{DirectoryOps, EventBus, FileRecord, RequestContext, StorageEngine};
use aeordb::server::create_temp_engine_for_tests;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_sync_engine(engine: Arc<StorageEngine>) -> (SyncEngine, Arc<PeerManager>) {
  let peer_manager = Arc::new(PeerManager::new());
  let clock_tracker = Arc::new(PeerClockTracker::new(30_000));
  let config = SyncConfig { periodic_interval_secs: 30 };
  let sync_engine = SyncEngine::new(engine, Arc::clone(&peer_manager), Arc::clone(&clock_tracker), config);
  (sync_engine, peer_manager)
}

fn add_active_peer(peer_manager: &PeerManager, node_id: u64) {
  add_active_peer_at(peer_manager, node_id, format!("http://localhost:{}", 9000 + node_id));
}

fn add_active_peer_at(peer_manager: &PeerManager, node_id: u64, address: String) {
  peer_manager.add_peer(&PeerConfig {
    node_id,
    address,
    label: Some(format!("peer-{}", node_id)),
    sync_paths: None,
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  });
  peer_manager.start_honeymoon(node_id, 1000);
  peer_manager.activate_peer(node_id);
}

#[derive(Clone)]
struct MaliciousPeerState {
  chunks_requests: Arc<AtomicUsize>,
}

async fn malicious_diff() -> Json<Value> {
  let chunk_hash = "22".repeat(32);
  Json(json!({
    "root_hash": "00".repeat(32),
    "changes": {
      "files_added": [{
        "path": "/.aeordb-system/config/secret.json",
        "hash": "11".repeat(32),
        "size": 1,
        "chunk_hashes": [chunk_hash.clone()],
        "content_type": "application/json"
      }],
      "files_modified": [],
      "files_deleted": [],
      "symlinks_added": [],
      "symlinks_modified": [],
      "symlinks_deleted": []
    },
    "chunk_hashes_needed": [chunk_hash]
  }))
}

async fn malicious_chunks(State(state): State<MaliciousPeerState>) -> Json<Value> {
  state.chunks_requests.fetch_add(1, Ordering::SeqCst);
  Json(json!({ "chunks": [] }))
}

async fn start_malicious_peer() -> (String, Arc<AtomicUsize>, CancellationToken, tokio::task::JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  let chunks_requests = Arc::new(AtomicUsize::new(0));
  let state = MaliciousPeerState { chunks_requests: Arc::clone(&chunks_requests) };
  let cancellation = CancellationToken::new();
  let shutdown = cancellation.clone();
  let application = Router::new().route("/sync/diff", post(malicious_diff)).route("/sync/chunks", post(malicious_chunks)).with_state(state);
  let handle = tokio::spawn(async move {
    axum::serve(listener, application).with_graceful_shutdown(shutdown.cancelled_owned()).await.unwrap();
  });
  (format!("http://{address}"), chunks_requests, cancellation, handle)
}

#[derive(Clone)]
struct ScriptedPeerState {
  diff: Arc<Value>,
  chunks: Arc<Value>,
  chunks_requests: Arc<AtomicUsize>,
  requested_hashes: Arc<Mutex<Vec<String>>>,
}

async fn scripted_diff(State(state): State<ScriptedPeerState>) -> Json<Value> {
  Json((*state.diff).clone())
}

async fn scripted_chunks(State(state): State<ScriptedPeerState>, Json(payload): Json<Value>) -> Json<Value> {
  state.chunks_requests.fetch_add(1, Ordering::SeqCst);
  if let Some(hashes) = payload.get("hashes").and_then(Value::as_array) {
    state.requested_hashes.lock().unwrap().extend(hashes.iter().filter_map(Value::as_str).map(str::to_string));
  }
  Json((*state.chunks).clone())
}

async fn start_scripted_peer(
  diff: Value,
  chunks: Value,
) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>, CancellationToken, tokio::task::JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  let chunks_requests = Arc::new(AtomicUsize::new(0));
  let requested_hashes = Arc::new(Mutex::new(Vec::new()));
  let state = ScriptedPeerState {
    diff: Arc::new(diff),
    chunks: Arc::new(chunks),
    chunks_requests: Arc::clone(&chunks_requests),
    requested_hashes: Arc::clone(&requested_hashes),
  };
  let cancellation = CancellationToken::new();
  let shutdown = cancellation.clone();
  let application = Router::new().route("/sync/diff", post(scripted_diff)).route("/sync/chunks", post(scripted_chunks)).with_state(state);
  let handle = tokio::spawn(async move {
    axum::serve(listener, application).with_graceful_shutdown(shutdown.cancelled_owned()).await.unwrap();
  });
  (format!("http://{address}"), chunks_requests, requested_hashes, cancellation, handle)
}

#[derive(Clone)]
struct SequencedPeerState {
  diffs: Arc<Mutex<std::collections::VecDeque<Value>>>,
  chunks: Arc<std::collections::HashMap<String, Value>>,
  diff_requests: Arc<Mutex<Vec<Value>>>,
}

async fn sequenced_diff(State(state): State<SequencedPeerState>, Json(payload): Json<Value>) -> Json<Value> {
  state.diff_requests.lock().unwrap().push(payload);
  Json(state.diffs.lock().unwrap().pop_front().expect("test peer received more diff requests than scripted"))
}

async fn sequenced_chunks(State(state): State<SequencedPeerState>, Json(payload): Json<Value>) -> Json<Value> {
  let chunks = payload["hashes"]
    .as_array()
    .expect("chunk request hashes")
    .iter()
    .map(|hash| state.chunks.get(hash.as_str().expect("encoded chunk hash")).expect("requested scripted chunk").clone())
    .collect::<Vec<_>>();
  Json(json!({ "chunks": chunks }))
}

async fn start_sequenced_peer(
  diffs: Vec<Value>,
  chunks: Vec<Value>,
) -> (String, Arc<Mutex<Vec<Value>>>, CancellationToken, tokio::task::JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  let chunks = chunks
    .into_iter()
    .map(|chunk| (chunk["hash"].as_str().expect("scripted chunk hash").to_string(), chunk))
    .collect::<std::collections::HashMap<_, _>>();
  let diff_requests = Arc::new(Mutex::new(Vec::new()));
  let state =
    SequencedPeerState { diffs: Arc::new(Mutex::new(diffs.into())), chunks: Arc::new(chunks), diff_requests: Arc::clone(&diff_requests) };
  let cancellation = CancellationToken::new();
  let shutdown = cancellation.clone();
  let application = Router::new().route("/sync/diff", post(sequenced_diff)).route("/sync/chunks", post(sequenced_chunks)).with_state(state);
  let handle = tokio::spawn(async move {
    axum::serve(listener, application).with_graceful_shutdown(shutdown.cancelled_owned()).await.unwrap();
  });
  (format!("http://{address}"), diff_requests, cancellation, handle)
}

fn remote_chunk(engine: &StorageEngine, data: &[u8]) -> (Vec<u8>, Value) {
  let hash = aeordb::engine::chunk_content_hash(data, &engine.hash_algo()).unwrap();
  let response = json!({
    "hash": hex::encode(&hash),
    "data": base64::engine::general_purpose::STANDARD.encode(data),
    "size": data.len(),
  });
  (hash, response)
}

fn remote_file_entry(engine: &StorageEngine, path: &str, content_type: Option<&str>, data: &[u8]) -> (Value, Value, String) {
  let (chunk_hash, chunk) = remote_chunk(engine, data);
  let file_hash = aeordb::engine::file_identity_hash(path, content_type, std::slice::from_ref(&chunk_hash), &engine.hash_algo()).unwrap();
  let content_hash = aeordb::engine::whole_file_content_hash(data, &engine.hash_algo()).unwrap();
  (
    json!({
      "path": path,
      "hash": hex::encode(file_hash),
      "content_hash": hex::encode(content_hash),
      "size": data.len(),
      "content_type": content_type,
      "created_at": 1_000,
      "updated_at": 2_000,
      "chunk_hashes": [hex::encode(&chunk_hash)],
    }),
    chunk,
    hex::encode(chunk_hash),
  )
}

fn remote_diff(root_hash: &str, files_added: Vec<Value>, symlinks_added: Vec<Value>, chunk_hashes_needed: Vec<String>) -> Value {
  json!({
    "root_hash": root_hash,
    "changes": {
      "files_added": files_added,
      "files_modified": [],
      "files_deleted": [],
      "symlinks_added": symlinks_added,
      "symlinks_modified": [],
      "symlinks_deleted": [],
    },
    "chunk_hashes_needed": chunk_hashes_needed,
  })
}

async fn stop_scripted_peer(cancellation: CancellationToken, server: tokio::task::JoinHandle<()>) {
  cancellation.cancel();
  server.await.unwrap();
}

async fn start_oversized_diff_peer() -> (String, tokio::task::JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  let handle = tokio::spawn(async move {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut request = [0_u8; 4096];
    let _ = socket.read(&mut request).await.unwrap();
    socket
      .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 134217729\r\nconnection: close\r\n\r\n")
      .await
      .unwrap();
    socket.flush().await.unwrap();
  });
  (format!("http://{address}"), handle)
}

async fn start_oversized_error_diff_peer() -> (String, tokio::task::JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  let handle = tokio::spawn(async move {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut request = [0_u8; 4096];
    let _ = socket.read(&mut request).await.unwrap();
    socket
      .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/plain\r\ncontent-length: 65537\r\nconnection: close\r\n\r\n")
      .await
      .unwrap();
    socket.flush().await.unwrap();
  });
  (format!("http://{address}"), handle)
}

async fn start_oversized_chunks_peer(diff: Value) -> (String, tokio::task::JoinHandle<()>) {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let address = listener.local_addr().unwrap();
  let diff_body = serde_json::to_vec(&diff).unwrap();
  let handle = tokio::spawn(async move {
    let (mut diff_socket, _) = listener.accept().await.unwrap();
    let mut request = [0_u8; 4096];
    let _ = diff_socket.read(&mut request).await.unwrap();
    let headers =
      format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", diff_body.len());
    diff_socket.write_all(headers.as_bytes()).await.unwrap();
    diff_socket.write_all(&diff_body).await.unwrap();
    diff_socket.flush().await.unwrap();
    drop(diff_socket);

    let (mut chunk_socket, _) = listener.accept().await.unwrap();
    let _ = chunk_socket.read(&mut request).await.unwrap();
    chunk_socket
      .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100663297\r\nconnection: close\r\n\r\n")
      .await
      .unwrap();
    chunk_socket.flush().await.unwrap();
  });
  (format!("http://{address}"), handle)
}

fn store_file(engine: &StorageEngine, path: &str, data: &[u8]) {
  let context = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  ops.store_file_buffered(&context, path, data, Some("text/plain")).unwrap();
}

#[test]
fn local_sync_rejects_malformed_peer_configuration() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  DirectoryOps::new(&local)
    .store_file_buffered(
      &RequestContext::system(),
      "/.aeordb-system/cluster/peers",
      b"not versioned peer configuration",
      Some("application/json"),
    )
    .unwrap();
  let (sync_engine, _) = make_sync_engine(local);

  let error = sync_engine.sync_with_local_engine(42, &remote).expect_err("sync must not interpret malformed peer authority as no filter");
  assert!(error.contains("Failed to load configuration for peer 42"), "unexpected error: {error}");
}

fn read_file(engine: &StorageEngine, path: &str) -> Vec<u8> {
  let ops = DirectoryOps::new(engine);
  ops.read_file_buffered(path).unwrap()
}

fn file_exists(engine: &StorageEngine, path: &str) -> bool {
  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(engine, &head).unwrap();
  tree.files.contains_key(path)
}

// ---------------------------------------------------------------------------
// Test: SyncEngine creation doesn't panic
// ---------------------------------------------------------------------------

#[test]
fn test_sync_engine_creation() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(engine);

  // Verify accessors work
  assert!(sync_engine.engine().head_hash().is_ok());
  assert!(sync_engine.peer_manager().all_peers().is_empty());
}

// ---------------------------------------------------------------------------
// Test: sync returns error for non-Active peer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_with_non_active_peer_disconnected() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(engine);

  // Add peer but leave it Disconnected
  peer_manager.add_peer(&PeerConfig {
    node_id: 42,
    address: "http://localhost:9042".to_string(),
    label: None,
    sync_paths: None,
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  });

  let result = sync_engine.sync_with_peer(42).await;
  assert!(result.is_err());
  let error = result.unwrap_err();
  assert!(error.contains("not Active"), "Expected 'not Active' error, got: {}", error);
}

#[tokio::test]
async fn test_sync_with_non_active_peer_honeymoon() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(engine);

  peer_manager.add_peer(&PeerConfig {
    node_id: 43,
    address: "http://localhost:9043".to_string(),
    label: None,
    sync_paths: None,
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  });
  peer_manager.start_honeymoon(43, 1000);

  let result = sync_engine.sync_with_peer(43).await;
  assert!(result.is_err());
  assert!(result.unwrap_err().contains("not Active"));
}

// ---------------------------------------------------------------------------
// Test: sync returns error for unknown peer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_with_unknown_peer() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(engine);

  let result = sync_engine.sync_with_peer(999).await;
  assert!(result.is_err());
  assert!(result.unwrap_err().contains("not found"), "Expected 'not found' error");
}

// ---------------------------------------------------------------------------
// Test: sync_all_peers skips inactive peers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_all_peers_skips_inactive() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(engine);

  // Add one disconnected, one honeymoon, one active peer
  peer_manager.add_peer(&PeerConfig {
    node_id: 1,
    address: "http://localhost:9001".to_string(),
    label: Some("disconnected".to_string()),
    sync_paths: None,
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  });
  // node_id=1 stays Disconnected

  peer_manager.add_peer(&PeerConfig {
    node_id: 2,
    address: "http://localhost:9002".to_string(),
    label: Some("honeymoon".to_string()),
    sync_paths: None,
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  });
  peer_manager.start_honeymoon(2, 1000);
  // node_id=2 stays in Honeymoon

  peer_manager.add_peer(&PeerConfig {
    node_id: 3,
    address: "http://localhost:9003".to_string(),
    label: Some("active".to_string()),
    sync_paths: None,
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  });
  peer_manager.start_honeymoon(3, 1000);
  peer_manager.activate_peer(3);

  let results = sync_engine.sync_all_peers().await;

  // Only the active peer (node_id=3) should have been attempted
  assert_eq!(results.len(), 1, "Only active peers should be synced");
  assert_eq!(results[0].0, 3);
  // It will error because remote HTTP sync is not implemented, which is expected
  assert!(results[0].1.is_err());
}

// ---------------------------------------------------------------------------
// Test: sync_all_peers with no peers returns empty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sync_all_peers_empty() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(engine);

  let results = sync_engine.sync_all_peers().await;
  assert!(results.is_empty());
}

// ---------------------------------------------------------------------------
// Test: peer sync state persistence
// ---------------------------------------------------------------------------

#[test]
fn test_peer_sync_state_persistence() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(engine);

  // No state initially
  assert!(sync_engine.load_peer_sync_state(42).unwrap().is_none());

  // After a sync, state should be persisted
  // We test this through system_store directly
  let ctx = aeordb::engine::RequestContext::system();
  let state =
    PeerSyncState { last_synced_root_hash: Some("deadbeef".to_string()), last_local_root_hash: None, last_sync_at: Some(1234567890) };
  aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 42, &state).unwrap();

  let loaded = sync_engine.load_peer_sync_state(42).unwrap();
  assert!(loaded.is_some());
  let loaded = loaded.unwrap();
  assert_eq!(loaded.last_synced_root_hash, Some("deadbeef".to_string()));
  assert_eq!(loaded.last_local_root_hash, None);
  assert_eq!(loaded.last_sync_at, Some(1234567890));
}

#[test]
fn peer_sync_state_writes_v1_and_reads_legacy_v0_without_a_local_merge_base() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(Arc::clone(&engine));
  let context = RequestContext::system();
  let state =
    PeerSyncState { last_synced_root_hash: Some("11".repeat(32)), last_local_root_hash: Some("22".repeat(32)), last_sync_at: Some(1234) };
  aeordb::engine::system_store::store_peer_sync_state(&engine, &context, 55, &state).unwrap();
  let persisted: Value =
    serde_json::from_slice(&DirectoryOps::new(&engine).read_file_buffered("/.aeordb-system/sync-peers/55").unwrap()).unwrap();
  assert_eq!(persisted["$v"], 1);
  assert_eq!(sync_engine.load_peer_sync_state(55).unwrap().unwrap().last_local_root_hash, state.last_local_root_hash);

  DirectoryOps::new(&engine)
    .store_file_buffered(
      &context,
      "/.aeordb-system/sync-peers/56",
      format!(r#"{{"$v":0,"last_synced_root_hash":"{}","last_sync_at":99}}"#, "33".repeat(32)).as_bytes(),
      Some("application/json"),
    )
    .unwrap();
  let legacy = sync_engine.load_peer_sync_state(56).unwrap().unwrap();
  assert_eq!(legacy.last_synced_root_hash, Some("33".repeat(32)));
  assert_eq!(legacy.last_local_root_hash, None);
  assert_eq!(legacy.last_sync_at, Some(99));
}

#[test]
fn peer_sync_state_loader_surfaces_malformed_persisted_authority() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(engine);
  DirectoryOps::new(sync_engine.engine())
    .store_file_buffered(
      &RequestContext::system(),
      "/.aeordb-system/sync-peers/42",
      b"not a versioned peer sync state",
      Some("application/json"),
    )
    .unwrap();

  sync_engine.load_peer_sync_state(42).expect_err("malformed peer sync authority must not become an absent state");
}

#[test]
fn local_sync_rejects_a_semantically_invalid_persisted_base_hash() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&remote, "/must-not-sync.txt", b"remote bytes");
  let (sync_engine, _peer_manager) = make_sync_engine(Arc::clone(&local));
  let state = PeerSyncState { last_synced_root_hash: Some("not-a-hash".to_string()), last_local_root_hash: None, last_sync_at: Some(1) };
  aeordb::engine::system_store::store_peer_sync_state(&local, &RequestContext::system(), 43, &state).unwrap();
  let head_before = local.head_hash().unwrap();

  let error = sync_engine.sync_with_local_engine(43, &remote).expect_err("invalid persisted base hash must fail closed");

  assert!(error.contains("remote checkpoint") && error.contains("valid hex"), "unexpected error: {error}");
  assert_eq!(local.head_hash().unwrap(), head_before);
  assert!(!file_exists(&local, "/must-not-sync.txt"));
  assert_eq!(sync_engine.load_peer_sync_state(43).unwrap().unwrap().last_synced_root_hash, Some("not-a-hash".to_string()));
}

#[test]
fn test_peer_sync_state_overwrite() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(engine);
  let ctx = aeordb::engine::RequestContext::system();

  // Store initial state
  let state1 = PeerSyncState { last_synced_root_hash: Some("aaa".to_string()), last_local_root_hash: None, last_sync_at: Some(100) };
  aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 42, &state1).unwrap();

  // Overwrite with new state
  let state2 = PeerSyncState { last_synced_root_hash: Some("bbb".to_string()), last_local_root_hash: None, last_sync_at: Some(200) };
  aeordb::engine::system_store::store_peer_sync_state(sync_engine.engine(), &ctx, 42, &state2).unwrap();

  let loaded = sync_engine.load_peer_sync_state(42).unwrap().unwrap();
  assert_eq!(loaded.last_synced_root_hash, Some("bbb".to_string()));
  assert_eq!(loaded.last_sync_at, Some(200));
}

#[test]
fn test_peer_sync_state_multiple_peers() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, _peer_manager) = make_sync_engine(engine);
  let ctx = aeordb::engine::RequestContext::system();

  aeordb::engine::system_store::store_peer_sync_state(
    sync_engine.engine(),
    &ctx,
    1,
    &PeerSyncState { last_synced_root_hash: Some("hash1".to_string()), last_local_root_hash: None, last_sync_at: Some(100) },
  )
  .unwrap();

  aeordb::engine::system_store::store_peer_sync_state(
    sync_engine.engine(),
    &ctx,
    2,
    &PeerSyncState { last_synced_root_hash: Some("hash2".to_string()), last_local_root_hash: None, last_sync_at: Some(200) },
  )
  .unwrap();

  let state1 = sync_engine.load_peer_sync_state(1).unwrap().unwrap();
  let state2 = sync_engine.load_peer_sync_state(2).unwrap().unwrap();

  assert_eq!(state1.last_synced_root_hash, Some("hash1".to_string()));
  assert_eq!(state2.last_synced_root_hash, Some("hash2".to_string()));
  assert!(sync_engine.load_peer_sync_state(3).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync cycle — two engines, no HTTP
// Engine A has file /a.txt, Engine B has file /b.txt.
// After sync, Engine A should have both files.
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_both_add_different_files() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Store different files in each engine
  store_file(&engine_a, "/a.txt", b"content from A");
  store_file(&engine_b, "/b.txt", b"content from B");

  // Set up sync engine for A
  let (sync_engine_a, _peer_manager_a) = make_sync_engine(Arc::clone(&engine_a));

  // Sync A with B's engine directly
  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied, "Changes should have been applied");
  assert_eq!(result.conflicts_detected, 0, "No conflicts expected");
  assert!(result.operations_applied > 0, "Operations should have been applied");

  // Engine A should now have both files
  assert!(file_exists(&engine_a, "/a.txt"), "A should still have /a.txt");
  assert!(file_exists(&engine_a, "/b.txt"), "A should now have /b.txt from B");

  // Verify content
  assert_eq!(read_file(&engine_a, "/a.txt"), b"content from A");
  assert_eq!(read_file(&engine_a, "/b.txt"), b"content from B");
}

#[test]
fn local_sync_engine_emits_the_shared_namespace_acknowledgement() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&remote, "/from-remote.txt", b"remote event payload");

  let peer_manager = Arc::new(PeerManager::new());
  let clock_tracker = Arc::new(PeerClockTracker::new(30_000));
  let event_bus = Arc::new(EventBus::new());
  let mut receiver = event_bus.subscribe();
  let sync_engine =
    SyncEngine::new(Arc::clone(&local), peer_manager, clock_tracker, SyncConfig { periodic_interval_secs: 30 }).with_event_bus(event_bus);

  sync_engine.sync_with_local_engine(72, &remote).unwrap();

  let events = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
  let event = events
    .iter()
    .find(|event| event.event_type == "entries_created" && event.payload["mutation_kind"] == "sync_apply")
    .expect("sync orchestrator must publish the coordinator acknowledgement on its configured event bus");
  assert!(uuid::Uuid::parse_str(event.payload["operation_id"].as_str().unwrap()).is_ok());
  assert!(event.payload["publication_sequence"].as_u64().unwrap() > 0);
  assert_eq!(event.payload["entries"][0]["path"], "/from-remote.txt");
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync when engines are already identical
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_identical_engines() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Both engines start with the same empty state (identical HEAD)
  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

  let head_a = engine_a.head_hash().unwrap();
  let head_b = engine_b.head_hash().unwrap();
  assert_eq!(head_a, head_b, "Fresh engines should have identical HEAD");

  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
  assert!(!result.changes_applied, "No changes expected for identical engines");
  assert_eq!(result.conflicts_detected, 0);
  assert_eq!(result.operations_applied, 0);
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync with conflict (same file, different content)
// Both engines modify the same path => LWW conflict
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_conflict_same_file() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Force the local version to win so the remote loser must be retained as an
  // immutable conflict dependency rather than as the published path record.
  store_file(&engine_b, "/shared.txt", b"version from B");
  std::thread::sleep(std::time::Duration::from_millis(2));
  store_file(&engine_a, "/shared.txt", b"version from A");

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  // There should be a conflict detected (both added the same path)
  assert!(result.conflicts_detected > 0, "Should detect a conflict");

  // The file should still exist
  assert!(file_exists(&engine_a, "/shared.txt"));

  // Conflicts should be stored in /.aeordb-conflicts/
  let conflicts = list_conflicts(&engine_a).unwrap();
  assert!(!conflicts.is_empty(), "Conflicts should be stored in /.aeordb-conflicts/");
  assert_eq!(read_file(&engine_a, "/shared.txt"), b"version from A");

  aeordb::engine::conflict_store::resolve_conflict(&engine_a, &RequestContext::system(), "/shared.txt", "loser").unwrap();
  assert_eq!(read_file(&engine_a, "/shared.txt"), b"version from B");
}

#[test]
fn unresolved_sync_conflict_retains_both_versions_across_gc() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&remote, "/gc-conflict.txt", b"remote version that only conflict evidence retains");
  std::thread::sleep(std::time::Duration::from_millis(2));
  store_file(&local, "/gc-conflict.txt", b"newer local winner");
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));

  let result = sync_engine.sync_with_local_engine(93, &remote).unwrap();
  assert_eq!(result.conflicts_detected, 1);
  assert_eq!(read_file(&local, "/gc-conflict.txt"), b"newer local winner");

  run_gc(&local, &RequestContext::system(), false).unwrap();
  aeordb::engine::conflict_store::resolve_conflict(&local, &RequestContext::system(), "/gc-conflict.txt", "loser").unwrap();

  assert_eq!(read_file(&local, "/gc-conflict.txt"), b"remote version that only conflict evidence retains");
}

#[test]
fn local_sync_rejects_wrong_type_collision_for_a_retained_remote_conflict_version() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&remote, "/collision-conflict.txt", b"older remote");
  std::thread::sleep(std::time::Duration::from_millis(2));
  store_file(&local, "/collision-conflict.txt", b"newer local");
  let remote_record = DirectoryOps::new(&remote).get_metadata("/collision-conflict.txt").unwrap().unwrap();
  let remote_identity = aeordb::engine::file_identity_hash(
    &remote_record.path,
    remote_record.content_type.as_deref(),
    &remote_record.chunk_hashes,
    &remote.hash_algo(),
  )
  .unwrap();
  local.store_entry(EntryType::Chunk, &remote_identity, b"wrong retained-version type").unwrap();
  let local_before = read_file(&local, "/collision-conflict.txt");
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));

  let error =
    sync_engine.sync_with_local_engine(92, &remote).expect_err("wrong-type conflict dependency must reject the complete namespace receipt");

  assert!(error.contains("collision") || error.contains("Chunk") || error.contains("FileRecord"), "unexpected error: {error}");
  assert_eq!(read_file(&local, "/collision-conflict.txt"), local_before);
  assert!(list_conflicts(&local).unwrap().is_empty());
  assert!(sync_engine.load_peer_sync_state(92).unwrap().is_none());
}

#[test]
fn local_sync_conflict_evidence_failure_rejects_the_entire_namespace_receipt() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&local, "/shared.txt", b"base");
  store_file(&remote, "/shared.txt", b"base");
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));
  sync_engine.sync_with_local_engine(91, &remote).unwrap();

  store_file(&local, "/shared.txt", b"local version");
  store_file(&remote, "/shared.txt", b"remote version");
  store_file(&remote, "/remote-only.txt", b"must remain hidden on failure");
  let conflict_parent_key = aeordb::engine::directory_path_hash("/.aeordb-conflicts/shared.txt", &local.hash_algo()).unwrap();
  local.store_entry(EntryType::Chunk, &conflict_parent_key, b"wrong conflict-parent locator type").unwrap();
  let local_before = read_file(&local, "/shared.txt");
  let checkpoint_before = sync_engine.load_peer_sync_state(91).unwrap().unwrap().last_synced_root_hash;

  let error =
    sync_engine.sync_with_local_engine(91, &remote).expect_err("required conflict evidence failure must reject the complete sync receipt");

  assert!(error.contains("conflict") || error.contains("directory") || error.contains("file"), "unexpected error: {error}");
  assert_eq!(read_file(&local, "/shared.txt"), local_before);
  assert!(!file_exists(&local, "/remote-only.txt"), "non-conflicting operation escaped a rejected sync receipt");
  assert_eq!(sync_engine.load_peer_sync_state(91).unwrap().unwrap().last_synced_root_hash, checkpoint_before);
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync — one side adds, other side is empty (initial sync)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_cycle_one_side_empty() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Only B has files
  store_file(&engine_b, "/from_b_1.txt", b"data 1");
  store_file(&engine_b, "/from_b_2.txt", b"data 2");
  store_file(&engine_b, "/subdir/nested.txt", b"nested data");

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert_eq!(result.conflicts_detected, 0);

  // A should now have all of B's files
  assert!(file_exists(&engine_a, "/from_b_1.txt"));
  assert!(file_exists(&engine_a, "/from_b_2.txt"));
  assert!(file_exists(&engine_a, "/subdir/nested.txt"));

  assert_eq!(read_file(&engine_a, "/from_b_1.txt"), b"data 1");
  assert_eq!(read_file(&engine_a, "/subdir/nested.txt"), b"nested data");
}

#[test]
fn local_peer_sync_transfers_only_registry_portable_state_and_its_chunks() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();
  store_file(&engine_b, "/ordinary.txt", b"ordinary");
  store_file(&engine_b, "/.aeordb-system/users/user.json", b"portable user");
  store_file(&engine_b, "/.aeordb-system/config/secret.json", b"node-local secret");

  let secret_key = file_path_hash("/.aeordb-system/config/secret.json", &engine_b.hash_algo()).unwrap();
  let (secret_header, _key, secret_data) = engine_b.get_entry(&secret_key).unwrap().unwrap();
  let secret_chunks =
    FileRecord::deserialize(&secret_data, engine_b.hash_algo().hash_length(), secret_header.entry_version).unwrap().chunk_hashes;
  let (sync_engine_a, _peer_manager) = make_sync_engine(Arc::clone(&engine_a));

  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert!(file_exists(&engine_a, "/ordinary.txt"));
  let destination_ops = DirectoryOps::new(&engine_a);
  assert_eq!(destination_ops.read_file_buffered("/.aeordb-system/users/user.json").unwrap(), b"portable user");
  assert!(destination_ops.read_file_buffered("/.aeordb-system/config/secret.json").is_err());
  for chunk_hash in secret_chunks {
    assert!(!engine_a.has_entry(&chunk_hash).unwrap(), "local sync copied a chunk belonging only to omitted node-local state");
  }
}

#[test]
fn local_peer_sync_detects_portable_state_without_an_ordinary_file_change() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();
  store_file(&engine_b, "/.aeordb-system/users/user.json", b"portable user");
  let (sync_engine_a, _peer_manager) = make_sync_engine(Arc::clone(&engine_a));

  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert_eq!(DirectoryOps::new(&engine_a).read_file_buffered("/.aeordb-system/users/user.json").unwrap(), b"portable user");
}

#[test]
fn local_selective_sync_does_not_copy_chunks_outside_the_selected_paths() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();
  store_file(&engine_b, "/included/file.txt", b"included content");
  store_file(&engine_b, "/omitted/file.txt", b"omitted content");
  let source_tree = walk_version_tree(&engine_b, &engine_b.head_hash().unwrap()).unwrap();
  let omitted_chunks = source_tree.files["/omitted/file.txt"].1.chunk_hashes.clone();
  let peer = PeerConfig {
    node_id: 2,
    address: "http://unused.invalid".to_string(),
    label: None,
    sync_paths: Some(vec!["/included/**".to_string()]),
    last_clock_offset_ms: None,
    last_wire_time_ms: None,
    last_jitter_ms: None,
    clock_state_at: None,
  };
  aeordb::engine::system_store::store_peer_configs(&engine_a, &RequestContext::system(), &[peer]).unwrap();
  let (sync_engine_a, _peer_manager) = make_sync_engine(Arc::clone(&engine_a));

  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert!(file_exists(&engine_a, "/included/file.txt"));
  assert!(!file_exists(&engine_a, "/omitted/file.txt"));
  for chunk_hash in omitted_chunks {
    assert!(!engine_a.has_entry(&chunk_hash).unwrap(), "selective sync copied a chunk outside the configured paths");
  }
}

#[test]
fn local_peer_sync_does_not_stage_a_hash_mismatched_remote_chunk() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  let path = "/remote/corrupt-chunk.txt";
  store_file(&remote, path, b"canonical bytes");
  let chunk_hash = DirectoryOps::new(&remote).get_metadata(path).unwrap().unwrap().chunk_hashes[0].clone();
  remote.store_entry(EntryType::Chunk, &chunk_hash, b"forged bytes").unwrap();
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));

  let error = sync_engine.sync_with_local_engine(88, &remote).expect_err("corrupt peer chunks must fail before local staging");

  assert!(error.contains("hash") || error.contains("canonical"), "unexpected error: {error}");
  assert!(!local.has_entry(&chunk_hash).unwrap(), "failed sync imported a corrupt immutable chunk");
  assert!(sync_engine.load_peer_sync_state(88).unwrap().is_none());
  assert!(DirectoryOps::new(&local).read_file_buffered(path).is_err());
}

#[test]
fn local_peer_sync_does_not_read_an_omitted_malformed_file_record() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();
  store_file(&engine_b, "/ordinary.txt", b"ordinary");
  store_file(&engine_b, "/.aeordb-system/config/secret.json", b"node-local secret");
  let secret = DirectoryOps::new(&engine_b)
    .list_directory("/.aeordb-system/config")
    .unwrap()
    .into_iter()
    .find(|entry| entry.name == "secret.json")
    .unwrap();
  engine_b.store_entry(EntryType::FileRecord, &secret.hash, b"malformed omitted record").unwrap();
  let (sync_engine_a, _peer_manager) = make_sync_engine(Arc::clone(&engine_a));

  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert!(file_exists(&engine_a, "/ordinary.txt"));
  assert!(DirectoryOps::new(&engine_a).read_file_buffered("/.aeordb-system/config/secret.json").is_err());
}

#[test]
fn local_peer_sync_rejects_unknown_protected_state_before_reading_its_body() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();
  store_file(&engine_b, "/.aeordb-future/value.bin", b"unknown");
  let unknown =
    DirectoryOps::new(&engine_b).list_directory("/.aeordb-future").unwrap().into_iter().find(|entry| entry.name == "value.bin").unwrap();
  engine_b.store_entry(EntryType::FileRecord, &unknown.hash, b"malformed unknown record").unwrap();
  let (sync_engine_a, _peer_manager) = make_sync_engine(Arc::clone(&engine_a));

  let error = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap_err();

  assert!(error.contains("unknown_protected_system_family"), "unexpected error: {error}");
  assert!(DirectoryOps::new(&engine_a).read_file_buffered("/.aeordb-future/value.bin").is_err());
}

// ---------------------------------------------------------------------------
// Test: LOCAL sync updates peer sync state
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_updates_peer_state() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  store_file(&engine_b, "/b.txt", b"hello");

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

  // No state before sync
  assert!(sync_engine_a.load_peer_sync_state(2).unwrap().is_none());

  sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  // State should be recorded after sync
  let state = sync_engine_a.load_peer_sync_state(2).unwrap();
  assert!(state.is_some(), "Sync state should be saved");
  let state = state.unwrap();
  assert!(state.last_synced_root_hash.is_some());
  assert!(state.last_sync_at.is_some());
}

// ---------------------------------------------------------------------------
// Test: Subsequent sync after initial sync (incremental)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_incremental_second_sync() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Initial: B has one file
  store_file(&engine_b, "/first.txt", b"first");

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

  // First sync
  let result1 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
  assert!(result1.changes_applied);
  assert!(file_exists(&engine_a, "/first.txt"));

  // Now B adds another file
  store_file(&engine_b, "/second.txt", b"second");

  // Second sync should pick up only the new file
  let result2 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
  assert!(result2.changes_applied);
  assert!(file_exists(&engine_a, "/second.txt"));
  assert_eq!(read_file(&engine_a, "/second.txt"), b"second");
}

#[test]
fn local_incremental_sync_uses_distinct_remote_and_local_merge_bases() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&local, "/local-only.txt", b"must never become a remote deletion");
  store_file(&remote, "/remote-first.txt", b"first remote version");
  let first_remote_root = remote.head_hash().unwrap();
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));

  sync_engine.sync_with_local_engine(94, &remote).unwrap();

  let first_local_root = local.head_hash().unwrap();
  assert_ne!(first_remote_root, first_local_root);
  let state = sync_engine.load_peer_sync_state(94).unwrap().unwrap();
  assert_eq!(state.last_synced_root_hash, Some(hex::encode(&first_remote_root)));
  assert_eq!(state.last_local_root_hash, Some(hex::encode(&first_local_root)));

  store_file(&remote, "/remote-second.txt", b"second remote version");
  sync_engine.sync_with_local_engine(94, &remote).unwrap();

  assert_eq!(read_file(&local, "/local-only.txt"), b"must never become a remote deletion");
  assert_eq!(read_file(&local, "/remote-first.txt"), b"first remote version");
  assert_eq!(read_file(&local, "/remote-second.txt"), b"second remote version");
}

#[test]
fn local_sync_upgrades_an_ambiguous_legacy_checkpoint_with_a_conservative_full_remote_diff() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&local, "/legacy-local.txt", b"local survives legacy upgrade");
  store_file(&remote, "/legacy-remote-first.txt", b"first remote");
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));
  sync_engine.sync_with_local_engine(95, &remote).unwrap();
  let ambiguous_local_root = local.head_hash().unwrap();
  DirectoryOps::new(&local)
    .store_file_buffered(
      &RequestContext::system(),
      "/.aeordb-system/sync-peers/95",
      format!(r#"{{"$v":0,"last_synced_root_hash":"{}","last_sync_at":1}}"#, hex::encode(ambiguous_local_root)).as_bytes(),
      Some("application/json"),
    )
    .unwrap();
  store_file(&remote, "/legacy-remote-second.txt", b"second remote");

  sync_engine.sync_with_local_engine(95, &remote).unwrap();

  assert_eq!(read_file(&local, "/legacy-local.txt"), b"local survives legacy upgrade");
  assert_eq!(read_file(&local, "/legacy-remote-first.txt"), b"first remote");
  assert_eq!(read_file(&local, "/legacy-remote-second.txt"), b"second remote");
  let upgraded = sync_engine.load_peer_sync_state(95).unwrap().unwrap();
  assert_eq!(upgraded.last_synced_root_hash, Some(hex::encode(remote.head_hash().unwrap())));
  assert!(upgraded.last_local_root_hash.is_some());
}

#[test]
fn local_sync_rejects_a_missing_current_remote_checkpoint_without_publishing() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&local, "/local-before-missing-base.txt", b"local");
  store_file(&remote, "/remote-must-stay-hidden.txt", b"remote");
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));
  let state = PeerSyncState {
    last_synced_root_hash: Some("44".repeat(32)),
    last_local_root_hash: Some(hex::encode(local.head_hash().unwrap())),
    last_sync_at: Some(1),
  };
  aeordb::engine::system_store::store_peer_sync_state(&local, &RequestContext::system(), 96, &state).unwrap();
  let head_before = local.head_hash().unwrap();

  let error = sync_engine.sync_with_local_engine(96, &remote).expect_err("a v1 remote checkpoint must remain authoritative");

  assert!(error.contains("missing from the remote engine"), "unexpected error: {error}");
  assert_eq!(local.head_hash().unwrap(), head_before);
  assert!(!file_exists(&local, "/remote-must-stay-hidden.txt"));
  assert_eq!(sync_engine.load_peer_sync_state(96).unwrap().unwrap().last_synced_root_hash, state.last_synced_root_hash);
}

#[test]
fn local_sync_retry_after_lost_checkpoint_acknowledgement_is_idempotent() {
  let (local, _local_temp) = create_temp_engine_for_tests();
  let (remote, _remote_temp) = create_temp_engine_for_tests();
  store_file(&remote, "/checkpoint-base.txt", b"base");
  let (sync_engine, _) = make_sync_engine(Arc::clone(&local));
  sync_engine.sync_with_local_engine(97, &remote).unwrap();
  let prior_state = sync_engine.load_peer_sync_state(97).unwrap().unwrap();
  store_file(&remote, "/checkpoint-retry.txt", b"applied before checkpoint failure");
  let first = sync_engine.sync_with_local_engine(97, &remote).unwrap();
  assert!(first.changes_applied);
  aeordb::engine::system_store::store_peer_sync_state(&local, &RequestContext::system(), 97, &prior_state).unwrap();

  let retry = sync_engine.sync_with_local_engine(97, &remote).unwrap();

  assert!(!retry.changes_applied);
  assert_eq!(retry.conflicts_detected, 0);
  assert_eq!(read_file(&local, "/checkpoint-base.txt"), b"base");
  assert_eq!(read_file(&local, "/checkpoint-retry.txt"), b"applied before checkpoint failure");
  let recovered = sync_engine.load_peer_sync_state(97).unwrap().unwrap();
  assert_eq!(recovered.last_synced_root_hash, Some(hex::encode(remote.head_hash().unwrap())));
  assert!(recovered.last_local_root_hash.is_some());
}

// ---------------------------------------------------------------------------
// Test: Bidirectional sync (sync A->B, then B->A)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_bidirectional_convergence() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  store_file(&engine_a, "/from_a.txt", b"A's data");
  store_file(&engine_b, "/from_b.txt", b"B's data");

  // Sync A <- B (A gets B's files)
  let (sync_engine_a, _pm_a) = make_sync_engine(Arc::clone(&engine_a));
  sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  // Sync B <- A (B gets A's files)
  let (sync_engine_b, _pm_b) = make_sync_engine(Arc::clone(&engine_b));
  sync_engine_b.sync_with_local_engine(1, &engine_a).unwrap();

  // Both engines should now have both files
  assert!(file_exists(&engine_a, "/from_a.txt"));
  assert!(file_exists(&engine_a, "/from_b.txt"));
  assert!(file_exists(&engine_b, "/from_a.txt"));
  assert!(file_exists(&engine_b, "/from_b.txt"));

  // Content should match
  assert_eq!(read_file(&engine_a, "/from_a.txt"), b"A's data");
  assert_eq!(read_file(&engine_a, "/from_b.txt"), b"B's data");
  assert_eq!(read_file(&engine_b, "/from_a.txt"), b"A's data");
  assert_eq!(read_file(&engine_b, "/from_b.txt"), b"B's data");
}

// ---------------------------------------------------------------------------
// Test: Sync with large file (multiple chunks)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_large_file_multiple_chunks() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Create a file larger than the default chunk size (256KB)
  let large_data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
  store_file(&engine_b, "/large.bin", &large_data);

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert!(file_exists(&engine_a, "/large.bin"));

  let synced_data = read_file(&engine_a, "/large.bin");
  assert_eq!(synced_data.len(), large_data.len());
  assert_eq!(synced_data, large_data);
}

// ---------------------------------------------------------------------------
// Test: Sync when one side deletes a file (remote delete applied locally)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_remote_deletion() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Both start with a file
  store_file(&engine_a, "/shared.txt", b"shared content");
  store_file(&engine_b, "/shared.txt", b"shared content");

  // Sync so they have a common base
  let (sync_engine_a, _pm_a) = make_sync_engine(Arc::clone(&engine_a));
  sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  // Now B deletes the file
  let context = RequestContext::system();
  let ops_b = DirectoryOps::new(&engine_b);
  ops_b.delete_file(&context, "/shared.txt").unwrap();

  // Sync A <- B: A should see the delete
  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
  assert!(result.changes_applied);
  assert!(!file_exists(&engine_a, "/shared.txt"), "File should be deleted after sync");
}

// ---------------------------------------------------------------------------
// Test: Sync when one side modifies and the other deletes (modify wins)
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_modify_vs_delete_conflict() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // Both start with a file
  store_file(&engine_a, "/conflict.txt", b"original");
  store_file(&engine_b, "/conflict.txt", b"original");

  // Sync to establish common base
  let (sync_engine_a, _pm_a) = make_sync_engine(Arc::clone(&engine_a));
  sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  // A modifies the file, B deletes it
  store_file(&engine_a, "/conflict.txt", b"modified by A");
  let context = RequestContext::system();
  let ops_b = DirectoryOps::new(&engine_b);
  ops_b.delete_file(&context, "/conflict.txt").unwrap();

  // Sync A <- B: modify should win (safety-first rule)
  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  // The file should still exist (modify wins over delete)
  assert!(file_exists(&engine_a, "/conflict.txt"), "Modified file should survive (modify wins over delete)");
  assert!(result.conflicts_detected > 0, "Should detect modify-delete conflict");
}

// ---------------------------------------------------------------------------
// Test: Remote HTTP sync returns connection error for unreachable peer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_remote_sync_returns_connection_error() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(engine);

  add_active_peer(&peer_manager, 10);

  let result = sync_engine.sync_with_peer(10).await;
  assert!(result.is_err());
  let err = result.unwrap_err();
  assert!(err.contains("Failed to contact peer"), "Should indicate connection failure, got: {}", err);
}

#[tokio::test]
async fn remote_peer_sync_rejects_forbidden_paths_before_requesting_chunks() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (address, chunks_requests, cancellation, server) = start_malicious_peer().await;
  add_active_peer_at(&peer_manager, 70, address);

  let result = sync_engine.sync_with_peer(70).await;

  cancellation.cancel();
  server.await.unwrap();
  let error = result.unwrap_err();
  assert!(error.contains("system_family_transfer_omitted"), "unexpected error: {error}");
  assert_eq!(chunks_requests.load(Ordering::SeqCst), 0, "forbidden peer diff triggered a chunk request");
  assert!(DirectoryOps::new(&engine).read_file_buffered("/.aeordb-system/config/secret.json").is_err());
}

#[tokio::test]
async fn remote_peer_sync_rejects_oversized_diff_before_buffering_or_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(engine);
  let (address, server) = start_oversized_diff_peer().await;
  add_active_peer_at(&peer_manager, 69, address);

  let error = sync_engine.sync_with_peer(69).await.expect_err("oversized peer diff must fail before body buffering");

  server.await.unwrap();
  assert!(error.contains("diff response exceeds"), "unexpected error: {error}");
  assert!(sync_engine.load_peer_sync_state(69).unwrap().is_none());
}

#[tokio::test]
async fn remote_peer_sync_rejects_oversized_error_body_before_buffering_or_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(engine);
  let (address, server) = start_oversized_error_diff_peer().await;
  add_active_peer_at(&peer_manager, 67, address);

  let error = sync_engine.sync_with_peer(67).await.expect_err("oversized peer error body must fail before buffering");

  server.await.unwrap();
  assert!(error.contains("error response exceeds 65536 bytes"), "unexpected error: {error}");
  assert!(sync_engine.load_peer_sync_state(67).unwrap().is_none());
}

#[tokio::test]
async fn remote_peer_sync_memory_pressure_releases_response_reservations_without_checkpointing() {
  use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};

  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let diff = remote_diff(&"af".repeat(32), vec![], vec![], vec![]);
  let (address, _, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [] })).await;
  add_active_peer_at(&peer_manager, 66, address);
  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let streaming_before = snapshot.owner(MemoryOwner::StreamingRead).unwrap().clone();
  let remaining = policy.ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  let _pressure = coordinator.reserve(MemoryOwner::Task, remaining.saturating_sub(64), AdmissionClass::Workload).unwrap();
  let head_before = engine.head_hash().unwrap();

  let error = sync_engine.sync_with_peer(66).await.expect_err("response admission must fail under memory pressure");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("memory") || error.contains("limit") || error.contains("pressure"), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), head_before);
  assert!(sync_engine.load_peer_sync_state(66).unwrap().is_none());
  let streaming_after = coordinator.snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
  assert_eq!(streaming_after.reserved_bytes, streaming_before.reserved_bytes);
  assert_eq!(streaming_after.active_reservations, streaming_before.active_reservations);
}

#[tokio::test]
async fn remote_peer_sync_rejects_oversized_chunk_response_before_buffering_or_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (file, _, chunk_hash) = remote_file_entry(&engine, "/remote/oversized-chunks.txt", Some("text/plain"), b"chunk bytes");
  let diff = remote_diff(&"a0".repeat(32), vec![file], vec![], vec![chunk_hash]);
  let (address, server) = start_oversized_chunks_peer(diff).await;
  add_active_peer_at(&peer_manager, 68, address);

  let error = sync_engine.sync_with_peer(68).await.expect_err("oversized peer chunks must fail before body buffering");

  server.await.unwrap();
  assert!(error.contains("chunks response exceeds"), "unexpected error: {error}");
  assert!(sync_engine.load_peer_sync_state(68).unwrap().is_none());
  assert!(DirectoryOps::new(&engine).read_file_buffered("/remote/oversized-chunks.txt").is_err());
}

#[tokio::test]
async fn remote_peer_sync_rejects_forged_file_identity_without_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (mut file, chunk, chunk_hash) = remote_file_entry(&engine, "/remote/forged.txt", Some("text/plain"), b"trusted bytes");
  file["hash"] = Value::String("f0".repeat(32));
  let root_hash = "a1".repeat(32);
  let diff = remote_diff(&root_hash, vec![file], vec![], vec![chunk_hash]);
  let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
  add_active_peer_at(&peer_manager, 71, address);

  let error = sync_engine.sync_with_peer(71).await.expect_err("forged FileRecord identity must fail closed");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("claimed hash"), "unexpected error: {error}");
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 0, "forged file identity triggered a chunk request");
  assert!(sync_engine.load_peer_sync_state(71).unwrap().is_none());
  assert!(DirectoryOps::new(&engine).read_file_buffered("/remote/forged.txt").is_err());
}

#[tokio::test]
async fn remote_peer_sync_rejects_incomplete_chunk_manifest_before_fetching() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (file, chunk, _) = remote_file_entry(&engine, "/remote/missing-manifest.txt", Some("text/plain"), b"manifest bytes");
  let diff = remote_diff(&"a2".repeat(32), vec![file], vec![], vec![]);
  let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
  add_active_peer_at(&peer_manager, 72, address);

  let error = sync_engine.sync_with_peer(72).await.expect_err("incomplete chunk manifest must fail closed");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("chunk manifest is incomplete"), "unexpected error: {error}");
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 0);
  assert!(sync_engine.load_peer_sync_state(72).unwrap().is_none());
  assert!(DirectoryOps::new(&engine).read_file_buffered("/remote/missing-manifest.txt").is_err());
}

#[tokio::test]
async fn remote_peer_sync_rejects_malformed_diff_shapes_before_fetching() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (file, chunk, chunk_hash) = remote_file_entry(&engine, "/remote/strict-diff.txt", Some("text/plain"), b"strict diff");

  let mut unknown_field = remote_diff(&"b0".repeat(32), vec![file.clone()], vec![], vec![chunk_hash.clone()]);
  unknown_field["unexpected"] = Value::Bool(true);
  let mut noncanonical_path = file.clone();
  noncanonical_path["path"] = Value::String("/remote/../strict-diff.txt".to_string());
  let mut non_hex_file_hash = file.clone();
  non_hex_file_hash["hash"] = Value::String("zz".repeat(32));
  let mut short_content_hash = file.clone();
  short_content_hash["content_hash"] = Value::String("ab".to_string());
  let mut control_path = file.clone();
  control_path["path"] = Value::String("/remote/control\npath.txt".to_string());
  let mut oversized_path = file.clone();
  oversized_path["path"] = Value::String(format!("/{}", "x".repeat(u16::MAX as usize)));
  let cases = vec![
    (
      78,
      remote_diff(&"b1".repeat(32), vec![file.clone()], vec![], vec![chunk_hash.clone(), "cc".repeat(32)]),
      "incomplete or contains unrelated hashes",
    ),
    (79, remote_diff(&"b2".repeat(32), vec![file.clone()], vec![], vec![chunk_hash.clone(), chunk_hash.clone()]), "duplicate hash"),
    (80, remote_diff("b3", vec![file.clone()], vec![], vec![chunk_hash.clone()]), "root_hash is 1 bytes"),
    (81, unknown_field, "unknown field"),
    (82, remote_diff(&"b4".repeat(32), vec![noncanonical_path], vec![], vec![chunk_hash.clone()]), "not a canonical leaf path"),
    (83, remote_diff(&"b5".repeat(32), vec![non_hex_file_hash], vec![], vec![chunk_hash.clone()]), "file hash is not valid hex"),
    (107, remote_diff(&"b7".repeat(32), vec![short_content_hash], vec![], vec![chunk_hash.clone()]), "content_hash is 1 bytes"),
    (108, remote_diff(&"b8".repeat(32), vec![control_path], vec![], vec![chunk_hash.clone()]), "contains control characters"),
    (109, remote_diff(&"b9".repeat(32), vec![oversized_path], vec![], vec![chunk_hash.clone()]), "exceeds 65535 bytes"),
  ];

  for (node_id, diff, expected_error) in cases {
    let head_before = engine.head_hash().unwrap();
    let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk.clone()] })).await;
    add_active_peer_at(&peer_manager, node_id, address);

    let error = sync_engine.sync_with_peer(node_id).await.expect_err("malformed remote diff must fail closed");

    stop_scripted_peer(cancellation, server).await;
    assert!(error.contains(expected_error), "case {node_id} returned unexpected error: {error}");
    assert_eq!(chunk_requests.load(Ordering::SeqCst), 0, "case {node_id} fetched chunks for a malformed diff");
    assert!(sync_engine.load_peer_sync_state(node_id).unwrap().is_none());
    assert_eq!(engine.head_hash().unwrap(), head_before);
  }
}

#[tokio::test]
async fn remote_peer_sync_rejects_invalid_symlink_identity_and_target_before_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/strict-link";
  let target = "/remote/target";
  let valid_hash = aeordb::engine::symlink_identity_hash(path, target, &engine.hash_algo()).unwrap();
  let cases = vec![
    (84, json!({ "path": path, "hash": "dd".repeat(32), "target": target }), "claimed hash does not match its identity"),
    (85, json!({ "path": path, "hash": hex::encode(valid_hash), "target": "remote/relative" }), "has an invalid target"),
  ];

  for (node_id, symlink, expected_error) in cases {
    let head_before = engine.head_hash().unwrap();
    let diff = remote_diff(&"b6".repeat(32), vec![], vec![symlink], vec![]);
    let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [] })).await;
    add_active_peer_at(&peer_manager, node_id, address);

    let error = sync_engine.sync_with_peer(node_id).await.expect_err("invalid remote symlink must fail closed");

    stop_scripted_peer(cancellation, server).await;
    assert!(error.contains(expected_error), "case {node_id} returned unexpected error: {error}");
    assert_eq!(chunk_requests.load(Ordering::SeqCst), 0);
    assert!(sync_engine.load_peer_sync_state(node_id).unwrap().is_none());
    assert_eq!(engine.head_hash().unwrap(), head_before);
  }
}

#[tokio::test]
async fn remote_peer_sync_rejects_omitted_chunk_without_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (file, _, chunk_hash) = remote_file_entry(&engine, "/remote/omitted-chunk.txt", Some("text/plain"), b"missing bytes");
  let diff = remote_diff(&"a3".repeat(32), vec![file], vec![], vec![chunk_hash]);
  let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [] })).await;
  add_active_peer_at(&peer_manager, 73, address);

  let error = sync_engine.sync_with_peer(73).await.expect_err("omitted requested chunk must fail closed");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("returned 0 chunks for 1 requested"), "unexpected error: {error}");
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 1);
  assert!(sync_engine.load_peer_sync_state(73).unwrap().is_none());
  assert!(DirectoryOps::new(&engine).read_file_buffered("/remote/omitted-chunk.txt").is_err());
}

#[tokio::test]
async fn remote_peer_sync_rejects_chunk_hash_mismatch_without_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (file, mut chunk, chunk_hash) = remote_file_entry(&engine, "/remote/mismatched-chunk.txt", Some("text/plain"), b"expected bytes");
  let forged_data = b"forged bytes";
  chunk["data"] = Value::String(base64::engine::general_purpose::STANDARD.encode(forged_data));
  chunk["size"] = Value::from(forged_data.len());
  let diff = remote_diff(&"a4".repeat(32), vec![file], vec![], vec![chunk_hash]);
  let (address, _, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
  add_active_peer_at(&peer_manager, 74, address);

  let error = sync_engine.sync_with_peer(74).await.expect_err("chunk content mismatch must fail closed");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("payload does not match its claimed hash"), "unexpected error: {error}");
  assert!(sync_engine.load_peer_sync_state(74).unwrap().is_none());
  assert!(DirectoryOps::new(&engine).read_file_buffered("/remote/mismatched-chunk.txt").is_err());
}

#[tokio::test]
async fn remote_peer_sync_rejects_wrong_whole_file_hash_without_publishing_or_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/wrong-content-hash.txt";
  let (mut file, chunk, chunk_hash) = remote_file_entry(&engine, path, Some("text/plain"), b"whole hash bytes");
  file["content_hash"] = Value::String("ff".repeat(engine.hash_algo().hash_length()));
  let diff = remote_diff(&"ca".repeat(32), vec![file], Vec::new(), vec![chunk_hash]);
  let (address, _, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
  add_active_peer_at(&peer_manager, 110, address);
  let head_before = engine.head_hash().unwrap();

  let error = sync_engine.sync_with_peer(110).await.expect_err("wrong whole-file hash must fail closed");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("whole-file hash does not match"), "unexpected error: {error}");
  assert_eq!(engine.head_hash().unwrap(), head_before);
  assert!(DirectoryOps::new(&engine).read_file_buffered(path).is_err());
  assert!(sync_engine.load_peer_sync_state(110).unwrap().is_none());
}

#[tokio::test]
async fn remote_peer_sync_rejects_every_malformed_chunk_shape_without_staging() {
  let cases = ["unrequested", "invalid-base64", "size-mismatch", "decoded-limit", "unknown-field", "short-hash"];

  for (case_index, case) in cases.into_iter().enumerate() {
    let (engine, _temp) = create_temp_engine_for_tests();
    let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
    let path = format!("/remote/{case}.txt");
    let (file, mut chunk, chunk_hash) = remote_file_entry(&engine, &path, Some("text/plain"), b"strict chunk bytes");
    let expected_error = match case {
      "unrequested" => {
        let (_, unrelated) = remote_chunk(&engine, b"unrequested bytes");
        chunk = unrelated;
        "contains unrequested hash"
      }
      "invalid-base64" => {
        chunk["data"] = Value::String("not-base64!".to_string());
        "has invalid base64"
      }
      "size-mismatch" => {
        chunk["size"] = Value::from(999_u64);
        "declares 999 bytes"
      }
      "decoded-limit" => {
        chunk["size"] = Value::from(64_u64 * 1024 * 1024 + 1);
        "exceeds 67108864 decoded bytes"
      }
      "unknown-field" => {
        chunk["unexpected"] = Value::Bool(true);
        "unknown field"
      }
      "short-hash" => {
        chunk["hash"] = Value::String("aa".to_string());
        "chunk response hash is 1 bytes"
      }
      _ => unreachable!(),
    };
    let diff = remote_diff(&"c0".repeat(32), vec![file], vec![], vec![chunk_hash.clone()]);
    let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
    let node_id = 100 + case_index as u64;
    add_active_peer_at(&peer_manager, node_id, address);

    let error = sync_engine.sync_with_peer(node_id).await.expect_err("malformed remote chunk must fail closed");

    stop_scripted_peer(cancellation, server).await;
    assert!(error.contains(expected_error), "case {case} returned unexpected error: {error}");
    assert_eq!(chunk_requests.load(Ordering::SeqCst), 1);
    assert!(!engine.has_entry(&hex::decode(&chunk_hash).unwrap()).unwrap(), "case {case} staged a rejected chunk");
    assert!(sync_engine.load_peer_sync_state(node_id).unwrap().is_none());
    assert!(DirectoryOps::new(&engine).read_file_buffered(&path).is_err());
  }
}

#[tokio::test]
async fn remote_peer_sync_rejects_duplicate_chunks_without_staging_the_valid_prefix() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (first_file, first_chunk, first_hash) = remote_file_entry(&engine, "/remote/duplicate-a.txt", Some("text/plain"), b"first chunk");
  let (second_file, _, second_hash) = remote_file_entry(&engine, "/remote/duplicate-b.txt", Some("text/plain"), b"second chunk");
  let diff = remote_diff(&"c1".repeat(32), vec![first_file, second_file], vec![], vec![first_hash.clone(), second_hash.clone()]);
  let chunks = json!({ "chunks": [first_chunk.clone(), first_chunk] });
  let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, chunks).await;
  add_active_peer_at(&peer_manager, 106, address);

  let error = sync_engine.sync_with_peer(106).await.expect_err("duplicate remote chunks must fail closed");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("repeats hash"), "unexpected error: {error}");
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 1);
  assert!(!engine.has_entry(&hex::decode(first_hash).unwrap()).unwrap());
  assert!(!engine.has_entry(&hex::decode(second_hash).unwrap()).unwrap());
  assert!(sync_engine.load_peer_sync_state(106).unwrap().is_none());
}

#[tokio::test]
async fn remote_peer_sync_validates_all_operations_before_publishing_any_path() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let (first_file, first_chunk, first_chunk_hash) = remote_file_entry(&engine, "/remote/first.txt", Some("text/plain"), b"first");
  let (mut invalid_file, second_chunk, second_chunk_hash) = remote_file_entry(&engine, "/remote/second.txt", Some("text/plain"), b"second");
  invalid_file["size"] = Value::from(99_u64);
  let diff = remote_diff(&"a5".repeat(32), vec![first_file, invalid_file], vec![], vec![first_chunk_hash, second_chunk_hash]);
  let chunks = json!({ "chunks": [first_chunk, second_chunk] });
  let (address, _, _, cancellation, server) = start_scripted_peer(diff, chunks).await;
  add_active_peer_at(&peer_manager, 75, address);

  let error = sync_engine.sync_with_peer(75).await.expect_err("late invalid operation must reject the entire namespace receipt");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("declares 99 bytes"), "unexpected error: {error}");
  assert!(sync_engine.load_peer_sync_state(75).unwrap().is_none());
  let ops = DirectoryOps::new(&engine);
  assert!(ops.read_file_buffered("/remote/first.txt").is_err(), "earlier valid operation became visible");
  assert!(ops.read_file_buffered("/remote/second.txt").is_err());
}

#[tokio::test]
async fn remote_peer_sync_rejects_duplicate_cross_type_path_before_fetching() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/collision";
  let (file, chunk, chunk_hash) = remote_file_entry(&engine, path, Some("text/plain"), b"collision");
  let symlink_hash = aeordb::engine::symlink_identity_hash(path, "/remote/target", &engine.hash_algo()).unwrap();
  let symlink = json!({ "path": path, "hash": hex::encode(symlink_hash), "target": "/remote/target" });
  let diff = remote_diff(&"a6".repeat(32), vec![file], vec![symlink], vec![chunk_hash]);
  let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
  add_active_peer_at(&peer_manager, 76, address);

  let error = sync_engine.sync_with_peer(76).await.expect_err("duplicate mutation paths must fail closed");

  stop_scripted_peer(cancellation, server).await;
  assert!(error.contains("repeats mutation path"), "unexpected error: {error}");
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 0);
  assert!(sync_engine.load_peer_sync_state(76).unwrap().is_none());
}

#[tokio::test]
async fn remote_peer_sync_applies_valid_mixed_receipt_before_checkpointing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/valid.txt";
  let target = "/remote/valid-target";
  let (file, chunk, chunk_hash) = remote_file_entry(&engine, path, Some("text/plain"), b"valid bytes");
  let symlink_path = "/remote/valid-link";
  let symlink_hash = aeordb::engine::symlink_identity_hash(symlink_path, target, &engine.hash_algo()).unwrap();
  let symlink = json!({
    "path": symlink_path,
    "hash": hex::encode(symlink_hash),
    "target": target,
    "created_at": 3_000,
    "updated_at": 4_000,
  });
  let root_hash = "a7".repeat(32);
  let diff = remote_diff(&root_hash, vec![file], vec![symlink], vec![chunk_hash.clone()]);
  let (address, chunk_requests, requested_hashes, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
  add_active_peer_at(&peer_manager, 77, address);

  let result = sync_engine.sync_with_peer(77).await.unwrap();

  stop_scripted_peer(cancellation, server).await;
  assert!(result.changes_applied);
  assert_eq!(result.operations_applied, 2);
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 1);
  assert_eq!(*requested_hashes.lock().unwrap(), vec![chunk_hash]);
  let ops = DirectoryOps::new(&engine);
  assert_eq!(ops.read_file_buffered(path).unwrap(), b"valid bytes");
  let file_record = ops.get_metadata(path).unwrap().unwrap();
  assert_eq!((file_record.created_at, file_record.updated_at), (1_000, 2_000));
  let symlink_record = ops.get_symlink(symlink_path).unwrap().unwrap();
  assert_eq!(symlink_record.target, target);
  assert_eq!((symlink_record.created_at, symlink_record.updated_at), (3_000, 4_000));
  assert_eq!(sync_engine.load_peer_sync_state(77).unwrap().unwrap().last_synced_root_hash, Some(root_hash));
}

#[tokio::test]
async fn remote_peer_sync_retry_after_checkpoint_loss_is_idempotent() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/checkpoint-retry.txt";
  let (file, chunk, chunk_hash) = remote_file_entry(&engine, path, Some("text/plain"), b"checkpoint retry bytes");
  let remote_root_hash = "d1".repeat(32);
  let diff = remote_diff(&remote_root_hash, vec![file], Vec::new(), vec![chunk_hash]);
  let (address, chunk_requests, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [chunk] })).await;
  add_active_peer_at(&peer_manager, 112, address);
  let operations = DirectoryOps::new(&engine);

  let first = sync_engine.sync_with_peer(112).await.unwrap();

  assert!(first.changes_applied);
  assert_eq!(operations.read_file_buffered(path).unwrap(), b"checkpoint retry bytes");
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 1);
  assert!(sync_engine.load_peer_sync_state(112).unwrap().is_some());
  operations.delete_file(&RequestContext::system(), "/.aeordb-system/sync-peers/112").unwrap();
  assert!(sync_engine.load_peer_sync_state(112).unwrap().is_none());

  let retry = sync_engine.sync_with_peer(112).await.unwrap();

  stop_scripted_peer(cancellation, server).await;
  assert!(!retry.changes_applied, "repeating an acknowledged namespace receipt must be a no-op");
  assert_eq!(retry.operations_applied, 0);
  assert_eq!(retry.conflicts_detected, 0);
  assert_eq!(operations.read_file_buffered(path).unwrap(), b"checkpoint retry bytes");
  assert_eq!(chunk_requests.load(Ordering::SeqCst), 1, "retry fetched a chunk already staged by the first receipt");
  let checkpoint = sync_engine.load_peer_sync_state(112).unwrap().unwrap();
  assert_eq!(checkpoint.last_synced_root_hash, Some(remote_root_hash));
  assert_eq!(hex::decode(checkpoint.last_local_root_hash.unwrap()).unwrap().len(), engine.hash_algo().hash_length());
}

#[tokio::test]
async fn remote_peer_sync_three_way_merge_retains_and_can_resolve_the_remote_loser() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/conflict.txt";
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), path, b"newer local bytes", Some("text/plain")).unwrap();
  let (remote_file, remote_chunk, remote_chunk_hash) = remote_file_entry(&engine, path, Some("text/plain"), b"older remote bytes");
  let remote_root_hash = "a8".repeat(32);
  let diff = remote_diff(&remote_root_hash, vec![remote_file], Vec::new(), vec![remote_chunk_hash]);
  let (address, _, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [remote_chunk] })).await;
  add_active_peer_at(&peer_manager, 78, address);

  let result = sync_engine.sync_with_peer(78).await.unwrap();

  stop_scripted_peer(cancellation, server).await;
  assert!(!result.changes_applied, "the newer local winner must not require a namespace replacement");
  assert_eq!(result.conflicts_detected, 1);
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(path).unwrap(), b"newer local bytes");
  let conflicts = list_conflicts(&engine).unwrap();
  assert_eq!(conflicts.len(), 1);
  assert_eq!(conflicts[0]["path"], path);

  aeordb::engine::conflict_store::resolve_conflict(&engine, &RequestContext::system(), path, "loser").unwrap();
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(path).unwrap(), b"older remote bytes");
  assert!(list_conflicts(&engine).unwrap().is_empty());
}

#[tokio::test]
async fn remote_peer_sync_retains_and_can_resolve_a_losing_symlink_version() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/conflict-link";
  let local_target = "/targets/newer-local";
  let remote_target = "/targets/older-remote";
  DirectoryOps::new(&engine).store_symlink(&RequestContext::system(), path, local_target).unwrap();
  let remote_hash = aeordb::engine::symlink_identity_hash(path, remote_target, &engine.hash_algo()).unwrap();
  let remote_symlink = json!({
    "path": path,
    "hash": hex::encode(remote_hash),
    "target": remote_target,
    "created_at": 1_000,
    "updated_at": 2_000,
  });
  let diff = remote_diff(&"cb".repeat(32), Vec::new(), vec![remote_symlink], Vec::new());
  let (address, _, _, cancellation, server) = start_scripted_peer(diff, json!({ "chunks": [] })).await;
  add_active_peer_at(&peer_manager, 111, address);

  let result = sync_engine.sync_with_peer(111).await.unwrap();

  stop_scripted_peer(cancellation, server).await;
  assert_eq!(result.conflicts_detected, 1);
  assert_eq!(DirectoryOps::new(&engine).get_symlink(path).unwrap().unwrap().target, local_target);
  run_gc(&engine, &RequestContext::system(), false).unwrap();
  aeordb::engine::conflict_store::resolve_conflict(&engine, &RequestContext::system(), path, "loser").unwrap();
  assert_eq!(DirectoryOps::new(&engine).get_symlink(path).unwrap().unwrap().target, remote_target);
}

#[tokio::test]
async fn remote_peer_incremental_sync_uses_distinct_remote_and_local_merge_bases() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let (sync_engine, peer_manager) = make_sync_engine(Arc::clone(&engine));
  let path = "/remote/incremental-conflict.txt";
  let (base_file, base_chunk, base_chunk_hash) = remote_file_entry(&engine, path, Some("text/plain"), b"remote base");
  let (remote_update, remote_update_chunk, remote_update_chunk_hash) =
    remote_file_entry(&engine, path, Some("text/plain"), b"older remote update");
  let first_remote_root = "b1".repeat(32);
  let second_remote_root = "b2".repeat(32);
  let first_diff = remote_diff(&first_remote_root, vec![base_file], Vec::new(), vec![base_chunk_hash]);
  let mut second_diff = remote_diff(&second_remote_root, vec![remote_update], Vec::new(), vec![remote_update_chunk_hash]);
  let modified = second_diff["changes"]["files_added"].as_array_mut().unwrap().pop().unwrap();
  second_diff["changes"]["files_modified"] = json!([modified]);
  let (address, diff_requests, cancellation, server) =
    start_sequenced_peer(vec![first_diff, second_diff], vec![base_chunk, remote_update_chunk]).await;
  add_active_peer_at(&peer_manager, 79, address);

  let first = sync_engine.sync_with_peer(79).await.unwrap();
  assert!(first.changes_applied);
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(path).unwrap(), b"remote base");
  let first_state = sync_engine.load_peer_sync_state(79).unwrap().unwrap();
  assert_eq!(first_state.last_synced_root_hash, Some(first_remote_root.clone()));
  assert!(first_state.last_local_root_hash.is_some());

  std::thread::sleep(std::time::Duration::from_millis(2));
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), path, b"newer local update", Some("text/plain")).unwrap();
  let second = sync_engine.sync_with_peer(79).await.unwrap();

  stop_scripted_peer(cancellation, server).await;
  assert_eq!(second.conflicts_detected, 1);
  assert!(!second.changes_applied);
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(path).unwrap(), b"newer local update");
  let requests = diff_requests.lock().unwrap();
  assert_eq!(requests.len(), 2);
  assert!(requests[0].get("since_root_hash").is_none());
  assert_eq!(requests[1]["since_root_hash"], first_remote_root);
  drop(requests);
  let second_state = sync_engine.load_peer_sync_state(79).unwrap().unwrap();
  assert_eq!(second_state.last_synced_root_hash, Some(second_remote_root));
  assert!(second_state.last_local_root_hash.is_some());

  aeordb::engine::conflict_store::resolve_conflict(&engine, &RequestContext::system(), path, "loser").unwrap();
  assert_eq!(DirectoryOps::new(&engine).read_file_buffered(path).unwrap(), b"older remote update");
}

// ---------------------------------------------------------------------------
// Test: Sync with multiple files and nested directories
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_nested_directories() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  // B has a complex directory structure
  store_file(&engine_b, "/docs/readme.txt", b"readme content");
  store_file(&engine_b, "/docs/api/endpoints.json", b"{}");
  store_file(&engine_b, "/src/main.rs", b"fn main() {}");
  store_file(&engine_b, "/config.toml", b"[settings]");

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert!(file_exists(&engine_a, "/docs/readme.txt"));
  assert!(file_exists(&engine_a, "/docs/api/endpoints.json"));
  assert!(file_exists(&engine_a, "/src/main.rs"));
  assert!(file_exists(&engine_a, "/config.toml"));

  assert_eq!(read_file(&engine_a, "/docs/readme.txt"), b"readme content");
  assert_eq!(read_file(&engine_a, "/src/main.rs"), b"fn main() {}");
}

// ---------------------------------------------------------------------------
// Test: Sync twice with no changes second time
// ---------------------------------------------------------------------------

#[test]
fn test_local_sync_no_changes_second_time() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  store_file(&engine_b, "/file.txt", b"data");

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));

  // First sync applies changes
  let result1 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
  assert!(result1.changes_applied);

  // Second sync: no new changes from B
  let result2 = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();
  assert!(!result2.changes_applied, "No changes on second sync");
  assert_eq!(result2.operations_applied, 0);
}

// ---------------------------------------------------------------------------
// Test: SyncCycleResult fields are populated correctly
// ---------------------------------------------------------------------------

#[test]
fn test_sync_cycle_result_fields() {
  let (engine_a, _temp_a) = create_temp_engine_for_tests();
  let (engine_b, _temp_b) = create_temp_engine_for_tests();

  store_file(&engine_b, "/x.txt", b"x");
  store_file(&engine_b, "/y.txt", b"y");

  let (sync_engine_a, _pm) = make_sync_engine(Arc::clone(&engine_a));
  let result = sync_engine_a.sync_with_local_engine(2, &engine_b).unwrap();

  assert!(result.changes_applied);
  assert_eq!(result.conflicts_detected, 0);
  // At least 2 operations (add x.txt and y.txt)
  assert!(result.operations_applied >= 2, "Expected at least 2 ops, got {}", result.operations_applied);
}
