use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::durability_coordinator::CommitClass;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::first_authority::{
  IndexActivePointerPublicationRequestV1, IndexArtifactBatchPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::RetirementJournalOwnerV1;
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerWriteV1, EncodedImmutableIndexArtifactV1, decode_index_manifest, encode_active_pointer,
};
use aeordb::engine::HashAlgorithm;
use tokio_util::sync::CancellationToken;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
#[path = "../helpers/v4_index_authority.rs"]
mod authority_fixture;
use authority_fixture::{DATABASE_ID, create_publisher_for, reopen, retirement_owner_for};

#[test]
fn first_authority_selects_retries_replaces_and_reopens_one_active_pointer_pair() {
  let (_directory, path, publisher) = create_publisher();
  let scope_manifest = immutable_fixture("aidx-blake3-256-scope-catalog-manifest-empty.bin");
  let value_manifest = immutable_fixture("aidx-blake3-256-value-store-manifest-empty.bin");
  let manifest = immutable_fixture("aidx-blake3-256-field-index-manifest-empty.bin");
  let manifest_view = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&scope_manifest, &value_manifest, &manifest],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(&cancellation, &memory);

  let pointer_a = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let first = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_a,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(first.pointer_sequence, 1);
  assert_eq!(first.selected_slot, 0);
  assert!(!first.idempotent);

  let retry = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_a,
        publication_timestamp_ms: 1_700_000_000_301,
        monotonic_now_ms: 1_700_000_000_301,
      },
      &mut retirement,
    )
    .unwrap();
  assert!(retry.idempotent);
  assert_eq!(retry.observation, first.observation);

  let pointer_b = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 1,
    sequence: 2,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_b,
        publication_timestamp_ms: 1_700_000_000_400,
        monotonic_now_ms: 1_700_000_000_400,
      },
      &mut retirement,
    )
    .unwrap();

  let pointer_a_replacement = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 0,
    sequence: 3,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let replacement = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer_a_replacement,
        publication_timestamp_ms: 1_700_000_000_500,
        monotonic_now_ms: 1_700_000_000_500,
      },
      &mut retirement,
    )
    .unwrap();
  assert_eq!(replacement.pointer_sequence, 3);
  assert_eq!(replacement.selected_slot, 0);
  assert!(replacement.replaced_slot);
  assert!(replacement.retirement_hard_publication_sequence.is_some());

  drop(publisher);
  let reopened = reopen(&path);
  let pair = reopened.load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::FieldIndex, manifest_view.owner_id).unwrap();
  let selected = pair.selected.unwrap();
  assert_eq!(selected.bytes, pointer_a_replacement.value);
  assert_eq!(selected.pointer_sequence, 3);
  assert_eq!(selected.target_manifest_hash, manifest.key);
  assert_eq!(pair.slots[0].as_ref().unwrap().pointer_sequence, 3);
  assert_eq!(pair.slots[1].as_ref().unwrap().pointer_sequence, 2);
}

#[test]
fn index_hard_barrier_returns_real_durability_evidence_without_moving_semantic_authority() {
  let (_directory, path, publisher) = create_publisher();
  let before = publisher.observe().unwrap();
  let receipt = publisher.publish_index_hard_barrier(&DATABASE_ID, 1_700_000_000_200).unwrap();

  assert_eq!(receipt.durability.class, CommitClass::HardAuthority);
  assert_ne!(receipt.durability.sequence, 0);
  assert_eq!(receipt.durability.hard_frontier, receipt.durability.sequence);
  assert_eq!(receipt.observation.selected.header.slot_sequence, before.selected.header.slot_sequence + 1);
  assert_eq!(receipt.observation.selected.header.updated_at_ms, 1_700_000_000_200);
  assert_eq!(receipt.observation.selected.header.write_sequence_high_water, before.selected.header.write_sequence_high_water);
  assert_eq!(receipt.observation.selected.header.hot_tail_offset, before.selected.header.hot_tail_offset);
  assert_eq!(receipt.observation.selected.header.entry_count, before.selected.header.entry_count);
  assert_eq!(receipt.observation.selected.header.head_hash, before.selected.header.head_hash);

  let error = publisher.publish_index_hard_barrier(&DATABASE_ID, 0).unwrap_err();
  assert_eq!(error.code(), "index_hard_barrier_time");
  let error = publisher.publish_index_hard_barrier(&[0x99; 16], 1_700_000_000_201).unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_database_mismatch");

  drop(publisher);
  let reopened = reopen(&path).observe().unwrap();
  assert_eq!(reopened, receipt.observation);
}

