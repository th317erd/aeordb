use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::HashProfile;

const MAGIC: &[u8; 4] = b"ASFR";
const VERSION: u16 = 1;
const HEADER_LENGTH: usize = 32;
const DESCRIPTOR_FIXED_LENGTH: usize = 32;
const CRC_LENGTH: usize = 4;
const MAX_REGISTRY_LENGTH: usize = 1_048_576;
const CAMPAIGN_ID: &str = "aeordb-v4-nvt-gc-2026-08-03";
const BUILDER_REVISION: &str = "p0b2-system-family-v1";

#[derive(Clone, Copy)]
pub enum SystemFamilyFormat {
  RegistryV1,
}

impl SystemFamilyFormat {
  pub fn id(self) -> &'static str {
    "system-family-registry-v1"
  }

  pub fn family(self) -> &'static str {
    "SystemFamilyRegistryV1"
  }
}

#[derive(Clone)]
pub struct SystemFamilyFixtureCase {
  pub id: &'static str,
  pub format: SystemFamilyFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[repr(u8)]
enum StorageDomain {
  Path = 1,
  EntryType = 2,
  KvKeyPrefix = 3,
  ControlRegion = 4,
  ExternalWorkspace = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[repr(u8)]
enum MatchKind {
  AbsolutePathExact = 1,
  AbsolutePathPrefix = 2,
  DescendantReservedFile = 3,
  DescendantReservedSubtree = 4,
  ReservedPathSegment = 5,
  EntryTypeExact = 6,
  KvKeyPrefix = 7,
  ControlTagExact = 8,
  WorkspaceKindExact = 9,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Matcher {
  domain: StorageDomain,
  kind: MatchKind,
  bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Policy {
  semantic_role: u8,
  gc_policy: u8,
  physical_copy_policy: u8,
  logical_backup_policy: u8,
  data_export_policy: u8,
  peer_replication_policy: u8,
  cluster_join_policy: u8,
  client_sync_policy: u8,
  import_policy: u8,
  verify_policy: u8,
  repair_policy: u8,
  migration_policy: u8,
  spill_policy: u8,
  sensitivity: u8,
  event_policy: u8,
  absence_policy: u8,
  unknown_child_policy: u8,
  index_policy: u8,
}

#[derive(Clone, Debug)]
struct SourceRow {
  family_id: u16,
  label: &'static str,
  policy: Policy,
  matchers: Vec<Matcher>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Descriptor {
  family_id: u16,
  policy: Policy,
  matcher: Matcher,
}

#[derive(Debug)]
struct DecodedRegistry {
  descriptor_count: usize,
  family_count: usize,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct RegistryManifest {
  schema_version: u8,
  campaign_id: String,
  fixture_builder_revision: String,
  registry_magic: String,
  registry_schema_version: u16,
  binary: String,
  byte_length: usize,
  sha256: String,
  crc32_iso_hdlc: String,
  source_row_count: usize,
  descriptor_count: usize,
  family_ids: Vec<String>,
  descriptor_keys: Vec<String>,
  fingerprints: Fingerprints,
  semantic_projection_fingerprints: Fingerprints,
  operational_control_tags: BTreeMap<String, String>,
  external_workspace_kinds: BTreeMap<String, String>,
  source_rows: Vec<ManifestSourceRow>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Fingerprints {
  blake3_256: String,
  sha512: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ManifestSourceRow {
  family_id: String,
  label: String,
  matcher_count: usize,
  descriptor_keys: Vec<String>,
  policy: ManifestPolicy,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ManifestPolicy {
  semantic_role: u8,
  gc_policy: u8,
  physical_copy_policy: u8,
  logical_backup_policy: u8,
  data_export_policy: u8,
  peer_replication_policy: u8,
  cluster_join_policy: u8,
  client_sync_policy: u8,
  import_policy: u8,
  verify_policy: u8,
  repair_policy: u8,
  migration_policy: u8,
  spill_policy: u8,
  sensitivity: u8,
  event_policy: u8,
  absence_policy: u8,
  unknown_child_policy: u8,
  index_policy: u8,
}

impl From<Policy> for ManifestPolicy {
  fn from(value: Policy) -> Self {
    Self {
      semantic_role: value.semantic_role,
      gc_policy: value.gc_policy,
      physical_copy_policy: value.physical_copy_policy,
      logical_backup_policy: value.logical_backup_policy,
      data_export_policy: value.data_export_policy,
      peer_replication_policy: value.peer_replication_policy,
      cluster_join_policy: value.cluster_join_policy,
      client_sync_policy: value.client_sync_policy,
      import_policy: value.import_policy,
      verify_policy: value.verify_policy,
      repair_policy: value.repair_policy,
      migration_policy: value.migration_policy,
      spill_policy: value.spill_policy,
      sensitivity: value.sensitivity,
      event_policy: value.event_policy,
      absence_policy: value.absence_policy,
      unknown_child_policy: value.unknown_child_policy,
      index_policy: value.index_policy,
    }
  }
}

pub fn fixture_cases() -> Vec<SystemFamilyFixtureCase> {
  let bytes = build_registry();
  let decoded = decode_registry(&bytes).expect("ASFR fixture must decode");
  [HashProfile::Blake3_256, HashProfile::Sha512]
    .into_iter()
    .map(|profile| SystemFamilyFixtureCase {
      id: match profile {
        HashProfile::Blake3_256 => "asfr-blake3-256-registry-v1-valid",
        HashProfile::Sha512 => "asfr-sha512-registry-v1-valid",
      },
      format: SystemFamilyFormat::RegistryV1,
      profile,
      expected: leak(format!("system-family:registry:descriptors={}:families={}", decoded.descriptor_count, decoded.family_count)),
      relation: Some("same-canonical-registry-bytes:selected-width-fingerprint"),
      canonical_key: Some(hex::encode(operational_fingerprint(profile, &bytes))),
      bytes: bytes.clone(),
    })
    .collect()
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_registry(bytes) {
    Ok(decoded) => (
      format!("system-family:registry:descriptors={}:families={}", decoded.descriptor_count, decoded.family_count),
      Some(hex::encode(operational_fingerprint(profile, bytes))),
    ),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(bytes: &[u8]) -> Vec<String> {
  vec![
    "registry +0x000 len 4: ASFR".to_string(),
    "registry +0x004 len 2: schema_version=1".to_string(),
    "registry +0x006 len 2: header_length=32".to_string(),
    "registry +0x008 len 4: total_length".to_string(),
    "registry +0x00c len 4: descriptor_count".to_string(),
    "registry +0x010 len 4: descriptors_length".to_string(),
    "registry +0x014 len 4: flags=0".to_string(),
    "registry +0x018 len 8: reserved=0".to_string(),
    format!("registry +0x020 len {}: canonical descriptors", bytes.len().saturating_sub(36)),
    "registry final len 4: CRC-32/ISO-HDLC".to_string(),
  ]
}

pub fn generate(fixture_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
  let bytes = build_registry();
  let manifest = build_manifest(&bytes)?;
  fs::write(fixture_root.join("system-family-registry-v1.bin"), &bytes)?;
  fs::write(fixture_root.join("system-family-registry-v1.manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
  verify(fixture_root)
}

pub fn verify(fixture_root: &Path) -> Result<(), Box<dyn std::error::Error>> {
  let expected_bytes = build_registry();
  let actual_bytes = fs::read(fixture_root.join("system-family-registry-v1.bin"))?;
  if actual_bytes != expected_bytes {
    return Err("SystemFamily binary differs from independent construction".into());
  }
  decode_registry(&actual_bytes)?;
  let expected_manifest = build_manifest(&actual_bytes)?;
  let actual_manifest: RegistryManifest = serde_json::from_slice(&fs::read(fixture_root.join("system-family-registry-v1.manifest.json"))?)?;
  if actual_manifest != expected_manifest {
    return Err("SystemFamily manifest differs from fresh independent construction".into());
  }
  Ok(())
}

pub fn fingerprint(profile: HashProfile) -> Vec<u8> {
  operational_fingerprint(profile, &build_registry())
}

fn source_rows() -> Vec<SourceRow> {
  let path_exact = |path: &'static str| matcher(StorageDomain::Path, MatchKind::AbsolutePathExact, path.as_bytes());
  let path_prefix = |path: &'static str| matcher(StorageDomain::Path, MatchKind::AbsolutePathPrefix, path.as_bytes());
  let descendant_file = |segment: &'static str, suffix: &'static str| {
    let mut bytes = Vec::with_capacity(4 + segment.len() + suffix.len());
    put_u16_vec(&mut bytes, segment.len() as u16);
    bytes.extend_from_slice(segment.as_bytes());
    put_u16_vec(&mut bytes, suffix.len() as u16);
    bytes.extend_from_slice(suffix.as_bytes());
    matcher(StorageDomain::Path, MatchKind::DescendantReservedFile, &bytes)
  };
  let descendant_subtree = |segment: &'static str| {
    let mut bytes = Vec::with_capacity(2 + segment.len());
    put_u16_vec(&mut bytes, segment.len() as u16);
    bytes.extend_from_slice(segment.as_bytes());
    matcher(StorageDomain::Path, MatchKind::DescendantReservedSubtree, &bytes)
  };
  let scalar = |domain, kind, value: u16| matcher(domain, kind, &value.to_le_bytes());
  let entry_type = |value| scalar(StorageDomain::EntryType, MatchKind::EntryTypeExact, value);
  let control = |value| scalar(StorageDomain::ControlRegion, MatchKind::ControlTagExact, value);
  let workspace = |value| scalar(StorageDomain::ExternalWorkspace, MatchKind::WorkspaceKindExact, value);
  let kv_prefix = |bytes: &'static [u8]| matcher(StorageDomain::KvKeyPrefix, MatchKind::KvKeyPrefix, bytes);

  let rows = vec![
    row(0x0001, "root_index_config", policy(0x0001, 1, 0x01, 1, 1, 3), vec![path_exact("/.aeordb-config/indexes.json")]),
    row(0x0002, "descendant_index_config", policy(0x0002, 1, 0x01, 1, 1, 1), vec![descendant_file(".aeordb-config", "indexes.json")]),
    row(0x0003, "parser_config", policy(0x0003, 1, 0x01, 1, 1, 3), vec![path_exact("/.aeordb-config/parsers.json")]),
    row(0x0004, "lifecycle_config", policy(0x0004, 0, 0x02, 1, 1, 3), vec![path_exact("/.aeordb-config/lifecycle.json")]),
    row(0x0005, "cron_config", policy(0x0005, 0, 0x02, 1, 1, 3), vec![path_exact("/.aeordb-config/cron.json")]),
    row(0x0006, "webhook_config", policy(0x0006, 0, 0x02, 1, 1, 3), vec![path_exact("/.aeordb-config/webhooks.json")]),
    row(0x0007, "cors_config", policy(0x0007, 0, 0x02, 1, 1, 3), vec![path_exact("/.aeordb-config/cors.json")]),
    row(0x0008, "descendant_config", policy(0x0008, 0, 0x01, 1, 1, 1), vec![descendant_subtree(".aeordb-config")]),
    row(0x0009, "runtime_config", policy(0x0009, 0, 0x02, 2, 3, 3), vec![path_exact("/.aeordb-config/runtime.json")]),
    row(0x0010, "users", policy(0x0010, 0, 0x02, 1, 1, 3), vec![path_prefix("/.aeordb-system/users/")]),
    row(0x0011, "groups", policy(0x0011, 0, 0x02, 1, 1, 3), vec![path_prefix("/.aeordb-system/groups/")]),
    row(0x0012, "central_permissions", policy(0x0012, 0, 0x02, 1, 1, 3), vec![path_prefix("/.aeordb-system/permissions/")]),
    row(0x0013, "api_keys", policy(0x0013, 0, 0x02, 5, 4, 3), vec![path_prefix("/.aeordb-system/api-keys/")]),
    row(0x0014, "refresh_tokens", policy(0x0014, 0, 0x04, 5, 4, 3), vec![path_prefix("/.aeordb-system/refresh-tokens/")]),
    row(0x0015, "magic_links", policy(0x0015, 0, 0x04, 5, 4, 3), vec![path_prefix("/.aeordb-system/magic-links/")]),
    row(0x0016, "system_config", policy(0x0016, 0, 0x02, 5, 4, 3), vec![path_prefix("/.aeordb-system/config/")]),
    row(0x0017, "email_config", policy(0x0017, 0, 0x02, 5, 4, 3), vec![path_exact("/.aeordb-system/email-config.json")]),
    row(0x0018, "join_audit", policy(0x0018, 0, 0x04, 3, 4, 3), vec![path_prefix("/.aeordb-system/join-audit/")]),
    row(
      0x0019,
      "namespace_permissions",
      policy(0x0019, 0, 0x01, 1, 1, 1),
      vec![path_exact("/.aeordb-permissions"), descendant_file(".aeordb-permissions", "")],
    ),
    row(0x001a, "conflicts", policy(0x001a, 0, 0x04, 1, 3, 3), vec![path_exact("/.aeordb-conflicts"), path_prefix("/.aeordb-conflicts/")]),
    row(0x0020, "node_id", policy(0x0020, 0, 0x02, 3, 4, 3), vec![path_exact("/.aeordb-system/cluster/node_id")]),
    row(0x0021, "cluster_peers", policy(0x0021, 0, 0x02, 1, 4, 3), vec![path_exact("/.aeordb-system/cluster/peers")]),
    row(0x0022, "sync_peers", policy(0x0022, 0, 0x04, 3, 4, 3), vec![path_prefix("/.aeordb-system/sync-peers/")]),
    row(0x0030, "legacy_plugins", policy(0x0030, 0, 0x04, 1, 1, 3), vec![path_prefix("/.aeordb-system/plugins/")]),
    row(0x0031, "plugin_aliases", policy(0x0031, 1, 0x01, 1, 1, 3), vec![path_prefix("/.aeordb-system/plugin-aliases/")]),
    row(0x0032, "plugin_artifacts", policy(0x0032, 2, 0x05, 1, 1, 3), vec![path_prefix("/.aeordb-system/plugin-artifacts/blake3/")]),
    row(0x0033, "semantic_objects", policy(0x0033, 3, 0x05, 1, 1, 3), vec![path_prefix("/.aeordb-system/semantic-objects/")]),
    row(0x0040, "snapshots", policy(0x0040, 0, 0x06, 1, 1, 3), vec![entry_type(0x0005)]),
    row(0x0041, "forks", policy(0x0041, 0, 0x06, 1, 1, 3), vec![entry_type(0x0007)]),
    row(0x0042, "background_tasks", policy(0x0042, 0, 0x06, 3, 4, 3), vec![kv_prefix(b"aeordb.task.v1\0"), control(2)]),
    row(
      0x0043,
      "control_store",
      policy(0x0043, 0, 0x06, 3, 4, 3),
      vec![path_exact("/.aeordb-system/controls/v1"), path_prefix("/.aeordb-system/controls/v1/")],
    ),
    row(0x0044, "legacy_root_lifecycle", policy(0x0044, 0, 0x11, 1, 1, 3), vec![control(1)]),
    row(0x0050, "index_artifacts", policy(0x0050, 4, 0x08, 2, 2, 3), vec![entry_type(0x0009)]),
    row(0x0051, "gc_artifacts", policy(0x0051, 0, 0x14, 3, 4, 3), vec![entry_type(0x000a)]),
    row(0x0052, "deletion_records", policy(0x0052, 0, 0x02, 3, 4, 3), vec![entry_type(0x0004)]),
    row(0x0053, "void_allocator", policy(0x0053, 0, 0x02, 3, 4, 3), vec![entry_type(0x0006), control(3)]),
    row(0x0054, "database_authority", policy(0x0054, 0, 0x02, 3, 4, 3), vec![control(4), control(5)]),
    row(0x0055, "kv_authority", policy(0x0055, 0, 0x02, 3, 4, 3), vec![control(6), control(7), control(8)]),
    row(0x0056, "database_nvt", policy(0x0056, 4, 0x08, 3, 4, 3), vec![control(9)]),
    row(0x0057, "wal_publication", policy(0x0057, 0, 0x02, 3, 4, 3), vec![control(10), control(11)]),
    row(
      0x0060,
      "legacy_indexes",
      policy(0x0060, 4, 0x08, 3, 3, 3),
      vec![path_prefix("/.aeordb-indexes/"), descendant_subtree(".aeordb-indexes")],
    ),
    row(0x0061, "nested_logs", policy(0x0061, 0, 0x04, 3, 3, 3), vec![path_prefix("/.aeordb-logs/"), descendant_subtree(".aeordb-logs")]),
    row(
      0x0062,
      "legacy_snapshots",
      policy(0x0062, 0, 0x20, 3, 3, 3),
      vec![path_exact("/.aeordb-system/snapshots"), path_prefix("/.aeordb-system/snapshots/")],
    ),
    row(
      0x0063,
      "legacy_aliases",
      policy(0x0063, 0, 0x20, 3, 3, 3),
      vec![
        path_exact("/.aeordb-system/apikeys"),
        path_prefix("/.aeordb-system/apikeys/"),
        path_exact("/.aeordb-system/cluster/sync"),
        path_prefix("/.aeordb-system/cluster/sync/"),
      ],
    ),
    row(0x0070, "emergency_spill", policy(0x0070, 0, 0x02, 3, 4, 3), vec![workspace(1)]),
    row(0x0071, "external_workspaces", policy(0x0071, 0, 0x06, 3, 4, 3), vec![workspace(2), workspace(3), workspace(4)]),
  ];
  validate_source_rows(&rows).expect("canonical SystemFamily source rows must be valid");
  rows
}

fn row(family_id: u16, label: &'static str, policy: Policy, matchers: Vec<Matcher>) -> SourceRow {
  SourceRow { family_id, label, policy, matchers }
}

fn matcher(domain: StorageDomain, kind: MatchKind, bytes: &[u8]) -> Matcher {
  Matcher { domain, kind, bytes: bytes.to_vec() }
}

fn policy(
  family_id: u16,
  semantic_role: u8,
  gc_policy: u8,
  logical_backup_policy: u8,
  peer_replication_policy: u8,
  data_policy: u8,
) -> Policy {
  let derived = semantic_role == 4;
  let external = matches!(family_id, 0x0070 | 0x0071);
  let legacy = matches!(family_id, 0x0030 | 0x0062 | 0x0063);
  let credential = matches!(family_id, 0x0013..=0x0015);
  let secret = matches!(family_id, 0x0016 | 0x0017);
  let protected = matches!(family_id, 0x0010..=0x0018 | 0x0020..=0x0057 | 0x0070..=0x0071);
  let operational = matches!(family_id, 0x0018 | 0x0020..=0x0022 | 0x0042..=0x0057 | 0x0061 | 0x0070..=0x0071);
  let has_children = matches!(
    family_id,
    0x0002 | 0x0008 | 0x0010..=0x0016 | 0x0018 | 0x001a | 0x0022 | 0x0030..=0x0033 | 0x0043 | 0x0060..=0x0063
  );
  let absence_policy = match family_id {
    0x0001 | 0x0003 | 0x0005..=0x0007 => 1,
    0x0004 | 0x0051 => 6,
    0x0009 | 0x0050 | 0x0056 | 0x0060 => 4,
    0x0010..=0x0012 | 0x0021 | 0x0032..=0x0033 | 0x0040..=0x0044 | 0x0052..=0x0055 | 0x0057 | 0x0070..=0x0071 => 5,
    0x0013..=0x0018 | 0x001a | 0x0020 | 0x0022 | 0x0030..=0x0031 | 0x0061 => 2,
    0x0062..=0x0063 => 7,
    _ => 1,
  };
  let index_policy = match family_id {
    0x0001..=0x0003 | 0x0031..=0x0033 => 3,
    0x0008 | 0x0019 => 1,
    0x0040..=0x0071 => 0,
    _ => 2,
  };
  Policy {
    semantic_role,
    gc_policy,
    physical_copy_policy: if external { 4 } else { 1 },
    logical_backup_policy,
    data_export_policy: data_policy,
    peer_replication_policy,
    cluster_join_policy: if family_id == 0x0016 { 6 } else { 3 },
    client_sync_policy: data_policy,
    import_policy: if matches!(logical_backup_policy, 1 | 2) { logical_backup_policy } else { 4 },
    verify_policy: if derived {
      3
    } else if gc_policy & 0x20 != 0 {
      4
    } else if absence_policy == 5 {
      2
    } else {
      1
    },
    repair_policy: if derived {
      3
    } else if legacy {
      5
    } else if family_id == 0x0070 {
      4
    } else {
      2
    },
    migration_policy: if derived {
      3
    } else if legacy {
      4
    } else if external || !matches!(logical_backup_policy, 1 | 2) {
      2
    } else {
      1
    },
    spill_policy: match family_id {
      0x0057 => 2,
      0x0070 => 3,
      0x0071 => 4,
      _ => 1,
    },
    sensitivity: if credential {
      2
    } else if secret {
      3
    } else if protected {
      1
    } else {
      0
    },
    event_policy: if matches!(family_id, 0x0002 | 0x0008 | 0x0019) {
      1
    } else if credential || secret {
      4
    } else if operational {
      3
    } else if protected {
      2
    } else {
      0
    },
    absence_policy,
    unknown_child_policy: if has_children { 2 } else { 0 },
    index_policy,
  }
}

fn validate_source_rows(rows: &[SourceRow]) -> Result<(), &'static str> {
  if rows.len() != 46 {
    return Err("source_row_count");
  }
  let mut ids = BTreeSet::new();
  let mut matcher_owners = BTreeMap::new();
  for row in rows {
    if row.family_id == 0 || row.family_id == 0xfffe || !ids.insert(row.family_id) || row.matchers.is_empty() {
      return Err("source_row_identity");
    }
    validate_policy(row.policy)?;
    for matcher in &row.matchers {
      validate_matcher(matcher.domain, matcher.kind, &matcher.bytes)?;
      let key = (matcher.domain as u8, matcher.kind as u8, matcher.bytes.as_slice());
      if matcher_owners.insert(key, row.family_id).is_some_and(|owner| owner != row.family_id) {
        return Err("cross_family_match_overlap");
      }
    }
  }
  Ok(())
}

fn descriptors() -> Vec<Descriptor> {
  let mut descriptors: Vec<_> = source_rows()
    .into_iter()
    .flat_map(|row| row.matchers.into_iter().map(move |matcher| Descriptor { family_id: row.family_id, policy: row.policy, matcher }))
    .collect();
  descriptors.sort_by(descriptor_key_cmp);
  descriptors
}

fn descriptor_key_cmp(left: &Descriptor, right: &Descriptor) -> std::cmp::Ordering {
  (left.family_id, left.matcher.domain as u8, left.matcher.kind as u8, left.matcher.bytes.as_slice()).cmp(&(
    right.family_id,
    right.matcher.domain as u8,
    right.matcher.kind as u8,
    right.matcher.bytes.as_slice(),
  ))
}

fn build_registry() -> Vec<u8> {
  encode_registry(&descriptors()).expect("canonical SystemFamily registry must encode")
}

fn encode_registry(descriptors: &[Descriptor]) -> Result<Vec<u8>, &'static str> {
  if descriptors.is_empty() {
    return Err("descriptor_count");
  }
  let descriptors_length = descriptors.iter().try_fold(0usize, |total, descriptor| {
    DESCRIPTOR_FIXED_LENGTH
      .checked_add(descriptor.matcher.bytes.len())
      .and_then(|length| total.checked_add(length))
      .ok_or("registry_length_overflow")
  })?;
  let total_length =
    HEADER_LENGTH.checked_add(descriptors_length).and_then(|length| length.checked_add(CRC_LENGTH)).ok_or("registry_length_overflow")?;
  if total_length > MAX_REGISTRY_LENGTH || descriptors.len() > u32::MAX as usize || descriptors_length > u32::MAX as usize {
    return Err("registry_bounds");
  }
  let mut bytes = vec![0u8; HEADER_LENGTH];
  bytes[0..4].copy_from_slice(MAGIC);
  put_u16(&mut bytes, 4, VERSION)?;
  put_u16(&mut bytes, 6, HEADER_LENGTH as u16)?;
  put_u32(&mut bytes, 8, total_length as u32)?;
  put_u32(&mut bytes, 12, descriptors.len() as u32)?;
  put_u32(&mut bytes, 16, descriptors_length as u32)?;
  for descriptor in descriptors {
    validate_descriptor(descriptor)?;
    let start = bytes.len();
    bytes.resize(start + DESCRIPTOR_FIXED_LENGTH, 0);
    put_u16(&mut bytes, start, descriptor.family_id)?;
    bytes[start + 2] = descriptor.matcher.domain as u8;
    bytes[start + 3] = descriptor.matcher.kind as u8;
    let policy = descriptor.policy;
    bytes[start + 4..start + 22].copy_from_slice(&[
      policy.semantic_role,
      policy.gc_policy,
      policy.physical_copy_policy,
      policy.logical_backup_policy,
      policy.data_export_policy,
      policy.peer_replication_policy,
      policy.cluster_join_policy,
      policy.client_sync_policy,
      policy.import_policy,
      policy.verify_policy,
      policy.repair_policy,
      policy.migration_policy,
      policy.spill_policy,
      policy.sensitivity,
      policy.event_policy,
      policy.absence_policy,
      policy.unknown_child_policy,
      policy.index_policy,
    ]);
    put_u16(&mut bytes, start + 28, descriptor.matcher.bytes.len() as u16)?;
    bytes.extend_from_slice(&descriptor.matcher.bytes);
  }
  let crc = crc32fast::hash(&bytes);
  bytes.extend_from_slice(&crc.to_le_bytes());
  Ok(bytes)
}

fn decode_registry(bytes: &[u8]) -> Result<DecodedRegistry, &'static str> {
  let descriptors = decode_descriptors(bytes)?;
  let family_count = descriptors.iter().map(|descriptor| descriptor.family_id).collect::<BTreeSet<_>>().len();
  Ok(DecodedRegistry { descriptor_count: descriptors.len(), family_count })
}

fn decode_descriptors(bytes: &[u8]) -> Result<Vec<Descriptor>, &'static str> {
  if bytes.len() < HEADER_LENGTH + CRC_LENGTH || bytes.len() > MAX_REGISTRY_LENGTH {
    return Err("registry_length");
  }
  if &bytes[0..4] != MAGIC {
    return Err("registry_magic");
  }
  if read_u16(bytes, 4)? != VERSION || read_u16(bytes, 6)? as usize != HEADER_LENGTH {
    return Err("registry_version_or_header_length");
  }
  if read_u32(bytes, 8)? as usize != bytes.len() {
    return Err("registry_total_length");
  }
  let descriptor_count = read_u32(bytes, 12)? as usize;
  let descriptors_length = read_u32(bytes, 16)? as usize;
  if descriptor_count == 0 || descriptors_length != bytes.len() - HEADER_LENGTH - CRC_LENGTH {
    return Err("registry_descriptor_lengths");
  }
  if read_u32(bytes, 20)? != 0 || bytes[24..32].iter().any(|byte| *byte != 0) {
    return Err("registry_reserved");
  }
  let stored_crc = read_u32(bytes, bytes.len() - CRC_LENGTH)?;
  if stored_crc != crc32fast::hash(&bytes[..bytes.len() - CRC_LENGTH]) {
    return Err("registry_crc");
  }
  let descriptor_end = bytes.len() - CRC_LENGTH;
  let mut offset = HEADER_LENGTH;
  let mut descriptors = Vec::with_capacity(descriptor_count.min(4_096));
  let mut family_policies = BTreeMap::new();
  for _ in 0..descriptor_count {
    let fixed_end = offset.checked_add(DESCRIPTOR_FIXED_LENGTH).ok_or("descriptor_overflow")?;
    if fixed_end > descriptor_end {
      return Err("descriptor_truncated");
    }
    let family_id = read_u16(bytes, offset)?;
    let domain = decode_storage_domain(bytes[offset + 2])?;
    let kind = decode_match_kind(bytes[offset + 3])?;
    let policy = Policy {
      semantic_role: bytes[offset + 4],
      gc_policy: bytes[offset + 5],
      physical_copy_policy: bytes[offset + 6],
      logical_backup_policy: bytes[offset + 7],
      data_export_policy: bytes[offset + 8],
      peer_replication_policy: bytes[offset + 9],
      cluster_join_policy: bytes[offset + 10],
      client_sync_policy: bytes[offset + 11],
      import_policy: bytes[offset + 12],
      verify_policy: bytes[offset + 13],
      repair_policy: bytes[offset + 14],
      migration_policy: bytes[offset + 15],
      spill_policy: bytes[offset + 16],
      sensitivity: bytes[offset + 17],
      event_policy: bytes[offset + 18],
      absence_policy: bytes[offset + 19],
      unknown_child_policy: bytes[offset + 20],
      index_policy: bytes[offset + 21],
    };
    if bytes[offset + 22..offset + 24].iter().any(|byte| *byte != 0)
      || read_u32(bytes, offset + 24)? != 0
      || read_u16(bytes, offset + 30)? != 0
    {
      return Err("descriptor_reserved");
    }
    let matcher_length = read_u16(bytes, offset + 28)? as usize;
    let matcher_end = fixed_end.checked_add(matcher_length).ok_or("descriptor_overflow")?;
    if matcher_end > descriptor_end {
      return Err("matcher_truncated");
    }
    let descriptor = Descriptor { family_id, policy, matcher: Matcher { domain, kind, bytes: bytes[fixed_end..matcher_end].to_vec() } };
    validate_descriptor(&descriptor)?;
    if let Some(previous) = descriptors.last() {
      if descriptor_key_cmp(previous, &descriptor) != std::cmp::Ordering::Less {
        return Err("descriptor_order_or_duplicate");
      }
    }
    if family_policies.insert(family_id, policy).is_some_and(|previous| previous != policy) {
      return Err("family_policy_mismatch");
    }
    descriptors.push(descriptor);
    offset = matcher_end;
  }
  if offset != descriptor_end || descriptors.len() != descriptor_count {
    return Err("descriptor_count_or_trailing_bytes");
  }
  Ok(descriptors)
}

fn validate_descriptor(descriptor: &Descriptor) -> Result<(), &'static str> {
  if descriptor.family_id == 0 || descriptor.family_id == 0xfffe || descriptor.matcher.bytes.len() > u16::MAX as usize {
    return Err("descriptor_identity");
  }
  validate_policy(descriptor.policy)?;
  validate_matcher(descriptor.matcher.domain, descriptor.matcher.kind, &descriptor.matcher.bytes)
}

fn validate_policy(policy: Policy) -> Result<(), &'static str> {
  if policy.semantic_role > 4
    || policy.gc_policy == 0
    || policy.gc_policy & !0x3f != 0
    || !in_range(policy.physical_copy_policy, 1, 7)
    || !in_range(policy.logical_backup_policy, 1, 7)
    || !in_range(policy.data_export_policy, 1, 7)
    || !in_range(policy.peer_replication_policy, 1, 7)
    || !in_range(policy.cluster_join_policy, 1, 7)
    || !in_range(policy.client_sync_policy, 1, 7)
    || !in_range(policy.import_policy, 1, 7)
    || !in_range(policy.verify_policy, 1, 4)
    || !in_range(policy.repair_policy, 1, 5)
    || !in_range(policy.migration_policy, 1, 6)
    || !in_range(policy.spill_policy, 1, 4)
    || policy.sensitivity > 4
    || policy.event_policy > 4
    || !in_range(policy.absence_policy, 1, 7)
    || policy.unknown_child_policy > 3
    || policy.index_policy > 3
  {
    return Err("descriptor_policy_enum");
  }
  Ok(())
}

fn validate_matcher(domain: StorageDomain, kind: MatchKind, bytes: &[u8]) -> Result<(), &'static str> {
  let compatible = matches!(
    (domain, kind),
    (
      StorageDomain::Path,
      MatchKind::AbsolutePathExact
        | MatchKind::AbsolutePathPrefix
        | MatchKind::DescendantReservedFile
        | MatchKind::DescendantReservedSubtree
        | MatchKind::ReservedPathSegment
    ) | (StorageDomain::EntryType, MatchKind::EntryTypeExact)
      | (StorageDomain::KvKeyPrefix, MatchKind::KvKeyPrefix)
      | (StorageDomain::ControlRegion, MatchKind::ControlTagExact)
      | (StorageDomain::ExternalWorkspace, MatchKind::WorkspaceKindExact)
  );
  if !compatible {
    return Err("matcher_domain_kind");
  }
  match kind {
    MatchKind::AbsolutePathExact | MatchKind::AbsolutePathPrefix => {
      let path = std::str::from_utf8(bytes).map_err(|_| "matcher_path_utf8")?;
      validate_absolute_path(path)?;
      if kind == MatchKind::AbsolutePathExact && path.len() > 1 && path.ends_with('/') {
        return Err("matcher_exact_shape");
      }
      if kind == MatchKind::AbsolutePathPrefix && (path == "/" || !path.ends_with('/')) {
        return Err("matcher_prefix_shape");
      }
    }
    MatchKind::DescendantReservedFile => {
      if bytes.len() < 4 {
        return Err("matcher_descendant_file_length");
      }
      let segment_length = read_u16(bytes, 0)? as usize;
      let suffix_length_offset = 2usize.checked_add(segment_length).ok_or("matcher_length_overflow")?;
      if suffix_length_offset + 2 > bytes.len() {
        return Err("matcher_descendant_file_length");
      }
      let suffix_length = read_u16(bytes, suffix_length_offset)? as usize;
      if suffix_length_offset + 2 + suffix_length != bytes.len() {
        return Err("matcher_descendant_file_length");
      }
      validate_segment(&bytes[2..suffix_length_offset])?;
      if suffix_length > 0 {
        validate_relative_path(&bytes[suffix_length_offset + 2..])?;
      }
    }
    MatchKind::DescendantReservedSubtree | MatchKind::ReservedPathSegment => {
      if bytes.len() < 3 {
        return Err("matcher_segment_length");
      }
      let segment_length = read_u16(bytes, 0)? as usize;
      if segment_length + 2 != bytes.len() {
        return Err("matcher_segment_length");
      }
      validate_segment(&bytes[2..])?;
    }
    MatchKind::EntryTypeExact | MatchKind::ControlTagExact | MatchKind::WorkspaceKindExact => {
      if bytes.len() != 2 || read_u16(bytes, 0)? == 0 {
        return Err("matcher_scalar");
      }
    }
    MatchKind::KvKeyPrefix => {
      if bytes.is_empty() {
        return Err("matcher_kv_prefix");
      }
    }
  }
  Ok(())
}

fn validate_absolute_path(path: &str) -> Result<(), &'static str> {
  if !path.starts_with('/') || path.as_bytes().contains(&0) {
    return Err("matcher_path_shape");
  }
  if path.contains("//") {
    return Err("matcher_path_separator");
  }
  let core = path.strip_suffix('/').unwrap_or(path);
  if core.split('/').skip(1).any(|segment| segment.is_empty() || matches!(segment, "." | "..")) {
    return Err("matcher_path_segment");
  }
  Ok(())
}

fn validate_relative_path(bytes: &[u8]) -> Result<(), &'static str> {
  let path = std::str::from_utf8(bytes).map_err(|_| "matcher_relative_utf8")?;
  if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains("//") || path.as_bytes().contains(&0) {
    return Err("matcher_relative_shape");
  }
  if path.split('/').any(|segment| segment.is_empty() || matches!(segment, "." | "..")) {
    return Err("matcher_relative_segment");
  }
  Ok(())
}

fn validate_segment(bytes: &[u8]) -> Result<(), &'static str> {
  let segment = std::str::from_utf8(bytes).map_err(|_| "matcher_segment_utf8")?;
  if segment.is_empty() || segment.contains('/') || segment.as_bytes().contains(&0) || matches!(segment, "." | "..") {
    return Err("matcher_segment_shape");
  }
  Ok(())
}

fn build_manifest(bytes: &[u8]) -> Result<RegistryManifest, Box<dyn std::error::Error>> {
  let decoded = decode_descriptors(bytes)?;
  let rows = source_rows();
  let mut descriptors_by_family: BTreeMap<u16, Vec<String>> = BTreeMap::new();
  for descriptor in &decoded {
    descriptors_by_family.entry(descriptor.family_id).or_default().push(descriptor_key(descriptor));
  }
  let family_ids = rows.iter().map(|row| format!("0x{:04x}", row.family_id)).collect();
  let descriptor_keys = decoded.iter().map(descriptor_key).collect();
  let source_rows: Vec<ManifestSourceRow> = rows
    .into_iter()
    .map(|row| ManifestSourceRow {
      family_id: format!("0x{:04x}", row.family_id),
      label: row.label.to_string(),
      matcher_count: row.matchers.len(),
      descriptor_keys: descriptors_by_family.remove(&row.family_id).unwrap_or_default(),
      policy: row.policy.into(),
    })
    .collect();
  let mut operational_control_tags = BTreeMap::new();
  for (id, label) in control_tags() {
    operational_control_tags.insert(id.to_string(), label.to_string());
  }
  let mut external_workspace_kinds = BTreeMap::new();
  for (id, label) in workspace_kinds() {
    external_workspace_kinds.insert(id.to_string(), label.to_string());
  }
  Ok(RegistryManifest {
    schema_version: 1,
    campaign_id: CAMPAIGN_ID.to_string(),
    fixture_builder_revision: BUILDER_REVISION.to_string(),
    registry_magic: "ASFR".to_string(),
    registry_schema_version: VERSION,
    binary: "system-family-registry-v1.bin".to_string(),
    byte_length: bytes.len(),
    sha256: sha256_hex(bytes),
    crc32_iso_hdlc: format!("{:08x}", read_u32(bytes, bytes.len() - CRC_LENGTH)?),
    source_row_count: source_rows.len(),
    descriptor_count: decoded.len(),
    family_ids,
    descriptor_keys,
    fingerprints: Fingerprints {
      blake3_256: hex::encode(operational_fingerprint(HashProfile::Blake3_256, bytes)),
      sha512: hex::encode(operational_fingerprint(HashProfile::Sha512, bytes)),
    },
    semantic_projection_fingerprints: Fingerprints {
      blake3_256: hex::encode(semantic_projection_fingerprint(HashProfile::Blake3_256, &decoded)),
      sha512: hex::encode(semantic_projection_fingerprint(HashProfile::Sha512, &decoded)),
    },
    operational_control_tags,
    external_workspace_kinds,
    source_rows,
  })
}

fn descriptor_key(descriptor: &Descriptor) -> String {
  format!(
    "0x{:04x}:{:02x}:{:02x}:{}",
    descriptor.family_id,
    descriptor.matcher.domain as u8,
    descriptor.matcher.kind as u8,
    hex::encode(&descriptor.matcher.bytes)
  )
}

fn operational_fingerprint(profile: HashProfile, bytes: &[u8]) -> Vec<u8> {
  hash_parts(profile, &[b"aeordb.system-family-registry.v1\0", bytes])
}

fn semantic_projection_fingerprint(profile: HashProfile, descriptors: &[Descriptor]) -> Vec<u8> {
  let mut projection = Vec::new();
  for descriptor in descriptors.iter().filter(|descriptor| descriptor.policy.semantic_role != 0) {
    projection.extend_from_slice(&descriptor.family_id.to_le_bytes());
    projection.push(descriptor.matcher.domain as u8);
    projection.push(descriptor.matcher.kind as u8);
    projection.extend_from_slice(&(descriptor.matcher.bytes.len() as u16).to_le_bytes());
    projection.extend_from_slice(&descriptor.matcher.bytes);
    projection.push(descriptor.policy.semantic_role);
    projection.push(descriptor.policy.index_policy);
  }
  hash_parts(profile, &[b"aeordb.system-family-semantic-projection.v1\0", &projection])
}

fn hash_parts(profile: HashProfile, parts: &[&[u8]]) -> Vec<u8> {
  match profile {
    HashProfile::Blake3_256 => {
      let mut hasher = blake3::Hasher::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().as_bytes().to_vec()
    }
    HashProfile::Sha512 => {
      let mut hasher = sha2::Sha512::new();
      for part in parts {
        hasher.update(part);
      }
      hasher.finalize().to_vec()
    }
  }
}

fn control_tags() -> &'static [(u16, &'static str)] {
  &[
    (1, "legacy_root_and_root_lifecycle_evidence"),
    (2, "background_task_registry"),
    (3, "void_allocator_hot_tail"),
    (4, "database_header_slots"),
    (5, "head_base_target_authority"),
    (6, "kv_blocks_pages"),
    (7, "kv_resize_buffers"),
    (8, "locator_snapshot"),
    (9, "database_nvt_region"),
    (10, "wal_hot_tail"),
    (11, "buffer_publication_controls"),
  ]
}

