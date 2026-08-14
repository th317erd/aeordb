use std::collections::VecDeque;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::native_durability::{sync_directory_native, sync_file_all_native};
use aeordb::engine::v4::index_copy_on_write::{
  ArtifactDirectoryMutationRequestV1, ArtifactDirectoryPathV1, IndexCopyOnWriteClosureRequestV1, OrderedPageCompactionWindowRequestV1,
  TombstoneDropProofV1, compact_ordered_page_window_v1, default_index_directory_layout_v1, default_index_page_layout_v1,
  rewrite_artifact_directory_paths_v1, validate_index_copy_on_write_closure_v1,
};
use aeordb::engine::v4::index_artifact::{
  FieldIndexManifestBodyV1, FieldNvtManifestBodyV1, IndexManifestBodyV1, IndexManifestV1, IndexManifestWriteV1, ScopeCatalogManifestBodyV1,
  ValueStoreManifestBodyV1, decode_index_manifest, encode_index_manifest, validate_correctness_manifest_chain,
};
use aeordb::engine::v4::index_nvt::{
  ImmutableIndexPathV1, NvtBasisStatusV1, NvtEntryWriteV1, NvtFallbackReasonV1, NvtLookupAttemptV1, NvtLookupRequestV1, NvtLookupSourceV1,
  NvtTileWriteV1, decode_nvt_tile, default_nvt_healing_limits_v1, default_sparse_nvt_lookup_limits_v1, encode_nvt_tile,
  exact_posting_predecessor_v1, pin_field_index_v1, resolve_nvt_lookup_v1, select_nvt_predecessor_hint_v1, validate_field_nvt_basis_v1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryV1, ArtifactDirectoryEntryWriteV1, ArtifactDirectoryNodeV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1,
  OrderedPageV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1, decode_artifact_directory, decode_ordered_page,
  encode_artifact_directory, encode_ordered_page, encode_posting_record,
};
use aeordb::engine::v4::index_task::{
  IndexTaskAttachmentClosureBuilderV1, IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1, IndexTaskCheckpointV1,
  IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, decode_index_task_checkpoint, decode_mutation_journal,
  encode_index_task_checkpoint,
};
use aeordb::engine::v4::reader::MalformedInputClass;
use tempfile::TempDir;

const ROOT_ROLES: [(IndexTaskAttachmentRoleV1, &str); 7] = [
  (IndexTaskAttachmentRoleV1::ScopeOrdinalDirectoryRoot, "scope-ordinal-directory-leaf-valid.bin"),
  (IndexTaskAttachmentRoleV1::ScopeReverseDirectoryRoot, "scope-reverse-directory-leaf-valid.bin"),
  (IndexTaskAttachmentRoleV1::ValueDirectoryRoot, "value-directory-leaf-valid.bin"),
  (IndexTaskAttachmentRoleV1::ValueStateDirectoryRoot, "value-document-state-directory-leaf-valid.bin"),
  (IndexTaskAttachmentRoleV1::PostingDirectoryRoot, "posting-directory-leaf-valid.bin"),
  (IndexTaskAttachmentRoleV1::IndexStateDirectoryRoot, "index-document-state-directory-leaf-valid.bin"),
  (IndexTaskAttachmentRoleV1::NvtTileDirectoryRoot, "nvt-tile-directory-leaf-valid.bin"),
];

const LEAF_ARTIFACTS: [&str; 6] = [
  "scope-ordinal-page-valid.bin",
  "scope-reverse-page-valid.bin",
  "value-page-valid.bin",
  "value-document-state-page-valid.bin",
  "posting-page-valid.bin",
  "index-document-state-page-valid.bin",
];

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn profile_name(hash_algorithm: HashAlgorithm) -> &'static str {
  match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("P5-8 shadow proof uses only frozen v4 hash profiles"),
  }
}

fn fixture_bytes(hash_algorithm: HashAlgorithm, suffix: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("aidx-{}-{suffix}", profile_name(hash_algorithm)))).unwrap()
}

fn sample_posting_record(coordinate: u64, tombstone: bool) -> Vec<u8> {
  encode_posting_record(&PostingRecordV1 {
    tombstone,
    coordinate,
    document_ordinal: coordinate,
    source_value_ordinal: 0,
    expansion_ordinal: 0,
    posting_key: &coordinate.to_le_bytes(),
  })
  .unwrap()
}

fn sample_posting_page(
  hash_algorithm: HashAlgorithm,
  owner_id: &[u8],
  generation: u64,
  page_id: u64,
  previous_page_id: u64,
  next_page_id: u64,
  records: &[Vec<u8>],
) -> Vec<u8> {
  let records = records.iter().map(Vec::as_slice).collect::<Vec<_>>();
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation,
    page_id,
    previous_page_id,
    next_page_id,
    records: &records,
  })
  .unwrap()
  .value
}

fn sample_leaf_directory(hash_algorithm: HashAlgorithm, owner_id: &[u8], generation: u64, pages: &[&[u8]]) -> Vec<u8> {
  let pages = pages.iter().map(|page| decode_ordered_page(page, hash_algorithm).unwrap()).collect::<Vec<_>>();
  let entries = pages
    .iter()
    .map(|page| ArtifactDirectoryEntryWriteV1 {
      lower_fence: page.lower_fence,
      upper_fence: page.upper_fence,
      child_hash: &page.key,
      child_generation: page.generation,
      live_count: u64::from(page.live_count),
      tombstone_count: u64::from(page.tombstone_count),
      page_count: 1,
      logical_bytes: page.logical_live_bytes,
      minimum_page_id: page.page_id,
      maximum_page_id: page.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm,
    role: OrderedIndexRoleV1::Posting,
    owner_id,
    generation,
    level: 0,
    entries: &entries,
  })
  .unwrap()
  .value
}

fn key_name(key: &[u8]) -> String {
  const HEX: &[u8; 16] = b"0123456789abcdef";
  let mut name = String::with_capacity(key.len() * 2);
  for byte in key {
    name.push(char::from(HEX[usize::from(byte >> 4)]));
    name.push(char::from(HEX[usize::from(byte & 0x0f)]));
  }
  name
}

struct FileBackedShadowStore {
  root: PathBuf,
}

impl FileBackedShadowStore {
  fn create(root: &Path) -> Self {
    fs::create_dir_all(root).unwrap();
    Self { root: root.to_path_buf() }
  }

