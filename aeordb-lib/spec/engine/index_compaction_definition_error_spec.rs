use super::*;
use crate::engine::v4::reader::{FormatError, MalformedInputClass};

fn allocation_error() -> FormatError {
  FormatError::allocation_failure("selector_decode_allocation", "test host refusal")
}

#[test]
fn retained_definition_compaction_format_is_retryable() {
  assert_eq!(map_format_error(allocation_error()).class(), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);
}

#[test]
fn retained_definition_compaction_batch_format_is_retryable() {
  assert_eq!(
    map_application_error(IndexBatchApplicationErrorV1::Malformed(allocation_error())).class(),
    IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection
  );
}

#[test]
fn retained_definition_compaction_authority_read_is_retryable() {
  assert_eq!(
    map_authority_read_error(FirstAuthorityPublicationErrorV1::Format(allocation_error())).class(),
    IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection
  );
}

#[test]
fn retained_definition_compaction_artifact_read_is_resource_pressure() {
  assert!(matches!(
    map_artifact_read_error(FirstAuthorityPublicationErrorV1::Format(allocation_error())),
    IndexBatchArtifactReadErrorV1::ResourcePressure(_)
  ));
}

#[test]
fn retained_manifest_read_operational_failures_are_retryable_not_corrupt() {
  for code in ["immutable_index_read_allocation", "first_authority_readback_io"] {
    let error = FirstAuthorityPublicationErrorV1::Invalid { code, message: "test read failure".to_owned() };
    assert_eq!(map_authority_read_error(error).class(), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);
    let error = FirstAuthorityPublicationErrorV1::Invalid { code, message: "test read failure".to_owned() };
    let mapped = map_artifact_read_error(error);
    if code == "immutable_index_read_allocation" {
      assert!(matches!(mapped, IndexBatchArtifactReadErrorV1::ResourcePressure(_)));
    } else {
      assert!(matches!(mapped, IndexBatchArtifactReadErrorV1::Operational(_)));
    }
    let error = FirstAuthorityPublicationErrorV1::Invalid { code, message: "test read failure".to_owned() };
    assert_eq!(classify_authority_publication_error(&error), IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection);
  }
}

#[test]
fn retained_definition_compaction_publication_preserves_failure_boundary() {
  for boundary in [
    IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    IndexGenerationPublicationFailureBoundaryV1::PointerCommitUnknown,
    IndexGenerationPublicationFailureBoundaryV1::SuccessorPointerVisible,
  ] {
    let errors = [
      FrozenIndexGenerationPublicationErrorV1::Format { source: allocation_error(), boundary },
      FrozenIndexGenerationPublicationErrorV1::Authority { source: FirstAuthorityPublicationErrorV1::Format(allocation_error()), boundary },
      FrozenIndexGenerationPublicationErrorV1::ActivePointer {
        source: IndexActivePointerPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::Format(allocation_error())),
        boundary,
      },
    ];
    let expected = if boundary == IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained {
      IndexRuntimeCompactionErrorClassV1::RetryableBeforeSelection
    } else {
      IndexRuntimeCompactionErrorClassV1::CommitUnknown
    };
    for error in errors {
      assert_eq!(map_publication_error(error).class(), expected);
    }
  }
}

#[test]
fn retained_definition_compaction_deterministic_bounds_are_not_host_pressure() {
  let malformed = || FormatError::new(MalformedInputClass::AllocationAmplification, "selector_decode_allocation", "deterministic cap");
  assert_eq!(map_format_error(malformed()).class(), IndexRuntimeCompactionErrorClassV1::Corrupt);
  assert_eq!(
    map_application_error(IndexBatchApplicationErrorV1::Malformed(malformed())).class(),
    IndexRuntimeCompactionErrorClassV1::Corrupt
  );
  assert_eq!(
    map_authority_read_error(FirstAuthorityPublicationErrorV1::Format(malformed())).class(),
    IndexRuntimeCompactionErrorClassV1::Corrupt
  );
  assert!(matches!(
    map_artifact_read_error(FirstAuthorityPublicationErrorV1::Format(malformed())),
    IndexBatchArtifactReadErrorV1::Corrupt(_)
  ));
  assert_eq!(
    map_publication_error(FrozenIndexGenerationPublicationErrorV1::Format {
      source: malformed(),
      boundary: IndexGenerationPublicationFailureBoundaryV1::PriorAuthorityRetained,
    })
    .class(),
    IndexRuntimeCompactionErrorClassV1::Corrupt
  );
}
