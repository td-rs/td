//! Small encoding helpers used by the transport layer — the
//! `hex_encode`/`hex_decode` and `base64*` slices of
//! `tdutils/td/utils/base64.cpp` + `misc.cpp`, with the same error texts.

use tdutils::status::{Result, Status};

/// `td::hex_encode`: lowercase hex.
pub fn hex_encode(data: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 16] = b"0123456789abcdef";
    let mut res = Vec::with_capacity(data.len() * 2);
    for &c in data {
        res.push(ALPHABET[(c >> 4) as usize]);
        res.push(ALPHABET[(c & 15) as usize]);
    }
    res
}

/// `td::hex_decode`.
pub fn hex_decode(hex: &[u8]) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Result::Err(Status::error_message("Wrong hex string length"));
    }
    fn from_hex_char(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let mut res = Vec::with_capacity(hex.len() / 2);
    for pair in hex.chunks_exact(2) {
        let (Some(hi), Some(lo)) = (from_hex_char(pair[0]), from_hex_char(pair[1])) else {
            return Result::Err(Status::error_message("Wrong hex string"));
        };
        res.push((hi << 4) | lo);
    }
    Result::Ok(res)
}

/// `td::base64url_encode`: URL-safe alphabet, no padding.
pub fn base64url_encode(data: &[u8]) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(data)
        .into_bytes()
}

/// `td::base64_decode`: standard alphabet with padding.
pub fn base64_decode(data: &[u8]) -> Result<Vec<u8>> {
    use base64::Engine;
    let data = match std::str::from_utf8(data) {
        Ok(data) => data,
        Err(_) => return Result::Err(Status::error_message("Wrong base64")),
    };
    match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(decoded) => Result::Ok(decoded),
        Err(_) => Result::Err(Status::error_message("Wrong base64")),
    }
}

/// `td::base64url_decode`: URL-safe alphabet, no padding.
pub fn base64url_decode(data: &[u8]) -> Result<Vec<u8>> {
    use base64::Engine;
    let data = match std::str::from_utf8(data) {
        Ok(data) => data,
        Err(_) => return Result::Err(Status::error_message("Wrong base64")),
    };
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data) {
        Ok(decoded) => Result::Ok(decoded),
        Err(_) => Result::Err(Status::error_message("Wrong base64")),
    }
}
