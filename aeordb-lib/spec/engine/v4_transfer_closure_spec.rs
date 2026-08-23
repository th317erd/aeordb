use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::database_header::{SelectedDatabaseHeaderV4, decode_header_region};
use aeordb::engine::v4::namespace::{
  NamespaceRootV1, NamespaceTreeLayoutV0, NamespaceTreeRootV0, SemanticAvailabilityV1, SemanticStateV1, SemanticUnavailableReasonV1,
};
use aeordb::engine::v4::read_view::{
  CurrentReadAuthorizationV1, LoadedReadAuthorityV1, ReadViewAuthoritySourceV1, ReadViewAuthorizationErrorV1,
  ReadViewAuthorizationFailureV1, ReadViewAuthorizerV1, ReadViewConcealmentV1, ReadViewCredentialKindV1, ReadViewLifecycleErrorV1,
  ReadViewResolverV1, ReadViewSelectorV1, ReadViewSourceErrorV1, ResolvedReadViewV1, RootLifecycleObservationV1, RootReadPinCoordinatorV1,
};
use aeordb::engine::v4::root_authority::{
  ImmutableNamespaceAuthorityV1, RootAdmissionCommitV1, RootAuthorityKindV1, RootAuthorityReferenceRoleV1,
};
use aeordb::engine::v4::system_family::{SystemFamilySubjectV1, SystemFamilyTransferOperationV1, TransferPolicyV1};
use aeordb::engine::v4::transfer_closure::{
  TransferClosureClassifierV1, TransferClosureCompletionV1, TransferClosureDecisionV1, TransferClosureErrorV1, TransferClosureItemV1,
};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/database-header-v4")
}

fn hash(algorithm: HashAlgorithm, byte: u8) -> Vec<u8> {
  vec![byte; algorithm.hash_length()]
}

fn selected_header(algorithm: HashAlgorithm, head_hash: Vec<u8>) -> SelectedDatabaseHeaderV4 {
  let fixture = match algorithm {
    HashAlgorithm::Blake3_256 => "header-blake3-256-valid-ab.bin",
    HashAlgorithm::Sha512 => "header-sha512-valid-ab.bin",
    _ => panic!("transfer-closure tests use only frozen Blake3-256 and SHA-512 headers"),
  };
  let mut selected = decode_header_region(&fs::read(fixture_root().join(fixture)).unwrap()).unwrap();
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
    SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyDependencyCannotBeProven }
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

struct Source {
  header: SelectedDatabaseHeaderV4,
  authority: ImmutableNamespaceAuthorityV1,
}

impl ReadViewAuthoritySourceV1 for Source {
  fn capture_header(&self, _cancellation: &CancellationToken) -> Result<SelectedDatabaseHeaderV4, ReadViewSourceErrorV1> {
    Ok(self.header.clone())
  }

  fn load_verified_authority(
    &self,
    _header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    _cancellation: &CancellationToken,
  ) -> Result<LoadedReadAuthorityV1, ReadViewSourceErrorV1> {
    if root_hash != self.authority.root.root_hash {
      return Err(ReadViewSourceErrorV1::RootNotAdmitted);
    }
    Ok(LoadedReadAuthorityV1 { authority: self.authority.clone(), legacy_root_hash: None })
  }

  fn observe_lifecycle(
    &self,
    _header: &SelectedDatabaseHeaderV4,
    _root_hash: &[u8],
    _cancellation: &CancellationToken,
  ) -> Result<RootLifecycleObservationV1, ReadViewLifecycleErrorV1> {
    Ok(RootLifecycleObservationV1::Live)
  }
}

struct Authorizer;

impl ReadViewAuthorizerV1 for Authorizer {
  type CurrentAuthorization = ();
  type ResolvedAuthorization = ();

  fn authorize_current(
    &self,
    _cancellation: &CancellationToken,
  ) -> Result<CurrentReadAuthorizationV1<Self::CurrentAuthorization>, ReadViewAuthorizationErrorV1> {
    Ok(CurrentReadAuthorizationV1::new((), ReadViewCredentialKindV1::Ordinary, ReadViewConcealmentV1::Conceal))
  }

