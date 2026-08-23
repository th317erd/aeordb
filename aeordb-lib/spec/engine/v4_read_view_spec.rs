use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::database_header::{SelectedDatabaseHeaderV4, decode_header_region};
use aeordb::engine::v4::namespace::{
  NamespaceRootV1, NamespaceTreeLayoutV0, NamespaceTreeRootV0, SemanticAvailabilityV1, SemanticStateV1, SemanticUnavailableReasonV1,
};
use aeordb::engine::v4::read_view::{
  CurrentReadAuthorizationV1, LoadedReadAuthorityV1, ReadViewAuthoritySourceV1, ReadViewAuthorizationErrorV1,
  ReadViewAuthorizationFailureV1, ReadViewAuthorizerV1, ReadViewConcealmentV1, ReadViewCredentialKindV1, ReadViewResolverV1,
  ReadViewLifecycleErrorV1, ReadViewSelectorV1, ReadViewSourceErrorV1, RootLifecycleObservationV1, RootReadPinCoordinatorV1,
};
use aeordb::engine::v4::root_authority::{ImmutableNamespaceAuthorityV1, RootAdmissionCommitV1, RootAuthorityKindV1};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/database-header-v4")
}

fn hash(algorithm: HashAlgorithm, byte: u8) -> Vec<u8> {
  vec![byte; algorithm.hash_length()]
}

fn selected_header(algorithm: HashAlgorithm, head_hash: Vec<u8>) -> SelectedDatabaseHeaderV4 {
  let name = match algorithm {
    HashAlgorithm::Blake3_256 => "header-blake3-256-valid-ab.bin",
    HashAlgorithm::Sha512 => "header-sha512-valid-ab.bin",
    _ => panic!("the resolver spec uses only frozen Blake3-256 and SHA-512 headers"),
  };
  let mut selected = decode_header_region(&fs::read(fixture_root().join(name)).unwrap()).unwrap();
  selected.header.head_hash = head_hash;
  selected.header.slot_sequence = 20;
  selected.header.write_sequence_high_water = 200;
  selected
}

fn authority(header: &SelectedDatabaseHeaderV4, root_hash: Vec<u8>, content_only: bool) -> ImmutableNamespaceAuthorityV1 {
  let algorithm = header.header.hash_algorithm;
  let namespace_tree_root = hash(algorithm, 0x22);
  let semantic_state_root = hash(algorithm, 0x33);
  let availability = if content_only {
    SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured }
  } else {
    SemanticAvailabilityV1::Complete {
      compiler_fingerprint: hash(algorithm, 0x41),
      semantic_registry_fingerprint: hash(algorithm, 0x42),
      catalog_root: hash(algorithm, 0x43),
      catalog_record_count: 1,
      catalog_node_count: 1,
      definition_count: 1,
      dependency_count: 0,
    }
  };
  ImmutableNamespaceAuthorityV1 {
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
      availability,
    },
    admission: RootAdmissionCommitV1 {
      database_id: header.header.database_id,
      namespace_root: root_hash,
      transaction_id: [0x55; 16],
      publication_started_at_ms: 1_700_000_000_000,
      authority_kind: RootAuthorityKindV1::Head,
      recovered_from_selected_authority: false,
      authority_identity_digest: hash(algorithm, 0x56),
      authority_after: hash(algorithm, 0x57),
      selected_header_slot_sequence: 19,
      publication_sequence: 199,
      prepare_payload_hash: hash(algorithm, 0x58),
    },
  }
}

fn all_capabilities_profile() -> BinaryCapabilityProfileV1 {
  let all = CapabilitySetV1::from_bits(0..24).unwrap();
  BinaryCapabilityProfileV1::new(all, all)
}

fn pin_coordinator(algorithm: HashAlgorithm) -> RootReadPinCoordinatorV1 {
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(8 * 1_024 * 1_024, 16 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()));
  RootReadPinCoordinatorV1::new(memory, algorithm, 8, 16).unwrap()
}

