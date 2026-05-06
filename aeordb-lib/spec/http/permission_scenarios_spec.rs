use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::system_store;
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::engine::{EventBus, StorageEngine};
use aeordb::plugins::PluginManager;
use aeordb::auth::FileAuthProvider;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

// ===========================================================================
// Shared test infrastructure
// ===========================================================================

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
  metrics_exporter_prometheus::PrometheusBuilder::new()
    .build_recorder()
    .handle()
}

struct TestHarness {
  jwt_manager: Arc<JwtManager>,
  engine: Arc<StorageEngine>,
  rate_limiter: Arc<RateLimiter>,
  root_jwt: String,
  _temp_dir: tempfile::TempDir,
}

impl TestHarness {
  fn new() -> Self {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let rate_limiter = Arc::new(RateLimiter::default_config());
    let root_jwt = root_bearer_token(&jwt_manager);
    TestHarness {
      jwt_manager,
      engine,
      rate_limiter,
      root_jwt,
      _temp_dir: temp_dir,
    }
  }

  fn app(&self) -> axum::Router {
    let plugin_manager = Arc::new(PluginManager::new(self.engine.clone()));
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> = Arc::new(FileAuthProvider::new(self.engine.clone()));
    create_app_with_all(
      auth_provider,
      self.jwt_manager.clone(),
      plugin_manager,
      self.rate_limiter.clone(),
      make_prometheus_handle(),
      self.engine.clone(),
      Arc::new(EventBus::new()),
      CorsState { default_origins: None, rules: vec![] },
    )
  }