  fn restrict_to_selected_root(
    &self,
    _current: &Self::CurrentAuthorization,
    _authority: &LoadedReadAuthorityV1,
    _cancellation: &CancellationToken,
  ) -> Result<Self::ResolvedAuthorization, ReadViewAuthorizationFailureV1> {
    Ok(())
  }
}

fn resolved_view(algorithm: HashAlgorithm, content_only: bool) -> (ResolvedReadViewV1<()>, RootReadPinCoordinatorV1, CancellationToken) {
  let head = hash(algorithm, 0x11);
  let header = selected_header(algorithm, head.clone());
  let source = Arc::new(Source { authority: authority(&header, head, content_only), header });
  let memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(8 * 1_024 * 1_024, 16 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()));
  let coordinator = RootReadPinCoordinatorV1::new(memory, algorithm, 8, 16).unwrap();
  let capabilities = CapabilitySetV1::from_bits(0..24).unwrap();
  let resolver = ReadViewResolverV1::new(source, coordinator.clone(), BinaryCapabilityProfileV1::new(capabilities, capabilities));
  let cancellation = CancellationToken::new();
  let view = resolver.resolve(ReadViewSelectorV1::CurrentHead, &Authorizer, &cancellation).unwrap();
  (view, coordinator, cancellation)
}

fn authority_edges(view: &ResolvedReadViewV1<()>) -> Vec<(RootAuthorityReferenceRoleV1, Vec<u8>)> {
  vec![
    (RootAuthorityReferenceRoleV1::NamespaceRoot, view.authority().root.root_hash.clone()),
    (RootAuthorityReferenceRoleV1::NamespaceTreeRoot, view.authority().root.namespace_tree_root.clone()),
    (RootAuthorityReferenceRoleV1::SemanticStateRoot, view.authority().root.semantic_state_root.clone()),
    (RootAuthorityReferenceRoleV1::RootAdmissionCommit, view.authority().admission.namespace_root.clone()),
  ]
}

fn admit_authority_prefix(classifier: &mut TransferClosureClassifierV1<'_>, edges: &[(RootAuthorityReferenceRoleV1, Vec<u8>)]) {
  for (role, identity) in edges {
    assert_eq!(
      classifier.classify(TransferClosureItemV1::AuthorityEdge { role: *role, identity }).unwrap(),
      TransferClosureDecisionV1::RequiredAuthority { role: *role },
    );
  }
}

#[test]
fn exact_authority_prefix_streams_operation_policy_without_collecting_the_closure() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let (view, coordinator, _) = resolved_view(algorithm, false);
    let edges = authority_edges(&view);
    let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
    admit_authority_prefix(&mut classifier, &edges);

    assert_eq!(
      classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/docs/readme.md"))).unwrap(),
      TransferClosureDecisionV1::IncludeOrdinary,
    );
    assert_eq!(
      classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/docs/.aeordb-permissions"))).unwrap(),
      TransferClosureDecisionV1::IncludeKnown { family_id: 0x0019, policy: TransferPolicyV1::RequiredInclude },
    );
    assert_eq!(
      classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/.aeordb-system"))).unwrap(),
      TransferClosureDecisionV1::TraverseStructuralContainer,
    );
    assert_eq!(
      classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/docs/.aeordb-indexes/text.idx"))).unwrap(),
      TransferClosureDecisionV1::OmitKnown { family_id: 0x0060, policy: TransferPolicyV1::OmitDeclared },
    );

    let summary = classifier.finish().unwrap();
    assert_eq!(summary.operation, SystemFamilyTransferOperationV1::LogicalBackup);
    assert_eq!(summary.processed_items, 8);
    assert_eq!(summary.required_authority_edges, 4);
    assert_eq!(summary.included_items, 2);
    assert_eq!(summary.omitted_items, 1);
    assert_eq!(summary.structural_containers, 1);
    assert_eq!(summary.completion, TransferClosureCompletionV1::Complete);
    assert_eq!(coordinator.active_pin_count().unwrap(), 1);
    drop(view);
    assert_eq!(coordinator.active_pin_count().unwrap(), 0);
  }
}

