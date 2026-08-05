use std::cmp::Ordering;

use crate::core::HashProfile;
use crate::gc::{
  build_gc_value, decode_gc_value, decode_physical_incarnation, encode_physical_incarnation, immutable_key, put_u16, put_u32, put_u64,
  read_u16, read_u32, read_u64, GcFixtureCase, GcFormat, GcKind, PhysicalIncarnationId,
};

const MARK_CHECKPOINT_BODY_MAX: usize = 256 * 1024;
const MARK_CHECKPOINT_VALUE_MAX: usize = 32 + 40 + MARK_CHECKPOINT_BODY_MAX + 4;
const MARK_JOURNAL_MAX: usize = 16 * 1024 * 1024;
const WORKSPACE_MANIFEST_MAX: usize = 8 * 1024 * 1024;
const WORKSPACE_OBJECT_MAX: usize = 64 * 1024 * 1024;
const WORKSPACE_OBJECT_HEADER: usize = 80;
const MAX_WORKSPACE_RECORD: usize = 1024 * 1024;
const MAX_WORKSPACE_NAME: usize = 4 * 1024;
const MARK_REQUIRED_CAPABILITY_BITS: &[usize] = &[12, 13, 14, 15, 17];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WorkspaceObjectKind {
  Bitmap = 1,
  Frontier = 2,
  PathVisit = 3,
  Mutation = 4,
  Candidate = 5,
  Diagnostic = 6,
}

impl WorkspaceObjectKind {
  const ALL: [Self; 6] = [Self::Bitmap, Self::Frontier, Self::PathVisit, Self::Mutation, Self::Candidate, Self::Diagnostic];

  fn from_id(id: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| *kind as u16 == id)
  }

  fn name(self) -> &'static str {
    match self {
      Self::Bitmap => "bitmap",
      Self::Frontier => "frontier",
      Self::PathVisit => "path-visit",
      Self::Mutation => "mutation",
      Self::Candidate => "candidate",
      Self::Diagnostic => "diagnostic",
    }
  }
}

#[derive(Clone)]
struct WorkspaceObject {
  kind: WorkspaceObjectKind,
  ordinal: u64,
  logical_record_count: u64,
  name: String,
  bytes: Vec<u8>,
}

#[derive(Debug)]
struct DecodedWorkspaceObject {
  kind: WorkspaceObjectKind,
  ordinal: u64,
  logical_record_count: u64,
}

#[derive(Debug)]
struct WorkspaceDescriptor {
  kind: WorkspaceObjectKind,
  ordinal: u64,
  stored_length: u64,
  logical_record_count: u64,
  digest: [u8; 32],
  name: String,
}

pub fn fixture_cases() -> Vec<GcFixtureCase> {
  let mut cases = Vec::new();
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let objects = build_workspace_objects(profile);
    let manifest = build_workspace_manifest(profile, &objects);
    let manifest_digest = *blake3::hash(&manifest).as_bytes();
    let journal = build_mark_mutation_journal(profile);
    let journal_key = immutable_key(profile, GcKind::MarkMutationJournalSegment, &journal);

    for (label, state, phase, flags, path) in
      [("embedded", 1u16, 1u16, 1u32, default_workspace_path()), ("external-canceled", 4u16, 6u16, 3u32, "C:/AeorDB/gc/31323334/51525354")]
    {
      let checkpoint = build_mark_checkpoint(profile, state, phase, flags, path, &manifest_digest, &journal_key);
      let checkpoint_key = immutable_key(profile, GcKind::MarkRunCheckpoint, &checkpoint);
      cases.push(GcFixtureCase {
        id: leak(format!("agca-{}-mark-run-checkpoint-{label}", profile.label())),
        format: GcFormat::GcArtifactV1,
        profile,
        expected: leak(format!("gc:checkpoint:mark-run:{label}:state={state}:phase={phase}")),
        relation: Some("roots:gc-mark-workspace-manifest-v1"),
        canonical_key: Some(hex::encode(checkpoint_key)),
        bytes: checkpoint,
      });
    }

    cases.push(GcFixtureCase {
      id: leak(format!("agca-{}-mark-mutation-journal-reset", profile.label())),
      format: GcFormat::GcArtifactV1,
      profile,
      expected: "gc:journal:mark-mutation:reset:records=2:first=800:last=801",
      relation: Some("accelerates:bounded-mark-reconciliation"),
      canonical_key: Some(hex::encode(journal_key)),
      bytes: journal,
    });

    cases.push(GcFixtureCase {
      id: leak(format!("agcw-{}-mark-workspace-manifest", profile.label())),
      format: GcFormat::MarkWorkspaceManifestV1,
      profile,
      expected: leak(workspace_manifest_result(1, &objects)),
      relation: Some("closes:six-agwo-object-kinds"),
      canonical_key: None,
      bytes: manifest,
    });
    cases.push(GcFixtureCase {
      id: leak(format!("agcw-{}-mark-workspace-manifest-empty", profile.label())),
      format: GcFormat::MarkWorkspaceManifestV1,
      profile,
      expected: leak(workspace_manifest_result::<WorkspaceObject>(1, &[])),
      relation: Some("initial-checkpoint:zero-workspace-objects"),
      canonical_key: None,
      bytes: build_workspace_manifest(profile, &[]),
    });

    for object in objects {
      cases.push(GcFixtureCase {
        id: leak(format!("agwo-{}-{}-valid", profile.label(), object.kind.name())),
        format: GcFormat::MarkWorkspaceObjectV1,
        profile,
        expected: leak(format!(
          "gc:workspace-object:{}:ordinal={}:records={}",
          object.kind.name(),
          object.ordinal,
          object.logical_record_count
        )),
        relation: Some("listed-by:gc-mark-workspace-manifest-v1"),
        canonical_key: None,
        bytes: object.bytes,
      });
    }
  }
  cases
}

