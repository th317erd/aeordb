//! Physical selection must not mistake host pressure for a corrupt generation.
use std::io::{Read, Seek, SeekFrom, Write};

use super::retained_definition_read_resource_spec::manifest_chain_with_selector_segments;
use super::{ALGORITHM, measure_nth};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationErrorV1, IndexActivePointerPublicationRequestV1, IndexArtifactBatchPublicationRequestV1,
};
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerWriteV1, EncodedImmutableIndexArtifactV1, IndexManifestBodyV1, IndexManifestWriteV1,
  decode_index_manifest, encode_active_pointer, encode_index_manifest,
};
use aeordb::engine::v4::source_selector::JsonPathSegmentV1;
use tokio_util::sync::CancellationToken;

#[path = "../helpers/v4_index_authority.rs"]
pub(super) mod authority_fixture;
use authority_fixture::{DATABASE_ID, create_publisher_for, reopen, retirement_owner_for};

fn next_generation(previous: &[EncodedImmutableIndexArtifactV1; 3]) -> [EncodedImmutableIndexArtifactV1; 3] {
  let scope = decode_index_manifest(&previous[0].value, ALGORITHM).unwrap();
  let scope = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: ALGORITHM,
    generation: scope.generation + 1,
    owner_id: scope.owner_id,
    body: scope.details,
  })
  .unwrap();
  let value = decode_index_manifest(&previous[1].value, ALGORITHM).unwrap();
  let IndexManifestBodyV1::ValueStore(mut body) = value.details else {
    panic!("ValueStore manifest")
  };
  body.scope_catalog_manifest = &scope.key;
  let value = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: ALGORITHM,
    generation: value.generation + 1,
    owner_id: value.owner_id,
    body: IndexManifestBodyV1::ValueStore(body),
  })
  .unwrap();
  let field = decode_index_manifest(&previous[2].value, ALGORITHM).unwrap();
  let IndexManifestBodyV1::FieldIndex(mut body) = field.details else {
    panic!("FieldIndex manifest")
  };
  body.value_store_manifest = &value.key;
  let field = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: ALGORITHM,
    generation: field.generation + 1,
    owner_id: field.owner_id,
    body: IndexManifestBodyV1::FieldIndex(body),
  })
  .unwrap();
  [scope, value, field]
}

#[test]
fn physical_active_pointer_does_not_fall_back_on_real_definition_allocation_failure() {
  assert_selection_failure(AllocationSite::SelectorDecode);
}

#[test]
fn physical_active_pointer_does_not_fall_back_on_manifest_read_allocation_failure() {
  assert_selection_failure(AllocationSite::ManifestRead);
}

#[derive(Clone, Copy)]
enum AllocationSite {
  SelectorDecode,
  ManifestRead,
}

fn assert_selection_failure(site: AllocationSite) {
  let kind = ActivePointerKindV1::FieldIndex;
  let manifest_index = 2;
  let previous = manifest_chain_with_selector_segments(19)
    .map(|value| EncodedImmutableIndexArtifactV1 { key: decode_index_manifest(&value, ALGORITHM).unwrap().key, value });
  let next = next_generation(&previous);
  let (_directory, path, publisher) = create_publisher_for(ALGORITHM);
  publisher
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &DATABASE_ID,
      artifacts: &[&previous[0], &previous[1], &previous[2], &next[0], &next[1], &next[2]],
      publication_timestamp_ms: 1_700_000_000_200,
    })
    .unwrap();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let cancellation = CancellationToken::new();
  let mut retirement = retirement_owner_for(ALGORITHM, &cancellation, &memory);
  for (slot, manifest) in [&previous[manifest_index], &next[manifest_index]].into_iter().enumerate() {
    let view = decode_index_manifest(&manifest.value, ALGORITHM).unwrap();
    let pointer = encode_active_pointer(&ActivePointerWriteV1 {
      kind,
      hash_algorithm: ALGORITHM,
      generation: view.generation,
      owner_id: view.owner_id,
      slot: slot as u8,
      sequence: slot as u64 + 1,
      target_manifest_hash: &manifest.key,
    })
    .unwrap();
    publisher
      .publish_index_active_pointer(
        IndexActivePointerPublicationRequestV1 {
          database_id: &DATABASE_ID,
          pointer: &pointer,
          publication_timestamp_ms: 1_700_000_000_300 + slot as u64,
          monotonic_now_ms: 1_700_000_000_300 + slot as u64,
        },
        &mut retirement,
      )
      .unwrap();
  }
  let view = decode_index_manifest(&next[manifest_index].value, ALGORITHM).unwrap();
  let load = || publisher.load_index_active_pointer_pair(&DATABASE_ID, kind, view.owner_id);
  let selected = load().unwrap().selected.unwrap();
  assert_eq!(selected.pointer_sequence, 2);
  assert_eq!(selected.generation, view.generation);
  assert_eq!(selected.target_manifest_hash, next[manifest_index].key);
  let before = publisher.observe().unwrap();
  let before_bytes = std::fs::read(&path).unwrap();
  let (allocation_size, count, expected_code) = match site {
    AllocationSite::SelectorDecode => (19 * std::mem::size_of::<JsonPathSegmentV1<'_>>(), 4, "selector_decode_allocation"),
    AllocationSite::ManifestRead => {
      (publisher.locator(&next[1].key).unwrap().unwrap().total_length as usize, 2, "immutable_index_read_allocation")
    }
  };
  // Exercise both slots and both the nested ValueStore decode
  // and complete-chain revalidation. None can justify a fallback or repair.
  for occurrence in 1..=count {
    let (result, allocations) = measure_nth(allocation_size, occurrence, load);
    assert!(allocations.injected_failure, "{allocations:?}");
    match result {
      Err(error) => {
        assert_eq!(error.code(), expected_code);
        if matches!(site, AllocationSite::SelectorDecode) {
          assert!(matches!(error, FirstAuthorityPublicationErrorV1::Format(source) if source.is_allocation_failure()));
        }
      }
      other => panic!("host pressure must not select the older generation: {other:?}"),
    }
    assert_eq!(publisher.observe().unwrap(), before);
    assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
    assert_eq!(load().unwrap().selected.unwrap(), selected);
  }
  let value_locator = publisher.locator(&next[1].key).unwrap().unwrap();
  drop(publisher);
  let selected_after_reopen = reopen(&path).load_index_active_pointer_pair(&DATABASE_ID, kind, view.owner_id).unwrap().selected.unwrap();
  assert_eq!(selected_after_reopen, selected);

  // This fixture is disposable. Deliberately damage B's stored checksum after
  // the no-write/retry proofs: real corrupt bytes must still fall back to A.
  let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
  let checksum_offset = value_locator.offset + u64::from(value_locator.total_length) - 1;
  file.seek(SeekFrom::Start(checksum_offset)).unwrap();
  let mut byte = [0];
  file.read_exact(&mut byte).unwrap();
  byte[0] ^= 1;
  file.seek(SeekFrom::Start(checksum_offset)).unwrap();
  file.write_all(&byte).unwrap();
  file.sync_all().unwrap();
  drop(file);
  let reopened = reopen(&path);
  let pair = reopened.load_index_active_pointer_pair(&DATABASE_ID, kind, view.owner_id).unwrap();
  assert_eq!(pair.closure_invalid_slots, [false, true]);
  let selected = pair.selected.unwrap();
  assert_eq!(selected.pointer_sequence, 1);
  assert_eq!(selected.target_manifest_hash, previous[manifest_index].key);
  assert_eq!(reopened.observe().unwrap(), before);
}
