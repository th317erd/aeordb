//! Literal section framing plus frozen independent manifest payloads.
use std::cell::Cell;
use aeordb::engine::memory_coordinator::{HostMemorySample, MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use aeordb::engine::v4::plugin_artifact_identity::{
  inspect_plugin_artifact_identity_v1, PluginArtifactIdentityRequestV1, PluginArtifactIdentityV1,
};

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 1 << 20).unwrap())
}

#[path = "plugin_artifact_identity_fixtures.rs"]
mod fixtures;
use fixtures::*;

fn request<'a>(alias: &'a [u8], module: &'a [u8], alias_path: &'a str, artifact_path: &'a str) -> PluginArtifactIdentityRequestV1<'a> {
  PluginArtifactIdentityRequestV1 {
    alias_bytes: alias,
    module_bytes: module,
    alias_path,
    artifact_path,
    maximum_module_bytes: 64 << 20,
    maximum_workspace_bytes: 16 << 10,
  }
}

fn failure(result: Result<PluginArtifactIdentityV1<'_>, SemanticCompilationErrorV1>) -> SemanticCompilationErrorV1 {
  match result {
    Err(error) => error,
    Ok(_) => panic!("invalid identity was accepted"),
  }
}

#[test]
fn each_manifest_role_binds_exact_module_and_alias_with_borrowed_views() {
  for role in ["parser", "mapper", "both"] {
    let memory = memory();
    let module = module(role);
    let alias = alias(&module, role);
    let alias_path = alias_path();
    let artifact_path = artifact_path(&module);
    let identity = inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path, &artifact_path), &memory, &|| false).unwrap();
    assert_eq!(identity.module_bytes().as_ptr(), module.as_ptr());
    assert_eq!(identity.alias().alias.as_ptr(), alias[128..].as_ptr());
    assert_eq!(identity.alias().plugin_id, identity.manifest().plugin_id);
    assert_eq!(identity.alias().version, Some(identity.manifest().version));
    assert_eq!(identity.alias().author, identity.manifest().author);
    assert_eq!(identity.manifest().roles().len(), if role == "both" { 2 } else { 1 });
    let offset = identity.manifest().plugin_id.as_ptr() as usize - module.as_ptr() as usize;
    assert!(offset < module.len());
    assert!(memory.snapshot().unwrap().reserved_bytes > 0);
    drop(identity);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}

#[test]
fn every_alias_metadata_field_must_match_even_with_valid_crc_and_artifact_hash() {
  let module = module("both");
  let original = alias(&module, "both");
  let mut offset = 128 + 5;
  for length_offset in [20, 24, 28, 32] {
    let mut alias = original.clone();
    alias[offset] = match length_offset {
      20 => b'/',
      28 => b'2',
      _ => b'X',
    };
    if length_offset == 20 {
      alias[offset + 1] = b'x';
    }
    seal(&mut alias);
    // Isolate binding failures from the underlying alias codec's syntax/CRC.
    aeordb::engine::v4::plugin_identity::decode_plugin_alias_v1(&alias, &alias_path()).unwrap();
    assert!(
      inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path(), &artifact_path(&module)), &memory(), &|| false).is_err()
    );
    offset += u32::from_le_bytes(original[length_offset..length_offset + 4].try_into().unwrap()) as usize;
  }
}

#[test]
fn digest_length_and_protected_paths_are_independent_checks() {
  let module = module("parser");
  let original = alias(&module, "parser");
  let alias_path = alias_path();
  let artifact_path = artifact_path(&module);
  for offset in [40, 72] {
    let mut alias = original.clone();
    alias[offset] ^= 1;
    seal(&mut alias);
    assert!(inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path, &artifact_path), &memory(), &|| false).is_err());
  }
  for path in [
    artifact_path.to_uppercase(),
    format!("{artifact_path}/"),
    artifact_path[..artifact_path.len() - 1].to_string(),
    format!("/.aeordb-system/plugin-artifacts/blake3/{}", "0".repeat(64)),
  ] {
    assert!(inspect_plugin_artifact_identity_v1(request(&original, &module, &alias_path, &path), &memory(), &|| false).is_err());
  }
  assert!(inspect_plugin_artifact_identity_v1(request(&original, &module, "wrong", &artifact_path), &memory(), &|| false).is_err());
}

#[test]
fn missing_duplicate_and_malformed_named_sections_fail_after_rebinding_digest() {
  let mut duplicate = module("parser");
  custom("aeordb.plugin.v1", &manifest("parser"), &mut duplicate);
  let mut malformed = b"\0asm\x01\0\0\0".to_vec();
  custom("aeordb.plugin.v1", b"bad", &mut malformed);
  for module in [b"\0asm\x01\0\0\0".to_vec(), duplicate, malformed] {
    let alias = alias(&module, "parser");
    assert!(
      inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path(), &artifact_path(&module)), &memory(), &|| false).is_err()
    );
  }
}

