use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aeordb::engine::permission_resolver::{CrudlifyOp, evaluate_ordered_path_permissions};
use aeordb::engine::permissions::{PathPermissions, PermissionLink};
use aeordb::engine::v4::namespace::{NamespaceRootV1, NamespaceTreeLayoutV0, NamespaceTreeRootV0, SemanticAvailabilityV1, SemanticStateV1};
use aeordb::engine::v4::read_view::{
  CurrentReadAuthorizationV1, LoadedReadAuthorityV1, ReadViewAuthorizationErrorV1, ReadViewAuthorizationFailureV1, ReadViewAuthorizerV1,
  ReadViewConcealmentV1, ReadViewCredentialKindV1,
};
use aeordb::engine::v4::read_view_authorization::{
  CapturedCurrentPathAuthorizationSourceV1, CurrentPathAuthorizationSourceV1, CurrentPathAuthorizationV1, PathAuthorizationDecisionV1,
  ReadViewPermissionAuthorizerV1, SelectedRootPermissionRequestV1, SelectedRootPermissionSourceV1, SelectedRootRestrictionV1,
};
use aeordb::engine::v4::root_authority::{ImmutableNamespaceAuthorityV1, RootAdmissionCommitV1, RootAuthorityKindV1};
use tokio_util::sync::CancellationToken;

fn link(
  group: &str,
  allow: &str,
  deny: &str,
  others_allow: Option<&str>,
  others_deny: Option<&str>,
  path_pattern: Option<&str>,
) -> PermissionLink {
  PermissionLink {
    group: group.to_string(),
    allow: allow.to_string(),
    deny: deny.to_string(),
    others_allow: others_allow.map(str::to_string),
    others_deny: others_deny.map(str::to_string),
    path_pattern: path_pattern.map(str::to_string),
  }
}

