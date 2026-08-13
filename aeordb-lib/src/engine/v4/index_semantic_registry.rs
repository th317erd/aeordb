use std::sync::LazyLock;

use super::contract_generated::{SEMANTIC_BUNDLES, SemanticBundleContract, SemanticBundleKind};

pub const SOURCE_TYPE_NULL: u32 = 1 << 0;
pub const SOURCE_TYPE_BOOL: u32 = 1 << 1;
pub const SOURCE_TYPE_I64: u32 = 1 << 2;
pub const SOURCE_TYPE_U64: u32 = 1 << 3;
pub const SOURCE_TYPE_F64: u32 = 1 << 4;
pub const SOURCE_TYPE_UTF8: u32 = 1 << 5;
pub const SOURCE_TYPE_BYTES: u32 = 1 << 6;
pub const SOURCE_TYPE_ARRAY: u32 = 1 << 7;
pub const SOURCE_TYPE_MAP: u32 = 1 << 8;
pub const KNOWN_SOURCE_TYPES: u32 = SOURCE_TYPE_NULL
  | SOURCE_TYPE_BOOL
  | SOURCE_TYPE_I64
  | SOURCE_TYPE_U64
  | SOURCE_TYPE_F64
  | SOURCE_TYPE_UTF8
  | SOURCE_TYPE_BYTES
  | SOURCE_TYPE_ARRAY
  | SOURCE_TYPE_MAP;
pub const SCALAR_SOURCE_TYPES: u32 =
  SOURCE_TYPE_NULL | SOURCE_TYPE_BOOL | SOURCE_TYPE_I64 | SOURCE_TYPE_U64 | SOURCE_TYPE_F64 | SOURCE_TYPE_UTF8 | SOURCE_TYPE_BYTES;

pub const OPERATION_EQ: u64 = 1 << 0;
pub const OPERATION_IN: u64 = 1 << 1;
pub const OPERATION_GT: u64 = 1 << 2;
pub const OPERATION_LT: u64 = 1 << 3;
pub const OPERATION_BETWEEN: u64 = 1 << 4;
pub const OPERATION_CONTAINS: u64 = 1 << 5;
pub const OPERATION_SIMILAR: u64 = 1 << 6;
pub const OPERATION_PHONETIC: u64 = 1 << 7;
pub const OPERATION_FUZZY: u64 = 1 << 8;
pub const OPERATION_MATCH: u64 = 1 << 9;
pub const OPERATION_SORT: u64 = 1 << 10;
pub const OPERATION_AGGREGATE: u64 = 1 << 11;

pub const OPERATIONS_EXACT: u64 = OPERATION_EQ | OPERATION_IN;
pub const OPERATIONS_ORDERED: u64 =
  OPERATIONS_EXACT | OPERATION_GT | OPERATION_LT | OPERATION_BETWEEN | OPERATION_SORT | OPERATION_AGGREGATE;
pub const OPERATIONS_TRIGRAM: u64 = OPERATION_CONTAINS | OPERATION_SIMILAR | OPERATION_FUZZY | OPERATION_MATCH;
pub const OPERATIONS_PHONETIC: u64 = OPERATION_PHONETIC | OPERATION_MATCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConverterRegistryEntryV1 {
  pub id: u16,
  pub name: &'static str,
  pub corrected: bool,
  pub source_type_mask: u32,
  pub strategy_id: u16,
  pub tokenizing: bool,
  pub order_preserving: bool,
  pub behavior_fingerprint: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrategyRegistryEntryV1 {
  pub id: u16,
  pub name: &'static str,
  pub corrected: bool,
  pub operations: u64,
  pub behavior_fingerprint: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSelectorRegistryEntryV1 {
  pub id: u16,
  pub name: &'static str,
  pub corrected: bool,
  pub migration: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataSourceRegistryEntryV1 {
  pub id: u16,
  pub field_name: &'static str,
  pub corrected_source_type_mask: u32,
}

impl StrategyRegistryEntryV1 {
  pub fn supports_converter(self, converter_id: u16) -> bool {
    strategy_id_for_converter(converter_id) == Some(self.id) && (converter_id < 0x8000) == self.corrected
  }
}

static CONVERTER_REGISTRY: LazyLock<Vec<ConverterRegistryEntryV1>> = LazyLock::new(|| {
  SEMANTIC_BUNDLES.iter().filter(|bundle| bundle.kind == SemanticBundleKind::Converter).filter_map(converter_entry).collect::<Vec<_>>()
});

static STRATEGY_REGISTRY: LazyLock<Vec<StrategyRegistryEntryV1>> = LazyLock::new(|| {
  SEMANTIC_BUNDLES
    .iter()
    .filter(|bundle| bundle.kind == SemanticBundleKind::Strategy)
    .filter_map(|bundle| {
      operations_for_strategy(bundle.id).map(|operations| StrategyRegistryEntryV1 {
        id: bundle.id,
        name: bundle.name,
        corrected: bundle.corrected,
        operations,
        behavior_fingerprint: bundle.fingerprint_blake3,
      })
    })
    .collect::<Vec<_>>()
});

const SOURCE_SELECTOR_REGISTRY: &[SourceSelectorRegistryEntryV1] = &[
  SourceSelectorRegistryEntryV1 { id: 1, name: "metadata", corrected: true, migration: true },
  SourceSelectorRegistryEntryV1 { id: 2, name: "json_path", corrected: true, migration: true },
  SourceSelectorRegistryEntryV1 { id: 3, name: "plugin_mapper", corrected: true, migration: true },
  SourceSelectorRegistryEntryV1 { id: 4, name: "always_missing_v0", corrected: false, migration: true },
];

const METADATA_SOURCE_REGISTRY: &[MetadataSourceRegistryEntryV1] = &[
  MetadataSourceRegistryEntryV1 { id: 1, field_name: "@path", corrected_source_type_mask: SOURCE_TYPE_UTF8 },
  MetadataSourceRegistryEntryV1 { id: 2, field_name: "@filename", corrected_source_type_mask: SOURCE_TYPE_UTF8 },
  MetadataSourceRegistryEntryV1 { id: 3, field_name: "@extension", corrected_source_type_mask: SOURCE_TYPE_UTF8 },
  MetadataSourceRegistryEntryV1 { id: 4, field_name: "@content_type", corrected_source_type_mask: SOURCE_TYPE_NULL | SOURCE_TYPE_UTF8 },
  MetadataSourceRegistryEntryV1 { id: 5, field_name: "@size", corrected_source_type_mask: SOURCE_TYPE_U64 },
  MetadataSourceRegistryEntryV1 { id: 6, field_name: "@created_at", corrected_source_type_mask: SOURCE_TYPE_I64 },
  MetadataSourceRegistryEntryV1 { id: 7, field_name: "@updated_at", corrected_source_type_mask: SOURCE_TYPE_I64 },
  MetadataSourceRegistryEntryV1 { id: 8, field_name: "@hash", corrected_source_type_mask: SOURCE_TYPE_BYTES },
];

pub fn converter_registry() -> &'static [ConverterRegistryEntryV1] {
  &CONVERTER_REGISTRY
}

pub fn strategy_registry() -> &'static [StrategyRegistryEntryV1] {
  &STRATEGY_REGISTRY
}

pub fn source_selector_registry() -> &'static [SourceSelectorRegistryEntryV1] {
  SOURCE_SELECTOR_REGISTRY
}

