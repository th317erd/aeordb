//! Preserve source-parser allocation classification and pre-admission bounds.
use super::measure;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::parser_registry_compiler::SemanticCompilationErrorV1;
use aeordb::engine::v4::semantic_source_capture::{visit_semantic_source_aliases_v1, SemanticSourceAliasKindV1, SemanticSourceAliasRequestV1};

#[test]
fn alias_discovery_registry_vector_allocation_refuses_as_resource_and_retries() {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap());
  let request = SemanticSourceAliasRequestV1 {
    kind: SemanticSourceAliasKindV1::ParserRegistry,
    source: Some(br#"{"$v":1,"parsers":{"text/plain":"x"}}"#),
    maximum_source_bytes: 4096,
    maximum_workspace_bytes: 16 << 20,
    maximum_alias_occurrences: 1,
  };
  let (result, allocations) = measure(4 * std::mem::size_of::<(String, String)>(), || {
    visit_semantic_source_aliases_v1(request, &mut |_, _| panic!("allocation refusal emitted an alias"), &memory, &|| false)
  });
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })), "{result:?}");
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
  assert_eq!(visit_semantic_source_aliases_v1(request, &mut |_, _| Ok(()), &memory, &|| false).unwrap(), 1);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn alias_discovery_source_and_workspace_limits_precede_large_input_parsing() {
  let source = vec![b'x'; 2 << 20];
  for kind in [SemanticSourceAliasKindV1::ParserRegistry, SemanticSourceAliasKindV1::IndexConfiguration] {
    for source_limit in [1, source.len()] {
      let memory = MemoryCoordinator::new(MemoryPolicy::new(64 << 20, 96 << 20, 1, 16 << 20).unwrap());
      let request = SemanticSourceAliasRequestV1 {
        kind,
        source: Some(&source),
        maximum_source_bytes: source_limit,
        maximum_workspace_bytes: 1,
        maximum_alias_occurrences: 1,
      };
      let (result, allocations) = measure(0, || {
        visit_semantic_source_aliases_v1(request, &mut |_, _| panic!("preflight refusal emitted an alias"), &memory, &|| false)
      });
      assert!(matches!(result, Err(SemanticCompilationErrorV1::Resource { .. })));
      assert!(allocations.maximum < 4096, "{allocations:?}");
      assert!(allocations.total < 16 << 10, "{allocations:?}");
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
    }
  }
}
