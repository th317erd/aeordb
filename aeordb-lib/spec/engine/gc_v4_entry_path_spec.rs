use aeordb::engine::{TaskOriginV1, TaskQueue};

#[test]
fn persisted_task_origin_defaults_legacy_records_and_preserves_explicit_sources() {
  let (engine, _temporary) = aeordb::server::create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine);

  let direct = queue.enqueue("gc", serde_json::json!({"dry_run": true})).unwrap();
  let scheduled = queue.enqueue_with_origin("gc", serde_json::json!({"dry_run": true}), TaskOriginV1::Scheduled).unwrap();
  let repair = queue.enqueue_gc_repair_follow_up().unwrap();

  assert_eq!(direct.origin, TaskOriginV1::Direct);
  assert_eq!(scheduled.origin, TaskOriginV1::Scheduled);
  assert_eq!(repair.origin, TaskOriginV1::RepairFollowUp);
  assert_eq!(repair.args["dry_run"], true);

  let mut legacy = serde_json::to_value(&direct).unwrap();
  legacy.as_object_mut().unwrap().remove("origin");
  let decoded: aeordb::engine::TaskRecord = serde_json::from_value(legacy).unwrap();
  assert_eq!(decoded.origin, TaskOriginV1::Direct);
}

#[test]
fn every_production_doorway_names_the_shared_gc_executor_and_its_invocation() {
  let library_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
  let cli_gc = std::fs::read_to_string(library_root.join("../../aeordb-cli/src/commands/gc.rs")).unwrap();
  let http_gc = std::fs::read_to_string(library_root.join("server/gc_routes.rs")).unwrap();
  let engine_gc = std::fs::read_to_string(library_root.join("engine/gc.rs")).unwrap();
  let task_worker = std::fs::read_to_string(library_root.join("engine/task_worker.rs")).unwrap();
  let cron = std::fs::read_to_string(library_root.join("engine/cron_scheduler.rs")).unwrap();

  for (source, invocation) in [
    (&cli_gc, "GcRunInvocationV1::Cli"),
    (&http_gc, "GcRunInvocationV1::Http"),
    (&task_worker, "GcRunInvocationV1::Task"),
    (&task_worker, "GcRunInvocationV1::Scheduled"),
    (&task_worker, "GcRunInvocationV1::RepairFollowUp"),
  ] {
    assert!(source.contains("execute_gc_run"), "doorway must call the shared executor");
    assert!(source.contains(invocation), "doorway is missing {invocation}");
  }
  assert!(cron.contains("TaskOriginV1::Scheduled"));
  assert!(cron.contains("enqueue_with_origin"));

  for source in [&cli_gc, &http_gc, &task_worker] {
    assert!(!source.contains("run_gc_internal"));
    assert!(!source.contains("gc_mark("));
    assert!(!source.contains("gc_sweep("));
  }

  assert!(!engine_gc.contains("fn run_gc_internal"));
  for phase_mapping in [
    "GcRunPhaseV1::Prepare => self.prepare()",
    "GcRunPhaseV1::Inventory => self.inventory()",
    "GcRunPhaseV1::Mark => self.mark()",
    "GcRunPhaseV1::MutationConvergence => self.converge_mutations()",
    "GcRunPhaseV1::Finalize => self.finalize()",
  ] {
    assert!(engine_gc.contains(phase_mapping), "legacy GC phase mapping is missing {phase_mapping}");
  }

  let mut compatibility_callers = Vec::new();
  collect_rust_sources(&library_root, &mut compatibility_callers);
  let compatibility_callers: Vec<String> = compatibility_callers
    .into_iter()
    .filter_map(|path| {
      let source = std::fs::read_to_string(&path).unwrap();
      source.contains("execute_legacy_v3_gc_run_v1").then(|| path.strip_prefix(&library_root).unwrap().to_string_lossy().replace('\\', "/"))
    })
    .collect();
  assert_eq!(compatibility_callers, vec!["engine/gc.rs", "engine/v4/gc_run.rs"]);
}

fn collect_rust_sources(directory: &std::path::Path, paths: &mut Vec<std::path::PathBuf>) {
  let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(directory).unwrap().map(|entry| entry.unwrap().path()).collect();
  entries.sort();
  for path in entries {
    if path.is_dir() {
      collect_rust_sources(&path, paths);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      paths.push(path);
    }
  }
}
