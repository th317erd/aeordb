//! Read-only raw-artifact identity, not bytecode or executor admission.
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use super::parser_registry_compiler::SemanticCompilationErrorV1;
use super::plugin_identity::{decode_plugin_alias_v1, decode_plugin_manifest_payload_v1, AeorPluginManifestV1, PluginAliasRecordV1};

const IDENTITY_PATH: &str = "<plugin-artifact-identity>";
const ARTIFACT_PREFIX: &str = "/.aeordb-system/plugin-artifacts/blake3/";
const MAX_MODULE_BYTES: usize = 64 << 20;
const HASH_CHUNK_BYTES: usize = 64 << 10;
// Fixed stack workspace, not input buffers (owned/admitted by the caller).
// The core-only parser never grows its nested component-parser stack.
const WORKSPACE_BYTES: usize = 16 << 10;
const RESULT_BYTES: usize = std::mem::size_of::<PluginArtifactIdentityV1<'static>>();
const _: () = assert!(
  std::mem::size_of::<blake3::Hasher>()
    + std::mem::size_of::<wasmparser::Parser>()
    + std::mem::size_of::<wasmparser::Payload<'static>>()
    + RESULT_BYTES
    + 4096
    <= WORKSPACE_BYTES
);

#[derive(Clone, Copy)]
pub struct PluginArtifactIdentityRequestV1<'a> {
  pub alias_bytes: &'a [u8],
  pub alias_path: &'a str,
  pub artifact_path: &'a str,
  pub module_bytes: &'a [u8],
  pub maximum_module_bytes: usize,
  pub maximum_workspace_bytes: usize,
}

/// Exact borrowed identity only. This is not an executable dependency, archive
/// retention pin, compiler snapshot, validated bytecode or publication permit.
pub struct PluginArtifactIdentityV1<'a> {
  alias: PluginAliasRecordV1<'a>,
  manifest: AeorPluginManifestV1<'a>,
  module_bytes: &'a [u8],
  _memory: MemoryReservation,
}

impl<'a> PluginArtifactIdentityV1<'a> {
  pub const fn alias(&self) -> &PluginAliasRecordV1<'a> {
    &self.alias
  }
  pub const fn manifest(&self) -> &AeorPluginManifestV1<'a> {
    &self.manifest
  }
  pub const fn module_bytes(&self) -> &'a [u8] {
    self.module_bytes
  }
}