  /// Create a user via the admin API. Returns (user_id, username).
  async fn create_user(&self, username: &str, email: Option<&str>) -> (String, String) {
    let body = match email {
      Some(email) => format!(r#"{{"username":"{}","email":"{}"}}"#, username, email),
      None => format!(r#"{{"username":"{}"}}"#, username),
    };

    let request = Request::builder()
      .method("POST")
      .uri("/system/users")
      .header("content-type", "application/json")
      .header("authorization", &self.root_jwt)
      .body(Body::from(body))
      .unwrap();

    let response = self.app().oneshot(request).await.unwrap();
    assert_eq!(
      response.status(),
      StatusCode::CREATED,
      "Failed to create user '{}'",
      username,
    );

    let json = body_json(response.into_body()).await;
    let user_id = json["user_id"].as_str().unwrap().to_string();
    (user_id, username.to_string())
  }

  /// Create an API key for a user via the admin API. Returns the plaintext key.
  async fn create_api_key_for_user(&self, user_id: &str) -> String {
    let body = format!(r#"{{"user_id":"{}"}}"#, user_id);

    let request = Request::builder()
      .method("POST")
      .uri("/auth/keys/admin")
      .header("content-type", "application/json")
      .header("authorization", &self.root_jwt)
      .body(Body::from(body))
      .unwrap();

    let response = self.app().oneshot(request).await.unwrap();
    assert_eq!(
      response.status(),
      StatusCode::CREATED,
      "Failed to create API key for user '{}'",
      user_id,
    );

    let json = body_json(response.into_body()).await;
    json["api_key"].as_str().unwrap().to_string()
  }

  /// Exchange an API key for a JWT via POST /auth/token. Returns "Bearer <token>".
  async fn get_jwt_for_user(&self, api_key: &str) -> String {
    let body = format!(r#"{{"api_key":"{}"}}"#, api_key);

    let request = Request::builder()
      .method("POST")
      .uri("/auth/token")
      .header("content-type", "application/json")
      .body(Body::from(body))
      .unwrap();

    let response = self.app().oneshot(request).await.unwrap();
    assert_eq!(
      response.status(),
      StatusCode::OK,
      "Failed to exchange API key for JWT",
    );

    let json = body_json(response.into_body()).await;
    let token = json["token"].as_str().unwrap().to_string();
    format!("Bearer {}", token)
  }

  /// Create a group via the admin API.
  async fn create_group(
    &self,
    name: &str,
    default_allow: &str,
    default_deny: &str,
    query_field: &str,
    query_operator: &str,
    query_value: &str,
  ) {
    let body = serde_json::json!({
      "name": name,
      "default_allow": default_allow,
      "default_deny": default_deny,
      "query_field": query_field,
      "query_operator": query_operator,
      "query_value": query_value,
    });

    let request = Request::builder()
      .method("POST")
      .uri("/system/groups")
      .header("content-type", "application/json")
      .header("authorization", &self.root_jwt)
      .body(Body::from(serde_json::to_vec(&body).unwrap()))
      .unwrap();

    let response = self.app().oneshot(request).await.unwrap();
    assert_eq!(
      response.status(),
      StatusCode::CREATED,
      "Failed to create group '{}'",
      name,
    );
  }

  /// Set .permissions at a path using the root JWT via PUT /engine/{path}/.aeordb-permissions.
  async fn set_permissions(&self, path: &str, links: serde_json::Value) {
    let permissions_body = serde_json::json!({ "links": links });
    let permissions_path = if path == "/" || path.ends_with('/') {
      format!("{}.aeordb-permissions", path)
    } else {
      format!("{}/.aeordb-permissions", path)
    };

    let uri = format!("/files/{}", permissions_path.trim_start_matches('/'));

    let request = Request::builder()
      .method("PUT")
      .uri(&uri)
      .header("content-type", "application/json")
      .header("authorization", &self.root_jwt)
      .body(Body::from(serde_json::to_vec(&permissions_body).unwrap()))
      .unwrap();

    let response = self.app().oneshot(request).await.unwrap();
    assert_eq!(
      response.status(),
      StatusCode::CREATED,
      "Failed to set permissions at '{}'",
      uri,
    );
  }

  /// Store a file as root. Returns the status code.
  async fn root_store_file(&self, path: &str, data: &[u8]) -> StatusCode {
    let uri = format!("/files/{}", path.trim_start_matches('/'));

    let request = Request::builder()
      .method("PUT")
      .uri(&uri)
      .header("content-type", "application/octet-stream")
      .header("authorization", &self.root_jwt)
      .body(Body::from(data.to_vec()))
      .unwrap();

    self.app().oneshot(request).await.unwrap().status()
  }

  /// Store a file as a specific user (by JWT). Returns the status code.
  async fn user_store_file(&self, jwt: &str, path: &str, data: &[u8]) -> StatusCode {
    let uri = format!("/files/{}", path.trim_start_matches('/'));

    let request = Request::builder()
      .method("PUT")
      .uri(&uri)
      .header("content-type", "application/octet-stream")
      .header("authorization", jwt)
      .body(Body::from(data.to_vec()))
      .unwrap();

    self.app().oneshot(request).await.unwrap().status()
  }

  /// Read a file as a specific user. Returns (status, body_bytes).
  async fn user_read_file(&self, jwt: &str, path: &str) -> (StatusCode, Vec<u8>) {
    let uri = format!("/files/{}", path.trim_start_matches('/'));

    let request = Request::builder()
      .method("GET")
      .uri(&uri)
      .header("authorization", jwt)
      .body(Body::empty())
      .unwrap();

    let response = self.app().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = body_bytes(response.into_body()).await;
    (status, bytes)
  }

  /// List a directory as a specific user. Returns status code.
  async fn user_list_directory(&self, jwt: &str, path: &str) -> StatusCode {
    // Ensure path ends with '/' for list operation.
    let normalized_path = if path.ends_with('/') {
      path.to_string()
    } else {
      format!("{}/", path)
    };
    let uri = format!("/files/{}", normalized_path.trim_start_matches('/'));

    let request = Request::builder()
      .method("GET")
      .uri(&uri)
      .header("authorization", jwt)
      .body(Body::empty())
      .unwrap();

    self.app().oneshot(request).await.unwrap().status()
  }

  /// Delete a file as a specific user. Returns status code.
  async fn user_delete_file(&self, jwt: &str, path: &str) -> StatusCode {
    let uri = format!("/files/{}", path.trim_start_matches('/'));

    let request = Request::builder()
      .method("DELETE")
      .uri(&uri)
      .header("authorization", jwt)
      .body(Body::empty())
      .unwrap();

    self.app().oneshot(request).await.unwrap().status()
  }

  /// Deactivate a user via DELETE /admin/users/{user_id}.
  async fn deactivate_user(&self, user_id: &str) {
    let request = Request::builder()
      .method("DELETE")
      .uri(&format!("/system/users/{}", user_id))
      .header("authorization", &self.root_jwt)
      .body(Body::empty())
      .unwrap();

    let response = self.app().oneshot(request).await.unwrap();
    assert_eq!(
      response.status(),
      StatusCode::OK,
      "Failed to deactivate user '{}'",
      user_id,
    );
  }

  /// Make a full user (create + API key + JWT). Returns (user_id, jwt).
  async fn make_user(&self, username: &str) -> (String, String) {
    let (user_id, _) = self.create_user(username, None).await;
    let api_key = self.create_api_key_for_user(&user_id).await;
    let jwt = self.get_jwt_for_user(&api_key).await;
    (user_id, jwt)
  }
}

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

fn non_root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: uuid::Uuid::new_v4().to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body_bytes(body).await;
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ===========================================================================
// Scenario 1: Single Developer
// ===========================================================================

#[tokio::test]
async fn scenario_single_developer() {
  let harness = TestHarness::new();

  // Setup: root creates alice.
  let (alice_id, alice_jwt) = harness.make_user("alice").await;

  // Set root-level permissions: alice's auto-group gets full crudlify.
  let alice_group = format!("user:{}", alice_id);
  harness.set_permissions("/", serde_json::json!([
    { "group": alice_group, "allow": "crudlify", "deny": "........" }
  ])).await;

  // Alice stores a file.
  let status = harness.user_store_file(&alice_jwt, "project/readme.txt", b"Hello world").await;
  assert_eq!(status, StatusCode::CREATED, "Alice should be able to store a file");

  // Alice reads it back.
  let (status, data) = harness.user_read_file(&alice_jwt, "project/readme.txt").await;
  assert_eq!(status, StatusCode::OK, "Alice should be able to read the file");
  assert_eq!(data, b"Hello world");

  // Alice lists the directory.
  let status = harness.user_list_directory(&alice_jwt, "project/").await;
  assert_eq!(status, StatusCode::OK, "Alice should be able to list the directory");

  // Alice deletes the file.
  let status = harness.user_delete_file(&alice_jwt, "project/readme.txt").await;
  assert_eq!(status, StatusCode::OK, "Alice should be able to delete the file");

  // Alice overwrites (re-creates) a file.
  let status = harness.user_store_file(&alice_jwt, "project/data.bin", b"first").await;
  assert_eq!(status, StatusCode::CREATED, "Alice should be able to create a file");
  let status = harness.user_store_file(&alice_jwt, "project/data.bin", b"second").await;
  assert_eq!(status, StatusCode::CREATED, "Alice should be able to overwrite a file");
}

// ===========================================================================
// Scenario 2: Small Team (Admin/Developer/Viewer)
// ===========================================================================

#[tokio::test]
async fn scenario_small_team() {
  let harness = TestHarness::new();

  // Create three users.
  let (alice_id, alice_jwt) = harness.make_user("alice").await;
  let (bob_id, bob_jwt) = harness.make_user("bob").await;
  let (_carol_id, carol_jwt) = harness.make_user("carol").await;

  // Groups:
  // "developers" = alice + bob, allow crudli.. (no configure/deploy)
  // "viewers" = everyone active, allow .r..l...
  harness.create_group(
    "developers", "crudli..", "........",
    "user_id", "in", &format!("{},{}", alice_id, bob_id),
  ).await;

  harness.create_group(
    "viewers", ".r..l...", "........",
    "is_active", "eq", "true",
  ).await;

  // Set project-level permissions.
  harness.set_permissions("/project", serde_json::json!([
    { "group": "developers", "allow": "crudli..", "deny": "........" },
    { "group": "viewers", "allow": ".r..l...", "deny": "........" }
  ])).await;

  // Alice (developer): creates a file.
  let status = harness.user_store_file(&alice_jwt, "project/design.md", b"Architecture").await;
  assert_eq!(status, StatusCode::CREATED, "Alice (developer) should create files");

  // Bob (developer): creates a file.
  let status = harness.user_store_file(&bob_jwt, "project/impl.rs", b"fn main(){}").await;
  assert_eq!(status, StatusCode::CREATED, "Bob (developer) should create files");

  // Carol (viewer): cannot create.
  let status = harness.user_store_file(&carol_jwt, "project/hack.txt", b"sneaky").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Carol (viewer) should NOT create files");

  // Carol reads a file.
  let (status, data) = harness.user_read_file(&carol_jwt, "project/design.md").await;
  assert_eq!(status, StatusCode::OK, "Carol (viewer) should read files");
  assert_eq!(data, b"Architecture");

  // Carol lists the directory.
  let status = harness.user_list_directory(&carol_jwt, "project/").await;
  assert_eq!(status, StatusCode::OK, "Carol (viewer) should list directories");

  // Carol cannot delete.
  let status = harness.user_delete_file(&carol_jwt, "project/design.md").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Carol (viewer) should NOT delete files");

  // Bob cannot configure (.config).
  let uri = "/files/project/.config";
  let request = Request::builder()
    .method("PUT")
    .uri(uri)
    .header("content-type", "application/json")
    .header("authorization", &bob_jwt)
    .body(Body::from(r#"{"setting":"value"}"#))
    .unwrap();
  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::FORBIDDEN,
    "Bob (developer without f flag) should NOT configure",
  );
}

// ===========================================================================
// Scenario 3: Organization with Secrets
// ===========================================================================

#[tokio::test]
async fn scenario_organization_secrets() {
  let harness = TestHarness::new();

  // Create users.
  let (engineer_id, engineer_jwt) = harness.make_user("engineer").await;
  let (security_id, security_jwt) = harness.make_user("security_analyst").await;
  let (_employee_id, employee_jwt) = harness.make_user("employee").await;

  // Groups.
  harness.create_group(
    "employees", ".r..l...", "........",
    "is_active", "eq", "true",
  ).await;

  harness.create_group(
    "engineers", "crudli..", "........",
    "user_id", "in", &format!("{},{}", engineer_id, security_id),
  ).await;

  harness.create_group(
    "security_team", "crudlify", "........",
    "user_id", "eq", &security_id,
  ).await;

  // Permissions hierarchy:
  // /org/ -> employees = read+list
  harness.set_permissions("/org", serde_json::json!([
    { "group": "employees", "allow": ".r..l...", "deny": "........" }
  ])).await;

  // /org/engineering/ -> engineers = crudli, others denied
  harness.set_permissions("/org/engineering", serde_json::json!([
    {
      "group": "engineers",
      "allow": "crudli..",
      "deny": "........",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ])).await;

  // /org/engineering/secrets/ -> security_team = full access, others_deny = everything
  harness.set_permissions("/org/engineering/secrets", serde_json::json!([
    {
      "group": "security_team",
      "allow": "crudlify",
      "deny": "........",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ])).await;

  // /org/public/ -> employees = read+list
  harness.set_permissions("/org/public", serde_json::json!([
    { "group": "employees", "allow": ".r..l...", "deny": "........" }
  ])).await;

  // Root seeds some files.
  harness.root_store_file("org/engineering/design.md", b"design notes").await;
  harness.root_store_file("org/engineering/secrets/vault.key", b"TOP SECRET").await;
  harness.root_store_file("org/public/welcome.txt", b"Welcome!").await;

  // Engineer reads engineering files -> 200.
  let (status, _) = harness.user_read_file(&engineer_jwt, "org/engineering/design.md").await;
  assert_eq!(status, StatusCode::OK, "Engineer should read engineering files");

  // Engineer reads secrets -> 403 (others_deny blocks).
  let (status, _) = harness.user_read_file(&engineer_jwt, "org/engineering/secrets/vault.key").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Engineer should NOT read secrets");

  // Security member reads secrets -> 200.
  let (status, data) = harness.user_read_file(&security_jwt, "org/engineering/secrets/vault.key").await;
  assert_eq!(status, StatusCode::OK, "Security member should read secrets");
  assert_eq!(data, b"TOP SECRET");

  // Employee reads public -> 200.
  let (status, _) = harness.user_read_file(&employee_jwt, "org/public/welcome.txt").await;
  assert_eq!(status, StatusCode::OK, "Employee should read public files");

  // Employee reads engineering -> 403 (no engineering group membership).
  let (status, _) = harness.user_read_file(&employee_jwt, "org/engineering/design.md").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Employee should NOT read engineering files");
}

// ===========================================================================
// Scenario 4: Permission Inheritance
// ===========================================================================

#[tokio::test]
async fn scenario_permission_inheritance() {
  let harness = TestHarness::new();

  // Create users.
  let (_reader_id, reader_jwt) = harness.make_user("reader").await;
  let (writer_id, writer_jwt) = harness.make_user("writer").await;
  let (owner_id, owner_jwt) = harness.make_user("owner").await;

  // Groups.
  harness.create_group(
    "everyone", ".r..l...", "........",
    "is_active", "eq", "true",
  ).await;

  harness.create_group(
    "writers", "crudl...", "........",
    "user_id", "in", &format!("{},{}", writer_id, owner_id),
  ).await;

  harness.create_group(
    "owner_group", "crudlify", "........",
    "user_id", "eq", &owner_id,
  ).await;

  // / -> everyone allow .r..l...
  harness.set_permissions("/", serde_json::json!([
    { "group": "everyone", "allow": ".r..l...", "deny": "........" }
  ])).await;

  // /docs/ -> writers allow crudl...
  harness.set_permissions("/docs", serde_json::json!([
    { "group": "writers", "allow": "crudl...", "deny": "........" }
  ])).await;

  // /docs/private/ -> owner_group allow crudlify, non-members denied
  // Using others_deny so only the owner group retains access.
  harness.set_permissions("/docs/private", serde_json::json!([
    {
      "group": "owner_group",
      "allow": "crudlify",
      "deny": "........",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ])).await;

  // Root seeds files.
  harness.root_store_file("hello.txt", b"root file").await;
  harness.root_store_file("docs/guide.md", b"guide").await;
  harness.root_store_file("docs/private/confidential.md", b"secret").await;

  // Everyone reads root -> 200.
  let (status, _) = harness.user_read_file(&reader_jwt, "hello.txt").await;
  assert_eq!(status, StatusCode::OK, "Reader should read root files");

  // Writer creates in /docs/ -> 201.
  let status = harness.user_store_file(&writer_jwt, "docs/notes.md", b"my notes").await;
  assert_eq!(status, StatusCode::CREATED, "Writer should create in /docs/");

  // Writer reads /docs/private/ -> 403 (deny at deeper level overrides).
  let (status, _) = harness.user_read_file(&writer_jwt, "docs/private/confidential.md").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Writer should be denied from /docs/private/");

  // Owner reads /docs/private/ -> 200 (owner allow overrides writer deny at same level).
  let (status, data) = harness.user_read_file(&owner_jwt, "docs/private/confidential.md").await;
  assert_eq!(status, StatusCode::OK, "Owner should read /docs/private/");
  assert_eq!(data, b"secret");
}

// ===========================================================================
// Scenario 5: Multi-Tenant Isolation
// ===========================================================================

#[tokio::test]
async fn scenario_multi_tenant() {
  let harness = TestHarness::new();

  // Create tenant users.
  let (tenant_a_user_id, tenant_a_jwt) = harness.make_user("tenant_a_user").await;
  let (tenant_b_user_id, tenant_b_jwt) = harness.make_user("tenant_b_user").await;

  // Groups.
  harness.create_group(
    "tenant_a_users", "crudlify", "........",
    "user_id", "eq", &tenant_a_user_id,
  ).await;

  harness.create_group(
    "tenant_b_users", "crudlify", "........",
    "user_id", "eq", &tenant_b_user_id,
  ).await;

  // /tenant_a/ -> tenant_a_users = full access, others_deny = everything.
  harness.set_permissions("/tenant_a", serde_json::json!([
    {
      "group": "tenant_a_users",
      "allow": "crudlify",
      "deny": "........",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ])).await;

  // /tenant_b/ -> tenant_b_users = full access, others_deny = everything.
  harness.set_permissions("/tenant_b", serde_json::json!([
    {
      "group": "tenant_b_users",
      "allow": "crudlify",
      "deny": "........",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ])).await;

  // Root seeds files.
  harness.root_store_file("tenant_a/data.json", b"tenant A data").await;
  harness.root_store_file("tenant_b/data.json", b"tenant B data").await;

  // Tenant A accesses tenant A -> 200.
  let (status, data) = harness.user_read_file(&tenant_a_jwt, "tenant_a/data.json").await;
  assert_eq!(status, StatusCode::OK, "Tenant A should access own data");
  assert_eq!(data, b"tenant A data");

  // Tenant A accesses tenant B -> 403.
  let (status, _) = harness.user_read_file(&tenant_a_jwt, "tenant_b/data.json").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Tenant A should NOT access tenant B");

  // Tenant B accesses tenant B -> 200.
  let (status, data) = harness.user_read_file(&tenant_b_jwt, "tenant_b/data.json").await;
  assert_eq!(status, StatusCode::OK, "Tenant B should access own data");
  assert_eq!(data, b"tenant B data");

  // Tenant B accesses tenant A -> 403.
  let (status, _) = harness.user_read_file(&tenant_b_jwt, "tenant_a/data.json").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Tenant B should NOT access tenant A");

  // Root accesses everything -> 200.
  let (status, _) = harness.user_read_file(&harness.root_jwt, "tenant_a/data.json").await;
  assert_eq!(status, StatusCode::OK, "Root should access tenant A");

  let (status, _) = harness.user_read_file(&harness.root_jwt, "tenant_b/data.json").await;
  assert_eq!(status, StatusCode::OK, "Root should access tenant B");
}

// ===========================================================================
// Scenario 6: Security Attacks
// ===========================================================================

#[tokio::test]
async fn scenario_security_nil_uuid_via_http() {
  let harness = TestHarness::new();

  // Try to create a user with nil UUID by sending user_id in the payload.
  // The API does not accept user_id in the creation payload -- it auto-generates.
  // But the nil UUID validation is at the engine level (store_user).
  // We verify no user can have the nil UUID by checking the created user_id.
  let (user_id, _) = harness.create_user("attacker", None).await;
  assert_ne!(
    user_id,
    "00000000-0000-0000-0000-000000000000",
    "Created user_id must never be nil UUID",
  );
}

#[tokio::test]
async fn scenario_security_nil_uuid_api_key() {
  let harness = TestHarness::new();

  // Try to create an API key for the nil UUID (root) via the admin endpoint.
  // store_api_key validates user_id != nil UUID.
  let nil_uuid = "00000000-0000-0000-0000-000000000000";
  let body = format!(r#"{{"user_id":"{}"}}"#, nil_uuid);

  let request = Request::builder()
    .method("POST")
    .uri("/auth/keys/admin")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(body))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  // The store_api_key call should reject nil UUID, returning 500 (engine error).
  assert_ne!(
    response.status(),
    StatusCode::CREATED,
    "Should NOT be able to create API key for nil UUID",
  );
  assert!(
    response.status() == StatusCode::INTERNAL_SERVER_ERROR
      || response.status() == StatusCode::BAD_REQUEST
      || response.status() == StatusCode::FORBIDDEN,
    "Expected error status for nil UUID API key creation, got: {}",
    response.status(),
  );
}

#[tokio::test]
async fn scenario_security_non_root_admin_access() {
  let harness = TestHarness::new();

  // Create a regular user.
  let (_user_id, user_jwt) = harness.make_user("regular_user").await;

  // Non-root user tries to create a user via admin endpoint -> 403.
  let request = Request::builder()
    .method("POST")
    .uri("/system/users")
    .header("content-type", "application/json")
    .header("authorization", &user_jwt)
    .body(Body::from(r#"{"username":"hacker"}"#))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::FORBIDDEN,
    "Non-root user should NOT access admin user creation",
  );

  // Non-root user tries to list users -> 403.
  let request = Request::builder()
    .method("GET")
    .uri("/system/users")
    .header("authorization", &user_jwt)
    .body(Body::empty())
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::FORBIDDEN,
    "Non-root user should NOT list users",
  );

  // Non-root user tries to create a group -> 403.
  let request = Request::builder()
    .method("POST")
    .uri("/system/groups")
    .header("content-type", "application/json")
    .header("authorization", &user_jwt)
    .body(Body::from(
      r#"{"name":"evil","default_allow":"crudlify","default_deny":"........","query_field":"user_id","query_operator":"eq","query_value":"x"}"#,
    ))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::FORBIDDEN,
    "Non-root user should NOT create groups",
  );

  // Non-root user tries to list groups -> 403.
  let request = Request::builder()
    .method("GET")
    .uri("/system/groups")
    .header("authorization", &user_jwt)
    .body(Body::empty())
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::FORBIDDEN,
    "Non-root user should NOT list groups",
  );

  // Non-root user tries to create API keys -> 403.
  let request = Request::builder()
    .method("POST")
    .uri("/auth/keys/admin")
    .header("content-type", "application/json")
    .header("authorization", &user_jwt)
    .body(Body::from(r#"{"user_id":"some-id"}"#))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::FORBIDDEN,
    "Non-root user should NOT create API keys",
  );
}

#[tokio::test]
async fn scenario_security_deactivated_user() {
  let harness = TestHarness::new();

  // Create two users: one that will stay active, one that will be deactivated.
  let (active_id, _) = harness.create_user("stays_active", None).await;
  let active_api_key = harness.create_api_key_for_user(&active_id).await;
  let active_jwt = harness.get_jwt_for_user(&active_api_key).await;

  let (deact_id, _) = harness.create_user("soon_deactivated", None).await;
  let deact_api_key = harness.create_api_key_for_user(&deact_id).await;
  let deact_jwt = harness.get_jwt_for_user(&deact_api_key).await;

  // Create an is_active-based group.
  harness.create_group(
    "active_users", ".r..l...", "........",
    "is_active", "eq", "true",
  ).await;

  // Set permissions that rely on active_users group.
  harness.set_permissions("/gated", serde_json::json!([
    { "group": "active_users", "allow": ".r..l...", "deny": "........" }
  ])).await;

  harness.root_store_file("gated/secret.txt", b"gated data").await;

  // Both active users can access gated resource -> 200.
  let (status, _) = harness.user_read_file(&active_jwt, "gated/secret.txt").await;
  assert_eq!(status, StatusCode::OK, "Active user should access gated files");

  let (status, _) = harness.user_read_file(&deact_jwt, "gated/secret.txt").await;
  assert_eq!(status, StatusCode::OK, "Soon-deactivated user should access gated files while active");

  // Root deactivates one user.
  harness.deactivate_user(&deact_id).await;

  // The group cache from the previous app() call has cached the user's groups.
  // Each app() call creates fresh caches (new GroupCache and PermissionsCache
  // are instantiated in create_app_with_all). So the next read goes through
  // a fresh cache that will re-evaluate group membership.

  // Deactivated user tries to access gated resource -> 403.
  // The group cache is fresh (new app), so it re-loads the user from engine,
  // sees is_active = false, and "active_users" group no longer matches.
  let (status, _) = harness.user_read_file(&deact_jwt, "gated/secret.txt").await;
  assert_eq!(
    status,
    StatusCode::FORBIDDEN,
    "Deactivated user should be denied from is_active-gated resources",
  );

  // Active user still has access.
  let (status, _) = harness.user_read_file(&active_jwt, "gated/secret.txt").await;
  assert_eq!(status, StatusCode::OK, "Active user should still have access");
}

#[tokio::test]
async fn scenario_security_unsafe_query_field_email() {
  let harness = TestHarness::new();

  // Try to create a group with query on "email" -> rejected.
  let body = serde_json::json!({
    "name": "bad_email_group",
    "default_allow": "crudlify",
    "default_deny": "........",
    "query_field": "email",
    "query_operator": "eq",
    "query_value": "admin@example.com",
  });

  let request = Request::builder()
    .method("POST")
    .uri("/system/groups")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::BAD_REQUEST,
    "Group with email query_field should be rejected",
  );
}

#[tokio::test]
async fn scenario_security_unsafe_query_field_username() {
  let harness = TestHarness::new();

  // Try to create a group with query on "username" -> rejected.
  let body = serde_json::json!({
    "name": "bad_username_group",
    "default_allow": "crudlify",
    "default_deny": "........",
    "query_field": "username",
    "query_operator": "eq",
    "query_value": "admin",
  });

  let request = Request::builder()
    .method("POST")
    .uri("/system/groups")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::BAD_REQUEST,
    "Group with username query_field should be rejected",
  );
}

#[tokio::test]
async fn scenario_security_safe_query_field_user_id() {
  let harness = TestHarness::new();

  // Create a group with query on "user_id" -> succeeds.
  let body = serde_json::json!({
    "name": "safe_uid_group",
    "default_allow": "crudlify",
    "default_deny": "........",
    "query_field": "user_id",
    "query_operator": "eq",
    "query_value": "some-uuid-value",
  });

  let request = Request::builder()
    .method("POST")
    .uri("/system/groups")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::CREATED,
    "Group with user_id query_field should succeed",
  );
}

#[tokio::test]
async fn scenario_security_safe_query_field_is_active() {
  let harness = TestHarness::new();

  // Create a group with query on "is_active" -> succeeds.
  let body = serde_json::json!({
    "name": "safe_active_group",
    "default_allow": ".r......",
    "default_deny": "........",
    "query_field": "is_active",
    "query_operator": "eq",
    "query_value": "true",
  });

  let request = Request::builder()
    .method("POST")
    .uri("/system/groups")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::CREATED,
    "Group with is_active query_field should succeed",
  );
}

#[tokio::test]
async fn scenario_security_safe_query_field_created_at() {
  let harness = TestHarness::new();

  // Create a group with query on "created_at" -> succeeds.
  let body = serde_json::json!({
    "name": "safe_created_group",
    "default_allow": ".r......",
    "default_deny": "........",
    "query_field": "created_at",
    "query_operator": "gt",
    "query_value": "0",
  });

  let request = Request::builder()
    .method("POST")
    .uri("/system/groups")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::CREATED,
    "Group with created_at query_field should succeed",
  );
}

#[tokio::test]
async fn scenario_security_unsafe_query_field_arbitrary() {
  let harness = TestHarness::new();

  // Try to create a group with a completely bogus query field -> rejected.
  let body = serde_json::json!({
    "name": "bad_arbitrary_group",
    "default_allow": "crudlify",
    "default_deny": "........",
    "query_field": "password_hash",
    "query_operator": "eq",
    "query_value": "anything",
  });

  let request = Request::builder()
    .method("POST")
    .uri("/system/groups")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::BAD_REQUEST,
    "Group with arbitrary query_field should be rejected",
  );
}

// ===========================================================================
// Scenario 7: Default Deny
// ===========================================================================

#[tokio::test]
async fn scenario_default_deny() {
  let harness = TestHarness::new();

  // Create a user with an API key + JWT.
  let (_user_id, user_jwt) = harness.make_user("denied_user").await;

  // No .permissions files exist anywhere.
  // Root seeds a file (root bypasses all).
  let status = harness.root_store_file("unprotected/data.txt", b"some data").await;
  assert_eq!(status, StatusCode::CREATED, "Root should store files always");

  // User tries to read -> 403 (default deny).
  let (status, _) = harness.user_read_file(&user_jwt, "unprotected/data.txt").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "User should be denied without any permissions");

  // User tries to create -> 403.
  let status = harness.user_store_file(&user_jwt, "unprotected/new.txt", b"hack").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "User should be denied create without permissions");

  // User tries to list -> 403.
  let status = harness.user_list_directory(&user_jwt, "unprotected/").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "User should be denied list without permissions");