pub fn observe(format: GcFormat, profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  let result = match format {
    GcFormat::GcArtifactV1 => observe_gc_artifact(profile, bytes),
    GcFormat::MarkWorkspaceManifestV1 => {
      decode_workspace_manifest(profile, bytes).map(|descriptors| workspace_manifest_result(read_u16(bytes, 6).unwrap_or(0), &descriptors))
    }
    GcFormat::MarkWorkspaceObjectV1 => decode_workspace_object(profile, bytes).map(|object| {
      format!("gc:workspace-object:{}:ordinal={}:records={}", object.kind.name(), object.ordinal, object.logical_record_count)
    }),
  };
  match result {
    Ok(observed) => {
      let key = if format == GcFormat::GcArtifactV1 {
        read_u16(bytes, 6).ok().and_then(GcKind::from_id).map(|kind| hex::encode(immutable_key(profile, kind, bytes)))
      } else {
        None
      };
      (observed, key)
    }
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(format: GcFormat, profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  match format {
    GcFormat::GcArtifactV1 => {
      let kind = read_u16(bytes, 6).ok().and_then(GcKind::from_id).map_or("invalid", GcKind::name);
      vec![
        "envelope +0x000 len 32: AGCA common envelope".to_string(),
        format!("envelope artifact_kind: {kind}"),
        "identity: exact database/run/sequence identity".to_string(),
        "body: bounded mark checkpoint or mutation segment".to_string(),
        format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
      ]
    }
    GcFormat::MarkWorkspaceManifestV1 => vec![
      "manifest +0x000 len 88: AGCW fixed header".to_string(),
      format!("manifest +0x058 len {}: layout and authority hashes", 2 * profile.width() + 32),
      "manifest descriptors: sorted kind/ordinal/name closure".to_string(),
      format!("manifest +0x{:03x} len 4: crc32", bytes.len().saturating_sub(4)),
    ],
    GcFormat::MarkWorkspaceObjectV1 => vec![
      "object +0x000 len 80: AGWO fixed header".to_string(),
      "object body: exact kind-specific bounded payload".to_string(),
      format!("object +0x{:03x} len 4: crc32", bytes.len().saturating_sub(4)),
    ],
  }
}

fn observe_gc_artifact(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  match read_u16(bytes, 6).ok().and_then(GcKind::from_id) {
    Some(GcKind::MarkRunCheckpoint) => decode_mark_checkpoint(profile, bytes),
    Some(GcKind::MarkMutationJournalSegment) => decode_mark_mutation_journal(profile, bytes),
    _ => Err("mark_gc_artifact_kind"),
  }
}

fn build_mark_checkpoint(
  profile: HashProfile,
  state: u16,
  phase: u16,
  flags: u32,
  path: &str,
  manifest_digest: &[u8; 32],
  journal_head: &[u8],
) -> Vec<u8> {
  let h = profile.width();
  assert_eq!(journal_head.len(), h);
  let path_bytes = path.as_bytes();
  let mut body = vec![0u8; 236 + 4 * h + path_bytes.len()];
  put_u32(&mut body, 0, flags);
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, state);
  put_u16(&mut body, 8, phase);
  write_capabilities(&mut body[12..44], MARK_REQUIRED_CAPABILITY_BITS);
  put_u64(&mut body, 44, 1_700_000_100_000);
  put_u64(&mut body, 52, 1_700_000_100_500);
  fill_sequence(&mut body[60..60 + h], 0x11);
  fill_sequence(&mut body[60 + h..60 + 2 * h], 0x31);
  fill_sequence(&mut body[60 + 2 * h..60 + 3 * h], 0x51);
  fill_sequence(&mut body[60 + 3 * h..92 + 3 * h], 0x71);
  fill_sequence(&mut body[92 + 3 * h..124 + 3 * h], 0x91);
  put_u64(&mut body, 124 + 3 * h, 17);
  put_u64(&mut body, 132 + 3 * h, 900);
  put_u64(&mut body, 140 + 3 * h, 801);
  put_u64(&mut body, 148 + 3 * h, 512);
  put_u64(&mut body, 156 + 3 * h, 8);
  put_u32(&mut body, 164 + 3 * h, 64);
  put_u32(&mut body, 168 + 3 * h, path_bytes.len() as u32);
  body[172 + 3 * h..188 + 3 * h].copy_from_slice(&workspace_id());
  body[188 + 3 * h..220 + 3 * h].copy_from_slice(manifest_digest);
  body[220 + 3 * h..220 + 4 * h].copy_from_slice(journal_head);
  put_u64(&mut body, 220 + 4 * h, 16 * 1024 * 1024);
  put_u64(&mut body, 228 + 4 * h, 64 * 1024 * 1024);
  body[236 + 4 * h..].copy_from_slice(path_bytes);

  let mut identity = Vec::with_capacity(40);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&run_id());
  identity.extend_from_slice(&7u64.to_le_bytes());
  build_gc_value(GcKind::MarkRunCheckpoint, run_generation(), &identity, &body)
}