/// Inspect immutable caller-owned bytes without executing, installing, pinning
/// or publishing anything. The source owner accounts for input buffers and pins;
/// executor admission must separately validate bytecode, exports and profile.
pub fn inspect_plugin_artifact_identity_v1<'a>(
  request: PluginArtifactIdentityRequestV1<'a>,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<PluginArtifactIdentityV1<'a>, SemanticCompilationErrorV1> {
  check_cancelled(is_cancelled)?;
  if !(1..=MAX_MODULE_BYTES).contains(&request.module_bytes.len()) {
    return Err(invalid("raw module must contain 1..64 MiB"));
  }
  if request.module_bytes.len() > request.maximum_module_bytes || WORKSPACE_BYTES > request.maximum_workspace_bytes {
    return Err(resource("artifact inspection exceeds the caller's module/workspace limit"));
  }
  let mut reservation =
    memory.reserve(MemoryOwner::Task, WORKSPACE_BYTES as u64, AdmissionClass::Workload).map_err(|source| resource(source.to_string()))?;
  let alias = decode_plugin_alias_v1(request.alias_bytes, request.alias_path).map_err(|source| invalid(source.to_string()))?;
  if alias.flags & 5 != 0 {
    return Err(SemanticCompilationErrorV1::DependencyUnavailable {
      path: IDENTITY_PATH,
      message: "version-absent or opaque-legacy alias requires the explicit legacy adapter".into(),
    });
  }
  if alias.artifact_length != request.module_bytes.len() as u64 {
    return Err(invalid("raw module length differs from alias artifact length"));
  }
  let mut hasher = blake3::Hasher::new();
  for chunk in request.module_bytes.chunks(HASH_CHUNK_BYTES) {
    check(&reservation, is_cancelled)?;
    hasher.update(chunk);
  }
  check(&reservation, is_cancelled)?;
  let fingerprint = hasher.finalize();
  if alias.artifact_fingerprint != fingerprint.as_bytes() {
    return Err(invalid("raw module fingerprint differs from alias artifact fingerprint"));
  }
  validate_artifact_path(request.artifact_path, fingerprint.as_bytes())?;
  let mut manifest = None;
  // These payloads expose lazy section/body readers. This loop intentionally
  // proves only outer framing and named identity, NOT core section ordering,
  // bytecode validity, ABI exports, pure imports, memory policy or executability.
  for payload in wasmparser::Parser::new(0).parse_all(request.module_bytes) {
    check(&reservation, is_cancelled)?;
    match payload.map_err(|source| invalid(source.to_string()))? {
      wasmparser::Payload::Version { num, encoding, .. } if num != 1 || encoding != wasmparser::Encoding::Module => {
        return Err(invalid("artifact identity requires a version-1 core module"));
      }
      // A valid core-module version contributes no identity metadata. The
      // iterator still checks the remaining framing before identity is returned.
      wasmparser::Payload::UnknownSection { .. } => return Err(invalid("unknown outer module section")),
      wasmparser::Payload::CustomSection(section) if section.name() == "aeordb.plugin.v1" => {
        if manifest.is_some() {
          return Err(invalid("duplicate aeordb.plugin.v1 custom section"));
        }
        manifest = Some(decode_plugin_manifest_payload_v1(section.data()).map_err(|source| invalid(source.to_string()))?);
      }
      _ => {}
    }
  }
  let manifest = manifest.ok_or_else(|| invalid("missing aeordb.plugin.v1 custom section"))?;
  if alias.plugin_id != manifest.plugin_id
    || alias.name != manifest.name
    || alias.version != Some(manifest.version)
    || alias.author != manifest.author
  {
    return Err(invalid("alias metadata differs from the archived module manifest"));
  }
  check(&reservation, is_cancelled)?;
  reservation.shrink((WORKSPACE_BYTES - RESULT_BYTES) as u64).map_err(|source| resource(source.to_string()))?;
  Ok(PluginArtifactIdentityV1 { alias, manifest, module_bytes: request.module_bytes, _memory: reservation })
}

fn validate_artifact_path(path: &str, fingerprint: &[u8; 32]) -> Result<(), SemanticCompilationErrorV1> {
  let suffix = path.strip_prefix(ARTIFACT_PREFIX).ok_or_else(|| invalid("wrong protected artifact path prefix"))?.as_bytes();
  if suffix.len() != 64 {
    return Err(invalid("artifact path requires exactly 64 lowercase BLAKE3 hex digits"));
  }
  let hex = b"0123456789abcdef";
  for (index, byte) in fingerprint.iter().enumerate() {
    if suffix[2 * index] != hex[usize::from(byte >> 4)] || suffix[2 * index + 1] != hex[usize::from(byte & 15)] {
      return Err(invalid("protected artifact path differs from raw module identity"));
    }
  }
  Ok(())
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<(), SemanticCompilationErrorV1> {
  if is_cancelled() {
    Err(SemanticCompilationErrorV1::Cancelled)
  } else {
    Ok(())
  }
}

fn check(reservation: &MemoryReservation, is_cancelled: &dyn Fn() -> bool) -> Result<(), SemanticCompilationErrorV1> {
  check_cancelled(is_cancelled)?;
  reservation.check_admission().map_err(|source| resource(source.to_string()))
}

fn invalid(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::InvalidSource { path: IDENTITY_PATH, message: message.into() }
}

fn resource(message: impl Into<String>) -> SemanticCompilationErrorV1 {
  SemanticCompilationErrorV1::Resource { path: IDENTITY_PATH, message: message.into() }
}
