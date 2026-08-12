use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc::{
  GcActiveControlWriteV1, GcArtifactKindV1, decode_gc_active_control, decode_gc_artifact_envelope, encode_gc_active_control,
  immutable_gc_artifact_key,
};
use aeordb::engine::v4::gc_maintenance::{
  GcActiveControlObservationV1, GcArtifactInspectionClassV1, GcArtifactObservationV1, GcAuthoritativeCorruptionObservationV1,
  GcCorruptionScopeV1, GcDestinationStateV1, GcMaintenanceInspectionLimitsV1, GcMaintenanceInspectionOperationV1,
  GcMaintenanceInspectionSourceV1, GcMaintenanceObservationVisitorV1, GcRepairScopeV1, GcTransferDispositionV1,
  GcWorkspaceInspectionClassV1, GcWorkspaceObservationV1, inspect_gc_artifact_v1, inspect_gc_workspace_v1,
};
use aeordb::engine::v4::gc_run::{
  GcRunBasisV1, GcRunBudgetsV1, GcRunContextV1, GcRunErrorV1, GcRunIDV1, GcRunInvocationV1, GcRunModeV1, GcRunStateV1,
  NoopGcRunProgressSinkV1, execute_gc_run_v1,
};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn fixture_algorithm(path: &Path) -> HashAlgorithm {
  let name = path.file_name().unwrap().to_str().unwrap();
  if name.contains("sha512") {
    HashAlgorithm::Sha512
  } else {
    HashAlgorithm::Blake3_256
  }
}

fn binary_fixtures(directory: &str) -> Vec<PathBuf> {
  let mut fixtures: Vec<_> = fs::read_dir(fixture_root().join(directory))
    .unwrap()
    .map(|entry| entry.unwrap().path())
    .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("bin"))
    .collect();
  fixtures.sort();
  fixtures
}

#[test]
fn one_inspector_deeply_decodes_every_frozen_gc_artifact_at_both_hash_widths() {
  let mut kinds = BTreeSet::new();
  let mut controls = 0;
  let mut immutable = 0;

  for path in binary_fixtures("gc-artifact-v1") {
    let algorithm = fixture_algorithm(&path);
    let bytes = fs::read(&path).unwrap();
    let envelope = decode_gc_artifact_envelope(&bytes).unwrap();
    let expected_key = if envelope.kind.is_control() {
      decode_gc_active_control(&bytes, algorithm).unwrap().key
    } else {
      immutable_gc_artifact_key(algorithm, envelope.kind, &bytes)
    };
    let inspected = inspect_gc_artifact_v1(algorithm, &expected_key, &bytes).unwrap_or_else(|error| {
      panic!("{} failed deep inspection: {error}", path.display());
    });
    assert_eq!(inspected.kind, envelope.kind, "{}", path.display());
    assert_eq!(inspected.generation, envelope.generation, "{}", path.display());
    assert_eq!(inspected.canonical_key, expected_key, "{}", path.display());
    if envelope.kind.is_control() {
      assert_eq!(inspected.class, GcArtifactInspectionClassV1::ActiveControl);
      controls += 1;
    } else {
      assert_eq!(inspected.class, GcArtifactInspectionClassV1::ImmutableArtifact);
      immutable += 1;
    }
    kinds.insert(envelope.kind as u16);
  }

  assert_eq!(kinds, GcArtifactKindV1::ALL.into_iter().map(|kind| kind as u16).collect());
  assert_eq!(controls, 24);
  assert_eq!(immutable, 92);
}

#[test]
fn workspace_inspection_covers_every_manifest_and_bulk_object_fixture() {
  let manifests = binary_fixtures("gc-mark-workspace-manifest-v1");
  let objects = binary_fixtures("gc-mark-workspace-object-v1");
  assert_eq!(manifests.len(), 4);
  assert_eq!(objects.len(), 12);

  for path in manifests {
    let inspected = inspect_gc_workspace_v1(fixture_algorithm(&path), &fs::read(&path).unwrap()).unwrap();
    assert_eq!(inspected.class, GcWorkspaceInspectionClassV1::Manifest, "{}", path.display());
  }
  for path in objects {
    let inspected = inspect_gc_workspace_v1(fixture_algorithm(&path), &fs::read(&path).unwrap()).unwrap();
    assert_eq!(inspected.class, GcWorkspaceInspectionClassV1::Object, "{}", path.display());
  }
}