fn documents(entries: impl IntoIterator<Item = (&'static str, PathPermissions)>) -> BTreeMap<String, PathPermissions> {
  entries.into_iter().map(|(path, permissions)| (path.to_string(), permissions)).collect()
}

fn names(values: &[&str]) -> BTreeSet<String> {
  values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn ordered_evaluator_preserves_inheritance_deny_precedence_and_level_order() {
  let permissions = documents([
    ("/", PathPermissions { links: vec![link("editors", ".r......", "........", None, None, None)] }),
    ("/docs", PathPermissions { links: vec![link("editors", "........", ".r......", None, None, None)] }),
    ("/docs/private", PathPermissions { links: vec![link("editors", ".r......", "........", None, None, None)] }),
  ]);
  let mut loaded = Vec::new();
  let groups = vec!["editors".to_string()];

  let allowed = evaluate_ordered_path_permissions(&groups, "/docs/private/answer.txt", CrudlifyOp::Read, |level| {
    loaded.push(level.to_string());
    Ok::<_, ()>(permissions.get(level).cloned())
  })
  .unwrap();

  assert!(allowed);
  assert_eq!(loaded, ["/", "/docs", "/docs/private"]);
}

#[test]
fn ordered_evaluator_preserves_path_patterns_and_others_rules() {
  let permissions = documents([(
    "/docs",
    PathPermissions {
      links: vec![
        link("editors", ".r......", "........", None, None, Some("public.txt")),
        link("admins", "........", "........", Some(".r......"), None, Some("other.txt")),
      ],
    },
  )]);
  let groups = vec!["editors".to_string()];
  let evaluate = |path| {
    evaluate_ordered_path_permissions(&groups, path, CrudlifyOp::Read, |level| Ok::<_, ()>(permissions.get(level).cloned())).unwrap()
  };

  assert!(evaluate("docs/public.txt"));
  assert!(evaluate("/docs/other.txt"));
  assert!(!evaluate("/docs/private.txt"));
  assert!(!evaluate("/docs/nested/public.txt"));
}

#[test]
fn authorization_decision_intersection_is_restrictive_and_deterministic() {
  let direct = PathAuthorizationDecisionV1::direct();
  let left = PathAuthorizationDecisionV1::ancestor_navigation(names(&["alpha", "shared"])).unwrap();
  let right = PathAuthorizationDecisionV1::ancestor_navigation(names(&["shared", "zeta"])).unwrap();

  assert_eq!(direct.intersect(&direct), Some(direct.clone()));
  assert_eq!(direct.intersect(&left), Some(left.clone()));
  assert_eq!(left.intersect(&direct), Some(left.clone()));
  let overlap = left.intersect(&right).unwrap();
  assert_eq!(overlap.allowed_children(), Some(&names(&["shared"])));
  assert_eq!(right.intersect(&left), Some(overlap));

  let disjoint = PathAuthorizationDecisionV1::ancestor_navigation(names(&["none"])).unwrap();
  assert_eq!(left.intersect(&disjoint), None);
  assert_eq!(PathAuthorizationDecisionV1::ancestor_navigation(BTreeSet::new()), None);
}

#[derive(Clone)]
struct FakeCurrentSource {
  result: Result<CurrentReadAuthorizationV1<CurrentPathAuthorizationV1>, ReadViewAuthorizationErrorV1>,
  calls: Arc<AtomicUsize>,
}

impl CurrentPathAuthorizationSourceV1 for FakeCurrentSource {
  fn authorize_current(
    &self,
    _cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<CurrentPathAuthorizationV1>, ReadViewAuthorizationErrorV1> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    self.result.clone()
  }
}

#[derive(Clone)]
struct FakeSelectedSource {
  result: Result<Option<PathAuthorizationDecisionV1>, ReadViewAuthorizationFailureV1>,
  calls: Arc<AtomicUsize>,
  requests: Arc<std::sync::Mutex<Vec<(String, CrudlifyOp, Vec<String>)>>>,
}

struct CancelingSelectedSource {
  calls: Arc<AtomicUsize>,
}

impl SelectedRootPermissionSourceV1 for CancelingSelectedSource {
  fn authorize_selected_root(
    &self,
    _authority: &LoadedReadAuthorityV1,
    _request: SelectedRootPermissionRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<Option<PathAuthorizationDecisionV1>, ReadViewAuthorizationFailureV1> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    cancellation.cancel();
    Ok(Some(PathAuthorizationDecisionV1::direct()))
  }
}

impl SelectedRootPermissionSourceV1 for FakeSelectedSource {
  fn authorize_selected_root(
    &self,
    _authority: &LoadedReadAuthorityV1,
    request: SelectedRootPermissionRequestV1<'_>,
    _cancellation: &CancellationToken,
  ) -> Result<Option<PathAuthorizationDecisionV1>, ReadViewAuthorizationFailureV1> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    self.requests.lock().unwrap().push((request.path().to_string(), request.operation(), request.current_groups().to_vec()));
    self.result.clone()
  }
}

fn current(
  decision: PathAuthorizationDecisionV1,
  restriction: SelectedRootRestrictionV1,
) -> CurrentReadAuthorizationV1<CurrentPathAuthorizationV1> {
  let authorization = match restriction {
    SelectedRootRestrictionV1::PermissionDocuments => {
      CurrentPathAuthorizationV1::for_user("docs/", CrudlifyOp::List, vec!["current-editors".to_string()], decision)
    }
    SelectedRootRestrictionV1::RootCurrentPolicy => CurrentPathAuthorizationV1::for_root("docs/", CrudlifyOp::List),
    SelectedRootRestrictionV1::ShareCurrentPolicy => {
      CurrentPathAuthorizationV1::for_share_current_head("docs/", CrudlifyOp::List, decision)
    }
  };
  CurrentReadAuthorizationV1::new(
    authorization,
    if restriction == SelectedRootRestrictionV1::ShareCurrentPolicy {
      ReadViewCredentialKindV1::Share
    } else {
      ReadViewCredentialKindV1::Ordinary
    },
    ReadViewConcealmentV1::Conceal,
  )
}

fn authorizer(
  current_result: Result<CurrentReadAuthorizationV1<CurrentPathAuthorizationV1>, ReadViewAuthorizationErrorV1>,
  selected_result: Result<Option<PathAuthorizationDecisionV1>, ReadViewAuthorizationFailureV1>,
) -> (ReadViewPermissionAuthorizerV1<FakeCurrentSource, FakeSelectedSource>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
  let current_calls = Arc::new(AtomicUsize::new(0));
  let selected_calls = Arc::new(AtomicUsize::new(0));
  let current = FakeCurrentSource { result: current_result, calls: Arc::clone(&current_calls) };
  let selected = FakeSelectedSource {
    result: selected_result,
    calls: Arc::clone(&selected_calls),
    requests: Arc::new(std::sync::Mutex::new(Vec::new())),
  };
  (ReadViewPermissionAuthorizerV1::new(current, selected), current_calls, selected_calls)
}

fn loaded_authority() -> LoadedReadAuthorityV1 {
  let root_hash = vec![0x11; 32];
  let namespace_tree_root = vec![0x22; 32];
  let semantic_state_root = vec![0x33; 32];
  LoadedReadAuthorityV1 {
    authority: ImmutableNamespaceAuthorityV1 {
      root: NamespaceRootV1 {
        root_hash: root_hash.clone(),
        required_capabilities: [0; 32],
        namespace_tree_codec: 0,
        semantic_state_codec: 1,
        namespace_tree_root: namespace_tree_root.clone(),
        semantic_state_root: semantic_state_root.clone(),
      },
      namespace_tree: NamespaceTreeRootV0 { root_hash: namespace_tree_root, layout: NamespaceTreeLayoutV0::Empty, edges: Vec::new() },
      semantic_state: SemanticStateV1 {
        object_id: semantic_state_root,
        required_capabilities: [0; 32],
        semantic_catalog_codec: 1,
        semantic_definition_codec: 1,
        compiler_profile_version: 1,
        availability: SemanticAvailabilityV1::Complete {
          compiler_fingerprint: vec![0x41; 32],
          semantic_registry_fingerprint: vec![0x42; 32],
          catalog_root: vec![0x43; 32],
          catalog_record_count: 1,
          catalog_node_count: 1,
          definition_count: 1,
          dependency_count: 0,
        },
      },
      admission: RootAdmissionCommitV1 {
        database_id: [0x44; 16],
        namespace_root: root_hash,
        transaction_id: [0x45; 16],
        publication_started_at_ms: 1,
        authority_kind: RootAuthorityKindV1::Head,
        recovered_from_selected_authority: false,
        authority_identity_digest: vec![0x46; 32],
        authority_after: vec![0x47; 32],
        selected_header_slot_sequence: 1,
        publication_sequence: 1,
        prepare_payload_hash: vec![0x48; 32],
      },
    },
    legacy_root_hash: None,
  }
}

#[test]
fn authorizer_reuses_current_identity_and_selected_decision_only_restricts() {
  let current_decision = PathAuthorizationDecisionV1::direct();
  let selected_decision = PathAuthorizationDecisionV1::ancestor_navigation(names(&["allowed"])).unwrap();
  let (authorizer, current_calls, selected_calls) =
    authorizer(Ok(current(current_decision, SelectedRootRestrictionV1::PermissionDocuments)), Ok(Some(selected_decision.clone())));
  let cancellation = CancellationToken::new();

  let current = authorizer.authorize_current(&cancellation).unwrap();
  let resolved = authorizer.restrict_to_selected_root(current.authorization(), &loaded_authority(), &cancellation).unwrap();

  assert_eq!(resolved, selected_decision);
  assert_eq!(current_calls.load(Ordering::SeqCst), 1);
  assert_eq!(selected_calls.load(Ordering::SeqCst), 1);
  let requests = authorizer.selected_source().requests.lock().unwrap();
  assert_eq!(requests.as_slice(), &[("/docs/".to_string(), CrudlifyOp::List, vec!["current-editors".to_string()])]);
}

#[test]
fn selected_root_cannot_expand_current_filtered_navigation() {
  let current_decision = PathAuthorizationDecisionV1::ancestor_navigation(names(&["current", "shared"])).unwrap();
  let selected_decision = PathAuthorizationDecisionV1::ancestor_navigation(names(&["selected", "shared"])).unwrap();
  let (authorizer, _, _) =
    authorizer(Ok(current(current_decision, SelectedRootRestrictionV1::PermissionDocuments)), Ok(Some(selected_decision)));
  let cancellation = CancellationToken::new();
  let current = authorizer.authorize_current(&cancellation).unwrap();

  let resolved = authorizer.restrict_to_selected_root(current.authorization(), &loaded_authority(), &cancellation).unwrap();

  assert_eq!(resolved.allowed_children(), Some(&names(&["shared"])));
}

#[test]
fn empty_selected_intersection_and_explicit_selected_denial_fail_closed() {
  for selected in [Ok(PathAuthorizationDecisionV1::ancestor_navigation(names(&["selected"]))), Ok(None)] {
    let current_decision = PathAuthorizationDecisionV1::ancestor_navigation(names(&["current"])).unwrap();
    let (authorizer, _, _) = authorizer(Ok(current(current_decision, SelectedRootRestrictionV1::PermissionDocuments)), selected);
    let cancellation = CancellationToken::new();
    let current = authorizer.authorize_current(&cancellation).unwrap();

    let error = authorizer.restrict_to_selected_root(current.authorization(), &loaded_authority(), &cancellation).unwrap_err();

    assert_eq!(error, ReadViewAuthorizationFailureV1::Denied);
  }
}

#[test]
fn current_only_authority_skips_selected_root_permission_source() {
  for restriction in [SelectedRootRestrictionV1::RootCurrentPolicy, SelectedRootRestrictionV1::ShareCurrentPolicy] {
    let (authorizer, _, selected_calls) = authorizer(
      Ok(current(PathAuthorizationDecisionV1::direct(), restriction)),
      Err(ReadViewAuthorizationFailureV1::Corrupt("must not be observed".to_string())),
    );
    let cancellation = CancellationToken::new();
    let current = authorizer.authorize_current(&cancellation).unwrap();

    let resolved = authorizer.restrict_to_selected_root(current.authorization(), &loaded_authority(), &cancellation).unwrap();

    assert!(resolved.is_direct());
    assert_eq!(selected_calls.load(Ordering::SeqCst), 0);
  }
}

#[test]
fn credential_and_current_policy_mismatch_fails_before_selected_or_root_authority() {
  let mismatches = [
    CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_share_current_head("/docs/", CrudlifyOp::List, PathAuthorizationDecisionV1::direct()),
      ReadViewCredentialKindV1::Ordinary,
      ReadViewConcealmentV1::Conceal,
    ),
    CurrentReadAuthorizationV1::new(
      CurrentPathAuthorizationV1::for_user(
        "/docs/",
        CrudlifyOp::List,
        vec!["current-editors".to_string()],
        PathAuthorizationDecisionV1::direct(),
      ),
      ReadViewCredentialKindV1::Share,
      ReadViewConcealmentV1::Reveal,
    ),
  ];
  for mismatch in mismatches {
    let (authorizer, _, selected_calls) = authorizer(Ok(mismatch), Ok(Some(PathAuthorizationDecisionV1::direct())));

    let error = authorizer.authorize_current(&CancellationToken::new()).unwrap_err();

    assert!(matches!(error, ReadViewAuthorizationErrorV1::Corrupt { .. }));
    assert_eq!(selected_calls.load(Ordering::SeqCst), 0);
  }
}