fn decode_mark_checkpoint(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  let artifact = decode_gc_value(bytes, MARK_CHECKPOINT_VALUE_MAX)?;
  let h = profile.width();
  if artifact.kind != GcKind::MarkRunCheckpoint
    || artifact.identity.len() != 40
    || artifact.identity[..16] != database_id()
    || artifact.identity[16..32] != run_id()
    || read_u64(artifact.identity, 32)? == 0
    || artifact.generation != run_generation()
    || artifact.body.len() < 236 + 4 * h
  {
    return Err("mark_checkpoint_shape");
  }
  let body = artifact.body;
  let flags = read_u32(body, 0)?;
  let state = read_u16(body, 6)?;
  let phase = read_u16(body, 8)?;
  let path_length = read_u32(body, 168 + 3 * h)? as usize;
  if flags & !3 != 0
    || read_u16(body, 4)? != 1
    || !(1..=5).contains(&state)
    || !(1..=8).contains(&phase)
    || read_u16(body, 10)? != 0
    || (flags & 2 != 0) != (state == 4)
    || (state == 5 && flags & 1 != 0)
    || !valid_capabilities(&body[12..44], MARK_REQUIRED_CAPABILITY_BITS)
    || read_u64(body, 44)? == 0
    || read_u64(body, 52)? < read_u64(body, 44)?
    || body[60..60 + 3 * h].chunks(h).any(all_zero)
    || body[60 + 3 * h..124 + 3 * h].chunks(32).any(all_zero)
    || read_u64(body, 124 + 3 * h)? == 0
    || read_u64(body, 140 + 3 * h)? > read_u64(body, 132 + 3 * h)?
    || read_u64(body, 148 + 3 * h)? == 0
    || read_u64(body, 156 + 3 * h)? == 0
    || read_u32(body, 164 + 3 * h)? == 0
    || read_u64(body, 156 + 3 * h)?.checked_mul(u64::from(read_u32(body, 164 + 3 * h)?)) != Some(read_u64(body, 148 + 3 * h)?)
    || path_length == 0
    || 236usize.checked_add(4 * h).and_then(|n| n.checked_add(path_length)) != Some(body.len())
    || all_zero(&body[172 + 3 * h..188 + 3 * h])
    || all_zero(&body[188 + 3 * h..220 + 3 * h])
    || read_u64(body, 220 + 4 * h)? > read_u64(body, 228 + 4 * h)?
  {
    return Err("mark_checkpoint_fields");
  }
  let path = std::str::from_utf8(&body[236 + 4 * h..]).map_err(|_| "mark_checkpoint_path")?;
  if !canonical_workspace_path(path) {
    return Err("mark_checkpoint_path");
  }
  let label = if path.as_bytes().get(1) == Some(&b':') { "external-canceled" } else { "embedded" };
  Ok(format!("gc:checkpoint:mark-run:{label}:state={state}:phase={phase}"))
}

fn build_mark_mutation_journal(profile: HashProfile) -> Vec<u8> {
  let h = profile.width();
  let mut records = Vec::new();
  for (sequence, operation, ordinal) in [(800u64, 1u16, 1u8), (801, 2, 2)] {
    let payload = build_mutation_payload(profile, sequence, operation, ordinal);
    records.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    records.extend_from_slice(&payload);
  }
  let mut body = vec![0u8; 32 + h + records.len()];
  put_u32(&mut body, 0, 1);
  put_u16(&mut body, 4, 1);
  put_u64(&mut body, 8, 800);
  put_u64(&mut body, 16, 801);
  put_u32(&mut body, 24, 2);
  put_u32(&mut body, 28, records.len() as u32);
  body[32 + h..].copy_from_slice(&records);
  let mut identity = Vec::with_capacity(40);
  identity.extend_from_slice(&database_id());
  identity.extend_from_slice(&run_id());
  identity.extend_from_slice(&1u64.to_le_bytes());
  build_gc_value(GcKind::MarkMutationJournalSegment, run_generation(), &identity, &body)
}

fn build_mutation_payload(profile: HashProfile, sequence: u64, operation: u16, ordinal: u8) -> Vec<u8> {
  let h = profile.width();
  let mut payload = vec![0u8; 36 + 6 * h];
  put_u64(&mut payload, 0, sequence);
  fill_sequence(&mut payload[8..8 + h], 0x10 + ordinal);
  fill_sequence(&mut payload[8 + h..8 + 2 * h], 0x30 + ordinal);
  fill_sequence(&mut payload[8 + 2 * h..8 + 3 * h], 0x50 + ordinal);
  fill_sequence(&mut payload[8 + 3 * h..8 + 4 * h], 0x70 + ordinal);
  let physical = encode_physical_incarnation(profile, &sample_incarnation(profile, ordinal));
  payload[8 + 4 * h..32 + 6 * h].copy_from_slice(&physical);
  put_u16(&mut payload, 32 + 6 * h, operation);
  payload
}

fn decode_mark_mutation_journal(profile: HashProfile, bytes: &[u8]) -> Result<String, &'static str> {
  let artifact = decode_gc_value(bytes, MARK_JOURNAL_MAX)?;
  let h = profile.width();
  if artifact.kind != GcKind::MarkMutationJournalSegment
    || artifact.identity.len() != 40
    || artifact.identity[..16] != database_id()
    || artifact.identity[16..32] != run_id()
    || read_u64(artifact.identity, 32)? == 0
    || artifact.generation != run_generation()
    || artifact.body.len() < 32 + h
  {
    return Err("mark_journal_shape");
  }
  let body = artifact.body;
  let flags = read_u32(body, 0)?;
  let first = read_u64(body, 8)?;
  let last = read_u64(body, 16)?;
  let count = read_u32(body, 24)?;
  let records_length = read_u32(body, 28)? as usize;
  if flags & !1 != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != 0
    || first == 0
    || first > last
    || count == 0
    || 32usize.checked_add(h).and_then(|n| n.checked_add(records_length)) != Some(body.len())
    || (flags & 1 != 0) != all_zero(&body[32..32 + h])
  {
    return Err("mark_journal_header");
  }
  let mut cursor = 32 + h;
  let mut first_observed = None;
  let mut previous: Option<(u64, Vec<u8>)> = None;
  for _ in 0..count {
    let payload_length = read_u32(body, cursor)? as usize;
    cursor = cursor.checked_add(4).ok_or("mark_journal_record_length")?;
    if payload_length != 36 + 6 * h || cursor.checked_add(payload_length).is_none_or(|end| end > body.len()) {
      return Err("mark_journal_record_length");
    }
    let payload = &body[cursor..cursor + payload_length];
    let sequence = validate_mutation_payload(profile, payload)?;
    let mutation_id = payload[8..8 + h].to_vec();
    if previous.as_ref().is_some_and(|prior| prior.0 > sequence || (prior.0 == sequence && prior.1 >= mutation_id)) {
      return Err("mark_journal_order");
    }
    first_observed.get_or_insert(sequence);
    previous = Some((sequence, mutation_id));
    cursor += payload_length;
  }
  if cursor != body.len() || previous.as_ref().map(|v| v.0) != Some(last) || first_observed != Some(first) {
    return Err("mark_journal_bounds");
  }
  Ok(format!("gc:journal:mark-mutation:reset:records={count}:first={first}:last={last}"))
}