  fn reopen(root: &Path) -> Self {
    assert!(root.is_dir());
    Self { root: root.to_path_buf() }
  }

  fn put(&self, key: &[u8], value: &[u8]) {
    let path = self.root.join(key_name(key));
    fs::write(&path, value).unwrap();
    sync_file_all_native(&File::open(path).unwrap()).unwrap();
  }

  fn put_buffered(&self, key: &[u8], value: &[u8]) {
    fs::write(self.root.join(key_name(key)), value).unwrap();
  }

  fn get(&self, key: &[u8]) -> Vec<u8> {
    fs::read(self.root.join(key_name(key))).unwrap()
  }

  fn remove(&self, key: &[u8]) {
    fs::remove_file(self.root.join(key_name(key))).unwrap();
  }

  fn sync(&self) {
    sync_directory_native(&self.root).unwrap();
  }
}

struct TwoArtifactCache {
  entries: VecDeque<(Vec<u8>, Vec<u8>)>,
  peak_resident_bytes: usize,
}

impl TwoArtifactCache {
  fn new() -> Self {
    Self { entries: VecDeque::new(), peak_resident_bytes: 0 }
  }

  fn read_page(&mut self, store: &FileBackedShadowStore, key: &[u8], hash_algorithm: HashAlgorithm) -> (u64, usize) {
    let value = if let Some(index) = self.entries.iter().position(|(cached_key, _)| cached_key == key) {
      self.entries.remove(index).unwrap().1
    } else {
      store.get(key)
    };
    let page = decode_ordered_page(&value, hash_algorithm).unwrap();
    let result = (page.page_id, value.len());
    self.entries.push_front((key.to_vec(), value));
    while self.entries.len() > 2 {
      self.entries.pop_back();
    }
    let resident_bytes = self.entries.iter().map(|(_, value)| value.len()).sum();
    self.peak_resident_bytes = self.peak_resident_bytes.max(resident_bytes);
    result
  }
}

#[derive(Debug)]
struct OwnedAttachment {
  role: IndexTaskAttachmentRoleV1,
  owner_id: Vec<u8>,
  artifact_hash: Vec<u8>,
  birth_generation: u64,
}

#[derive(Debug)]
struct RootSummary {
  role: IndexTaskAttachmentRoleV1,
  key: Vec<u8>,
  owner_id: Vec<u8>,
  generation: u64,
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
  logical_bytes: u64,
  minimum_page_id: u64,
  maximum_page_id: u64,
}

fn root_summary(roots: &[RootSummary], role: IndexTaskAttachmentRoleV1) -> &RootSummary {
  roots.iter().find(|root| root.role == role).unwrap()
}

impl OwnedAttachment {
  fn as_write(&self) -> IndexTaskAttachmentWriteV1<'_> {
    IndexTaskAttachmentWriteV1 {
      role: self.role,
      owner_id: &self.owner_id,
      artifact_hash: &self.artifact_hash,
      birth_generation: self.birth_generation,
    }
  }
}

struct StoredShadowGeneration {
  _temporary: TempDir,
  root: PathBuf,
  checkpoint_key: Vec<u8>,
}