#[derive(Clone)]
struct FakeAuthoritySource {
  header: SelectedDatabaseHeaderV4,
  accepted_root: Vec<u8>,
  authority: ImmutableNamespaceAuthorityV1,
  header_error: Option<ReadViewSourceErrorV1>,
  authority_error: Option<ReadViewSourceErrorV1>,
  lifecycle: RootLifecycleObservationV1,
  lifecycle_error: Option<ReadViewLifecycleErrorV1>,
  legacy_root_hash: Option<Vec<u8>>,
  cancel_after_header: bool,
  cancel_after_authority: bool,
  header_calls: Arc<AtomicUsize>,
  authority_calls: Arc<AtomicUsize>,
  lifecycle_calls: Arc<AtomicUsize>,
  requested_roots: Arc<Mutex<Vec<Vec<u8>>>>,
  order: Arc<Mutex<Vec<&'static str>>>,
}

impl FakeAuthoritySource {
  fn new(header: SelectedDatabaseHeaderV4, accepted_root: Vec<u8>, content_only: bool) -> Self {
    Self {
      authority: authority(&header, accepted_root.clone(), content_only),
      header,
      accepted_root,
      header_error: None,
      authority_error: None,
      lifecycle: RootLifecycleObservationV1::Live,
      lifecycle_error: None,
      legacy_root_hash: None,
      cancel_after_header: false,
      cancel_after_authority: false,
      header_calls: Arc::new(AtomicUsize::new(0)),
      authority_calls: Arc::new(AtomicUsize::new(0)),
      lifecycle_calls: Arc::new(AtomicUsize::new(0)),
      requested_roots: Arc::new(Mutex::new(Vec::new())),
      order: Arc::new(Mutex::new(Vec::new())),
    }
  }
}

impl ReadViewAuthoritySourceV1 for FakeAuthoritySource {
  fn capture_header(&self, cancellation: &CancellationToken) -> Result<SelectedDatabaseHeaderV4, ReadViewSourceErrorV1> {
    self.header_calls.fetch_add(1, Ordering::SeqCst);
    self.order.lock().unwrap().push("header");
    if let Some(error) = &self.header_error {
      return Err(error.clone());
    }
    if self.cancel_after_header {
      cancellation.cancel();
    }
    Ok(self.header.clone())
  }

  fn load_verified_authority(
    &self,
    _header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<LoadedReadAuthorityV1, ReadViewSourceErrorV1> {
    self.authority_calls.fetch_add(1, Ordering::SeqCst);
    self.requested_roots.lock().unwrap().push(root_hash.to_vec());
    self.order.lock().unwrap().push("authority");
    if let Some(error) = &self.authority_error {
      return Err(error.clone());
    }
    if root_hash != self.accepted_root {
      return Err(ReadViewSourceErrorV1::RootNotAdmitted);
    }
    if self.cancel_after_authority {
      cancellation.cancel();
    }
    Ok(LoadedReadAuthorityV1::new(self.authority.clone(), self.legacy_root_hash.clone()))
  }

  fn observe_lifecycle(
    &self,
    _header: &SelectedDatabaseHeaderV4,
    _root_hash: &[u8],
    _cancellation: &CancellationToken,
  ) -> Result<RootLifecycleObservationV1, ReadViewLifecycleErrorV1> {
    self.lifecycle_calls.fetch_add(1, Ordering::SeqCst);
    self.order.lock().unwrap().push("lifecycle");
    if let Some(error) = &self.lifecycle_error {
      return Err(error.clone());
    }
    Ok(self.lifecycle)
  }
}

#[derive(Clone)]
struct FakeAuthorizer {
  credential_kind: ReadViewCredentialKindV1,
  concealment: ReadViewConcealmentV1,
  current_error: Option<ReadViewAuthorizationErrorV1>,
  selected_error: Option<ReadViewAuthorizationFailureV1>,
  deny_current: bool,
  deny_selected: bool,
  cancel_after_current: bool,
  cancel_after_selected: bool,
  order: Arc<Mutex<Vec<&'static str>>>,
}

impl FakeAuthorizer {
  fn standard(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
    Self {
      credential_kind: ReadViewCredentialKindV1::Ordinary,
      concealment: ReadViewConcealmentV1::Conceal,
      current_error: None,
      selected_error: None,
      deny_current: false,
      deny_selected: false,
      cancel_after_current: false,
      cancel_after_selected: false,
      order,
    }
  }
}

impl ReadViewAuthorizerV1 for FakeAuthorizer {
  type CurrentAuthorization = String;
  type ResolvedAuthorization = String;

