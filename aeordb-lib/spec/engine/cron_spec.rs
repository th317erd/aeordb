use aeordb::engine::cron_scheduler::{
  create_cron_schedule, cron_matches_now, delete_cron_schedule, load_cron_config, run_cron_tick, save_cron_config,
  seed_default_cron_if_missing, update_cron_schedule, validate_cron_expression, CronConfig, CronSchedule, CronScheduleUpdate,
};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::entry_type::EntryType;
use aeordb::engine::memory_coordinator::{AdmissionClass, CriticalMemoryPurpose, MemoryOwner};
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::task_queue::{TaskOriginV1, TaskQueue, TaskStatus};
use aeordb::server::create_temp_engine_for_tests;

// ---------------------------------------------------------------------------
// 1. validate_cron_expression — valid
// ---------------------------------------------------------------------------
#[test]
fn test_validate_cron_expression_valid() {
  // Standard cron: "at 03:00 every Sunday"
  // Standard cron: "at 03:00 every Sunday" (Unix DOW 0 = Sunday)
  assert!(validate_cron_expression("0 3 * * 0").is_ok());
  // Same with named day
  assert!(validate_cron_expression("0 3 * * SUN").is_ok());
  // Every minute
  assert!(validate_cron_expression("* * * * *").is_ok());
  // Every hour at minute 30
  assert!(validate_cron_expression("30 * * * *").is_ok());
  // 1st of every month at midnight
  assert!(validate_cron_expression("0 0 1 * *").is_ok());
}

// ---------------------------------------------------------------------------
// 2. validate_cron_expression — invalid
// ---------------------------------------------------------------------------
#[test]
fn test_validate_cron_expression_invalid() {
  let result = validate_cron_expression("not a cron");
  assert!(result.is_err());
  let msg = result.unwrap_err();
  assert!(!msg.is_empty(), "error message should be non-empty");

  // Too few fields
  assert!(validate_cron_expression("0 3 *").is_err());
  // Too many fields
  assert!(validate_cron_expression("0 3 * * * * * *").is_err());
  // Invalid range
  assert!(validate_cron_expression("99 3 * * *").is_err());
}

// ---------------------------------------------------------------------------
// 3. cron_matches_now — current minute should match
// ---------------------------------------------------------------------------
#[test]
fn test_cron_matches_now() {
  let now = chrono::Utc::now();
  let expr = format!("{} {} * * *", now.format("%M"), now.format("%H"));
  assert!(cron_matches_now(&expr), "expression '{}' should match the current minute", expr);
}

// ---------------------------------------------------------------------------
// 4. cron_does_not_match — expression for a different time
// ---------------------------------------------------------------------------
#[test]
fn test_cron_does_not_match() {
  let now = chrono::Utc::now();
  // Pick a minute that is definitely NOT now: (current_minute + 30) % 60
  let other_minute = (now.format("%M").to_string().parse::<u32>().unwrap() + 30) % 60;
  let other_hour = now.format("%H").to_string().parse::<u32>().unwrap();
  // Use a specific day-of-month that is also wrong to be extra safe
  // (29th of Feb is rare enough, but let's use a different hour too)
  let wrong_hour = (other_hour + 12) % 24;
  let expr = format!("{} {} * * *", other_minute, wrong_hour);
  assert!(!cron_matches_now(&expr), "expression '{}' should NOT match the current minute", expr);
}

// ---------------------------------------------------------------------------
// 5. load_cron_config from engine
// ---------------------------------------------------------------------------
#[test]
fn test_load_cron_config_from_engine() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  let config = CronConfig {
    schedules: vec![
      CronSchedule {
        id: "s1".to_string(),
        task_type: "reindex".to_string(),
        schedule: "0 3 * * 0".to_string(),
        args: serde_json::json!({"path": "/docs"}),
        enabled: true,
      },
      CronSchedule {
        id: "s2".to_string(),
        task_type: "gc".to_string(),
        schedule: "0 0 * * *".to_string(),
        args: serde_json::json!({}),
        enabled: false,
      },
    ],
  };

  let data = serde_json::to_vec_pretty(&config).unwrap();
  ops.store_file_buffered(&ctx, "/.aeordb-config/cron.json", &data, Some("application/json")).unwrap();

  let schedules = load_cron_config(&engine).unwrap();
  assert_eq!(schedules.len(), 2);
  assert_eq!(schedules[0].id, "s1");
  assert_eq!(schedules[0].task_type, "reindex");
  assert_eq!(schedules[0].schedule, "0 3 * * 0");
  assert!(schedules[0].enabled);
  assert_eq!(schedules[1].id, "s2");
  assert!(!schedules[1].enabled);
}