fn persist_fixture_generation(hash_algorithm: HashAlgorithm) -> StoredShadowGeneration {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().join("immutable");
  let store = FileBackedShadowStore::create(&root);
  let mut attachments = Vec::new();
  let mut roots = Vec::new();
  let field_seed_bytes = fixture_bytes(hash_algorithm, "field-index-manifest-populated.bin");
  let field_seed = decode_index_manifest(&field_seed_bytes, hash_algorithm).unwrap();
  let field_owner = field_seed.owner_id.to_vec();
  let posting_seed = fixture_bytes(hash_algorithm, "posting-directory-leaf-valid.bin");
  let state_seed = fixture_bytes(hash_algorithm, "index-document-state-directory-leaf-valid.bin");
  let field_generation = field_seed
    .generation
    .max(decode_artifact_directory(&posting_seed, hash_algorithm).unwrap().generation)
    .max(decode_artifact_directory(&state_seed, hash_algorithm).unwrap().generation);

  for (role, suffix) in ROOT_ROLES {
    let bytes = if role == IndexTaskAttachmentRoleV1::NvtTileDirectoryRoot {
      let posting_owner = &root_summary(&roots, IndexTaskAttachmentRoleV1::PostingDirectoryRoot).owner_id;
      assert_eq!(posting_owner, &field_owner);
      let tile_fixture_bytes = fixture_bytes(hash_algorithm, "nvt-tile-valid.bin");
      let tile_fixture = decode_nvt_tile(&tile_fixture_bytes, hash_algorithm).unwrap();
      let tile_entries = tile_fixture
        .entries
        .iter()
        .map(|entry| {
          let entry = entry.unwrap();
          NvtEntryWriteV1 {
            relative_cell: entry.relative_cell,
            predecessor_page_id: entry.predecessor_page_id,
            successor_page_id: entry.successor_page_id,
            approximate_live_postings: entry.approximate_live_postings,
            sample_coordinate: entry.sample_coordinate,
          }
        })
        .collect::<Vec<_>>();
      let tile = encode_nvt_tile(&NvtTileWriteV1 {
        hash_algorithm,
        owner_id: &field_owner,
        generation: tile_fixture.generation,
        resolution: tile_fixture.resolution,
        tile_start_cell: tile_fixture.tile_start_cell,
        tile_cell_count: tile_fixture.tile_cell_count,
        basis_posting_generation: field_generation,
        entries: &tile_entries,
      })
      .unwrap();
      let directory_fixture_bytes = fixture_bytes(hash_algorithm, suffix);
      let directory_fixture = decode_artifact_directory(&directory_fixture_bytes, hash_algorithm).unwrap();
      let fence = tile_fixture.tile_start_cell.to_le_bytes();
      let entries = [ArtifactDirectoryEntryWriteV1 {
        lower_fence: &fence,
        upper_fence: &fence,
        child_hash: &tile.key,
        child_generation: tile_fixture.generation,
        live_count: u64::try_from(tile_entries.len()).unwrap(),
        tombstone_count: 0,
        page_count: 1,
        logical_bytes: u64::try_from(tile.value.len()).unwrap(),
        minimum_page_id: 0,
        maximum_page_id: 0,
        physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
      }];
      let directory = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
        hash_algorithm,
        role: OrderedIndexRoleV1::NvtTile,
        owner_id: &field_owner,
        generation: directory_fixture.generation,
        level: 0,
        entries: &entries,
      })
      .unwrap();
      store.put(&tile.key, &tile.value);
      directory.value
    } else {
      fixture_bytes(hash_algorithm, suffix)
    };
    let directory = decode_artifact_directory(&bytes, hash_algorithm).unwrap();
    roots.push(RootSummary {
      role,
      key: directory.key.clone(),
      owner_id: directory.owner_id.to_vec(),
      generation: directory.generation,
      live_count: directory.live_count,
      tombstone_count: directory.tombstone_count,
      page_count: directory.page_count,
      logical_bytes: directory.logical_bytes,
      minimum_page_id: directory.minimum_page_id,
      maximum_page_id: directory.maximum_page_id,
    });
    attachments.push(OwnedAttachment {
      role,
      owner_id: directory.owner_id.to_vec(),
      artifact_hash: directory.key.clone(),
      birth_generation: directory.generation,
    });
    store.put(&directory.key, &bytes);
  }

  for suffix in LEAF_ARTIFACTS {
    let bytes = fixture_bytes(hash_algorithm, suffix);
    let key = decode_ordered_page(&bytes, hash_algorithm).unwrap().key;
    store.put(&key, &bytes);
  }

  let ordinal_root = root_summary(&roots, IndexTaskAttachmentRoleV1::ScopeOrdinalDirectoryRoot);
  let reverse_root = root_summary(&roots, IndexTaskAttachmentRoleV1::ScopeReverseDirectoryRoot);
  assert_eq!(ordinal_root.owner_id, reverse_root.owner_id);
  assert_eq!(ordinal_root.live_count, reverse_root.live_count);
  let scope_fixture_bytes = fixture_bytes(hash_algorithm, "scope-catalog-manifest-populated.bin");
  let scope_fixture = decode_index_manifest(&scope_fixture_bytes, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ScopeCatalog(scope_fixture_body) = scope_fixture.details else {
    panic!("populated ScopeCatalog fixture decoded as another manifest kind");
  };
  assert_eq!(scope_fixture.owner_id, ordinal_root.owner_id);
  let scope = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: scope_fixture.generation.max(ordinal_root.generation).max(reverse_root.generation),
    owner_id: scope_fixture.owner_id,
    body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 {
      ordinal_directory_root: Some(&ordinal_root.key),
      reverse_directory_root: Some(&reverse_root.key),
      live_document_count: ordinal_root.live_count,
      retained_tombstone_count: ordinal_root.tombstone_count,
      ordinal_page_count: ordinal_root.page_count,
      reverse_page_count: reverse_root.page_count,
      ..scope_fixture_body
    }),
  })
  .unwrap();

  let value_root = root_summary(&roots, IndexTaskAttachmentRoleV1::ValueDirectoryRoot);
  let value_state_root = root_summary(&roots, IndexTaskAttachmentRoleV1::ValueStateDirectoryRoot);
  assert_eq!(value_root.owner_id, value_state_root.owner_id);
  let value_fixture_bytes = fixture_bytes(hash_algorithm, "value-store-manifest-populated.bin");
  let value_fixture = decode_index_manifest(&value_fixture_bytes, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ValueStore(value_fixture_body) = value_fixture.details else {
    panic!("populated ValueStore fixture decoded as another manifest kind");
  };
  assert_eq!(value_fixture.owner_id, value_root.owner_id);
  let value = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: value_fixture.generation.max(value_root.generation).max(value_state_root.generation),
    owner_id: value_fixture.owner_id,
    body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
      scope_catalog_manifest: &scope.key,
      value_directory_root: Some(&value_root.key),
      document_state_directory_root: Some(&value_state_root.key),
      value_page_count: value_root.page_count,
      state_page_count: value_state_root.page_count,
      unindexable_document_count: value_state_root.live_count,
      live_value_count: value_root.live_count,
      value_tombstone_count: value_root.tombstone_count,
      state_tombstone_count: value_state_root.tombstone_count,
      live_canonical_value_bytes: value_root.logical_bytes,
      ..value_fixture_body
    }),
  })
  .unwrap();

  let posting_root = root_summary(&roots, IndexTaskAttachmentRoleV1::PostingDirectoryRoot);
  let index_state_root = root_summary(&roots, IndexTaskAttachmentRoleV1::IndexStateDirectoryRoot);
  assert_eq!(posting_root.owner_id, index_state_root.owner_id);
  let field_fixture_bytes = fixture_bytes(hash_algorithm, "field-index-manifest-populated.bin");
  let field_fixture = decode_index_manifest(&field_fixture_bytes, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldIndex(field_fixture_body) = field_fixture.details else {
    panic!("populated FieldIndex fixture decoded as another manifest kind");
  };
  let field_source_head = field_fixture_body.coverage.source_head_hash.to_vec();
  assert_eq!(field_fixture.owner_id, posting_root.owner_id);
  let field = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: field_generation,
    owner_id: field_fixture.owner_id,
    body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 {
      value_store_manifest: &value.key,
      posting_directory_root: Some(&posting_root.key),
      document_state_directory_root: Some(&index_state_root.key),
      first_page_id: posting_root.minimum_page_id,
      last_page_id: posting_root.maximum_page_id,
      next_page_id: field_fixture_body.next_page_id.max(posting_root.maximum_page_id + 1),
      posting_page_count: posting_root.page_count,
      state_page_count: index_state_root.page_count,
      live_posting_count: posting_root.live_count,
      posting_tombstone_count: posting_root.tombstone_count,
      unindexable_document_count: index_state_root.live_count,
      state_tombstone_count: index_state_root.tombstone_count,
      live_canonical_posting_bytes: posting_root.logical_bytes,
      ..field_fixture_body
    }),
  })
  .unwrap();

  let nvt_root = root_summary(&roots, IndexTaskAttachmentRoleV1::NvtTileDirectoryRoot);
  assert_eq!(field_fixture.owner_id, nvt_root.owner_id);
  let nvt_fixture_bytes = fixture_bytes(hash_algorithm, "field-nvt-manifest-populated.bin");
  let nvt_fixture = decode_index_manifest(&nvt_fixture_bytes, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldNvt(nvt_fixture_body) = nvt_fixture.details else {
    panic!("populated FieldNvt fixture decoded as another manifest kind");
  };
  let nvt = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: nvt_fixture.generation.max(nvt_root.generation),
    owner_id: field_fixture.owner_id,
    body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
      basis_posting_generation: field_generation,
      basis_source_head_hash: &field_source_head,
      tile_directory_root: Some(&nvt_root.key),
      tile_count: nvt_root.page_count,
      populated_cell_count: nvt_root.live_count,
      ..nvt_fixture_body
    }),
  })
  .unwrap();

  for (role, bytes) in [
    (IndexTaskAttachmentRoleV1::CandidateScopeManifest, scope.value),
    (IndexTaskAttachmentRoleV1::CandidateValueManifest, value.value),
    (IndexTaskAttachmentRoleV1::CandidateFieldManifest, field.value),
    (IndexTaskAttachmentRoleV1::CandidateNvtManifest, nvt.value),
  ] {
    let manifest = decode_index_manifest(&bytes, hash_algorithm).unwrap();
    attachments.push(OwnedAttachment {
      role,
      owner_id: manifest.owner_id.to_vec(),
      artifact_hash: manifest.key.clone(),
      birth_generation: manifest.generation,
    });
    store.put(&manifest.key, &bytes);
  }

  let journal_bytes = fixture_bytes(hash_algorithm, "task-mutation-journal-valid.bin");
  let journal = decode_mutation_journal(&journal_bytes, hash_algorithm).unwrap();
  attachments.push(OwnedAttachment {
    role: IndexTaskAttachmentRoleV1::MutationJournalHead,
    owner_id: vec![0x91; hash_algorithm.hash_length()],
    artifact_hash: journal.key.clone(),
    birth_generation: journal.generation,
  });
  store.put(&journal.key, &journal_bytes);

  attachments.sort_by_key(|attachment| attachment.role);
  let attachment_writes = attachments.iter().map(OwnedAttachment::as_write).collect::<Vec<_>>();
  let field_manifest = attachments.iter().find(|attachment| attachment.role == IndexTaskAttachmentRoleV1::CandidateFieldManifest).unwrap();
  let checkpoint = encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
    hash_algorithm,
    task_id: journal.owner_id,
    checkpoint_sequence: 1,
    generation: 41,
    task_kind: IndexTaskKindV1::ScopeBuild,
    state: IndexTaskStateV1::CompleteUnpublished,
    phase: 6,
    required_capabilities: &[0; 32],
    started_at_ms: 1_800_000_000_000,
    updated_at_ms: 1_800_000_000_001,
    source_root: &field_source_head,
    target_root: Some(&field_manifest.artifact_hash),
    primary_id: Some(&field_manifest.owner_id),
    journal_head: Some(&journal.key),
    journal_floor_sequence: journal.first_sequence,
    journal_audited_through: journal.last_sequence,
    next_document_ordinal: 3,
    completed_work: 7,
    total_work_hint: 7,
    resume_key: b"/docs/guide.md",
    attachments: &attachment_writes,
    external: None,
  })
  .unwrap();
  store.put(&checkpoint.key, &checkpoint.value);
  store.sync();
  drop(store);

  StoredShadowGeneration { _temporary: temporary, root, checkpoint_key: checkpoint.key }
}

