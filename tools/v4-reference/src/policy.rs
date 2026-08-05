use crate::core::HashProfile;

const POLICY_LENGTH: usize = 128;
const WASM32_ADDRESS_SPACE: u64 = 1u64 << 32;
const WASM_PAGE_SIZE: u64 = 64 * 1_024;

#[derive(Clone, Copy)]
pub enum PolicyFormat {
  InvocationPolicyV1,
}

impl PolicyFormat {
  pub fn id(self) -> &'static str {
    "invocation-policy-v1"
  }

  pub fn family(self) -> &'static str {
    "InvocationPolicyV1"
  }
}

#[derive(Clone)]
pub struct PolicyFixtureCase {
  pub id: &'static str,
  pub format: PolicyFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PolicyKind {
  Native,
  PureWasm,
  LegacyWasm,
}

impl PolicyKind {
  fn name(self) -> &'static str {
    match self {
      Self::Native => "native",
      Self::PureWasm => "pure-wasm32",
      Self::LegacyWasm => "legacy-wasm32",
    }
  }
}

pub fn fixture_cases() -> Vec<PolicyFixtureCase> {
  let mut cases = Vec::with_capacity(4);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for kind in [PolicyKind::Native, PolicyKind::PureWasm, PolicyKind::LegacyWasm] {
      cases.push(PolicyFixtureCase {
        id: fixture_id(profile, kind),
        format: PolicyFormat::InvocationPolicyV1,
        profile,
        expected: expected(kind),
        relation: Some(match kind {
          PolicyKind::Native => "executor-profile:aeordb-native-deterministic-v1",
          PolicyKind::PureWasm => "executor-profile:wasmi-0.42.1-aeordb-pure-v1",
          PolicyKind::LegacyWasm => "executor-profile:wasmi-0.42.1-aeordb-legacy-stubs-v0",
        }),
        canonical_key: None,
        bytes: build_policy(kind),
      });
    }
  }
  cases
}

pub fn observe(_profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_policy(bytes) {
    Ok(kind) => (format!("policy:{}", kind.name()), None),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines() -> Vec<String> {
  vec![
    "policy +0x000 len 16: magic, schema, length, and flags".to_string(),
    "policy +0x010 len 8: backend, host, and semantic IDs".to_string(),
    "policy +0x018 len 56: u64 request/response/resource limits".to_string(),
    "policy +0x050 len 28: u32 structural and executor limits".to_string(),
    "policy +0x06c len 20: reserved zero".to_string(),
  ]
}

pub(crate) fn build_policy(kind: PolicyKind) -> Vec<u8> {
  let mut value = vec![0u8; POLICY_LENGTH];
  value[0..4].copy_from_slice(b"AIVP");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, POLICY_LENGTH as u16);
  put_u32(&mut value, 8, POLICY_LENGTH as u32);
  put_u16(&mut value, 20, 1);
  put_u16(&mut value, 22, 1);
  put_u64(&mut value, 32, 4 * 1_024 * 1_024);
  put_u64(&mut value, 64, 100_000);
  put_u64(&mut value, 72, 64 * 1_024);
  put_u32(&mut value, 80, 32);
  put_u32(&mut value, 84, 65_535);
  put_u32(&mut value, 100, 4_096);
  put_u32(&mut value, 104, 256);

  match kind {
    PolicyKind::Native => {
      put_u16(&mut value, 16, 1);
    }
    PolicyKind::PureWasm | PolicyKind::LegacyWasm => {
      put_u16(&mut value, 16, 2);
      put_u16(&mut value, 18, if matches!(kind, PolicyKind::PureWasm) { 1 } else { 2 });
      put_u64(&mut value, 24, 8 * 1_024 * 1_024);
      put_u64(&mut value, 40, 64 * 1_024 * 1_024);
      put_u64(&mut value, 48, 50_000_000);
      put_u64(&mut value, 56, 100_000);
      put_u32(&mut value, 88, 1);
      put_u32(&mut value, 92, 1);
      put_u32(&mut value, 96, 1);
    }
  }
  value
}

pub(crate) fn decode_policy(value: &[u8]) -> Result<PolicyKind, &'static str> {
  if value.len() != POLICY_LENGTH {
    return Err("policy_length");
  }
  if &value[0..4] != b"AIVP"
    || read_u16(value, 4)? != 1
    || read_u16(value, 6)? as usize != POLICY_LENGTH
    || read_u32(value, 8)? as usize != POLICY_LENGTH
  {
    return Err("policy_envelope");
  }
  if read_u32(value, 12)? != 0 || value[108..128].iter().any(|byte| *byte != 0) {
    return Err("policy_reserved");
  }
  if read_u16(value, 20)? != 1 || read_u16(value, 22)? != 1 {
    return Err("policy_semantics");
  }

  let request = read_u64(value, 24)?;
  let response = read_u64(value, 32)?;
  let memory = read_u64(value, 40)?;
  let fuel = read_u64(value, 48)?;
  let table_elements = read_u64(value, 56)?;
  let structure_nodes = read_u64(value, 64)?;
  let scalar_bytes = read_u64(value, 72)?;
  let structure_depth = read_u32(value, 80)?;
  let container_members = read_u32(value, 84)?;
  let instances = read_u32(value, 88)?;
  let memories = read_u32(value, 92)?;
  let tables = read_u32(value, 96)?;
  let stack = read_u32(value, 100)?;
  let recursion = read_u32(value, 104)?;
  if [response, structure_nodes, scalar_bytes].contains(&0)
    || [structure_depth, container_members, stack, recursion].contains(&0)
    || [response, structure_nodes, scalar_bytes].contains(&u64::MAX)
    || [structure_depth, container_members, stack, recursion].contains(&u32::MAX)
  {
    return Err("policy_common_limits");
  }

  match (read_u16(value, 16)?, read_u16(value, 18)?) {
    (1, 0) => {
      if request != 0
        || [memory, fuel, table_elements].iter().any(|limit| *limit != 0)
        || [instances, memories, tables].iter().any(|limit| *limit != 0)
      {
        return Err("policy_native_context");
      }
      Ok(PolicyKind::Native)
    }
    (2, host @ (1 | 2)) => {
      if [request, memory, fuel, table_elements].contains(&0)
        || [request, memory, fuel, table_elements].contains(&u64::MAX)
        || [instances, memories, tables].contains(&0)
        || [instances, memories, tables].contains(&u32::MAX)
        || memory > WASM32_ADDRESS_SPACE
        || memory % WASM_PAGE_SIZE != 0
      {
        return Err("policy_wasm_context");
      }
      Ok(if host == 1 { PolicyKind::PureWasm } else { PolicyKind::LegacyWasm })
    }
    (1 | 2, _) => Err("policy_host_profile"),
    _ => Err("policy_backend"),
  }
}

