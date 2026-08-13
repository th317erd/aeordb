use std::any::TypeId;

use aeordb::engine::field_index_v0_nvt::FieldIndexV0Nvt;
use aeordb::engine::index_store::FieldIndex;
use aeordb::engine::kv_nvt::KvNvt;
use aeordb::engine::kv_store::KVStore;
use aeordb::engine::legacy_nvt_v1::LegacyNvtV1;
use aeordb::engine::nvt::NormalizedVectorTable;
use aeordb::engine::scalar_converter::{HashConverter, StringConverter};
use aeordb::engine::HashAlgorithm;

fn legacy_hash_nvt_fixture(bucket_count: u32, buckets: &[(u64, u32)]) -> Vec<u8> {
  assert_eq!(buckets.len(), bucket_count as usize);
  let mut bytes = Vec::new();
  bytes.push(1);
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.push(1);
  bytes.extend_from_slice(&bucket_count.to_le_bytes());
  for (offset, entry_count) in buckets {
    bytes.extend_from_slice(&offset.to_le_bytes());
    bytes.extend_from_slice(&entry_count.to_le_bytes());
  }
  bytes
}

fn empty_kv_store_fixture(bucket_count: u32) -> Vec<u8> {
  let nvt = legacy_hash_nvt_fixture(bucket_count, &vec![(0, 0); bucket_count as usize]);
  let mut bytes = Vec::new();
  bytes.push(1);
  bytes.extend_from_slice(&1u16.to_le_bytes());
  bytes.extend_from_slice(&0u64.to_le_bytes());
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes
}

fn populated_kv_store_fixture() -> Vec<u8> {
  let hash = vec![0; 32];
  let nvt = legacy_hash_nvt_fixture(2, &[(0, 1), (0, 0)]);
  let mut bytes = Vec::new();
  bytes.push(1);
  bytes.extend_from_slice(&1u16.to_le_bytes());
  bytes.extend_from_slice(&1u64.to_le_bytes());
  bytes.push(0);
  bytes.extend_from_slice(&hash);
  bytes.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
  bytes.extend_from_slice(&96u32.to_le_bytes());
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes
}

fn empty_v0_field_index_fixture(field_name: &str, bucket_count: u32) -> Vec<u8> {
  let nvt = legacy_hash_nvt_fixture(bucket_count, &vec![(0, 0); bucket_count as usize]);
  let mut bytes = Vec::new();
  bytes.push(0);
  bytes.extend_from_slice(&(field_name.len() as u16).to_le_bytes());
  bytes.extend_from_slice(field_name.as_bytes());
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.push(1);
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes
}

fn populated_v0_field_index_fixture(field_name: &str, bucket_count: u32) -> Vec<u8> {
  let mut buckets = vec![(0, 0); bucket_count as usize];
  buckets[0] = (0, 1);
  let nvt = legacy_hash_nvt_fixture(bucket_count, &buckets);
  let mut bytes = Vec::new();
  bytes.push(0);
  bytes.extend_from_slice(&(field_name.len() as u16).to_le_bytes());
  bytes.extend_from_slice(field_name.as_bytes());
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.push(1);
  bytes.extend_from_slice(&(nvt.len() as u32).to_le_bytes());
  bytes.extend_from_slice(&nvt);
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.extend_from_slice(&0f64.to_le_bytes());
  bytes.extend_from_slice(&[0; 32]);
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes
}

#[test]
fn legacy_kv_and_v0_field_nvt_have_distinct_owners() {
  assert_ne!(TypeId::of::<LegacyNvtV1>(), TypeId::of::<KvNvt>());
  assert_ne!(TypeId::of::<LegacyNvtV1>(), TypeId::of::<FieldIndexV0Nvt>());
  assert_ne!(TypeId::of::<KvNvt>(), TypeId::of::<FieldIndexV0Nvt>());
}

#[test]
fn public_normalized_vector_table_facade_preserves_legacy_bytes() {
  let mut legacy = LegacyNvtV1::new(Box::new(HashConverter), 2);
  legacy.update_bucket(0, 0x0102_0304_0506_0708, 3);
  legacy.update_bucket(1, 16, 1);

  let expected = legacy_hash_nvt_fixture(2, &[(0x0102_0304_0506_0708, 3), (16, 1)]);
  assert_eq!(legacy.serialize(), expected);

  let facade = NormalizedVectorTable::deserialize(&expected).unwrap();
  assert_eq!(facade.serialize(), expected);
}

#[test]
fn kv_nvt_preserves_legacy_payload_and_complete_kv_store_bytes() {
  let mut nvt = KvNvt::new(2);
  nvt.update_bucket(0, 0x0102_0304_0506_0708, 3);
  nvt.update_bucket(1, 16, 1);
  let expected = legacy_hash_nvt_fixture(2, &[(0x0102_0304_0506_0708, 3), (16, 1)]);
  assert_eq!(nvt.serialize(), expected);
  assert_eq!(KvNvt::deserialize(&expected).unwrap().serialize(), expected);

  let store = KVStore::new(HashAlgorithm::Blake3_256, 2);
  assert_eq!(store.serialize(), empty_kv_store_fixture(2));

  let mut populated = KVStore::new(HashAlgorithm::Blake3_256, 2);
  populated.insert(aeordb::engine::KVEntry {
    type_flags: aeordb::engine::KV_TYPE_CHUNK,
    hash: vec![0; 32],
    offset: 0x0102_0304_0506_0708,
    total_length: 96,
  });
  assert_eq!(populated.serialize(), populated_kv_store_fixture());
}

#[test]
fn field_index_v0_nvt_preserves_legacy_payload_and_complete_index_bytes() {
  let mut nvt = FieldIndexV0Nvt::new(Box::new(HashConverter), 2);
  nvt.update_bucket(0, 0x0102_0304_0506_0708, 3);
  nvt.update_bucket(1, 16, 1);
  let expected = legacy_hash_nvt_fixture(2, &[(0x0102_0304_0506_0708, 3), (16, 1)]);
  assert_eq!(nvt.serialize(), expected);
  assert_eq!(FieldIndexV0Nvt::deserialize(&expected).unwrap().serialize(), expected);

  let index = FieldIndex::new("hash".to_string(), Box::new(HashConverter));
  assert_eq!(index.serialize(32), empty_v0_field_index_fixture("hash", 1_024));

  let mut populated = FieldIndex::new("hash".to_string(), Box::new(HashConverter));
  populated.insert(&[0; 32], vec![0; 32]);
  populated.ensure_nvt_current();
  assert_eq!(populated.serialize(32), populated_v0_field_index_fixture("hash", 1_024));
}

#[test]
fn kv_wrapper_rejects_non_hash_legacy_payload_without_weakening_v0_field_compatibility() {
  let bytes = LegacyNvtV1::new(Box::new(StringConverter::new(32)), 2).serialize();
  assert!(KvNvt::deserialize(&bytes).is_err());
  assert_eq!(FieldIndexV0Nvt::deserialize(&bytes).unwrap().serialize(), bytes);
}