#[derive(Debug, Clone)]
struct RootExpectation {
  role: OrderedIndexRoleV1,
  owner_id: Vec<u8>,
  maximum_generation: u64,
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
  logical_bytes: Option<u64>,
  minimum_page_id: Option<u64>,
  maximum_page_id: Option<u64>,
}

fn assert_directory_matches_expectation(directory: &ArtifactDirectoryNodeV1<'_>, expected: &RootExpectation) {
  assert_eq!(directory.role, expected.role);
  assert_eq!(directory.owner_id, expected.owner_id);
  assert!(directory.generation <= expected.maximum_generation);
  assert_eq!(directory.live_count, expected.live_count);
  assert_eq!(directory.tombstone_count, expected.tombstone_count);
  assert_eq!(directory.page_count, expected.page_count);
  if let Some(logical_bytes) = expected.logical_bytes {
    assert_eq!(directory.logical_bytes, logical_bytes);
  }
  if let Some(page_id) = expected.minimum_page_id {
    assert_eq!(directory.minimum_page_id, page_id);
  }
  if let Some(page_id) = expected.maximum_page_id {
    assert_eq!(directory.maximum_page_id, page_id);
  }
}

fn assert_descriptor_matches_directory(entry: &ArtifactDirectoryEntryV1<'_>, child: &ArtifactDirectoryNodeV1<'_>) {
  assert_eq!(entry.child_hash, child.key);
  assert_eq!(entry.child_generation, child.generation);
  assert_eq!(entry.lower_fence, child.lower_fence);
  assert_eq!(entry.upper_fence, child.upper_fence);
  assert_eq!(entry.live_count, child.live_count);
  assert_eq!(entry.tombstone_count, child.tombstone_count);
  assert_eq!(entry.page_count, child.page_count);
  assert_eq!(entry.logical_bytes, child.logical_bytes);
  assert_eq!(entry.minimum_page_id, child.minimum_page_id);
  assert_eq!(entry.maximum_page_id, child.maximum_page_id);
}

fn assert_descriptor_matches_page(entry: &ArtifactDirectoryEntryV1<'_>, page: &OrderedPageV1<'_>, expected: &RootExpectation) {
  assert_eq!(page.role, expected.role);
  assert_eq!(page.owner_id, expected.owner_id);
  assert!(page.generation <= expected.maximum_generation);
  assert_eq!(entry.child_hash, page.key);
  assert_eq!(entry.child_generation, page.generation);
  assert_eq!(entry.lower_fence, page.lower_fence);
  assert_eq!(entry.upper_fence, page.upper_fence);
  assert_eq!(entry.live_count, u64::from(page.live_count));
  assert_eq!(entry.tombstone_count, u64::from(page.tombstone_count));
  assert_eq!(entry.page_count, 1);
  assert_eq!(entry.logical_bytes, page.logical_live_bytes);
  assert_eq!(entry.minimum_page_id, page.page_id);
  assert_eq!(entry.maximum_page_id, page.page_id);
  assert_eq!(page.records.iter().collect::<Result<Vec<_>, _>>().unwrap().len(), page.records.len());
}

