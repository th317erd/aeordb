//! Bind execution-test copies to reviewed native semantics, never rewrite goldens.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::native_semantics::NativeSemanticComponentV1;
use aeordb::engine::v4::value_store::decode_value_store_definition;

pub fn pin_native_semantics(bytes: &mut [u8], algorithm: HashAlgorithm) {
  decode_value_store_definition(bytes, algorithm).unwrap();
  let read_u32 = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
  let fixed = 32 + algorithm.hash_length();
  let table = fixed + 80 + read_u32(fixed) + read_u32(fixed + 4) + read_u32(fixed + 8);
  assert_eq!(&bytes[table..table + 4], b"ADPT");
  let count = read_u32(table + 16);
  let mut offset = table + 32;
  for _ in 0..count {
    let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    let id_length = u32::from_le_bytes(bytes[offset + 20..offset + 24].try_into().unwrap()) as usize;
    let id = std::str::from_utf8(&bytes[offset + 96..offset + 96 + id_length]).unwrap();
    if u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap()) == 2 {
      let component = NativeSemanticComponentV1::ALL
        .into_iter()
        .find(|component| component.dependency_id() == id)
        .unwrap_or_else(|| panic!("execution fixture has an unbound native ID: {id}"));
      assert_eq!(u16::from_le_bytes(bytes[offset + 6..offset + 8].try_into().unwrap()), component.role());
      bytes[offset + 40..offset + 72].copy_from_slice(&component.fingerprint());
    }
    offset += length;
  }
  assert_eq!(offset, bytes.len());
  decode_value_store_definition(bytes, algorithm).unwrap();
}
