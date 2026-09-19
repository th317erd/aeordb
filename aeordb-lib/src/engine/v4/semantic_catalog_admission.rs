//! Read-only admission of an exact persisted compiler base, not root authority.
use super::*;
use super::super::config_value::{CanonicalValueBounds, borrow_canonical_value};
use super::super::dependency::encode_dependency_record;
use super::super::field_definition::decode_field_index_definition;
use super::super::native_semantics::NativeSemanticComponentV1;
use super::super::parser_plan::{ParserCandidateKind, ParserPlanKind};
use super::super::value_store::{ValueStoreDefinitionV1, ValueStoreSemanticFamily, decode_value_store_definition};
use super::update::{identifier, two_members, visit_dependencies};

/// Validate one pinned immutable snapshot before it can become an incremental
/// compiler base. This does not admit a namespace, pin objects, inspect live
/// aliases, execute dependencies, or publish bytes. The source must enforce
/// kind-specific allocation caps; the caller retains its snapshot/GC protection.
/// Unknown producer profiles remain structurally retainable, but cannot claim
/// the current compiler's stronger update invariants through this entry point.
pub fn admit_semantic_catalog_v1(
  request: SemanticCatalogCompilationRequestV1,
  semantic_state_id: &[u8],
  registry: &CompiledParserRegistryV1,
  source: &dyn SemanticCatalogObjectSourceV1,
  memory: &MemoryCoordinator,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<CompiledSemanticCatalogV1> {
  let mut compiler = Compiler::new(request, registry, memory, is_cancelled)?;
  if semantic_state_id.len() != request.hash_algorithm.hash_length() || semantic_state_id.iter().all(|byte| *byte == 0) {
    return Err(invalid("semantic_catalog_state_identity", "admission requires one nonzero selected-algorithm state identity"));
  }
  compiler.validate_definition(2, registry.projection())?;
  let bytes = source
    .load_semantic_object(1, semantic_state_id)?
    .ok_or_else(|| corrupt("semantic_catalog_state_missing", "selected semantic state is missing"))?;
  compiler.check()?;
  let decoded = decode_semantic_object(&bytes, request.hash_algorithm).map_err(format_error)?;
  if decoded.object_id != semantic_state_id {
    return Err(corrupt("semantic_catalog_state_identity", "state bytes do not match the requested identity"));
  }
  let state = decoded.semantic_state.ok_or_else(|| corrupt("semantic_catalog_state_kind", "selected object is not a semantic state"))?;
  if state.required_capabilities != request.required_capabilities {
    return Err(invalid("semantic_catalog_base_capabilities", "captured capabilities differ from the persisted base"));
  }
  let SemanticAvailabilityV1::Complete {
    compiler_fingerprint,
    semantic_registry_fingerprint,
    catalog_root,
    catalog_record_count,
    catalog_node_count,
    definition_count,
    dependency_count,
  } = state.availability
  else {
    return Err(invalid("semantic_catalog_base_unavailable", "content-only state cannot serve as a compiled update base"));
  };
  if compiler_fingerprint != semantic_compiler_fingerprint_v1(request.hash_algorithm)
    || semantic_registry_fingerprint
      != embedded_system_family_registry(request.hash_algorithm).map_err(format_error)?.semantic_projection_fingerprint
  {
    return Err(invalid("semantic_catalog_base_profile", "persisted compiler or semantic registry profile is not supported for updates"));
  }
  if definition_count != catalog_record_count {
    return Err(corrupt("semantic_catalog_counts", "compiler catalog must bind one distinct definition per record"));
  }
  compiler.root = Some(catalog_root);
  compiler.records = catalog_record_count;
  compiler.nodes = catalog_node_count;
  compiler.dependencies = dependency_count;
  compiler.configurations = request.expected_configuration_count;
  validate_catalog_closure(&mut compiler, registry, source, None)?;
  Ok(CompiledSemanticCatalogV1 {
    semantic_state: EncodedSemanticObjectV1 { object_id: state.object_id, value: bytes },
    hash_algorithm: request.hash_algorithm,
    catalog_root: compiler.root,
    record_count: compiler.records,
    node_count: compiler.nodes,
    dependency_count: compiler.dependencies,
    configuration_count: compiler.configurations,
    _memory: compiler.reservation,
  })
}

pub(super) struct CatalogCandidateAdmissionV1<'a> {
  pub(super) snapshot: SemanticCatalogSnapshotV1<'a>,
  pub(super) require_unused: bool,
}