fn fixture_id(profile: HashProfile, kind: PolicyKind) -> &'static str {
  match (profile, kind) {
    (HashProfile::Blake3_256, PolicyKind::Native) => "aivp-blake3-256-native-valid",
    (HashProfile::Blake3_256, PolicyKind::PureWasm) => "aivp-blake3-256-pure-wasm-valid",
    (HashProfile::Blake3_256, PolicyKind::LegacyWasm) => "aivp-blake3-256-legacy-wasm-valid",
    (HashProfile::Sha512, PolicyKind::Native) => "aivp-sha512-native-valid",
    (HashProfile::Sha512, PolicyKind::PureWasm) => "aivp-sha512-pure-wasm-valid",
    (HashProfile::Sha512, PolicyKind::LegacyWasm) => "aivp-sha512-legacy-wasm-valid",
  }
}

fn expected(kind: PolicyKind) -> &'static str {
  match kind {
    PolicyKind::Native => "policy:native",
    PolicyKind::PureWasm => "policy:pure-wasm32",
    PolicyKind::LegacyWasm => "policy:legacy-wasm32",
  }
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  let raw = bytes.get(offset..offset + 2).ok_or("truncated")?;
  Ok(u16::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let raw = bytes.get(offset..offset + 4).ok_or("truncated")?;
  Ok(u32::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, &'static str> {
  let raw = bytes.get(offset..offset + 8).ok_or("truncated")?;
  Ok(u64::from_le_bytes(raw.try_into().map_err(|_| "truncated")?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn policy_fixtures_match_expected_backends() {
    for case in fixture_cases() {
      assert_eq!(observe(case.profile, &case.bytes).0, case.expected, "fixture {}", case.id);
    }
  }

  #[test]
  fn every_policy_byte_is_part_of_structure_or_enclosing_identity() {
    for case in fixture_cases() {
      let original_digest = case.profile.digest(&case.bytes);
      for index in 0..case.bytes.len() {
        let mut mutated = case.bytes.clone();
        mutated[index] ^= 1;
        let observed = observe(case.profile, &mutated).0;
        assert!(
          observed.starts_with("error:") || case.profile.digest(&mutated) != original_digest,
          "fixture {} byte {index} was not protected",
          case.id
        );
      }
    }
  }

  #[test]
  fn policy_decoder_rejects_framing_semantics_and_context_mismatches() {
    let native = build_policy(PolicyKind::Native);
    for length in [0, 4, 127] {
      assert_eq!(decode_policy(&native[..length]).err(), Some("policy_length"));
    }

    let mut reserved = native.clone();
    reserved[108] = 1;
    assert_eq!(decode_policy(&reserved).err(), Some("policy_reserved"));

    let mut semantics = native.clone();
    put_u16(&mut semantics, 20, 2);
    assert_eq!(decode_policy(&semantics).err(), Some("policy_semantics"));

    let mut native_request = native.clone();
    put_u64(&mut native_request, 24, 1);
    assert_eq!(decode_policy(&native_request).err(), Some("policy_native_context"));

    let mut wasm_host = build_policy(PolicyKind::PureWasm);
    put_u16(&mut wasm_host, 18, 0);
    assert_eq!(decode_policy(&wasm_host).err(), Some("policy_host_profile"));
  }

  #[test]
  fn wasm_memory_and_finite_limit_boundaries_fail_closed() {
    let wasm = build_policy(PolicyKind::PureWasm);

    let mut unaligned = wasm.clone();
    put_u64(&mut unaligned, 40, WASM_PAGE_SIZE + 1);
    assert_eq!(decode_policy(&unaligned).err(), Some("policy_wasm_context"));

    let mut too_large = wasm.clone();
    put_u64(&mut too_large, 40, WASM32_ADDRESS_SPACE + WASM_PAGE_SIZE);
    assert_eq!(decode_policy(&too_large).err(), Some("policy_wasm_context"));

    let mut unlimited = wasm.clone();
    put_u64(&mut unlimited, 48, u64::MAX);
    assert_eq!(decode_policy(&unlimited).err(), Some("policy_wasm_context"));

    let mut zero_common = wasm.clone();
    put_u64(&mut zero_common, 64, 0);
    assert_eq!(decode_policy(&zero_common).err(), Some("policy_common_limits"));
  }
}
