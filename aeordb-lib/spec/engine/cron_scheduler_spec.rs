use aeordb::engine::cron_scheduler::{
    cron_matches_now, validate_cron_expression, CronConfig, CronSchedule,
};

// ===========================================================================
// CronSchedule serde round-trip
// ===========================================================================

#[test]
fn cron_schedule_serde_roundtrip() {
    let schedule = CronSchedule {
        id: "s1".to_string(),
        task_type: "reindex".to_string(),
        schedule: "*/10 * * * *".to_string(),
        args: serde_json::json!({"path": "/data", "force": true}),
        enabled: true,
    };
    let json = serde_json::to_string(&schedule).unwrap();
    let restored: CronSchedule = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.id, "s1");
    assert_eq!(restored.task_type, "reindex");
    assert_eq!(restored.schedule, "*/10 * * * *");
    assert_eq!(restored.args, serde_json::json!({"path": "/data", "force": true}));
    assert!(restored.enabled);
}

#[test]
fn cron_schedule_enabled_defaults_to_true() {
    let json = r#"{
        "id": "x",
        "task_type": "gc",
        "schedule": "0 0 * * *",
        "args": null
    }"#;
    let schedule: CronSchedule = serde_json::from_str(json).unwrap();
    assert!(schedule.enabled, "enabled should default to true when omitted");
}

#[test]
fn cron_schedule_enabled_false_explicit() {
    let json = r#"{
        "id": "y",
        "task_type": "gc",
        "schedule": "0 0 * * *",
        "args": {},
        "enabled": false
    }"#;
    let schedule: CronSchedule = serde_json::from_str(json).unwrap();
    assert!(!schedule.enabled);
}

#[test]
fn cron_config_serde_roundtrip() {
    let config = CronConfig {
        schedules: vec![
            CronSchedule {
                id: "a".to_string(),
                task_type: "backup".to_string(),
                schedule: "0 2 * * 0".to_string(),
                args: serde_json::json!({}),
                enabled: true,
            },
            CronSchedule {
                id: "b".to_string(),
                task_type: "cleanup".to_string(),
                schedule: "30 4 1 * *".to_string(),
                args: serde_json::json!({"max_age_days": 30}),
                enabled: false,
            },
        ],
    };
    let json = serde_json::to_string_pretty(&config).unwrap();
    let restored: CronConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.schedules.len(), 2);
    assert_eq!(restored.schedules[0].id, "a");
    assert_eq!(restored.schedules[1].id, "b");
    assert!(restored.schedules[0].enabled);
    assert!(!restored.schedules[1].enabled);
}

#[test]
fn cron_config_empty_schedules() {
    let config = CronConfig {
        schedules: vec![],
    };
    let json = serde_json::to_string(&config).unwrap();
    let restored: CronConfig = serde_json::from_str(&json).unwrap();
    assert!(restored.schedules.is_empty());
}

// ===========================================================================
// validate_cron_expression -- DOW conversion edge cases
// ===========================================================================

#[test]
fn validate_unix_dow_zero_sunday() {
    // Unix DOW 0 = Sunday. The internal converter should map this to 1 for the crate.
    assert!(validate_cron_expression("0 0 * * 0").is_ok());
}

#[test]
fn validate_unix_dow_seven_sunday() {
    // Unix DOW 7 = also Sunday.
    assert!(validate_cron_expression("0 0 * * 7").is_ok());
}

#[test]
fn validate_dow_range() {
    // Monday through Friday: Unix DOW 1-5 -> crate 2-6
    assert!(validate_cron_expression("0 9 * * 1-5").is_ok());
}

#[test]
fn validate_dow_range_with_sunday() {
    // Sunday through Wednesday: Unix DOW 0-3
    assert!(validate_cron_expression("0 9 * * 0-3").is_ok());
}

#[test]
fn validate_dow_list() {
    // Monday, Wednesday, Friday: Unix DOW 1,3,5
    assert!(validate_cron_expression("0 9 * * 1,3,5").is_ok());
}

#[test]
fn validate_dow_list_with_zero() {
    // Sunday and Saturday: Unix DOW 0,6
    assert!(validate_cron_expression("0 0 * * 0,6").is_ok());
}