#[test]
fn malformed_wrong_key_and_cross_family_values_fail_without_shallow_success() {
  let path = fixture_root().join("gc-artifact-v1/agca-blake3-256-void-catalog-source.bin");
  let bytes = fs::read(path).unwrap();
  let envelope = decode_gc_artifact_envelope(&bytes).unwrap();
  let key = immutable_gc_artifact_key(HashAlgorithm::Blake3_256, envelope.kind, &bytes);

  let mut wrong_key = key.clone();
  wrong_key[0] ^= 0x80;
  assert_eq!(inspect_gc_artifact_v1(HashAlgorithm::Blake3_256, &wrong_key, &bytes).unwrap_err().code(), "gc_inspection_key_mismatch");

  assert!(inspect_gc_artifact_v1(HashAlgorithm::Blake3_256, &key, &bytes[..31]).is_err());
  let mut corrupt = bytes.clone();
  *corrupt.last_mut().unwrap() ^= 1;
  assert!(inspect_gc_artifact_v1(HashAlgorithm::Blake3_256, &key, &corrupt).is_err());
  let mut unknown = bytes.clone();
  unknown[6..8].copy_from_slice(&0xffff_u16.to_le_bytes());
  assert!(inspect_gc_artifact_v1(HashAlgorithm::Blake3_256, &key, &unknown).is_err());

  assert!(inspect_gc_workspace_v1(HashAlgorithm::Blake3_256, &bytes).is_err());
  let workspace = fs::read(fixture_root().join("gc-mark-workspace-object-v1/agwo-blake3-256-bitmap-valid.bin")).unwrap();
  assert!(inspect_gc_artifact_v1(HashAlgorithm::Blake3_256, &key, &workspace).is_err());
}

#[derive(Clone)]
struct OwnedControl {
  kind: GcArtifactKindV1,
  slot: u8,
  key: Vec<u8>,
  bytes: Vec<u8>,
}

#[derive(Clone)]
struct OwnedArtifact {
  key: Vec<u8>,
  bytes: Vec<u8>,
}

#[derive(Clone)]
struct OwnedCorruption {
  scope: GcCorruptionScopeV1,
  code: String,
  root_hash: Option<Vec<u8>>,
  path_digest: Option<Vec<u8>>,
  evidence_keys: Vec<Vec<u8>>,
}

struct FixtureSource {
  basis: GcRunBasisV1,
  controls: Vec<OwnedControl>,
  artifacts: Vec<OwnedArtifact>,
  workspaces: Vec<Vec<u8>>,
  corruptions: Vec<OwnedCorruption>,
  cancel_after_first_artifact: bool,
}

impl GcMaintenanceInspectionSourceV1 for FixtureSource {
  fn capture_basis(&mut self, _cancellation: &CancellationToken) -> Result<GcRunBasisV1, GcRunErrorV1> {
    Ok(self.basis.clone())
  }

