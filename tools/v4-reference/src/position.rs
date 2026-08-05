use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use crate::config;
use crate::core::HashProfile;

const MAX_DECODED_LENGTH: usize = 1_024 * 1_024;
const MAX_ENCODED_LENGTH: usize = (MAX_DECODED_LENGTH * 4).div_ceil(3);
const MAX_COMPONENTS: usize = 32;
const FIXED_WITHOUT_HASHES: usize = 24;

#[derive(Clone, Copy)]
pub enum PositionFormat {
  LogicalPositionV1,
}

impl PositionFormat {
  pub fn id(self) -> &'static str {
    "logical-position-v1"
  }

  pub fn family(self) -> &'static str {
    "LogicalPositionTokenV1"
  }
}

#[derive(Clone)]
pub struct PositionFixtureCase {
  pub id: &'static str,
  pub format: PositionFormat,
  pub profile: HashProfile,
  pub expected: &'static str,
  pub relation: Option<&'static str>,
  pub canonical_key: Option<String>,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteKind {
  DirectoryListing = 1,
  Query = 2,
  GlobalSearch = 3,
  AggregateGroups = 4,
}

impl RouteKind {
  fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::DirectoryListing),
      2 => Some(Self::Query),
      3 => Some(Self::GlobalSearch),
      4 => Some(Self::AggregateGroups),
      _ => None,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::DirectoryListing => "directory-listing",
      Self::Query => "query",
      Self::GlobalSearch => "global-search",
      Self::AggregateGroups => "aggregate-groups",
    }
  }
}

#[derive(Clone)]
struct Component {
  tag: u16,
  state: u8,
  payload: Vec<u8>,
}

impl Component {
  fn present(tag: u16, payload: impl Into<Vec<u8>>) -> Self {
    Self { tag, state: 0, payload: payload.into() }
  }

  fn typed_null() -> Self {
    Self { tag: 0, state: 1, payload: Vec::new() }
  }

  fn missing() -> Self {
    Self { tag: 0, state: 2, payload: Vec::new() }
  }
}

struct PositionSpec<'a> {
  route: RouteKind,
  order_definition_json: &'a str,
  namespace_root: Vec<u8>,
  file_key: Vec<u8>,
  revision: Vec<u8>,
  components: &'a [Component],
}

#[derive(Debug)]
struct DecodedPosition {
  route: RouteKind,
  decoded_length: usize,
  order_fingerprint: Vec<u8>,
  namespace_root: Vec<u8>,
  file_key: Vec<u8>,
  revision: Vec<u8>,
  tuple: Vec<u8>,
  component_count: u8,
}

pub fn fixture_cases() -> Vec<PositionFixtureCase> {
  let mut cases = Vec::with_capacity(10);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    for route in [RouteKind::DirectoryListing, RouteKind::Query, RouteKind::GlobalSearch, RouteKind::AggregateGroups] {
      let components = sample_components(route);
      let order = sample_order_definition(route);
      let bytes = encode_position(profile, sample_spec(profile, route, &order, &components));
      let decoded = decode_position(profile, &bytes).expect("sample APOS token must decode");
      cases.push(PositionFixtureCase {
        id: leak(format!("apos-{}-{}-valid", profile.label(), route.name())),
        format: PositionFormat::LogicalPositionV1,
        profile,
        expected: position_expected(&decoded, false),
        relation: Some("logical-root-and-order-bound-position:not-authorization-or-physical-plan"),
        canonical_key: None,
        bytes,
      });
    }
    let bytes = build_maximum_token(profile);
    let decoded = decode_position(profile, &bytes).expect("maximum APOS token must decode");
    cases.push(PositionFixtureCase {
      id: leak(format!("apos-{}-maximum-decoded-length-valid", profile.label())),
      format: PositionFormat::LogicalPositionV1,
      profile,
      expected: position_expected(&decoded, true),
      relation: Some("boundary:decoded-length-exactly-1048576"),
      canonical_key: None,
      bytes,
    });
  }
  cases
}

