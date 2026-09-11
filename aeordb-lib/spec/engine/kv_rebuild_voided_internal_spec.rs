use super::*;

#[test]
fn voided_incarnations_resolve_before_exclusion_across_spilled_runs() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let temporary = tempfile::tempdir().unwrap();
    let mut workspace =
      KvRebuildWorkspace::new_with_limits(&temporary.path().join("test.aeordb"), algorithm, None, AdmissionClass::Maintenance, 3, 2)
        .unwrap();
    let key = |value| vec![value; algorithm.hash_length()];
    // Input order deliberately differs from logical chronology and physical order.
    for index in (1..=12u8).rev() {
      workspace.push_value(KV_TYPE_DIRECTORY, &key(index), 100, 4, 99, RebuildOrder { timestamp: 10, offset: 100 }).unwrap();
      workspace.push_voided_value(KV_TYPE_DIRECTORY, &key(index), 200, 4, 99, RebuildOrder { timestamp: 20, offset: 200 }).unwrap();
    }
    // A new write after retirement wins; a later physical offset alone does not.
    workspace.push_value(KV_TYPE_DIRECTORY, &key(1), 50, 4, 99, RebuildOrder { timestamp: 30, offset: 50 }).unwrap();
    workspace.push_value(KV_TYPE_DIRECTORY, &key(2), 300, 4, 99, RebuildOrder { timestamp: 5, offset: 300 }).unwrap();
    // Equal timestamps are resolved by offset, including retirement evidence.
    workspace.push_value(KV_TYPE_DIRECTORY, &key(3), 250, 4, 99, RebuildOrder { timestamp: 20, offset: 250 }).unwrap();
    workspace.push_voided_value(KV_TYPE_DIRECTORY, &key(4), 400, 4, 99, RebuildOrder { timestamp: 20, offset: 400 }).unwrap();
    // An isolated voided record must not produce a locator, even a deleted one.
    workspace.push_voided_value(KV_TYPE_DIRECTORY, &key(13), 500, 4, 99, RebuildOrder { timestamp: 40, offset: 500 }).unwrap();
    workspace.finish().unwrap();
    let mut resolved = Vec::new();
    workspace
      .visit_resolved(|record| {
        resolved.push(record);
        Ok(())
      })
      .unwrap();
    assert_eq!(workspace.resolved_record_count().unwrap(), 2);
    assert_eq!(resolved.iter().map(|record| (record.hash.clone(), record.offset)).collect::<Vec<_>>(), vec![(key(1), 50), (key(3), 250)]);
  }
}

#[test]
fn voided_directory_resolution_applies_legacy_preference_only_after_retirement() {
  let temporary = tempfile::tempdir().unwrap();
  let mut workspace = KvRebuildWorkspace::new_with_limits(
    &temporary.path().join("test.aeordb"),
    HashAlgorithm::Blake3_256,
    None,
    AdmissionClass::Maintenance,
    1,
    2,
  )
  .unwrap();
  let retired = vec![1; 32];
  let recreated = vec![2; 32];
  workspace.push_value(KV_TYPE_DIRECTORY, &retired, 100, 4, 99, RebuildOrder { timestamp: 10, offset: 100 }).unwrap();
  workspace.push_voided_value(KV_TYPE_DIRECTORY, &retired, 200, 0, 95, RebuildOrder { timestamp: 20, offset: 200 }).unwrap();
  workspace.push_voided_value(KV_TYPE_DIRECTORY, &recreated, 300, 4, 99, RebuildOrder { timestamp: 10, offset: 300 }).unwrap();
  workspace.push_value(KV_TYPE_DIRECTORY, &recreated, 400, 0, 95, RebuildOrder { timestamp: 20, offset: 400 }).unwrap();
  workspace.finish().unwrap();
  let mut resolved = Vec::new();
  workspace
    .visit_resolved(|record| {
      resolved.push(record);
      Ok(())
    })
    .unwrap();
  assert_eq!(resolved.len(), 1);
  assert_eq!(resolved[0].hash, recreated, "a later empty rewrite must survive an earlier physical retirement");
  assert_eq!(resolved[0].offset, 400);
}

