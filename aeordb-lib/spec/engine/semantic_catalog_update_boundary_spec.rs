use super::*;
use std::collections::BTreeMap;
use super::super::super::config_value::{CanonicalConfigValueV1 as Value, encode_canonical_value};

#[test]
fn projection_schema_requires_exact_members_and_nonzero_selected_width_byte_ids() {
  for names in [vec![], vec!["fields"], vec!["different", "scope_id"], vec!["fields", "other"], vec!["fields", "scope_id", "z_extra"]] {
    let map = Value::Map(names.iter().map(|name| (name.to_string(), Value::Null)).collect());
    let bytes = encode_canonical_value(&map, CanonicalValueBounds::CONFIG).unwrap();
    let projection = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    assert!(
      matches!(two_members(projection, "fields", "scope_id"), Err(SemanticCatalogCompilationErrorV1::Catalog(error)) if error.class() == super::super::super::semantic_catalog::SemanticCatalogReadErrorClassV1::Corrupt)
    );
  }
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for value in [
      Value::Null,
      Value::Bytes(vec![]),
      Value::Bytes(vec![0; algorithm.hash_length()]),
      Value::Bytes(vec![1; algorithm.hash_length() + 1]),
    ] {
      let bytes = encode_canonical_value(&value, CanonicalValueBounds::CONFIG).unwrap();
      let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
      assert!(identifier(value, algorithm).is_err());
    }
    let bytes = encode_canonical_value(&Value::Bytes(vec![1; algorithm.hash_length()]), CanonicalValueBounds::CONFIG).unwrap();
    let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
    assert_eq!(identifier(value, algorithm).unwrap(), vec![1; algorithm.hash_length()]);
  }
  let bytes = encode_canonical_value(&Value::Null, CanonicalValueBounds::CONFIG).unwrap();
  let value = borrow_canonical_value(&bytes, CanonicalValueBounds::CONFIG).unwrap();
  assert!(two_members(value, "fields", "scope_id").is_err());
}

#[test]
fn dependency_reference_reader_rejects_wrong_owner_class_and_nonbyte_registry_entries() {
  let bytes =
    encode_canonical_value(&Value::Map(BTreeMap::from([("text/plain".into(), Value::Null)])), CanonicalValueBounds::CONFIG).unwrap();
  for class in [1, 2, 3, 4, 5, 6, 7] {
    let error = visit_dependencies(class, &bytes, HashAlgorithm::Blake3_256, |_, _| panic!("malformed input reached visitor")).unwrap_err();
    assert!(matches!(error, SemanticCatalogCompilationErrorV1::Catalog(_)));
  }
}