  fn authorize_current(
    &self,
    cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<Self::CurrentAuthorization>, ReadViewAuthorizationErrorV1> {
    self.order.lock().unwrap().push("current_auth");
    if let Some(error) = &self.current_error {
      return Err(error.clone());
    }
    if self.deny_current {
      return Err(ReadViewAuthorizationErrorV1::denied(self.concealment));
    }
    if self.cancel_after_current {
      cancellation.cancel();
    }
    Ok(CurrentReadAuthorizationV1::new("current".to_string(), self.credential_kind, self.concealment))
  }

  fn restrict_to_selected_root(
    &self,
    current: &Self::CurrentAuthorization,
    _header: &SelectedDatabaseHeaderV4,
    _authority: &LoadedReadAuthorityV1,
    cancellation: &CancellationToken,
  ) -> Result<Self::ResolvedAuthorization, ReadViewAuthorizationFailureV1> {
    self.order.lock().unwrap().push("selected_auth");
    if let Some(error) = &self.selected_error {
      return Err(error.clone());
    }
    if self.deny_selected {
      return Err(ReadViewAuthorizationFailureV1::Denied);
    }
    if self.cancel_after_selected {
      cancellation.cancel();
    }
    Ok(format!("{current}+selected"))
  }
}

#[test]
fn current_authorization_denial_touches_no_header_root_or_lifecycle_source() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x11);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false));
  let mut authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  authorizer.deny_current = true;
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "read_authorization_denied");
  assert_eq!(error.concealment(), Some(ReadViewConcealmentV1::Conceal));
  assert_eq!(source.header_calls.load(Ordering::SeqCst), 0);
  assert_eq!(source.authority_calls.load(Ordering::SeqCst), 0);
  assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 0);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  assert_eq!(*source.order.lock().unwrap(), ["current_auth"]);
}

#[test]
fn current_authorization_operational_and_corruption_failures_touch_no_source() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x0a);
  for (authorization_error, expected_code) in [
    (
      ReadViewAuthorizationErrorV1::unavailable(ReadViewConcealmentV1::Reveal, "permission store unavailable"),
      "read_authorization_unavailable",
    ),
    (ReadViewAuthorizationErrorV1::corrupt(ReadViewConcealmentV1::Conceal, "permission state corrupt"), "read_authorization_corrupt"),
  ] {
    let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false));
    let mut authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
    let expected_concealment = authorization_error.concealment();
    authorizer.current_error = Some(authorization_error);
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pin_coordinator(algorithm), all_capabilities_profile());

    let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

    assert_eq!(error.code(), expected_code);
    assert_eq!(error.concealment(), Some(expected_concealment));
    assert_eq!(source.header_calls.load(Ordering::SeqCst), 0);
    assert_eq!(source.authority_calls.load(Ordering::SeqCst), 0);
    assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 0);
  }
}