fn validate_mutation_payload(profile: HashProfile, payload: &[u8]) -> Result<u64, &'static str> {
  let h = profile.width();
  if payload.len() != 36 + 6 * h || payload[8..8 + 4 * h].chunks(h).any(all_zero) {
    return Err("mark_mutation_payload");
  }
  let sequence = read_u64(payload, 0)?;
  let physical_end = 32 + 6 * h;
  decode_physical_incarnation(profile, &payload[8 + 4 * h..physical_end])?;
  let operation = read_u16(payload, physical_end)?;
  if sequence == 0 || !(1..=10).contains(&operation) || read_u16(payload, physical_end + 2)? != 0 {
    return Err("mark_mutation_fields");
  }
  Ok(sequence)
}

fn build_workspace_objects(profile: HashProfile) -> Vec<WorkspaceObject> {
  WorkspaceObjectKind::ALL
    .into_iter()
    .enumerate()
    .map(|(index, kind)| {
      let ordinal = index as u64 + 1;
      let (body, count) = build_workspace_body(profile, kind);
      let bytes = build_workspace_object(kind, ordinal, &body);
      WorkspaceObject { kind, ordinal, logical_record_count: count, name: format!("{}/{ordinal:016x}.agwo", kind.name()), bytes }
    })
    .collect()
}

fn build_workspace_body(profile: HashProfile, kind: WorkspaceObjectKind) -> (Vec<u8>, u64) {
  match kind {
    WorkspaceObjectKind::Bitmap => {
      let mut body = vec![0u8; 35];
      put_u16(&mut body, 4, 1);
      put_u64(&mut body, 8, 256);
      put_u64(&mut body, 16, 19);
      put_u64(&mut body, 24, 3);
      body[32..].copy_from_slice(&[0x93, 0x40, 0x05]);
      (body, 19)
    }
    WorkspaceObjectKind::Frontier => {
      let h = profile.width();
      let mut record = vec![0u8; 36 + 4 * h];
      let record_length = record.len() as u32;
      put_u32(&mut record, 0, record_length);
      put_u16(&mut record, 4, 1);
      put_u16(&mut record, 6, 3);
      fill_sequence(&mut record[12..12 + h], 0x21);
      fill_sequence(&mut record[12 + h..12 + 2 * h], 0x41);
      let physical = encode_physical_incarnation(profile, &sample_incarnation(profile, 3));
      record[12 + 2 * h..].copy_from_slice(&physical);
      (build_run_body(1, 1, &record), 1)
    }
    WorkspaceObjectKind::PathVisit => {
      let h = profile.width();
      let mut record = vec![0u8; 8 + 2 * h];
      let record_length = record.len() as u32;
      put_u32(&mut record, 0, record_length);
      fill_sequence(&mut record[8..8 + h], 0x31);
      fill_sequence(&mut record[8 + h..], 0x71);
      (build_run_body(2, 1, &record), 1)
    }
    WorkspaceObjectKind::Mutation => {
      let payload = build_mutation_payload(profile, 802, 2, 4);
      let mut record = Vec::with_capacity(4 + payload.len());
      record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
      record.extend_from_slice(&payload);
      (build_run_body(3, 1, &record), 1)
    }
    WorkspaceObjectKind::Candidate => {
      let h = profile.width();
      let mut record = vec![0u8; 32 + 2 * h];
      let record_length = record.len() as u32;
      put_u32(&mut record, 0, record_length);
      put_u16(&mut record, 6, 2);
      let physical = encode_physical_incarnation(profile, &sample_incarnation(profile, 5));
      record[8..].copy_from_slice(&physical);
      (build_run_body(4, 1, &record), 1)
    }
    WorkspaceObjectKind::Diagnostic => {
      let h = profile.width();
      let context = b"{\"branch\":\"/workspaces/wyatt\"}";
      let mut record = vec![0u8; 32 + h + context.len()];
      let record_length = record.len() as u32;
      put_u32(&mut record, 0, record_length);
      put_u16(&mut record, 4, 7);
      record[6] = 3;
      put_i64(&mut record, 8, 1_700_000_100_700);
      fill_sequence(&mut record[16..16 + h], 0x51);
      put_u64(&mut record, 16 + h, 4_096);
      put_u32(&mut record, 24 + h, context.len() as u32);
      record[32 + h..].copy_from_slice(context);
      (build_run_body(5, 1, &record), 1)
    }
  }
}

fn build_run_body(codec: u16, count: u32, records: &[u8]) -> Vec<u8> {
  let mut body = vec![0u8; 24 + records.len()];
  put_u16(&mut body, 4, codec);
  put_u32(&mut body, 8, count);
  put_u32(&mut body, 12, records.len() as u32);
  put_u64(&mut body, 16, run_generation());
  body[24..].copy_from_slice(records);
  body
}

fn build_workspace_object(kind: WorkspaceObjectKind, ordinal: u64, body: &[u8]) -> Vec<u8> {
  let total_length = WORKSPACE_OBJECT_HEADER + body.len() + 4;
  let mut bytes = vec![0u8; total_length];
  bytes[..4].copy_from_slice(b"AGWO");
  put_u16(&mut bytes, 4, 1);
  put_u16(&mut bytes, 6, kind as u16);
  put_u64(&mut bytes, 8, total_length as u64);
  bytes[16..32].copy_from_slice(&database_id());
  bytes[32..48].copy_from_slice(&run_id());
  put_u64(&mut bytes, 48, run_generation());
  put_u64(&mut bytes, 56, 7);
  put_u64(&mut bytes, 64, ordinal);
  put_u64(&mut bytes, 72, body.len() as u64);
  bytes[80..80 + body.len()].copy_from_slice(body);
  write_crc(&mut bytes);
  bytes
}