fn workspace_kinds() -> &'static [(u16, &'static str)] {
  &[(1, "emergency_spill"), (2, "migration_workspace"), (3, "gc_mark_workspace"), (4, "index_workspace")]
}

#[cfg(test)]
fn classify_path(path: &str) -> Result<Option<u16>, &'static str> {
  validate_absolute_path(path)?;
  if path.len() > 1 && path.ends_with('/') {
    return Err("matcher_exact_shape");
  }
  let descriptors = descriptors();
  let mut winners: Vec<(u8, usize, u16)> = Vec::new();
  for descriptor in descriptors.iter().filter(|descriptor| descriptor.matcher.domain == StorageDomain::Path) {
    if let Some((priority, specificity)) = path_match_score(path, &descriptor.matcher)? {
      winners.push((priority, specificity, descriptor.family_id));
    }
  }
  winners.sort_unstable_by_key(|winner| std::cmp::Reverse((winner.0, winner.1)));
  if let Some((priority, specificity, family)) = winners.first().copied() {
    if winners.iter().any(|winner| winner.0 == priority && winner.1 == specificity && winner.2 != family) {
      return Err("cross_family_match_overlap");
    }
    return Ok(Some(family));
  }
  if path.split('/').any(|segment| segment.starts_with(".aeordb-")) {
    Ok(Some(0xfffe))
  } else {
    Ok(None)
  }
}