// ---------------------------------------------------------------------------
// 6. load_cron_config — missing file returns empty
// ---------------------------------------------------------------------------
#[test]
fn test_load_cron_config_missing_file() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let schedules = load_cron_config(&engine).unwrap();
  assert!(schedules.is_empty(), "should return empty vec when config file missing");
}

// ---------------------------------------------------------------------------
// 7. save and reload cron config — roundtrip
// ---------------------------------------------------------------------------
#[test]
fn test_save_and_reload_cron_config() {
  let (engine, _temp) = create_temp_engine_for_tests();

  let config = CronConfig {
    schedules: vec![CronSchedule {
      id: "rt1".to_string(),
      task_type: "backup".to_string(),
      schedule: "30 2 * * 1-5".to_string(),
      args: serde_json::json!({"target": "/backups"}),
      enabled: true,
    }],
  };

  save_cron_config(&engine, &config).unwrap();

  let reloaded = load_cron_config(&engine).unwrap();
  assert_eq!(reloaded.len(), 1);
  assert_eq!(reloaded[0].id, "rt1");
  assert_eq!(reloaded[0].task_type, "backup");
  assert_eq!(reloaded[0].schedule, "30 2 * * 1-5");
  assert_eq!(reloaded[0].args, serde_json::json!({"target": "/backups"}));
  assert!(reloaded[0].enabled);
}

// ---------------------------------------------------------------------------
// 8. disabled schedule field — deserializes correctly
// ---------------------------------------------------------------------------
#[test]
fn test_disabled_schedule_field() {
  // Explicit enabled: false
  let json = r#"{
        "id": "d1",
        "task_type": "gc",
        "schedule": "0 4 * * *",
        "args": {},
        "enabled": false
    }"#;
  let schedule: CronSchedule = serde_json::from_str(json).unwrap();
  assert!(!schedule.enabled);

  // Missing enabled field — should default to true
  let json_no_enabled = r#"{
        "id": "d2",
        "task_type": "gc",
        "schedule": "0 4 * * *",
        "args": {}
    }"#;
  let schedule2: CronSchedule = serde_json::from_str(json_no_enabled).unwrap();
  assert!(schedule2.enabled, "enabled should default to true when missing");
}

// ---------------------------------------------------------------------------
// 9. load_cron_config — malformed JSON fails closed
// ---------------------------------------------------------------------------
#[test]
fn test_load_cron_config_malformed_json() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();

  // Store garbage at the config path
  ops.store_file_buffered(&ctx, "/.aeordb-config/cron.json", b"not json at all{{{", Some("application/json")).unwrap();

  let error = load_cron_config(&engine).expect_err("malformed cron authority must not become an empty schedule");
  assert!(error.to_string().contains("cron config"), "unexpected error: {error}");
}

// ---------------------------------------------------------------------------
// 10. cron_matches_now — invalid expression returns false, not panic
// ---------------------------------------------------------------------------
#[test]
fn test_cron_matches_now_invalid_expression() {
  assert!(!cron_matches_now("garbage"));
  assert!(!cron_matches_now(""));
  assert!(!cron_matches_now("99 99 99 99 99"));
}

// ---------------------------------------------------------------------------
// 11. save_cron_config — empty schedules
// ---------------------------------------------------------------------------
#[test]
fn test_save_empty_config() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let config = CronConfig { schedules: vec![] };
  save_cron_config(&engine, &config).unwrap();

  let reloaded = load_cron_config(&engine).unwrap();
  assert!(reloaded.is_empty());
}

// ---------------------------------------------------------------------------
// 12. dedup — enqueue does not create duplicate pending tasks
// ---------------------------------------------------------------------------
#[test]
fn test_cron_dedup_logic() {
  // This test verifies the dedup logic used by the scheduler:
  // if a pending task with the same type+args exists, skip enqueue.
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());

  let args = serde_json::json!({"path": "/docs"});

  // Enqueue first task
  let r1 = queue.enqueue("reindex", args.clone()).unwrap();
  assert_eq!(r1.status, TaskStatus::Pending);

  // Simulate dedup check (same logic as spawn_cron_scheduler)
  let tasks = queue.list_tasks().unwrap();
  let has_pending = tasks
    .iter()
    .any(|t| (t.status == TaskStatus::Pending || t.status == TaskStatus::Running) && t.task_type == "reindex" && t.args == args);
  assert!(has_pending, "should detect existing pending task");

  // If the task is completed, dedup should NOT block
  queue.update_status(&r1.id, TaskStatus::Completed, None).unwrap();

  let tasks2 = queue.list_tasks().unwrap();
  let has_pending2 = tasks2
    .iter()
    .any(|t| (t.status == TaskStatus::Pending || t.status == TaskStatus::Running) && t.task_type == "reindex" && t.args == args);
  assert!(!has_pending2, "completed task should not block new enqueue");
}