  // User tries to delete -> 403.
  let status = harness.user_delete_file(&user_jwt, "unprotected/data.txt").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "User should be denied delete without permissions");

  // Root accesses same path -> 200 (bypass).
  let (status, data) = harness.user_read_file(&harness.root_jwt, "unprotected/data.txt").await;
  assert_eq!(status, StatusCode::OK, "Root should always access files");
  assert_eq!(data, b"some data");
}

// ===========================================================================
// Additional Security Edge Cases
// ===========================================================================

#[tokio::test]
async fn scenario_security_no_auth_token_on_engine_routes() {
  let harness = TestHarness::new();

  // No authorization header at all -> 401.
  let request = Request::builder()
    .method("GET")
    .uri("/files/anything.txt")
    .body(Body::empty())
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::UNAUTHORIZED,
    "No auth token should result in 401",
  );
}

#[tokio::test]
async fn scenario_security_invalid_jwt_on_engine_routes() {
  let harness = TestHarness::new();

  // Invalid JWT -> 401.
  let request = Request::builder()
    .method("GET")
    .uri("/files/anything.txt")
    .header("authorization", "Bearer invalid.jwt.token")
    .body(Body::empty())
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::UNAUTHORIZED,
    "Invalid JWT should result in 401",
  );
}

#[tokio::test]
async fn scenario_security_expired_jwt_on_engine_routes() {
  let harness = TestHarness::new();

  // Create an expired JWT.
  let past = chrono::Utc::now().timestamp() - 3600;
  let claims = TokenClaims {
    sub: uuid::Uuid::new_v4().to_string(),
    iss: "aeordb".to_string(),
    iat: past - 7200,
    exp: past,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = harness
    .jwt_manager
    .create_token(&claims)
    .expect("create token");
  let expired_jwt = format!("Bearer {}", token);

  let request = Request::builder()
    .method("GET")
    .uri("/files/anything.txt")
    .header("authorization", &expired_jwt)
    .body(Body::empty())
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::UNAUTHORIZED,
    "Expired JWT should result in 401",
  );
}