#[test]
fn every_truncated_module_and_bad_section_frame_is_rejected() {
  let original = module("parser");
  for end in 0..original.len() {
    let module = &original[..end];
    let alias = alias(module, "parser");
    assert!(
      inspect_plugin_artifact_identity_v1(request(&alias, module, &alias_path(), &artifact_path(module)), &memory(), &|| false).is_err(),
      "end {end}"
    );
  }
  for tail in [vec![0], vec![0, 0xff, 0xff, 0xff, 0xff, 0x1f], vec![0, 2, 1, 0xff], vec![0, 1, 2], vec![127, 0]] {
    let mut module = original.clone();
    module.extend_from_slice(&tail);
    let alias = alias(&module, "parser");
    assert!(
      inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path(), &artifact_path(&module)), &memory(), &|| false).is_err(),
      "tail {tail:?}"
    );
  }
  for header in [b"\0asm\x02\0\0\0", b"\0asm\x0d\0\x01\0"] {
    let mut module = original.clone();
    module[..8].copy_from_slice(header);
    let alias = alias(&module, "parser");
    assert!(
      inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path(), &artifact_path(&module)), &memory(), &|| false).is_err()
    );
  }
}

#[test]
fn unrelated_custom_bytes_change_raw_identity_but_not_copied_metadata() {
  let module = module("parser");
  let old_alias = alias(&module, "parser");
  let mut changed = module.clone();
  custom("opaque.other", &[0, 0xff, 1, 2], &mut changed);
  let new_alias = alias(&changed, "parser");
  assert!(inspect_plugin_artifact_identity_v1(request(&old_alias, &changed, &alias_path(), &artifact_path(&changed)), &memory(), &|| {
    false
  })
  .is_err());
  let memory = memory();
  let path = alias_path();
  let new_path = artifact_path(&changed);
  let old_path = artifact_path(&module);
  let old = inspect_plugin_artifact_identity_v1(request(&old_alias, &module, &path, &old_path), &memory, &|| false).unwrap();
  let new = inspect_plugin_artifact_identity_v1(request(&new_alias, &changed, &path, &new_path), &memory, &|| false).unwrap();
  assert_eq!(old.manifest().plugin_id, new.manifest().plugin_id);
  assert_ne!(old.alias().artifact_fingerprint, new.alias().artifact_fingerprint);
}