#[test]
fn current_head_resolution_captures_header_once_orders_authority_and_owns_pin() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x12);
  let header = selected_header(algorithm, head.clone());
  let source = Arc::new(FakeAuthoritySource::new(header.clone(), head.clone(), false));
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();

  assert_eq!(source.header_calls.load(Ordering::SeqCst), 1);
  assert_eq!(source.authority_calls.load(Ordering::SeqCst), 1);
  assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 1);
  assert_eq!(*source.order.lock().unwrap(), ["current_auth", "header", "lifecycle", "authority", "selected_auth"]);
  assert_eq!(view.database_id(), header.header.database_id);
  assert_eq!(view.physical_instance_id(), header.header.physical_instance_id);
  assert_eq!(view.selected_header_slot(), header.selected_slot);
  assert_eq!(view.header_slot_sequence(), header.header.slot_sequence);
  assert_eq!(view.write_sequence_high_water(), header.header.write_sequence_high_water);
  assert_eq!(view.root_metadata().hash, head);
  assert!(!view.is_explicit_root());
  assert_eq!(view.legacy_root_hash(), None);
  assert_eq!(view.authorization(), "current+selected");
  assert_eq!(view.credential_kind(), ReadViewCredentialKindV1::Ordinary);
  assert_eq!(view.concealment(), ReadViewConcealmentV1::Conceal);
  assert_eq!(view.system_family_registry().operational_fingerprint, header.header.system_family_registry_fingerprint);
  assert!(!view.cancellation().is_cancelled());
  assert_eq!(coordinator.active_pin_count().unwrap(), 1);
  drop(view);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn explicit_unknown_root_never_falls_back_to_head() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x13);
  let explicit = hash(algorithm, 0x14);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false));
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&explicit), &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "invalid_namespace_root");
  assert_eq!(*source.requested_roots.lock().unwrap(), [explicit]);
  assert!(!source.requested_roots.lock().unwrap().contains(&head));
  assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 1);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn share_credentials_can_resolve_only_the_captured_current_head() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x15);
  let historical = hash(algorithm, 0x16);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), historical.clone(), false));
  let mut authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  authorizer.credential_kind = ReadViewCredentialKindV1::Share;
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&historical), &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "share_historical_root_forbidden");
  assert_eq!(source.header_calls.load(Ordering::SeqCst), 1);
  assert_eq!(source.authority_calls.load(Ordering::SeqCst), 0);
  assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 0);

  let current_source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false));
  let current_authorizer = FakeAuthorizer { order: Arc::clone(&current_source.order), ..authorizer };
  let current_resolver = ReadViewResolverV1::new(Arc::clone(&current_source), coordinator.clone(), all_capabilities_profile());
  let current = current_resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&head), &current_authorizer, &CancellationToken::new()).unwrap();
  assert!(current.is_explicit_root());
  drop(current);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn selected_root_denial_happens_under_lifecycle_pin_and_preserves_current_concealment() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x17);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false));
  let mut authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  authorizer.deny_selected = true;
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "read_authorization_denied");
  assert_eq!(error.concealment(), Some(ReadViewConcealmentV1::Conceal));
  assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 1);
  assert_eq!(*source.order.lock().unwrap(), ["current_auth", "header", "lifecycle", "authority", "selected_auth"]);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn selected_root_operational_and_corruption_failures_release_lifecycle_pin() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x0b);
  for (authorization_error, expected_code) in [
    (ReadViewAuthorizationFailureV1::Unavailable("permission store unavailable".to_string()), "read_authorization_unavailable"),
    (ReadViewAuthorizationFailureV1::Corrupt("permission state corrupt".to_string()), "read_authorization_corrupt"),
  ] {
    let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false));
    let mut authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
    authorizer.selected_error = Some(authorization_error);
    let coordinator = pin_coordinator(algorithm);
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

    let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

    assert_eq!(error.code(), expected_code);
    assert_eq!(error.concealment(), Some(ReadViewConcealmentV1::Conceal));
    assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 1);
    assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  }
}

#[test]
fn cancellation_after_current_authorization_stops_before_header_capture() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x18);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false));
  let mut authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  authorizer.cancel_after_current = true;
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "read_view_canceled");
  assert_eq!(source.header_calls.load(Ordering::SeqCst), 0);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn content_only_root_resolves_without_inventing_semantics() {
  let algorithm = HashAlgorithm::Sha512;
  let head = hash(algorithm, 0x19);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, true));
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator, all_capabilities_profile());

  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap();

  assert!(matches!(
    &view.authority().semantic_state.availability,
    SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured }
  ));
}