pub fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_position(profile, bytes) {
    Ok(position) => (position_expected(&position, position.decoded_length == MAX_DECODED_LENGTH).to_string(), None),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub fn annotation_lines(profile: HashProfile, token: &[u8]) -> Vec<String> {
  match decode_base64(token) {
    Ok(decoded) => vec![
      format!("token len {}: canonical unpadded base64url", token.len()),
      format!("decoded +0x000 len {}: APOS v1 logical position", decoded.len()),
      format!("decoded hash width: H={}", profile.width()),
      "decoded layout: 20 + 4H fixed fields, canonical component tuple, CRC32".to_string(),
    ],
    Err(error) => vec![format!("invalid token: {error}")],
  }
}

fn sample_spec<'a>(profile: HashProfile, route: RouteKind, order: &'a str, components: &'a [Component]) -> PositionSpec<'a> {
  PositionSpec {
    route,
    order_definition_json: order,
    namespace_root: sample_hash(profile, 0x11),
    file_key: sample_hash(profile, 0x41),
    revision: sample_hash(profile, 0x71),
    components,
  }
}

fn sample_components(route: RouteKind) -> Vec<Component> {
  match route {
    RouteKind::DirectoryListing => vec![
      Component::present(4, 0u64.to_le_bytes()),
      Component::present(3, b"guide.md".to_vec()),
      Component::present(3, b"Guide.md".to_vec()),
    ],
    RouteKind::Query => vec![Component::present(3, b"/docs/guide.md".to_vec()), Component::typed_null(), Component::missing()],
    RouteKind::GlobalSearch => vec![Component::present(6, 0.875f64.to_le_bytes()), Component::present(3, b"/docs/search.md".to_vec())],
    RouteKind::AggregateGroups => vec![Component::present(4, 42u64.to_le_bytes()), Component::present(2, b"group-a".to_vec())],
  }
}

fn sample_order_definition(route: RouteKind) -> String {
  let (sort, directories_first, name_collation, null_policy, multi_value, score) = match route {
    RouteKind::DirectoryListing => (
      r#"[{"field":"category","direction":"asc","comparator":"u64_order_v1"},{"field":"name_folded","direction":"asc","comparator":"utf8_binary_order_v1"},{"field":"name_raw","direction":"asc","comparator":"utf8_binary_order_v1"}]"#,
      "always",
      "aeor-listing-lowercase-then-raw-utf8-v1",
      "not-applicable",
      "not-applicable",
      "not-applicable",
    ),
    RouteKind::Query => (
      r#"[{"field":"@path","direction":"asc","comparator":"utf8_binary_order_v1"},{"field":"optional","direction":"asc","comparator":"null"},{"field":"missing","direction":"asc","comparator":"missing"}]"#,
      "not-applicable",
      "not-applicable",
      "present-null-missing-only-present-reverses",
      "minimum-ascending-maximum-descending",
      "not-applicable",
    ),
    RouteKind::GlobalSearch => (
      r#"[{"field":"@score","direction":"desc","comparator":"f64_finite_order_v1"},{"field":"@path","direction":"asc","comparator":"utf8_binary_order_v1"}]"#,
      "not-applicable",
      "not-applicable",
      "present-null-missing-only-present-reverses",
      "minimum-ascending-maximum-descending",
      "corrected-finite-score-v1",
    ),
    RouteKind::AggregateGroups => (
      r#"[{"field":"@count","direction":"desc","comparator":"u64_order_v1"},{"field":"group_tuple","direction":"asc","comparator":"bytes_binary_order_v1"}]"#,
      "not-applicable",
      "not-applicable",
      "present-null-missing-only-present-reverses",
      "minimum-ascending-maximum-descending",
      "not-applicable",
    ),
  };
  let semantics = format!(
    "route={};sort={sort};directories={directories_first};collation={name_collation};nulls={null_policy};multi={multi_value};score={score}",
    route as u16
  );
  let fingerprint = blake3::hash(semantics.as_bytes()).to_hex();
  format!(
    r#"{{"default_ties":["canonical_path_asc","FileKey_asc","RecordRevisionHash_asc"],"directories_first":"{directories_first}","multi_value_selector":"{multi_value}","name_collation":"{name_collation}","null_missing_policy":"{null_policy}","route_kind":{},"score_semantics":"{score}","semantic_fingerprints":["{fingerprint}"],"sort":{sort}}}"#,
    route as u16
  )
}

fn maximum_order_definition() -> String {
  let mut sort = String::from("[");
  for index in 0..MAX_COMPONENTS {
    if index != 0 {
      sort.push(',');
    }
    sort.push_str(&format!(r#"{{"comparator":"bytes_binary_order_v1","direction":"asc","field":"field-{index:02}"}}"#));
  }
  sort.push(']');
  let semantics = blake3::hash(sort.as_bytes()).to_hex();
  format!(
    r#"{{"default_ties":["canonical_path_asc","FileKey_asc","RecordRevisionHash_asc"],"directories_first":"not-applicable","multi_value_selector":"minimum-ascending-maximum-descending","name_collation":"not-applicable","null_missing_policy":"present-null-missing-only-present-reverses","route_kind":2,"score_semantics":"not-applicable","semantic_fingerprints":["{semantics}"],"sort":{sort}}}"#
  )
}

fn build_maximum_token(profile: HashProfile) -> Vec<u8> {
  let tuple_length = MAX_DECODED_LENGTH - FIXED_WITHOUT_HASHES - 4 * profile.width();
  let small_components_length = (MAX_COMPONENTS - 1) * 9;
  let final_payload_length = tuple_length - small_components_length - 8;
  let mut components = Vec::with_capacity(MAX_COMPONENTS);
  for _ in 0..MAX_COMPONENTS - 1 {
    components.push(Component::present(8, vec![0]));
  }
  components.push(Component::present(2, vec![0xa5; final_payload_length]));
  let order = maximum_order_definition();
  let token = encode_position(profile, sample_spec(profile, RouteKind::Query, &order, &components));
  assert_eq!(decode_base64(&token).expect("maximum token base64").len(), MAX_DECODED_LENGTH);
  token
}

fn encode_position(profile: HashProfile, spec: PositionSpec<'_>) -> Vec<u8> {
  let h = profile.width();
  let tuple = encode_components(spec.components);
  let order_fingerprint =
    order_fingerprint(profile, spec.route, spec.order_definition_json).expect("sample order definition must be canonical");
  let total_length = FIXED_WITHOUT_HASHES + 4 * h + tuple.len();
  let mut decoded = vec![0u8; total_length];
  decoded[..4].copy_from_slice(b"APOS");
  put_u16(&mut decoded, 4, 1);
  put_u16(&mut decoded, 6, spec.route as u16);
  put_u32(&mut decoded, 8, total_length as u32);
  put_u16(&mut decoded, 12, profile.algorithm_id());
  decoded[14] = spec.components.len() as u8;
  decoded[16..16 + h].copy_from_slice(&order_fingerprint);
  decoded[16 + h..16 + 2 * h].copy_from_slice(&spec.namespace_root);
  put_u32(&mut decoded, 16 + 2 * h, tuple.len() as u32);
  decoded[20 + 2 * h..20 + 3 * h].copy_from_slice(&spec.file_key);
  decoded[20 + 3 * h..20 + 4 * h].copy_from_slice(&spec.revision);
  decoded[20 + 4 * h..20 + 4 * h + tuple.len()].copy_from_slice(&tuple);
  let crc_offset = decoded.len() - 4;
  let crc = crc32fast::hash(&decoded[..crc_offset]);
  put_u32(&mut decoded, crc_offset, crc);
  URL_SAFE_NO_PAD.encode(decoded).into_bytes()
}

fn encode_components(components: &[Component]) -> Vec<u8> {
  let length: usize = components.iter().map(|component| 8 + component.payload.len()).sum();
  let mut bytes = Vec::with_capacity(length);
  for component in components {
    bytes.extend_from_slice(&component.tag.to_le_bytes());
    bytes.push(component.state);
    bytes.push(0);
    bytes.extend_from_slice(&(component.payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&component.payload);
  }
  bytes
}

fn order_fingerprint(profile: HashProfile, expected_route: RouteKind, definition_json: &str) -> Result<Vec<u8>, String> {
  let definition = config::canonicalize_json(definition_json)?;
  validate_order_definition(expected_route, definition_json)?;
  let mut preimage = Vec::with_capacity(25 + definition.len());
  preimage.extend_from_slice(b"aeordb.position-order.v1\0");
  preimage.extend_from_slice(&definition);
  Ok(profile.digest(&preimage))
}

fn validate_order_definition(expected_route: RouteKind, definition_json: &str) -> Result<(), String> {
  const KEYS: [&str; 9] = [
    "default_ties",
    "directories_first",
    "multi_value_selector",
    "name_collation",
    "null_missing_policy",
    "route_kind",
    "score_semantics",
    "semantic_fingerprints",
    "sort",
  ];
  const SORT_KEYS: [&str; 3] = ["comparator", "direction", "field"];
  const DEFAULT_TIES: [&str; 3] = ["canonical_path_asc", "FileKey_asc", "RecordRevisionHash_asc"];

  let value: serde_json::Value = serde_json::from_str(definition_json).map_err(|_| "INVALID_POSITION_ORDER:json".to_string())?;
  let object = value.as_object().ok_or_else(|| "INVALID_POSITION_ORDER:map".to_string())?;
  if object.len() != KEYS.len() || !KEYS.iter().all(|key| object.contains_key(*key)) {
    return Err("INVALID_POSITION_ORDER:keys".to_string());
  }
  if object.get("route_kind").and_then(serde_json::Value::as_u64) != Some(expected_route as u64) {
    return Err("INVALID_POSITION_ORDER:route".to_string());
  }

  for key in ["directories_first", "multi_value_selector", "name_collation", "null_missing_policy", "score_semantics"] {
    if object.get(key).and_then(serde_json::Value::as_str).is_none_or(str::is_empty) {
      return Err(format!("INVALID_POSITION_ORDER:{key}"));
    }
  }

  let ties =
    object.get("default_ties").and_then(serde_json::Value::as_array).ok_or_else(|| "INVALID_POSITION_ORDER:default-ties".to_string())?;
  if ties.len() != DEFAULT_TIES.len() || ties.iter().zip(DEFAULT_TIES).any(|(actual, expected)| actual.as_str() != Some(expected)) {
    return Err("INVALID_POSITION_ORDER:default-ties".to_string());
  }

  let fingerprints = object
    .get("semantic_fingerprints")
    .and_then(serde_json::Value::as_array)
    .ok_or_else(|| "INVALID_POSITION_ORDER:semantic-fingerprints".to_string())?;
  if fingerprints.is_empty()
    || fingerprints.len() > MAX_COMPONENTS
    || fingerprints.iter().any(|value| {
      value.as_str().is_none_or(|fingerprint| {
        fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
      })
    })
  {
    return Err("INVALID_POSITION_ORDER:semantic-fingerprints".to_string());
  }

  let sort = object.get("sort").and_then(serde_json::Value::as_array).ok_or_else(|| "INVALID_POSITION_ORDER:sort".to_string())?;
  if sort.is_empty() || sort.len() > MAX_COMPONENTS {
    return Err("INVALID_POSITION_ORDER:sort".to_string());
  }
  for row in sort {
    let row = row.as_object().ok_or_else(|| "INVALID_POSITION_ORDER:sort".to_string())?;
    if row.len() != SORT_KEYS.len() || !SORT_KEYS.iter().all(|key| row.contains_key(*key)) {
      return Err("INVALID_POSITION_ORDER:sort".to_string());
    }
    let field = row.get("field").and_then(serde_json::Value::as_str);
    let comparator = row.get("comparator").and_then(serde_json::Value::as_str);
    let direction = row.get("direction").and_then(serde_json::Value::as_str);
    if field.is_none_or(str::is_empty) || comparator.is_none_or(str::is_empty) || !matches!(direction, Some("asc" | "desc")) {
      return Err("INVALID_POSITION_ORDER:sort".to_string());
    }
  }
  Ok(())
}

fn decode_position(profile: HashProfile, token: &[u8]) -> Result<DecodedPosition, &'static str> {
  let decoded = decode_base64(token)?;
  let h = profile.width();
  let minimum = FIXED_WITHOUT_HASHES + 4 * h;
  if decoded.len() < minimum || decoded.len() > MAX_DECODED_LENGTH {
    return Err("INVALID_POSITION_CURSOR:length");
  }
  if &decoded[..4] != b"APOS" || read_u16(&decoded, 4)? != 1 {
    return Err("INVALID_POSITION_CURSOR:framing");
  }
  let route = RouteKind::from_id(read_u16(&decoded, 6)?).ok_or("INVALID_POSITION_CURSOR:route")?;
  let component_count = decoded[14];
  if read_u32(&decoded, 8)? as usize != decoded.len()
    || read_u16(&decoded, 12)? != profile.algorithm_id()
    || component_count as usize > MAX_COMPONENTS
    || decoded[15] != 0
  {
    return Err("INVALID_POSITION_CURSOR:header");
  }
  let crc_offset = decoded.len() - 4;
  if read_u32(&decoded, crc_offset)? != crc32fast::hash(&decoded[..crc_offset]) {
    return Err("INVALID_POSITION_CURSOR:crc");
  }
  let order_fingerprint = decoded[16..16 + h].to_vec();
  let namespace_root = decoded[16 + h..16 + 2 * h].to_vec();
  let tuple_length = read_u32(&decoded, 16 + 2 * h)? as usize;
  let file_key = decoded[20 + 2 * h..20 + 3 * h].to_vec();
  let revision = decoded[20 + 3 * h..20 + 4 * h].to_vec();
  let tuple_start = 20 + 4 * h;
  if [order_fingerprint.as_slice(), namespace_root.as_slice(), file_key.as_slice(), revision.as_slice()]
    .iter()
    .any(|hash| hash.iter().all(|byte| *byte == 0))
    || tuple_start.checked_add(tuple_length).and_then(|end| end.checked_add(4)) != Some(decoded.len())
  {
    return Err("INVALID_POSITION_CURSOR:identity-or-tuple-length");
  }
  let tuple = decoded[tuple_start..tuple_start + tuple_length].to_vec();
  validate_components(&tuple, component_count)?;
  Ok(DecodedPosition {
    route,
    decoded_length: decoded.len(),
    order_fingerprint,
    namespace_root,
    file_key,
    revision,
    tuple,
    component_count,
  })
}

fn decode_base64(token: &[u8]) -> Result<Vec<u8>, &'static str> {
  if token.is_empty() || token.len() > MAX_ENCODED_LENGTH || token.contains(&b'=') {
    return Err("INVALID_POSITION_CURSOR:base64");
  }
  let spelling = std::str::from_utf8(token).map_err(|_| "INVALID_POSITION_CURSOR:base64")?;
  let decoded = URL_SAFE_NO_PAD.decode(spelling).map_err(|_| "INVALID_POSITION_CURSOR:base64")?;
  if URL_SAFE_NO_PAD.encode(&decoded) != spelling {
    return Err("INVALID_POSITION_CURSOR:base64");
  }
  Ok(decoded)
}

fn validate_components(tuple: &[u8], expected_count: u8) -> Result<(), &'static str> {
  let mut offset = 0usize;
  let mut count = 0usize;
  while offset < tuple.len() {
    let header = tuple.get(offset..offset + 8).ok_or("INVALID_POSITION_CURSOR:component-truncated")?;
    let tag = read_u16(header, 0)?;
    let state = header[2];
    let payload_length = read_u32(header, 4)? as usize;
    if header[3] != 0 {
      return Err("INVALID_POSITION_CURSOR:component-reserved");
    }
    let end =
      offset.checked_add(8).and_then(|start| start.checked_add(payload_length)).ok_or("INVALID_POSITION_CURSOR:component-length")?;
    let payload = tuple.get(offset + 8..end).ok_or("INVALID_POSITION_CURSOR:component-length")?;
    match state {
      0 => validate_present_component(tag, payload)?,
      1 | 2 if tag == 0 && payload.is_empty() => {}
      _ => return Err("INVALID_POSITION_CURSOR:component-state"),
    }
    count += 1;
    if count > MAX_COMPONENTS {
      return Err("INVALID_POSITION_CURSOR:component-count");
    }
    offset = end;
  }
  if offset != tuple.len() || count != expected_count as usize {
    return Err("INVALID_POSITION_CURSOR:component-count");
  }
  Ok(())
}

fn validate_present_component(tag: u16, payload: &[u8]) -> Result<(), &'static str> {
  match tag {
    2 => Ok(()),
    3 => std::str::from_utf8(payload).map(|_| ()).map_err(|_| "INVALID_POSITION_CURSOR:component-utf8"),
    4 | 5 | 7 => {
      if payload.len() == 8 {
        Ok(())
      } else {
        Err("INVALID_POSITION_CURSOR:component-payload")
      }
    }
    6 => {
      if payload.len() != 8 {
        return Err("INVALID_POSITION_CURSOR:component-payload");
      }
      let value = f64::from_le_bytes(payload.try_into().map_err(|_| "INVALID_POSITION_CURSOR:component-f64")?);
      if !value.is_finite() || (value == 0.0 && value.to_bits() != 0) {
        Err("INVALID_POSITION_CURSOR:component-f64")
      } else {
        Ok(())
      }
    }
    8 => {
      if payload.len() == 1 && payload[0] <= 1 {
        Ok(())
      } else {
        Err("INVALID_POSITION_CURSOR:component-payload")
      }
    }
    _ => Err("INVALID_POSITION_CURSOR:component-tag"),
  }
}

