//! Independent Round 16 ASEM dependency definitions and owner bindings.

use super::{
  CoreFixtureCase, CoreFormat, HashProfile, build_semantic_envelope, put_u16, put_u32, semantic_catalog_lookup_digest, semantic_object_id,
};

pub(super) fn fixture_cases(profile: HashProfile) -> Vec<CoreFixtureCase> {
  let identifiers = match profile {
    HashProfile::Blake3_256 => [
      ["asem-blake3-256-wasm-parser-definition-valid", "asem-blake3-256-wasm-parser-binding-valid"],
      ["asem-blake3-256-wasm-mapper-definition-valid", "asem-blake3-256-wasm-mapper-binding-valid"],
      ["asem-blake3-256-native-dependency-definition-valid", "asem-blake3-256-native-dependency-binding-valid"],
    ],
    HashProfile::Sha512 => [
      ["asem-sha512-wasm-parser-definition-valid", "asem-sha512-wasm-parser-binding-valid"],
      ["asem-sha512-wasm-mapper-definition-valid", "asem-sha512-wasm-mapper-binding-valid"],
      ["asem-sha512-native-dependency-definition-valid", "asem-sha512-native-dependency-binding-valid"],
    ],
  };
  let mut cases = Vec::new();
  for (index, identifiers) in identifiers.into_iter().enumerate() {
    let class = if index == 2 { 7 } else { 6 };
    let relation = if class == 7 { "native:parser-resolution" } else { "wasm:mapper-binary-canonical-v1" };
    let table = crate::dependency::fixture_cases()
      .into_iter()
      .find(|case| case.profile == profile && case.relation == Some(relation))
      .expect("independent ADPT dependency fixture");
    let mut canonical_record = table.bytes[32..].to_vec();
    if index == 0 {
      // Parser and mapper share every artifact byte and fingerprint. Only the
      // declared role/ABI differ; their complete definition IDs must differ.
      put_u16(&mut canonical_record, 6, 1);
      put_u16(&mut canonical_record, 12, 3);
    }
    let mut preimage = if class == 6 {
      b"aeordb.semantic.executable-dependency-definition.v1\0".to_vec()
    } else {
      b"aeordb.semantic.native-dependency-definition.v1\0".to_vec()
    };
    preimage.extend_from_slice(&canonical_record);
    let semantic_id = profile.digest(&preimage);
    let mut body = vec![0; 16 + profile.width() + canonical_record.len()];
    put_u16(&mut body, 0, class);
    put_u16(&mut body, 2, 1);
    body[8..8 + profile.width()].copy_from_slice(&semantic_id);
    put_u32(&mut body, 8 + profile.width(), canonical_record.len() as u32);
    body[16 + profile.width()..].copy_from_slice(&canonical_record);
    let definition = build_semantic_envelope(4, 1, body);
    let definition_id = semantic_object_id(profile, 4, &definition);
    cases.push(CoreFixtureCase {
      id: identifiers[0],
      format: CoreFormat::SemanticObjectV1,
      profile,
      expected: if class == 6 { "semantic:definition:class=6" } else { "semantic:definition:class=7" },
      relation: Some("round-16:complete-dependency-record-identity"),
      canonical_key: Some(hex::encode(&definition_id)),
      bytes: definition,
    });

    let lookup = semantic_catalog_lookup_digest(profile, class, &semantic_id);
    let record_length = 8 + 3 * profile.width();
    let mut body = vec![0; 16 + profile.width() + record_length];
    put_u32(&mut body, 4, 1);
    body[8..8 + profile.width()].copy_from_slice(&lookup);
    put_u32(&mut body, 8 + profile.width(), record_length as u32);
    let record = 16 + profile.width();
    put_u16(&mut body, record, class);
    put_u32(&mut body, record + 4, profile.width() as u32);
    body[record + 8..record + 8 + profile.width()].copy_from_slice(&semantic_id);
    body[record + 8 + profile.width()..record + 8 + 2 * profile.width()].copy_from_slice(&definition_id);
    body[record + 8 + 2 * profile.width()..].copy_from_slice(&semantic_id);
    let leaf = build_semantic_envelope(2, 1, body);
    cases.push(CoreFixtureCase {
      id: identifiers[1],
      format: CoreFormat::SemanticObjectV1,
      profile,
      expected: "semantic:catalog-leaf:records=1",
      relation: Some(identifiers[0]),
      canonical_key: Some(hex::encode(semantic_object_id(profile, 2, &leaf))),
      bytes: leaf,
    });
  }
  cases
}
