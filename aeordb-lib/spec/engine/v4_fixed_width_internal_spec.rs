// Shared across the format modules so each real private reader is exercised.
macro_rules! fixed_width_reader_tests {
  ($($reader:ident: $integer:ty),+ $(,)?) => {
    #[test]
    fn fixed_width_reads_preserve_values_and_reject_all_invalid_offsets() {
      $(
        {
          let width = std::mem::size_of::<$integer>();
          // Independently specified little-endian bytes include the sign bit.
          let encoded = <$integer>::MIN.wrapping_add(0x1234 as $integer).to_le_bytes();
          let expected = <$integer>::MIN.wrapping_add(0x1234 as $integer);
          let mut bytes = vec![0xa5; width + 4];
          bytes[2..2 + width].copy_from_slice(&encoded);
          assert_eq!($reader(&bytes, 2).unwrap(), expected, stringify!($reader));
          assert_eq!($reader(&encoded, 0).unwrap(), expected, stringify!($reader));
          assert_eq!($reader(&[0; 8][..width], 0).unwrap(), 0);
          assert_eq!($reader(&<$integer>::MAX.to_le_bytes(), 0).unwrap(), <$integer>::MAX);

          let baseline = $reader(&[], 0).unwrap_err();
          for length in 0..width {
            let error = $reader(&encoded[..length], 0).unwrap_err();
            assert_eq!(error.code(), baseline.code());
            assert_eq!(error.class(), baseline.class());
          }
          for offset in [bytes.len() - width + 1, bytes.len(), bytes.len() + 1, usize::MAX - width + 1, usize::MAX] {
            let error = $reader(&bytes, offset).unwrap_err();
            assert_eq!(error.code(), baseline.code(), "{} offset {offset}", stringify!($reader));
            assert_eq!(error.class(), baseline.class());
          }
        }
      )+
    }
  };
}