#[tokio::test]
async fn scenario_security_root_bypasses_all_permissions() {
  let harness = TestHarness::new();

  // Set up a deny-everything permission at root level.
  harness.create_group(
    "nobody", "........", "........",
    "is_active", "eq", "false",
  ).await;

  harness.set_permissions("/", serde_json::json!([
    {
      "group": "nobody",
      "allow": "........",
      "deny": "........",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ])).await;

  // Root stores, reads, and deletes despite the deny-all.
  let status = harness.root_store_file("lockdown/file.txt", b"data").await;
  assert_eq!(status, StatusCode::CREATED, "Root should bypass deny-all");

  let (status, _) = harness.user_read_file(&harness.root_jwt, "lockdown/file.txt").await;
  assert_eq!(status, StatusCode::OK, "Root should bypass deny-all for reads");

  let status = harness.user_delete_file(&harness.root_jwt, "lockdown/file.txt").await;
  assert_eq!(status, StatusCode::OK, "Root should bypass deny-all for deletes");
}

#[tokio::test]
async fn scenario_security_deny_overrides_allow_at_same_level() {
  let harness = TestHarness::new();

  let (user_id, user_jwt) = harness.make_user("conflicted_user").await;

  // Create two groups: one that allows, one that denies.
  // User is a member of both.
  harness.create_group(
    "allowgroup", "crudlify", "........",
    "user_id", "eq", &user_id,
  ).await;

  harness.create_group(
    "denygroup", "........", "........",
    "user_id", "eq", &user_id,
  ).await;

  // At the same level: allowgroup grants crudlify, denygroup denies crudlify.
  harness.set_permissions("/conflict", serde_json::json!([
    { "group": "allowgroup", "allow": "crudlify", "deny": "........" },
    { "group": "denygroup", "allow": "........", "deny": "crudlify" }
  ])).await;

  harness.root_store_file("conflict/test.txt", b"data").await;

  // Deny should override allow at the same level.
  let (status, _) = harness.user_read_file(&user_jwt, "conflict/test.txt").await;
  assert_eq!(
    status,
    StatusCode::FORBIDDEN,
    "Deny should override allow at same level",
  );
}

#[tokio::test]
async fn scenario_security_others_flags_apply_to_non_members() {
  let harness = TestHarness::new();

  let (member_id, member_jwt) = harness.make_user("member").await;
  let (_outsider_id, outsider_jwt) = harness.make_user("outsider").await;

  harness.create_group(
    "exclusive_club", "crudlify", "........",
    "user_id", "eq", &member_id,
  ).await;

  // Members get full access, non-members get read-only via others_allow.
  harness.set_permissions("/clubhouse", serde_json::json!([
    {
      "group": "exclusive_club",
      "allow": "crudlify",
      "deny": "........",
      "others_allow": ".r..l...",
      "others_deny": "........"
    }
  ])).await;

  harness.root_store_file("clubhouse/welcome.txt", b"welcome").await;

  // Member: full access.
  let (status, _) = harness.user_read_file(&member_jwt, "clubhouse/welcome.txt").await;
  assert_eq!(status, StatusCode::OK, "Member should read");

  let status = harness.user_store_file(&member_jwt, "clubhouse/new.txt", b"content").await;
  assert_eq!(status, StatusCode::CREATED, "Member should create");

  // Outsider: read-only via others_allow.
  let (status, _) = harness.user_read_file(&outsider_jwt, "clubhouse/welcome.txt").await;
  assert_eq!(status, StatusCode::OK, "Outsider should read via others_allow");

  // Outsider: cannot create (no 'c' in others_allow).
  let status = harness.user_store_file(&outsider_jwt, "clubhouse/hack.txt", b"nope").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Outsider should NOT create");
}

#[tokio::test]
async fn scenario_deeper_permission_overrides_shallower() {
  let harness = TestHarness::new();

  let (_user_id, user_jwt) = harness.make_user("layered_user").await;

  // "everyone" group.
  harness.create_group(
    "all_active", ".r..l...", "........",
    "is_active", "eq", "true",
  ).await;

  // Shallow: deny read at root.
  harness.set_permissions("/", serde_json::json!([
    {
      "group": "all_active",
      "allow": "........",
      "deny": "crudlify",
      "others_allow": "........",
      "others_deny": "crudlify"
    }
  ])).await;

  // Deeper: allow read at /open/.
  harness.set_permissions("/open", serde_json::json!([
    { "group": "all_active", "allow": ".r..l...", "deny": "........" }
  ])).await;

  harness.root_store_file("open/public.txt", b"public data").await;
  harness.root_store_file("closed/private.txt", b"private data").await;

  // User can read /open/public.txt (deeper allow overrides shallower deny).
  let (status, _) = harness.user_read_file(&user_jwt, "open/public.txt").await;
  assert_eq!(status, StatusCode::OK, "Deeper allow should override shallower deny");

  // User cannot read /closed/private.txt (still denied from root).
  let (status, _) = harness.user_read_file(&user_jwt, "closed/private.txt").await;
  assert_eq!(status, StatusCode::FORBIDDEN, "Root deny should still apply without deeper override");
}

#[tokio::test]
async fn scenario_security_update_group_to_unsafe_field_rejected() {
  let harness = TestHarness::new();

  // Create a safe group first.
  harness.create_group(
    "mutable_group", "crudlify", "........",
    "user_id", "eq", "some-value",
  ).await;

  // Try to update its query_field to "email" -> rejected.
  let body = serde_json::json!({ "query_field": "email" });

  let request = Request::builder()
    .method("PATCH")
    .uri("/system/groups/mutable_group")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::BAD_REQUEST,
    "Updating group to unsafe query_field should be rejected",
  );

  // Try to update to "username" -> rejected.
  let body = serde_json::json!({ "query_field": "username" });

  let request = Request::builder()
    .method("PATCH")
    .uri("/system/groups/mutable_group")
    .header("content-type", "application/json")
    .header("authorization", &harness.root_jwt)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = harness.app().oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::BAD_REQUEST,
    "Updating group to unsafe query_field 'username' should be rejected",
  );
}

#[tokio::test]
async fn scenario_security_non_root_jwt_with_random_uuid() {
  let harness = TestHarness::new();

  // Craft a JWT with a random UUID that doesn't correspond to any real user.
  // This user won't be in any groups, so default deny should kick in.
  let auth = non_root_bearer_token(&harness.jwt_manager);

  harness.root_store_file("phantom/data.txt", b"data").await;

  // No permissions set. Default deny.
  let (status, _) = harness.user_read_file(&auth, "phantom/data.txt").await;
  assert_eq!(
    status,
    StatusCode::FORBIDDEN,
    "Random UUID JWT should be denied by default",
  );

  // Even with permissions for "all active users", this user doesn't exist
  // in system_store so group_cache returns empty groups.
  harness.create_group(
    "phantom_active", ".r..l...", "........",
    "is_active", "eq", "true",
  ).await;

  harness.set_permissions("/phantom", serde_json::json!([
    { "group": "phantom_active", "allow": ".r..l...", "deny": "........" }
  ])).await;

  let (status, _) = harness.user_read_file(&auth, "phantom/data.txt").await;
  assert_eq!(
    status,
    StatusCode::FORBIDDEN,
    "Non-existent user should be denied even with active-user group",
  );
}
