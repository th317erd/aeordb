//! Round 9: retain unknown executors without executing different semantics.
use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use aeordb::engine::v4::dependency::decode_dependency_table;
use aeordb::engine::v4::index_definition_runtime::IndexDefinitionRuntimeV1;
use aeordb::engine::v4::index_producer_collector::{
  IndexParserExecutorV1, IndexParserExecutionRequestV1, IndexParserOutcomeV1, IndexParserExecutionErrorV1,
};
use aeordb::engine::v4::index_source::{
  PluginMapperExecutorV1, PluginMapperOutcomeV1, PluginMapperRequestV1, SourceDocumentV1, SourceOperationalErrorClassV1,
  SourceOperationalResultV1, ValueStoreRuntimeV1,
};
use aeordb::engine::v4::source_evaluator::{
  AuthoritativeSourceDocumentV1, AuthoritativeSourceEvaluationErrorV1, AuthoritativeSourceEvaluatorV1, AuthoritativeSourceMemoryPolicyV1,
};
use aeordb::engine::v4::value_store::decode_value_store_definition;

fn fixture(family: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{family}/{name}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> usize {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

fn definition_dependency_offsets(bytes: &[u8], algorithm: HashAlgorithm) -> Vec<usize> {
  // Frozen AVST/ADPT offsets, independent of the production decoder or writer.
  let fixed = 32 + algorithm.hash_length();
  let table = fixed + 80 + u32_at(bytes, fixed) + u32_at(bytes, fixed + 4) + u32_at(bytes, fixed + 8);
  assert_eq!(&bytes[table..table + 4], b"ADPT");
  let mut cursor = table + 32;
  let mut offsets = Vec::new();
  for _ in 0..u32_at(bytes, table + 16) {
    offsets.push(cursor);
    cursor += u32_at(bytes, cursor);
  }
  assert_eq!(cursor, bytes.len());
  offsets
}

#[test]
fn dependency_tables_retain_unknown_abi_and_executor_profiles_for_known_kinds() {
  for profile in ["blake3-256", "sha512"] {
    for name in ["native-parser-resolution", "wasm-mapper"] {
      let original = fixture("dependency-table-v1", &format!("adpt-{profile}-{name}-valid"));
      assert_eq!(decode_dependency_table(&original).unwrap().records.len(), 1);
      for (abi, executor) in [(5, None), (u16::MAX, None), (0, Some(4)), (0, Some(u16::MAX)), (5, Some(4))] {
        let mut bytes = original.clone();
        if abi != 0 {
          bytes[44..46].copy_from_slice(&abi.to_le_bytes());
        }
        if let Some(executor) = executor {
          bytes[46..48].copy_from_slice(&executor.to_le_bytes());
        }
        let decoded = decode_dependency_table(&bytes).unwrap_or_else(|error| panic!("{profile}/{name}: {error}"));
        assert_eq!(decoded.records[0].abi, u16_at(&bytes, 44));
        assert_eq!(decoded.records[0].executor_profile, u16_at(&bytes, 46));
      }
    }
  }
}

#[test]
fn dependency_retention_still_rejects_unknown_kinds_and_known_invalid_combinations() {
  for name in ["native-parser-resolution", "wasm-mapper"] {
    let original = fixture("dependency-table-v1", &format!("adpt-blake3-256-{name}-valid"));
    let native = name == "native-parser-resolution";
    let invalid_fields = [
      (4, 0),
      (4, u16::MAX),
      (6, 0),
      (6, 5),
      (14, 0),
      (12, if native { 1 } else { 0 }),
      (14, if native { 2 } else { 1 }),
      (16, 0),
      (16, if native { 1 } else { 2 }),
      (18, if native { 1 } else { 0 }),
    ];
    for (offset, value) in invalid_fields {
      let mut bytes = original.clone();
      bytes[32 + offset..34 + offset].copy_from_slice(&value.to_le_bytes());
      assert!(decode_dependency_table(&bytes).is_err(), "{name}: field {offset}={value}");
    }
    let mut bytes = original.clone();
    bytes[72..104].fill(0);
    assert!(decode_dependency_table(&bytes).is_err());
    let mut bytes = original;
    bytes[104] = 1;
    assert!(decode_dependency_table(&bytes).is_err());
  }
}

#[test]
fn dependency_retention_checks_known_constraints_even_beside_unknown_fields() {
  let native = fixture("dependency-table-v1", "adpt-blake3-256-native-parser-resolution-valid");
  let wasm = fixture("dependency-table-v1", "adpt-blake3-256-wasm-mapper-valid");
  for kind in [1, 2] {
    for role in 0..=5 {
      for abi in [0, 1, 2, 3, 4, 5, u16::MAX] {
        for executor in [0, 1, 2, 3, 4, u16::MAX] {
          let mut bytes = if kind == 1 { wasm.clone() } else { native.clone() };
          for (offset, value) in [(38, role), (44, abi), (46, executor)] {
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
          }
          // Independent permanent-ID table: unknown fields do not excuse
          // an invalid known role, ABI, executor, or known ABI/profile pair.
          let expected = matches!(
            (kind, role, abi, executor),
            (2, 1 | 3 | 4, 0 | 5..=u16::MAX, 1 | 4..=u16::MAX)
              | (1, 1, 1, 3 | 4..=u16::MAX)
              | (1, 2, 2, 3 | 4..=u16::MAX)
              | (1, 1, 3, 2 | 4..=u16::MAX)
              | (1, 2, 4, 2 | 4..=u16::MAX)
              | (1, 1 | 2, 5..=u16::MAX, 2..=u16::MAX)
          );
          assert_eq!(decode_dependency_table(&bytes).is_ok(), expected, "kind={kind} role={role} ABI={abi} executor={executor}");
        }
      }
    }
  }
}

#[test]
fn complete_value_store_definitions_retain_unknown_profiles_without_losing_identity() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for name in ["json-corrected", "json-legacy", "mapper-corrected", "mapper-legacy"] {
      let original = fixture("value-store-definition-v1", &format!("avst-{profile}-{name}-valid"));
      let original_identity = decode_value_store_definition(&original, algorithm).unwrap().value_store_id;
      for record_offset in definition_dependency_offsets(&original, algorithm) {
        for offset in [12, 14] {
          let mut bytes = original.clone();
          bytes[record_offset + offset..record_offset + offset + 2].copy_from_slice(&u16::MAX.to_le_bytes());
          let definition = decode_value_store_definition(&bytes, algorithm)
            .unwrap_or_else(|error| panic!("{profile}/{name}: record {record_offset}, field {offset}: {error}"));
          assert_ne!(definition.value_store_id, original_identity);
          assert_eq!(definition.value_store_id.len(), algorithm.hash_length());
        }
      }
    }
  }
}