fn decode_workspace_object(profile: HashProfile, bytes: &[u8]) -> Result<DecodedWorkspaceObject, &'static str> {
  if bytes.len() < WORKSPACE_OBJECT_HEADER + 4 || bytes.len() > WORKSPACE_OBJECT_MAX {
    return Err("workspace_object_length");
  }
  verify_crc(bytes)?;
  let kind = WorkspaceObjectKind::from_id(read_u16(bytes, 6)?).ok_or("workspace_object_kind")?;
  let body_length = usize::try_from(read_u64(bytes, 72)?).map_err(|_| "workspace_object_length")?;
  let ordinal = read_u64(bytes, 64)?;
  let complete_length = usize::try_from(read_u64(bytes, 8)?).map_err(|_| "workspace_object_length")?;
  if &bytes[..4] != b"AGWO"
    || read_u16(bytes, 4)? != 1
    || complete_length != bytes.len()
    || bytes[16..32] != database_id()
    || bytes[32..48] != run_id()
    || read_u64(bytes, 48)? != run_generation()
    || read_u64(bytes, 56)? != 7
    || ordinal == 0
    || WORKSPACE_OBJECT_HEADER.checked_add(body_length).and_then(|n| n.checked_add(4)) != Some(bytes.len())
  {
    return Err("workspace_object_header");
  }
  let body = &bytes[80..80 + body_length];
  let logical_record_count = validate_workspace_body(profile, kind, body)?;
  Ok(DecodedWorkspaceObject { kind, ordinal, logical_record_count })
}

fn validate_workspace_body(profile: HashProfile, kind: WorkspaceObjectKind, body: &[u8]) -> Result<u64, &'static str> {
  if kind == WorkspaceObjectKind::Bitmap {
    if body.len() < 32 || read_u32(body, 0)? != 0 || read_u16(body, 4)? != 1 || read_u16(body, 6)? != 0 {
      return Err("workspace_bitmap_header");
    }
    let start = read_u64(body, 8)?;
    let bit_count = read_u64(body, 16)?;
    let byte_count = usize::try_from(read_u64(body, 24)?).map_err(|_| "workspace_bitmap_length")?;
    let expected_bytes =
      usize::try_from(bit_count.checked_add(7).ok_or("workspace_bitmap_range")? / 8).map_err(|_| "workspace_bitmap_range")?;
    if bit_count == 0
      || start.checked_add(bit_count).is_none()
      || byte_count != expected_bytes
      || 32usize.checked_add(byte_count) != Some(body.len())
      || bit_count % 8 != 0 && body.last().is_some_and(|last| last & !((1u8 << (bit_count % 8)) - 1) != 0)
    {
      return Err("workspace_bitmap_fields");
    }
    return Ok(bit_count);
  }

  let expected_codec = kind as u16 - 1;
  if body.len() < 24
    || read_u32(body, 0)? != 0
    || read_u16(body, 4)? != expected_codec
    || read_u16(body, 6)? != 0
    || read_u64(body, 16)? != run_generation()
  {
    return Err("workspace_run_header");
  }
  let count = read_u32(body, 8)?;
  let records_length = read_u32(body, 12)? as usize;
  if count == 0 || 24usize.checked_add(records_length) != Some(body.len()) {
    return Err("workspace_run_length");
  }
  let mut cursor = 24;
  let mut previous: Option<Vec<u8>> = None;
  for _ in 0..count {
    let framed_length = read_u32(body, cursor)? as usize;
    let record_length =
      if kind == WorkspaceObjectKind::Mutation { framed_length.checked_add(4).ok_or("workspace_record_length")? } else { framed_length };
    if framed_length == 0
      || framed_length > MAX_WORKSPACE_RECORD
      || record_length < 4
      || cursor.checked_add(record_length).is_none_or(|end| end > body.len())
    {
      return Err("workspace_record_length");
    }
    let record = &body[cursor..cursor + record_length];
    let key = validate_workspace_record(profile, kind, record)?;
    if previous.as_ref().is_some_and(|prior| prior >= &key) {
      return Err("workspace_record_order");
    }
    previous = Some(key);
    cursor += record_length;
  }
  if cursor != body.len() {
    return Err("workspace_record_trailing");
  }
  Ok(u64::from(count))
}

fn validate_workspace_record(profile: HashProfile, kind: WorkspaceObjectKind, record: &[u8]) -> Result<Vec<u8>, &'static str> {
  let h = profile.width();
  match kind {
    WorkspaceObjectKind::Bitmap => Err("workspace_record_kind"),
    WorkspaceObjectKind::Frontier => {
      if record.len() != 36 + 4 * h || read_u32(record, 0)? as usize != record.len() {
        return Err("workspace_frontier_length");
      }
      let record_kind = read_u16(record, 4)?;
      let flags = read_u16(record, 6)?;
      let family = read_u16(record, 8)?;
      if !(1..=4).contains(&record_kind)
        || flags & !3 != 0
        || read_u16(record, 10)? != 0
        || (record_kind == 3) != (family != 0)
        || all_zero(&record[12..12 + h])
        || (flags & 1 != 0) != !all_zero(&record[12 + h..12 + 2 * h])
        || (flags & 2 != 0) != !all_zero(&record[12 + 2 * h..])
      {
        return Err("workspace_frontier_fields");
      }
      if flags & 2 != 0 {
        decode_physical_incarnation(profile, &record[12 + 2 * h..])?;
      }
      Ok(record[4..].to_vec())
    }
    WorkspaceObjectKind::PathVisit => {
      if record.len() != 8 + 2 * h
        || read_u32(record, 0)? as usize != record.len()
        || read_u32(record, 4)? != 0
        || all_zero(&record[8..8 + h])
        || all_zero(&record[8 + h..])
      {
        return Err("workspace_path_visit_fields");
      }
      Ok(record[8..].to_vec())
    }
    WorkspaceObjectKind::Mutation => {
      if record.len() < 4 || read_u32(record, 0)? as usize + 4 != record.len() {
        return Err("workspace_mutation_length");
      }
      let payload = &record[4..];
      validate_mutation_payload(profile, payload)?;
      let mut key = Vec::with_capacity(8 + h);
      key.extend_from_slice(&payload[..8 + h]);
      Ok(key)
    }
    WorkspaceObjectKind::Candidate => {
      if record.len() != 32 + 2 * h
        || read_u32(record, 0)? as usize != record.len()
        || read_u16(record, 4)? != 0
        || !(1..=7).contains(&read_u16(record, 6)?)
      {
        return Err("workspace_candidate_fields");
      }
      decode_physical_incarnation(profile, &record[8..])?;
      Ok(record[8..].to_vec())
    }
    WorkspaceObjectKind::Diagnostic => {
      if record.len() < 32 + h || read_u32(record, 0)? as usize != record.len() {
        return Err("workspace_diagnostic_length");
      }
      let context_length = read_u32(record, 24 + h)? as usize;
      if !(1..=10).contains(&read_u16(record, 4)?)
        || !(1..=3).contains(&record[6])
        || record[7] != 0
        || read_i64(record, 8)? <= 0
        || read_u64(record, 16 + h)? == 0
        || context_length == 0
        || context_length > 4_096
        || read_u32(record, 28 + h)? != 0
        || 32usize.checked_add(h).and_then(|n| n.checked_add(context_length)) != Some(record.len())
      {
        return Err("workspace_diagnostic_fields");
      }
      let context = std::str::from_utf8(&record[32 + h..]).map_err(|_| "workspace_diagnostic_utf8")?;
      if context.as_bytes().contains(&0) {
        return Err("workspace_diagnostic_utf8");
      }
      let mut key = Vec::new();
      key.extend_from_slice(&record[8..16]);
      key.extend_from_slice(&record[4..6]);
      key.extend_from_slice(&record[16..16 + h]);
      key.extend_from_slice(context.as_bytes());
      Ok(key)
    }
  }
}