  fn visit_active_controls(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1> {
    for control in &self.controls {
      visitor.observe_active_control(
        GcActiveControlObservationV1 {
          expected_kind: control.kind,
          expected_slot: control.slot,
          expected_key: &control.key,
          bytes: &control.bytes,
        },
        cancellation,
      )?;
    }
    Ok(())
  }

  fn visit_immutable_artifacts(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1> {
    for (index, artifact) in self.artifacts.iter().enumerate() {
      visitor.observe_immutable_artifact(GcArtifactObservationV1 { expected_key: &artifact.key, bytes: &artifact.bytes }, cancellation)?;
      if self.cancel_after_first_artifact && index == 0 {
        cancellation.cancel();
      }
    }
    Ok(())
  }

  fn visit_workspaces(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1> {
    for workspace in &self.workspaces {
      visitor.observe_workspace(GcWorkspaceObservationV1 { bytes: workspace }, cancellation)?;
    }
    Ok(())
  }

  fn visit_authoritative_corruption(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1> {
    for corruption in &self.corruptions {
      let evidence: Vec<_> = corruption.evidence_keys.iter().map(Vec::as_slice).collect();
      visitor.observe_authoritative_corruption(
        GcAuthoritativeCorruptionObservationV1 {
          scope: corruption.scope,
          code: &corruption.code,
          root_hash: corruption.root_hash.as_deref(),
          path_digest: corruption.path_digest.as_deref(),
          evidence_keys: &evidence,
        },
        cancellation,
      )?;
    }
    Ok(())
  }
}

fn basis(algorithm: HashAlgorithm, database_id: [u8; 16]) -> GcRunBasisV1 {
  let hash_width = algorithm.hash_length();
  GcRunBasisV1 {
    hash_algorithm: algorithm,
    database_id,
    generation: 7,
    authority_root_set_digest: vec![0x21; hash_width],
    semantic_state_digest: vec![0x22; hash_width],
    kv_layout_generation: 3,
    kv_layout_fingerprint: vec![0x23; hash_width],
    effective_policy_fingerprint: [0x24; 32],
    system_family_registry_fingerprint: [0x25; 32],
    captured_header_sequence: 8,
    captured_write_high_water: 90,
    reconciled_through_sequence: 89,
    mutation_journal_head: Some(vec![0x26; hash_width]),
  }
}

fn context(cancellation: CancellationToken) -> GcRunContextV1 {
  GcRunContextV1::new(
    GcRunIDV1::new([0x33; 16]).unwrap(),
    GcRunInvocationV1::Embedded,
    GcRunModeV1::NonDestructiveMark,
    1_700_000_000_000,
    GcRunBudgetsV1::new(64 * 1_024 * 1_024, 128 * 1_024 * 1_024, 256 * 1_024 * 1_024, 8 * 1_024 * 1_024).unwrap(),
    cancellation,
    Arc::new(NoopGcRunProgressSinkV1),
  )
  .unwrap()
}

fn complete_source(algorithm: HashAlgorithm) -> FixtureSource {
  let workspace_name = if algorithm == HashAlgorithm::Sha512 {
    "gc-mark-workspace-manifest-v1/agcw-sha512-mark-workspace-manifest-empty.bin"
  } else {
    "gc-mark-workspace-manifest-v1/agcw-blake3-256-mark-workspace-manifest-empty.bin"
  };
  let workspace = fs::read(fixture_root().join(workspace_name)).unwrap();
  let database_id = inspect_gc_workspace_v1(algorithm, &workspace).unwrap().database_id;
  let mut artifacts = Vec::new();
  let algorithm_name = if algorithm == HashAlgorithm::Sha512 { "sha512" } else { "blake3-256" };
  for path in binary_fixtures("gc-artifact-v1") {
    if !path.file_name().unwrap().to_str().unwrap().contains(algorithm_name) {
      continue;
    }
    let bytes = fs::read(path).unwrap();
    let envelope = decode_gc_artifact_envelope(&bytes).unwrap();
    if !envelope.kind.is_control() {
      artifacts.push(OwnedArtifact { key: immutable_gc_artifact_key(algorithm, envelope.kind, &bytes), bytes });
    }
  }
  let mut controls = Vec::new();
  for kind in [
    GcArtifactKindV1::QuarantineActiveControl,
    GcArtifactKindV1::MarkRunActiveControl,
    GcArtifactKindV1::PhysicalInventoryActiveControl,
    GcArtifactKindV1::AuditCatalogActiveControl,
    GcArtifactKindV1::VoidCatalogActiveControl,
    GcArtifactKindV1::RootLifecycleActiveControl,
  ] {
    let target_kind = kind.control_target().unwrap();
    let target = artifacts.iter().find(|artifact| decode_gc_artifact_envelope(&artifact.bytes).unwrap().kind == target_kind).unwrap();
    let generation = decode_gc_artifact_envelope(&target.bytes).unwrap().generation;
    for slot in 0..=1 {
      let encoded = encode_gc_active_control(&GcActiveControlWriteV1 {
        kind,
        hash_algorithm: algorithm,
        database_id: &database_id,
        slot,
        sequence: u64::from(slot) + 1,
        generation,
        target_manifest_hash: &target.key,
      })
      .unwrap();
      controls.push(OwnedControl { kind, slot, key: encoded.key, bytes: encoded.value });
    }
  }
  FixtureSource {
    basis: basis(algorithm, database_id),
    controls,
    artifacts,
    workspaces: vec![workspace],
    corruptions: vec![],
    cancel_after_first_artifact: false,
  }
}

fn namespace_corruption(algorithm: HashAlgorithm, code: &'static str, seed: u8) -> OwnedCorruption {
  OwnedCorruption {
    scope: GcCorruptionScopeV1::AuthoritativeNamespace,
    code: code.to_string(),
    root_hash: Some(vec![seed; algorithm.hash_length()]),
    path_digest: Some(vec![seed.saturating_add(1); algorithm.hash_length()]),
    evidence_keys: vec![vec![seed.saturating_add(2); algorithm.hash_length()], vec![seed.saturating_add(3); algorithm.hash_length()]],
  }
}

#[test]
fn shared_run_context_closes_control_targets_and_projects_transfer_and_reset_policy() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let source = complete_source(algorithm);
    let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
    let status = execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap();
    assert_eq!(status.state, GcRunStateV1::Complete, "unexpected findings: {:?}", operation.summary().map(|summary| summary.findings));
    let summary = operation.summary().unwrap();
    assert_eq!(summary.selected_control_families, 6);
    assert_eq!(summary.findings.len(), 0);
    assert_eq!(summary.repair_tickets.len(), 0);
    assert_eq!(summary.path_latches.len(), 0);
    assert!(summary.destructive_gc_eligible);
    assert_eq!(summary.backup.gc_artifacts.physical_copy, GcTransferDispositionV1::IncludeValidated);
    assert_eq!(summary.backup.gc_artifacts.logical_backup, GcTransferDispositionV1::OmitDeclared);
    assert_eq!(summary.backup.gc_artifacts.import, GcTransferDispositionV1::NodeLocal);
    assert_eq!(summary.backup.gc_workspaces.physical_copy, GcTransferDispositionV1::NodeLocal);
    assert_eq!(summary.backup.gc_workspaces.logical_backup, GcTransferDispositionV1::OmitDeclared);
    assert_eq!(summary.backup.gc_workspaces.import, GcTransferDispositionV1::NodeLocal);
    assert_eq!(summary.migration.destination_state, GcDestinationStateV1::NeverMarked);
    assert!(!summary.migration.copy_gc_artifacts);
    assert!(!summary.migration.copy_gc_audit_or_corrupt_evidence);
    assert!(!summary.migration.copy_gc_workspaces);
    assert!(!summary.migration.copy_gc_repair_state);
    assert_eq!(summary.migration.required_fresh_complete_marks, 2);
  }
}

#[test]
fn selected_target_corruption_is_incomplete_and_proposes_no_unsafe_reset() {
  let mut source = complete_source(HashAlgorithm::Blake3_256);
  let target = source
    .controls
    .iter()
    .max_by_key(|control| decode_gc_active_control(&control.bytes, HashAlgorithm::Blake3_256).unwrap().sequence)
    .unwrap();
  let target_key = decode_gc_active_control(&target.bytes, HashAlgorithm::Blake3_256).unwrap().target_manifest_hash.to_vec();
  let artifact = source.artifacts.iter_mut().find(|artifact| artifact.key == target_key).unwrap();
  *artifact.bytes.last_mut().unwrap() ^= 1;

  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  let status = execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap();
  assert_eq!(status.state, GcRunStateV1::Incomplete);
  assert_eq!(status.code.as_deref(), Some("gc_maintenance_incomplete"));
  let summary = operation.summary().unwrap();
  assert!(!summary.destructive_gc_eligible);
  assert!(summary.findings.iter().any(|finding| finding.code == "gc_control_target_unavailable"));
  assert!(summary.repair_tickets.iter().any(|ticket| ticket.scope == GcRepairScopeV1::GcAuthority));
  assert!(summary.path_latches.is_empty());
  assert_eq!(summary.migration.destination_state, GcDestinationStateV1::NeverMarked);
}

#[test]
fn authoritative_namespace_corruption_deduplicates_ticket_and_path_latch_proposals() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut source = complete_source(algorithm);
  let corruption = namespace_corruption(algorithm, "btree_child_checksum", 0x41);
  let mut reordered = corruption.clone();
  reordered.evidence_keys.reverse();
  reordered.evidence_keys.push(reordered.evidence_keys[0].clone());
  source.corruptions = vec![corruption, reordered];
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  let status = execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap();
  assert_eq!(status.state, GcRunStateV1::Incomplete);
  let summary = operation.summary().unwrap();
  assert_eq!(summary.repair_tickets.len(), 1);
  assert_eq!(summary.repair_tickets[0].scope, GcRepairScopeV1::AuthoritativeNamespace);
  assert_eq!(summary.path_latches.len(), 1);
  assert_eq!(summary.path_latches[0].ticket_ids, vec![summary.repair_tickets[0].ticket_id]);
  assert!(!summary.destructive_gc_eligible);
}

#[test]
fn duplicate_controls_limits_and_cancellation_fail_without_partial_authority() {
  let mut source = complete_source(HashAlgorithm::Blake3_256);
  source.controls.push(source.controls[0].clone());
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  let status = execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap();
  assert_eq!(status.state, GcRunStateV1::Incomplete);
  assert!(operation.summary().unwrap().findings.iter().any(|finding| finding.code == "gc_control_duplicate_slot"));

  let source = complete_source(HashAlgorithm::Blake3_256);
  let limits = GcMaintenanceInspectionLimitsV1::new(1, 1, 8, 8).unwrap();
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, limits).unwrap();
  assert_eq!(execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap_err().code(), "gc_maintenance_artifact_limit");

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let source = complete_source(HashAlgorithm::Blake3_256);
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  assert_eq!(execute_gc_run_v1(&context(cancellation), &mut operation).unwrap_err().code(), "gc_run_cancelled");
  assert!(operation.summary().is_none());