#[cfg(test)]
fn path_match_score(path: &str, matcher: &Matcher) -> Result<Option<(u8, usize)>, &'static str> {
  let matched = match matcher.kind {
    MatchKind::AbsolutePathExact => (path.as_bytes() == matcher.bytes).then_some((5, matcher.bytes.len())),
    MatchKind::AbsolutePathPrefix => path.as_bytes().starts_with(&matcher.bytes).then_some((2, matcher.bytes.len())),
    MatchKind::DescendantReservedFile => {
      let (segment, suffix) = decode_descendant_file(&matcher.bytes)?;
      let parts: Vec<_> = path.split('/').skip(1).collect();
      parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
          if index == 0 || *part != segment {
            return None;
          }
          let remaining = parts[index + 1..].join("/");
          (remaining == suffix).then_some(index)
        })
        .max()
        .map(|index| (4, index * u16::MAX as usize + matcher.bytes.len()))
    }
    MatchKind::DescendantReservedSubtree => {
      let segment = decode_segment_matcher(&matcher.bytes)?;
      let parts: Vec<_> = path.split('/').skip(1).collect();
      parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| (index > 0 && *part == segment && index + 1 < parts.len()).then_some(index))
        .max()
        .map(|index| (3, index * u16::MAX as usize + matcher.bytes.len()))
    }
    MatchKind::ReservedPathSegment => {
      let segment = decode_segment_matcher(&matcher.bytes)?;
      path
        .split('/')
        .skip(1)
        .enumerate()
        .filter_map(|(index, part)| (part == segment).then_some(index))
        .max()
        .map(|index| (1, index * u16::MAX as usize + matcher.bytes.len()))
    }
    _ => None,
  };
  Ok(matched)
}