pub fn metadata_source_registry() -> &'static [MetadataSourceRegistryEntryV1] {
  METADATA_SOURCE_REGISTRY
}

pub fn metadata_source_registry_entry(metadata_id: u16) -> Option<&'static MetadataSourceRegistryEntryV1> {
  metadata_source_registry().iter().find(|entry| entry.id == metadata_id)
}

pub fn converter_registry_entry(converter_id: u16) -> Option<&'static ConverterRegistryEntryV1> {
  converter_registry().iter().find(|entry| entry.id == converter_id)
}

pub fn strategy_registry_entry(strategy_id: u16, corrected: bool) -> Option<&'static StrategyRegistryEntryV1> {
  strategy_registry().iter().find(|entry| entry.id == strategy_id && entry.corrected == corrected)
}

fn converter_entry(bundle: &SemanticBundleContract) -> Option<ConverterRegistryEntryV1> {
  let (source_type_mask, tokenizing, order_preserving) = match bundle.id {
    0x0001 => (SCALAR_SOURCE_TYPES, false, false),
    0x0002 => (SOURCE_TYPE_BYTES, false, true),
    0x0003 => (SOURCE_TYPE_UTF8, false, true),
    0x0004 | 0x0005 => (SOURCE_TYPE_I64 | SOURCE_TYPE_U64, false, true),
    0x0006 => (SOURCE_TYPE_I64 | SOURCE_TYPE_U64 | SOURCE_TYPE_F64, false, true),
    0x0007 => (SOURCE_TYPE_I64 | SOURCE_TYPE_U64 | SOURCE_TYPE_UTF8, false, true),
    0x0008 => (SOURCE_TYPE_BOOL, false, true),
    0x0009..=0x000c => (SOURCE_TYPE_UTF8, true, false),
    0x8001..=0x8009 => (SOURCE_TYPE_BYTES, false, false),
    0x800a..=0x800d => (SOURCE_TYPE_BYTES, true, false),
    _ => return None,
  };
  Some(ConverterRegistryEntryV1 {
    id: bundle.id,
    name: bundle.name,
    corrected: bundle.corrected,
    source_type_mask,
    strategy_id: strategy_id_for_converter(bundle.id)?,
    tokenizing,
    order_preserving,
    behavior_fingerprint: bundle.fingerprint_blake3,
  })
}

fn strategy_id_for_converter(converter_id: u16) -> Option<u16> {
  match converter_id {
    0x0001 | 0x8001 => Some(1),
    0x0002..=0x0008 | 0x8002..=0x8009 => Some(2),
    0x0009 | 0x800a => Some(3),
    0x000a | 0x800b => Some(4),
    0x000b | 0x800c => Some(5),
    0x000c | 0x800d => Some(6),
    _ => None,
  }
}

fn operations_for_strategy(strategy_id: u16) -> Option<u64> {
  match strategy_id {
    1 => Some(OPERATIONS_EXACT),
    2 => Some(OPERATIONS_ORDERED),
    3 => Some(OPERATIONS_TRIGRAM),
    4..=6 => Some(OPERATIONS_PHONETIC),
    _ => None,
  }
}
