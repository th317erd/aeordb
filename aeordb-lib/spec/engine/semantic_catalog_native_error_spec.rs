use super::*;
use crate::engine::EngineError;
use crate::engine::v4::reader::{FormatError, MalformedInputClass};
use crate::engine::v4::semantic_catalog::SemanticCatalogReadErrorClassV1 as Class;

#[test]
fn native_catalog_engine_resource_refusal_remains_resource_limit() {
  let error = authority_error(FirstAuthorityPublicationErrorV1::Engine(EngineError::ResourceExhausted("test refusal".into())));
  assert_eq!(error.class(), Class::ResourceLimit);
}

#[test]
fn native_catalog_engine_cancellation_remains_cancelled() {
  let error = authority_error(FirstAuthorityPublicationErrorV1::Engine(EngineError::Cancelled("test cancel".into())));
  assert_eq!(error.class(), Class::Cancelled);
}

#[test]
fn native_catalog_engine_corruption_does_not_become_a_transient_outage() {
  let error = authority_error(FirstAuthorityPublicationErrorV1::Engine(EngineError::CorruptEntry { offset: 42, reason: "test".into() }));
  assert_eq!(error.class(), Class::Corrupt);
}

#[test]
fn native_catalog_format_origin_and_known_read_errors_preserve_their_categories() {
  for wrapped in [false, true] {
    for allocation in [false, true] {
      let format = if allocation {
        FormatError::allocation_failure("test_allocation", "host refusal")
      } else {
        FormatError::new(MalformedInputClass::AllocationAmplification, "test_allocation", "deterministic malformed limit")
      };
      let source = if wrapped {
        FirstAuthorityPublicationErrorV1::Header(DatabaseHeaderPublicationErrorV4::Format(format))
      } else {
        FirstAuthorityPublicationErrorV1::Format(format)
      };
      let error = authority_error(source);
      assert_eq!(error.class(), if allocation { Class::ResourceLimit } else { Class::Corrupt });
      assert_eq!(error.code(), "test_allocation");
    }
  }
  for (code, expected) in [
    ("first_authority_readback_allocation", Class::ResourceLimit),
    ("first_authority_system_file_allocation", Class::ResourceLimit),
    ("immutable_semantic_object_allocation", Class::ResourceLimit),
    ("captured_authority_cancelled", Class::Cancelled),
    ("first_authority_readback_io", Class::Unavailable),
    ("immutable_semantic_object_identity", Class::Corrupt),
  ] {
    let error = publication_error(ImmutableEntityBatchPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::Invalid {
      code,
      message: "test".into(),
    }));
    assert_eq!(error.code(), code);
    assert_eq!(error.class(), expected);
  }
}

#[test]
fn native_catalog_batch_allocation_failure_is_not_malformed_input() {
  for (code, expected) in [
    ("immutable_semantic_object_allocation", Class::ResourceLimit),
    ("immutable_entity_batch_allocation", Class::ResourceLimit),
    ("immutable_entity_receipt_allocation", Class::ResourceLimit),
    ("immutable_entity_allocation", Class::ResourceLimit),
    ("immutable_entity_expectation_allocation", Class::ResourceLimit),
    ("immutable_semantic_object_collision", Class::Corrupt),
  ] {
    let error = publication_error(ImmutableEntityBatchPublicationErrorV1::Invalid { code, message: "test".into() });
    assert_eq!(error.code(), code);
    assert_eq!(error.class(), expected);
  }
}

#[test]
fn native_catalog_io_and_poisoned_authority_remain_unavailable() {
  let sources = [
    FirstAuthorityPublicationErrorV1::Engine(EngineError::IoError(std::io::Error::other("test"))),
    FirstAuthorityPublicationErrorV1::Engine(EngineError::DurabilityFailure("test".into())),
    FirstAuthorityPublicationErrorV1::Engine(EngineError::PostMutationDurabilityFailure("test".into())),
    FirstAuthorityPublicationErrorV1::Engine(EngineError::ShuttingDown),
    FirstAuthorityPublicationErrorV1::StateLockPoisoned,
    FirstAuthorityPublicationErrorV1::Header(DatabaseHeaderPublicationErrorV4::PublicationLockPoisoned),
  ];
  for source in sources {
    assert_eq!(authority_error(source).class(), Class::Unavailable);
  }
}

#[test]
fn native_catalog_post_publication_failure_returns_unavailable_without_claiming_completion() {
  use crate::engine::v4::database_header::decode_header_region;
  use crate::engine::v4::first_authority::ImmutableEntityBatchPublicationReceiptV1;
  use crate::engine::v4::header_publication::DatabaseHeaderObservationV4;
  // Constructed error-classification fixture, not an injected native I/O fault.
  let region = *include_bytes!("../fixtures/v4/database-header-v4/header-blake3-256-valid-ab.bin");
  let selected = decode_header_region(&region).unwrap();
  let receipt = ImmutableEntityBatchPublicationReceiptV1 {
    entities: Vec::new(),
    observation: DatabaseHeaderObservationV4 { region, selected },
    idempotent: false,
  };
  let error = publication_error(ImmutableEntityBatchPublicationErrorV1::Committed {
    code: "test_post_commit",
    message: "durable objects, failed postcondition".into(),
    receipt: Box::new(receipt),
  });
  assert_eq!(error.code(), "test_post_commit");
  assert_eq!(error.class(), Class::Unavailable);
}