  let cancellation = CancellationToken::new();
  let mut source = complete_source(HashAlgorithm::Blake3_256);
  source.cancel_after_first_artifact = true;
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  assert_eq!(execute_gc_run_v1(&context(cancellation), &mut operation).unwrap_err().code(), "gc_run_cancelled");
  assert!(operation.summary().is_none());
}

#[test]
fn one_sided_control_fallback_is_valid_but_corrupt_control_state_is_not() {
  let mut source = complete_source(HashAlgorithm::Blake3_256);
  source.controls.retain(|control| !(control.kind == GcArtifactKindV1::QuarantineActiveControl && control.slot == 1));
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  let status = execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap();
  assert_eq!(status.state, GcRunStateV1::Complete);
  assert_eq!(operation.summary().unwrap().selected_control_families, 6);

  let mut source = complete_source(HashAlgorithm::Blake3_256);
  *source.controls[0].bytes.last_mut().unwrap() ^= 1;
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  let status = execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap();
  assert_eq!(status.state, GcRunStateV1::Incomplete);
  let summary = operation.summary().unwrap();
  assert!(summary.findings.iter().any(|finding| finding.code == "gc_control_invalid"));
  assert!(summary.repair_tickets.iter().any(|ticket| ticket.scope == GcRepairScopeV1::GcAuthority));
  assert!(!summary.destructive_gc_eligible);
}

