use super::{measure, measure_nth};
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorClassV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1,
  SemanticCatalogTraversalBoundsV1,
};

struct NoStorage;
impl SemanticCatalogObjectSourceV1 for NoStorage {
  fn load_semantic_object(&self, _: u16, _: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    panic!("lookup allocation refusal must precede storage access");
  }
}

#[test]
fn first_record_identity_and_prefix_allocation_refusals_are_recoverable_before_storage() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let root = vec![1; algorithm.hash_length()];
    for occurrence in [1, 2] {
      let (result, allocations) = measure_nth(algorithm.hash_length(), occurrence, || {
        SemanticCatalogReaderV1::new(algorithm, &NoStorage).with_first_record(
          &root,
          SemanticCatalogTraversalBoundsV1::new(1, 1).unwrap(),
          &|| false,
          |_| Ok(()),
        )
      });
      assert!(allocations.injected_failure, "allocation {occurrence} was not exercised");
      let error = result.unwrap_err();
      assert_eq!(error.class(), SemanticCatalogReadErrorClassV1::ResourceLimit);
      assert_eq!(error.code(), "semantic_catalog_allocation");
    }
  }
}

#[test]
fn lookup_digest_allocation_refusal_is_recoverable_before_storage_access() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let root = vec![1; algorithm.hash_length()];
    let owner = vec![2; algorithm.hash_length()];
    let bounds = SemanticCatalogTraversalBoundsV1::new(1, 1).unwrap();
    let (result, allocations) = measure(algorithm.hash_length(), || {
      SemanticCatalogReaderV1::new(algorithm, &NoStorage).with_record(&root, bounds, 3, &owner, &|| false, |_| Ok(()))
    });
    assert!(allocations.injected_failure);
    assert_eq!(result.unwrap_err().class(), SemanticCatalogReadErrorClassV1::ResourceLimit);
  }
}

#[test]
fn lookup_identity_and_prefix_allocation_refusals_are_recoverable_before_storage() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let root = vec![1; algorithm.hash_length()];
    let owner = vec![2; algorithm.hash_length()];
    for occurrence in [2, 3] {
      let (result, allocations) = measure_nth(algorithm.hash_length(), occurrence, || {
        SemanticCatalogReaderV1::new(algorithm, &NoStorage).with_record(
          &root,
          SemanticCatalogTraversalBoundsV1::new(1, 1).unwrap(),
          3,
          &owner,
          &|| false,
          |_| Ok(()),
        )
      });
      assert!(allocations.injected_failure, "allocation {occurrence} was not exercised");
      assert_eq!(result.unwrap_err().class(), SemanticCatalogReadErrorClassV1::ResourceLimit);
    }
  }
}
