use aeordb::engine::v4::index_scope_ordinal_checkpoint::{
  MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1, ScopeOrdinalPendingClaimWriteV1, decode_scope_ordinal_claim_resume_v1,
  encode_scope_ordinal_claim_resume_v1,
};
use aeordb::engine::HashAlgorithm;

fn hash(algorithm: HashAlgorithm, byte: u8) -> Vec<u8> {
  vec![byte; algorithm.hash_length()]
}

fn fixture(name: &str) -> Vec<u8> {
  let hex = match name {
    "blake3-256" => include_str!("../fixtures/v4/scope-ordinal-resume-v1/sorc-blake3-256-valid.hex"),
    "sha512" => include_str!("../fixtures/v4/scope-ordinal-resume-v1/sorc-sha512-valid.hex"),
    _ => panic!("unknown scope ordinal resume fixture {name}"),
  };
  hex::decode(hex.trim()).unwrap()
}

fn claims(algorithm: HashAlgorithm) -> Vec<ScopeOrdinalPendingClaimWriteV1<'static>> {
  let first = Box::leak(hash(algorithm, 0x31).into_boxed_slice());
  let second = Box::leak(hash(algorithm, 0x32).into_boxed_slice());
  vec![
    ScopeOrdinalPendingClaimWriteV1 {
      operation_id: [0x11; 16],
      request_fingerprint: first,
      document_ordinal: 7,
      source_publication_sequence: 101,
    },
    ScopeOrdinalPendingClaimWriteV1 {
      operation_id: [0x12; 16],
      request_fingerprint: second,
      document_ordinal: 9,
      source_publication_sequence: 103,
    },
  ]
}

#[test]
fn claim_resume_round_trips_every_database_hash_profile() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    let encoded = encode_scope_ordinal_claim_resume_v1(algorithm, 100, &claims(algorithm)).unwrap();
    let decoded = decode_scope_ordinal_claim_resume_v1(&encoded, algorithm).unwrap();
    assert_eq!(decoded.applied_through_sequence, 100);
    assert_eq!(decoded.claims.len(), 2);
    assert_eq!(decoded.claims[0].operation_id, [0x11; 16]);
    assert_eq!(decoded.claims[0].document_ordinal, 7);
    assert_eq!(decoded.claims[0].source_publication_sequence, 101);
    assert_eq!(decoded.claims[1].request_fingerprint, hash(algorithm, 0x32));
  }
}

#[test]
fn exact_blake3_and_sha512_bytes_are_frozen_independently() {
  let blake3 = encode_scope_ordinal_claim_resume_v1(HashAlgorithm::Blake3_256, 100, &claims(HashAlgorithm::Blake3_256)).unwrap();
  let sha512 = encode_scope_ordinal_claim_resume_v1(HashAlgorithm::Sha512, 100, &claims(HashAlgorithm::Sha512)).unwrap();

  assert_eq!(blake3, fixture("blake3-256"));
  assert_eq!(sha512, fixture("sha512"));
}

#[test]
fn malformed_headers_lengths_order_and_claim_fields_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let valid = encode_scope_ordinal_claim_resume_v1(algorithm, 100, &claims(algorithm)).unwrap();
  for mut malformed in [
    {
      let mut bytes = valid.clone();
      bytes[0] ^= 1;
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes[6..8].copy_from_slice(&31u16.to_le_bytes());
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes[26] = 1;
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes.push(0);
      bytes
    },
  ] {
    assert!(decode_scope_ordinal_claim_resume_v1(&malformed, algorithm).is_err());
    malformed.clear();
  }

  let fingerprint = hash(algorithm, 0x41);
  let cases = [
    ScopeOrdinalPendingClaimWriteV1 {
      operation_id: [0; 16],
      request_fingerprint: &fingerprint,
      document_ordinal: 1,
      source_publication_sequence: 101,
    },
    ScopeOrdinalPendingClaimWriteV1 {
      operation_id: [1; 16],
      request_fingerprint: &fingerprint[..31],
      document_ordinal: 1,
      source_publication_sequence: 101,
    },
    ScopeOrdinalPendingClaimWriteV1 {
      operation_id: [1; 16],
      request_fingerprint: &fingerprint,
      document_ordinal: 0,
      source_publication_sequence: 101,
    },
    ScopeOrdinalPendingClaimWriteV1 {
      operation_id: [1; 16],
      request_fingerprint: &fingerprint,
      document_ordinal: 1,
      source_publication_sequence: 100,
    },
  ];
  for claim in cases {
    assert!(encode_scope_ordinal_claim_resume_v1(algorithm, 100, &[claim]).is_err());
  }

  let mut reversed = claims(algorithm);
  reversed.reverse();
  assert!(encode_scope_ordinal_claim_resume_v1(algorithm, 100, &reversed).is_err());
  let duplicate = vec![claims(algorithm)[0], claims(algorithm)[0]];
  assert!(encode_scope_ordinal_claim_resume_v1(algorithm, 100, &duplicate).is_err());

  let row_length = 32 + algorithm.hash_length();
  for malformed in [
    {
      let mut bytes = valid.clone();
      bytes[32..48].fill(0);
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes[48..48 + algorithm.hash_length()].fill(0);
      bytes
    },
    {
      let mut bytes = valid.clone();
      let ordinal = 48 + algorithm.hash_length();
      bytes[ordinal..ordinal + 8].fill(0);
      bytes
    },
    {
      let mut bytes = valid.clone();
      let sequence = 56 + algorithm.hash_length();
      bytes[sequence..sequence + 8].copy_from_slice(&100u64.to_le_bytes());
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes[32 + row_length..48 + row_length].copy_from_slice(&[0x10; 16]);
      bytes
    },
    {
      let mut bytes = valid.clone();
      bytes[12..16].copy_from_slice(&(MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1 + 1).to_le_bytes());
      bytes
    },
  ] {
    assert!(decode_scope_ordinal_claim_resume_v1(&malformed, algorithm).is_err());
  }

  assert!(decode_scope_ordinal_claim_resume_v1(&valid, HashAlgorithm::Sha512).is_err());
}

#[test]
fn hard_claim_limit_accepts_the_boundary_and_rejects_one_more_before_encoding() {
  let algorithm = HashAlgorithm::Blake3_256;
  let fingerprint = hash(algorithm, 0x51);
  let mut claims = Vec::new();
  for index in 1u32..=MAX_SCOPE_ORDINAL_PENDING_CLAIMS_V1 {
    let mut operation_id = [0; 16];
    operation_id[12..].copy_from_slice(&index.to_be_bytes());
    claims.push(ScopeOrdinalPendingClaimWriteV1 {
      operation_id,
      request_fingerprint: &fingerprint,
      document_ordinal: u64::from(index),
      source_publication_sequence: 1_000 + u64::from(index),
    });
  }
  let boundary = encode_scope_ordinal_claim_resume_v1(algorithm, 999, &claims).unwrap();
  assert_eq!(decode_scope_ordinal_claim_resume_v1(&boundary, algorithm).unwrap().claims.len(), claims.len());

  let mut operation_id = [0xff; 16];
  operation_id[15] = 0xfe;
  claims.push(ScopeOrdinalPendingClaimWriteV1 {
    operation_id,
    request_fingerprint: &fingerprint,
    document_ordinal: 99_999,
    source_publication_sequence: 99_999,
  });
  assert!(encode_scope_ordinal_claim_resume_v1(algorithm, 999, &claims).is_err());
}
