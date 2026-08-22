use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerSlotObservationV1, ActivePointerV1, plan_active_pointer_rewrite, select_closure_valid_active_pointer,
};
use aeordb::engine::v4::reader::MalformedInputClass;

const OWNER: [u8; 32] = [0x11; 32];
const OTHER_OWNER: [u8; 32] = [0x22; 32];
const TARGET_A: [u8; 32] = [0x31; 32];
const TARGET_B: [u8; 32] = [0x32; 32];

#[test]
fn closure_selection_prefers_usable_authority_over_a_newer_broken_pointer() {
  let older = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, 4, 41, &TARGET_A);
  let newer_broken = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 1, 9, 97, &TARGET_B);

  let selected = select_closure_valid_active_pointer(
    ActivePointerKindV1::FieldIndex,
    &OWNER,
    structural(&older, true),
    structural(&newer_broken, false),
  )
  .unwrap();
  assert_eq!(selected.selected().unwrap().slot, 0);
  assert!(!selected.repair_required());

  let plan =
    plan_active_pointer_rewrite(ActivePointerKindV1::FieldIndex, &OWNER, structural(&older, true), structural(&newer_broken, false))
      .unwrap();
  assert_eq!(plan.selection(), selected);
  assert_eq!(plan.expected_kind(), ActivePointerKindV1::FieldIndex);
  assert_eq!(plan.expected_owner_id(), OWNER);
  assert_eq!(plan.write_slot(), 0);
  assert_eq!(plan.next_sequence(), 98);
}

#[test]
fn selection_and_rewrite_cover_missing_invalid_and_unusable_pairs() {
  let brand_new = plan_active_pointer_rewrite(
    ActivePointerKindV1::ScopeCatalog,
    &OWNER,
    ActivePointerSlotObservationV1::Missing,
    ActivePointerSlotObservationV1::Missing,
  )
  .unwrap();
  assert!(brand_new.selection().selected().is_none());
  assert!(!brand_new.selection().repair_required());
  assert_eq!(brand_new.write_slot(), 0);
  assert_eq!(brand_new.next_sequence(), 1);

  let b = pointer(ActivePointerKindV1::ScopeCatalog, &OWNER, 1, 7, 12, &TARGET_A);
  let replace_invalid_a = plan_active_pointer_rewrite(
    ActivePointerKindV1::ScopeCatalog,
    &OWNER,
    ActivePointerSlotObservationV1::StructurallyInvalid,
    structural(&b, true),
  )
  .unwrap();
  assert_eq!(replace_invalid_a.selection().selected().unwrap().slot, 1);
  assert_eq!(replace_invalid_a.write_slot(), 0);
  assert_eq!(replace_invalid_a.next_sequence(), 13);

  let a = pointer(ActivePointerKindV1::ScopeCatalog, &OWNER, 0, 6, 11, &TARGET_B);
  let replace_missing_b =
    plan_active_pointer_rewrite(ActivePointerKindV1::ScopeCatalog, &OWNER, structural(&a, true), ActivePointerSlotObservationV1::Missing)
      .unwrap();
  assert_eq!(replace_missing_b.selection().selected().unwrap().slot, 0);
  assert_eq!(replace_missing_b.write_slot(), 1);
  assert_eq!(replace_missing_b.next_sequence(), 12);

  let a_unusable = pointer(ActivePointerKindV1::ScopeCatalog, &OWNER, 0, 8, 20, &TARGET_A);
  let b_unusable = pointer(ActivePointerKindV1::ScopeCatalog, &OWNER, 1, 9, 30, &TARGET_B);
  let neither_usable =
    plan_active_pointer_rewrite(ActivePointerKindV1::ScopeCatalog, &OWNER, structural(&a_unusable, false), structural(&b_unusable, false))
      .unwrap();
  assert!(neither_usable.selection().selected().is_none());
  assert_eq!(neither_usable.write_slot(), 0);
  assert_eq!(neither_usable.next_sequence(), 31);

  let newer_a = pointer(ActivePointerKindV1::ScopeCatalog, &OWNER, 0, 10, 40, &TARGET_A);
  let older_b = pointer(ActivePointerKindV1::ScopeCatalog, &OWNER, 1, 9, 30, &TARGET_B);
  let replace_older_b =
    plan_active_pointer_rewrite(ActivePointerKindV1::ScopeCatalog, &OWNER, structural(&newer_a, true), structural(&older_b, true)).unwrap();
  assert_eq!(replace_older_b.selection().selected().unwrap().slot, 0);
  assert_eq!(replace_older_b.write_slot(), 1);
  assert_eq!(replace_older_b.next_sequence(), 41);
}

