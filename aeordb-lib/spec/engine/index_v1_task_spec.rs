use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_artifact::decode_index_manifest;
use aeordb::engine::v4::index_page::decode_artifact_directory;
use aeordb::engine::v4::index_task::{
  ExternalWorkspaceDescriptorWriteV1, IndexTaskAttachmentClosureBuilderV1, IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1,
  IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1,
  MutationRecordWriteV1, MutationSideWriteV1, decode_index_task_checkpoint, decode_mutation_journal, encode_index_task_checkpoint,
  encode_mutation_journal,
};
use aeordb::engine::v4::reader::MalformedInputClass;

fn fixture_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn fixture_bytes(name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(name)).unwrap()
}

fn profile_name(hash_algorithm: HashAlgorithm) -> &'static str {
  match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("P5-7 tests use only frozen v4 hash profiles"),
  }
}

fn sample_hash(hash_algorithm: HashAlgorithm, start: u8) -> Vec<u8> {
  (0..hash_algorithm.hash_length()).map(|index| start.wrapping_add(index as u8)).collect()
}

fn sample_journal_records(hash_algorithm: HashAlgorithm, owner_kind: JournalOwnerKindV1) -> Vec<MutationRecordWriteV1<'static>> {
  let root_before = Box::leak(sample_hash(hash_algorithm, 0x41).into_boxed_slice());
  let root_after = Box::leak(sample_hash(hash_algorithm, 0x51).into_boxed_slice());
  let mutation_id = Box::leak(sample_hash(hash_algorithm, 0x61).into_boxed_slice());
  if owner_kind == JournalOwnerKindV1::Task {
    vec![
      MutationRecordWriteV1 {
        kind: MutationKindV1::Create,
        sequence: 900,
        mutation_id,
        batch_ordinal: 0,
        batch_count: 2,
        root_before,
        root_after,
        before: None,
        after: Some(MutationSideWriteV1 { path: "/docs/a.md", revision: Box::leak(sample_hash(hash_algorithm, 0x71).into_boxed_slice()) }),
        committed_at_ms: 1_800_000_000_001,
      },
      MutationRecordWriteV1 {
        kind: MutationKindV1::Create,
        sequence: 900,
        mutation_id,
        batch_ordinal: 1,
        batch_count: 2,
        root_before,
        root_after,
        before: None,
        after: Some(MutationSideWriteV1 { path: "/docs/b.md", revision: Box::leak(sample_hash(hash_algorithm, 0x81).into_boxed_slice()) }),
        committed_at_ms: 1_800_000_000_001,
      },
    ]
  } else {
    vec![MutationRecordWriteV1 {
      kind: MutationKindV1::Update,
      sequence: 901,
      mutation_id,
      batch_ordinal: 0,
      batch_count: 1,
      root_before,
      root_after,
      before: Some(MutationSideWriteV1 {
        path: "/docs/system.json",
        revision: Box::leak(sample_hash(hash_algorithm, 0x72).into_boxed_slice()),
      }),
      after: Some(MutationSideWriteV1 {
        path: "/docs/system.json",
        revision: Box::leak(sample_hash(hash_algorithm, 0x82).into_boxed_slice()),
      }),
      committed_at_ms: 1_800_000_000_002,
    }]
  }
}

fn encode_sample_journal(hash_algorithm: HashAlgorithm, owner_kind: JournalOwnerKindV1) -> Vec<u8> {
  let records = sample_journal_records(hash_algorithm, owner_kind);
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm,
    owner_id: if owner_kind == JournalOwnerKindV1::System { *b"AEORIDXJOURNALV1" } else { [0x31; 16] },
    owner_kind,
    generation: if owner_kind == JournalOwnerKindV1::Task { 40 } else { 41 },
    segment_ordinal: if owner_kind == JournalOwnerKindV1::Task { 0 } else { 8 },
    chain_reset: owner_kind == JournalOwnerKindV1::Task,
    previous_segment: if owner_kind == JournalOwnerKindV1::Task {
      Box::leak(vec![0; hash_algorithm.hash_length()].into_boxed_slice())
    } else {
      Box::leak(sample_hash(hash_algorithm, 0x21).into_boxed_slice())
    },
    semantic_state_root: Box::leak(sample_hash(hash_algorithm, 0x91).into_boxed_slice()),
    runtime_boot_id: [0xa1; 16],
    records: &records,
  })
  .unwrap()
  .value
}