#[cfg(test)]
fn validate_context(
  position: &DecodedPosition,
  requested_route: RouteKind,
  requested_root: &[u8],
  requested_order: &[u8],
  resolved_file_key: &[u8],
  resolved_revision: &[u8],
  recomputed_tuple: &[u8],
) -> Result<(), &'static str> {
  if position.route != requested_route {
    return Err("INVALID_POSITION_CURSOR");
  }
  if position.namespace_root != requested_root {
    return Err("POSITION_ROOT_MISMATCH");
  }
  if position.order_fingerprint != requested_order {
    return Err("POSITION_ORDER_MISMATCH");
  }
  if position.file_key != resolved_file_key || position.revision != resolved_revision || position.tuple != recomputed_tuple {
    return Err("INVALID_POSITION_CURSOR");
  }
  Ok(())
}

fn position_expected(position: &DecodedPosition, maximum: bool) -> &'static str {
  let identity = format!(
    "tuple={}:order={}:root={}:file={}:revision={}",
    position.tuple.len(),
    hex::encode(&position.order_fingerprint[..4]),
    hex::encode(&position.namespace_root[..4]),
    hex::encode(&position.file_key[..4]),
    hex::encode(&position.revision[..4])
  );
  if maximum {
    leak(format!(
      "position:maximum:route={}:components={}:decoded={}:{identity}",
      position.route.name(),
      position.component_count,
      position.decoded_length
    ))
  } else {
    leak(format!(
      "position:{}:components={}:decoded={}:{identity}",
      position.route.name(),
      position.component_count,
      position.decoded_length
    ))
  }
}

