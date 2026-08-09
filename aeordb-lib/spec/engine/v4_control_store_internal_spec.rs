use super::*;

fn selection(slot: SystemControlSlotV1, database_id: &[u8]) -> SystemControlSelectionV1<'_> {
  SystemControlSelectionV1 {
    selected_slot: slot,
    control: SystemControlV1 { kind: SystemControlKindV1::DurabilityLatch, sequence: 7, database_id, identity: Vec::new(), body: &[] },
    redundancy_degraded: false,
  }
}

#[test]
fn loaded_mutable_conversion_rejects_a_missing_selected_slot() {
  let database_id = [0x11; 16];
  let error = loaded_mutable_control_from_selection(selection(SystemControlSlotV1::A, &database_id), None, Some(b"slot-b")).unwrap_err();

  assert_eq!(error.code(), "control_store_selected_slot_missing");
}

#[test]
fn loaded_mutable_conversion_rejects_an_immutable_selection() {
  let database_id = [0x22; 16];
  let error =
    loaded_mutable_control_from_selection(selection(SystemControlSlotV1::Immutable, &database_id), Some(b"slot-a"), None).unwrap_err();

  assert_eq!(error.code(), "control_store_mutable_selected_immutable");
}

#[test]
fn loaded_mutable_conversion_rejects_a_wrong_width_database_id() {
  let database_id = [0x33; 15];
  let error = loaded_mutable_control_from_selection(selection(SystemControlSlotV1::A, &database_id), Some(b"slot-a"), None).unwrap_err();

  assert_eq!(error.code(), "control_store_database_id_width");
}

#[test]
fn mutable_publication_slot_selection_rejects_an_immutable_current_slot() {
  let error = next_mutable_publication_slot(Some(SystemControlSlotV1::Immutable)).unwrap_err();

  assert!(matches!(error, EngineError::InvalidInput(ref message) if message.contains("immutable")));
}

#[test]
fn persisted_control_store_paths_contain_no_panic_macros() {
  let source = include_str!("../../src/engine/v4/control_store.rs");

  assert!(!source.contains("unreachable!("));
  assert!(!source.contains(".expect("));
}