#[cfg(test)]
fn decode_descendant_file(bytes: &[u8]) -> Result<(&str, &str), &'static str> {
  let segment_length = read_u16(bytes, 0)? as usize;
  let suffix_offset = 2 + segment_length;
  let suffix_length = read_u16(bytes, suffix_offset)? as usize;
  let segment = std::str::from_utf8(&bytes[2..suffix_offset]).map_err(|_| "matcher_path_utf8")?;
  let suffix = std::str::from_utf8(&bytes[suffix_offset + 2..suffix_offset + 2 + suffix_length]).map_err(|_| "matcher_path_utf8")?;
  Ok((segment, suffix))
}

#[cfg(test)]
fn decode_segment_matcher(bytes: &[u8]) -> Result<&str, &'static str> {
  let length = read_u16(bytes, 0)? as usize;
  std::str::from_utf8(&bytes[2..2 + length]).map_err(|_| "matcher_path_utf8")
}

#[cfg(test)]
fn classify_scalar(domain: StorageDomain, value: u16) -> Result<Option<u16>, &'static str> {
  let matches: BTreeSet<_> = descriptors()
    .into_iter()
    .filter(|descriptor| descriptor.matcher.domain == domain && descriptor.matcher.bytes == value.to_le_bytes())
    .map(|descriptor| descriptor.family_id)
    .collect();
  if matches.len() > 1 {
    return Err("cross_family_match_overlap");
  }
  Ok(matches.into_iter().next())
}

