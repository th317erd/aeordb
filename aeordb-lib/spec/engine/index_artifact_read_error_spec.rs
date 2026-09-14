use super::*;

#[test]
fn retained_artifact_read_io_is_unavailable_not_corrupt() {
  let error = map_first_authority_error(FirstAuthorityPublicationErrorV1::Invalid {
    code: "first_authority_readback_io",
    message: "test positional read failure".to_owned(),
  });
  assert_eq!(error.class(), NativeSelectedArtifactCursorErrorClassV1::Unavailable);
  assert_eq!(error.code(), "first_authority_readback_io");
}

#[test]
fn retained_artifact_format_and_resource_errors_keep_their_categories() {
  let pressure =
    map_first_authority_error(FirstAuthorityPublicationErrorV1::Format(FormatError::allocation_failure("allocation", "host refusal")));
  assert_eq!(pressure.class(), NativeSelectedArtifactCursorErrorClassV1::ResourceLimit);
  let malformed = map_first_authority_error(FirstAuthorityPublicationErrorV1::Format(FormatError::new(
    super::super::reader::MalformedInputClass::AllocationAmplification,
    "allocation",
    "deterministic bound",
  )));
  assert_eq!(malformed.class(), NativeSelectedArtifactCursorErrorClassV1::Corrupt);
  let representation = map_first_authority_error(FirstAuthorityPublicationErrorV1::Invalid {
    code: "immutable_index_representation",
    message: "invalid stored bytes".to_owned(),
  });
  assert_eq!(representation.class(), NativeSelectedArtifactCursorErrorClassV1::Corrupt);
}