// ---------------------------------------------------------------------------
// 13. save_cron_config then overwrite — latest config wins
// ---------------------------------------------------------------------------
#[test]
fn test_save_cron_config_overwrite() {
  let (engine, _temp) = create_temp_engine_for_tests();

  let config1 = CronConfig {
    schedules: vec![CronSchedule {
      id: "v1".to_string(),
      task_type: "gc".to_string(),
      schedule: "0 0 * * *".to_string(),
      args: serde_json::json!({}),
      enabled: true,
    }],
  };
  save_cron_config(&engine, &config1).unwrap();

  let config2 = CronConfig {
    schedules: vec![
      CronSchedule {
        id: "v2a".to_string(),
        task_type: "backup".to_string(),
        schedule: "0 1 * * *".to_string(),
        args: serde_json::json!({"dest": "/b"}),
        enabled: true,
      },
      CronSchedule {
        id: "v2b".to_string(),
        task_type: "reindex".to_string(),
        schedule: "30 3 * * *".to_string(),
        args: serde_json::json!({"path": "/"}),
        enabled: false,
      },
    ],
  };
  save_cron_config(&engine, &config2).unwrap();

  let reloaded = load_cron_config(&engine).unwrap();
  assert_eq!(reloaded.len(), 2);
  assert_eq!(reloaded[0].id, "v2a");
  assert_eq!(reloaded[1].id, "v2b");
}

// ---------------------------------------------------------------------------
// 14. validate_cron_expression — edge cases
// ---------------------------------------------------------------------------
#[test]
fn test_validate_cron_expression_edge_cases() {
  // Empty string
  assert!(validate_cron_expression("").is_err());
  // Step values
  assert!(validate_cron_expression("*/5 * * * *").is_ok());
  // Ranges
  assert!(validate_cron_expression("0 9-17 * * 1-5").is_ok());
  // Lists
  assert!(validate_cron_expression("0,15,30,45 * * * *").is_ok());
}

#[test]
fn invalid_complete_config_is_refused_before_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let before = engine.durability_snapshot().unwrap().next_sequence;
  let config = CronConfig {
    schedules: vec![CronSchedule {
      id: "invalid".to_string(),
      task_type: "gc".to_string(),
      schedule: "not a cron expression".to_string(),
      args: serde_json::json!({}),
      enabled: true,
    }],
  };

  let error = save_cron_config(&engine, &config).expect_err("invalid complete config must fail");

  assert!(error.to_string().contains("invalid cron expression"), "unexpected error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert!(load_cron_config(&engine).unwrap().is_empty());
}

#[test]
fn concurrent_cron_mutations_retain_every_schedule_with_one_acknowledgement_each() {
  const WRITERS: usize = 12;
  let (engine, _temp) = create_temp_engine_for_tests();
  let before = engine.durability_snapshot().unwrap().next_sequence;
  let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));

  std::thread::scope(|scope| {
    for index in 0..WRITERS {
      let engine = engine.clone();
      let barrier = barrier.clone();
      scope.spawn(move || {
        barrier.wait();
        create_cron_schedule(
          &engine,
          CronSchedule {
            id: format!("concurrent-{index:02}"),
            task_type: "gc".to_string(),
            schedule: "0 3 * * *".to_string(),
            args: serde_json::json!({"writer": index}),
            enabled: true,
          },
        )
        .unwrap();
      });
    }
  });

  let mut schedules = load_cron_config(&engine).unwrap();
  schedules.sort_by(|left, right| left.id.cmp(&right.id));
  assert_eq!(schedules.len(), WRITERS);
  for (index, schedule) in schedules.iter().enumerate() {
    assert_eq!(schedule.id, format!("concurrent-{index:02}"));
  }
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before + WRITERS as u64);
}