#[test]
fn content_only_roots_finish_as_explicit_data_only_closures() {
  let (view, _, _) = resolved_view(HashAlgorithm::Blake3_256, true);
  let edges = authority_edges(&view);
  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 4).unwrap();
  admit_authority_prefix(&mut classifier, &edges);
  assert_eq!(
    classifier.finish().unwrap().completion,
    TransferClosureCompletionV1::DataOnly { reason: SemanticUnavailableReasonV1::LegacyDependencyCannotBeProven },
  );
}

#[test]
fn authority_edges_are_exact_complete_unique_and_precede_payload() {
  let (view, _, _) = resolved_view(HashAlgorithm::Blake3_256, false);
  let edges = authority_edges(&view);

  let classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
  assert_eq!(classifier.finish().unwrap_err().code(), "transfer_closure_authority_incomplete");

  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
  assert_eq!(
    classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/docs/readme.md"))).unwrap_err().code(),
    "transfer_closure_authority_incomplete",
  );
  assert_eq!(
    classifier.classify(TransferClosureItemV1::AuthorityEdge { role: edges[0].0, identity: &edges[0].1 }).unwrap_err().code(),
    "transfer_closure_failed",
  );

  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
  let wrong = hash(HashAlgorithm::Blake3_256, 0x99);
  assert_eq!(
    classifier.classify(TransferClosureItemV1::AuthorityEdge { role: edges[0].0, identity: &wrong }).unwrap_err().code(),
    "transfer_closure_authority_mismatch",
  );

  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
  classifier.classify(TransferClosureItemV1::AuthorityEdge { role: edges[0].0, identity: &edges[0].1 }).unwrap();
  assert_eq!(
    classifier.classify(TransferClosureItemV1::AuthorityEdge { role: edges[0].0, identity: &edges[0].1 }).unwrap_err().code(),
    "transfer_closure_authority_duplicate",
  );

  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
  admit_authority_prefix(&mut classifier, &edges);
  classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/docs/readme.md"))).unwrap();
  assert_eq!(
    classifier.classify(TransferClosureItemV1::AuthorityEdge { role: edges[0].0, identity: &edges[0].1 }).unwrap_err().code(),
    "transfer_closure_authority_after_payload",
  );
}

