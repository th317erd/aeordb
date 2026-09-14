//! Allocation-free structural views under the canonical value codec owner.
use std::iter::FusedIterator;

use super::super::reader::BoundedReader;
use super::{CanonicalValueBounds, FRAME_LENGTH, FormatResult, MalformedInputClass, error, length_error, u32_at, validate_canonical_value};

/// A complete, validated frame borrowing its immutable source bytes.
/// Structural validity does not prove a compiler profile or projection schema.
#[derive(Debug, Clone, Copy)]
pub struct BorrowedCanonicalValueV1<'a> {
  bytes: &'a [u8],
}

/// Validate the entire root once, without constructing an owned value tree.
/// Child views preserve that validation and never revalidate their subtrees.
pub fn borrow_canonical_value(bytes: &[u8], bounds: CanonicalValueBounds) -> FormatResult<BorrowedCanonicalValueV1<'_>> {
  validate_canonical_value(bytes, bounds)?;
  Ok(BorrowedCanonicalValueV1 { bytes })
}

impl<'a> BorrowedCanonicalValueV1<'a> {
  pub fn encoded_bytes(self) -> &'a [u8] {
    self.bytes
  }

  /// Return only a canonical byte string's payload, not other scalar encodings.
  pub fn as_bytes(self) -> Option<&'a [u8]> {
    if self.bytes.first() != Some(&0x08) {
      return None;
    }
    self.bytes.get(FRAME_LENGTH..)
  }

  pub fn array_entries(self) -> FormatResult<BorrowedCanonicalArrayV1<'a>> {
    Ok(BorrowedCanonicalArrayV1 { cursor: ContainerCursor::new(self, 0x09)? })
  }

  pub fn map_entries(self) -> FormatResult<BorrowedCanonicalMapV1<'a>> {
    Ok(BorrowedCanonicalMapV1 { cursor: ContainerCursor::new(self, 0x0a)? })
  }
}

/// A forward-only view of array members, fused on exhaustion or error.
#[derive(Debug)]
pub struct BorrowedCanonicalArrayV1<'a> {
  cursor: ContainerCursor<'a>,
}

impl<'a> Iterator for BorrowedCanonicalArrayV1<'a> {
  type Item = FormatResult<BorrowedCanonicalValueV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    self.cursor.next_entry(read_value)
  }
}

impl FusedIterator for BorrowedCanonicalArrayV1<'_> {}

/// A forward-only view of sorted map keys and values. Keys borrow the input.
#[derive(Debug)]
pub struct BorrowedCanonicalMapV1<'a> {
  cursor: ContainerCursor<'a>,
}

impl<'a> Iterator for BorrowedCanonicalMapV1<'a> {
  type Item = FormatResult<(&'a str, BorrowedCanonicalValueV1<'a>)>;

  fn next(&mut self) -> Option<Self::Item> {
    self.cursor.next_entry(|bytes, reader| {
      let key_length = read_length(reader)?;
      let key = std::str::from_utf8(reader.read_exact(key_length)?).map_err(|source| {
        error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "config_map_key_utf8", format!("invalid UTF-8: {source}"))
      })?;
      Ok((key, read_value(bytes, reader)?))
    })
  }
}

impl FusedIterator for BorrowedCanonicalMapV1<'_> {}

#[derive(Debug)]
struct ContainerCursor<'a> {
  bytes: &'a [u8],
  reader: BoundedReader<'a>,
  remaining: usize,
}

impl<'a> ContainerCursor<'a> {
  fn new(value: BorrowedCanonicalValueV1<'a>, expected_tag: u8) -> FormatResult<Self> {
    let mut reader = BoundedReader::new(value.bytes, value.bytes.len())?;
    if reader.read_u8()? != expected_tag {
      return Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "config_borrowed_container_type",
        "canonical value is not the requested container type",
      ));
    }
    reader.read_u32()?;
    let remaining = read_length(&mut reader)?;
    Ok(Self { bytes: value.bytes, reader, remaining })
  }

  fn next_entry<T>(&mut self, read: impl FnOnce(&'a [u8], &mut BoundedReader<'a>) -> FormatResult<T>) -> Option<FormatResult<T>> {
    if self.remaining == 0 {
      return None;
    }
    self.remaining -= 1;
    let result = read(self.bytes, &mut self.reader).and_then(|value| {
      if self.remaining == 0 {
        self.reader.finish()?;
      }
      Ok(value)
    });
    match result {
      Ok(value) => Some(Ok(value)),
      Err(error) => {
        self.remaining = 0;
        Some(Err(error))
      }
    }
  }
}

fn read_length(reader: &mut BoundedReader<'_>) -> FormatResult<usize> {
  usize::try_from(reader.read_u32()?).map_err(|source| length_error(format!("canonical length does not fit usize: {source}")))
}

fn read_value<'a>(bytes: &'a [u8], reader: &mut BoundedReader<'a>) -> FormatResult<BorrowedCanonicalValueV1<'a>> {
  let start = reader.position();
  let header = reader.read_exact(FRAME_LENGTH)?;
  let length =
    usize::try_from(u32_at(header, 1)?).map_err(|source| length_error(format!("canonical payload length does not fit usize: {source}")))?;
  reader.read_exact(length)?;
  let bytes = bytes
    .get(start..reader.position())
    .ok_or_else(|| error(MalformedInputClass::TruncationOrTrailingBytes, "config_value_truncated", "canonical frame is truncated"))?;
  Ok(BorrowedCanonicalValueV1 { bytes })
}

#[cfg(test)]
#[path = "../../../spec/engine/canonical_value_borrowed_internal_spec.rs"]
mod internal_spec;