#[cfg(test)]
fn classify_kv_key(key: &[u8]) -> Result<Option<u16>, &'static str> {
  let mut winners: Vec<_> = descriptors()
    .into_iter()
    .filter(|descriptor| descriptor.matcher.domain == StorageDomain::KvKeyPrefix && key.starts_with(&descriptor.matcher.bytes))
    .map(|descriptor| (descriptor.matcher.bytes.len(), descriptor.family_id))
    .collect();
  winners.sort_unstable_by(|left, right| right.cmp(left));
  if let Some((length, family)) = winners.first().copied() {
    if winners.iter().any(|winner| winner.0 == length && winner.1 != family) {
      return Err("cross_family_match_overlap");
    }
    Ok(Some(family))
  } else {
    Ok(None)
  }
}

fn decode_storage_domain(value: u8) -> Result<StorageDomain, &'static str> {
  match value {
    1 => Ok(StorageDomain::Path),
    2 => Ok(StorageDomain::EntryType),
    3 => Ok(StorageDomain::KvKeyPrefix),
    4 => Ok(StorageDomain::ControlRegion),
    5 => Ok(StorageDomain::ExternalWorkspace),
    _ => Err("storage_domain_enum"),
  }
}

fn decode_match_kind(value: u8) -> Result<MatchKind, &'static str> {
  match value {
    1 => Ok(MatchKind::AbsolutePathExact),
    2 => Ok(MatchKind::AbsolutePathPrefix),
    3 => Ok(MatchKind::DescendantReservedFile),
    4 => Ok(MatchKind::DescendantReservedSubtree),
    5 => Ok(MatchKind::ReservedPathSegment),
    6 => Ok(MatchKind::EntryTypeExact),
    7 => Ok(MatchKind::KvKeyPrefix),
    8 => Ok(MatchKind::ControlTagExact),
    9 => Ok(MatchKind::WorkspaceKindExact),
    _ => Err("match_kind_enum"),
  }
}