#[test]
fn operation_specific_registry_policy_covers_every_storage_subject_domain() {
  let (view, _, _) = resolved_view(HashAlgorithm::Blake3_256, false);
  let edges = authority_edges(&view);

  let mut backup = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 10).unwrap();
  admit_authority_prefix(&mut backup, &edges);
  for (subject, expected) in [
    (
      SystemFamilySubjectV1::EntryType(9),
      TransferClosureDecisionV1::IncludeKnown { family_id: 0x0050, policy: TransferPolicyV1::OptionalValidated },
    ),
    (
      SystemFamilySubjectV1::KvKey(b"aeordb.task.v1\0job"),
      TransferClosureDecisionV1::OmitKnown { family_id: 0x0042, policy: TransferPolicyV1::OmitDeclared },
    ),
    (
      SystemFamilySubjectV1::ControlTag(2),
      TransferClosureDecisionV1::OmitKnown { family_id: 0x0042, policy: TransferPolicyV1::OmitDeclared },
    ),
    (
      SystemFamilySubjectV1::ExternalWorkspaceKind(2),
      TransferClosureDecisionV1::OmitKnown { family_id: 0x0071, policy: TransferPolicyV1::OmitDeclared },
    ),
  ] {
    assert_eq!(backup.classify(TransferClosureItemV1::StorageSubject(subject)).unwrap(), expected);
  }
  backup.finish().unwrap();

  let mut logical = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 5).unwrap();
  admit_authority_prefix(&mut logical, &edges);
  assert_eq!(
    logical.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/.aeordb-conflicts/item.json"))).unwrap(),
    TransferClosureDecisionV1::IncludeKnown { family_id: 0x001a, policy: TransferPolicyV1::RequiredInclude },
  );

  let mut peer = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::PeerReplication, 5).unwrap();
  admit_authority_prefix(&mut peer, &edges);
  assert_eq!(
    peer.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/.aeordb-conflicts/item.json"))).unwrap(),
    TransferClosureDecisionV1::OmitKnown { family_id: 0x001a, policy: TransferPolicyV1::OmitDeclared },
  );

  let mut redacted = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 5).unwrap();
  admit_authority_prefix(&mut redacted, &edges);
  assert_eq!(
    redacted.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/.aeordb-system/api-keys/key.json"))).unwrap(),
    TransferClosureDecisionV1::OmitKnown { family_id: 0x0013, policy: TransferPolicyV1::RedactOmit },
  );

  let mut node_local = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::PeerReplication, 5).unwrap();
  admit_authority_prefix(&mut node_local, &edges);
  assert_eq!(
    node_local.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/.aeordb-system/api-keys/key.json"))).unwrap(),
    TransferClosureDecisionV1::OmitKnown { family_id: 0x0013, policy: TransferPolicyV1::NodeLocal },
  );

  let mut named_subset = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::ClusterJoin, 5).unwrap();
  admit_authority_prefix(&mut named_subset, &edges);
  assert_eq!(
    named_subset
      .classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/.aeordb-system/config/settings.json")))
      .unwrap(),
    TransferClosureDecisionV1::OmitKnown { family_id: 0x0016, policy: TransferPolicyV1::NamedSubsetOnly },
  );

  for (operation, expected_policy) in [
    (SystemFamilyTransferOperationV1::PhysicalCopy, TransferPolicyV1::RequiredInclude),
    (SystemFamilyTransferOperationV1::LogicalBackup, TransferPolicyV1::RequiredInclude),
    (SystemFamilyTransferOperationV1::DataExport, TransferPolicyV1::RequiredInclude),
    (SystemFamilyTransferOperationV1::PeerReplication, TransferPolicyV1::RequiredInclude),
    (SystemFamilyTransferOperationV1::ClusterJoin, TransferPolicyV1::OmitDeclared),
    (SystemFamilyTransferOperationV1::ClientSync, TransferPolicyV1::RequiredInclude),
    (SystemFamilyTransferOperationV1::Import, TransferPolicyV1::RequiredInclude),
  ] {
    let mut classifier = TransferClosureClassifierV1::for_read_view(&view, operation, 5).unwrap();
    admit_authority_prefix(&mut classifier, &edges);
    let decision =
      classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/docs/.aeordb-permissions"))).unwrap();
    let expected = if expected_policy == TransferPolicyV1::OmitDeclared {
      TransferClosureDecisionV1::OmitKnown { family_id: 0x0019, policy: expected_policy }
    } else {
      TransferClosureDecisionV1::IncludeKnown { family_id: 0x0019, policy: expected_policy }
    };
    assert_eq!(decision, expected, "operation {operation:?}");
  }
}

#[test]
fn unknown_protected_state_fails_closed_in_every_protected_subject_domain() {
  let (view, _, _) = resolved_view(HashAlgorithm::Blake3_256, false);
  let edges = authority_edges(&view);
  for subject in [
    SystemFamilySubjectV1::Path("/docs/.aeordb-future/value"),
    SystemFamilySubjectV1::KvKey(b"aeordb.future.v1\0value"),
    SystemFamilySubjectV1::ControlTag(u16::MAX),
    SystemFamilySubjectV1::ExternalWorkspaceKind(u16::MAX),
  ] {
    let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 5).unwrap();
    admit_authority_prefix(&mut classifier, &edges);
    assert_eq!(classifier.classify(TransferClosureItemV1::StorageSubject(subject)).unwrap_err().code(), "unknown_protected_system_family",);
    assert_eq!(classifier.finish().unwrap_err().code(), "transfer_closure_failed");
  }

  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 5).unwrap();
  admit_authority_prefix(&mut classifier, &edges);
  assert_eq!(
    classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("relative/path"))).unwrap_err().code(),
    "system_family_absolute_path",
  );
  assert_eq!(classifier.finish().unwrap_err().code(), "transfer_closure_failed");
}

