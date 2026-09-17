//! Existing compiler resolver calls are an independently exercised consumer edge.
use super::*;
use std::cell::RefCell;
use std::collections::BTreeSet;
use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::dependency::DependencyRecordV1;
use aeordb::engine::v4::index_configuration_compiler::{
  compile_index_configuration_v1, IndexConfigurationAliasSnapshotV1, IndexConfigurationCompilationRequestV1,
};
use aeordb::engine::v4::parser_registry_compiler::{
  compile_parser_registry_v1, ParserAliasSnapshotV1, ParserRegistryCompilationRequestV1, SemanticCompilationErrorV1,
};

#[derive(Default)]
struct Snapshot(RefCell<Vec<(u16, String)>>);

impl Snapshot {
  fn resolve(&self, role: u16, alias: &str) -> Result<Option<DependencyRecordV1<'static>>, SemanticCompilationErrorV1> {
    self.0.borrow_mut().push((role, alias.into()));
    Ok(Some(DependencyRecordV1 {
      kind: 1,
      role,
      flags: 4,
      abi: role + 2,
      executor_profile: 2,
      fingerprint_semantics: 1,
      artifact_kind: 1,
      artifact_length: 123,
      fingerprint: [0x42; 32],
      dependency_id: "/org/example/parser-and-mapper",
      version: "1.2.3",
    }))
  }
}

impl ParserAliasSnapshotV1 for Snapshot {
  fn resolve_parser_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(1, alias)
  }
}

impl IndexConfigurationAliasSnapshotV1 for Snapshot {
  fn resolve_mapper_alias(&self, alias: &str) -> Result<Option<DependencyRecordV1<'_>>, SemanticCompilationErrorV1> {
    self.resolve(2, alias)
  }
}

fn discovered(kind: SemanticSourceAliasKindV1, source: &[u8], memory: &MemoryCoordinator) -> Vec<(u16, String)> {
  let mut rows = Vec::new();
  visit_semantic_source_aliases_v1(
    request(kind, Some(source)),
    &mut |role, alias| {
      rows.push((
        match role {
          SemanticSourceAliasRoleV1::Parser => 1,
          SemanticSourceAliasRoleV1::Mapper => 2,
        },
        alias.into(),
      ));
      Ok(())
    },
    memory,
    &|| false,
  )
  .unwrap();
  rows
}

#[test]
fn discovery_matches_actual_registry_and_configuration_resolver_requirements_for_all_hashes() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let memory = MemoryCoordinator::new(MemoryPolicy::new(384 << 20, 512 << 20, 1, 16 << 20).unwrap());
    let snapshot = Snapshot::default();
    let registry_source = br#"{"$v":1,"parsers":{"text/b":"shared","TEXT/A":"shared"}}"#;
    let expected = discovered(SemanticSourceAliasKindV1::ParserRegistry, registry_source, &memory);
    assert_eq!(expected, vec![(1, "shared".into()), (1, "shared".into())]);
    let registry = compile_parser_registry_v1(
      ParserRegistryCompilationRequestV1 {
        source: Some(registry_source),
        hash_algorithm: algorithm,
        maximum_source_bytes: 64 << 10,
        maximum_workspace_bytes: 32 << 20,
      },
      &snapshot,
      &memory,
      &|| false,
    )
    .unwrap();
    assert_eq!(*snapshot.0.borrow(), expected);
    for (source, expected) in [
      (r#"{"$v":1,"parser":"unused","indexes":[]}"#, Vec::new()),
      (r#"{"$v":1,"parser":"unused","indexes":[{"name":"@hash","type":"typed_exact_blake3_v1"}]}"#, Vec::new()),
      (r#"{"$v":1,"parser":"shared","indexes":[{"name":"x","type":"typed_exact_blake3_v1"}]}"#, vec![(1, "shared".to_owned())]),
      (
        r#"{"$v":1,"parser":"shared","indexes":[{"name":"z","type":"typed_exact_blake3_v1","source":{"plugin":"shared"}},{"name":"a","type":"typed_exact_blake3_v1","source":{"plugin":"shared"}}]}"#,
        vec![(1, "shared".to_owned()), (2, "shared".to_owned())],
      ),
    ] {
      let found: BTreeSet<_> = discovered(SemanticSourceAliasKindV1::IndexConfiguration, source.as_bytes(), &memory).into_iter().collect();
      let expected: BTreeSet<_> = expected.into_iter().collect();
      assert_eq!(found, expected);
      snapshot.0.borrow_mut().clear();
      let compiled = compile_index_configuration_v1(
        IndexConfigurationCompilationRequestV1 {
          source: source.as_bytes(),
          owner_path: "/",
          registry: &registry,
          hash_algorithm: algorithm,
          maximum_source_bytes: 64 << 10,
          maximum_workspace_bytes: 128 << 20,
        },
        &snapshot,
        &memory,
        &|| false,
      )
      .unwrap();
      assert_eq!(snapshot.0.borrow().iter().cloned().collect::<BTreeSet<_>>(), expected);
      drop(compiled);
    }
    drop(registry);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
