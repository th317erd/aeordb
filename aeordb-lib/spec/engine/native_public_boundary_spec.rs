//! Native conformance prerequisites, exercised through the public dispatch API.
use aeordb::engine::native_parsers::parse_native;

#[test]
fn ten_byte_gif_header_does_not_panic() {
  let bytes = b"GIF89a\x01\x00\x01\x00";
  assert_eq!(bytes.len(), 10);
  let _outcome = parse_native(bytes, "image/gif", "tiny.gif", "/tiny.gif", bytes.len() as u64);
}

fn wav(byte_rate: u32) -> Vec<u8> {
  let mut bytes = b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x40\x1f\x00\x00".to_vec();
  bytes.extend_from_slice(&byte_rate.to_le_bytes());
  bytes.extend_from_slice(b"\x02\x00\x10\x00data\x00\x00\x00\x00");
  bytes
}

#[test]
fn wav_bit_rate_is_exact_across_the_full_stored_u32_byte_rate() {
  for byte_rate in [0, 1, 16_000, u32::MAX / 8, u32::MAX / 8 + 1, u32::MAX] {
    let bytes = wav(byte_rate);
    let result = parse_native(&bytes, "audio/wav", "tiny.wav", "/tiny.wav", bytes.len() as u64).unwrap().unwrap();
    assert_eq!(result["metadata"]["bitrate"].as_u64(), Some(u64::from(byte_rate) * 8), "byte rate {byte_rate}");
  }
}

#[test]
fn native_media_prefixes_do_not_panic_at_format_header_boundaries() {
  let wav_bytes = wav(16_000);
  let seeds: &[(&str, &str, &[u8])] = &[
    ("image/gif", "tiny.gif", b"GIF89a\x01\x00\x01\x00\x00\x00\x00;"),
    ("image/jpeg", "tiny.jpg", b"\xff\xd8\xff\xe0\x00\x02\xff\xd9"),
    ("image/png", "tiny.png", b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00"),
    ("image/bmp", "tiny.bmp", b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
    ("image/webp", "tiny.webp", b"RIFF\x04\x00\x00\x00WEBP"),
    ("image/tiff", "tiny.tiff", b"II\x2a\x00\x08\x00\x00\x00\x00\x00"),
    ("audio/wav", "tiny.wav", &wav_bytes),
    ("audio/ogg", "tiny.ogg", b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00"),
    ("audio/mpeg", "tiny.mp3", b"ID3\x04\x00\x00\x00\x00\x00\x00"),
    ("video/x-flv", "tiny.flv", b"FLV\x01\x05\x00\x00\x00\x09"),
    ("application/pdf", "tiny.pdf", b"%PDF-1.7\n%%EOF\n"),
  ];
  let mut failures = Vec::new();
  for (mime, filename, seed) in seeds {
    for length in 0..=seed.len() {
      let outcome = std::panic::catch_unwind(|| parse_native(&seed[..length], mime, filename, filename, length as u64));
      if outcome.is_err() {
        failures.push(format!("{filename}/{length}"));
      }
    }
  }
  assert!(failures.is_empty(), "panicking native parser prefixes: {failures:?}");
}