#[test]
fn typed_cron_crud_uses_one_acknowledgement_per_operation() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let before = engine.durability_snapshot().unwrap().next_sequence;

  let created = create_cron_schedule(
    &engine,
    CronSchedule {
      id: "typed-crud".to_string(),
      task_type: "gc".to_string(),
      schedule: "0 3 * * *".to_string(),
      args: serde_json::json!({}),
      enabled: true,
    },
  )
  .unwrap();
  assert_eq!(created.id, "typed-crud");

  let updated = update_cron_schedule(
    &engine,
    "typed-crud",
    CronScheduleUpdate { enabled: Some(false), schedule: Some("15 3 * * *".to_string()), task_type: None, args: None },
  )
  .unwrap();
  assert!(!updated.enabled);
  assert_eq!(updated.schedule, "15 3 * * *");

  delete_cron_schedule(&engine, "typed-crud").unwrap();
  assert!(load_cron_config(&engine).unwrap().is_empty());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before + 3);
}

#[test]
fn cron_tick_enqueues_due_work_once_and_reports_deduplication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  save_cron_config(
    &engine,
    &CronConfig {
      schedules: vec![
        CronSchedule {
          id: "due-a".to_string(),
          task_type: "gc".to_string(),
          schedule: "* * * * *".to_string(),
          args: serde_json::json!({"dry_run": true}),
          enabled: true,
        },
        CronSchedule {
          id: "due-b".to_string(),
          task_type: "gc".to_string(),
          schedule: "* * * * *".to_string(),
          args: serde_json::json!({"dry_run": true}),
          enabled: true,
        },
      ],
    },
  )
  .unwrap();

  let first = run_cron_tick(&queue, &engine).unwrap();
  let second = run_cron_tick(&queue, &engine).unwrap();

  assert_eq!(first.tasks_enqueued, 1);
  assert_eq!(first.tasks_deduplicated, 1);
  assert_eq!(second.tasks_enqueued, 0);
  assert_eq!(second.tasks_deduplicated, 2);
  let tasks = queue.list_tasks().unwrap();
  assert_eq!(tasks.len(), 1);
  assert_eq!(tasks[0].origin, TaskOriginV1::Scheduled);
}

#[test]
fn cron_tick_propagates_task_registry_corruption_without_enqueueing() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  save_cron_config(
    &engine,
    &CronConfig {
      schedules: vec![CronSchedule {
        id: "due".to_string(),
        task_type: "gc".to_string(),
        schedule: "* * * * *".to_string(),
        args: serde_json::json!({}),
        enabled: true,
      }],
    },
  )
  .unwrap();
  let registry_key = blake3::hash(b"::aeordb:task:_registry").as_bytes().to_vec();
  engine.store_entry(EntryType::FileRecord, &registry_key, b"not-json").unwrap();

  let error = run_cron_tick(&queue, &engine).expect_err("task registry corruption must fail the tick");

  assert!(error.to_string().contains("task registry is malformed"), "unexpected error: {error}");
}

#[test]
fn cron_tick_propagates_enqueue_pressure_and_retries_without_phantom_work() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let queue = TaskQueue::new(engine.clone());
  save_cron_config(
    &engine,
    &CronConfig {
      schedules: vec![CronSchedule {
        id: "due-under-pressure".to_string(),
        task_type: "gc".to_string(),
        schedule: "* * * * *".to_string(),
        args: serde_json::json!({"dry_run": true}),
        enabled: true,
      }],
    },
  )
  .unwrap();
  let before = engine.durability_snapshot().unwrap().next_sequence;
  let memory = engine.memory_coordinator();
  let snapshot = memory.snapshot().unwrap();
  let remaining = snapshot.policy.unwrap().emergency_reserve_bytes - snapshot.critical_reserved_bytes;
  let pressure = memory
    .reserve(
      MemoryOwner::DurabilityWaiters,
      remaining.saturating_sub(8 * 1024),
      AdmissionClass::Critical(CriticalMemoryPurpose::DurableWrite),
    )
    .unwrap();

  let error = run_cron_tick(&queue, &engine).expect_err("enqueue pressure must fail the scheduler tick");

  assert!(error.to_string().contains("emergency reserve"), "unexpected error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert!(queue.list_tasks().unwrap().is_empty());

  drop(pressure);
  let retry = run_cron_tick(&queue, &engine).unwrap();
  assert_eq!(retry.tasks_enqueued, 1);
  assert_eq!(retry.tasks_deduplicated, 0);
  assert_eq!(queue.list_tasks().unwrap().len(), 1);
}