#[test]
fn later_empty_directory_recreation_is_not_eclipsed_by_pre_retirement_nonempty_data() {
  let temporary = tempfile::tempdir().unwrap();
  let mut workspace = KvRebuildWorkspace::new_with_limits(
    &temporary.path().join("test.aeordb"),
    HashAlgorithm::Blake3_256,
    None,
    AdmissionClass::Maintenance,
    1,
    2,
  )
  .unwrap();
  let key = vec![1; 32];
  workspace.push_value(KV_TYPE_DIRECTORY, &key, 100, 4, 99, RebuildOrder { timestamp: 10, offset: 100 }).unwrap();
  workspace.push_voided_value(KV_TYPE_DIRECTORY, &key, 200, 4, 99, RebuildOrder { timestamp: 20, offset: 200 }).unwrap();
  workspace.push_value(KV_TYPE_DIRECTORY, &key, 300, 0, 95, RebuildOrder { timestamp: 30, offset: 300 }).unwrap();
  workspace.finish().unwrap();
  let mut resolved = Vec::new();
  workspace
    .visit_resolved(|record| {
      resolved.push(record);
      Ok(())
    })
    .unwrap();
  assert_eq!(resolved.len(), 1, "the retirement boundary must not eclipse a later legitimate write");
  assert_eq!(resolved[0].offset, 300);
}

#[test]
fn voided_record_admission_rejects_bad_hashes_and_finalized_workspaces() {
  let temporary = tempfile::tempdir().unwrap();
  let mut workspace = KvRebuildWorkspace::new_with_limits(
    &temporary.path().join("test.aeordb"),
    HashAlgorithm::Blake3_256,
    None,
    AdmissionClass::Maintenance,
    1,
    2,
  )
  .unwrap();
  let order = RebuildOrder { timestamp: 1, offset: 100 };
  assert!(matches!(workspace.push_voided_value(KV_TYPE_DIRECTORY, &[1; 31], 100, 4, 99, order), Err(EngineError::InvalidInput(_))));
  workspace.push_voided_value(KV_TYPE_DIRECTORY, &[1; 32], 100, 4, 99, order).unwrap();
  workspace.finish().unwrap();
  assert_eq!(workspace.resolved_record_count().unwrap(), 0);
  assert!(matches!(workspace.push_voided_value(KV_TYPE_DIRECTORY, &[1; 32], 100, 4, 99, order), Err(EngineError::InvalidInput(_))));
}

#[test]
fn retirement_resolution_matches_independent_history_model() {
  // Enumerate every three-record combination of empty/nonempty values,
  // empty/nonempty retired values and logical deletion. The oracle filters a
  // complete tiny history; it does not use the production sort/resolver.
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for capacity in [1, 3, 2048] {
      let temporary = tempfile::tempdir().unwrap();
      let mut workspace = KvRebuildWorkspace::new_with_limits(
        &temporary.path().join("test.aeordb"),
        algorithm,
        None,
        AdmissionClass::Maintenance,
        capacity,
        2,
      )
      .unwrap();
      let mut expected = Vec::new();
      let mut identifier = 0u32;
      for type_flags in [KV_TYPE_DIRECTORY, crate::engine::kv_store::KV_TYPE_FILE_RECORD] {
        for orders in [[(10i64, 300u64), (20, 100), (20, 200)], [(0, 100), (0, 200), (0, 300)]] {
          for history in 0..125u32 {
            let actions = [history % 5, (history / 5) % 5, history / 25];
            let mut key = vec![0; algorithm.hash_length()];
            key[..4].copy_from_slice(&identifier.to_be_bytes());
            identifier += 1;
            let cutoff = (0..3).filter(|&index| matches!(actions[index], 2 | 3)).map(|index| orders[index]).max();
            let selected = (0..3)
              .filter(|&index| actions[index] < 2 && cutoff.is_none_or(|cutoff| orders[index] > cutoff))
              .max_by_key(|&index| (type_flags == KV_TYPE_DIRECTORY && actions[index] == 1, orders[index]));
            if let Some(index) = selected {
              let deleted = (0..3).any(|other| actions[other] == 4 && orders[other] > orders[index]);
              expected.push((key.clone(), orders[index].1, if deleted { type_flags | KV_FLAG_DELETED } else { type_flags }));
            }
            // Reverse insertion exercises histories crossing multiple run files.
            for index in (0..3).rev() {
              let order = RebuildOrder { timestamp: orders[index].0, offset: orders[index].1 };
              let value_length = if actions[index] % 2 == 1 { 4 } else { 0 };
              match actions[index] {
                0 | 1 => workspace.push_value(type_flags, &key, order.offset, value_length, 99, order).unwrap(),
                2 | 3 => workspace.push_voided_value(type_flags, &key, order.offset, value_length, 99, order).unwrap(),
                4 => workspace.push_record(WorkspaceRecord::deletion(&key, order).unwrap()).unwrap(),
                _ => unreachable!(),
              }
            }
          }
        }
      }
      workspace.finish().unwrap();
      let mut actual = Vec::new();
      workspace
        .visit_resolved(|record| {
          actual.push((record.hash, record.offset, record.type_flags));
          Ok(())
        })
        .unwrap();
      assert_eq!(workspace.resolved_record_count().unwrap() as usize, expected.len());
      assert_eq!(actual.len(), expected.len());
      for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual, expected, "hash width {} capacity {capacity}", algorithm.hash_length());
      }
    }
  }
}

