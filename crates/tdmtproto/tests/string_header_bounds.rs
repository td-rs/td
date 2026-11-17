//! Regression: the TL string-header bounds checks in the generated reader.
//!
//! libFuzzer found both off-by-ones on 2026-11-11 (the `updates_container`
//! target, Main CI): a `0xff` long-string header checked `need(7)` before
//! reading `buf[pos+7]`, and a `0xfe` mid header checked `need(3)` before
//! reading `buf[pos+3]` — each panicking when the header sat exactly at the
//! buffer tail. Wire-shaped input must be rejected, never panic.

use tdmtproto::telegram_api;

/// `error` (id, code, message string) — the smallest type whose parse
/// reaches the string reader, with the string last.
fn error_buffer(string_header: &[u8]) -> Vec<u8> {
    let mut buf = telegram_api::Error::ID.to_le_bytes().to_vec();
    buf.extend_from_slice(&1i32.to_le_bytes());
    buf.extend_from_slice(string_header);
    buf
}

#[test]
fn truncated_long_string_header_is_rejected() {
    // 0xff promises an 8-byte header; leave exactly 7 of them.
    let buf = error_buffer(&[0xff, 1, 2, 3, 4, 5, 6, 7]);
    let truncated = &buf[..buf.len() - 1];
    assert!(telegram_api::Error::parse(truncated).is_err());
}

#[test]
fn truncated_mid_string_header_is_rejected() {
    // 0xfe promises 3 length bytes after the marker.
    let buf = error_buffer(&[0xfe, 1, 2, 3]);
    assert!(telegram_api::Error::parse(&buf[..buf.len() - 1]).is_err());
    assert!(telegram_api::Error::parse(&error_buffer(&[0xfe, 1, 2])).is_err());
}

#[test]
fn well_formed_error_still_parses() {
    // Short string: length byte 3, payload "abc", one pad byte.
    let e = telegram_api::Error::parse(&error_buffer(&[3, b'a', b'b', b'c']))
        .expect("well-formed error must parse");
    assert_eq!((e.code, e.text.as_str()), (1, "abc"));
}
