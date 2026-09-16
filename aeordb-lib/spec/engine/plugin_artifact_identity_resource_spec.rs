use super::measure;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::plugin_artifact_identity::{inspect_plugin_artifact_identity_v1, PluginArtifactIdentityRequestV1};

#[path = "plugin_artifact_identity_fixtures.rs"]
mod fixtures;

fn warmed_memory() -> MemoryCoordinator {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap());
  // Native macOS may allocate its mutex bookkeeping on first use; measure the
  // inspector separately from this already-accounted coordinator initialization.
  drop(memory.reserve(MemoryOwner::Task, 0, AdmissionClass::Workload).unwrap());
  memory
}

#[test]
fn artifact_identity_has_no_heap_work_or_input_sized_retained_charge() {
  let mut retained = None;
  for (padding, sections) in [(0, 0), (1 << 20, 0), (0, 20_000)] {
    let mut module = fixtures::module("both");
    fixtures::custom("padding", &vec![0; padding], &mut module);
    for _ in 0..sections {
      fixtures::custom("other", &[], &mut module);
    }
    let alias = fixtures::alias(&module, "both");
    let alias_path = fixtures::alias_path();
    let artifact_path = fixtures::artifact_path(&module);
    let memory = warmed_memory();
    let request = PluginArtifactIdentityRequestV1 {
      alias_bytes: &alias,
      alias_path: &alias_path,
      artifact_path: &artifact_path,
      module_bytes: &module,
      maximum_module_bytes: 64 << 20,
      maximum_workspace_bytes: 16 << 10,
    };
    let (result, allocations) = measure(0, || inspect_plugin_artifact_identity_v1(request, &memory, &|| false));
    let identity = result.unwrap();
    assert_eq!(allocations.total, 0, "{padding} bytes, {sections} sections: {allocations:?}");
    assert_eq!(identity.module_bytes().as_ptr(), module.as_ptr());
    let charge = memory.snapshot().unwrap().reserved_bytes;
    assert!(charge > 0 && charge < 1024);
    assert_eq!(*retained.get_or_insert(charge), charge);
    drop(identity);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn amplified_outer_lengths_are_bounded_diagnostics_not_allocations() {
  for tail in [vec![0, 0xff, 0xff, 0xff, 0xff, 0x0f], vec![0, 5, 0xff, 0xff, 0xff, 0xff, 0x0f]] {
    let mut module = fixtures::module("parser");
    module.extend_from_slice(&tail);
    let alias = fixtures::alias(&module, "parser");
    let alias_path = fixtures::alias_path();
    let artifact_path = fixtures::artifact_path(&module);
    let memory = warmed_memory();
    let request = PluginArtifactIdentityRequestV1 {
      alias_bytes: &alias,
      alias_path: &alias_path,
      artifact_path: &artifact_path,
      module_bytes: &module,
      maximum_module_bytes: 64 << 20,
      maximum_workspace_bytes: 16 << 10,
    };
    let (result, allocations) = measure(0, || inspect_plugin_artifact_identity_v1(request, &memory, &|| false));
    assert!(result.is_err());
    assert!(allocations.total < 4096 && allocations.maximum < 1024, "{allocations:?}");
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