#[test]
fn active_pointer_refuses_incomplete_foreign_and_noncanonical_publication_without_moving_authority() {
  let (_directory, _path, publisher) = create_publisher();
  let scope_manifest = immutable_fixture("aidx-blake3-256-scope-catalog-manifest-empty.bin");
  let value_manifest = immutable_fixture("aidx-blake3-256-value-store-manifest-empty.bin");
  let field_manifest = immutable_fixture("aidx-blake3-256-field-index-manifest-empty.bin");
  let field = decode_index_manifest(&field_manifest.value, ALGORITHM).unwrap();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&field_manifest],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner(&cancellation, &memory);
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: field.generation,
    owner_id: field.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &field_manifest.key,
  })
  .unwrap();
  let before = publisher.observe().unwrap();
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_target_closure");
  assert_eq!(publisher.observe().unwrap(), before);
  assert!(publisher
    .load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::FieldIndex, field.owner_id)
    .unwrap()
    .selected
    .is_none());

  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&scope_manifest, &value_manifest],
      publication_timestamp_ms: 1_700_000_000_400,
    })
    .unwrap();
  let ready = publisher.observe().unwrap();
  let wrong_slot = encode_active_pointer(&ActivePointerWriteV1 { slot: 1, ..active_pointer_write(&field_manifest, 0, 1) }).unwrap();
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &wrong_slot,
        publication_timestamp_ms: 1_700_000_000_500,
        monotonic_now_ms: 1_700_000_000_500,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_rewrite_plan");
  assert_eq!(publisher.observe().unwrap(), ready);

  let foreign_database = [0x99; 16];
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &foreign_database,
        pointer: &pointer,
        publication_timestamp_ms: 1_700_000_000_501,
        monotonic_now_ms: 1_700_000_000_501,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_database_mismatch");
  assert_eq!(publisher.observe().unwrap(), ready);

  let mut bad_key = pointer.clone();
  bad_key.key[0] ^= 1;
  let error = publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &bad_key,
        publication_timestamp_ms: 1_700_000_000_502,
        monotonic_now_ms: 1_700_000_000_502,
      },
      &mut retirement,
    )
    .unwrap_err();
  assert_eq!(error.code(), "index_active_pointer_prepared_mismatch");
  assert_eq!(publisher.observe().unwrap(), ready);
}

#[test]
fn active_pointer_authority_round_trips_the_widest_hash_profile() {
  let algorithm = HashAlgorithm::Sha512;
  let (_directory, path, publisher) = create_publisher_for(algorithm);
  let manifest = immutable_fixture_for(algorithm, "aidx-sha512-scope-catalog-manifest-empty.bin");
  let manifest_view = decode_index_manifest(&manifest.value, algorithm).unwrap();
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&manifest],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner_for(algorithm, &cancellation, &memory);
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind: ActivePointerKindV1::ScopeCatalog,
    hash_algorithm: algorithm,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot: 0,
    sequence: 1,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  publisher
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &DATABASE_ID,
        pointer: &pointer,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 1_700_000_000_300,
      },
      &mut retirement,
    )
    .unwrap();
  drop(publisher);
  let selected = reopen(&path)
    .load_index_active_pointer_pair(&DATABASE_ID, ActivePointerKindV1::ScopeCatalog, manifest_view.owner_id)
    .unwrap()
    .selected
    .unwrap();
  assert_eq!(selected.bytes, pointer.value);
  assert_eq!(selected.owner_id.len(), 64);
}

fn create_publisher() -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  create_publisher_for(ALGORITHM)
}

fn retirement_owner(cancellation: &CancellationToken, memory: &MemoryCoordinator) -> RetirementJournalOwnerV1 {
  retirement_owner_for(ALGORITHM, cancellation, memory)
}

fn immutable_fixture(name: &str) -> EncodedImmutableIndexArtifactV1 {
  immutable_fixture_for(ALGORITHM, name)
}

fn immutable_fixture_for(algorithm: HashAlgorithm, name: &str) -> EncodedImmutableIndexArtifactV1 {
  let value = fs::read(fixture_root().join(name)).unwrap();
  let key = decode_index_manifest(&value, algorithm).unwrap().key;
  EncodedImmutableIndexArtifactV1 { key, value }
}

fn active_pointer_write(manifest: &EncodedImmutableIndexArtifactV1, slot: u8, sequence: u64) -> ActivePointerWriteV1<'_> {
  let manifest_view = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
  ActivePointerWriteV1 {
    kind: ActivePointerKindV1::FieldIndex,
    hash_algorithm: ALGORITHM,
    generation: manifest_view.generation,
    owner_id: manifest_view.owner_id,
    slot,
    sequence,
    target_manifest_hash: &manifest.key,
  }
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}