#[test]
fn current_head_without_complete_admission_is_corruption_not_an_explicit_root_miss() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x1a);
  let mut source = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false);
  source.authority_error = Some(ReadViewSourceErrorV1::RootNotAdmitted);
  let source = Arc::new(source);
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator, all_capabilities_profile());

  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "root_authority_corrupt");
  assert_eq!(source.header_calls.load(Ordering::SeqCst), 1);
  assert_eq!(source.authority_calls.load(Ordering::SeqCst), 1);
  assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_is_observed_at_every_resolver_boundary_before_more_authority_work() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x1b);

  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false));
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());
  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &canceled).unwrap_err();
  assert_eq!(error.code(), "read_view_canceled");
  assert!(source.order.lock().unwrap().is_empty());

  let mut after_header = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
  after_header.cancel_after_header = true;
  let after_header = Arc::new(after_header);
  let authorizer = FakeAuthorizer::standard(Arc::clone(&after_header.order));
  let resolver = ReadViewResolverV1::new(Arc::clone(&after_header), coordinator.clone(), all_capabilities_profile());
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "read_view_canceled");
  assert_eq!(after_header.authority_calls.load(Ordering::SeqCst), 0);

  let mut after_authority = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
  after_authority.cancel_after_authority = true;
  let after_authority = Arc::new(after_authority);
  let authorizer = FakeAuthorizer::standard(Arc::clone(&after_authority.order));
  let resolver = ReadViewResolverV1::new(Arc::clone(&after_authority), coordinator.clone(), all_capabilities_profile());
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "read_view_canceled");
  assert!(!after_authority.order.lock().unwrap().contains(&"selected_auth"));

  let after_selected = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false));
  let mut authorizer = FakeAuthorizer::standard(Arc::clone(&after_selected.order));
  authorizer.cancel_after_selected = true;
  let resolver = ReadViewResolverV1::new(Arc::clone(&after_selected), coordinator.clone(), all_capabilities_profile());
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "read_view_canceled");
  assert_eq!(after_selected.lifecycle_calls.load(Ordering::SeqCst), 1);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn invalid_hash_and_coordinator_algorithm_mismatch_stop_before_authority_lookup() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x1c);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false));
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  for invalid in [vec![1; 31], vec![0; 32]] {
    let error = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&invalid), &authorizer, &CancellationToken::new()).unwrap_err();
    assert_eq!(error.code(), "invalid_root_hash");
  }
  assert_eq!(source.header_calls.load(Ordering::SeqCst), 2);
  assert_eq!(source.authority_calls.load(Ordering::SeqCst), 0);

  let sha_head = hash(HashAlgorithm::Sha512, 0x1d);
  let mismatch = Arc::new(FakeAuthoritySource::new(selected_header(HashAlgorithm::Sha512, sha_head.clone()), sha_head, false));
  let mismatch_authorizer = FakeAuthorizer::standard(Arc::clone(&mismatch.order));
  let mismatch_resolver = ReadViewResolverV1::new(Arc::clone(&mismatch), coordinator, all_capabilities_profile());
  let error = mismatch_resolver.resolve(ReadViewSelectorV1::CurrentHead, &mismatch_authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "read_view_coordinator_mismatch");
  assert_eq!(mismatch.authority_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn source_failures_remain_distinct_and_carry_current_concealment() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x1e);
  for (source_error, expected_code, expected_authority_calls) in [
    (ReadViewSourceErrorV1::HeaderUnavailable("offline".to_string()), "database_header_unavailable", 0),
    (ReadViewSourceErrorV1::HeaderCorrupt("bad slots".to_string()), "database_header_corrupt", 0),
    (ReadViewSourceErrorV1::AuthorityUnavailable("offline".to_string()), "root_authority_unavailable", 1),
    (ReadViewSourceErrorV1::AuthorityCorrupt("bad closure".to_string()), "root_authority_corrupt", 1),
  ] {
    let mut source = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
    if expected_authority_calls == 0 {
      source.header_error = Some(source_error);
    } else {
      source.authority_error = Some(source_error);
    }
    let source = Arc::new(source);
    let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), pin_coordinator(algorithm), all_capabilities_profile());

    let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

    assert_eq!(error.code(), expected_code);
    assert_eq!(error.concealment(), Some(ReadViewConcealmentV1::Conceal));
    assert_eq!(source.authority_calls.load(Ordering::SeqCst), expected_authority_calls);
    assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), usize::from(expected_authority_calls != 0));
  }
}

