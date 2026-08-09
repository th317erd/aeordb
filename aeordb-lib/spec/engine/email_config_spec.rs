use aeordb::engine::email_config::{EmailConfig, OAuthConfig, SmtpConfig, load_email_config, save_email_config};
use aeordb::engine::{DirectoryOps, EngineError, RequestContext};
use aeordb::server::create_temp_engine_for_tests;

fn smtp_config(password: String) -> EmailConfig {
  EmailConfig::Smtp(SmtpConfig {
    host: "smtp.example.com".to_string(),
    port: 587,
    username: "mailer@example.com".to_string(),
    password,
    from_address: "noreply@example.com".to_string(),
    from_name: "AeorDB".to_string(),
    tls: "starttls".to_string(),
  })
}

#[test]
fn email_config_round_trips_with_explicit_masking_result() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let config = smtp_config("secret".to_string());

  save_email_config(&engine, &config).unwrap();
  let loaded = load_email_config(&engine).unwrap().unwrap();
  let masked = loaded.masked().unwrap();

  assert_eq!(masked["password"], "--------");
  assert_eq!(masked["configured"], true);
  assert_eq!(masked["host"], "smtp.example.com");
}

#[test]
fn email_config_rejects_oversized_fields_and_empty_required_authority_without_writing() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let oversized = smtp_config("x".repeat(16 * 1024 + 1));
  assert!(matches!(save_email_config(&engine, &oversized), Err(EngineError::ResourceExhausted(_))));

  let empty_host = EmailConfig::Smtp(SmtpConfig {
    host: String::new(),
    port: 587,
    username: "mailer@example.com".to_string(),
    password: "secret".to_string(),
    from_address: "noreply@example.com".to_string(),
    from_name: "AeorDB".to_string(),
    tls: "starttls".to_string(),
  });
  assert!(matches!(save_email_config(&engine, &empty_host), Err(EngineError::InvalidInput(_))));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(load_email_config(&engine).unwrap().is_none());
}

#[test]
fn email_config_accepts_the_exact_field_limit() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let config = smtp_config("x".repeat(16 * 1024));

  save_email_config(&engine, &config).unwrap();

  let loaded = load_email_config(&engine).unwrap().unwrap();
  let EmailConfig::Smtp(loaded) = loaded else {
    panic!("expected SMTP email config");
  };
  assert_eq!(loaded.password, "x".repeat(16 * 1024));
}

#[test]
fn email_config_refuses_oversized_persisted_documents_before_deserialization() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let oversized = serde_json::to_vec(&smtp_config("x".repeat(128 * 1024))).unwrap();
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), "/.aeordb-system/email-config.json", &oversized, Some("application/json"))
    .unwrap();

  assert!(matches!(load_email_config(&engine), Err(EngineError::ResourceExhausted(_))));
}

#[test]
fn email_config_surfaces_malformed_and_invalid_persisted_authority() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);
  let context = RequestContext::system();

  ops.store_file_buffered(&context, "/.aeordb-system/email-config.json", b"{not-json", Some("application/json")).unwrap();
  assert!(matches!(load_email_config(&engine), Err(EngineError::JsonParseError(_))));

  let invalid = br#"{
    "provider": "smtp",
    "host": "",
    "port": 587,
    "username": "mailer@example.com",
    "password": "secret",
    "from_address": "noreply@example.com",
    "from_name": "AeorDB",
    "tls": "starttls"
  }"#;
  ops.store_file_buffered(&context, "/.aeordb-system/email-config.json", invalid, Some("application/json")).unwrap();
  assert!(matches!(load_email_config(&engine), Err(EngineError::CorruptEntry { .. })));
}

#[test]
fn oauth_email_config_bounds_optional_urls_and_required_identity() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let invalid = EmailConfig::OAuth(OAuthConfig {
    oauth_provider: "custom".to_string(),
    client_id: "client".to_string(),
    client_secret: "secret".to_string(),
    refresh_token: "refresh".to_string(),
    from_address: "noreply@example.com".to_string(),
    from_name: "AeorDB".to_string(),
    token_url: Some("x".repeat(16 * 1024 + 1)),
    send_url: Some("https://mail.example.com/send".to_string()),
  });

  assert!(matches!(save_email_config(&engine, &invalid), Err(EngineError::ResourceExhausted(_))));
}

#[test]
fn email_config_rejects_zero_ports_controls_and_empty_oauth_secrets() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let mut zero_port = smtp_config("secret".to_string());
  let EmailConfig::Smtp(config) = &mut zero_port else {
    unreachable!();
  };
  config.port = 0;
  assert!(matches!(save_email_config(&engine, &zero_port), Err(EngineError::InvalidInput(_))));

  let mut control_bearing = smtp_config("secret".to_string());
  let EmailConfig::Smtp(config) = &mut control_bearing else {
    unreachable!();
  };
  config.from_name = "AeorDB\nBcc: victim@example.com".to_string();
  assert!(matches!(save_email_config(&engine, &control_bearing), Err(EngineError::InvalidInput(_))));

  let empty_oauth_secret = EmailConfig::OAuth(OAuthConfig {
    oauth_provider: "custom".to_string(),
    client_id: "client".to_string(),
    client_secret: String::new(),
    refresh_token: "refresh".to_string(),
    from_address: "noreply@example.com".to_string(),
    from_name: "AeorDB".to_string(),
    token_url: Some("https://auth.example.com/token".to_string()),
    send_url: Some("https://mail.example.com/send".to_string()),
  });
  assert!(matches!(save_email_config(&engine, &empty_oauth_secret), Err(EngineError::InvalidInput(_))));

  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(load_email_config(&engine).unwrap().is_none());
}
