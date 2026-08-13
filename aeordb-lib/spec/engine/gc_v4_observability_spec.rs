use std::sync::Arc;

use aeordb::engine::configuration_observability::ConfigurationVisibility;
use aeordb::engine::gc::{execute_gc_run, GcExecutionRequestV1};
use aeordb::engine::task_worker::process_next_task;
use aeordb::engine::v4::gc_run::{GcRunInvocationV1, GcRunStateV1};
use aeordb::engine::{EventBus, RequestContext, StorageEngine, TaskQueue, EVENT_GC_STATUS};
use aeordb::plugins::PluginManager;
use tokio_util::sync::CancellationToken;

fn create_engine(name: &str) -> (StorageEngine, tempfile::TempDir) {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join(format!("{name}.aeordb"));
  (StorageEngine::create(database.to_str().unwrap()).unwrap(), temporary)
}

#[test]
fn every_shared_executor_run_updates_one_engine_owned_terminal_status() {
  let (engine, _temporary) = create_engine("gc-observability-engine");
  assert!(engine.gc_run_status().is_none());

  let execution = execute_gc_run(
    &engine,
    &RequestContext::system(),
    GcExecutionRequestV1::new(GcRunInvocationV1::Embedded, true, CancellationToken::new()),
  )
  .unwrap();

  let projected = engine.gc_run_status().expect("the shared executor must publish status even when the caller observer is a no-op");
  assert_eq!(projected.status, execution.status);
  assert_eq!(projected.status.state, GcRunStateV1::Complete);
  assert_eq!(projected.task_id, None);
}

#[test]
fn root_runtime_health_includes_gc_status_while_redacted_runtime_omits_it() {
  let (engine, _temporary) = create_engine("gc-observability-visibility");
  execute_gc_run(&engine, &RequestContext::system(), GcExecutionRequestV1::new(GcRunInvocationV1::Cli, true, CancellationToken::new()))
    .unwrap();

  let root = engine.runtime_observability_snapshot(ConfigurationVisibility::Root).unwrap();
  let redacted = engine.runtime_observability_snapshot(ConfigurationVisibility::Redacted).unwrap();
  assert_eq!(root.gc.as_ref().map(|snapshot| snapshot.status.state), Some(GcRunStateV1::Complete));
  assert!(redacted.gc.is_none(), "non-root runtime observability must not disclose GC status");
  assert!(serde_json::to_value(redacted).unwrap().get("gc").is_none(), "omitted status must not serialize as null");
}

#[test]
fn task_binding_is_validated_and_projects_the_same_status_by_task_id() {
  let (engine, _temporary) = create_engine("gc-observability-task");
  let task_id = uuid::Uuid::new_v4().to_string();
  let request = GcExecutionRequestV1::new(GcRunInvocationV1::Task, true, CancellationToken::new()).with_task_id(task_id.clone()).unwrap();

  let execution = execute_gc_run(&engine, &RequestContext::system(), request).unwrap();
  let projected = engine.gc_run_status_for_task(&task_id).expect("the active task binding must resolve its exact GC run");
  assert_eq!(projected.status, execution.status);
  assert_eq!(projected.task_id.as_deref(), Some(task_id.as_str()));
  assert!(engine.gc_run_status_for_task(&uuid::Uuid::new_v4().to_string()).is_none());

  let malformed =
    GcExecutionRequestV1::new(GcRunInvocationV1::Task, true, CancellationToken::new()).with_task_id("not-a-task-id".to_string());
  assert!(malformed.is_err(), "malformed task identities must fail before run publication");
}