fn build_workspace_manifest(profile: HashProfile, objects: &[WorkspaceObject]) -> Vec<u8> {
  let h = profile.width();
  let descriptors_length = objects.iter().map(|object| 68 + object.name.len()).sum::<usize>();
  let total_length = 120 + 2 * h + descriptors_length + 4;
  let mut bytes = vec![0u8; total_length];
  bytes[..4].copy_from_slice(b"AGCW");
  put_u16(&mut bytes, 4, 1);
  put_u16(&mut bytes, 6, 1);
  put_u64(&mut bytes, 8, total_length as u64);
  bytes[16..32].copy_from_slice(&database_id());
  bytes[32..48].copy_from_slice(&run_id());
  put_u64(&mut bytes, 48, run_generation());
  put_u64(&mut bytes, 56, 7);
  put_u64(&mut bytes, 64, 1_700_000_100_000);
  put_u64(&mut bytes, 72, 1_700_000_100_500);
  put_u16(&mut bytes, 80, profile.algorithm_id());
  put_u16(&mut bytes, 82, objects.len() as u16);
  fill_sequence(&mut bytes[88..88 + h], 0x51);
  fill_sequence(&mut bytes[88 + h..88 + 2 * h], 0x11);
  fill_sequence(&mut bytes[88 + 2 * h..120 + 2 * h], 0x71);
  let mut cursor = 120 + 2 * h;
  for object in objects {
    let name = object.name.as_bytes();
    put_u16(&mut bytes, cursor, object.kind as u16);
    put_u64(&mut bytes, cursor + 4, object.ordinal);
    put_u64(&mut bytes, cursor + 12, object.bytes.len() as u64);
    put_u64(&mut bytes, cursor + 20, object.logical_record_count);
    bytes[cursor + 28..cursor + 60].copy_from_slice(blake3::hash(&object.bytes).as_bytes());
    put_u32(&mut bytes, cursor + 60, name.len() as u32);
    bytes[cursor + 68..cursor + 68 + name.len()].copy_from_slice(name);
    cursor += 68 + name.len();
  }
  write_crc(&mut bytes);
  bytes
}

fn decode_workspace_manifest(profile: HashProfile, bytes: &[u8]) -> Result<Vec<WorkspaceDescriptor>, &'static str> {
  let h = profile.width();
  if bytes.len() < 124 + 2 * h || bytes.len() > WORKSPACE_MANIFEST_MAX {
    return Err("workspace_manifest_length");
  }
  verify_crc(bytes)?;
  let state = read_u16(bytes, 6)?;
  let count = read_u16(bytes, 82)? as usize;
  let complete_length = usize::try_from(read_u64(bytes, 8)?).map_err(|_| "workspace_manifest_length")?;
  if &bytes[..4] != b"AGCW"
    || read_u16(bytes, 4)? != 1
    || !(1..=5).contains(&state)
    || complete_length != bytes.len()
    || bytes[16..32] != database_id()
    || bytes[32..48] != run_id()
    || read_u64(bytes, 48)? != run_generation()
    || read_u64(bytes, 56)? != 7
    || read_u64(bytes, 64)? == 0
    || read_u64(bytes, 72)? < read_u64(bytes, 64)?
    || read_u16(bytes, 80)? != profile.algorithm_id()
    || read_u32(bytes, 84)? != 0
    || all_zero(&bytes[88..88 + 2 * h])
    || all_zero(&bytes[88 + 2 * h..120 + 2 * h])
  {
    return Err("workspace_manifest_header");
  }
  let end = bytes.len() - 4;
  let mut cursor = 120 + 2 * h;
  let mut descriptors = Vec::with_capacity(count);
  for _ in 0..count {
    if cursor.checked_add(68).is_none_or(|next| next > end) {
      return Err("workspace_descriptor_length");
    }
    let kind = WorkspaceObjectKind::from_id(read_u16(bytes, cursor)?).ok_or("workspace_descriptor_kind")?;
    let flags = read_u16(bytes, cursor + 2)?;
    let ordinal = read_u64(bytes, cursor + 4)?;
    let stored_length = read_u64(bytes, cursor + 12)?;
    let logical_record_count = read_u64(bytes, cursor + 20)?;
    let digest: [u8; 32] = bytes[cursor + 28..cursor + 60].try_into().map_err(|_| "workspace_descriptor_digest")?;
    let name_length = read_u32(bytes, cursor + 60)? as usize;
    if flags != 0
      || ordinal == 0
      || stored_length < (WORKSPACE_OBJECT_HEADER + 4) as u64
      || logical_record_count == 0
      || all_zero(&digest)
      || name_length == 0
      || name_length > MAX_WORKSPACE_NAME
      || read_u32(bytes, cursor + 64)? != 0
      || cursor.checked_add(68).and_then(|n| n.checked_add(name_length)).is_none_or(|next| next > end)
    {
      return Err("workspace_descriptor_fields");
    }
    let name = std::str::from_utf8(&bytes[cursor + 68..cursor + 68 + name_length]).map_err(|_| "workspace_descriptor_name")?;
    if !canonical_relative_name(name) {
      return Err("workspace_descriptor_name");
    }
    descriptors.push(WorkspaceDescriptor { kind, ordinal, stored_length, logical_record_count, digest, name: name.to_string() });
    cursor += 68 + name_length;
  }
  if cursor != end || descriptors.windows(2).any(|pair| descriptor_cmp(&pair[0], &pair[1]) != Ordering::Less) {
    return Err("workspace_descriptor_order_or_trailing");
  }
  Ok(descriptors)
}