fn unknown_selector_bytes(profile: &str, algorithm: HashAlgorithm, family: &str) -> Vec<u8> {
  let mut bytes = fixture("value-store-definition-v1", &format!("avst-{profile}-json-{family}-valid"));
  let offsets = definition_dependency_offsets(&bytes, algorithm);
  let selector = offsets.into_iter().find(|offset| u16_at(&bytes, offset + 6) == 4).unwrap();
  bytes[selector + 40..selector + 72].fill(0xa7);
  bytes
}

#[test]
fn direct_selector_runtime_refuses_unknown_semantics_without_rejecting_structural_retention() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for family in ["corrected", "legacy"] {
      let bytes = unknown_selector_bytes(profile, algorithm, family);
      decode_value_store_definition(&bytes, algorithm).unwrap();
      let runtime = ValueStoreRuntimeV1::from_encoded(&bytes, algorithm).unwrap();
      let record = FileRecord::new("/source.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
      // Supply a parsed value: absent parser output already returns unavailable
      // in the old runtime and would conceal the missing selector identity gate.
      let parsed_value = CanonicalConfigValueV1::Map(std::collections::BTreeMap::new());
      let document = SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed_value) };
      let error = runtime.extract(document, None, &|| false).unwrap_err();
      assert_eq!(error.class(), SourceOperationalErrorClassV1::DependencyUnavailable);
      assert_eq!(
        runtime.extract(SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed_value) }, None, &|| true).unwrap_err().class(),
        SourceOperationalErrorClassV1::Cancelled
      );
    }
  }
}

#[test]
fn shared_producer_and_query_evaluators_preserve_unavailability_and_release_memory() {
  for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
    for family in ["corrected", "legacy"] {
      let algorithm = HashAlgorithm::Blake3_256;
      let bytes = unknown_selector_bytes("blake3-256", algorithm, family);
      let definition = decode_value_store_definition(&bytes, algorithm).unwrap();
      let memory = MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 32 << 20).unwrap());
      let evaluator = AuthoritativeSourceEvaluatorV1::from_encoded(
        &bytes,
        algorithm,
        definition.scope_id,
        &definition.value_store_id,
        memory.clone(),
        policy,
      )
      .unwrap();
      let retained = memory.snapshot().unwrap().reserved_bytes;
      let record = FileRecord::new("/source.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
      let document = AuthoritativeSourceDocumentV1 { namespace_root: &[1; 32], record_revision_hash: &[2; 32], file_record: &record };
      let parser = NeverParse;
      let error = match evaluator.evaluate(document, &parser, None, &|| false) {
        Ok(_) => panic!("unknown selector implementation was executed"),
        Err(error) => error,
      };
      match error {
        AuthoritativeSourceEvaluationErrorV1::Source(error) => {
          assert_eq!(error.class(), SourceOperationalErrorClassV1::DependencyUnavailable);
        }
        other => {
          panic!("unavailable executor must not become malformed configuration: {other}")
        }
      }
      assert_eq!(memory.snapshot().unwrap().reserved_bytes, retained);
      assert!(matches!(evaluator.evaluate(document, &parser, None, &|| true), Err(AuthoritativeSourceEvaluationErrorV1::Cancelled)));
      drop(evaluator);
      for owner in MemoryOwner::ALL {
        assert_eq!(memory.snapshot().unwrap().owner(owner).unwrap().reserved_bytes, 0, "{owner:?}");
      }
    }
  }
}