/// Shared complete/partial catalog proof. The caller owns a fresh compiler;
/// any error discards it, including its temporary reachability reservation.
pub(super) fn validate_catalog_closure(
  compiler: &mut Compiler<'_>,
  registry: &CompiledParserRegistryV1,
  source: &dyn SemanticCatalogObjectSourceV1,
  candidates: Option<CatalogCandidateAdmissionV1<'_>>,
) -> Result<()> {
  let request = compiler.request;
  let is_cancelled = compiler.is_cancelled;
  let catalog_record_count = compiler.records;
  let catalog_node_count = compiler.nodes;
  let catalog_root = compiler.root.as_deref().ok_or_else(|| corrupt("semantic_catalog_root", "catalog has no registry root"))?;
  // Refuse impossible claimed geometry before scanning the catalog, but do not
  // allocate from unverified counts. The complete walk below must first agree.
  let rounded_records = catalog_record_count
    .checked_add(7)
    .ok_or_else(|| resource("semantic_catalog_reachability_memory", "reachability bitmap geometry overflow"))?;
  let bitmap_length =
    usize::try_from(rounded_records / 8).map_err(|error| resource("semantic_catalog_reachability_memory", error.to_string()))?;
  if STAGING_WORKSPACE_BYTES.checked_add(bitmap_length).is_none_or(|bytes| bytes > request.maximum_workspace_bytes) {
    return Err(resource("semantic_catalog_reachability_memory", "reachability bitmap exceeds the caller workspace ceiling"));
  }
  let bounds = SemanticCatalogTraversalBoundsV1::new(catalog_record_count, catalog_node_count)?;
  let reader = SemanticCatalogReaderV1::new(request.hash_algorithm, source);
  // First establish the whole tree's counts/identities. A subsequent point
  // lookup may then trust untouched subtrees, including their ordinal weights.
  let stats = reader.walk_catalog(catalog_root, bounds, is_cancelled, |record| {
    compiler.check().map_err(catalog_check_error)?;
    reader.with_definition(record, is_cancelled, |definition| {
      validate_binding(record, definition, request.hash_algorithm).map_err(catalog_check_error)
    })
  })?;
  if stats.class_counts[1] != compiler.configurations
    || stats.class_counts[2] != 1
    || stats.class_counts[6].checked_add(stats.class_counts[7]) != Some(compiler.dependencies)
  {
    return Err(corrupt("semantic_catalog_counts", "actual configuration, registry or dependency count differs"));
  }
  compiler.check()?;
  compiler.reservation.grow(bitmap_length as u64).map_err(|error| resource("semantic_catalog_reachability_memory", error.to_string()))?;
  let mut bitmap = Vec::new();
  bitmap.try_reserve_exact(bitmap_length).map_err(|error| resource("semantic_catalog_reachability_memory", error.to_string()))?;
  bitmap.resize(bitmap_length, 0);
  let mut graph = AdmissionGraph {
    compiler,
    reader: SemanticCatalogReaderV1::new(request.hash_algorithm, source),
    bounds,
    registry,
    bitmap: &mut bitmap,
    marked: 0,
  };
  let mut ordinal = 0u64;
  let root = compiler.root.as_deref().ok_or_else(|| corrupt("semantic_catalog_root", "validated catalog root disappeared"))?;
  reader.walk_catalog(root, bounds, is_cancelled, |record| {
    let position = ordinal;
    ordinal = ordinal.checked_add(1).ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_counts", "ordinal overflow"))?;
    if matches!(record.record_kind, 1 | 2) {
      graph.mark(position).map_err(catalog_check_error)?;
      reader.with_definition(record, is_cancelled, |definition| graph.primary(record, definition).map_err(catalog_check_error))?;
    }
    Ok(())
  })?;
  if let Some(candidates) = candidates {
    let snapshot = candidates.snapshot;
    if snapshot.record_count > compiler.dependencies {
      return Err(corrupt("semantic_catalog_progress_candidates", "candidate count exceeds actual dependency count"));
    }
    match snapshot.root_object_id {
      Some(candidate_root) => {
        let candidate_bounds = SemanticCatalogTraversalBoundsV1::new(snapshot.record_count, snapshot.node_count)?;
        reader.walk_catalog(candidate_root, candidate_bounds, is_cancelled, |record| {
          compiler.check().map_err(catalog_check_error)?;
          if !matches!(record.record_kind, 6 | 7) {
            return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_progress_candidate_kind", "candidate is not a dependency"));
          }
          let ordinal = reader
            .with_record_ordinal(root, bounds, record.record_kind, record.owner_key, is_cancelled, |ordinal, retained| {
              if retained.semantic_id != record.semantic_id || retained.definition_object_id != record.definition_object_id {
                return Err(SemanticCatalogReadErrorV1::corrupt(
                  "semantic_catalog_progress_candidate_binding",
                  "candidate differs from its exact main-catalog binding",
                ));
              }
              Ok(ordinal)
            })?
            .ok_or_else(|| {
              SemanticCatalogReadErrorV1::corrupt("semantic_catalog_progress_candidate_missing", "candidate is absent from main catalog")
            })?;
          if candidates.require_unused && graph.is_marked(ordinal).map_err(catalog_check_error)? {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "semantic_catalog_progress_live_candidate",
              "pruning candidate is still reachable",
            ));
          }
          graph.mark(ordinal).map_err(catalog_check_error)
        })?;
      }
      None if snapshot.record_count == 0 && snapshot.node_count == 0 => {}
      None => return Err(corrupt("semantic_catalog_progress_candidates", "absent candidate root has nonzero counts")),
    }
  }
  if graph.marked != catalog_record_count {
    return Err(corrupt("semantic_catalog_orphan", "catalog retains definitions not reachable from configuration and registry owners"));
  }
  drop(bitmap);
  compiler.reservation.shrink(bitmap_length as u64).map_err(|error| resource("semantic_catalog_reachability_memory", error.to_string()))?;
  compiler.check()
}