#[test]
fn retirement_excludes_equal_order_values_and_keeps_later_offset_ties() {
  let temporary = tempfile::tempdir().unwrap();
  let mut workspace = KvRebuildWorkspace::new_with_limits(
    &temporary.path().join("test.aeordb"),
    HashAlgorithm::Blake3_256,
    None,
    AdmissionClass::Maintenance,
    1,
    2,
  )
  .unwrap();
  let order = RebuildOrder { timestamp: 10, offset: 100 };
  for key in [[1; 32], [2; 32]] {
    workspace.push_value(KV_TYPE_DIRECTORY, &key, 100, 4, 99, order).unwrap();
    workspace.push_voided_value(KV_TYPE_DIRECTORY, &key, 100, 4, 99, order).unwrap();
  }
  workspace.push_value(KV_TYPE_DIRECTORY, &[2; 32], 101, 0, 95, RebuildOrder { timestamp: 10, offset: 101 }).unwrap();
  workspace.finish().unwrap();
  let mut actual = Vec::new();
  workspace
    .visit_resolved(|record| {
      actual.push(record);
      Ok(())
    })
    .unwrap();
  assert_eq!(actual.len(), 1);
  assert_eq!(actual[0].hash, [2; 32]);
  assert_eq!(actual[0].offset, 101);
}

#[test]
fn logical_deletion_preserves_its_offset_tiebreaker_through_scratch_runs() {
  let temporary = tempfile::tempdir().unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let mut workspace =
    KvRebuildWorkspace::new_with_limits(&temporary.path().join("test.aeordb"), algorithm, None, AdmissionClass::Maintenance, 1, 2).unwrap();
  let deleted = file_path_hash("/deleted-same-time", &algorithm).unwrap();
  let recreated = file_path_hash("/recreated-same-time", &algorithm).unwrap();
  workspace.push_value(1, &deleted, 100, 4, 99, RebuildOrder { timestamp: 10, offset: 100 }).unwrap();
  workspace.push_deletion_path("/deleted-same-time", RebuildOrder { timestamp: 10, offset: 200 }).unwrap();
  workspace.push_deletion_path("/recreated-same-time", RebuildOrder { timestamp: 10, offset: 300 }).unwrap();
  workspace.push_value(1, &recreated, 400, 4, 99, RebuildOrder { timestamp: 10, offset: 400 }).unwrap();
  workspace.finish().unwrap();
  let mut resolved = Vec::new();
  workspace
    .visit_resolved(|record| {
      resolved.push(record);
      Ok(())
    })
    .unwrap();
  assert_eq!(resolved.len(), 2);
  assert!(resolved.iter().find(|record| record.hash == deleted).unwrap().is_deleted());
  assert!(!resolved.iter().find(|record| record.hash == recreated).unwrap().is_deleted());
}

#[test]
fn voided_record_admission_observes_cancellation() {
  let temporary = tempfile::tempdir().unwrap();
  let mut workspace = KvRebuildWorkspace::new_with_limits(
    &temporary.path().join("test.aeordb"),
    HashAlgorithm::Blake3_256,
    None,
    AdmissionClass::Maintenance,
    1,
    2,
  )
  .unwrap();
  workspace.cancellation = Some(Arc::new(AtomicBool::new(true)));
  assert!(matches!(
    workspace.push_voided_value(KV_TYPE_DIRECTORY, &[1; 32], 100, 4, 99, RebuildOrder { timestamp: 1, offset: 100 },),
    Err(EngineError::ShuttingDown)
  ));
  assert_eq!(workspace.raw_record_count(), 0);
}

#[test]
fn voided_record_scratch_rejects_other_versions_and_unknown_actions() {
  let algorithm = HashAlgorithm::Blake3_256;
  let record_length = RUN_RECORD_FIXED_LENGTH + 32 + RUN_RECORD_CRC_LENGTH;
  let header = encode_run_header(algorithm, 32, record_length, 0).unwrap();
  assert_eq!(decode_run_header(&header, algorithm, 32, record_length).unwrap(), 0);
  for version in [1u16, 3u16] {
    let mut altered = header;
    altered[8..10].copy_from_slice(&version.to_le_bytes());
    let checksum = crc32fast::hash(&altered[..28]);
    altered[28..32].copy_from_slice(&checksum.to_le_bytes());
    assert!(matches!(decode_run_header(&altered, algorithm, 32, record_length), Err(EngineError::CorruptEntry { .. })));
  }
  assert!(matches!(WorkspaceAction::from_u8(2).unwrap(), WorkspaceAction::VoidCoveredValue));
  assert!(matches!(WorkspaceAction::from_u8(3), Err(EngineError::CorruptEntry { .. })));
}