fn in_range(value: u8, minimum: u8, maximum: u8) -> bool {
  (minimum..=maximum).contains(&value)
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), &'static str> {
  let target = bytes.get_mut(offset..offset + 2).ok_or("write_bounds")?;
  target.copy_from_slice(&value.to_le_bytes());
  Ok(())
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), &'static str> {
  let target = bytes.get_mut(offset..offset + 4).ok_or("write_bounds")?;
  target.copy_from_slice(&value.to_le_bytes());
  Ok(())
}

fn put_u16_vec(bytes: &mut Vec<u8>, value: u16) {
  bytes.extend_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  let source: [u8; 2] = bytes.get(offset..offset + 2).ok_or("read_bounds")?.try_into().map_err(|_| "read_bounds")?;
  Ok(u16::from_le_bytes(source))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let source: [u8; 4] = bytes.get(offset..offset + 4).ok_or("read_bounds")?.try_into().map_err(|_| "read_bounds")?;
  Ok(u32::from_le_bytes(source))
}

#[cfg(test)]
fn repair_crc(bytes: &mut [u8]) {
  let crc_offset = bytes.len() - CRC_LENGTH;
  let crc = crc32fast::hash(&bytes[..crc_offset]);
  bytes[crc_offset..].copy_from_slice(&crc.to_le_bytes());
}