struct AdmissionGraph<'a, 'source> {
  compiler: &'a Compiler<'a>,
  reader: SemanticCatalogReaderV1<'source>,
  bounds: SemanticCatalogTraversalBoundsV1,
  registry: &'a CompiledParserRegistryV1,
  bitmap: &'a mut [u8],
  marked: u64,
}

impl AdmissionGraph<'_, '_> {
  fn root(&self) -> Result<&[u8]> {
    self.compiler.root.as_deref().ok_or_else(|| corrupt("semantic_catalog_root", "admitted catalog has no registry root"))
  }

  fn is_marked(&self, ordinal: u64) -> Result<bool> {
    self.compiler.check()?;
    if ordinal >= self.compiler.records {
      return Err(corrupt("semantic_catalog_ordinal", "reachability position is outside the validated catalog"));
    }
    let byte = self
      .bitmap
      .get((ordinal / 8) as usize)
      .ok_or_else(|| corrupt("semantic_catalog_ordinal", "reachability position exceeds the admitted bitmap"))?;
    Ok(*byte & (1 << (ordinal % 8)) != 0)
  }

  fn mark(&mut self, ordinal: u64) -> Result<()> {
    self.compiler.check()?;
    if ordinal >= self.compiler.records {
      return Err(corrupt("semantic_catalog_ordinal", "reachability position is outside the validated catalog"));
    }
    let byte = self
      .bitmap
      .get_mut((ordinal / 8) as usize)
      .ok_or_else(|| corrupt("semantic_catalog_ordinal", "reachability position exceeds the admitted bitmap"))?;
    let mask = 1 << (ordinal % 8);
    if *byte & mask == 0 {
      *byte |= mask;
      self.marked = self.marked.checked_add(1).ok_or_else(|| corrupt("semantic_catalog_counts", "reachability count overflow"))?;
    }
    Ok(())
  }

