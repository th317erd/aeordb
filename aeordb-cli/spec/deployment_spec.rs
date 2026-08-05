use std::fs;
use std::process::Command;

use aeordb::engine::v4::control_store::V3TransitionControlStore;
use aeordb::engine::v4::deployment_guard::{DeploymentCapabilitiesV1, TRANSITION_RECOVERY_CAPABILITY_V1};
use aeordb::engine::v4::system_control::{SystemControlKindV1, decode_system_control};
use aeordb::engine::{HashAlgorithm, StorageEngine};

fn aeordb() -> Command {
  Command::new(env!("CARGO_BIN_EXE_aeordb"))
}

fn publish_fixture_recovery_controls(engine: &StorageEngine) {
  let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../aeordb-lib/spec/fixtures/v4/system-control-v1");
  let catalog = fs::read(root.join("control-blake3-256-emergency-spill-catalog-valid.bin")).unwrap();
  let latch = fs::read(root.join("control-blake3-256-durability-latch-valid.bin")).unwrap();
  let catalog_control = decode_system_control(&catalog, HashAlgorithm::Blake3_256).unwrap();
  let latch_control = decode_system_control(&latch, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(catalog_control.database_id, latch_control.database_id);
  let database_id: [u8; 16] = catalog_control.database_id.try_into().unwrap();
  let store = V3TransitionControlStore::new(engine);
  store.publish_mutable(SystemControlKindV1::EmergencySpillCatalog, database_id, &[], &catalog).unwrap();
  store.publish_mutable(SystemControlKindV1::DurabilityLatch, database_id, &[], &latch).unwrap();
}

#[test]
fn deployment_capabilities_are_machine_readable_and_require_exact_tokens() {
  let output = aeordb().args(["deployment-capabilities", "--json"]).output().unwrap();
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let report: DeploymentCapabilitiesV1 = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(report.capabilities, vec![TRANSITION_RECOVERY_CAPABILITY_V1]);

  let supported = aeordb().args(["deployment-capabilities", "--require", TRANSITION_RECOVERY_CAPABILITY_V1]).output().unwrap();
  assert!(supported.status.success());
  let unsupported = aeordb().args(["deployment-capabilities", "--require", "aeordb.not-real.v1"]).output().unwrap();
  assert_eq!(unsupported.status.code(), Some(3));
}

#[test]
fn deployment_check_refuses_an_old_candidate_during_active_recovery_and_allows_a_compatible_candidate() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("active.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();
  publish_fixture_recovery_controls(&engine);
  engine.shutdown().unwrap();
  drop(engine);

  let refused = aeordb().args(["deployment-check", "--database", database.to_str().unwrap(), "--json"]).output().unwrap();
  assert_eq!(refused.status.code(), Some(3), "{}", String::from_utf8_lossy(&refused.stderr));
  let refused_json: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
  assert_eq!(refused_json["decision"]["allowed"], false);
  assert_eq!(refused_json["state"]["requires_transition_capability"], true);

  let allowed = aeordb()
    .args([
      "deployment-check",
      "--database",
      database.to_str().unwrap(),
      "--candidate-capability",
      TRANSITION_RECOVERY_CAPABILITY_V1,
      "--json",
    ])
    .output()
    .unwrap();
  assert!(allowed.status.success(), "{}", String::from_utf8_lossy(&allowed.stderr));
}

#[test]
fn deployment_check_allows_an_inactive_downgrade_and_fails_closed_on_corrupt_input() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("inactive.aeordb");
  StorageEngine::create(database.to_str().unwrap()).unwrap().shutdown().unwrap();
  let allowed = aeordb().args(["deployment-check", "--database", database.to_str().unwrap(), "--json"]).output().unwrap();
  assert!(allowed.status.success(), "{}", String::from_utf8_lossy(&allowed.stderr));

  let corrupt = temp.path().join("corrupt.aeordb");
  fs::write(&corrupt, b"not a database").unwrap();
  let failed = aeordb().args(["deployment-check", "--database", corrupt.to_str().unwrap(), "--json"]).output().unwrap();
  assert_eq!(failed.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&failed.stderr).contains("inspection failed"));

  let missing = temp.path().join("missing.aeordb");
  let failed = aeordb().args(["deployment-check", "--database", missing.to_str().unwrap(), "--json"]).output().unwrap();
  assert_eq!(failed.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&failed.stderr).contains("inspection failed"));
}

#[test]
fn deployment_check_requires_a_quiescent_database_before_an_incompatible_downgrade() {
  let temp = tempfile::tempdir().unwrap();
  let database = temp.path().join("open.aeordb");
  let engine = StorageEngine::create(database.to_str().unwrap()).unwrap();

  let failed = aeordb().args(["deployment-check", "--database", database.to_str().unwrap(), "--json"]).output().unwrap();

  assert_eq!(failed.status.code(), Some(1));
  assert!(String::from_utf8_lossy(&failed.stderr).contains("still open"));
  engine.shutdown().unwrap();
}
