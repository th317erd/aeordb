use std::path::{Path, PathBuf};

use aeordb::engine::memory_coordinator::MemoryCoordinator;
use aeordb::engine::v4::first_authority::VoidClaimAdmissionPermitV1;
use aeordb::engine::v4::gc_void_settlement::{VoidClaimAllocationLimitsV1, VoidClaimAllocationOwnerV1, VoidClaimConsumptionPermitV1};
use tokio_util::sync::CancellationToken;

fn collect_rust_files(root: &Path, output: &mut Vec<PathBuf>) {
  for entry in std::fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      collect_rust_files(&path, output);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      output.push(path);
    }
  }
}

#[test]
fn settlement_has_one_disconnected_allocator_owner_and_consumes_the_private_claim_permit() {
  let constructor: fn(
    VoidClaimAdmissionPermitV1,
    VoidClaimAllocationLimitsV1,
    &MemoryCoordinator,
    CancellationToken,
  ) -> Result<VoidClaimAllocationOwnerV1, _> = VoidClaimAllocationOwnerV1::new;
  let finisher: fn(VoidClaimAllocationOwnerV1) -> Result<VoidClaimConsumptionPermitV1, _> = VoidClaimAllocationOwnerV1::finish;
  assert_eq!(std::mem::size_of_val(&constructor), std::mem::size_of::<usize>());
  assert_eq!(std::mem::size_of_val(&finisher), std::mem::size_of::<usize>());

  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let mut source_files = Vec::new();
  collect_rust_files(&source_root, &mut source_files);
  let owners: Vec<_> =
    source_files.iter().filter(|path| std::fs::read_to_string(path).unwrap().contains("pub struct VoidClaimAllocationOwnerV1")).collect();
  assert_eq!(owners.len(), 1);
  assert!(owners[0].ends_with("engine/v4/gc_void_settlement.rs"));

  let source = std::fs::read_to_string(owners[0]).unwrap();
  for forbidden in ["VoidManager", "find_void", "StorageEngine", "server::", "run_gc"] {
    assert!(!source.contains(forbidden), "P4-7d must not activate live {forbidden}");
  }
}