#[test]
fn never_marked_state_is_valid_for_migration_but_cannot_enable_destructive_gc() {
  let mut source = complete_source(HashAlgorithm::Blake3_256);
  source.controls.clear();
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  let status = execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap();
  assert_eq!(status.state, GcRunStateV1::Complete);
  let summary = operation.summary().unwrap();
  assert_eq!(summary.selected_control_families, 0);
  assert!(!summary.destructive_gc_eligible);
  assert_eq!(summary.migration.destination_state, GcDestinationStateV1::NeverMarked);
  assert_eq!(summary.migration.required_fresh_complete_marks, 2);
}

#[test]
fn configured_bounds_and_malformed_corruption_observations_fail_loudly() {
  for limits in [
    GcMaintenanceInspectionLimitsV1::new(0, 1, 1, 1),
    GcMaintenanceInspectionLimitsV1::new(1, 0, 1, 1),
    GcMaintenanceInspectionLimitsV1::new(1, 1, 0, 1),
    GcMaintenanceInspectionLimitsV1::new(1, 1, 1, 0),
    GcMaintenanceInspectionLimitsV1::new(1_000_000_001, 1, 1, 1),
  ] {
    assert_eq!(limits.unwrap_err().code(), "gc_maintenance_limits");
  }

  let mut source = complete_source(HashAlgorithm::Blake3_256);
  source.workspaces.push(source.workspaces[0].clone());
  let limits = GcMaintenanceInspectionLimitsV1::new(4_096, 1, 16, 8).unwrap();
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, limits).unwrap();
  assert_eq!(execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap_err().code(), "gc_maintenance_workspace_limit");

  let mut source = complete_source(HashAlgorithm::Blake3_256);
  source.corruptions = vec![
    namespace_corruption(HashAlgorithm::Blake3_256, "btree_one", 0x51),
    namespace_corruption(HashAlgorithm::Blake3_256, "btree_two", 0x61),
  ];
  let limits = GcMaintenanceInspectionLimitsV1::new(4_096, 8, 1, 8).unwrap();
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, limits).unwrap();
  assert_eq!(execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap_err().code(), "gc_maintenance_finding_limit");

  let mut source = complete_source(HashAlgorithm::Blake3_256);
  source.corruptions = vec![
    namespace_corruption(HashAlgorithm::Blake3_256, "btree_one", 0x51),
    namespace_corruption(HashAlgorithm::Blake3_256, "btree_two", 0x61),
  ];
  let limits = GcMaintenanceInspectionLimitsV1::new(4_096, 8, 8, 1).unwrap();
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, limits).unwrap();
  assert_eq!(
    execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap_err().code(),
    "gc_maintenance_repair_ticket_limit"
  );

  let mut source = complete_source(HashAlgorithm::Blake3_256);
  let mut malformed = namespace_corruption(HashAlgorithm::Blake3_256, "btree_bad", 0x71);
  malformed.evidence_keys.clear();
  source.corruptions = vec![malformed];
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  assert_eq!(
    execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap_err().code(),
    "gc_maintenance_corruption_observation"
  );

  let mut source = complete_source(HashAlgorithm::Blake3_256);
  let mut malformed = namespace_corruption(HashAlgorithm::Blake3_256, "btree_bad", 0x71);
  malformed.code = "x".repeat(1_000_000);
  source.corruptions = vec![malformed];
  let mut operation = GcMaintenanceInspectionOperationV1::new(source, GcMaintenanceInspectionLimitsV1::for_tests()).unwrap();
  assert_eq!(
    execute_gc_run_v1(&context(CancellationToken::new()), &mut operation).unwrap_err().code(),
    "gc_maintenance_corruption_observation"
  );
  assert!(operation.summary().is_none());
}

#[test]
fn maintenance_inspection_remains_read_only_and_disconnected_from_runtime_adapters() {
  let source = include_str!("../../src/engine/v4/gc_maintenance.rs");
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "V4FirstAuthorityPublisher",
    "ControlStore",
    "VoidManager",
    "execute_sweep_locator_removals",
    "crate::server",
    "task_worker",
    "std::fs::",
    "tokio::spawn",
  ] {
    assert!(!source.contains(forbidden), "maintenance inspection gained forbidden runtime capability: {forbidden}");
  }
}
