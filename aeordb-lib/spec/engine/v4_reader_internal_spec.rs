use super::*;

#[test]
fn fixed_array_reads_are_bounded_for_every_offset_and_width() {
  let bytes = [0x81, 0x02, 0xf3, 0x04, 0xa5, 0x06, 0xd7, 0x08];
  for offset in 0..=bytes.len() + 1 {
    assert_eq!(fixed_array_at::<2>(&bytes, offset), bytes.get(offset..).and_then(|tail| tail.get(..2)).map(|raw| [raw[0], raw[1]]));
  }
  assert_eq!(fixed_array_at::<8>(&bytes, 0), Some(bytes));
  assert_eq!(fixed_array_at::<0>(&bytes, bytes.len()), Some([]));
  assert_eq!(fixed_array_at::<9>(&bytes, 0), None);
  assert_eq!(fixed_array_at::<2>(&bytes, usize::MAX), None);
  assert_eq!(fixed_array_at::<8>(&bytes, usize::MAX - 3), None);
  assert_eq!(fixed_array_at::<0>(&bytes, usize::MAX), None);
}

#[test]
fn scalar_reads_keep_cursor_and_allocation_state_on_truncation() {
  macro_rules! check {
    ($reader:ident, $integer:ty) => {
      let encoded = <$integer>::MAX.to_le_bytes();
      for length in 0..encoded.len() {
        let mut reader = BoundedReader::new(&encoded[..length], length).unwrap();
        let error = reader.$reader().unwrap_err();
        assert_eq!(error.code(), "truncated_input");
        assert_eq!(error.class(), MalformedInputClass::TruncationOrTrailingBytes);
        assert_eq!(error.context(), format!("need {} bytes at 0, only {length} remain", encoded.len()));
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.allocated_bytes(), 0);
      }
      let mut reader = BoundedReader::new(&encoded, encoded.len()).unwrap();
      assert_eq!(reader.$reader().unwrap(), <$integer>::MAX);
      assert_eq!(reader.position(), encoded.len());
      reader.finish().unwrap();
      assert_eq!(reader.$reader().unwrap_err().code(), "truncated_input");
      assert_eq!(reader.position(), encoded.len());
      assert_eq!(reader.allocated_bytes(), 0);
    };
  }
  check!(read_u16, u16);
  check!(read_u32, u32);
  check!(read_u64, u64);
  check!(read_i64, i64);

  let bytes = [0xa5, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
  let mut reader = BoundedReader::new(&bytes, bytes.len()).unwrap();
  assert_eq!(reader.read_u8().unwrap(), 0xa5);
  assert_eq!(reader.read_i64().unwrap(), -2);
  reader.finish().unwrap();
  assert_eq!(reader.read_exact(usize::MAX).unwrap_err().code(), "reader_offset_overflow");
  assert_eq!(reader.position(), bytes.len());
}