fn sample_attachments(hash_algorithm: HashAlgorithm, external: bool) -> Vec<IndexTaskAttachmentWriteV1<'static>> {
  if external {
    vec![IndexTaskAttachmentWriteV1 {
      role: IndexTaskAttachmentRoleV1::MutationJournalHead,
      owner_id: Box::leak(sample_hash(hash_algorithm, 0x21).into_boxed_slice()),
      artifact_hash: Box::leak(sample_hash(hash_algorithm, 0x31).into_boxed_slice()),
      birth_generation: 17,
    }]
  } else {
    vec![
      IndexTaskAttachmentWriteV1 {
        role: IndexTaskAttachmentRoleV1::ScopeOrdinalDirectoryRoot,
        owner_id: Box::leak(sample_hash(hash_algorithm, 0x11).into_boxed_slice()),
        artifact_hash: Box::leak(sample_hash(hash_algorithm, 0x21).into_boxed_slice()),
        birth_generation: 12,
      },
      IndexTaskAttachmentWriteV1 {
        role: IndexTaskAttachmentRoleV1::CandidateScopeManifest,
        owner_id: Box::leak(sample_hash(hash_algorithm, 0x11).into_boxed_slice()),
        artifact_hash: Box::leak(sample_hash(hash_algorithm, 0x31).into_boxed_slice()),
        birth_generation: 13,
      },
    ]
  }
}

fn encode_sample_checkpoint(hash_algorithm: HashAlgorithm, external: bool) -> Vec<u8> {
  let attachments = sample_attachments(hash_algorithm, external);
  let mut capabilities = [0u8; 32];
  for bit in [7usize, 8, 9, 10, 11] {
    capabilities[bit / 8] |= 1 << (bit % 8);
  }
  encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
    hash_algorithm,
    task_id: if external { [0xc2; 16] } else { [0xc1; 16] },
    checkpoint_sequence: if external { 18 } else { 17 },
    generation: 20,
    task_kind: if external { IndexTaskKindV1::V0Migration } else { IndexTaskKindV1::ScopeBuild },
    state: if external { IndexTaskStateV1::Running } else { IndexTaskStateV1::CompleteUnpublished },
    phase: if external { 3 } else { 6 },
    required_capabilities: &capabilities,
    started_at_ms: 1_800_000_000_010,
    updated_at_ms: 1_800_000_000_020,
    source_root: Box::leak(sample_hash(hash_algorithm, 0x41).into_boxed_slice()),
    target_root: (!external).then(|| Box::leak(sample_hash(hash_algorithm, 0x51).into_boxed_slice()) as &[u8]),
    primary_id: (!external).then(|| Box::leak(sample_hash(hash_algorithm, 0x11).into_boxed_slice()) as &[u8]),
    journal_head: Some(Box::leak(sample_hash(hash_algorithm, 0x61).into_boxed_slice())),
    journal_floor_sequence: 800,
    journal_audited_through: 900,
    next_document_ordinal: 1_024,
    completed_work: if external { 100 } else { 200 },
    total_work_hint: 1_000,
    resume_key: if external { b"legacy-offset:0000000000001000".as_slice() } else { b"/docs/guide.md".as_slice() },
    attachments: &attachments,
    external: external.then_some(ExternalWorkspaceDescriptorWriteV1 {
      workspace_id: [0xd1; 16],
      manifest_digest: [0xe1; 32],
      durable_sequence: 77,
      durable_bytes: 8192,
      path: "/var/lib/aeordb/workspaces/migrate-01/run-0001",
    }),
  })
  .unwrap()
  .value
}

#[test]
fn journal_writers_match_all_independent_both_width_fixtures() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for owner_kind in [JournalOwnerKindV1::Task, JournalOwnerKindV1::System] {
      let owner = if owner_kind == JournalOwnerKindV1::Task { "task" } else { "system" };
      let expected = fixture_bytes(&format!("aidx-{}-{owner}-mutation-journal-valid.bin", profile_name(hash_algorithm)));
      let encoded = encode_sample_journal(hash_algorithm, owner_kind);
      assert_eq!(encoded, expected);
      assert_eq!(
        decode_mutation_journal(&encoded, hash_algorithm).unwrap().records.len(),
        if owner_kind == JournalOwnerKindV1::Task { 2 } else { 1 }
      );
    }
  }
}

