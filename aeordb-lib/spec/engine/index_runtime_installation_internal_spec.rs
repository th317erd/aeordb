use crate::engine::memory_coordinator::MemoryOwner;
use crate::engine::native_durability::PlatformFileIdentityDescriptorV1;
use crate::engine::v4::index_coordinator_recovery::IndexRecoveryOptionsV1;
use crate::engine::v4::index_operation_control::IndexOperationKindV1;
use crate::engine::v4::index_recovery_store::{IndexScopeOrdinalStoreRegistryOptionsV1, NativeIndexOperationDescriptorV1};
use crate::engine::{HashAlgorithm, StorageEngine};

use super::*;

fn reserved_index_bytes(engine: &StorageEngine) -> u64 {
  engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes
}

#[test]
fn descriptor_catalog_retains_its_memory_reservation_for_the_runtime_lifetime() {
  let directory = tempfile::tempdir().unwrap();
  let engine = StorageEngine::create(directory.path().join("descriptor-order.aeordb").to_str().unwrap()).unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let identity = IndexRuntimeShadowIdentityV1 {
    database_id: [0x11; 16],
    migration_id: [0x12; 16],
    source_physical_instance_id: [0x13; 16],
    destination_physical_instance_id: [0x14; 16],
    source_file_identity: PlatformFileIdentityDescriptorV1 {
      platform: 1,
      schema: 1,
      flags: 0,
      volume_identity: [0x15; 16],
      file_identity: [0x16; 16],
      birth_identity: [0; 16],
    },
    hash_algorithm: algorithm,
    system_family_registry_fingerprint: vec![0x17; algorithm.hash_length()],
  };
  let descriptor = NativeIndexOperationDescriptorV1::new(
    algorithm,
    identity.database_id,
    vec![0x21; algorithm.hash_length()],
    [0x22; 16],
    IndexOperationKindV1::Build,
    vec![0x23; algorithm.hash_length()],
    None,
    None,
  )
  .unwrap();
  let options = IndexRuntimeNativeRecoveryOptionsV1::new(
    8,
    1_024 * 1_024,
    IndexScopeOrdinalStoreRegistryOptionsV1::new(8, 1_024 * 1_024).unwrap(),
    IndexRecoveryOptionsV1::new(8, 1_024 * 1_024, 8, 1_024 * 1_024).unwrap(),
  )
  .unwrap();
  let cancellation = CancellationToken::new();
  let baseline = reserved_index_bytes(&engine);

  let catalog = NativeIndexOperationDescriptorCatalogV1::new(
    algorithm,
    identity.database_id,
    std::slice::from_ref(&descriptor),
    options.maximum_operation_descriptors,
    options.maximum_descriptor_bytes,
    engine.memory_coordinator(),
    &|| cancellation.is_cancelled(),
  )
  .unwrap();

  assert_eq!(catalog.descriptors(), std::slice::from_ref(&descriptor));
  assert_eq!(reserved_index_bytes(&engine), baseline + catalog.retained_bytes());
  drop(catalog);
  assert_eq!(reserved_index_bytes(&engine), baseline);
}

#[test]
fn final_installation_frontier_rejects_cancellation_and_semantic_authority_drift() {
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert!(matches!(
    validate_final_installation_frontier(&cancellation, &[0x11], &[0x12], 7, &[0x11], &[0x12], 7),
    Err(NativeIndexRuntimeInstallationErrorV1::Canceled)
  ));

  let active = CancellationToken::new();
  assert!(matches!(
    validate_final_installation_frontier(&active, &[0x11], &[0x12], 7, &[0x21], &[0x12], 8),
    Err(NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_semantic_authority_changed", .. })
  ));
  assert!(matches!(
    validate_final_installation_frontier(&active, &[0x11], &[0x12], 7, &[0x11], &[0x22], 8),
    Err(NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_semantic_authority_changed", .. })
  ));
  assert!(validate_final_installation_frontier(&active, &[0x11], &[0x12], 7, &[0x11], &[0x12], 7).is_ok());
}