fn verify_directory_tree(store: &FileBackedShadowStore, root_key: &[u8], expected: &RootExpectation) -> usize {
  fn visit(store: &FileBackedShadowStore, key: &[u8], expected: &RootExpectation, depth: usize, visited: &mut usize) {
    assert!(depth <= 16, "directory depth exceeded the frozen cap");
    *visited += 1;
    assert!(*visited <= 65_536, "directory traversal exceeded the frozen artifact cap");
    let bytes = store.get(key);
    let directory = decode_artifact_directory(&bytes, hash_algorithm_for_width(expected.owner_id.len())).unwrap();
    if depth == 0 {
      assert_directory_matches_expectation(&directory, expected);
    } else {
      assert_eq!(directory.role, expected.role);
      assert_eq!(directory.owner_id, expected.owner_id);
      assert!(directory.generation <= expected.maximum_generation);
    }
    for entry in &directory.entries {
      let child_bytes = store.get(entry.child_hash);
      if directory.level != 0 {
        let child = decode_artifact_directory(&child_bytes, hash_algorithm_for_width(expected.owner_id.len())).unwrap();
        assert_descriptor_matches_directory(entry, &child);
        visit(store, entry.child_hash, expected, depth + 1, visited);
      } else if expected.role == OrderedIndexRoleV1::NvtTile {
        let tile = decode_nvt_tile(&child_bytes, hash_algorithm_for_width(expected.owner_id.len())).unwrap();
        assert_eq!(tile.owner_id, expected.owner_id);
        assert!(tile.generation <= expected.maximum_generation);
        assert_eq!(entry.child_hash, tile.key);
        assert_eq!(entry.child_generation, tile.generation);
        assert_eq!(entry.lower_fence, tile.tile_start_cell.to_le_bytes());
        assert_eq!(entry.upper_fence, tile.tile_start_cell.to_le_bytes());
        assert_eq!(entry.live_count, u64::try_from(tile.entries.len()).unwrap());
        assert_eq!(entry.tombstone_count, 0);
        assert_eq!(entry.page_count, 1);
        assert_eq!(entry.minimum_page_id, 0);
        assert_eq!(entry.maximum_page_id, 0);
      } else {
        let page = decode_ordered_page(&child_bytes, hash_algorithm_for_width(expected.owner_id.len())).unwrap();
        assert_descriptor_matches_page(entry, &page, expected);
      }
    }
  }

  let mut visited = 0;
  visit(store, root_key, expected, 0, &mut visited);
  visited
}

fn hash_algorithm_for_width(width: usize) -> HashAlgorithm {
  match width {
    32 => HashAlgorithm::Blake3_256,
    64 => HashAlgorithm::Sha512,
    _ => panic!("unexpected v4 hash width {width}"),
  }
}

fn verify_manifest_roots(
  store: &FileBackedShadowStore,
  scope: &IndexManifestV1<'_>,
  value: &IndexManifestV1<'_>,
  field: &IndexManifestV1<'_>,
  nvt: &IndexManifestV1<'_>,
) -> usize {
  let IndexManifestBodyV1::ScopeCatalog(scope_body) = &scope.details else {
    panic!("expected ScopeCatalog manifest");
  };
  let IndexManifestBodyV1::ValueStore(value_body) = &value.details else {
    panic!("expected ValueStore manifest");
  };
  let IndexManifestBodyV1::FieldIndex(field_body) = &field.details else {
    panic!("expected FieldIndex manifest");
  };
  let IndexManifestBodyV1::FieldNvt(nvt_body) = &nvt.details else {
    panic!("expected FieldNvt manifest");
  };
  let roots = [
    (
      scope_body.ordinal_directory_root.unwrap(),
      RootExpectation {
        role: OrderedIndexRoleV1::ScopeOrdinal,
        owner_id: scope.owner_id.to_vec(),
        maximum_generation: scope.generation,
        live_count: scope_body.live_document_count,
        tombstone_count: scope_body.retained_tombstone_count,
        page_count: scope_body.ordinal_page_count,
        logical_bytes: None,
        minimum_page_id: None,
        maximum_page_id: None,
      },
    ),
    (
      scope_body.reverse_directory_root.unwrap(),
      RootExpectation {
        role: OrderedIndexRoleV1::ScopeReverse,
        owner_id: scope.owner_id.to_vec(),
        maximum_generation: scope.generation,
        live_count: scope_body.live_document_count,
        tombstone_count: 0,
        page_count: scope_body.reverse_page_count,
        logical_bytes: None,
        minimum_page_id: None,
        maximum_page_id: None,
      },
    ),
    (
      value_body.value_directory_root.unwrap(),
      RootExpectation {
        role: OrderedIndexRoleV1::Value,
        owner_id: value.owner_id.to_vec(),
        maximum_generation: value.generation,
        live_count: value_body.live_value_count,
        tombstone_count: value_body.value_tombstone_count,
        page_count: value_body.value_page_count,
        logical_bytes: Some(value_body.live_canonical_value_bytes),
        minimum_page_id: None,
        maximum_page_id: None,
      },
    ),
    (
      value_body.document_state_directory_root.unwrap(),
      RootExpectation {
        role: OrderedIndexRoleV1::ValueDocumentState,
        owner_id: value.owner_id.to_vec(),
        maximum_generation: value.generation,
        live_count: value_body.unindexable_document_count,
        tombstone_count: value_body.state_tombstone_count,
        page_count: value_body.state_page_count,
        logical_bytes: None,
        minimum_page_id: None,
        maximum_page_id: None,
      },
    ),
    (
      field_body.posting_directory_root.unwrap(),
      RootExpectation {
        role: OrderedIndexRoleV1::Posting,
        owner_id: field.owner_id.to_vec(),
        maximum_generation: field.generation,
        live_count: field_body.live_posting_count,
        tombstone_count: field_body.posting_tombstone_count,
        page_count: field_body.posting_page_count,
        logical_bytes: Some(field_body.live_canonical_posting_bytes),
        minimum_page_id: Some(field_body.first_page_id),
        maximum_page_id: Some(field_body.last_page_id),
      },
    ),
    (
      field_body.document_state_directory_root.unwrap(),
      RootExpectation {
        role: OrderedIndexRoleV1::IndexDocumentState,
        owner_id: field.owner_id.to_vec(),
        maximum_generation: field.generation,
        live_count: field_body.unindexable_document_count,
        tombstone_count: field_body.state_tombstone_count,
        page_count: field_body.state_page_count,
        logical_bytes: None,
        minimum_page_id: None,
        maximum_page_id: None,
      },
    ),
    (
      nvt_body.tile_directory_root.unwrap(),
      RootExpectation {
        role: OrderedIndexRoleV1::NvtTile,
        owner_id: nvt.owner_id.to_vec(),
        maximum_generation: nvt.generation,
        live_count: nvt_body.populated_cell_count,
        tombstone_count: 0,
        page_count: nvt_body.tile_count,
        logical_bytes: None,
        minimum_page_id: Some(0),
        maximum_page_id: Some(0),
      },
    ),
  ];
  roots.iter().map(|(key, expected)| verify_directory_tree(store, key, expected)).sum()
}