#[test]
fn operational_limits_cancellation_and_pressure_release_then_retry() {
  let mut module = module("parser");
  custom("padding", &vec![0; 256 << 10], &mut module);
  let alias = alias(&module, "parser");
  let alias_path = alias_path();
  let artifact_path = artifact_path(&module);
  let memory = memory();
  let request = request(&alias, &module, &alias_path, &artifact_path);
  for limited in [
    PluginArtifactIdentityRequestV1 { maximum_module_bytes: 1, ..request },
    PluginArtifactIdentityRequestV1 { maximum_workspace_bytes: 1, ..request },
  ] {
    assert!(matches!(
      failure(inspect_plugin_artifact_identity_v1(limited, &memory, &|| false)),
      SemanticCompilationErrorV1::Resource { .. }
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
  for after in [1, 3, 5] {
    let calls = Cell::new(0);
    let cancelled = || {
      calls.set(calls.get() + 1);
      calls.get() >= after
    };
    assert!(matches!(failure(inspect_plugin_artifact_identity_v1(request, &memory, &cancelled)), SemanticCompilationErrorV1::Cancelled));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
  memory.update_host_sample(HostMemorySample { rss_bytes: 32 << 20, ..HostMemorySample::default() }).unwrap();
  assert!(matches!(failure(inspect_plugin_artifact_identity_v1(request, &memory, &|| false)), SemanticCompilationErrorV1::Resource { .. }));
  memory.update_host_sample(HostMemorySample::default()).unwrap();
  drop(inspect_plugin_artifact_identity_v1(request, &memory, &|| false).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn legacy_metadata_requires_adapter_and_optional_author_must_agree() {
  let module = module("parser");
  for absent_version in [false, true] {
    let mut alias = alias(&module, "parser");
    if absent_version {
      let version_start = 128 + 5 + "/org/example/parser".len() + "Fixture".len();
      alias.drain(version_start..version_start + 5);
      alias[28..32].fill(0);
      alias[12] |= 1;
      let length = alias.len() as u32;
      alias[8..12].copy_from_slice(&length.to_le_bytes());
    } else {
      alias[12] |= 4;
    }
    seal(&mut alias);
    let memory = memory();
    assert!(matches!(
      failure(inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path(), &artifact_path(&module)), &memory, &|| false)),
      SemanticCompilationErrorV1::DependencyUnavailable { .. }
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
  let mut alias = alias(&module, "parser");
  alias[12] &= !2;
  alias[32..36].copy_from_slice(&1u32.to_le_bytes());
  alias.insert(alias.len() - 4, b'A');
  let length = alias.len() as u32;
  alias[8..12].copy_from_slice(&length.to_le_bytes());
  seal(&mut alias);
  assert!(matches!(
    failure(inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path(), &artifact_path(&module)), &memory(), &|| false)),
    SemanticCompilationErrorV1::InvalidSource { .. }
  ));
}

#[test]
fn cancellation_and_pressure_at_every_boundary_release_the_lease() {
  let module = module("both");
  let alias = alias(&module, "both");
  let alias_path = alias_path();
  let artifact_path = artifact_path(&module);
  let request = request(&alias, &module, &alias_path, &artifact_path);
  let memory = memory();
  let calls = Cell::new(0);
  drop(
    inspect_plugin_artifact_identity_v1(request, &memory, &|| {
      calls.set(calls.get() + 1);
      false
    })
    .unwrap(),
  );
  let boundary_count = calls.get();
  assert!(boundary_count > 8, "must check traversal events, not just preflight/hash");
  for boundary in 1..=boundary_count {
    calls.set(0);
    assert!(matches!(
      failure(inspect_plugin_artifact_identity_v1(request, &memory, &|| {
        calls.set(calls.get() + 1);
        calls.get() == boundary
      })),
      SemanticCompilationErrorV1::Cancelled
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    calls.set(0);
    assert!(matches!(
      failure(inspect_plugin_artifact_identity_v1(request, &memory, &|| {
        calls.set(calls.get() + 1);
        if calls.get() == boundary {
          memory.update_host_sample(HostMemorySample { rss_bytes: 32 << 20, ..HostMemorySample::default() }).unwrap();
        }
        false
      })),
      SemanticCompilationErrorV1::Resource { .. }
    ));
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    memory.update_host_sample(HostMemorySample::default()).unwrap();
  }
  drop(inspect_plugin_artifact_identity_v1(request, &memory, &|| false).unwrap());
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn exact_sixty_four_mib_is_borrowed_and_one_more_byte_is_invalid() {
  let mut module = module("parser");
  let maximum = 64 << 20;
  // section ID + four-byte section LEB + one-byte name LEB + seven-byte name.
  let padding = maximum - module.len() - 13;
  custom("padding", &vec![0; padding], &mut module);
  assert_eq!(module.len(), maximum);
  let alias = alias(&module, "parser");
  let alias_path = alias_path();
  let artifact_path = artifact_path(&module);
  let memory = memory();
  let identity = inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path, &artifact_path), &memory, &|| false).unwrap();
  assert_eq!(identity.module_bytes().as_ptr(), module.as_ptr());
  assert!(memory.snapshot().unwrap().reserved_bytes < 1024);
  drop(identity);
  module.push(0);
  assert!(matches!(
    failure(inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path, &artifact_path), &memory, &|| false)),
    SemanticCompilationErrorV1::InvalidSource { .. }
  ));
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn identity_does_not_claim_lazy_function_body_validation() {
  let mut module = b"\0asm\x01\0\0\0".to_vec();
  custom("aeordb.plugin.v1", &manifest("parser"), &mut module);
  // Framed code body but no matching function/type, invalid opcode and no end.
  module.extend_from_slice(&[10, 4, 1, 2, 0, 0xff]);
  assert!(wasmi::Module::validate(&wasmi::Engine::default(), &module).is_err());
  let alias = alias(&module, "parser");
  let alias_path = alias_path();
  let artifact_path = artifact_path(&module);
  let identity = inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path, &artifact_path), &memory(), &|| false).unwrap();
  assert_eq!(identity.module_bytes(), module);
}

#[test]
fn decoder_and_parser_error_details_survive_identity_classification() {
  let module = module("parser");
  let mut alias = alias(&module, "parser");
  alias[80] ^= 1; // Valid signed timestamp, now an invalid CRC.
  let error =
    failure(inspect_plugin_artifact_identity_v1(request(&alias, &module, &alias_path(), &artifact_path(&module)), &memory(), &|| false));
  assert!(matches!(&error, SemanticCompilationErrorV1::InvalidSource { .. }));
  assert!(error.to_string().contains("plugin_alias_crc"), "{error}");

  let mut malformed = module.clone();
  malformed.push(0); // Missing section length; digest/path deliberately repaired.
  let alias = fixtures::alias(&malformed, "parser");
  let error = failure(inspect_plugin_artifact_identity_v1(
    request(&alias, &malformed, &alias_path(), &artifact_path(&malformed)),
    &memory(),
    &|| false,
  ));
  assert!(matches!(&error, SemanticCompilationErrorV1::InvalidSource { .. }));
  assert!(error.to_string().contains("offset"), "parser diagnostic lost its byte offset: {error}");
}
