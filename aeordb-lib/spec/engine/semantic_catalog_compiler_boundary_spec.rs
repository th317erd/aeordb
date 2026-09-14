use super::*;

#[test]
fn catalog_control_owner_limit_includes_kind_prefix_and_configuration_suffix() {
  let maximum = 65_537 - 2 - "/.aeordb-config/indexes.json".len();
  let path = format!("/{}", "a".repeat(maximum - 1));
  let key = configuration_owner(&path).unwrap();
  assert_eq!(key.len(), 65_537);
  assert_eq!(&key[..2], &1u16.to_le_bytes());
  assert!(key.ends_with(b"/.aeordb-config/indexes.json"));
  assert!(matches!(
    configuration_owner(&(path + "b")),
    Err(SemanticCatalogCompilationErrorV1::InvalidInput { code: "semantic_catalog_configuration_path", .. })
  ));
  assert_eq!(configuration_owner("/").unwrap(), b"\x01\x00/.aeordb-config/indexes.json");
}
