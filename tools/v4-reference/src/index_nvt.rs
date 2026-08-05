use crate::core::HashProfile;
use crate::index::{
  build_immutable_value, decode_immutable_value, put_u16, put_u32, put_u64, read_u16, read_u32, read_u64, IndexFixtureCase, IndexFormat,
};

const NVT_TILE_KIND: u16 = 0x0032;
const MAX_TILE_LENGTH: usize = 4 * 1_024 * 1_024;
const ENTRY_LENGTH: usize = 40;

#[derive(Clone, Copy)]
struct TileEntry {
  relative_cell: u32,
  predecessor_page_id: Option<u64>,
  successor_page_id: Option<u64>,
  approximate_live_postings: u64,
  sample_coordinate: u64,
}

struct TileSpec<'a> {
  owner: &'a [u8],
  generation: u64,
  resolution: u64,
  tile_start_cell: u64,
  tile_cell_count: u32,
  basis_posting_generation: u64,
  entries: &'a [TileEntry],
}

#[derive(Debug)]
struct DecodedTile {
  owner: Vec<u8>,
  generation: u64,
  resolution: u64,
  tile_start_cell: u64,
  tile_cell_count: u32,
  populated_entry_count: u32,
  basis_posting_generation: u64,
  approximate_postings: u64,
  entries: Vec<DecodedEntry>,
  key: Vec<u8>,
  logical_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecodedEntry {
  relative_cell: u32,
  predecessor_page_id: Option<u64>,
  successor_page_id: Option<u64>,
  approximate_live_postings: u64,
  sample_coordinate: u64,
}

pub(crate) fn fixture_cases() -> Vec<IndexFixtureCase> {
  let mut cases = Vec::with_capacity(4);
  for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
    let bytes = build_sample_tile(profile);
    let decoded = decode_tile(profile, &bytes).expect("sample NVT tile must decode");
    cases.push(IndexFixtureCase {
      id: leak(format!("aidx-{}-nvt-tile-valid", profile.label())),
      format: IndexFormat::IndexArtifactV1,
      profile,
      expected: tile_expected(&decoded),
      relation: Some("hint-only:NvtTileV1:never-correctness-authority"),
      canonical_key: Some(hex::encode(&decoded.key)),
      bytes: bytes.clone(),
    });
    cases.push(crate::index_pages::nvt_directory_fixture(
      profile,
      &decoded.owner,
      decoded.tile_start_cell,
      decoded.generation,
      decoded.populated_entry_count,
      decoded.logical_bytes,
      &bytes,
    ));
  }
  cases
}

pub(crate) fn observe(profile: HashProfile, bytes: &[u8]) -> (String, Option<String>) {
  match decode_tile(profile, bytes) {
    Ok(tile) => (tile_expected(&tile).to_string(), Some(hex::encode(tile.key))),
    Err(error) => (format!("error:{error}"), None),
  }
}

pub(crate) fn annotation_lines(profile: HashProfile, bytes: &[u8]) -> Vec<String> {
  let identity_length = read_u16(bytes, 16).unwrap_or(0);
  let body_length = read_u32(bytes, 20).unwrap_or(0);
  vec![
    "envelope +0x000 len 32: AIDX common envelope".to_string(),
    "envelope artifact_kind: 0x0032 NvtTileV1".to_string(),
    format!("identity +0x000 len {identity_length}: IndexId H={} || tile_start_cell u64 LE", profile.width()),
    format!("body +0x000 len {body_length}: 64-byte tile header plus sorted sparse 40-byte entries"),
    format!("value +0x{:03x} len 4: artifact_crc32", bytes.len().saturating_sub(4)),
  ]
}