#[test]
fn defensive_closure_and_capability_errors_precede_selected_authorization() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x1f);
  let mut malformed = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
  malformed.authority.root.root_hash = hash(algorithm, 0x20);
  let malformed = Arc::new(malformed);
  let mut denied = FakeAuthorizer::standard(Arc::clone(&malformed.order));
  denied.deny_selected = true;
  let resolver = ReadViewResolverV1::new(Arc::clone(&malformed), pin_coordinator(algorithm), all_capabilities_profile());
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &denied, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "root_authority_corrupt");
  assert!(!malformed.order.lock().unwrap().contains(&"selected_auth"));
  assert_eq!(malformed.lifecycle_calls.load(Ordering::SeqCst), 1);

  let allowed = FakeAuthorizer::standard(Arc::clone(&malformed.order));
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &allowed, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "root_authority_corrupt");
  assert!(!malformed.order.lock().unwrap().contains(&"selected_auth"));
  assert_eq!(malformed.lifecycle_calls.load(Ordering::SeqCst), 2);

  let mut unsupported = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false);
  unsupported.authority.root.required_capabilities = CapabilitySetV1::from_bits([23]).unwrap().into_bytes();
  let unsupported = Arc::new(unsupported);
  let authorizer = FakeAuthorizer::standard(Arc::clone(&unsupported.order));
  let supported_readers = CapabilitySetV1::from_bits(0..23).unwrap();
  let profile = BinaryCapabilityProfileV1::new(supported_readers, all_capabilities_profile().supported_writer_capabilities);
  let resolver = ReadViewResolverV1::new(Arc::clone(&unsupported), pin_coordinator(algorithm), profile);
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "unsupported_root_capabilities");
  assert!(!unsupported.order.lock().unwrap().contains(&"selected_auth"));
  assert_eq!(unsupported.lifecycle_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn captured_header_admission_fails_before_root_lookup_and_preserves_concealment() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x26);
  let source = Arc::new(FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false));
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let unsupported_profile = BinaryCapabilityProfileV1::new(CapabilitySetV1::empty(), CapabilitySetV1::empty());
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), pin_coordinator(algorithm), unsupported_profile);

  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

  assert_eq!(error.code(), "missing_reader_capabilities");
  assert_eq!(error.concealment(), Some(ReadViewConcealmentV1::Conceal));
  assert_eq!(source.header_calls.load(Ordering::SeqCst), 1);
  assert_eq!(source.authority_calls.load(Ordering::SeqCst), 0);
  assert_eq!(source.lifecycle_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn authority_high_water_and_legacy_mapping_corruption_fail_before_selected_authorization() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x27);
  let mut future = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
  future.authority.admission.publication_sequence = future.header.header.write_sequence_high_water + 1;
  let future = Arc::new(future);
  let authorizer = FakeAuthorizer::standard(Arc::clone(&future.order));
  let resolver = ReadViewResolverV1::new(Arc::clone(&future), pin_coordinator(algorithm), all_capabilities_profile());
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "root_authority_corrupt");
  assert!(!future.order.lock().unwrap().contains(&"selected_auth"));
  assert_eq!(future.lifecycle_calls.load(Ordering::SeqCst), 1);

  let mut legacy = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head, false);
  legacy.legacy_root_hash = Some(vec![0; algorithm.hash_length()]);
  let legacy = Arc::new(legacy);
  let authorizer = FakeAuthorizer::standard(Arc::clone(&legacy.order));
  let resolver = ReadViewResolverV1::new(Arc::clone(&legacy), pin_coordinator(algorithm), all_capabilities_profile());
  let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.code(), "root_authority_corrupt");
  assert!(!legacy.order.lock().unwrap().contains(&"selected_auth"));
  assert_eq!(legacy.lifecycle_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn lifecycle_state_and_source_error_matrix_never_leaks_a_pin() {
  let algorithm = HashAlgorithm::Blake3_256;
  let head = hash(algorithm, 0x21);
  for (lifecycle, expected_code) in [
    (RootLifecycleObservationV1::LogicallyRetired, "root_expired"),
    (RootLifecycleObservationV1::PhysicallyReclaimed, "root_expired"),
    (RootLifecycleObservationV1::UnknownOrUnadmitted, "invalid_namespace_root"),
    (RootLifecycleObservationV1::Corrupt, "root_lifecycle_corrupt"),
    (RootLifecycleObservationV1::Unavailable, "root_lifecycle_unavailable"),
  ] {
    let mut source = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
    source.lifecycle = lifecycle;
    let source = Arc::new(source);
    let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
    let coordinator = pin_coordinator(algorithm);
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

    let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

    assert_eq!(error.code(), expected_code);
    assert_eq!(source.authority_calls.load(Ordering::SeqCst), 0);
    assert_eq!(coordinator.active_pin_count().unwrap(), 0);
    assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
  }

  for (lifecycle_error, expected_code) in [
    (ReadViewLifecycleErrorV1::Corrupt("bad lifecycle".to_string()), "root_lifecycle_corrupt"),
    (ReadViewLifecycleErrorV1::Unavailable("offline".to_string()), "root_lifecycle_unavailable"),
    (ReadViewLifecycleErrorV1::Canceled, "read_view_canceled"),
  ] {
    let mut source = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
    source.lifecycle_error = Some(lifecycle_error);
    let source = Arc::new(source);
    let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
    let coordinator = pin_coordinator(algorithm);
    let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

    let error = resolver.resolve(ReadViewSelectorV1::CurrentHead, &authorizer, &CancellationToken::new()).unwrap_err();

    assert_eq!(error.code(), expected_code);
    assert_eq!(source.authority_calls.load(Ordering::SeqCst), 0);
    assert_eq!(coordinator.active_pin_count().unwrap(), 0);
    assert_eq!(coordinator.tracked_root_count().unwrap(), 0);
  }
}

#[test]
fn pending_root_metadata_and_legacy_mapping_are_advisory_and_owned_by_the_view() {
  let algorithm = HashAlgorithm::Sha512;
  let head = hash(algorithm, 0x24);
  let legacy = hash(algorithm, 0x25);
  let mut source = FakeAuthoritySource::new(selected_header(algorithm, head.clone()), head.clone(), false);
  source.lifecycle =
    RootLifecycleObservationV1::PendingDelete { pending_since_ms: 10_000, grace_at_pending_ms: 1_000, current_configured_grace_ms: 2_000 };
  source.legacy_root_hash = Some(legacy.clone());
  let source = Arc::new(source);
  let authorizer = FakeAuthorizer::standard(Arc::clone(&source.order));
  let coordinator = pin_coordinator(algorithm);
  let resolver = ReadViewResolverV1::new(Arc::clone(&source), coordinator.clone(), all_capabilities_profile());

  let view = resolver.resolve(ReadViewSelectorV1::ExplicitRoot(&head), &authorizer, &CancellationToken::new()).unwrap();

  assert_eq!(view.root_metadata().expires_at_ms, Some(12_000));
  assert_eq!(view.legacy_root_hash(), Some(legacy.as_slice()));
  assert_eq!(coordinator.active_pin_count().unwrap(), 1);
  drop(view);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);
}

#[test]
fn read_view_service_remains_disconnected_from_routes_and_ordinary_storage_callers() {
  fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        collect_rust_files(&path, files);
      } else if path.extension().is_some_and(|extension| extension == "rs") {
        files.push(path);
      }
    }
  }

  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let read_view_path = source_root.join("engine/v4/read_view.rs");
  let mut files = Vec::new();
  collect_rust_files(&source_root, &mut files);
  let resolver_owners: Vec<_> = files
    .iter()
    .filter(|path| *path != &read_view_path)
    .filter(|path| fs::read_to_string(path).unwrap().contains("ReadViewResolverV1"))
    .collect();
  assert!(resolver_owners.is_empty(), "read-view resolver gained a production caller before Child 06 activation: {resolver_owners:?}");

  let source = fs::read_to_string(read_view_path).unwrap();
  for forbidden in ["crate::server", "DirectoryOps", "StorageEngine", "axum::", "Router<", "route("] {
    assert!(!source.contains(forbidden), "disconnected P3b-3c read-view service unexpectedly contains {forbidden}");
  }
}