#[test]
fn validate_dow_step() {
    // Every other day starting from Sunday: */2
    assert!(validate_cron_expression("0 0 * * */2").is_ok());
}

#[test]
fn validate_dow_step_with_range() {
    // Every other day Mon-Fri: 1-5/2
    assert!(validate_cron_expression("0 0 * * 1-5/2").is_ok());
}

#[test]
fn validate_dow_wildcard() {
    assert!(validate_cron_expression("0 0 * * *").is_ok());
}

#[test]
fn validate_dow_question_mark() {
    assert!(validate_cron_expression("0 0 * * ?").is_ok());
}

#[test]
fn validate_named_days() {
    assert!(validate_cron_expression("0 0 * * MON").is_ok());
    assert!(validate_cron_expression("0 0 * * SUN").is_ok());
    assert!(validate_cron_expression("0 0 * * SAT").is_ok());
    assert!(validate_cron_expression("0 0 * * MON,WED,FRI").is_ok());
}

// ===========================================================================
// validate_cron_expression -- minute/hour/dom/month fields
// ===========================================================================

#[test]
fn validate_minute_step() {
    assert!(validate_cron_expression("*/5 * * * *").is_ok());
    assert!(validate_cron_expression("*/15 * * * *").is_ok());
}

#[test]
fn validate_minute_range() {
    assert!(validate_cron_expression("0-30 * * * *").is_ok());
}

#[test]
fn validate_minute_list() {
    assert!(validate_cron_expression("0,15,30,45 * * * *").is_ok());
}

#[test]
fn validate_hour_range() {
    assert!(validate_cron_expression("0 9-17 * * *").is_ok());
}

#[test]
fn validate_dom_specific() {
    assert!(validate_cron_expression("0 0 1 * *").is_ok());
    assert!(validate_cron_expression("0 0 15 * *").is_ok());
    assert!(validate_cron_expression("0 0 31 * *").is_ok());
}

#[test]
fn validate_month_range() {
    assert!(validate_cron_expression("0 0 1 1-6 *").is_ok());
}

// ===========================================================================
// validate_cron_expression -- invalid expressions
// ===========================================================================

#[test]
fn validate_empty_string() {
    assert!(validate_cron_expression("").is_err());
}

#[test]
fn validate_garbage() {
    assert!(validate_cron_expression("not a cron at all").is_err());
}

#[test]
fn validate_too_few_fields() {
    assert!(validate_cron_expression("0 3").is_err());
    assert!(validate_cron_expression("* *").is_err());
}

#[test]
fn validate_too_many_fields() {
    assert!(validate_cron_expression("0 0 0 0 0 0 0 0").is_err());
}

#[test]
fn validate_out_of_range_minute() {
    assert!(validate_cron_expression("99 0 * * *").is_err());
}

#[test]
fn validate_out_of_range_hour() {
    assert!(validate_cron_expression("0 25 * * *").is_err());
}

#[test]
fn validate_negative_values() {
    assert!(validate_cron_expression("-1 0 * * *").is_err());
}

// ===========================================================================
// cron_matches_now -- edge cases
// ===========================================================================

#[test]
fn matches_now_with_every_minute() {
    // "* * * * *" should always match.
    assert!(cron_matches_now("* * * * *"));
}

#[test]
fn matches_now_with_current_minute_and_hour() {
    let now = chrono::Utc::now();
    let expr = format!("{} {} * * *", now.format("%M"), now.format("%H"));
    assert!(cron_matches_now(&expr));
}

#[test]
fn does_not_match_different_hour() {
    let now = chrono::Utc::now();
    let wrong_hour = (now.format("%H").to_string().parse::<u32>().unwrap() + 12) % 24;
    let wrong_minute = (now.format("%M").to_string().parse::<u32>().unwrap() + 30) % 60;
    let expr = format!("{} {} * * *", wrong_minute, wrong_hour);
    assert!(!cron_matches_now(&expr));
}

#[test]
fn does_not_match_invalid_expression() {
    assert!(!cron_matches_now("garbage"));
    assert!(!cron_matches_now(""));
    assert!(!cron_matches_now("99 99 99 99 99"));
}