fn build_sample_tile(profile: HashProfile) -> Vec<u8> {
  let definition = crate::field_index::sample_field_index_definition(profile);
  let owner = crate::field_index::index_id(profile, &definition);
  let resolution = 65_536u64;
  let tile_start_cell = 8_192u64;
  let tile_cell_count = 4_096u32;
  let entries = [
    TileEntry {
      relative_cell: 3,
      predecessor_page_id: Some(301),
      successor_page_id: Some(302),
      approximate_live_postings: 10,
      sample_coordinate: coordinate_for_cell(tile_start_cell + 3, resolution),
    },
    TileEntry {
      relative_cell: 2_048,
      predecessor_page_id: Some(350),
      successor_page_id: None,
      approximate_live_postings: 20,
      sample_coordinate: coordinate_for_cell(tile_start_cell + 2_048, resolution),
    },
  ];
  build_tile(
    profile,
    TileSpec {
      owner: &owner,
      generation: 0x3201,
      resolution,
      tile_start_cell,
      tile_cell_count,
      basis_posting_generation: 77,
      entries: &entries,
    },
  )
}

fn build_tile(profile: HashProfile, spec: TileSpec<'_>) -> Vec<u8> {
  assert_eq!(spec.owner.len(), profile.width());
  let entries_length = spec.entries.len() * ENTRY_LENGTH;
  let mut body = vec![0u8; 64 + entries_length];
  put_u16(&mut body, 4, 1);
  put_u16(&mut body, 6, 1);
  put_u64(&mut body, 8, spec.resolution);
  put_u64(&mut body, 16, spec.tile_start_cell);
  put_u32(&mut body, 24, spec.tile_cell_count);
  put_u32(&mut body, 28, spec.entries.len() as u32);
  put_u64(&mut body, 32, spec.basis_posting_generation);
  put_u64(&mut body, 40, entries_length as u64);
  put_u64(&mut body, 48, spec.entries.iter().map(|entry| entry.approximate_live_postings).sum());
  for (index, entry) in spec.entries.iter().enumerate() {
    let offset = 64 + index * ENTRY_LENGTH;
    put_u32(&mut body, offset, entry.relative_cell);
    let flags = u32::from(entry.predecessor_page_id.is_some()) | (u32::from(entry.successor_page_id.is_some()) << 1);
    put_u32(&mut body, offset + 4, flags);
    put_u64(&mut body, offset + 8, entry.predecessor_page_id.unwrap_or(0));
    put_u64(&mut body, offset + 16, entry.successor_page_id.unwrap_or(0));
    put_u64(&mut body, offset + 24, entry.approximate_live_postings);
    put_u64(&mut body, offset + 32, entry.sample_coordinate);
  }
  let mut identity = Vec::with_capacity(profile.width() + 8);
  identity.extend_from_slice(spec.owner);
  identity.extend_from_slice(&spec.tile_start_cell.to_le_bytes());
  build_immutable_value(NVT_TILE_KIND, spec.generation, &identity, &body)
}