#[test]
fn equal_sequences_require_one_target_and_have_one_deterministic_repair() {
  let a = pointer(ActivePointerKindV1::FieldNvt, &OWNER, 0, 5, 77, &TARGET_A);
  let b_same = pointer(ActivePointerKindV1::FieldNvt, &OWNER, 1, 5, 77, &TARGET_A);
  let redundant =
    plan_active_pointer_rewrite(ActivePointerKindV1::FieldNvt, &OWNER, structural(&a, true), structural(&b_same, true)).unwrap();
  assert_eq!(redundant.selection().selected().unwrap().slot, 0);
  assert!(redundant.selection().repair_required());
  assert_eq!(redundant.write_slot(), 1);
  assert_eq!(redundant.next_sequence(), 78);

  let one_usable =
    select_closure_valid_active_pointer(ActivePointerKindV1::FieldNvt, &OWNER, structural(&a, false), structural(&b_same, true)).unwrap();
  assert_eq!(one_usable.selected().unwrap().slot, 1);
  assert!(one_usable.repair_required());

  let b_ambiguous = pointer(ActivePointerKindV1::FieldNvt, &OWNER, 1, 6, 77, &TARGET_B);
  let error =
    select_closure_valid_active_pointer(ActivePointerKindV1::FieldNvt, &OWNER, structural(&a, true), structural(&b_ambiguous, false))
      .unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::AmbiguousEqualSequenceSelector);
}

#[test]
fn planner_rejects_foreign_pairs_and_never_wraps_publication_sequence() {
  let a = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, u64::MAX, u64::MAX, &TARGET_A);
  let b = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 1, 3, 8, &TARGET_B);
  let selected =
    select_closure_valid_active_pointer(ActivePointerKindV1::FieldIndex, &OWNER, structural(&a, true), structural(&b, true)).unwrap();
  assert_eq!(selected.selected().unwrap().sequence, u64::MAX);
  assert_eq!(
    plan_active_pointer_rewrite(ActivePointerKindV1::FieldIndex, &OWNER, structural(&a, true), structural(&b, true),).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );

  let wrong_owner = pointer(ActivePointerKindV1::FieldIndex, &OTHER_OWNER, 1, 3, 8, &TARGET_B);
  let wrong_kind = pointer(ActivePointerKindV1::FieldNvt, &OWNER, 1, 3, 8, &TARGET_B);
  let wrong_slot = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, 3, 8, &TARGET_B);
  for invalid_b in [&wrong_owner, &wrong_kind, &wrong_slot] {
    assert_eq!(
      select_closure_valid_active_pointer(ActivePointerKindV1::FieldIndex, &OWNER, structural(&a, true), structural(invalid_b, true),)
        .unwrap_err()
        .class(),
      MalformedInputClass::CrossRecordClosureMismatch
    );
  }
  assert_eq!(
    select_closure_valid_active_pointer(
      ActivePointerKindV1::FieldIndex,
      &[0; 32],
      structural(&a, true),
      ActivePointerSlotObservationV1::Missing,
    )
    .unwrap_err()
    .class(),
    MalformedInputClass::IdentityKeyOrGenerationMismatch
  );
}

#[test]
fn structural_observations_recheck_every_identity_field() {
  let valid = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, 3, 7, &TARGET_A);
  let zero_generation = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, 0, 7, &TARGET_A);
  let zero_sequence = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, 3, 0, &TARGET_A);
  let short_target = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, 3, 7, &TARGET_A[..31]);
  let zero_target = pointer(ActivePointerKindV1::FieldIndex, &OWNER, 0, 3, 7, &[0; 32]);
  for invalid_a in [&zero_generation, &zero_sequence, &short_target, &zero_target] {
    assert_eq!(
      select_closure_valid_active_pointer(
        ActivePointerKindV1::FieldIndex,
        &OWNER,
        structural(invalid_a, true),
        ActivePointerSlotObservationV1::Missing,
      )
      .unwrap_err()
      .class(),
      MalformedInputClass::IdentityKeyOrGenerationMismatch
    );
  }

  let no_usable = select_closure_valid_active_pointer(
    ActivePointerKindV1::FieldIndex,
    &OWNER,
    structural(&valid, false),
    ActivePointerSlotObservationV1::StructurallyInvalid,
  )
  .unwrap();
  assert!(no_usable.selected().is_none());
  assert!(!no_usable.repair_required());
}

fn structural<'a>(pointer: &'a ActivePointerV1<'a>, closure_valid: bool) -> ActivePointerSlotObservationV1<'a> {
  ActivePointerSlotObservationV1::Structural { pointer, closure_valid }
}

fn pointer<'a>(
  kind: ActivePointerKindV1,
  owner_id: &'a [u8],
  slot: u8,
  generation: u64,
  sequence: u64,
  target_manifest_hash: &'a [u8],
) -> ActivePointerV1<'a> {
  ActivePointerV1 { kind, generation, owner_id, slot, sequence, target_manifest_hash, key: vec![slot] }
}