fn attached_key(checkpoint: &IndexTaskCheckpointV1<'_>, role: IndexTaskAttachmentRoleV1) -> Vec<u8> {
  checkpoint.attachments.iter().map(Result::unwrap).find(|attachment| attachment.role == role).unwrap().artifact_hash.to_vec()
}

#[test]
fn complete_shadow_generation_closes_after_file_backed_restart_at_both_hash_widths() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let generation = persist_fixture_generation(hash_algorithm);
    let store = FileBackedShadowStore::reopen(&generation.root);
    let checkpoint_bytes = store.get(&generation.checkpoint_key);
    let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();

    let mut closure = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
    for attachment in checkpoint.attachments.iter() {
      let attachment = attachment.unwrap();
      closure.observe_encoded(&store.get(attachment.artifact_hash)).unwrap();
    }
    let closure = closure.finish().unwrap();
    assert_eq!(closure.checkpoint_hash(), checkpoint.key);
    assert_eq!(closure.rooted_artifact_count(), 12);
    assert!(closure.journal_head_validated());

    let scope_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::CandidateScopeManifest));
    let value_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::CandidateValueManifest));
    let field_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::CandidateFieldManifest));
    let nvt_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::CandidateNvtManifest));
    let scope = decode_index_manifest(&scope_bytes, hash_algorithm).unwrap();
    let value = decode_index_manifest(&value_bytes, hash_algorithm).unwrap();
    let field = decode_index_manifest(&field_bytes, hash_algorithm).unwrap();
    let nvt = decode_index_manifest(&nvt_bytes, hash_algorithm).unwrap();
    validate_correctness_manifest_chain(&scope, &value, &field, hash_algorithm).unwrap();
    let pinned_field = pin_field_index_v1(&field_bytes, hash_algorithm).unwrap();
    let nvt_basis = validate_field_nvt_basis_v1(&pinned_field, Some(&nvt_bytes));
    assert!(matches!(nvt_basis, NvtBasisStatusV1::Usable(_)), "connected NVT basis did not close: {nvt_basis:?}");
    assert_eq!(verify_manifest_roots(&store, &scope, &value, &field, &nvt), 7);
    assert_eq!(fs::read_dir(&generation.root).unwrap().count(), 20);
  }
}

#[test]
fn missing_unpublished_artifact_fails_checkpoint_closure_after_restart() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let generation = persist_fixture_generation(hash_algorithm);
  let store = FileBackedShadowStore::reopen(&generation.root);
  let checkpoint_bytes = store.get(&generation.checkpoint_key);
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();
  let missing = checkpoint.attachments.entry_at(4).unwrap().artifact_hash.to_vec();
  store.remove(&missing);

  let mut closure = IndexTaskAttachmentClosureBuilderV1::new(&checkpoint, hash_algorithm).unwrap();
  for attachment in checkpoint.attachments.iter().take(4) {
    let attachment = attachment.unwrap();
    closure.observe_encoded(&store.get(attachment.artifact_hash)).unwrap();
  }
  let after_gap = checkpoint.attachments.entry_at(5).unwrap();
  assert!(closure.observe_encoded(&store.get(after_gap.artifact_hash)).is_err());
  assert!(closure.finish().is_err());
  assert!(!generation.root.join(key_name(&missing)).exists());
}