fn sha256_hex(bytes: &[u8]) -> String {
  hex::encode(Sha256::digest(bytes))
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn canonical_registry_round_trips_all_rows_and_descriptors() {
    let bytes = build_registry();
    let decoded = decode_registry(&bytes).unwrap();
    assert_eq!(decoded.family_count, 46);
    assert!(decoded.descriptor_count > 46);
    assert_eq!(source_rows().len(), 46);
    assert_eq!(read_u32(&bytes, 8).unwrap() as usize, bytes.len());
  }

  #[test]
  fn every_registry_byte_is_crc_or_structure_protected() {
    let canonical = build_registry();
    for offset in 0..canonical.len() {
      let mut mutated = canonical.clone();
      mutated[offset] ^= 1;
      assert!(decode_registry(&mutated).is_err(), "byte {offset} was not protected");
    }
  }

  #[test]
  fn repaired_crc_rejects_reserved_enum_policy_and_count_corruption() {
    for (offset, value, expected) in [
      (20, 1, "registry_reserved"),
      (24, 1, "registry_reserved"),
      (HEADER_LENGTH + 2, 0xff, "storage_domain_enum"),
      (HEADER_LENGTH + 3, 0xff, "match_kind_enum"),
      (HEADER_LENGTH + 4, 0xff, "descriptor_policy_enum"),
      (HEADER_LENGTH + 22, 1, "descriptor_reserved"),
      (HEADER_LENGTH + 24, 1, "descriptor_reserved"),
    ] {
      let mut bytes = build_registry();
      bytes[offset] = value;
      repair_crc(&mut bytes);
      assert_eq!(decode_registry(&bytes).unwrap_err(), expected);
    }
    let mut zero_count = build_registry();
    zero_count[12..16].copy_from_slice(&0u32.to_le_bytes());
    repair_crc(&mut zero_count);
    assert_eq!(decode_registry(&zero_count).unwrap_err(), "registry_descriptor_lengths");
  }

  #[test]
  fn decoder_rejects_truncation_trailing_bytes_and_amplification() {
    let canonical = build_registry();
    for length in 0..HEADER_LENGTH + CRC_LENGTH {
      assert!(decode_registry(&canonical[..length.min(canonical.len())]).is_err());
    }
    let mut trailing = canonical.clone();
    trailing.insert(trailing.len() - CRC_LENGTH, 0);
    let trailing_length = trailing.len() as u32;
    trailing[8..12].copy_from_slice(&trailing_length.to_le_bytes());
    let descriptor_bytes = trailing.len() - HEADER_LENGTH - CRC_LENGTH;
    trailing[16..20].copy_from_slice(&(descriptor_bytes as u32).to_le_bytes());
    repair_crc(&mut trailing);
    assert!(decode_registry(&trailing).is_err());

    let mut amplified = canonical;
    amplified[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    repair_crc(&mut amplified);
    assert!(decode_registry(&amplified).is_err());
  }

  #[test]
  fn descriptor_order_duplicates_and_family_policy_drift_are_rejected() {
    let canonical = decode_descriptors(&build_registry()).unwrap();
    let mut reversed = canonical.clone();
    reversed.swap(0, 1);
    assert_eq!(decode_registry(&encode_registry(&reversed).unwrap()).unwrap_err(), "descriptor_order_or_duplicate");

    let mut duplicate = canonical.clone();
    duplicate.insert(1, duplicate[0].clone());
    assert_eq!(decode_registry(&encode_registry(&duplicate).unwrap()).unwrap_err(), "descriptor_order_or_duplicate");

    let mut drift = canonical;
    let family = drift.iter().position(|descriptor| descriptor.family_id == 0x0019).unwrap();
    let second = drift.iter().rposition(|descriptor| descriptor.family_id == 0x0019).unwrap();
    drift[second].policy.event_policy = 2;
    assert_ne!(family, second);
    assert_eq!(decode_registry(&encode_registry(&drift).unwrap()).unwrap_err(), "family_policy_mismatch");
  }

  #[test]
  fn path_specificity_and_unknown_protected_behavior_are_frozen() {
    assert_eq!(classify_path("/.aeordb-config/indexes.json").unwrap(), Some(0x0001));
    assert_eq!(classify_path("/docs/.aeordb-config/indexes.json").unwrap(), Some(0x0002));
    assert_eq!(classify_path("/docs/.aeordb-config/custom.json").unwrap(), Some(0x0008));
    assert_eq!(classify_path("/docs/.aeordb-permissions").unwrap(), Some(0x0019));
    assert_eq!(classify_path("/docs/.aeordb-config/archive/.aeordb-indexes/postings/page.bin").unwrap(), Some(0x0060));
    assert_eq!(classify_path("/.aeordb-indexes/postings/page.bin").unwrap(), Some(0x0060));
    assert_eq!(classify_path("/.aeordb-logs/index.log").unwrap(), Some(0x0061));
    assert_eq!(classify_path("/.aeordb-system/controls/v1/index-registry/a.ctrl").unwrap(), Some(0x0043));
    assert_eq!(classify_path("/docs/.aeordb-unknown/value").unwrap(), Some(0xfffe));
    assert_eq!(classify_path("/docs/readme.md").unwrap(), None);
    assert!(classify_path("/docs/").is_err());
  }

  #[test]
  fn non_path_domains_classify_without_cross_domain_aliasing() {
    assert_eq!(classify_scalar(StorageDomain::EntryType, 0x0005).unwrap(), Some(0x0040));
    assert_eq!(classify_scalar(StorageDomain::EntryType, 0x0009).unwrap(), Some(0x0050));
    assert_eq!(classify_scalar(StorageDomain::ControlRegion, 9).unwrap(), Some(0x0056));
    assert_eq!(classify_scalar(StorageDomain::ExternalWorkspace, 4).unwrap(), Some(0x0071));
    assert_eq!(classify_kv_key(b"aeordb.task.v1\0task-1").unwrap(), Some(0x0042));
    assert_eq!(classify_scalar(StorageDomain::EntryType, 0xffff).unwrap(), None);
  }

  #[test]
  fn operational_and_semantic_fingerprints_have_selected_width_and_domains() {
    let bytes = build_registry();
    let descriptors = decode_descriptors(&bytes).unwrap();
    let operational_32 = operational_fingerprint(HashProfile::Blake3_256, &bytes);
    let operational_64 = operational_fingerprint(HashProfile::Sha512, &bytes);
    assert_eq!(operational_32.len(), 32);
    assert_eq!(operational_64.len(), 64);
    assert_ne!(operational_32, operational_64[..32]);
    let baseline_semantic = semantic_projection_fingerprint(HashProfile::Blake3_256, &descriptors);
    assert_ne!(operational_32, baseline_semantic);

    let mut transfer_change = descriptors.clone();
    transfer_change.iter_mut().find(|descriptor| descriptor.family_id == 0x0001).unwrap().policy.peer_replication_policy = 2;
    assert_eq!(baseline_semantic, semantic_projection_fingerprint(HashProfile::Blake3_256, &transfer_change));
    let mut index_change = descriptors;
    index_change.iter_mut().find(|descriptor| descriptor.family_id == 0x0001).unwrap().policy.index_policy = 2;
    assert_ne!(baseline_semantic, semantic_projection_fingerprint(HashProfile::Blake3_256, &index_change));
  }

  #[test]
  fn manifest_closes_over_rows_descriptors_and_permanent_registries() {
    let bytes = build_registry();
    let manifest = build_manifest(&bytes).unwrap();
    assert_eq!(manifest.source_row_count, 46);
    assert_eq!(manifest.family_ids.len(), 46);
    assert_eq!(manifest.descriptor_keys.len(), manifest.descriptor_count);
    assert_eq!(manifest.operational_control_tags.len(), 11);
    assert_eq!(manifest.external_workspace_kinds.len(), 4);
    assert!(manifest.source_rows.iter().all(|row| row.matcher_count == row.descriptor_keys.len()));
  }
}
