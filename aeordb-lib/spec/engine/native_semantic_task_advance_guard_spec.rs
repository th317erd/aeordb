//! Recheck ownership, generation, capabilities and admission at every final boundary.
use super::*;
use super::advance_boundary::{resumed_request, start_fixture_compilation};
fn advance_selection(publisher: &V4FirstAuthorityPublisher) -> LoadedMutableSystemControlV1 {
  publisher.load_mutable_system_control(SystemControlKindV1::SemanticMutationTask, &[1; 16], &[2; 16]).unwrap().unwrap()
}

fn check_advance_final_guards(boundary: usize) {
  use crate::engine::memory_coordinator::HostMemorySample;
  for change in 0..9 {
    with_initial_task_for_work(|mut fixture| {
      start_fixture_compilation(&mut fixture);
      let protection = fixture.publisher.acquire_staging_protection(fixture.memory, fixture.cancellation).unwrap();
      let observed = observe_work_task(&fixture);
      let baseline = fixture.memory.snapshot().unwrap().reserved_bytes;
      let input = resumed_request(&fixture);
      let work = protection.begin_semantic_task_work(&observed, input, fixture.memory, fixture.cancellation, fixture.retirement).unwrap();
      let mut bytes_after_change = None;
      let change_authority = std::cell::RefCell::new(|| {
        assert!(fixture.publisher.root_state.try_lock().is_ok());
        match change {
          0 => fixture.cancellation.cancel(),
          1 => {
            fixture.memory.update_host_sample(HostMemorySample { rss_bytes: 512 << 20, ..HostMemorySample::default() }).unwrap();
          }
          2..=5 => {
            let mut header = fixture.publisher.observe().unwrap().selected.header;
            header.slot_sequence += 1;
            match change {
              2 => header.physical_instance_id = [9; 16],
              3 => header.writer_fence_epoch += 1,
              4 => header.required_reader_capabilities[3] &= !0b0010,
              5 => header.required_writer_capabilities[3] &= !0b1000,
              _ => unreachable!(),
            }
            write_redundant_header(fixture.publisher, &header);
          }
          6 => {
            let generation = fixture
              .publisher
              .load_mutable_system_control(SystemControlKindV1::SemanticMutationGeneration, &[1; 16], &[])
              .unwrap()
              .unwrap();
            let mut bytes = generation.bytes;
            bytes[16..24].copy_from_slice(&(generation.control_sequence + 1).to_le_bytes());
            crc(&mut bytes);
            seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationGeneration, &[], SystemControlSlotV1::B, &bytes)]);
          }
          7 => {
            let mut bytes = advance_selection(fixture.publisher).bytes;
            bytes[16..24].copy_from_slice(&5u64.to_le_bytes());
            bytes[32 + 64..32 + 72].copy_from_slice(&4u64.to_le_bytes());
            crc(&mut bytes);
            seed(fixture.publisher, &[(SystemControlKindV1::SemanticMutationTask, &[2; 16], SystemControlSlotV1::A, &bytes)]);
          }
          8 => {
            fixture.publisher.root_state.lock().unwrap().staging_accounting_failed = true;
          }
          _ => unreachable!(),
        }
        bytes_after_change = Some(fs::read(fixture.path).unwrap());
      });
      let request = advance_request(fixture.tree, input.publication_timestamp_ms + 20);
      let result = work.advance_compilation_observed(
        request,
        fixture.retirement,
        (
          || {
            if boundary == 0 {
              change_authority.borrow_mut()();
            }
          },
          || {
            if boundary == 1 {
              change_authority.borrow_mut()();
            }
          },
          || {
            if boundary == 2 {
              change_authority.borrow_mut()();
            }
          },
        ),
        (
          &mut NoopFirstAuthorityDependencyObserverV1,
          &mut NoopFirstAuthorityDependencyObserverV1,
          &mut NoopFirstAuthorityDependencyObserverV1,
        ),
      );
      let error = result.unwrap_err();
      assert!(bytes_after_change.is_some(), "case {change} failed before its target boundary: {error}");
      assert!(error.committed_receipt().is_none());
      assert!(error.committed_checkpoint_receipt().is_none());
      assert!(error.committed_candidate_receipt().is_none());
      assert_eq!(fs::read(fixture.path).unwrap(), bytes_after_change.unwrap());
      if change == 1 {
        fixture.memory.update_host_sample(HostMemorySample::default()).unwrap();
      }
      if change == 8 {
        fixture.publisher.root_state.lock().unwrap().staging_accounting_failed = false;
      }
      assert_eq!(fixture.memory.snapshot().unwrap().reserved_bytes, baseline);
      assert_eq!(advance_selection(fixture.publisher).control_sequence, if change == 7 { 5 } else { 4 });
    });
  }
}

#[test]
fn native_task_advance_candidate_rechecks_all_final_guards() {
  check_advance_final_guards(0);
}
#[test]
fn native_task_advance_pair_rechecks_all_final_guards() {
  check_advance_final_guards(1);
}
#[test]
fn native_task_advance_selection_rechecks_all_final_guards() {
  check_advance_final_guards(2);
}
