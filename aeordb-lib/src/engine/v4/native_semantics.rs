//! Exact executable native identities, separate from structural reader support.
use super::dependency::DependencyRecordV1;

/// Frozen Round 9 semantic bundles. Unknown identities are retainable but
/// cannot execute this implementation. Hash framing and independent inputs are
/// in spec/fixtures/v4/native-semantic-conformance-v1/README.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSemanticComponentV1 {
  MimeRouter,
  RawJson,
  NativeSuite,
  RegexSelector,
}

impl NativeSemanticComponentV1 {
  pub const ALL: [Self; 4] = [Self::MimeRouter, Self::RawJson, Self::NativeSuite, Self::RegexSelector];

  pub const fn dependency_id(self) -> &'static str {
    match self {
      Self::MimeRouter => "/org/aeordev/aeordb/native/mime-router-v1",
      Self::RawJson => "/org/aeordev/aeordb/native/raw-json-v1",
      Self::NativeSuite => "/org/aeordev/aeordb/native/native-suite-v1",
      Self::RegexSelector => "/org/aeordev/aeordb/native/aeor-regex-v1",
    }
  }

  pub const fn role(self) -> u16 {
    match self {
      Self::MimeRouter => 3,
      Self::RawJson => 1,
      Self::NativeSuite => 1,
      Self::RegexSelector => 4,
    }
  }

  pub const fn fingerprint(self) -> [u8; 32] {
    match self {
      Self::MimeRouter => [
        0xc7, 0x2d, 0x35, 0xd7, 0x4d, 0x0b, 0xca, 0xdc, 0x28, 0xac, 0xe7, 0x0b, 0x63, 0x92, 0xd1, 0x69, 0x9f, 0x9a, 0x80, 0x3a, 0x14, 0x92,
        0x88, 0x52, 0xbf, 0x6e, 0x6b, 0x9c, 0xb2, 0x90, 0xf7, 0x29,
      ],
      Self::RawJson => [
        0x25, 0xa5, 0x72, 0x79, 0x98, 0x3b, 0x4d, 0xbc, 0x4f, 0xfd, 0x4d, 0xc6, 0xc3, 0x57, 0x66, 0x23, 0xd5, 0xd2, 0xf9, 0xfd, 0x0e, 0x1b,
        0xf3, 0xa9, 0xc8, 0x70, 0x9a, 0x80, 0xae, 0x30, 0x4a, 0x68,
      ],
      Self::NativeSuite => [
        0xed, 0x95, 0x2f, 0x3f, 0x85, 0x14, 0xd9, 0xbf, 0xb6, 0xf6, 0x87, 0x93, 0x48, 0xe9, 0xb3, 0xd2, 0x43, 0xcc, 0x97, 0x14, 0xc5, 0x83,
        0xce, 0x20, 0xfb, 0xe5, 0x9c, 0xf2, 0x41, 0x21, 0xd8, 0xbb,
      ],
      Self::RegexSelector => [
        0xea, 0xe5, 0x08, 0x00, 0xc6, 0x58, 0xf8, 0x04, 0xc4, 0xbd, 0xa0, 0xd0, 0xb2, 0x03, 0x31, 0x39, 0x9d, 0xb7, 0x7f, 0x07, 0x85, 0x45,
        0x75, 0x9f, 0x2f, 0x20, 0xf5, 0x2b, 0x88, 0xfe, 0x34, 0xd6,
      ],
    }
  }

  pub const fn dependency_record(self) -> DependencyRecordV1<'static> {
    DependencyRecordV1 {
      kind: 2,
      role: self.role(),
      flags: 0,
      abi: 0,
      executor_profile: 1,
      fingerprint_semantics: 2,
      artifact_kind: 0,
      artifact_length: 0,
      fingerprint: self.fingerprint(),
      dependency_id: self.dependency_id(),
      version: "1.0.0",
    }
  }

  pub fn matches_dependency(self, dependency: &DependencyRecordV1<'_>) -> bool {
    *dependency == self.dependency_record()
  }
}