  fn definition(&mut self, class: u16, owner: &[u8]) -> Result<Vec<u8>> {
    self.compiler.check()?;
    let (ordinal, bytes) = self
      .reader
      .with_record_ordinal(self.root()?, self.bounds, class, owner, self.compiler.is_cancelled, |ordinal, record| {
        self.reader.with_definition(record, self.compiler.is_cancelled, |definition| {
          validate_binding(record, definition, self.compiler.request.hash_algorithm).map_err(catalog_check_error)?;
          Ok((ordinal, copy_bytes(definition).map_err(catalog_check_error)?))
        })
      })?
      .ok_or_else(|| corrupt("semantic_catalog_definition_missing", "a typed ownership edge has no catalog binding"))?;
    self.mark(ordinal)?;
    Ok(bytes)
  }

  fn primary(&mut self, record: SemanticCatalogRecordV1<'_>, bytes: &[u8]) -> Result<()> {
    self.compiler.check()?;
    let algorithm = self.compiler.request.hash_algorithm;
    if record.record_kind == 2 {
      if record.owner_key != b"\x02\0/.aeordb-config/parsers.json" || record.semantic_id != self.registry.projection().semantic_id {
        return Err(corrupt("semantic_catalog_registry", "registry binding differs from the exact captured compiler registry"));
      }
      let expected = decode_semantic_definition_record(&self.registry.projection().object.value, algorithm).map_err(format_error)?;
      if bytes != expected.definition {
        return Err(corrupt("semantic_catalog_registry", "stored registry bytes differ from the captured registry"));
      }
      return self.dependencies(2, bytes);
    }
    let projection = borrow_canonical_value(bytes, CanonicalValueBounds::CONFIG).map_err(format_error)?;
    let (fields, scope) = two_members(projection, "fields", "scope_id")?;
    let scope_id = identifier(scope, algorithm)?;
    let scope_bytes = self.definition(3, scope_id)?;
    let scope = decode_scope_definition(&scope_bytes, algorithm).map_err(format_error)?;
    if scope.scope_id != scope_id || configuration_owner(scope.owner_path)? != record.owner_key {
      return Err(corrupt("semantic_catalog_configuration_owner", "configuration projection does not own its scope"));
    }
    for field in fields.map_entries().map_err(format_error)? {
      self.compiler.check()?;
      let (name, field) = field.map_err(format_error)?;
      let (indexes, value_id) = two_members(field, "indexes", "value_store_id")?;
      let value_id = identifier(value_id, algorithm)?;
      let value_bytes = self.definition(4, value_id)?;
      let value = decode_value_store_definition(&value_bytes, algorithm).map_err(format_error)?;
      if value.value_store_id != value_id
        || value.scope_id != scope_id
        || value.field_name != name
        || value.semantic_family != ValueStoreSemanticFamily::CorrectedV1
      {
        return Err(corrupt("semantic_catalog_value_owner", "configuration field does not own a corrected ValueStore"));
      }
      self.parser_registry(&value)?;
      self.dependencies(4, &value_bytes)?;
      let mut previous: Option<&[u8]> = None;
      for index in indexes.array_entries().map_err(format_error)? {
        self.compiler.check()?;
        let index_id = identifier(index.map_err(format_error)?, algorithm)?;
        if previous.is_some_and(|previous| previous >= index_id) {
          return Err(corrupt("semantic_catalog_index_order", "projection indexes are not strictly ordered"));
        }
        previous = Some(index_id);
        let index_bytes = self.definition(5, index_id)?;
        let index = decode_field_index_definition(&index_bytes, algorithm).map_err(format_error)?;
        if index.index_id != index_id || index.value_store_id != value_id || !index.corrected || !index.converter.corrected {
          return Err(corrupt("semantic_catalog_index_owner", "projection index does not belong to its corrected ValueStore"));
        }
      }
      if previous.is_none() {
        return Err(corrupt("semantic_catalog_projection_schema", "compiler field has no indexes"));
      }
    }
    self.compiler.check()
  }