#[test]
fn does_not_match_wrong_day_of_month() {
    // Pick a day-of-month that is definitely not today.
    let today = chrono::Utc::now().format("%d").to_string().parse::<u32>().unwrap();
    let wrong_day = if today == 1 { 28 } else { 1 };
    let expr = format!("0 0 {} * *", wrong_day);
    assert!(!cron_matches_now(&expr));
}

// ===========================================================================
// CronSchedule field access
// ===========================================================================

#[test]
fn schedule_fields_accessible() {
    let s = CronSchedule {
        id: "test-id".to_string(),
        task_type: "my-task".to_string(),
        schedule: "0 0 * * *".to_string(),
        args: serde_json::json!({"key": "value"}),
        enabled: true,
    };
    assert_eq!(s.id, "test-id");
    assert_eq!(s.task_type, "my-task");
    assert_eq!(s.schedule, "0 0 * * *");
    assert_eq!(s.args["key"], "value");
    assert!(s.enabled);
}

#[test]
fn schedule_clone() {
    let s = CronSchedule {
        id: "c".to_string(),
        task_type: "gc".to_string(),
        schedule: "0 0 * * *".to_string(),
        args: serde_json::json!(null),
        enabled: false,
    };
    let cloned = s.clone();
    assert_eq!(cloned.id, s.id);
    assert_eq!(cloned.enabled, s.enabled);
}

// ===========================================================================
// Serde edge cases
// ===========================================================================

#[test]
fn schedule_with_null_args() {
    let json = r#"{
        "id": "n",
        "task_type": "noop",
        "schedule": "0 0 * * *",
        "args": null
    }"#;
    let s: CronSchedule = serde_json::from_str(json).unwrap();
    assert!(s.args.is_null());
}

#[test]
fn schedule_with_array_args() {
    let json = r#"{
        "id": "arr",
        "task_type": "batch",
        "schedule": "0 0 * * *",
        "args": [1, 2, 3]
    }"#;
    let s: CronSchedule = serde_json::from_str(json).unwrap();
    assert!(s.args.is_array());
    assert_eq!(s.args.as_array().unwrap().len(), 3);
}

#[test]
fn schedule_missing_required_fields_fails_deserialization() {
    // Missing "schedule" field.
    let json = r#"{
        "id": "missing",
        "task_type": "gc",
        "args": {}
    }"#;
    let result = serde_json::from_str::<CronSchedule>(json);
    assert!(result.is_err());
}

#[test]
fn config_with_extra_fields_ignored() {
    // Extra fields should be silently ignored (serde default behavior).
    let json = r#"{
        "schedules": [],
        "extra_field": "ignored"
    }"#;
    let config: CronConfig = serde_json::from_str(json).unwrap();
    assert!(config.schedules.is_empty());
}

// ===========================================================================
// DOW conversion: validate expressions that exercise all DOW paths
// ===========================================================================

#[test]
fn validate_all_single_unix_dow_values() {
    // Unix DOW 0..7 should all be valid after conversion.
    for dow in 0..=7 {
        let expr = format!("0 0 * * {}", dow);
        assert!(
            validate_cron_expression(&expr).is_ok(),
            "DOW {} should be valid, got error: {:?}",
            dow,
            validate_cron_expression(&expr)
        );
    }
}

#[test]
fn validate_dow_range_0_to_6() {
    assert!(validate_cron_expression("0 0 * * 0-6").is_ok());
}

#[test]
fn validate_dow_list_all_days() {
    assert!(validate_cron_expression("0 0 * * 0,1,2,3,4,5,6").is_ok());
}

#[test]
fn validate_dow_step_0_to_6_slash_2() {
    assert!(validate_cron_expression("0 0 * * 0-6/2").is_ok());
}

#[test]
fn validate_complex_expression() {
    // "At minute 0 and 30, every 2 hours, on Monday through Friday"
    assert!(validate_cron_expression("0,30 */2 * * 1-5").is_ok());
}

#[test]
fn validate_every_minute_every_day() {
    assert!(validate_cron_expression("* * * * *").is_ok());
}