#[test]
fn item_limits_and_request_cancellation_are_terminal_and_leak_no_state() {
  let (view, coordinator, cancellation) = resolved_view(HashAlgorithm::Blake3_256, false);
  let edges = authority_edges(&view);
  assert_eq!(
    TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 3).unwrap_err().code(),
    "transfer_closure_limit_too_small",
  );

  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 4).unwrap();
  admit_authority_prefix(&mut classifier, &edges);
  assert_eq!(
    classifier.classify(TransferClosureItemV1::StorageSubject(SystemFamilySubjectV1::Path("/docs/readme.md"))).unwrap_err().code(),
    "transfer_closure_item_limit",
  );
  assert_eq!(classifier.finish().unwrap_err().code(), "transfer_closure_failed");

  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
  cancellation.cancel();
  assert_eq!(
    classifier.classify(TransferClosureItemV1::AuthorityEdge { role: edges[0].0, identity: &edges[0].1 }).unwrap_err().code(),
    "transfer_closure_canceled",
  );
  assert_eq!(classifier.finish().unwrap_err().code(), "transfer_closure_failed");
  assert_eq!(coordinator.active_pin_count().unwrap(), 1);
  drop(view);
  assert_eq!(coordinator.active_pin_count().unwrap(), 0);

  let (view, _, cancellation) = resolved_view(HashAlgorithm::Blake3_256, false);
  cancellation.cancel();
  assert_eq!(
    TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap_err().code(),
    "transfer_closure_canceled",
  );

  let (view, _, cancellation) = resolved_view(HashAlgorithm::Blake3_256, false);
  let edges = authority_edges(&view);
  let mut classifier = TransferClosureClassifierV1::for_read_view(&view, SystemFamilyTransferOperationV1::LogicalBackup, 8).unwrap();
  admit_authority_prefix(&mut classifier, &edges);
  cancellation.cancel();
  assert_eq!(classifier.finish().unwrap_err().code(), "transfer_closure_canceled");
}

#[test]
fn transfer_classifier_remains_disconnected_from_production_transfer_callers() {
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
  let classifier_path = source_root.join("engine/v4/transfer_closure.rs");
  let mut files = Vec::new();
  collect_rust_files(&source_root, &mut files);
  let owners: Vec<_> = files
    .iter()
    .filter(|path| *path != &classifier_path)
    .filter(|path| fs::read_to_string(path).unwrap().contains("TransferClosureClassifierV1"))
    .collect();
  assert!(owners.is_empty(), "transfer classifier gained a production caller before Child 06/P7 activation: {owners:?}");

  let source = fs::read_to_string(classifier_path).unwrap();
  for forbidden in ["crate::server", "DirectoryOps", "StorageEngine", "engine::system_family_policy", "Vec<TransferClosure"] {
    assert!(!source.contains(forbidden), "disconnected P3b-3d classifier unexpectedly contains {forbidden}");
  }
  assert!(source.contains("SystemFamilyPolicyResolverV1"));
  assert!(source.contains("TransferPolicyV1::FailUnknown"));
  assert!(source.contains("TransferClosureErrorV1::TransferRefused"));

  let refusal = TransferClosureErrorV1::TransferRefused { family_id: 0xfffe, operation: SystemFamilyTransferOperationV1::LogicalBackup };
  assert_eq!(refusal.code(), "system_family_transfer_refused");
  assert!(refusal.to_string().contains("0xfffe"));
}