#[test]
fn status_sse_uses_the_exact_engine_projection_and_terminal_payload() {
  let (engine, _temporary) = create_engine("gc-observability-events");
  let event_bus = Arc::new(EventBus::new());
  let mut receiver = event_bus.subscribe();
  let execution = execute_gc_run(
    &engine,
    &RequestContext::with_bus(event_bus),
    GcExecutionRequestV1::new(GcRunInvocationV1::Http, true, CancellationToken::new()),
  )
  .unwrap();

  let mut terminal = None;
  while let Ok(event) = receiver.try_recv() {
    if event.event_type == EVENT_GC_STATUS && event.payload["state"] == "complete" {
      terminal = Some(event);
    }
  }
  let terminal = terminal.expect("the event bus must receive the terminal GC status");
  assert_eq!(terminal.payload["run_id"], execution.status.run_id.to_string());
  assert_eq!(terminal.payload, serde_json::to_value(engine.gc_run_status().unwrap()).unwrap());
}

#[test]
fn queued_task_gc_projects_the_same_task_bound_status_through_its_event_bus() {
  let (engine, _temporary) = create_engine("gc-observability-task-events");
  let engine = Arc::new(engine);
  let queue = TaskQueue::new(engine.clone());
  let plugin_manager = PluginManager::new(engine.clone());
  let event_bus = EventBus::new();
  let mut receiver = event_bus.subscribe();
  let task = queue.enqueue("gc", serde_json::json!({"dry_run": true})).unwrap();

  assert!(process_next_task(&queue, &engine, &plugin_manager, &event_bus).unwrap());

  let projected = engine.gc_run_status_for_task(&task.id).expect("task worker must retain the task-bound GC status");
  assert_eq!(projected.task_id.as_deref(), Some(task.id.as_str()));
  assert_eq!(projected.status.state, GcRunStateV1::Complete);
  let mut terminal = None;
  while let Ok(event) = receiver.try_recv() {
    if event.event_type == EVENT_GC_STATUS && event.payload["state"] == "complete" {
      terminal = Some(event);
    }
  }
  let terminal = terminal.expect("task worker must publish terminal GC status through its EventBus");
  assert_eq!(terminal.payload, serde_json::to_value(projected).unwrap());
}

#[test]
fn production_doorways_cannot_opt_out_by_constructing_noop_status_sinks() {
  for (name, source) in [
    ("CLI", include_str!("../../../aeordb-cli/src/commands/gc.rs")),
    ("HTTP", include_str!("../../src/server/gc_routes.rs")),
    ("task", include_str!("../../src/engine/task_worker.rs")),
  ] {
    assert!(!source.contains("NoopGcRunProgressSinkV1"), "{name} doorway still constructs an optional no-op GC status sink");
  }
}

#[test]
fn one_bounded_registry_is_the_only_production_gc_status_publisher() {
  let library_source_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
  let registry_source = std::fs::read_to_string(library_source_root.join("engine/gc_run_status.rs")).unwrap();
  assert!(registry_source.contains("current: ArcSwapOption<RetainedGcRunStatusV1>"));
  assert!(!registry_source.contains("Vec<"), "the current/latest status authority must not retain run history");

  let mut rust_source_paths = Vec::new();
  collect_rust_source_paths(&library_source_root, &mut rust_source_paths);
  let event_publishers: Vec<String> = rust_source_paths
    .into_iter()
    .filter_map(|path| {
      let source = std::fs::read_to_string(&path).unwrap();
      source
        .contains("EngineEvent::new(EVENT_GC_STATUS")
        .then(|| path.strip_prefix(&library_source_root).unwrap().to_string_lossy().replace('\\', "/"))
    })
    .collect();
  assert_eq!(event_publishers, vec!["engine/gc_run_status.rs"]);

  for adapter in ["server/gc_routes.rs", "server/portal_routes.rs", "server/sse_routes.rs", "server/task_routes.rs", "metrics/mod.rs"] {
    let source = std::fs::read_to_string(library_source_root.join(adapter)).unwrap();
    assert!(!source.contains("GcRunStatusV1 {"), "{adapter} must project the engine snapshot instead of constructing GC state");
  }
}

fn collect_rust_source_paths(directory: &std::path::Path, paths: &mut Vec<std::path::PathBuf>) {
  let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(directory).unwrap().map(|entry| entry.unwrap().path()).collect();
  entries.sort();
  for path in entries {
    if path.is_dir() {
      collect_rust_source_paths(&path, paths);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      paths.push(path);
    }
  }
}
