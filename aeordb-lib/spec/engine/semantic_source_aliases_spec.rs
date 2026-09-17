//! Independent declared dependency requests, never a complete-union oracle.
#[path = "semantic_source_aliases_compiler_spec.rs"]
mod compiler;
#[path = "semantic_source_aliases_validation_spec.rs"]
mod validation;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::semantic_source_capture::{
  visit_semantic_source_aliases_v1, SemanticSourceAliasKindV1, SemanticSourceAliasRequestV1, SemanticSourceAliasRoleV1,
};

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap())
}

fn request(kind: SemanticSourceAliasKindV1, source: Option<&[u8]>) -> SemanticSourceAliasRequestV1<'_> {
  SemanticSourceAliasRequestV1 {
    kind,
    source,
    maximum_source_bytes: 64 << 10,
    maximum_workspace_bytes: 32 << 20,
    maximum_alias_occurrences: 1024,
  }
}

#[test]
fn registry_alias_discovery_preserves_normalized_mime_order_and_repeated_aliases() {
  let memory = memory();
  let source = br#"{"$v":1,"parsers":{"text/z":"same","TEXT/A":"first","application/x-last":"same"}}"#;
  let mut seen = Vec::new();
  let count = visit_semantic_source_aliases_v1(
    request(SemanticSourceAliasKindV1::ParserRegistry, Some(source)),
    &mut |role, alias| {
      seen.push((role, alias.to_owned()));
      Ok(())
    },
    &memory,
    &|| false,
  )
  .expect("valid registry must expose its exact parser requests");
  assert_eq!(count, 3);
  assert_eq!(
    seen,
    vec![
      (SemanticSourceAliasRoleV1::Parser, "same".into()),
      (SemanticSourceAliasRoleV1::Parser, "first".into()),
      (SemanticSourceAliasRoleV1::Parser, "same".into())
    ]
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn configuration_alias_discovery_keeps_parser_mapper_roles_and_source_occurrences() {
  let memory = memory();
  let source = br#"{"$v":1,"parser":"shared","indexes":[{"name":"z","type":"typed_exact_blake3_v1","source":{"plugin":"shared"}},{"name":"a","type":"typed_exact_blake3_v1","source":{"plugin":"shared"}},{"name":"plain","type":"typed_exact_blake3_v1"},{"name":"@hash","type":"typed_exact_blake3_v1"}]}"#;
  let mut seen = Vec::new();
  let count = visit_semantic_source_aliases_v1(
    request(SemanticSourceAliasKindV1::IndexConfiguration, Some(source)),
    &mut |role, alias| {
      seen.push((role, alias.to_owned()));
      Ok(())
    },
    &memory,
    &|| false,
  )
  .expect("valid source must expose used parser and each mapper occurrence");
  assert_eq!(count, 3);
  assert_eq!(
    seen,
    vec![
      (SemanticSourceAliasRoleV1::Parser, "shared".into()),
      (SemanticSourceAliasRoleV1::Mapper, "shared".into()),
      (SemanticSourceAliasRoleV1::Mapper, "shared".into())
    ]
  );
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn absent_empty_and_unused_parser_sources_complete_without_dependency_notifications() {
  for (kind, source) in [
    (SemanticSourceAliasKindV1::ParserRegistry, None),
    (SemanticSourceAliasKindV1::IndexConfiguration, None),
    (SemanticSourceAliasKindV1::ParserRegistry, Some(br#"{"$v":1,"parsers":{}}"#.as_slice())),
    (SemanticSourceAliasKindV1::IndexConfiguration, Some(br#"{"$v":1,"parser":"missing","indexes":[]}"#.as_slice())),
    (
      SemanticSourceAliasKindV1::IndexConfiguration,
      Some(br#"{"$v":1,"parser":"missing","indexes":[{"name":"@hash","type":"typed_exact_blake3_v1"}]}"#.as_slice()),
    ),
  ] {
    let memory = memory();
    let mut request = request(kind, source);
    request.maximum_alias_occurrences = 0;
    let count = visit_semantic_source_aliases_v1(request, &mut |_, _| panic!("unused dependency was emitted"), &memory, &|| false)
      .expect("empty dependency discovery must complete");
    assert_eq!(count, 0);
    assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  }
}