#[test]
fn checkpoint_writers_match_all_independent_both_width_fixtures() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for external in [false, true] {
      let mode = if external { "external" } else { "embedded" };
      let expected = fixture_bytes(&format!("aidx-{}-index-task-checkpoint-{mode}-valid.bin", profile_name(hash_algorithm)));
      let encoded = encode_sample_checkpoint(hash_algorithm, external);
      assert_eq!(encoded, expected);
      let decoded = decode_index_task_checkpoint(&encoded, hash_algorithm).unwrap();
      assert_eq!(decoded.external.is_some(), external);
    }
  }
}

fn single_mutation(hash_algorithm: HashAlgorithm, kind: MutationKindV1) -> MutationRecordWriteV1<'static> {
  let before = matches!(kind, MutationKindV1::Update | MutationKindV1::Delete | MutationKindV1::Move | MutationKindV1::Transition)
    .then(|| MutationSideWriteV1 { path: "/before", revision: Box::leak(sample_hash(hash_algorithm, 0x71).into_boxed_slice()) });
  let after =
    matches!(kind, MutationKindV1::Create | MutationKindV1::Update | MutationKindV1::Move | MutationKindV1::Copy | MutationKindV1::Restore)
      .then(|| MutationSideWriteV1 {
        path: if kind == MutationKindV1::Move { "/after" } else { "/before" },
        revision: Box::leak(sample_hash(hash_algorithm, 0x81).into_boxed_slice()),
      });
  MutationRecordWriteV1 {
    kind,
    sequence: 1,
    mutation_id: Box::leak(sample_hash(hash_algorithm, 0x11).into_boxed_slice()),
    batch_ordinal: 0,
    batch_count: 1,
    root_before: Box::leak(sample_hash(hash_algorithm, 0x21).into_boxed_slice()),
    root_after: Box::leak(sample_hash(hash_algorithm, 0x31).into_boxed_slice()),
    before,
    after,
    committed_at_ms: 1,
  }
}