  fn dependencies(&mut self, class: u16, bytes: &[u8]) -> Result<()> {
    let algorithm = self.compiler.request.hash_algorithm;
    visit_dependencies(class, bytes, algorithm, |class, expected| {
      let retained = self.definition(class, &expected.semantic_id)?;
      let expected = decode_semantic_definition_record(&expected.object.value, algorithm).map_err(format_error)?;
      if retained != expected.definition {
        return Err(corrupt("semantic_catalog_dependency_closure", "dependency record differs from its exact retained binding"));
      }
      Ok(())
    })
  }

  fn parser_registry(&self, value: &ValueStoreDefinitionV1<'_>) -> Result<()> {
    for dependency in &value.dependencies.records {
      self.compiler.check()?;
      let supported = if dependency.kind == 1 {
        matches!(dependency.role, 1 | 2)
          && dependency.flags == 4
          && dependency.abi == dependency.role + 2
          && dependency.executor_profile == 2
      } else {
        NativeSemanticComponentV1::ALL.iter().any(|component| component.dependency_record() == *dependency)
      };
      if !supported {
        return Err(invalid("semantic_catalog_dependency_profile", "dependency is retainable but not a current compiler output"));
      }
    }
    if value.parser_plan.kind != ParserPlanKind::Automatic {
      return Ok(());
    }
    for candidate in &value.parser_plan.candidates {
      let component = match candidate.kind {
        ParserCandidateKind::RawJson => NativeSemanticComponentV1::RawJson,
        ParserCandidateKind::NativeSuite => NativeSemanticComponentV1::NativeSuite,
        _ => continue,
      };
      let dependency = candidate.dependency_ordinal.checked_sub(1).and_then(|ordinal| value.dependencies.records.get(ordinal as usize));
      if dependency != Some(&component.dependency_record()) {
        return Err(corrupt("semantic_catalog_native_candidate", "automatic native candidate resolves to a different component"));
      }
    }
    let mut expected = self.registry.entries().iter();
    for candidate in value.parser_plan.candidates.iter().filter(|candidate| candidate.kind == ParserCandidateKind::Registry) {
      self.compiler.check()?;
      let entry =
        expected.next().ok_or_else(|| corrupt("semantic_catalog_parser_registry", "automatic plan has extra registry candidates"))?;
      let dependency = candidate
        .dependency_ordinal
        .checked_sub(1)
        .and_then(|ordinal| value.dependencies.records.get(ordinal as usize))
        .ok_or_else(|| corrupt("semantic_catalog_parser_registry", "automatic candidate dependency is missing"))?;
      if candidate.match_bytes != entry.essence().as_bytes()
        || encode_dependency_record(dependency).map_err(format_error)? != entry.dependency_bytes()
      {
        return Err(corrupt("semantic_catalog_parser_registry", "automatic plan differs from the captured registry"));
      }
    }
    if expected.next().is_some() {
      return Err(corrupt("semantic_catalog_parser_registry", "automatic plan omitted captured registry candidates"));
    }
    Ok(())
  }
}

fn validate_binding(record: SemanticCatalogRecordV1<'_>, bytes: &[u8], algorithm: HashAlgorithm) -> Result<()> {
  let rebuilt = encode_semantic_definition_object(record.record_kind, bytes, algorithm).map_err(format_error)?;
  if rebuilt.semantic_id != record.semantic_id
    || rebuilt.object.object_id != record.definition_object_id
    || (record.record_kind >= 3 && record.owner_key != record.semantic_id)
  {
    return Err(corrupt("semantic_catalog_definition_closure", "definition identity differs from its exact typed binding"));
  }
  Ok(())
}