#[cfg(test)]
fn verify_workspace_closure(profile: HashProfile, manifest: &[u8], objects: &[WorkspaceObject]) -> Result<(), &'static str> {
  let descriptors = decode_workspace_manifest(profile, manifest)?;
  if descriptors.len() != objects.len() {
    return Err("workspace_closure_count");
  }
  for (descriptor, object) in descriptors.iter().zip(objects) {
    let decoded = decode_workspace_object(profile, &object.bytes)?;
    if descriptor.kind != decoded.kind
      || descriptor.ordinal != decoded.ordinal
      || descriptor.stored_length != object.bytes.len() as u64
      || descriptor.logical_record_count != decoded.logical_record_count
      || descriptor.digest != *blake3::hash(&object.bytes).as_bytes()
      || descriptor.name != object.name
    {
      return Err("workspace_closure_mismatch");
    }
  }
  Ok(())
}

trait WorkspaceClosureRow {
  fn stored_length(&self) -> u64;
  fn logical_record_count(&self) -> u64;
  fn digest(&self) -> [u8; 32];
}

impl WorkspaceClosureRow for WorkspaceObject {
  fn stored_length(&self) -> u64 {
    self.bytes.len() as u64
  }

  fn logical_record_count(&self) -> u64 {
    self.logical_record_count
  }

  fn digest(&self) -> [u8; 32] {
    *blake3::hash(&self.bytes).as_bytes()
  }
}

impl WorkspaceClosureRow for WorkspaceDescriptor {
  fn stored_length(&self) -> u64 {
    self.stored_length
  }

  fn logical_record_count(&self) -> u64 {
    self.logical_record_count
  }

  fn digest(&self) -> [u8; 32] {
    self.digest
  }
}

fn workspace_manifest_result<T: WorkspaceClosureRow>(state: u16, rows: &[T]) -> String {
  let bytes = rows.iter().map(WorkspaceClosureRow::stored_length).sum::<u64>();
  let records = rows.iter().map(WorkspaceClosureRow::logical_record_count).sum::<u64>();
  let mut closure = blake3::Hasher::new();
  for row in rows {
    closure.update(&row.digest());
  }
  let closure = closure.finalize().to_hex();
  format!("gc:workspace-manifest:state={state}:objects={}:records={records}:bytes={bytes}:closure={}", rows.len(), &closure[..16])
}

fn descriptor_cmp(left: &WorkspaceDescriptor, right: &WorkspaceDescriptor) -> Ordering {
  (left.kind, left.ordinal, left.name.as_bytes()).cmp(&(right.kind, right.ordinal, right.name.as_bytes()))
}

fn canonical_relative_name(name: &str) -> bool {
  !name.is_empty()
    && !name.starts_with('/')
    && !name.ends_with('/')
    && !name.contains('\\')
    && !name.as_bytes().contains(&0)
    && name.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn canonical_workspace_path(path: &str) -> bool {
  if path.is_empty() || path.contains('\\') || path.as_bytes().contains(&0) || (path.len() > 1 && path.ends_with('/')) {
    return false;
  }
  let remainder = if let Some(rest) = path.strip_prefix('/') {
    rest
  } else if path.len() >= 3 && path.as_bytes()[0].is_ascii_uppercase() && path.as_bytes()[1] == b':' && path.as_bytes()[2] == b'/' {
    &path[3..]
  } else {
    return false;
  };
  !remainder.is_empty() && remainder.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

fn sample_incarnation(profile: HashProfile, ordinal: u8) -> PhysicalIncarnationId {
  let mut logical_key = vec![0u8; profile.width()];
  let mut digest = vec![0u8; profile.width()];
  fill_sequence(&mut logical_key, 0x91u8.wrapping_add(ordinal));
  fill_sequence(&mut digest, 0xb1u8.wrapping_add(ordinal));
  PhysicalIncarnationId {
    logical_key,
    integrity_or_legacy_digest: digest,
    wal_offset: 8_192 + u64::from(ordinal) * 4_096,
    write_sequence: 700 + u64::from(ordinal),
    entity_length: 2_048,
    entry_type: 2,
    entity_version: 1,
  }
}

fn database_id() -> [u8; 16] {
  sequence_array(0x31)
}

fn run_id() -> [u8; 16] {
  sequence_array(0x51)
}

fn workspace_id() -> [u8; 16] {
  sequence_array(0x71)
}

fn run_generation() -> u64 {
  77
}

fn default_workspace_path() -> &'static str {
  "/srv/data/.taraani.aeordb-gc-3132333435363738393a3b3c3d3e3f40-5152535455565758595a5b5c5d5e5f60"
}

fn sequence_array(start: u8) -> [u8; 16] {
  let mut value = [0u8; 16];
  fill_sequence(&mut value, start);
  value
}

fn fill_sequence(bytes: &mut [u8], start: u8) {
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(index as u8);
  }
}