fn journal_for_records<'a>(hash_algorithm: HashAlgorithm, records: &'a [MutationRecordWriteV1<'a>]) -> MutationJournalWriteV1<'a> {
  MutationJournalWriteV1 {
    hash_algorithm,
    owner_id: [0x11; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: Box::leak(vec![0; hash_algorithm.hash_length()].into_boxed_slice()),
    semantic_state_root: Box::leak(sample_hash(hash_algorithm, 0x41).into_boxed_slice()),
    runtime_boot_id: [0x51; 16],
    records,
  }
}

#[test]
fn journal_writer_accepts_every_mutation_kind_and_rejects_invalid_presence_path_width_and_batch_closure() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  for kind in [
    MutationKindV1::Create,
    MutationKindV1::Update,
    MutationKindV1::Delete,
    MutationKindV1::Move,
    MutationKindV1::Copy,
    MutationKindV1::Restore,
    MutationKindV1::Transition,
  ] {
    let records = [single_mutation(hash_algorithm, kind)];
    assert!(encode_mutation_journal(&journal_for_records(hash_algorithm, &records)).is_ok(), "{kind:?}");
  }

  let mut invalid = single_mutation(hash_algorithm, MutationKindV1::Create);
  invalid.before = invalid.after;
  let error = encode_mutation_journal(&journal_for_records(hash_algorithm, &[invalid])).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut invalid = single_mutation(hash_algorithm, MutationKindV1::Move);
  invalid.after = invalid.before;
  assert_eq!(
    encode_mutation_journal(&journal_for_records(hash_algorithm, &[invalid])).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );

  let mut invalid = single_mutation(hash_algorithm, MutationKindV1::Update);
  invalid.before.as_mut().unwrap().path = "relative";
  assert_eq!(
    encode_mutation_journal(&journal_for_records(hash_algorithm, &[invalid])).unwrap_err().class(),
    MalformedInputClass::InvalidUtf8PathGlobOrNativePath
  );

  let mut invalid = single_mutation(hash_algorithm, MutationKindV1::Delete);
  invalid.mutation_id = &[1; 31];
  assert_eq!(
    encode_mutation_journal(&journal_for_records(hash_algorithm, &[invalid])).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let mut invalid = single_mutation(hash_algorithm, MutationKindV1::Create);
  invalid.batch_count = 2;
  assert_eq!(
    encode_mutation_journal(&journal_for_records(hash_algorithm, &[invalid])).unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn journal_writer_rejects_invalid_stream_chain_owner_bounds_and_order_without_publishing_partial_bytes() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let mut records = vec![single_mutation(hash_algorithm, MutationKindV1::Create)];
  let mut request = journal_for_records(hash_algorithm, &records);
  request.owner_id = [0; 16];
  assert_eq!(encode_mutation_journal(&request).unwrap_err().class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let mut request = journal_for_records(hash_algorithm, &records);
  request.chain_reset = false;
  assert_eq!(encode_mutation_journal(&request).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  records.push(single_mutation(hash_algorithm, MutationKindV1::Create));
  assert_eq!(
    encode_mutation_journal(&journal_for_records(hash_algorithm, &records)).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let oversized_records = (0..=10_000)
    .map(|index| {
      let mut record = single_mutation(hash_algorithm, MutationKindV1::Create);
      record.sequence = index + 1;
      record
    })
    .collect::<Vec<_>>();
  assert_eq!(
    encode_mutation_journal(&journal_for_records(hash_algorithm, &oversized_records)).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let large_path = format!("/{}", "a".repeat(2 * 1_024 * 1_024));
  let oversized_bytes = (0..9)
    .map(|index| {
      let mut record = single_mutation(hash_algorithm, MutationKindV1::Create);
      record.sequence = index + 1;
      record.after.as_mut().unwrap().path = &large_path;
      record
    })
    .collect::<Vec<_>>();
  assert_eq!(
    encode_mutation_journal(&journal_for_records(hash_algorithm, &oversized_bytes)).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );
}

fn minimal_checkpoint<'a>(
  hash_algorithm: HashAlgorithm,
  attachments: &'a [IndexTaskAttachmentWriteV1<'a>],
) -> IndexTaskCheckpointWriteV1<'a> {
  IndexTaskCheckpointWriteV1 {
    hash_algorithm,
    task_id: [0x11; 16],
    checkpoint_sequence: 1,
    generation: 1,
    task_kind: IndexTaskKindV1::ScopeBuild,
    state: IndexTaskStateV1::Running,
    phase: 1,
    required_capabilities: &[0; 32],
    started_at_ms: 1,
    updated_at_ms: 2,
    source_root: Box::leak(sample_hash(hash_algorithm, 0x21).into_boxed_slice()),
    target_root: None,
    primary_id: None,
    journal_head: None,
    journal_floor_sequence: 0,
    journal_audited_through: 0,
    next_document_ordinal: 0,
    completed_work: 0,
    total_work_hint: 0,
    resume_key: &[],
    attachments,
    external: None,
  }
}

#[test]
fn checkpoint_writer_rejects_identity_capability_phase_time_progress_journal_and_component_errors() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.task_id = [0; 16];
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);

  let mut capabilities = [0u8; 32];
  capabilities[3] = 1;
  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.required_capabilities = &capabilities;
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::UnknownRequiredCapability);

  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.phase = 0;
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::UnknownTypeKindOrEnum);

  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.updated_at_ms = 0;
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.completed_work = 2;
  request.total_work_hint = 1;
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.journal_floor_sequence = 1;
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let oversized_resume = vec![0; 1_048_577];
  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.resume_key = &oversized_resume;
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::AllocationAmplification);
}

#[test]
fn checkpoint_writer_rejects_unsorted_invalid_or_oversized_attachments_and_external_descriptors() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let first = IndexTaskAttachmentWriteV1 {
    role: IndexTaskAttachmentRoleV1::CandidateScopeManifest,
    owner_id: Box::leak(sample_hash(hash_algorithm, 0x21).into_boxed_slice()),
    artifact_hash: Box::leak(sample_hash(hash_algorithm, 0x31).into_boxed_slice()),
    birth_generation: 1,
  };
  let second = IndexTaskAttachmentWriteV1 { role: IndexTaskAttachmentRoleV1::ScopeOrdinalDirectoryRoot, ..first };
  let attachments = [first, second];
  assert_eq!(
    encode_index_task_checkpoint(&minimal_checkpoint(hash_algorithm, &attachments)).unwrap_err().class(),
    MalformedInputClass::NoncanonicalOrderOrDuplicate
  );

  let invalid = IndexTaskAttachmentWriteV1 { owner_id: &[1; 31], ..first };
  assert_eq!(
    encode_index_task_checkpoint(&minimal_checkpoint(hash_algorithm, &[invalid])).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );

  let oversized = vec![first; 4_097];
  assert_eq!(
    encode_index_task_checkpoint(&minimal_checkpoint(hash_algorithm, &oversized)).unwrap_err().class(),
    MalformedInputClass::AllocationAmplification
  );

  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.external = Some(ExternalWorkspaceDescriptorWriteV1 {
    workspace_id: [0x11; 16],
    manifest_digest: [0x21; 32],
    durable_sequence: 1,
    durable_bytes: 1,
    path: "relative",
  });
  assert_eq!(encode_index_task_checkpoint(&request).unwrap_err().class(), MalformedInputClass::InvalidUtf8PathGlobOrNativePath);
}