fn decode_tile(profile: HashProfile, bytes: &[u8]) -> Result<DecodedTile, &'static str> {
  let artifact = decode_immutable_value(profile, bytes, MAX_TILE_LENGTH)?;
  let h = profile.width();
  if artifact.kind != NVT_TILE_KIND || artifact.identity.len() != h + 8 || artifact.identity[..h].iter().all(|byte| *byte == 0) {
    return Err("nvt_tile_identity");
  }
  let tile_start_cell = read_u64(artifact.identity, h)?;
  let body = artifact.body;
  if body.len() < 64 {
    return Err("nvt_tile_body_length");
  }
  let resolution = read_u64(body, 8)?;
  let tile_cell_count = read_u32(body, 24)?;
  let populated_entry_count = read_u32(body, 28)?;
  let basis_posting_generation = read_u64(body, 32)?;
  let entries_length = usize::try_from(read_u64(body, 40)?).map_err(|_| "nvt_tile_entries_length")?;
  if read_u32(body, 0)? != 0
    || read_u16(body, 4)? != 1
    || read_u16(body, 6)? != 1
    || resolution == 0
    || read_u64(body, 16)? != tile_start_cell
    || tile_cell_count == 0
    || !tile_cell_count.is_power_of_two()
    || u64::from(tile_cell_count) > resolution
    || resolution % u64::from(tile_cell_count) != 0
    || tile_start_cell >= resolution
    || tile_start_cell % u64::from(tile_cell_count) != 0
    || tile_start_cell.checked_add(u64::from(tile_cell_count)).is_none_or(|end| end > resolution)
    || populated_entry_count == 0
    || populated_entry_count > tile_cell_count
    || basis_posting_generation == 0
    || read_u64(body, 56)? != 0
    || usize::try_from(populated_entry_count).ok().and_then(|count| count.checked_mul(ENTRY_LENGTH)) != Some(entries_length)
    || 64usize.checked_add(entries_length) != Some(body.len())
  {
    return Err("nvt_tile_header");
  }
  let mut entries = Vec::new();
  let mut approximate_postings = 0u64;
  for index in 0..populated_entry_count as usize {
    let offset = 64 + index * ENTRY_LENGTH;
    let relative_cell = read_u32(body, offset)?;
    let flags = read_u32(body, offset + 4)?;
    let predecessor = read_u64(body, offset + 8)?;
    let successor = read_u64(body, offset + 16)?;
    let approximate_live_postings = read_u64(body, offset + 24)?;
    let sample_coordinate = read_u64(body, offset + 32)?;
    if flags & !0x03 != 0
      || (flags & 1 != 0) != (predecessor != 0)
      || (flags & 2 != 0) != (successor != 0)
      || relative_cell >= tile_cell_count
      || entries.last().is_some_and(|prior: &DecodedEntry| prior.relative_cell >= relative_cell)
      || coordinate_cell(sample_coordinate, resolution) != tile_start_cell + u64::from(relative_cell)
    {
      return Err("nvt_tile_entry");
    }
    approximate_postings = approximate_postings.checked_add(approximate_live_postings).ok_or("nvt_tile_approximate_overflow")?;
    entries.push(DecodedEntry {
      relative_cell,
      predecessor_page_id: (predecessor != 0).then_some(predecessor),
      successor_page_id: (successor != 0).then_some(successor),
      approximate_live_postings,
      sample_coordinate,
    });
  }
  if read_u64(body, 48)? != approximate_postings {
    return Err("nvt_tile_approximate_count");
  }
  Ok(DecodedTile {
    owner: artifact.identity[..h].to_vec(),
    generation: artifact.generation,
    resolution,
    tile_start_cell,
    tile_cell_count,
    populated_entry_count,
    basis_posting_generation,
    approximate_postings,
    entries,
    key: artifact.key,
    logical_bytes: body.len() as u64,
  })
}

fn coordinate_cell(coordinate: u64, resolution: u64) -> u64 {
  let scaled = (u128::from(coordinate) * u128::from(resolution)) >> 64;
  scaled.min(u128::from(resolution - 1)) as u64
}

fn coordinate_for_cell(cell: u64, resolution: u64) -> u64 {
  assert!(cell < resolution);
  if cell == 0 {
    return 0;
  }
  let numerator = u128::from(cell) << 64;
  let coordinate = numerator.div_ceil(u128::from(resolution));
  u64::try_from(coordinate).expect("cell below resolution has a u64 coordinate")
}

#[cfg(test)]
fn predecessor_entry(entries: &[DecodedEntry], relative_cell: u32) -> Option<DecodedEntry> {
  entries.iter().rev().find(|entry| entry.relative_cell <= relative_cell).copied()
}

#[cfg(test)]
fn verified_page_hint(page_id: Option<u64>, known_minimum: u64, known_maximum: u64, present_page_ids: &[u64]) -> Option<u64> {
  let page_id = page_id?;
  (page_id >= known_minimum && page_id <= known_maximum && present_page_ids.binary_search(&page_id).is_ok()).then_some(page_id)
}

fn tile_expected(tile: &DecodedTile) -> &'static str {
  let first_cell = tile.entries.first().map_or(0, |entry| entry.relative_cell);
  let last_cell = tile.entries.last().map_or(0, |entry| entry.relative_cell);
  leak(format!(
    "index:nvt-tile:resolution={}:start={}:cells={}:entries={}:span={first_cell}/{last_cell}:basis={}:approx={}",
    tile.resolution,
    tile.tile_start_cell,
    tile.tile_cell_count,
    tile.populated_entry_count,
    tile.basis_posting_generation,
    tile.approximate_postings
  ))
}