#[test]
fn disposable_nvt_failures_preserve_exact_results_but_authoritative_corruption_fails_closed() {
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let generation = persist_fixture_generation(hash_algorithm);
  let store = FileBackedShadowStore::reopen(&generation.root);
  let checkpoint_bytes = store.get(&generation.checkpoint_key);
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, hash_algorithm).unwrap();
  let field_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::CandidateFieldManifest));
  let nvt_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::CandidateNvtManifest));
  let posting_directory_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::PostingDirectoryRoot));
  let posting_directory = decode_artifact_directory(&posting_directory_bytes, hash_algorithm).unwrap();
  let posting_page_bytes = store.get(posting_directory.entries[0].child_hash);
  let posting_page = decode_ordered_page(&posting_page_bytes, hash_algorithm).unwrap();
  let posting_directories = [posting_directory_bytes.as_slice()];
  let exact_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &posting_page_bytes };
  let field = pin_field_index_v1(&field_bytes, hash_algorithm).unwrap();
  let exact = exact_posting_predecessor_v1(&field, posting_page.upper_fence, Some(&exact_path), default_sparse_nvt_lookup_limits_v1())
    .unwrap()
    .unwrap();
  assert_eq!(exact.page_id, posting_page.page_id);

  let NvtBasisStatusV1::Unavailable(absent) = validate_field_nvt_basis_v1(&field, None) else {
    panic!("absent NVT unexpectedly became authoritative");
  };
  assert_eq!(absent.reason, NvtFallbackReasonV1::Absent);
  let absent_resolution = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: posting_page.maximum_coordinate,
    target_posting_position: posting_page.upper_fence,
    attempt: NvtLookupAttemptV1::Fallback { basis: None, cause: &absent },
    exact_posting_path: Some(&exact_path),
    lookup_limits: default_sparse_nvt_lookup_limits_v1(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  assert_eq!(absent_resolution.source, NvtLookupSourceV1::ExactFallback);
  assert_eq!(absent_resolution.anchor.unwrap(), exact);

  let mut corrupt_manifest = nvt_bytes.clone();
  let corrupt_offset = corrupt_manifest.len() / 2;
  corrupt_manifest[corrupt_offset] ^= 0x80;
  let NvtBasisStatusV1::Unavailable(corrupt) = validate_field_nvt_basis_v1(&field, Some(&corrupt_manifest)) else {
    panic!("corrupt NVT manifest unexpectedly became authoritative");
  };
  assert_eq!(corrupt.reason, NvtFallbackReasonV1::Corrupt);
  assert_eq!(corrupt.diagnostic.as_ref().unwrap().class(), MalformedInputClass::ChecksumOrIntegrityMismatch);
  let corrupt_resolution = resolve_nvt_lookup_v1(&NvtLookupRequestV1 {
    field: &field,
    target_coordinate: posting_page.maximum_coordinate,
    target_posting_position: posting_page.upper_fence,
    attempt: NvtLookupAttemptV1::Fallback { basis: None, cause: &corrupt },
    exact_posting_path: Some(&exact_path),
    lookup_limits: default_sparse_nvt_lookup_limits_v1(),
    healing_limits: default_nvt_healing_limits_v1(),
  })
  .unwrap();
  assert_eq!(corrupt_resolution.anchor.unwrap(), exact);

  let nvt = decode_index_manifest(&nvt_bytes, hash_algorithm).unwrap();
  let IndexManifestBodyV1::FieldNvt(nvt_body) = &nvt.details else {
    panic!("checkpoint NVT attachment decoded as another manifest kind");
  };
  let stale = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: nvt.generation,
    owner_id: nvt.owner_id,
    body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
      basis_posting_generation: nvt_body.basis_posting_generation + 1,
      ..nvt_body.clone()
    }),
  })
  .unwrap();
  let NvtBasisStatusV1::Unavailable(stale) = validate_field_nvt_basis_v1(&field, Some(&stale.value)) else {
    panic!("stale NVT unexpectedly became authoritative");
  };
  assert_eq!(stale.reason, NvtFallbackReasonV1::StalePostingGeneration);
  assert_eq!(
    exact_posting_predecessor_v1(&field, posting_page.upper_fence, Some(&exact_path), default_sparse_nvt_lookup_limits_v1(),)
      .unwrap()
      .unwrap(),
    exact
  );

  let NvtBasisStatusV1::Usable(basis) = validate_field_nvt_basis_v1(&field, Some(&nvt_bytes)) else {
    panic!("connected NVT basis must be usable before tile corruption");
  };
  let nvt_directory_bytes = store.get(&attached_key(&checkpoint, IndexTaskAttachmentRoleV1::NvtTileDirectoryRoot));
  let nvt_directory = decode_artifact_directory(&nvt_directory_bytes, hash_algorithm).unwrap();
  let mut corrupt_tile = store.get(nvt_directory.entries[0].child_hash);
  let corrupt_offset = corrupt_tile.len() / 2;
  corrupt_tile[corrupt_offset] ^= 0x40;
  let nvt_directories = [nvt_directory_bytes.as_slice()];
  let candidates = [ImmutableIndexPathV1 { directories: &nvt_directories, leaf: &corrupt_tile }];
  let tile_selection =
    select_nvt_predecessor_hint_v1(&basis, posting_page.maximum_coordinate, &candidates, default_sparse_nvt_lookup_limits_v1()).unwrap();
  assert!(tile_selection.hint.is_none());
  assert_eq!(tile_selection.fallback.unwrap().reason, NvtFallbackReasonV1::Corrupt);
  assert_eq!(
    exact_posting_predecessor_v1(&field, posting_page.upper_fence, Some(&exact_path), default_sparse_nvt_lookup_limits_v1(),)
      .unwrap()
      .unwrap(),
    exact
  );

  let mut corrupt_directory = posting_directory_bytes.clone();
  let corrupt_offset = corrupt_directory.len() / 2;
  corrupt_directory[corrupt_offset] ^= 0x20;
  let corrupt_directories = [corrupt_directory.as_slice()];
  let corrupt_path = ImmutableIndexPathV1 { directories: &corrupt_directories, leaf: &posting_page_bytes };
  assert_eq!(
    exact_posting_predecessor_v1(&field, posting_page.upper_fence, Some(&corrupt_path), default_sparse_nvt_lookup_limits_v1(),)
      .unwrap_err()
      .class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );

  let mut corrupt_page = posting_page_bytes.clone();
  let corrupt_offset = corrupt_page.len() / 2;
  corrupt_page[corrupt_offset] ^= 0x10;
  let corrupt_path = ImmutableIndexPathV1 { directories: &posting_directories, leaf: &corrupt_page };
  assert_eq!(
    exact_posting_predecessor_v1(&field, posting_page.upper_fence, Some(&corrupt_path), default_sparse_nvt_lookup_limits_v1(),)
      .unwrap_err()
      .class(),
    MalformedInputClass::ChecksumOrIntegrityMismatch
  );
}

#[test]
fn large_inactive_page_set_reopens_with_constant_artifact_cache_residency() {
  const PAGE_COUNT: usize = 256;
  const POSTING_KEY_BYTES: usize = 48 * 1_024;
  let hash_algorithm = HashAlgorithm::Blake3_256;
  let owner_id = vec![0x51; hash_algorithm.hash_length()];
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().join("large-shadow");
  let store = FileBackedShadowStore::create(&root);
  let mut keys = Vec::with_capacity(PAGE_COUNT);
  let mut total_artifact_bytes = 0usize;
  let mut maximum_artifact_bytes = 0usize;

  for index in 0..PAGE_COUNT {
    let page_id = u64::try_from(index + 1).unwrap();
    let mut posting_key = vec![b'k'; POSTING_KEY_BYTES];
    posting_key[..8].copy_from_slice(&page_id.to_le_bytes());
    let record = encode_posting_record(&PostingRecordV1 {
      tombstone: false,
      coordinate: page_id,
      document_ordinal: page_id,
      source_value_ordinal: 0,
      expansion_ordinal: 0,
      posting_key: &posting_key,
    })
    .unwrap();
    let records = [record.as_slice()];
    let page = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::Posting,
      owner_id: &owner_id,
      generation: 1,
      page_id,
      previous_page_id: page_id.saturating_sub(1),
      next_page_id: if index + 1 == PAGE_COUNT { 0 } else { page_id + 1 },
      records: &records,
    })
    .unwrap();
    total_artifact_bytes += page.value.len();
    maximum_artifact_bytes = maximum_artifact_bytes.max(page.value.len());
    store.put_buffered(&page.key, &page.value);
    keys.push(page.key);
  }
  assert!(total_artifact_bytes > 12 * 1_024 * 1_024);
  drop(store);

  let store = FileBackedShadowStore::reopen(&root);
  let mut cache = TwoArtifactCache::new();
  for (index, key) in keys.iter().enumerate() {
    let (page_id, bytes) = cache.read_page(&store, key, hash_algorithm);
    assert_eq!(page_id, u64::try_from(index + 1).unwrap());
    assert_eq!(bytes, maximum_artifact_bytes);
  }
  for (index, key) in keys.iter().enumerate().rev() {
    let (page_id, _) = cache.read_page(&store, key, hash_algorithm);
    assert_eq!(page_id, u64::try_from(index + 1).unwrap());
  }

  assert!(cache.peak_resident_bytes <= 2 * maximum_artifact_bytes);
  assert!(cache.peak_resident_bytes * 64 < total_artifact_bytes);
  assert!(keys.capacity() * hash_algorithm.hash_length() * 64 < total_artifact_bytes);
  assert_eq!(fs::read_dir(root).unwrap().count(), PAGE_COUNT);
}