fn artifact_fixture(hash_algorithm: HashAlgorithm, suffix: &str) -> Vec<u8> {
  fixture_bytes(&format!("aidx-{}-{suffix}", profile_name(hash_algorithm)))
}

fn closure_artifacts(hash_algorithm: HashAlgorithm) -> Vec<(IndexTaskAttachmentRoleV1, Vec<u8>, Vec<u8>, u64)> {
  let specifications = [
    (IndexTaskAttachmentRoleV1::ScopeOrdinalDirectoryRoot, "scope-ordinal-directory-leaf-valid.bin"),
    (IndexTaskAttachmentRoleV1::ScopeReverseDirectoryRoot, "scope-reverse-directory-leaf-valid.bin"),
    (IndexTaskAttachmentRoleV1::ValueDirectoryRoot, "value-directory-leaf-valid.bin"),
    (IndexTaskAttachmentRoleV1::ValueStateDirectoryRoot, "value-document-state-directory-leaf-valid.bin"),
    (IndexTaskAttachmentRoleV1::PostingDirectoryRoot, "posting-directory-leaf-valid.bin"),
    (IndexTaskAttachmentRoleV1::IndexStateDirectoryRoot, "index-document-state-directory-leaf-valid.bin"),
    (IndexTaskAttachmentRoleV1::NvtTileDirectoryRoot, "nvt-tile-directory-leaf-valid.bin"),
    (IndexTaskAttachmentRoleV1::CandidateScopeManifest, "scope-catalog-manifest-populated.bin"),
    (IndexTaskAttachmentRoleV1::CandidateValueManifest, "value-store-manifest-populated.bin"),
    (IndexTaskAttachmentRoleV1::CandidateFieldManifest, "field-index-manifest-populated.bin"),
    (IndexTaskAttachmentRoleV1::CandidateNvtManifest, "field-nvt-manifest-populated.bin"),
  ];
  let mut artifacts = specifications
    .into_iter()
    .map(|(role, suffix)| {
      let bytes = artifact_fixture(hash_algorithm, suffix);
      if role.id() <= 7 {
        let (owner_id, generation) = {
          let decoded = decode_artifact_directory(&bytes, hash_algorithm).unwrap();
          (decoded.owner_id.to_vec(), decoded.generation)
        };
        (role, bytes, owner_id, generation)
      } else {
        let (owner_id, generation) = {
          let decoded = decode_index_manifest(&bytes, hash_algorithm).unwrap();
          (decoded.owner_id.to_vec(), decoded.generation)
        };
        (role, bytes, owner_id, generation)
      }
    })
    .collect::<Vec<_>>();
  let journal = encode_sample_journal(hash_algorithm, JournalOwnerKindV1::Task);
  let generation = decode_mutation_journal(&journal, hash_algorithm).unwrap().generation;
  artifacts.push((IndexTaskAttachmentRoleV1::MutationJournalHead, journal, sample_hash(hash_algorithm, 0xf1), generation));
  artifacts
}

fn closure_checkpoint(hash_algorithm: HashAlgorithm, artifacts: &[(IndexTaskAttachmentRoleV1, Vec<u8>, Vec<u8>, u64)]) -> Vec<u8> {
  let attachments = artifacts
    .iter()
    .map(|(role, bytes, owner_id, birth_generation)| {
      let artifact_hash = if role.id() <= 7 {
        decode_artifact_directory(bytes, hash_algorithm).unwrap().key
      } else if role.id() <= 11 {
        decode_index_manifest(bytes, hash_algorithm).unwrap().key
      } else {
        decode_mutation_journal(bytes, hash_algorithm).unwrap().key
      };
      IndexTaskAttachmentWriteV1 {
        role: *role,
        owner_id,
        artifact_hash: Box::leak(artifact_hash.into_boxed_slice()),
        birth_generation: *birth_generation,
      }
    })
    .collect::<Vec<_>>();
  let journal_head = attachments.last().unwrap().artifact_hash;
  let mut request = minimal_checkpoint(hash_algorithm, &attachments);
  request.task_id = [0x31; 16];
  request.journal_head = Some(journal_head);
  request.journal_floor_sequence = 900;
  request.journal_audited_through = 900;
  encode_index_task_checkpoint(&request).unwrap().value
}