fn sample_hash(profile: HashProfile, start: u8) -> Vec<u8> {
  (0..profile.width()).map(|index| start.wrapping_add(index as u8)).collect()
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, &'static str> {
  let bytes = bytes.get(offset..offset + 2).ok_or("INVALID_POSITION_CURSOR:truncated")?;
  Ok(u16::from_le_bytes(bytes.try_into().map_err(|_| "INVALID_POSITION_CURSOR:truncated")?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, &'static str> {
  let bytes = bytes.get(offset..offset + 4).ok_or("INVALID_POSITION_CURSOR:truncated")?;
  Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| "INVALID_POSITION_CURSOR:truncated")?))
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repair_crc_and_encode(decoded: &mut [u8]) -> Vec<u8> {
    let crc_offset = decoded.len() - 4;
    put_u32(decoded, crc_offset, crc32fast::hash(&decoded[..crc_offset]));
    URL_SAFE_NO_PAD.encode(decoded).into_bytes()
  }

  #[test]
  fn position_fixtures_decode_canonically_at_both_hash_widths() {
    for case in fixture_cases() {
      let (observed, key) = observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, None, "public wire tokens have no KV key");
      assert!(!case.bytes.contains(&b'='));
    }
  }

  #[test]
  fn malformed_base64_and_oversized_input_fail_before_decoded_allocation() {
    let profile = HashProfile::Blake3_256;
    let components = sample_components(RouteKind::Query);
    let order = sample_order_definition(RouteKind::Query);
    let token = encode_position(profile, sample_spec(profile, RouteKind::Query, &order, &components));
    let mut padded = token.clone();
    padded.push(b'=');
    assert_eq!(decode_position(profile, &padded).err(), Some("INVALID_POSITION_CURSOR:base64"));
    assert_eq!(decode_position(profile, b"not+url/base64").err(), Some("INVALID_POSITION_CURSOR:base64"));
    assert_eq!(decode_position(profile, &vec![b'a'; MAX_ENCODED_LENGTH + 1]).err(), Some("INVALID_POSITION_CURSOR:base64"));
  }

  #[test]
  fn header_crc_hash_identity_and_tuple_lengths_fail_closed() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let components = sample_components(RouteKind::Query);
      let order = sample_order_definition(RouteKind::Query);
      let token = encode_position(profile, sample_spec(profile, RouteKind::Query, &order, &components));
      let baseline = decode_base64(&token).unwrap();
      let h = profile.width();

      for offset in [0, 4, 6, 8, 12, 14, 15, 16 + 2 * h] {
        let mut changed = baseline.clone();
        changed[offset] ^= 0x80;
        let changed = repair_crc_and_encode(&mut changed);
        assert!(decode_position(profile, &changed).is_err(), "header offset {offset} accepted");
      }
      for range in [16..16 + h, 16 + h..16 + 2 * h, 20 + 2 * h..20 + 3 * h, 20 + 3 * h..20 + 4 * h] {
        let mut changed = baseline.clone();
        changed[range].fill(0);
        let changed = repair_crc_and_encode(&mut changed);
        assert_eq!(decode_position(profile, &changed).err(), Some("INVALID_POSITION_CURSOR:identity-or-tuple-length"));
      }
      let mut corrupt_crc = token.clone();
      let last = corrupt_crc.len() - 1;
      corrupt_crc[last] = if corrupt_crc[last] == b'A' { b'B' } else { b'A' };
      assert!(decode_position(profile, &corrupt_crc).is_err());
      let other = if profile == HashProfile::Blake3_256 { HashProfile::Sha512 } else { HashProfile::Blake3_256 };
      assert!(decode_position(other, &token).is_err());
    }
  }

  #[test]
  fn component_registry_states_payloads_and_counts_are_closed() {
    let valid = [
      Component::present(2, Vec::new()),
      Component::present(3, "hello".as_bytes().to_vec()),
      Component::present(4, u64::MAX.to_le_bytes()),
      Component::present(5, i64::MIN.to_le_bytes()),
      Component::present(6, 1.5f64.to_le_bytes()),
      Component::present(7, i64::MIN.to_le_bytes()),
      Component::present(8, vec![1]),
      Component::typed_null(),
      Component::missing(),
    ];
    assert_eq!(validate_components(&encode_components(&valid), valid.len() as u8), Ok(()));
    assert_eq!(validate_components(&encode_components(&valid), valid.len() as u8 - 1), Err("INVALID_POSITION_CURSOR:component-count"));

    for invalid in [
      Component::present(1, vec![0]),
      Component::present(3, vec![0xff]),
      Component::present(4, vec![0; 7]),
      Component::present(6, f64::NAN.to_le_bytes()),
      Component::present(6, (-0.0f64).to_le_bytes()),
      Component::present(8, vec![2]),
      Component { tag: 2, state: 1, payload: vec![0] },
    ] {
      assert!(validate_components(&encode_components(&[invalid]), 1).is_err());
    }
    let mut reserved = encode_components(&[Component::present(2, vec![0])]);
    reserved[3] = 1;
    assert_eq!(validate_components(&reserved, 1), Err("INVALID_POSITION_CURSOR:component-reserved"));
  }

  #[test]
  fn exact_maximum_decoded_length_passes_and_one_more_fails() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let token = build_maximum_token(profile);
      assert_eq!(decode_position(profile, &token).unwrap().decoded_length, MAX_DECODED_LENGTH);
      let mut decoded = decode_base64(&token).unwrap();
      let crc = decoded.split_off(decoded.len() - 4);
      decoded.push(0);
      decoded.extend_from_slice(&crc);
      let length = decoded.len() as u32;
      put_u32(&mut decoded, 8, length);
      let oversized = repair_crc_and_encode(&mut decoded);
      assert_eq!(decode_position(profile, &oversized).err(), Some("INVALID_POSITION_CURSOR:base64"));
    }
  }

  #[test]
  fn order_fingerprint_binds_canonical_semantics_but_not_root_or_page_parameters() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let first = sample_order_definition(RouteKind::Query);
      let equivalent = format!("  {first}  ");
      assert_eq!(order_fingerprint(profile, RouteKind::Query, &first), order_fingerprint(profile, RouteKind::Query, &equivalent));

      let changed = first.replace("minimum-ascending-maximum-descending", "maximum-always");
      assert_ne!(order_fingerprint(profile, RouteKind::Query, &first), order_fingerprint(profile, RouteKind::Query, &changed));

      let components = sample_components(RouteKind::Query);
      let left = decode_position(profile, &encode_position(profile, sample_spec(profile, RouteKind::Query, &first, &components))).unwrap();
      let mut right_spec = sample_spec(profile, RouteKind::Query, &first, &components);
      right_spec.namespace_root[0] ^= 1;
      let right = decode_position(profile, &encode_position(profile, right_spec)).unwrap();
      assert_eq!(left.order_fingerprint, right.order_fingerprint);
      assert_ne!(left.namespace_root, right.namespace_root);
      assert_eq!(left.file_key, right.file_key);
      assert_eq!(left.revision, right.revision);
      assert_eq!(left.tuple, right.tuple);
    }
  }

  #[test]
  fn order_definition_requires_the_exact_route_schema() {
    let profile = HashProfile::Blake3_256;
    let valid = sample_order_definition(RouteKind::Query);
    assert!(order_fingerprint(profile, RouteKind::Query, &valid).is_ok());

    let wrong_route = valid.replace(r#""route_kind":2"#, r#""route_kind":1"#);
    assert_eq!(order_fingerprint(profile, RouteKind::Query, &wrong_route).unwrap_err(), "INVALID_POSITION_ORDER:route");

    let missing_policy = valid.replace(r#","score_semantics":"not-applicable""#, "");
    assert_eq!(order_fingerprint(profile, RouteKind::Query, &missing_policy).unwrap_err(), "INVALID_POSITION_ORDER:keys");

    let extra_policy = valid.replacen('{', r#"{"physical_page":7,"#, 1);
    assert_eq!(order_fingerprint(profile, RouteKind::Query, &extra_policy).unwrap_err(), "INVALID_POSITION_ORDER:keys");

    let bad_sort = valid.replace(r#""direction":"asc""#, r#""direction":"sideways""#);
    assert_eq!(order_fingerprint(profile, RouteKind::Query, &bad_sort).unwrap_err(), "INVALID_POSITION_ORDER:sort");

    let duplicate = valid.replacen('{', r#"{"route_kind":2,"#, 1);
    assert!(order_fingerprint(profile, RouteKind::Query, &duplicate).unwrap_err().contains("duplicate canonical config map key"));
  }

  #[test]
  fn unsigned_tokens_are_revalidated_against_route_root_order_and_resolved_position() {
    let profile = HashProfile::Blake3_256;
    let components = sample_components(RouteKind::Query);
    let order = sample_order_definition(RouteKind::Query);
    let position =
      decode_position(profile, &encode_position(profile, sample_spec(profile, RouteKind::Query, &order, &components))).unwrap();
    assert_eq!(
      validate_context(
        &position,
        RouteKind::Query,
        &position.namespace_root,
        &position.order_fingerprint,
        &position.file_key,
        &position.revision,
        &position.tuple
      ),
      Ok(())
    );

    let mut changed = position.namespace_root.clone();
    changed[0] ^= 1;
    assert_eq!(
      validate_context(
        &position,
        RouteKind::Query,
        &changed,
        &position.order_fingerprint,
        &position.file_key,
        &position.revision,
        &position.tuple
      ),
      Err("POSITION_ROOT_MISMATCH")
    );
    assert_eq!(
      validate_context(
        &position,
        RouteKind::DirectoryListing,
        &position.namespace_root,
        &position.order_fingerprint,
        &position.file_key,
        &position.revision,
        &position.tuple
      ),
      Err("INVALID_POSITION_CURSOR")
    );

    changed = position.order_fingerprint.clone();
    changed[0] ^= 1;
    assert_eq!(
      validate_context(
        &position,
        RouteKind::Query,
        &position.namespace_root,
        &changed,
        &position.file_key,
        &position.revision,
        &position.tuple
      ),
      Err("POSITION_ORDER_MISMATCH")
    );

    changed = position.tuple.clone();
    changed[0] ^= 1;
    assert_eq!(
      validate_context(
        &position,
        RouteKind::Query,
        &position.namespace_root,
        &position.order_fingerprint,
        &position.file_key,
        &position.revision,
        &changed
      ),
      Err("INVALID_POSITION_CURSOR")
    );
  }
}