fn write_capabilities(bytes: &mut [u8], bits: &[usize]) {
  for bit in bits {
    bytes[bit / 8] |= 1 << (bit % 8);
  }
}

fn valid_capabilities(bytes: &[u8], bits: &[usize]) -> bool {
  let mut expected = [0u8; 32];
  write_capabilities(&mut expected, bits);
  bytes == expected
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, &'static str> {
  Ok(i64::from_le_bytes(bytes.get(offset..offset + 8).ok_or("mark_truncated")?.try_into().map_err(|_| "mark_truncated")?))
}

fn write_crc(bytes: &mut [u8]) {
  let crc_offset = bytes.len() - 4;
  put_u32(bytes, crc_offset, crc32fast::hash(&bytes[..crc_offset]));
}

fn verify_crc(bytes: &[u8]) -> Result<(), &'static str> {
  let crc_offset = bytes.len().checked_sub(4).ok_or("workspace_crc")?;
  if read_u32(bytes, crc_offset)? != crc32fast::hash(&bytes[..crc_offset]) {
    return Err("workspace_crc");
  }
  Ok(())
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repair_gc_crc(bytes: &mut [u8]) {
    write_crc(bytes);
  }

  #[test]
  fn mark_and_workspace_fixtures_round_trip() {
    let cases = fixture_cases();
    assert_eq!(cases.len(), 22);
    for case in cases {
      let (observed, _) = observe(case.format, case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
    }
  }

  #[test]
  fn every_fixture_byte_is_crc_or_structure_protected() {
    for case in fixture_cases() {
      for index in 0..case.bytes.len() {
        let mut changed = case.bytes.clone();
        changed[index] ^= 1;
        assert!(observe(case.format, case.profile, &changed).0.starts_with("error:"), "fixture {} byte {index}", case.id);
      }
    }
  }

  #[test]
  fn workspace_manifest_closes_every_typed_object() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let objects = build_workspace_objects(profile);
      let manifest = build_workspace_manifest(profile, &objects);
      assert_eq!(verify_workspace_closure(profile, &manifest, &objects), Ok(()));

      let mut wrong = objects.clone();
      wrong[2].bytes[80] ^= 1;
      assert_eq!(verify_workspace_closure(profile, &manifest, &wrong), Err("workspace_crc"));
    }
  }

  #[test]
  fn repaired_checksums_do_not_bypass_semantic_validation() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let objects = build_workspace_objects(profile);
      let manifest = build_workspace_manifest(profile, &objects);
      let manifest_digest = *blake3::hash(&manifest).as_bytes();
      let journal = build_mark_mutation_journal(profile);
      let journal_key = immutable_key(profile, GcKind::MarkMutationJournalSegment, &journal);

      let mut checkpoint = build_mark_checkpoint(profile, 1, 1, 1, default_workspace_path(), &manifest_digest, &journal_key);
      let body_offset = 32 + 40;
      checkpoint[body_offset + 6..body_offset + 8].copy_from_slice(&0u16.to_le_bytes());
      repair_gc_crc(&mut checkpoint);
      assert_eq!(decode_mark_checkpoint(profile, &checkpoint).err(), Some("mark_checkpoint_fields"));

      let mut bad_journal = journal;
      let h = profile.width();
      let operation_offset = 32 + 40 + 32 + h + 4 + 32 + 6 * h;
      bad_journal[operation_offset..operation_offset + 2].copy_from_slice(&11u16.to_le_bytes());
      repair_gc_crc(&mut bad_journal);
      assert!(decode_mark_mutation_journal(profile, &bad_journal).is_err());

      let mut bad_manifest = manifest;
      put_u32(&mut bad_manifest, 84, 1);
      write_crc(&mut bad_manifest);
      assert_eq!(decode_workspace_manifest(profile, &bad_manifest).err(), Some("workspace_manifest_header"));

      let mut bitmap = objects[0].bytes.clone();
      *bitmap.get_mut(114).unwrap() |= 0x80;
      write_crc(&mut bitmap);
      assert_eq!(decode_workspace_object(profile, &bitmap).err(), Some("workspace_bitmap_fields"));
    }
  }

  #[test]
  fn paths_names_lengths_and_order_fail_closed() {
    assert!(canonical_workspace_path("/var/lib/aeordb/gc/run"));
    assert!(canonical_workspace_path("D:/AeorDB/gc/run"));
    for path in ["", "relative/run", "/var//run", "/var/../run", "d:/lower/drive", "D:\\AeorDB\\run", "/trailing/"] {
      assert!(!canonical_workspace_path(path), "accepted {path:?}");
    }
    for name in ["", "/absolute", "../escape", "a/../b", "a//b", "a\\b", "trailing/"] {
      assert!(!canonical_relative_name(name), "accepted {name:?}");
    }

    let profile = HashProfile::Blake3_256;
    let objects = build_workspace_objects(profile);
    let mut manifest = build_workspace_manifest(profile, &objects);
    let descriptor_start = 120 + 2 * profile.width();
    put_u16(&mut manifest, descriptor_start, WorkspaceObjectKind::Diagnostic as u16);
    write_crc(&mut manifest);
    assert_eq!(decode_workspace_manifest(profile, &manifest).err(), Some("workspace_descriptor_order_or_trailing"));
  }

  #[test]
  fn fixed_formulas_hold_for_both_hash_widths() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let h = profile.width();
      assert_eq!(build_mutation_payload(profile, 1, 1, 1).len(), 36 + 6 * h);
      let journal = build_mark_mutation_journal(profile);
      let decoded = decode_gc_value(&journal, MARK_JOURNAL_MAX).unwrap();
      assert_eq!(decoded.identity.len(), 40);
      assert_eq!(decoded.body.len(), 32 + h + 2 * (4 + 36 + 6 * h));

      for object in build_workspace_objects(profile) {
        assert_eq!(read_u64(&object.bytes, 8).unwrap() as usize, object.bytes.len());
        assert_eq!(read_u64(&object.bytes, 72).unwrap() as usize + WORKSPACE_OBJECT_HEADER + 4, object.bytes.len());
      }
    }
  }
}