#[test]
fn bounded_compaction_closure_reopens_without_mutating_or_retaining_the_source_generation() {
  let hash_algorithm = HashAlgorithm::Sha512;
  let owner_id = vec![0x61; hash_algorithm.hash_length()];
  let previous_page = sample_posting_page(hash_algorithm, &owner_id, 7, 5, 0, 10, &[sample_posting_record(1, false)]);
  let left_page = sample_posting_page(hash_algorithm, &owner_id, 7, 10, 5, 30, &[sample_posting_record(2, true)]);
  let right_page = sample_posting_page(hash_algorithm, &owner_id, 7, 30, 10, 40, &[sample_posting_record(3, true)]);
  let next_page = sample_posting_page(hash_algorithm, &owner_id, 7, 40, 30, 0, &[sample_posting_record(4, false)]);
  let source_root = sample_leaf_directory(hash_algorithm, &owner_id, 7, &[&previous_page, &left_page, &right_page, &next_page]);
  let left = decode_ordered_page(&left_page, hash_algorithm).unwrap();
  let right = decode_ordered_page(&right_page, hash_algorithm).unwrap();
  let previous = decode_ordered_page(&previous_page, hash_algorithm).unwrap();
  let next = decode_ordered_page(&next_page, hash_algorithm).unwrap();
  let proof_keys = [left.key.as_slice(), right.key.as_slice()];
  let compaction_sources = [left_page.as_slice(), right_page.as_slice()];
  let proof = TombstoneDropProofV1 {
    owner_id: &owner_id,
    source_page_keys: &proof_keys,
    coverage_epoch_id: 9,
    covered_through_sequence: 100,
    journal_contiguous_through_sequence: 100,
    pin_safe_through_generation: 7,
  };
  let page_plan = compact_ordered_page_window_v1(&OrderedPageCompactionWindowRequestV1 {
    hash_algorithm,
    source_pages: &compaction_sources,
    previous_posting_page: Some(&previous_page),
    next_posting_page: Some(&next_page),
    generation: 8,
    next_page_id: 50,
    tombstone_drop_proof: Some(&proof),
    layout: default_index_page_layout_v1(),
  })
  .unwrap();
  assert_eq!(page_plan.retired_page_ids, vec![10, 30]);
  assert_eq!(page_plan.replacements.len(), 4);

  let path_nodes = [source_root.as_slice()];
  let paths = [
    ArtifactDirectoryPathV1 { source_page_key: &left.key, directories: &path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &right.key, directories: &path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &previous.key, directories: &path_nodes },
    ArtifactDirectoryPathV1 { source_page_key: &next.key, directories: &path_nodes },
  ];
  let directory_plan = rewrite_artifact_directory_paths_v1(&ArtifactDirectoryMutationRequestV1 {
    hash_algorithm,
    generation: 8,
    page_plan: &page_plan,
    paths: &paths,
    layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  let closure_sources = [left_page.as_slice(), right_page.as_slice(), previous_page.as_slice(), next_page.as_slice()];
  let summary = validate_index_copy_on_write_closure_v1(&IndexCopyOnWriteClosureRequestV1 {
    hash_algorithm,
    generation: 8,
    initial_next_page_id: 50,
    source_pages: &closure_sources,
    paths: &paths,
    page_plan: &page_plan,
    directory_plan: &directory_plan,
    page_layout: default_index_page_layout_v1(),
    directory_layout: default_index_directory_layout_v1(),
  })
  .unwrap();
  assert_eq!((summary.live_count, summary.tombstone_count, summary.page_count), (2, 0, 2));
  assert!(summary.retained_encoded_bytes <= default_index_directory_layout_v1().maximum_workspace_bytes);

  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().join("compacted-shadow");
  let store = FileBackedShadowStore::create(&root);
  for source in [&previous_page, &left_page, &right_page, &next_page] {
    let page = decode_ordered_page(source, hash_algorithm).unwrap();
    store.put(&page.key, source);
  }
  for replacement in &page_plan.replacements {
    for artifact in &replacement.artifacts {
      store.put(&artifact.key, &artifact.value);
    }
  }
  for artifact in &directory_plan.artifacts {
    store.put(&artifact.key, &artifact.value);
  }
  store.sync();
  drop(store);

  let store = FileBackedShadowStore::reopen(&root);
  let new_root_key = summary.root_key.as_ref().unwrap();
  let new_root_bytes = store.get(new_root_key);
  let new_root = decode_artifact_directory(&new_root_bytes, hash_algorithm).unwrap();
  assert_eq!(new_root.entries.len(), 2);
  let reopened_pages = new_root
    .entries
    .iter()
    .map(|entry| decode_ordered_page(&store.get(entry.child_hash), hash_algorithm).unwrap().page_id)
    .collect::<Vec<_>>();
  assert_eq!(reopened_pages, vec![5, 40]);
  assert_eq!(decode_ordered_page(&store.get(&previous.key), hash_algorithm).unwrap().next_page_id, 10);
  assert_eq!(decode_ordered_page(&store.get(&next.key), hash_algorithm).unwrap().previous_page_id, 30);
}