fn close_single_attachment(
  hash_algorithm: HashAlgorithm,
  task_id: [u8; 16],
  journal_head: Option<&[u8]>,
  attachment: IndexTaskAttachmentWriteV1<'_>,
  artifact: &[u8],
) -> Result<(), MalformedInputClass> {
  let attachments = [attachment];
  let mut request = minimal_checkpoint(hash_algorithm, &attachments);
  request.task_id = task_id;
  request.journal_head = journal_head;
  if journal_head.is_some() {
    request.journal_floor_sequence = 1;
    request.journal_audited_through = 1;
  }
  let checkpoint_bytes = encode_index_task_checkpoint(&request).unwrap().value;
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();
  let mut builder = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
  builder.observe_encoded(artifact).map_err(|error| error.class())?;
  builder.finish().map(|_| ()).map_err(|error| error.class())
}

#[test]
fn streaming_checkpoint_attachment_closure_roots_every_registered_role_at_both_hash_widths() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let artifacts = closure_artifacts(hash_algorithm);
    let checkpoint_bytes = closure_checkpoint(hash_algorithm, &artifacts);
    let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();
    let mut builder = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
    for (_, bytes, _, _) in &artifacts {
      builder.observe_encoded(bytes).unwrap();
    }
    let closure = builder.finish().unwrap();
    assert_eq!(closure.checkpoint_hash(), checkpoint.key);
    assert_eq!(closure.rooted_artifact_count(), 12);
    assert!(closure.journal_head_validated());
  }
}

#[test]
fn streaming_checkpoint_attachment_closure_fails_closed_for_missing_extra_reordered_corrupt_or_detached_artifacts() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let artifacts = closure_artifacts(hash_algorithm);
  let checkpoint_bytes = closure_checkpoint(hash_algorithm, &artifacts);
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();

  let mut missing = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
  for (_, bytes, _, _) in artifacts.iter().take(11) {
    missing.observe_encoded(bytes).unwrap();
  }
  assert_eq!(missing.finish().unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut reordered = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
  assert_eq!(reordered.observe_encoded(&artifacts[1].1).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);
  assert_eq!(reordered.observe_encoded(&artifacts[0].1).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut corrupt_bytes = artifacts[0].1.clone();
  corrupt_bytes[40] ^= 1;
  let mut corrupt = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
  assert_eq!(corrupt.observe_encoded(&corrupt_bytes).unwrap_err().class(), MalformedInputClass::ChecksumOrIntegrityMismatch);
  assert_eq!(corrupt.finish().unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut complete = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
  for (_, bytes, _, _) in &artifacts {
    complete.observe_encoded(bytes).unwrap();
  }
  assert_eq!(complete.observe_encoded(&artifacts[0].1).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);
}

#[test]
fn streaming_checkpoint_attachment_closure_rejects_wrong_hash_generation_owner_role_and_profile() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let artifacts = closure_artifacts(hash_algorithm);
  let (role, bytes, owner_id, generation) = &artifacts[0];
  let artifact_hash = decode_artifact_directory(bytes, hash_algorithm).unwrap().key;
  let valid = IndexTaskAttachmentWriteV1 { role: *role, owner_id, artifact_hash: &artifact_hash, birth_generation: *generation };
  assert_eq!(close_single_attachment(hash_algorithm, [0x11; 16], None, valid, bytes), Ok(()));

  let wrong_hash = sample_hash(hash_algorithm, 0xf1);
  assert_eq!(
    close_single_attachment(hash_algorithm, [0x11; 16], None, IndexTaskAttachmentWriteV1 { artifact_hash: &wrong_hash, ..valid }, bytes),
    Err(MalformedInputClass::CrossRecordClosureMismatch)
  );
  assert_eq!(
    close_single_attachment(
      hash_algorithm,
      [0x11; 16],
      None,
      IndexTaskAttachmentWriteV1 { birth_generation: generation + 1, ..valid },
      bytes,
    ),
    Err(MalformedInputClass::CrossRecordClosureMismatch)
  );
  let wrong_owner = sample_hash(hash_algorithm, 0xe1);
  assert_eq!(
    close_single_attachment(hash_algorithm, [0x11; 16], None, IndexTaskAttachmentWriteV1 { owner_id: &wrong_owner, ..valid }, bytes),
    Err(MalformedInputClass::CrossRecordClosureMismatch)
  );
  assert_eq!(
    close_single_attachment(
      hash_algorithm,
      [0x11; 16],
      None,
      IndexTaskAttachmentWriteV1 { role: IndexTaskAttachmentRoleV1::ScopeReverseDirectoryRoot, ..valid },
      bytes,
    ),
    Err(MalformedInputClass::CrossRecordClosureMismatch)
  );

  let checkpoint_bytes = closure_checkpoint(hash_algorithm, &artifacts);
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();
  assert_eq!(
    IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, HashAlgorithm::Sha512).unwrap_err().class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
}

#[test]
fn journal_attachment_closure_accepts_own_task_or_fixed_system_stream_and_rejects_foreign_or_detached_task_streams() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  for owner_kind in [JournalOwnerKindV1::Task, JournalOwnerKindV1::System] {
    let journal = encode_sample_journal(hash_algorithm, owner_kind);
    let decoded = decode_mutation_journal(&journal, hash_algorithm).unwrap();
    let owner_id = sample_hash(hash_algorithm, 0xf1);
    let attachment = IndexTaskAttachmentWriteV1 {
      role: IndexTaskAttachmentRoleV1::MutationJournalHead,
      owner_id: &owner_id,
      artifact_hash: &decoded.key,
      birth_generation: decoded.generation,
    };
    assert_eq!(close_single_attachment(hash_algorithm, [0x31; 16], Some(&decoded.key), attachment, &journal), Ok(()));

    if owner_kind == JournalOwnerKindV1::Task {
      assert_eq!(
        close_single_attachment(hash_algorithm, [0x32; 16], Some(&decoded.key), attachment, &journal),
        Err(MalformedInputClass::CrossRecordClosureMismatch)
      );
      let detached_head = sample_hash(hash_algorithm, 0xe1);
      assert_eq!(
        close_single_attachment(hash_algorithm, [0x31; 16], Some(&detached_head), attachment, &journal),
        Err(MalformedInputClass::CrossRecordClosureMismatch)
      );
    }
  }
}