#[test]
fn current_failure_prevents_selected_root_permission_access() {
  for error in [
    ReadViewAuthorizationErrorV1::denied(ReadViewConcealmentV1::Conceal),
    ReadViewAuthorizationErrorV1::unavailable(ReadViewConcealmentV1::Reveal, "current permission source unavailable"),
    ReadViewAuthorizationErrorV1::corrupt(ReadViewConcealmentV1::Conceal, "current permission source corrupt"),
  ] {
    let expected = error.clone();
    let (authorizer, current_calls, selected_calls) = authorizer(Err(error), Ok(Some(PathAuthorizationDecisionV1::direct())));

    assert_eq!(authorizer.authorize_current(&CancellationToken::new()).unwrap_err(), expected);
    assert_eq!(current_calls.load(Ordering::SeqCst), 1);
    assert_eq!(selected_calls.load(Ordering::SeqCst), 0);
  }
}

#[test]
fn captured_current_source_preserves_credential_concealment_and_outcome() {
  let authorized = current(PathAuthorizationDecisionV1::direct(), SelectedRootRestrictionV1::PermissionDocuments);
  let source = CapturedCurrentPathAuthorizationSourceV1::new(Ok(authorized.clone()));
  let observed = source.authorize_current(&CancellationToken::new()).unwrap();
  assert_eq!(observed.authorization(), authorized.authorization());
  assert_eq!(observed.credential_kind(), ReadViewCredentialKindV1::Ordinary);
  assert_eq!(observed.concealment(), ReadViewConcealmentV1::Conceal);

  let failure = ReadViewAuthorizationErrorV1::denied(ReadViewConcealmentV1::Reveal);
  let source = CapturedCurrentPathAuthorizationSourceV1::new(Err(failure.clone()));
  assert_eq!(source.authorize_current(&CancellationToken::new()).unwrap_err(), failure);
}