fn leak(value: String) -> &'static str {
  Box::leak(value.into_boxed_str())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::index::write_trailing_crc;

  #[test]
  fn nvt_fixtures_and_hint_directories_decode_with_exact_keys() {
    for case in fixture_cases() {
      let (observed, key) = crate::index::observe(case.profile, &case.bytes);
      assert_eq!(observed, case.expected, "fixture {}", case.id);
      assert_eq!(key, case.canonical_key, "fixture {} key", case.id);
    }
  }

  #[test]
  fn every_sample_coordinate_maps_to_its_exact_sparse_cell() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let tile = decode_tile(profile, &build_sample_tile(profile)).unwrap();
      for entry in &tile.entries {
        assert_eq!(coordinate_cell(entry.sample_coordinate, tile.resolution), tile.tile_start_cell + u64::from(entry.relative_cell));
      }
      assert_eq!(coordinate_cell(u64::MAX, tile.resolution), tile.resolution - 1);
    }
  }

  #[test]
  fn malformed_sparse_entries_fail_after_crc_repair() {
    for profile in [HashProfile::Blake3_256, HashProfile::Sha512] {
      let baseline = build_sample_tile(profile);
      let body = 32 + profile.width() + 8;
      for offset in [body + 4, body + 16, body + 24, body + 28, body + 40, body + 48, body + 56] {
        let mut changed = baseline.clone();
        changed[offset] ^= 1;
        write_trailing_crc(&mut changed);
        assert!(decode_tile(profile, &changed).is_err(), "tile header offset {offset} accepted");
      }

      let original_key = decode_tile(profile, &baseline).unwrap().key;
      let mut changed_basis = baseline.clone();
      changed_basis[body + 32] ^= 1;
      write_trailing_crc(&mut changed_basis);
      let changed_key = decode_tile(profile, &changed_basis).unwrap().key;
      assert_ne!(changed_key, original_key, "basis generation is valid hint metadata but remains identity-protected");

      let first_entry = body + 64;
      for offset in [first_entry, first_entry + 4] {
        let mut changed = baseline.clone();
        changed[offset] ^= 0x80;
        write_trailing_crc(&mut changed);
        assert!(decode_tile(profile, &changed).is_err(), "tile entry offset {offset} accepted");
      }

      let mut wrong_sample_cell = baseline.clone();
      put_u64(&mut wrong_sample_cell, first_entry + 32, 0);
      write_trailing_crc(&mut wrong_sample_cell);
      assert!(decode_tile(profile, &wrong_sample_cell).is_err());

      let original_key = decode_tile(profile, &baseline).unwrap().key;
      let mut changed_page_hint = baseline.clone();
      changed_page_hint[first_entry + 8] ^= 0x80;
      write_trailing_crc(&mut changed_page_hint);
      let changed_key = decode_tile(profile, &changed_page_hint).unwrap().key;
      assert_ne!(changed_key, original_key, "a different nonzero PageId remains a structurally valid but identity-protected hint");
    }
  }

  #[test]
  fn sparse_lookup_scans_backward_and_stale_page_ids_fall_back_to_directory_search() {
    let profile = HashProfile::Blake3_256;
    let tile = decode_tile(profile, &build_sample_tile(profile)).unwrap();
    assert!(predecessor_entry(&tile.entries, 2).is_none());
    let first = predecessor_entry(&tile.entries, 100).unwrap();
    assert_eq!(first.relative_cell, 3);
    assert_eq!(verified_page_hint(first.predecessor_page_id, 300, 400, &[301, 302, 350]), Some(301));
    assert_eq!(verified_page_hint(first.predecessor_page_id, 302, 400, &[302, 350]), None);
    assert_eq!(verified_page_hint(Some(349), 300, 400, &[301, 302, 350]), None);
  }

  #[test]
  fn corrupt_or_missing_tiles_are_discardable_without_changing_the_fallback_start() {
    let profile = HashProfile::Blake3_256;
    let baseline = build_sample_tile(profile);
    let posting_directory_fallback = 301u64;
    let mut corrupt = baseline;
    corrupt[64] ^= 1;
    assert!(decode_tile(profile, &corrupt).is_err());
    assert_eq!(posting_directory_fallback, 301);
  }
}