#[test]
fn duplicate_schedule_ids_are_rejected_without_replacing_persisted_authority() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  let duplicate = serde_json::to_vec_pretty(&CronConfig {
    schedules: vec![
      CronSchedule {
        id: "duplicate".to_string(),
        task_type: "gc".to_string(),
        schedule: "0 3 * * *".to_string(),
        args: serde_json::json!({}),
        enabled: true,
      },
      CronSchedule {
        id: "duplicate".to_string(),
        task_type: "cleanup".to_string(),
        schedule: "0 * * * *".to_string(),
        args: serde_json::json!({}),
        enabled: false,
      },
    ],
  })
  .unwrap();
  ops.store_file_buffered(&RequestContext::system(), "/.aeordb-config/cron.json", &duplicate, Some("application/json")).unwrap();
  let before = engine.durability_snapshot().unwrap().next_sequence;

  let load_error = load_cron_config(&engine).expect_err("duplicate persisted ids must fail closed");
  let mutate_error = create_cron_schedule(
    &engine,
    CronSchedule {
      id: "probe".to_string(),
      task_type: "gc".to_string(),
      schedule: "0 3 * * *".to_string(),
      args: serde_json::json!({}),
      enabled: true,
    },
  )
  .expect_err("duplicate persisted ids must not be replaced");

  assert!(load_error.to_string().contains("duplicate schedule id"), "unexpected error: {load_error}");
  assert!(mutate_error.to_string().contains("duplicate schedule id"), "unexpected error: {mutate_error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert_eq!(ops.read_file_buffered("/.aeordb-config/cron.json").unwrap(), duplicate);
}

#[test]
fn oversized_cron_authority_is_rejected_before_buffering_or_replacement() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  let oversized = vec![b' '; 1024 * 1024 + 1];
  ops.store_file_buffered(&RequestContext::system(), "/.aeordb-config/cron.json", &oversized, Some("application/json")).unwrap();
  let before = engine.durability_snapshot().unwrap().next_sequence;

  let load_error = load_cron_config(&engine).expect_err("oversized stored authority must fail before JSON parsing");
  let mutate_error = create_cron_schedule(
    &engine,
    CronSchedule {
      id: "probe".to_string(),
      task_type: "gc".to_string(),
      schedule: "0 3 * * *".to_string(),
      args: serde_json::json!({}),
      enabled: true,
    },
  )
  .expect_err("oversized stored authority must not be replaced");

  assert!(load_error.to_string().contains("1048576-byte limit"), "unexpected error: {load_error}");
  assert!(mutate_error.to_string().contains("1048576-byte limit"), "unexpected error: {mutate_error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert_eq!(ops.read_file_buffered("/.aeordb-config/cron.json").unwrap(), oversized);
}

#[test]
fn oversized_cron_replacement_is_refused_before_namespace_publication() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let before = engine.durability_snapshot().unwrap().next_sequence;
  let payload = "x".repeat(1024 * 1024);

  let error = create_cron_schedule(
    &engine,
    CronSchedule {
      id: "oversized".to_string(),
      task_type: "gc".to_string(),
      schedule: "0 3 * * *".to_string(),
      args: serde_json::json!({"payload": payload}),
      enabled: true,
    },
  )
  .expect_err("oversized proposed authority must fail");

  assert!(error.to_string().contains("exceeding the 1048576-byte limit"), "unexpected error: {error}");
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert!(load_cron_config(&engine).unwrap().is_empty());
}

#[test]
fn default_cron_seed_is_one_acknowledged_write_then_a_true_noop() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let before = engine.durability_snapshot().unwrap().next_sequence;

  assert!(seed_default_cron_if_missing(&engine).unwrap());
  let after_seed = engine.durability_snapshot().unwrap().next_sequence;
  assert_eq!(after_seed, before + 1);
  let schedules = load_cron_config(&engine).unwrap();
  assert_eq!(schedules.len(), 2);
  assert!(schedules.iter().any(|schedule| schedule.id == "default-cleanup"));
  assert!(schedules.iter().any(|schedule| schedule.id == "default-gc"));

  assert!(!seed_default_cron_if_missing(&engine).unwrap());
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, after_seed);
}

#[test]
fn default_cron_seed_preserves_existing_malformed_authority_for_diagnostics() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  let malformed = b"existing malformed cron authority";
  ops.store_file_buffered(&RequestContext::system(), "/.aeordb-config/cron.json", malformed, Some("application/json")).unwrap();
  let before = engine.durability_snapshot().unwrap().next_sequence;

  assert!(!seed_default_cron_if_missing(&engine).unwrap());

  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, before);
  assert_eq!(ops.read_file_buffered("/.aeordb-config/cron.json").unwrap(), malformed);
  assert!(load_cron_config(&engine).is_err());
}