#[test]
fn empty_checkpoint_attachment_closure_is_complete_only_without_a_journal_head() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let checkpoint_bytes = encode_index_task_checkpoint(&minimal_checkpoint(hash_algorithm, &[])).unwrap().value;
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();
  let closure = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap().finish().unwrap();
  assert_eq!(closure.rooted_artifact_count(), 0);
  assert!(!closure.journal_head_validated());

  let journal_head = sample_hash(hash_algorithm, 0x31);
  let mut request = minimal_checkpoint(hash_algorithm, &[]);
  request.journal_head = Some(&journal_head);
  request.journal_floor_sequence = 1;
  request.journal_audited_through = 1;
  let checkpoint_bytes = encode_index_task_checkpoint(&request).unwrap().value;
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();
  assert_eq!(
    IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap().finish().unwrap_err().class(),
    MalformedInputClass::CrossRecordClosureMismatch
  );
}

#[test]
fn task_codec_and_closure_writer_surface_remains_shadow_only() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = fs::read_to_string(root.join("src/engine/v4/index_task.rs")).unwrap();
  assert_eq!(source.matches("pub fn encode_mutation_journal(").count(), 1);
  assert_eq!(source.matches("pub fn encode_index_task_checkpoint(").count(), 1);
  assert_eq!(source.matches("pub struct IndexTaskAttachmentClosureBuilderV1").count(), 1);

  let forbidden = ["src/engine/storage_engine.rs", "src/server", "src/tasks.rs"];
  for relative in forbidden {
    let path = root.join(relative);
    let contents = if path.is_dir() {
      fs::read_dir(path)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "rs"))
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
    } else {
      fs::read_to_string(path).unwrap_or_default()
    };
    assert!(!contents.contains("encode_mutation_journal("), "{relative}");
    assert!(!contents.contains("encode_index_task_checkpoint("), "{relative}");
    assert!(!contents.contains("IndexTaskAttachmentClosureBuilderV1"), "{relative}");
  }
}