struct NeverParse;

struct NeverMap;

impl PluginMapperExecutorV1 for NeverMap {
  fn invoke(&self, _: PluginMapperRequestV1<'_>) -> SourceOperationalResultV1<PluginMapperOutcomeV1> {
    panic!("unknown mapper ABI/profile must not be passed to the current executor");
  }
}

#[test]
fn unknown_mapper_abi_and_profile_are_refused_before_parser_or_mapper_work() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    for family in ["corrected", "legacy"] {
      let original = fixture("value-store-definition-v1", &format!("avst-{profile}-mapper-{family}-valid"));
      let record_offset =
        definition_dependency_offsets(&original, algorithm).into_iter().find(|offset| u16_at(&original, offset + 6) == 2).unwrap();
      for field_offset in [12, 14] {
        let mut bytes = original.clone();
        bytes[record_offset + field_offset..record_offset + field_offset + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        let runtime = ValueStoreRuntimeV1::from_encoded(&bytes, algorithm).unwrap();
        let record = FileRecord::new("/source.json".to_string(), Some("application/json".to_string()), 0, Vec::new());
        let parsed_value = CanonicalConfigValueV1::Null;
        let document = SourceDocumentV1 { file_record: &record, parsed_value: Some(&parsed_value) };
        assert_eq!(
          runtime.extract(document, Some(&NeverMap), &|| false).unwrap_err().class(),
          SourceOperationalErrorClassV1::DependencyUnavailable
        );
        assert_eq!(runtime.extract(document, Some(&NeverMap), &|| true).unwrap_err().class(), SourceOperationalErrorClassV1::Cancelled);
        for policy in [AuthoritativeSourceMemoryPolicyV1::producer(), AuthoritativeSourceMemoryPolicyV1::selected_query()] {
          let memory = MemoryCoordinator::new(MemoryPolicy::new(128 << 20, 192 << 20, 1, 32 << 20).unwrap());
          let evaluator = AuthoritativeSourceEvaluatorV1::from_encoded(
            &bytes,
            algorithm,
            runtime.definition().scope_id,
            &runtime.definition().value_store_id,
            memory.clone(),
            policy,
          )
          .unwrap();
          let root = vec![1; algorithm.hash_length()];
          let revision = vec![2; algorithm.hash_length()];
          let document = AuthoritativeSourceDocumentV1 { namespace_root: &root, record_revision_hash: &revision, file_record: &record };
          let Err(AuthoritativeSourceEvaluationErrorV1::Source(error)) =
            evaluator.evaluate(document, &NeverParse, Some(&NeverMap), &|| false)
          else {
            panic!("unknown mapper must remain a typed unavailable source dependency");
          };
          assert_eq!(error.class(), SourceOperationalErrorClassV1::DependencyUnavailable);
          drop(evaluator);
          assert_eq!(memory.snapshot().unwrap().reserved_bytes, 0);
        }
      }
    }
  }
}

impl IndexParserExecutorV1 for NeverParse {
  fn parse(&self, _: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    panic!("unavailable selector must be refused before parser work");
  }
}

#[test]
fn completed_canonical_values_remain_queryable_without_the_original_selector_executor() {
  for (algorithm, profile) in [(HashAlgorithm::Blake3_256, "blake3-256"), (HashAlgorithm::Sha512, "sha512")] {
    let bytes = unknown_selector_bytes(profile, algorithm, "corrected");
    let definition = decode_value_store_definition(&bytes, algorithm).unwrap();
    let mut field = fixture("field-index-definition-v1", &format!("afix-{profile}-typed_exact_blake3_v1-valid"));
    field[32..32 + algorithm.hash_length()].copy_from_slice(&definition.value_store_id);
    let runtime = IndexDefinitionRuntimeV1::from_encoded(&bytes, &field, algorithm).unwrap();
    let canonical =
      encode_canonical_value(&CanonicalConfigValueV1::String("retained result".to_string()), CanonicalValueBounds::SOURCE_VALUE).unwrap();
    let result = runtime.compile_source_values(std::slice::from_ref(&canonical)).unwrap();
    assert_eq!(result.values.len(), 1);
    assert_eq!(result.values[0].canonical_value, canonical);
    assert_eq!(result.posting_count, 1);
  }
}