#[test]
fn selected_root_failures_and_cancellation_remain_typed() {
  for failure in [
    ReadViewAuthorizationFailureV1::Unavailable("selected permission source unavailable".to_string()),
    ReadViewAuthorizationFailureV1::Corrupt("selected permission source corrupt".to_string()),
    ReadViewAuthorizationFailureV1::Canceled,
  ] {
    let (authorizer, _, _) =
      authorizer(Ok(current(PathAuthorizationDecisionV1::direct(), SelectedRootRestrictionV1::PermissionDocuments)), Err(failure.clone()));
    let cancellation = CancellationToken::new();
    let current = authorizer.authorize_current(&cancellation).unwrap();

    assert_eq!(authorizer.restrict_to_selected_root(current.authorization(), &loaded_authority(), &cancellation).unwrap_err(), failure);
  }
}

#[test]
fn cancellation_before_or_during_selected_evaluation_never_returns_authority() {
  let selected_calls = Arc::new(AtomicUsize::new(0));
  let current = current(PathAuthorizationDecisionV1::direct(), SelectedRootRestrictionV1::PermissionDocuments);
  let authorizer = ReadViewPermissionAuthorizerV1::new(
    CapturedCurrentPathAuthorizationSourceV1::new(Ok(current)),
    CancelingSelectedSource { calls: Arc::clone(&selected_calls) },
  );
  let cancellation = CancellationToken::new();
  let current = authorizer.authorize_current(&cancellation).unwrap();

  let error = authorizer.restrict_to_selected_root(current.authorization(), &loaded_authority(), &cancellation).unwrap_err();
  assert_eq!(error, ReadViewAuthorizationFailureV1::Canceled);
  assert_eq!(selected_calls.load(Ordering::SeqCst), 1);

  let before = CancellationToken::new();
  before.cancel();
  let error = authorizer.restrict_to_selected_root(current.authorization(), &loaded_authority(), &before).unwrap_err();
  assert_eq!(error, ReadViewAuthorizationFailureV1::Canceled);
  assert_eq!(selected_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn current_resolver_delegates_to_the_shared_ordered_evaluator() {
  let source = include_str!("../../src/engine/permission_resolver.rs");
  let direct = source.split("pub fn check_direct_permission").nth(1).unwrap().split("pub fn has_descendant_grants").next().unwrap();
  assert_eq!(direct.matches("evaluate_ordered_path_permissions").count(), 1);
  assert!(!direct.contains("parse_crudlify_flags"));
}

#[test]
fn permission_intersection_module_remains_storage_and_service_neutral() {
  let source = include_str!("../../src/engine/v4/read_view_authorization.rs");
  for forbidden in ["StorageEngine", "DirectoryOps", "AppState", "tokio::spawn", "std::thread", "permissions_cache", "grants_index_cache"] {
    assert!(!source.contains(forbidden), "permission intersection module acquired forbidden dependency {forbidden}");
  }
}
